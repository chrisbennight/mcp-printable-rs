"""Exercise packaged Orca slicing, toolpath previews, and restart recovery."""
import io
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
import zipfile

from PIL import Image
from capture_fixture import COLORS, create_fixture, verify_bed_surface, verify_camera_replay, verify_capture


def verify_package(path):
    with zipfile.ZipFile(path) as package:
        names = set(package.namelist())
        assert any(name.endswith(".gcode") and package.getinfo(name).file_size > 0
                   for name in names), "package has no G-code"
        models = [name for name in names if name.endswith(".model")]
        assert models, "package has no model"
        assert any(ET.fromstring(package.read(name)).find(".//{*}triangle") is not None
                   for name in models), "package has no geometry"
        thumbnails = set()
        for name in names:
            if not name.endswith(".rels"):
                continue
            for relationship in ET.fromstring(package.read(name)):
                if "thumbnail" not in relationship.get("Type", "").lower():
                    continue
                target = relationship.attrib["Target"]
                assert target.startswith("/"), f"unexpected thumbnail target: {target}"
                thumbnails.add(target.lstrip("/"))
        assert thumbnails, "package has no thumbnail relationships"
        assert {"Metadata/plate_1.png", "Metadata/plate_1_small.png"} <= thumbnails, (
            f"missing expected plate thumbnail relationships: {sorted(thumbnails)}")
        for name in thumbnails:
            assert name in names, f"missing thumbnail: {name}"
            with Image.open(io.BytesIO(package.read(name))) as image:
                image.load()
                assert image.format == "PNG", f"thumbnail is not PNG: {name}"
                assert min(image.size) > 0
                rgba = image.convert("RGBA")
                background = Image.new("RGBA", rgba.size, "white")
                background.alpha_composite(rgba)
                assert any(low != high for low, high in background.convert("RGB").getextrema()), f"blank thumbnail: {name}"


def request(action, params):
    body = json.dumps({"action": action, "params": params}).encode()
    req = urllib.request.Request("http://127.0.0.1:8003/slice", body,
                                 {"Content-Type": "application/json"})
    started = time.monotonic()
    diagnostic = None
    if action == "capture":
        print("NATIVE_CAPTURE_START", json.dumps({key: params[key] for key in
              ("plates", "objects", "camera") if key in params}), flush=True)

        def report_capture_progress():
            scratch = root / ".printable/storage/scratch"
            for path in scratch.glob("*/orca-runtime.*/orca-capture-*/log/*.log.*"):
                try:
                    print("NATIVE_CAPTURE_PROGRESS", path.read_text()[-12000:], flush=True)
                except FileNotFoundError:
                    pass
            for name in ("memory.current", "memory.events", "pids.current", "pids.events"):
                path = Path("/sys/fs/cgroup") / name
                if path.is_file():
                    print("NATIVE_CAPTURE_RESOURCE", name, path.read_text().strip(), flush=True)

        diagnostic = threading.Timer(30, report_capture_progress)
        diagnostic.start()
    try:
        timeout = params.get("timeout_seconds", 300) + 30 if action == "capture" else 120
        with urllib.request.urlopen(req, timeout=timeout) as response:
            result = json.load(response)
    except urllib.error.HTTPError:
        if action == "capture":
            for path in (project / "observations").glob("orca-failed-*.json"):
                print("NATIVE_CAPTURE_FAILURE", path.read_text(), flush=True)
        raise
    finally:
        if diagnostic is not None:
            diagnostic.cancel()
            diagnostic.join()
    if action == "capture":
        print("NATIVE_CAPTURE_DONE", round(time.monotonic() - started, 2), flush=True)
    return result


