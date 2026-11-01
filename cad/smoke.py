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
from unittest.mock import patch

import cadquery as cq

from build import build, original_names, vertical_holes
from step import import_step


def close(actual, expected):
    assert len(actual) == len(expected)
    assert all(math.isclose(a, b, abs_tol=1e-5) for a, b in zip(actual, expected)), (actual, expected)


def inspection_smoke(root):
    source = ('result = cq.Assembly(name="board")\n'
              'part = cq.Workplane().box(2, 4, 6)\n'
              'result.add(part, name="USB_left", loc=cq.Location((10, 20, 30), (0, 0, 1), 90))\n'
              'result.add(part, name="USB_right", loc=cq.Location((50, 20, 30)))\n'
              'result["USB_left"].addSubshape(part.val().Faces()[0], color=cq.Color("red"))\n')
    reports = []
    for name, options in [
        ("first", {"selection": {"name_contains": "usb"}, "limit": 1}),
        ("second", {"selection": {"name_contains": "usb"}, "offset": 1}),
        ("local", {"selection": {"region_mm": {"min": [7, 18, 26], "max": [13, 22, 34]}}, "frame": "component"}),
        ("missing", {"selection": {"component_ids": ["absent"]}}),
    ]:
        directory = root / name
        directory.mkdir()
        (directory / "model.py").write_text(source)
        build({"action": "inspect", "source": "model.py", "parameters": {}, "inspection": {"retain_model": False, **options}}, directory)
        assert {path.name for path in (directory / "output").iterdir()} == {"report.json", "components.json", "measurements.json"}
        reports.append(json.loads((directory / "output/report.json").read_text()))
    first, second, local, missing = reports
    assert first["matched_count"] == 2 and first["next_offset"] == 1
    assert second["next_offset"] is None
    assert first["components"][0]["id"] != second["components"][0]["id"]
    close(first["components"][0]["bounds_mm"]["size"], [4, 2, 6])
    close(first["components"][0]["bounds_mm"]["min"], [8, 19, 27])
    assert local["matched_count"] == 1
    close(local["components"][0]["bounds_mm"]["size"], [2, 4, 6])
    assert missing["missing_component_ids"] == ["absent"] and not missing["components"]

    from model import load_model
    retained = root / "retained"
    retained.mkdir()
    (retained / "source.py").write_text(source)
    build({"action":"inspect", "source":"source.py", "parameters":{}, "inspection":{
        "selection":{"name_contains":"USB_left"}, "questions":[{"kind":"features", "id":"direct"}]}}, retained)
    direct = json.loads((retained / "output/report.json").read_text())
    assert direct["answers"][0]["status"] == "measured"
    model = retained / "output/model.cadmodel"
    restored, _ = load_model(model)
    assert [name for _, name, _, _ in restored] == ["board/USB_left", "board/USB_right"]
    assert list(restored["USB_left"]._subshape_colors.values()) == [cq.Color("red")]
    close([restored.toCompound().BoundingBox().xlen], [43])
    (retained / "source.py").write_text('raise AssertionError("source must not run again")\n')
    followup = root / "followup"
    followup.mkdir()
    options = {"selection":{"component_ids":["board/USB_left"]}, "questions":[
        {"kind":"features", "id":"faces", "component_id":"board/USB_left"},
        {"kind":"distance", "id":"gap", "first":{"selection":{"name_contains":"USB_left"}}, "second":{"selection":{"name_contains":"USB_right"}}},
        {"kind":"alignment", "id":"axes", "first":{"component_id":"board/USB_left", "face_index":0}, "second":{"component_id":"board/USB_right", "face_index":0}},
        {"kind":"section", "id":"cut", "component_id":"board/USB_left", "origin_mm":[10,20,30], "normal":[0,0,1]},
        {"kind":"validity", "id":"valid", "component_id":"board/USB_left"},
        {"kind":"validity", "id":"missing", "component_id":"absent"},
        {"kind":"validity", "id":"ambiguous", "selection":{"name_contains":"USB"}}], "exports":["step", "stl", "glb", "svg"]}
    with patch("build.runpy.run_path", side_effect=AssertionError("script replay")), patch("build.import_step", side_effect=AssertionError("STEP replay")), patch("build.export_glb", side_effect=ValueError("preview unavailable")):
        build({"action":"inspect", "source":str(model), "reuse_model":True, "inspection":options}, followup)
    report = json.loads((followup / "output/report.json").read_text())
    answers = {answer["id"]: answer for answer in report["answers"]}
    assert all(answers[key]["status"] == "measured" for key in ("faces", "gap", "axes", "cut", "valid")), answers
    assert answers["missing"]["status"] == "unavailable"
    assert answers["ambiguous"]["status"] == "unavailable"
    close([answers["gap"]["evidence"]["distance_mm"]], [37])
    close([answers["axes"]["evidence"]["angle_degrees"]], [90])
    close([answers["cut"]["evidence"]["length_mm"]], [12])
    assert answers["valid"]["evidence"]["valid"]
    assert {row["format"]:row["status"] for row in report["exports"]} == {"step":"completed", "stl":"completed", "glb":"failed", "svg":"completed"}
    chosen, _ = import_step(followup / "output/selected.step")
    close([chosen.toCompound().BoundingBox().xlen], [4])
    assert len(chosen.toCompound().Solids()) == 1

    from inspection import answer as native_answer

    def interrupted_question(question, *args):
        if question["id"] == "later":
            raise KeyboardInterrupt("native interruption")
        return native_answer(question, *args)

    for name, questions, exports, target, injected in [
        ("later-question", [{"kind":"validity", "id":"first"}, {"kind":"validity", "id":"later"}], [], "inspection.answer", interrupted_question),
        ("bounds-export", [], ["glb"], "build.export_glb", KeyboardInterrupt("native interruption")),
    ]:
        directory = root / name
        directory.mkdir()
        (directory / "part.py").write_text('result = cq.Workplane().box(10, 20, 30)\n')
        with patch(target, side_effect=injected):
            try:
                build({"action":"inspect", "source":"part.py", "parameters":{}, "inspection":{
                    "retain_model":False, "questions":questions, "exports":exports}}, directory)
            except KeyboardInterrupt:
                pass
            else:
                raise AssertionError("expected native interruption")
        progress = json.loads((directory / "progress.json").read_text())
        assert "measurements.json" in progress["completed_artifacts"]
        partial = json.loads((directory / "output/measurements.json").read_text())
        close(partial["components"][0]["bounds_mm"]["size"], [10,20,30])
        assert [item["id"] for item in partial["answers"]] == (["first"] if questions else [])


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
        inspection_smoke(root)
        interrupted = root / "failed-export"
        interrupted.mkdir()
        (interrupted / "part.py").write_text('result = cq.Workplane().box(10, 20, 30)\n')
        with patch("build.export_glb", side_effect=ValueError("export unavailable")):
            try:
                build({"action":"model", "source":"part.py", "parameters":{},
                       "linear_tolerance_mm":0.05, "angular_tolerance_rad":0.1}, interrupted)
            except ValueError as error:
                assert str(error) == "export unavailable"
            else:
                raise AssertionError("expected an export failure")
        partial = json.loads((interrupted / "output/measurements.json").read_text())
        close(partial["bounds_mm"]["size"], [10, 20, 30])
        assert partial["completion"] == "partial" and "valid" not in partial
        progress = json.loads((interrupted / "progress.json").read_text())
        assert progress["stage"] == "glb_export"
        assert "model.stl" in progress["completed_artifacts"]
        assert "model.glb" not in progress["completed_artifacts"]
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
        inspected = root / "inspected-step"
        inspected.mkdir()
        (inspected / "source.step").write_bytes(named.read_bytes())
        build({"action": "inspect", "source": "source.step", "parameters": {},
               "inspection": {"selection": {"name_contains": "InstanceName"}}}, inspected)
        measured = json.loads((inspected / "output/report.json").read_text())
        assert measured["matched_count"] == 1
        close(measured["components"][0]["bounds_mm"]["size"], [2, 3, 4])
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
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError("CAD worker exited during startup")
            try:
                with urllib.request.urlopen(endpoint + "/healthz", timeout=1):
                    break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise RuntimeError("CAD worker did not become healthy")
        with urllib.request.urlopen(endpoint + "/readyz", timeout=5) as response:
            readiness = json.load(response)
        assert readiness["state"] == "ready"
        assert readiness["engine"] == "CadQuery" and readiness["version"] == cq.__version__
        data = json.dumps({"action": "model", "params": {"project_id": "smoke", "source": "part.py", "parameters": {"width": 42}, "output_dir": "builds/one"}}).encode()
        subprocess.run(["/usr/local/bin/printable-cad-worker", "--healthcheck"], env=environment, check=True, timeout=5)
        request = urllib.request.Request(endpoint + "/build", data=data, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=60) as response:
            result = json.load(response)
        close(result["report"]["bounds_mm"]["size"], [42, 20, 30])
        assert len(result["artifacts"]) == 4
        assert (project / "builds/one/inputs/part.py").read_bytes() == (project / "part.py").read_bytes()
        assert json.loads((project / "builds/one/report.json").read_text()) == result
        (project / "surface.py").write_text('result = cq.Face.makePlane(10, 20)\n')
        (project / "parts.py").write_text('result = cq.Assembly().add(cq.Workplane().box(10,20,30), name="first").add(cq.Workplane().box(10,20,30), name="second", loc=cq.Location((20,0,0)))\n')
        for name, source, qualification, expected, criterion in [
            ("qualified", "part.py", {"policy":"printable_part", "dimensions_mm":[42,20,30]}, "passed", None),
            ("wrong-size", "part.py", {"policy":"printable_part", "dimensions_mm":[50,20,30]}, "failed", "dimensions"),
            ("wrong-units", "part.py", {"policy":"printable_part", "units":"inch", "dimensions_mm":[42,20,30]}, "failed", "units"),
            ("parts", "parts.py", {"policy":"printable_part", "dimensions_mm":[30,20,30], "solid_count":2}, "passed", None),
            ("inspect-surface", "surface.py", {"policy":"inspection"}, "passed", None),
            ("reject-surface", "surface.py", {"policy":"printable_part"}, "failed", "meaningful_solids"),
        ]:
            request = urllib.request.Request(endpoint + "/build", data=json.dumps({"action":"model", "params":{
                "project_id":"smoke", "source":source, "parameters":{"width":42},
                "output_dir":"builds/" + name, "qualification":qualification}}).encode(), headers={"Content-Type":"application/json"})
            with urllib.request.urlopen(request, timeout=60) as response:
                result = json.load(response)
            assert result["completion"] == "completed" and result["qualification"]["status"] == expected
            if criterion:
                assert result["qualification"]["criteria"][criterion]["status"] == "failed"
            assert len(result["artifacts"]) == 4
            assert (project / "builds" / name / "inputs" / source).read_bytes() == (project / source).read_bytes()
            assert json.loads((project / "builds" / name / "report.json").read_text()) == result
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


if __name__ == "__main__":
    main()
