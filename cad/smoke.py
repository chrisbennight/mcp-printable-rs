"""Dedicated native CAD integration smoke; never part of fake-backed unit tests."""

import json
import math
import re
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

import cadquery as cq

from build import build, original_names, vertical_holes
from step import import_step


def close(actual, expected):
    assert len(actual) == len(expected)
    assert all(math.isclose(a, b, abs_tol=1e-5) for a, b in zip(actual, expected)), (actual, expected)


def main():
    hole_grid = [(-12, -8), (-12, 8), (12, -8), (12, 8)]
    for width in [40, 60]:
        part = cq.Workplane("XY").box(width, 30, 5).faces(">Z").workplane().pushPoints(hole_grid).hole(4).val()
        measured = vertical_holes(part)
        assert measured["status"] == "measured", measured
        actual = sorted((tuple(h["center_mm"]) for h in measured["holes"]), key=lambda p: tuple(round(v, 6) for v in p))
        assert len(actual) == len(hole_grid), measured
        for point, expected in zip(actual, sorted(hole_grid)):
            close(point, expected)
        assert all(math.isclose(h["radius_mm"], 2, abs_tol=1e-5) for h in measured["holes"])
    boss = cq.Workplane("XY").circle(2).extrude(5).val()
    assert vertical_holes(boss) == {"status": "measured", "holes": []}
    tilted = part.rotate((0, 0, 0), (1, 0, 0), 45)
    assert vertical_holes(tilted) == {"status": "measured", "holes": []}
    assert vertical_holes(cq.Face.makePlane(10, 20))["status"] == "unmeasured"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "model.py").write_text('result = cq.Workplane().box(parameters["width"], 20, 30)\n')
        build({"action": "model", "source": "model.py", "parameters": {"width": 10},
               "linear_tolerance_mm": 0.05, "angular_tolerance_rad": 0.1}, root)
        output = root / "output"
        report = json.loads((output / "report.json").read_text())
        close(report["bounds_mm"]["size"], [10, 20, 30])
        assert report["valid"] and report["solid_count"] == 1
        stl = (output / "model.stl").read_bytes()
        triangles = struct.unpack_from("<I", stl, 80)[0]
        assert len(stl) == 84 + triangles * 50
        vertices = [struct.unpack_from("<fff", stl, 84 + face * 50 + 12 + vertex * 12)
                    for face in range(triangles) for vertex in range(3)]
        close([max(v[i] for v in vertices) - min(v[i] for v in vertices) for i in range(3)], [10, 20, 30])
        glb = (output / "model.glb").read_bytes()
        size = struct.unpack_from("<I", glb, 12)[0]
        document = json.loads(glb[20:20 + size])
        positions = [document["accessors"][p["attributes"]["POSITION"]]
                     for mesh in document["meshes"] for p in mesh["primitives"]]
        extents = [max(p["max"][i] for p in positions) - min(p["min"][i] for p in positions) for i in range(3)]
        close(sorted(extents), [0.01, 0.02, 0.03])

        assembly = cq.Assembly(name="vendor")
        assembly.add(cq.Workplane().box(25.4, 10, 5), name="first", loc=cq.Location((0, 0, 0)))
        assembly.add(cq.Workplane().box(25.4, 10, 5), name="second", loc=cq.Location((50.8, 0, 0)))
        step = root / "vendor.step"
        assembly.export(str(step), unit="MM", outputUnit="INCH")
        step.write_text(step.read_text().replace("'first'", "'repeated'").replace("'second'", "'repeated'"))
        imported, metadata = import_step(step)
        close([imported.toCompound().BoundingBox().xlen], [76.2])
        assert len(imported.toCompound().Solids()) == 2
        assert metadata["declared_length_units"]
        assert len(list(imported)) == 2

        product = cq.Assembly(name="vendor")
        product.add(cq.Workplane().box(2, 3, 4), name="PartName")
        named = root / "named.step"
        product.export(str(named))
        contents = named.read_text()
        renamed, count = re.subn(r"(NEXT_ASSEMBLY_USAGE_OCCURRENCE\('[^']*',)'PartName'", r"\1'InstanceName'", contents)
        assert count == 1
        named.write_text(renamed)
        named_assembly, _ = import_step(named)
        assert any(v["occurrence_name"] == "InstanceName" and v["product_name"] == "PartName"
                   for v in original_names(named_assembly).values())
        converted = root / "imported"
        converted.mkdir()
        (converted / "source.step").write_bytes(named.read_bytes())
        build({"action": "import_step", "source": "source.step", "parameters": {},
               "linear_tolerance_mm": 0.05, "angular_tolerance_rad": 0.1}, converted)
        inventory = json.loads((converted / "output/components.json").read_text())
        assert any(v["product_name"] == "vendor" for v in inventory["nodes"].values())
        assert any(v["occurrence_name"] == "InstanceName" and v["product_name"] == "PartName"
                   for v in inventory["components"])

        if "--worker" in sys.argv:
            worker_smoke(root)


def worker_smoke(root):
    workspace = root / "workspace"
    manifests = workspace / ".printable/projects"
    manifests.mkdir(parents=True)
    project = workspace / "projects/smoke"
    project.mkdir(parents=True)
    (manifests / "smoke.json").write_text(json.dumps({"format_version": 1, "project_id": "smoke", "name": "Smoke", "description": "", "root": "projects/smoke"}))
    (project / "part.py").write_text('result = cq.Workplane().box(parameters["width"], 20, 30)\n')
    environment = {
        "PRINTABLE_WORKSPACE_ROOT": str(workspace),
        "PRINTABLE_CAD_LISTEN": "127.0.0.1:8002",
    }
    endpoint = "http://127.0.0.1:8002"
    process = subprocess.Popen(["/usr/local/bin/printable-cad-worker"], env=environment)
    try:
        for _ in range(50):
            if process.poll() is not None:
                raise RuntimeError("CAD worker exited during startup")
            try:
                with urllib.request.urlopen(endpoint + "/healthz", timeout=1):
                    break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise RuntimeError("CAD worker did not become healthy")
        data = json.dumps({"action": "model", "params": {"project_id": "smoke", "source": "part.py", "parameters": {"width": 42}, "output_dir": "builds/one"}}).encode()
        subprocess.run(["/usr/local/bin/printable-cad-worker", "--healthcheck"], env=environment, check=True, timeout=5)
        request = urllib.request.Request(endpoint + "/build", data=data, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=60) as response:
            result = json.load(response)
        close(result["report"]["bounds_mm"]["size"], [42, 20, 30])
        assert len(result["artifacts"]) == 4
        assert (project / "builds/one/inputs/part.py").read_bytes() == (project / "part.py").read_bytes()
        assert json.loads((project / "builds/one/report.json").read_text()) == result
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


if __name__ == "__main__":
    main()