def cube():
    points = [(0,0,0),(10,0,0),(10,10,0),(0,10,0),
              (0,0,3),(10,0,3),(10,10,3),(0,10,3)]
    faces = [(0,2,1),(0,3,2),(4,5,6),(4,6,7),(0,1,5),(0,5,4),
             (1,2,6),(1,6,5),(2,3,7),(2,7,6),(3,0,4),(3,4,7)]
    text = "solid fixture\n"
    for face in faces:
        text += "facet normal 0 0 0\nouter loop\n"
        for index in face:
            text += "vertex " + " ".join(map(str, points[index])) + "\n"
        text += "endloop\nendfacet\n"
    return text + "endsolid fixture\n"


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    project = root / "projects/native-slice"
    project.mkdir(parents=True)
    manifests = root / ".printable/projects"
    manifests.mkdir(parents=True)
    (manifests / "native-slice.json").write_text(json.dumps({
        "format_version": 1, "project_id": "native-slice", "name": "Native slice",
        "description": "", "root": "projects/native-slice"}))
    (project / "source.stl").write_text(cube())
    env = {**os.environ, "PRINTABLE_WORKSPACE_ROOT": str(root)}

    def start():
        process = subprocess.Popen(["/usr/local/bin/printable-slicer-worker"], env=env)
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError("slicer worker exited during startup")
            try:
                with urllib.request.urlopen("http://127.0.0.1:8003/healthz", timeout=1):
                    return process
            except OSError:
                time.sleep(0.1)
        process.terminate()
        process.wait(timeout=10)
        raise RuntimeError("slicer worker did not become healthy")

    worker = start()
    try:
        with urllib.request.urlopen("http://127.0.0.1:8003/readyz", timeout=5) as response:
            readiness = json.load(response)
        assert readiness["state"] == "ready"
        assert readiness["engine"] == "OrcaSlicer" and readiness["version"] == "2.4.2"
        assert readiness["native_capture"] is True
        assert all(count > 0 for count in readiness["profile_counts"].values())
        vendor = request("profiles", {"category": "filament", "query": "Polymaker",
                                      "printer": "Bambu Lab X1 Carbon 0.4 nozzle"})
        assert vendor["total"] > 0, "vendor subfolder profiles are missing"
        library_child = request("settings", {"category": "filament", "profile": {"name": "AliZ PLA @P1-X1"},
                                             "query": "nozzle_temperature"})
        assert any(row["value"] for row in library_child["settings"]), "shared library parent did not resolve"
        hidden = request("settings", {"category": "process", "profile": {"name": "0.20mm Standard @BBL X1C"},
                                      "query": "brim_type"})
        assert any(row["key"] == "brim_type" and row["overrideable"] for row in hidden["settings"])
        handles = []
        colours = {"a1": "#0A2CE3", "x1c-reslice": "#C12E1F"}
        for model, suffix, printer in [("a1", "A1", "Bambu Lab A1 0.4 nozzle"),
                                        ("x1c", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("x1c-reslice", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("capture-template", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle")]:
            profiles = request("profiles", {"category": "process", "printer": printer, "query": "0.20mm Standard"})
            assert profiles["total"] > 0
            inspected = request("settings", {"category": "filament", "profile": {"name": f"Bambu PLA Basic @BBL {suffix}"}, "query": "plate_temp"})
            assert any(row["key"] == "hot_plate_temp" for row in inspected["settings"])
            handle = {"project_id": "native-slice", "output_dir": model}
            process = {"name": f"0.20mm Standard @BBL {suffix}"}
            filament = {"name": f"Bambu PLA Basic @BBL {suffix}"}
            if model == "a1":
                process["overrides"] = {"brim_type": "no_brim"}
                filament["overrides"] = {"pressure_advance": ["0.031"]}
            if model in colours:
                filament["colour"] = colours[model]
            state = request("prepare", {**handle, "source": "x1c/model.gcode.3mf" if model == "x1c-reslice" else "source.stl",
                "build_plate": "High Temp Plate" if model == "x1c" else "Textured PEI Plate",
                "printer": {"name": printer},
                "process": process,
                "filaments": ([{**filament, "colour": color} for color in COLORS]
                              if model == "capture-template" else [filament]),
                "auto_arrange": True, "timeout_seconds": 300})
            deadline = time.monotonic() + 360
            while state["status"] == "running" and time.monotonic() < deadline:
                time.sleep(0.5)
                state = request("status", handle)
            if state["status"] != "completed":
                log = project / model / "build-log.json"
                raise RuntimeError(f"slice failed: {state}; {log.read_text() if log.exists() else 'no log'}")
            assert state["setup"]["build_plate"] == ("High Temp Plate" if model == "x1c" else "Textured PEI Plate")
            assert isinstance(state["progress"]["total_percent"], (int, float))
            toolpath = next(name for name in state["artifacts"] if name.endswith(".gcode"))
            gcode = (project / model / toolpath).read_text()
            assert f'; curr_bed_type = {state["setup"]["build_plate"]}' in gcode
            temperature = state["setup"]["filaments"][0]["bed_temperature_initial_layer"][0]
            assert f"M190 S{temperature}" in gcode or f"M140 S{temperature}" in gcode
            if model == "a1":
                assert "; brim_type = no_brim" in gcode, "process override did not reach Orca"
                assert "; pressure_advance = 0.031" in gcode, "filament override did not reach Orca"
            if model in colours:
                assert f"; filament_colour = {colours[model]}" in gcode, "slot colour did not reach the G-code"
                with zipfile.ZipFile(project / model / "model.gcode.3mf") as package:
                    project_settings = json.loads(package.read("Metadata/project_settings.config"))
                assert project_settings["filament_colour"][0].upper() == colours[model], project_settings["filament_colour"]
            verify_package(project / model / "model.gcode.3mf")
            review = request("review", {"slice": handle, "toolpath": toolpath, "first_layer": 1, "last_layer": 1})
            assert review["kind"] == "actual_toolpath" and review["segments"] > 0
            assert (root / review["image"]["path"]).read_bytes().startswith(b"\x89PNG")
            settings = json.loads((project / model / "settings.json").read_text())
            assert settings["printer"]["name"] == printer
            assert settings["printer"]["machine_start_gcode"]
            assert all("inherits" not in profile for profile in [settings["printer"], settings["process"], *settings["filaments"]])
            handles.append(handle)
        fixture_path = project / "capture-layout.3mf"
        fixture = create_fixture(project / "capture-template/model.gcode.3mf", fixture_path)
        source_hash = hashlib.sha256(fixture_path.read_bytes()).hexdigest()
        capture_params = {"project_id": "native-slice",
                          "source": {"kind": "project", "path": fixture_path.name},
                          "width": 1600, "height": 1000}
        overview = request("capture", capture_params)
        assert overview["source_sha256"] == source_hash
        verify_capture(root, overview, fixture)
        replay = request("capture", {**capture_params, "camera": overview["camera"]})
        verify_camera_replay(replay["camera"], overview["camera"])
        assert replay["image_sha256"] == overview["image_sha256"], "camera replay changed native pixels"
        target = list(overview["camera"]["target"])
        target[0] += 5
        zoom = overview["camera"]["zoom"] * 0.5
        custom_frame = request("capture", {**capture_params, "width": 1200, "height": 800,
            "camera": {"target": target, "zoom": zoom}})
        verify_capture(root, custom_frame, fixture)
        assert (custom_frame["width"], custom_frame["height"]) == (1200, 800)
        assert all(math.isclose(a, b, rel_tol=1e-12, abs_tol=1e-9)
                   for a, b in zip(custom_frame["camera"]["target"], target))
        assert abs(custom_frame["camera"]["zoom"] - zoom) <= 1e-9 * max(1, zoom)
        for preset, forward in {"top": [0, 0, -1], "bottom": [0, 0, 1],
                                "front": [0, 1, 0], "rear": [0, -1, 0],
                                "left": [1, 0, 0], "right": [-1, 0, 0]}.items():
            result = request("capture", {**capture_params, "plates": [2], "objects": [1],
                "camera": {"preset": preset},
                "visibility": {"beds": False, "plate_identifiers": False, "prime_towers": False}})
            native = verify_capture(root, result, fixture, [2])
            assert all(abs(-actual - expected) < 1e-9 for actual, expected in
                       zip(native["view_matrix"][2][:3], forward)), "native axis preset looks in the wrong direction"
        for controls in [
            {"plates": [2], "camera": {"preset": "top"}},
            {"camera": {"preset": "top", "projection": "perspective"}},
            {"plates": [2], "camera": {"preset": "top", "projection": "perspective"},
             "visibility": {"beds": False, "plate_identifiers": False, "prime_towers": False}},
        ]:
            result = request("capture", {**capture_params, **controls})
            verify_capture(root, result, fixture, controls.get("plates"))
        object_controls = {"objects": [1], "camera": {"orientation": [0.9238795, 0, 0, 0.3826834]},
            "visibility": {"beds": False, "plate_identifiers": False, "prime_towers": False}}
        object_capture = request("capture", {**capture_params, **object_controls})
        verify_capture(root, object_capture, fixture)
        by_object_path = project / "capture-by-object.3mf"
        by_object_fixture = create_fixture(project / "capture-template/model.gcode.3mf",
                                          by_object_path, print_sequence="by object")
        by_object = request("capture", {**capture_params, **object_controls,
            "source": {"kind": "project", "path": by_object_path.name}})
        verify_capture(root, by_object, by_object_fixture)
        assert by_object["image_sha256"] == object_capture["image_sha256"], "hidden plates leaked sequential-print clearance overlays"
        diagnostics_before = set((project / "observations").glob("orca-failed-*.json"))
        try:
            request("capture", {**capture_params, "camera": {"zoom": 500}})
        except urllib.error.HTTPError as error:
            failure = json.load(error)
            assert error.code == 422 and failure["code"] == "slice", failure
        else:
            raise AssertionError("native capture silently clamped an unsupported explicit zoom")
        diagnostics = set((project / "observations").glob("orca-failed-*.json")) - diagnostics_before
        assert len(diagnostics) == 1, "native camera rejection did not retain diagnostics"
        prepared = request("capture", {"project_id": "native-slice",
            "source": {"kind": "prepared", "output_dir": "x1c-reslice"}})
        prepared_state = request("status", {"project_id": "native-slice", "output_dir": "x1c-reslice"})
        assert prepared["source_sha256"] == prepared_state["artifacts"]["model.gcode.3mf"]["sha256"]
        assert prepared["slice_provenance"] == prepared_state["provenance"]
        verify_bed_surface(root, prepared, [(x, y, 0) for x in (64, 192) for y in (64, 192)])
        prepared_native = json.loads((root / prepared["native_metadata"]["path"]).read_text())
        prepared_models = [volume for volume in prepared_native["volumes"] if not volume["prime_tower"]]
        assert len(prepared_models) == 1, "prepared capture omitted or duplicated its source model"
        bounds = prepared_models[0]["bounds_mm"]
        assert all(abs(high - low - expected) < 0.005 for low, high, expected in
                   zip(bounds["min"], bounds["max"], [10, 10, 3])), "prepared capture changed its source geometry"
        assert hashlib.sha256(fixture_path.read_bytes()).hexdigest() == source_hash, "capture changed source bytes"
        print("Native three-plate layout capture, source transforms, colors, towers, camera controls and replay passed")
        # Two-nozzle printers group the loaded filaments between their nozzles.
        # The bundled profiles set no filament colours, so this slice also checks
        # that a colour list shorter than the filament count is handled.
        handle = {"project_id": "native-slice", "output_dir": "h2d-two-filaments"}
        state = request("prepare", {**handle, "source": "source.stl", "build_plate": "Textured PEI Plate",
            "printer": {"name": "Bambu Lab H2D 0.4 nozzle"},
            "process": {"name": "0.20mm Standard @BBL H2D"},
            "filaments": [{"name": "Bambu PLA Basic @BBL H2D"}, {"name": "Bambu PETG HF @BBL H2D 0.4 nozzle"}],
            "auto_arrange": True, "timeout_seconds": 300})
        deadline = time.monotonic() + 360
        while state["status"] == "running" and time.monotonic() < deadline:
            time.sleep(0.5)
            state = request("status", handle)
        if state["status"] != "completed":
            log = project / handle["output_dir"] / "build-log.json"
            raise RuntimeError(f"H2D slice failed: {state}; {log.read_text() if log.exists() else 'no log'}")
        toolpath = next(name for name in state["artifacts"] if name.endswith(".gcode"))
        gcode = (project / handle["output_dir"] / toolpath).read_text()
        assert "; filament_type = PLA;PETG" in gcode, "both H2D filaments did not reach Orca"
        handles.append(handle)
        print("Native H2D two-filament slice passed")
        worker.terminate()
        worker.wait(timeout=10)
        worker = start()
        for handle in handles:
            assert request("status", handle)["status"] == "completed"
        print("Native A1/X1C slices, overrides, slot colours, vendor profiles, package thumbnails, "
              "actual toolpath images, and restart recovery passed")
    finally:
        worker.terminate()
        worker.wait(timeout=10)
