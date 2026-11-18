"""Exercise packaged Orca slicing, toolpath previews, and restart recovery."""
import base64
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
from capture_fixture import COLORS, create_banded_fixture, create_fixture, verify_bed_surface, verify_camera_replay, verify_capture
from preparation_fixture import create_software_profiles, extruded_fixture, linear_deposition_paths, object_material_assignments, tower_paths

ENGINE_SOURCE_REVISION = "6320f7282263ac81d5a133c47e77e08191b409a2"


def verify_thumbnail(payload, name):
    with Image.open(io.BytesIO(payload)) as image:
        image.load()
        assert image.format == "PNG", f"thumbnail is not PNG: {name}"
        assert min(image.size) > 0
        rgba = image.convert("RGBA")
        background = Image.new("RGBA", rgba.size, "white")
        background.alpha_composite(rgba)
        assert any(low != high for low, high in background.convert("RGB").getextrema()), f"blank thumbnail: {name}"


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
            verify_thumbnail(package.read(name), name)


def verify_library_preview():
    body = json.dumps({"project_id": "native-slice", "source": "source.stl"}).encode()
    req = urllib.request.Request("http://127.0.0.1:8003/preview", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as response:
        payload = base64.b64decode(json.load(response)["png_base64"], validate=True)
    verify_thumbnail(payload, "project library preview")


def verify_paint(project, plain):
    painted = project / "painted.3mf"
    assigned = False
    with zipfile.ZipFile(plain) as original, zipfile.ZipFile(painted, "w", zipfile.ZIP_DEFLATED) as output:
        for name in original.namelist():
            data = original.read(name)
            if name.endswith(".model") and not assigned:
                mesh = ET.fromstring(data)
                triangle = mesh.find(".//{*}triangle")
                if triangle is not None:
                    for key, value in {"paint_supports": "4", "paint_seam": "8",
                                       "paint_color": "8", "paint_fuzzy_skin": "4"}.items():
                        triangle.set(key, value)
                    data = ET.tostring(mesh, encoding="utf-8", xml_declaration=True)
                    assigned = True
            output.writestr(name, data)
    assert assigned, "the paint fixture did not contain a mesh triangle"
    invalid = project / "invalid-paint.3mf"
    invalid.write_text("Invalid synthetic test project\n")
    hashes = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in (plain, painted, invalid)}

    def inspect(path):
        return request("inspect_paint", {"project_id": "native-slice",
                                         "source": str(path.relative_to(project))})

    clean = inspect(plain)
    assert clean["source_sha256"] == hashes[plain]
    assert clean["summary"]["painted_facets_total"] == 0 and not clean["painted_volumes"]
    assert not any(clean["categories"].values())
    result = inspect(painted)
    assert result["source_sha256"] == hashes[painted]
    assert result["summary"]["painted_facets_total"] == 4 and all(result["categories"].values()), result
    assert result["painted_volumes"] == [{
        "object": 1, "volume": 1, "supports": {"enforcer_facets": 1, "blocker_facets": 0},
        "seam": {"enforcer_facets": 0, "blocker_facets": 1},
        "material": [{"filament": 2, "facets": 1}], "fuzzy_skin_facets": 1,
    }]
    try:
        inspect(invalid)
    except urllib.error.HTTPError as error:
        assert error.code == 422 and "could not inspect 3MF paint" in json.load(error)["error"]
    else:
        raise AssertionError("native paint inspection accepted an invalid project")
    assert all(hashlib.sha256(path.read_bytes()).hexdigest() == digest for path, digest in hashes.items())


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
    legacy = project / "orca-2.4.2-flat.3mf"
    legacy.write_bytes((Path(__file__).parent / "fixtures" / legacy.name).read_bytes())
    legacy_hash = hashlib.sha256(legacy.read_bytes()).hexdigest()
    software_profile_root = root / "profiles/BBL"
    software_profiles = create_software_profiles(
        os.environ.get("PRINTABLE_SLICER_PROFILES", "/opt/orca-worker-profiles/BBL"),
        software_profile_root)
    substitution_profile = "Printable compatibility fixture"
    (software_profile_root / "process" / "compatibility-fixture.json").write_text(json.dumps({
        "type": "process", "name": substitution_profile, "inherits": "0.20mm Standard @BBL X1C",
        "from": "user", "instantiation": "true", "ironing_type": "unknown_compatibility_fixture_value",
    }))
    env = {**os.environ, "PRINTABLE_WORKSPACE_ROOT": str(root),
           "PRINTABLE_SLICER_PROFILES": str(software_profile_root)}

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
        assert readiness["engine"] == "OrcaSlicer" and readiness["version"] == "2.5.1"
        assert readiness["native_capture"] is True
        assert all(count > 0 for count in readiness["profile_counts"].values())
        verify_library_preview()
        vendor = request("profiles", {"category": "filament", "query": "Fiberon",
                                      "printer": "Bambu Lab X1 Carbon 0.4 nozzle"})
        assert vendor["total"] > 0, "vendor subfolder profiles are missing"
        library_child = request("settings", {"category": "filament", "profile": {"name": "AliZ PLA @P1-X1"},
                                             "query": "nozzle_temperature"})
        assert any(row["value"] for row in library_child["settings"]), "shared library parent did not resolve"
        hidden = request("settings", {"category": "process", "profile": {"name": "0.20mm Standard @BBL X1C"},
                                      "query": "brim_type"})
        assert any(row["key"] == "brim_type" and row["overrideable"] for row in hidden["settings"])
        handles = []
        terminal_states = {}
        colours = {"a1": "#0A2CE3", "x1c-reslice": "#C12E1F"}
        for model, suffix, printer in [("a1", "A1", "Bambu Lab A1 0.4 nozzle"),
                                        ("x1c", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("x1c-reslice", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("legacy-reslice", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("capture-template", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle")]:
            profiles = request("profiles", {"category": "process", "printer": printer, "query": "0.20mm Standard"})
            assert profiles["total"] > 0
            inspected = request("settings", {"category": "filament", "profile": {"name": f"Bambu PLA Basic @BBL {suffix}"}, "query": "plate_temp"})
            assert any(row["key"] == "hot_plate_temp" for row in inspected["settings"])
            handle = {"project_id": "native-slice", "output_dir": model}
            process = {"name": f"0.20mm Standard @BBL {suffix}"}
            if model == "x1c":
                process["name"] = substitution_profile
            filament = {"name": "AliZ PLA @P1-X1" if model == "x1c-reslice"
                        else f"Bambu PLA Basic @BBL {suffix}"}
            if model in ("a1", "legacy-reslice"):
                process["overrides"] = {"brim_type": "no_brim"}
                filament["overrides"] = {"pressure_advance": ["0.031"]}
            if model == "legacy-reslice":
                process["overrides"].update({"layer_height": "0.16", "enable_arc_fitting": "0"})
            if model in colours:
                filament["colour"] = colours[model]
            source = {"x1c-reslice": "x1c/model.gcode.3mf", "legacy-reslice": legacy.name}.get(model, "source.stl")
            state = request("prepare", {**handle, "source": source,
                "build_plate": "High Temp Plate" if model == "x1c" else "Textured PEI Plate",
                "printer": {"name": printer},
                "process": process,
                "filaments": ([{**filament, "colour": color} for color in COLORS]
                              if model == "capture-template" else [filament]),
                "auto_arrange": model != "legacy-reslice", "timeout_seconds": 300})
            deadline = time.monotonic() + 360
            while state["status"] == "running" and time.monotonic() < deadline:
                time.sleep(0.5)
                state = request("status", handle)
            if state["status"] != "completed":
                log = project / model / "build-log.json"
                raise RuntimeError(f"slice failed: {state}; {log.read_text() if log.exists() else 'no log'}")
            if model == "x1c":
                assert state["settings_substitutions"] == {
                    "state": "reported", "keys": ["ironing_type"], "truncated": False}
                retained_result = (project / model / "result.json").read_text()
                assert "warnings" not in json.loads(retained_result)
                assert "unknown_compatibility_fixture_value" not in retained_result
            assert state["setup"]["build_plate"] == ("High Temp Plate" if model == "x1c" else "Textured PEI Plate")
            assert isinstance(state["progress"]["total_percent"], (int, float))
            toolpath = next(name for name in state["artifacts"] if name.endswith(".gcode"))
            gcode = (project / model / toolpath).read_text()
            assert f'; curr_bed_type = {state["setup"]["build_plate"]}' in gcode
            temperature = state["setup"]["filaments"][0]["bed_temperature_initial_layer"][0]
            assert f"M190 S{temperature}" in gcode or f"M140 S{temperature}" in gcode
            if model in ("a1", "legacy-reslice"):
                assert "; brim_type = no_brim" in gcode, "process override did not reach Orca"
                assert "; pressure_advance = 0.031" in gcode, "filament override did not reach Orca"
            if model == "legacy-reslice":
                assert "; layer_height = 0.16" in gcode and "; enable_arc_fitting = disabled" in gcode
                assert state["source_sha256"] == legacy_hash
                assert hashlib.sha256(legacy.read_bytes()).hexdigest() == legacy_hash
            if model == "x1c-reslice":
                assert '; filament_settings_id = "AliZ PLA @P1-X1"' in gcode, (
                    "selected shared filament did not reach Orca")
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
        verify_paint(project, project / "capture-template/model.gcode.3mf")
        fixture_path = project / "capture-layout.3mf"
        fixture = create_fixture(project / "capture-template/model.gcode.3mf", fixture_path)
        source_hash = hashlib.sha256(fixture_path.read_bytes()).hexdigest()
        capture_params = {"project_id": "native-slice",
                          "source": {"kind": "project", "path": fixture_path.name},
                          "width": 1600, "height": 1000}
        overview = request("capture", capture_params)
        assert overview["source_sha256"] == source_hash
        build_revision = overview["engine"]["source_revision"]
        assert len(build_revision) >= 7 and ENGINE_SOURCE_REVISION.startswith(build_revision), (
            "native AppImage was built from a different source revision", build_revision)
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

        # These isolated child profiles retain native BBL tower generation but
        # replace firmware scripts. Their output must never reach a printer.
        material_source = project / "material-layout.3mf"
        material_fixture = create_fixture(project / "capture-template/model.gcode.3mf", material_source,
                                          part_heights_mm=(2, 4), plate_count=2)
        material_hash = hashlib.sha256(material_source.read_bytes()).hexdigest()
        inspected = request("inspect_layout", {"project_id": "native-slice", "source": material_source.name,
            "output_dir": "material-source-inspection", "expected_source_sha256": material_hash})
        assert inspected["source_sha256"] == material_hash and inspected["instances_count"] == 4
        assert {item["native_plate"] for item in inspected["instances"]} == {1, 2}
        common_overrides = {"enable_prime_tower": "1", "wipe_tower_wall_type": "rectangle",
            "prime_tower_width": "35", "prime_tower_brim_width": "3",
            "prime_tower_enable_framework": "1", "wipe_tower_rotation_angle": "0"}
        material_results = {}

        def material_trial(name, *, overrides=None, layout=None, source=None, clearance=None, plate=0,
                           expected_source_state="passed", constraint=None, require_towers=True, linear=False):
            handle = {"project_id": "native-slice", "output_dir": name}
            params = {**handle, "source": source or material_source.name, "plate": plate,
                "build_plate": "Textured PEI Plate", "printer": {"name": software_profiles["linear_printer" if linear else "printer"]},
                "process": {"name": software_profiles["process"],
                            "overrides": {**common_overrides, **(overrides or {})}},
                "filaments": [{"name": software_profiles["pla"], "colour": color} for color in COLORS],
                "auto_arrange": False, "auto_orient": False, "timeout_seconds": 300}
            if layout is not None:
                params["layout"] = layout
            if clearance is not None:
                params["clearance"] = clearance
            state = request("prepare", params)
            deadline = time.monotonic() + 360
            while state["status"] == "running" and time.monotonic() < deadline:
                time.sleep(0.5)
                state = request("status", handle)
            assert state["source_layout"]["state"] == expected_source_state, state.get("source_layout")
            if state["status"] != "completed":
                expected_reasons = {
                    "crowded": ("collision", "too close", "overlap", "g-code conflicts detected"),
                    "edge": ("outside", "exclusion", "bounds", "border", "out of", "not fit", "too close", "unprintable area"),
                    "sequential": ("collision", "too close", "by object", "sequential", "prime tower", "wipe tower"),
                    "slender": ("unprintable area", "object conflicts were detected"),
                    "compacted_tower": ("object conflicts were detected",),
                }
                if (constraint and state["status"] == "failed"
                        and any(reason in state.get("error", "").lower() for reason in expected_reasons[constraint])):
                    handles.append(handle)
                    terminal_states[name] = "failed"
                    material_results[name] = {"state": state, "towers": []}
                    print("NATIVE_CONSTRAINT_REJECTION", name, state["error"], flush=True)
                    return []
                log = project / name / "build-log.json"
                raise RuntimeError(f"material fixture failed: {state}; {log.read_text() if log.exists() else 'no native log'}")
            paths = sorted(key for key in state["artifacts"] if key.endswith(".gcode"))
            assert len(paths) == (2 if plate == 0 else 1), ("native selected plate toolpaths missing", paths)
            measured = ([tower_paths((project / name / path).read_text()) for path in paths]
                        if require_towers else [])
            final_report = project / name / "final-layout.json"
            final_bytes = final_report.read_bytes()
            assert hashlib.sha256(final_bytes).hexdigest() == state["final_layout"]["report_sha256"]
            final = json.loads(final_bytes)
            assert final["native_package_sha256"] == state["artifacts"]["model.gcode.3mf"]["sha256"]
            preflight_bytes = (project / name / "preflight.json").read_bytes()
            assert hashlib.sha256(preflight_bytes).hexdigest() == state["preflight"]["report_sha256"]
            retained_preflight = json.loads(preflight_bytes)
            assert (retained_preflight["source_layout"]["final_layout_preservation"]
                    == state["source_layout"]["final_layout_preservation"]
                    == state["final_layout"]["state"]), "retained and inline placement preservation disagree"
            assert state["final_layout"]["state"] in (("passed", "unmeasured", "failed") if constraint else ("passed", "unmeasured")), state["final_layout"]
            assert state["final_layout"]["authored_feature_preservation"] == "unverified_after_native_export"
            assert state["final_layout"]["material_assignment_preservation"] == "unverified_after_native_export"
            for evidence in state["preflight"]["plates"].values():
                assert evidence["coordinate_frame"] == "final_gcode" and evidence["units"] == "mm"
                if require_towers:
                    if linear:
                        assert any(item["role"] == "prime_tower" for item in evidence["deposited_footprints"]["items"]), evidence
                    else:
                        assert evidence["status"] in ("blocked", "unresolved") and any(
                            reason.startswith("unsupported_coordinate_or_motion_command:M")
                            for reason in evidence["unresolved"]), evidence
            handles.append(handle)
            material_results[name] = {"state": state, "towers": measured}
            print("NATIVE_PREPARATION_RESULT", json.dumps({"case": name,
                "profile_scope": "synthetic_linear_type1" if linear else "bbl_type1_with_unmodeled_firmware",
                "source_sha256": state["source_sha256"], "slice_provenance": state["provenance"],
                "native_package_sha256": state["artifacts"]["model.gcode.3mf"]["sha256"],
                "toolpath_sha256": {path: state["artifacts"][path]["sha256"] for path in paths},
                "towers": [{"minimum_mm": tower["minimum_mm"], "maximum_mm": tower["maximum_mm"],
                            "deposited_paths": len(tower["paths"]), "toolchanges": len(tower["toolchanges"])}
                           for tower in measured],
                "model_outer_wall_centerline_bounds_mm": [tower["model_outer_wall_centerline_bounds_mm"] for tower in measured],
                "source_layout_state": state["source_layout"]["state"], "final_layout_state": state["final_layout"]["state"],
                "preflight_status": state["preflight"]["status"], "physical_result": "not_observed"}), flush=True)
            return measured

        baseline_towers = material_trial("material-baseline")
        selected = next(item for item in inspected["instances"] if item["build_index"] == 0)
        placed_transform = list(selected["build_transform_3mf"])
        placed_transform[9] += 5
        original_tower = (256 * 0.73, 256 * 0.62)
        shifted_towers = material_trial("material-shifted", layout={"source_sha256": material_hash,
            "placements": [{"build_index": 0, "transform_3mf": placed_transform}],
            "tower_positions": [{"plate": 2, "position_mm": [original_tower[0] + 8, original_tower[1] + 6]}],
            "minimum_gap_mm": 3})
        for edge in ("minimum_mm", "maximum_mm"):
            assert all(abs(a - b) < 0.02 for a, b in zip(baseline_towers[0][edge], shifted_towers[0][edge])), (
                "unselected plate tower moved", edge)
            assert all(abs((b - a) - expected) < 0.02 for a, b, expected in
                       zip(baseline_towers[1][edge], shifted_towers[1][edge], (8, 6, 0))), (
                "selected tower placement did not affect native paths", edge)
        for plate_index, (baseline, shifted) in enumerate(zip(baseline_towers, shifted_towers)):
            for material in ("0", "1"):
                expected_shift = (5, 0, 0) if plate_index == 0 and material == "0" else (0, 0, 0)
                for edge in ("minimum_mm", "maximum_mm"):
                    original = baseline["model_outer_wall_centerline_bounds_mm"][material][edge]
                    placed = shifted["model_outer_wall_centerline_bounds_mm"][material][edge]
                    assert all(abs((b - a) - delta) < 0.02 for a, b, delta in zip(original, placed, expected_shift)), (
                        "selected model placement or unselected model preservation failed in actual toolpaths", plate_index, material, edge)
        wide_towers = material_trial("material-wide", overrides={"prime_tower_width": "45"})
        for baseline, wider in zip(baseline_towers, wide_towers):
            assert wider["maximum_mm"][0] - wider["minimum_mm"][0] > (
                baseline["maximum_mm"][0] - baseline["minimum_mm"][0] + 5), "rectangle tower width override was ineffective"
        rotated_towers = material_trial("material-rotated", overrides={"wipe_tower_rotation_angle": "45"})
        assert any(abs(a - b) > 0.5 for a, b in zip(baseline_towers[0]["maximum_mm"], rotated_towers[0]["maximum_mm"])), (
            "pinned BBL rotation path did not affect actual tower coordinates")
        cone_towers = material_trial("material-cone", overrides={"wipe_tower_cone_angle": "60"})
        for baseline, cone in zip(baseline_towers, cone_towers):
            def coordinates(tower):
                return sorted((path["material"], tuple(sorted(tuple(round(value, 6) for value in path[edge])
                    for edge in ("from_mm", "to_mm")))) for path in tower["paths"])
            assert coordinates(baseline) == coordinates(cone), "Type 1 cone unexpectedly changed generated tower paths"
        framework_off = material_trial("material-framework-off", overrides={"prime_tower_enable_framework": "0"})
        assert coordinates(framework_off[0]) == coordinates(baseline_towers[0]), "constant-depth framework fixture behavior changed"
        print("NATIVE_CONTROL_EFFECT", json.dumps({"control": "prime_tower_enable_framework",
            "effect": "not_observed_in_constant_depth_fixture", "physical_result": "not_observed"}), flush=True)
        larger_brim = material_trial("material-larger-brim", overrides={"prime_tower_brim_width": "8"})
        assert any(abs(a - b) > 1 for a, b in zip(baseline_towers[0]["minimum_mm"], larger_brim[0]["minimum_mm"])), "tower brim override did not change actual footprint"
        material_trial("material-obstructed-head", linear=True, clearance={
            "initial_position": {"position_mm": [0, 0, 0], "source": "explicit software fixture start"},
            "machine_envelopes": [{"component": "synthetic obstructing head", "minimum_mm": [-30, -30, -0.1],
                "maximum_mm": [30, 30, 10], "source": "deliberate software regression obstruction; no hardware claim"}]})
        obstructed = material_results["material-obstructed-head"]["state"]
        assert obstructed["preflight"]["status"] == "blocked", obstructed["preflight"]
        assert any(finding["kind"] == "toolhead_interference" for evidence in obstructed["preflight"]["plates"].values()
                   for finding in evidence["findings"]), "successful native slice did not retain the known head obstruction"
        slender_source = project / "material-slender.3mf"
        create_fixture(project / "capture-template/model.gcode.3mf", slender_source,
                       part_heights_mm=(20, 30), plate_count=1, xy_scale=(0.35, 0.35))
        material_trial("material-slender-sparse", source=slender_source.name, plate=1, constraint="slender",
                       overrides={"wipe_tower_no_sparse_layers": "1", "prime_tower_width": "8"})
        sparse_state = material_results["material-slender-sparse"]["state"]
        if sparse_state["status"] == "completed":
            assert sparse_state["preflight"]["status"] in ("blocked", "unresolved"), sparse_state["preflight"]
        banded_source = project / "material-banded.3mf"
        banded = create_banded_fixture(project / "capture-template/model.gcode.3mf", banded_source)
        banded_hash = hashlib.sha256(banded_source.read_bytes()).hexdigest()
        material_trial("banded-compact-refusal", source=banded_source.name, plate=1,
            require_towers=False, linear=True, constraint="compacted_tower",
            overrides={"prime_tower_width": "8", "wipe_tower_no_sparse_layers": "1"},
            layout={"source_sha256": banded_hash, "tower_positions": [{"plate": 1,
                "position_mm": banded["tower_position_mm"]}]})
        assert material_results["banded-compact-refusal"]["state"]["status"] == "failed"
        rejection = json.loads((project / "banded-compact-refusal" / "build-log.json").read_text())
        assert "No sparse layers" in rejection["stderr"], "native refusal did not identify compacted tower clearance"
        # The wider tower with more XY separation passes the engine's clearance
        # check. The independent envelope still detects a late descent.
        separated_tower = [coordinate + 55 for coordinate in banded["tower_position_mm"]]
        banded_evidence = {}
        for skip_sparse in (False, True):
            name = f"banded-sparse-{int(skip_sparse)}"
            material_trial(name, source=banded_source.name, plate=1, require_towers=False, linear=True,
                overrides={"prime_tower_width": "20", "wipe_tower_no_sparse_layers": str(int(skip_sparse))},
                layout={"source_sha256": banded_hash, "tower_positions": [{"plate": 1,
                    "position_mm": separated_tower}]},
                clearance={"initial_position": {"position_mm": [0, 0, 0], "source": "explicit software fixture start"},
                    "machine_envelopes": [{"component": "synthetic sparse-descent head", "minimum_mm": [-93, -71, 0.01],
                        "maximum_mm": [93, 71, 14], "source": "deliberate software fixture envelope; no hardware claim"}]})
            state = material_results[name]["state"]
            toolpath = next(key for key in state["artifacts"] if key.endswith(".gcode"))
            descents = []
            deposition, changes = linear_deposition_paths((project / name / toolpath).read_text(),
                                                          descents=descents)
            tower = [path for path in deposition if path["feature"] in ("Prime tower", "Wipe tower")]
            assert len(tower) > 8 and len(changes) >= 2 and {material for _, material in changes} >= {0, 1}
            assert any(path["to_mm"][2] > 11 for path in deposition
                       if path["feature"] not in ("Prime tower", "Wipe tower"))
            banded_evidence[skip_sparse] = {"tower": tower, "state": state, "deposition": deposition,
                                            "descents": descents}
        dense_height = max(path["to_mm"][2] for path in banded_evidence[False]["tower"])
        sparse_height = max(path["to_mm"][2] for path in banded_evidence[True]["tower"])
        tower_levels = {sparse: {round(path["to_mm"][2], 4) for path in banded_evidence[sparse]["tower"]}
                        for sparse in (False, True)}
        assert len(tower_levels[False]) > len(tower_levels[True]), "sparse-layer override did not remove intermediate tower layers"
        tower_features = ("Prime tower", "Wipe tower")
        tall_part_line = next(path["line"] for path in banded_evidence[True]["deposition"]
                              if path["feature"] not in tower_features and path["to_mm"][2] > 8)
        late_descent_lines = {path["line"] for path in banded_evidence[True]["descents"]
                              if path["line"] > tall_part_line and path["feature"] in tower_features
                              and path["to_mm"][2] < 5}
        assert late_descent_lines, "fixture did not exercise a real late descent to the sparse tower"
        sparse_report = json.loads((project / "banded-sparse-1/preflight.json").read_text())
        blocked_lines = late_descent_lines.intersection(
            finding["source_line"] for plate in sparse_report["plates"].values()
            for finding in plate["findings"] if finding["kind"] == "toolhead_interference")
        assert sparse_report["status"] == "blocked" and blocked_lines, sparse_report
        print("NATIVE_SPARSE_DESCENT", json.dumps({"dense_tower_height_mm": dense_height,
            "sparse_tower_height_mm": sparse_height, "blocked_descent_lines": sorted(blocked_lines),
            "physical_result": "not_observed"}), flush=True)
        assert not any(finding["kind"] == "toolhead_interference" for plate in
            banded_evidence[False]["state"]["preflight"]["plates"].values() for finding in plate["findings"])
        assert hashlib.sha256(banded_source.read_bytes()).hexdigest() == banded_hash
        first, second = [next(item for item in inspected["instances"] if item["build_index"] == index)
                         for index in (0, 1)]
        centers = [[(item["bounds"]["minimum_mm"][axis] + item["bounds"]["maximum_mm"][axis]) / 2
                    for axis in (0, 1)] for item in (first, second)]
        crowded_transform = list(second["build_transform_3mf"])
        for axis in (0, 1):
            crowded_transform[9 + axis] += centers[0][axis] - centers[1][axis]
        material_trial("material-crowded", plate=1, constraint="crowded", expected_source_state="failed",
            require_towers=False, layout={"source_sha256": material_hash,
                "placements": [{"build_index": 1, "transform_3mf": crowded_transform}]})
        crowded = material_results["material-crowded"]["state"]
        assert any(finding["kind"] == "source_part_interference" for finding in crowded["source_layout"]["findings"])
        if crowded["status"] == "completed":
            assert crowded["preflight"]["status"] == "blocked"
        material_trial("material-fused-interface", plate=1, constraint="crowded", require_towers=False,
            layout={"source_sha256": material_hash,
                "placements": [{"build_index": 1, "transform_3mf": crowded_transform}],
                "intended_interfaces": [{"build_indices": [0, 1],
                    "reason": "Deliberate software fixture fused interface; no physical bond claim"}]})
        fused = material_results["material-fused-interface"]["state"]
        assert fused["source_layout"]["state"] == "passed" and not fused["source_layout"]["findings"]
        fused_report = json.loads((project / "material-fused-interface/source-layout.json").read_text())
        assert any(pair.get("intended_interface") and pair["solid_analysis"]["relation"] == "interfering"
                   for pair in fused_report["pairs"]), "intended interface lost its actual interference evidence"
        material_trial("material-tower-edge", plate=1, constraint="edge", require_towers=False,
            layout={"source_sha256": material_hash,
                    "tower_positions": [{"plate": 1, "position_mm": [-10, 160]}]})
        edge = material_results["material-tower-edge"]["state"]
        if edge["status"] == "completed":
            assert edge["preflight"]["status"] in ("blocked", "unresolved"), edge["preflight"]
            assert edge["final_layout"]["state"] in ("failed", "unmeasured"), edge["final_layout"]
        sequential_transform = list(crowded_transform)
        sequential_transform[9] += 25
        material_trial("material-sequential", plate=1, constraint="sequential", require_towers=False, linear=True,
            overrides={"print_sequence": "by object", "enable_prime_tower": "0"},
            layout={"source_sha256": material_hash,
                "placements": [{"build_index": 1, "transform_3mf": sequential_transform}]},
            clearance={"initial_position": {"position_mm": [0, 0, 0], "source": "explicit software fixture start"},
                "machine_envelopes": [{"component": "synthetic sequential head", "minimum_mm": [-30, -30, 0.01],
                    "maximum_mm": [30, 30, 10], "source": "deliberate software fixture envelope; no hardware claim"}]})
        sequential = material_results["material-sequential"]["state"]
        if sequential["status"] == "completed":
            assert sequential["preflight"]["status"] == "blocked", sequential["preflight"]
            assert any(finding["kind"] == "toolhead_interference" for evidence in sequential["preflight"]["plates"].values()
                       for finding in evidence["findings"])
        resliced_towers = material_trial("material-reslice", source="material-baseline/model.gcode.3mf")
        for plate, (baseline, resliced) in enumerate(zip(baseline_towers, resliced_towers), start=1):
            assert coordinates(baseline) == coordinates(resliced), "reslice changed actual tower paths or materials"
            for material in ("0", "1"):
                for edge in ("minimum_mm", "maximum_mm"):
                    original = baseline["model_outer_wall_centerline_bounds_mm"][material][edge]
                    retained = resliced["model_outer_wall_centerline_bounds_mm"][material][edge]
                    assert all(abs(a-b) < 0.02 for a,b in zip(original,retained)), "reslice changed actual model placement"
            origin = [0 if plate == 1 else 256 * 1.2, 0, 0]
            objects = [{"object": obj["object"], "material": obj["object"] - 1,
                        "bounds_mm": {side: [value - offset for value, offset in zip(obj["bounds_mm"][side], origin)]
                                      for side in ("min", "max")}}
                       for obj in material_fixture["objects"] if obj["plate"] == plate]
            assignments = []
            for name in ("material-baseline", "material-reslice"):
                paths = sorted(key for key in material_results[name]["state"]["artifacts"] if key.endswith(".gcode"))
                assignments.append(object_material_assignments((project / name / paths[plate - 1]).read_text(), objects))
            assert assignments[0] == assignments[1], "reslice changed object material assignments"
            material_results["material-reslice"].setdefault("object_material_assignments", {})[str(plate)] = assignments[1]
        assert hashlib.sha256(material_source.read_bytes()).hexdigest() == material_hash
        print("Native repeated material changes, two-plate tower paths, position, rectangle width, rotation, cone applicability and reslicing passed")

        def surface_trial(name, kind, overrides, *, interface=False):
            source = project / f"{kind}-fixture.stl"
            design = extruded_fixture(source, kind)
            source_hash = hashlib.sha256(source.read_bytes()).hexdigest()
            handle = {"project_id": "native-slice", "output_dir": name}
            materials = [{"name": software_profiles["pla"], "colour": color}
                         for color in (COLORS if interface else COLORS[:1])]
            state = request("prepare", {**handle, "source": source.name,
                "build_plate": "Textured PEI Plate", "printer": {"name": software_profiles["printer"]},
                "process": {"name": software_profiles["process"], "overrides": overrides},
                "filaments": materials, "auto_arrange": True, "auto_orient": False, "timeout_seconds": 300})
            deadline = time.monotonic() + 360
            while state["status"] == "running" and time.monotonic() < deadline:
                time.sleep(0.5)
                state = request("status", handle)
            if state["status"] != "completed":
                log = project / name / "build-log.json"
                raise RuntimeError(f"surface fixture failed: {state}; {log.read_text() if log.exists() else 'no native log'}")
            toolpath = next(key for key in state["artifacts"] if key.endswith(".gcode"))
            gcode = (project / name / toolpath).read_text()
            paths, changes = linear_deposition_paths(gcode)
            assert len(paths) > 10, "surface fixture has no useful actual deposition"
            final = json.loads((project / name / "final-layout.json").read_text())
            instances = final["layout"]["instances"]
            assert instances, "final native surface geometry was not inspected"
            bounds = instances[0]["footprint"]["bounds"]
            assert all(abs(a - b) < 0.001 for a, b in zip(bounds["dimensions_mm"], design["bounds_mm"]["max"])), (
                "native slice changed a fitting-face dimension", bounds, design)
            review = request("review", {"slice": handle, "toolpath": toolpath,
                                       "first_layer": 1, "last_layer": 1000})
            assert review["estimates"]["time"] and review["estimates"]["filament_grams"], review["estimates"]
            assert review["provenance"] and review["slice_provenance"] == state["provenance"]
            assert hashlib.sha256(source.read_bytes()).hexdigest() == source_hash
            handles.append(handle)
            return {"handle": handle, "state": state, "design": design, "review": review,
                    "paths": paths, "changes": changes, "bounds": bounds, "toolpath": toolpath}

        flat_baseline = surface_trial("flat-baseline", "flat", {"ironing_type": "no ironing"})
        flat_finish = surface_trial("flat-finish", "flat", {"ironing_type": "top"})
        assert not any(path["feature"] == "Ironing" for path in flat_baseline["paths"])
        assert sum(path["feature"] == "Ironing" for path in flat_finish["paths"]) > 8
        def comparison_candidate(result, label, rationale):
            return {"label": label, "slice": result["handle"],
                "slice_provenance": result["state"]["provenance"], "rationale": rationale,
                "reviews": [result["review"]["provenance"]],
                "conditions": {"calibration": "Software-only fixture; no physical calibration or print",
                               "material": "Same software PLA slot and declared conditioning"}}
        comparison = request("compare", {"project_id": "native-slice", "output_dir": "flat-comparison",
            "objective": "finish_named_faces", "critical_faces": ["Horizontal top face; preserve fitting rim"],
            "baseline": comparison_candidate(flat_baseline, "baseline", "Keep the standard non-ironed top"),
            "candidates": [comparison_candidate(flat_finish, "ironed", "Compare native ironing paths and estimates")]})
        comparison_path = root / comparison["report_artifact"]["path"]
        comparison_bytes = comparison_path.read_bytes()
        assert hashlib.sha256(comparison_bytes).hexdigest() == comparison["report_sha256"]
        assert comparison["automatic_selection"] == "not_performed" and comparison["physical_result"] == "not_observed"
        assert comparison["baseline"]["slice_provenance"] == flat_baseline["state"]["provenance"]
        assert comparison["candidates"][0]["slice_provenance"] == flat_finish["state"]["provenance"]
        for candidate in (comparison["baseline"], comparison["candidates"][0]):
            assert candidate["current_source"] == "matches_retained_source"
            assert candidate["estimates"] and all(estimate["time_native"] is not None
                and estimate["filament_grams_by_slot_native"] is not None
                and estimate["support_grams"] is None and estimate["tower_grams"] is None
                and estimate["flushing_grams"] is None for estimate in candidate["estimates"].values())
        for candidate, sliced in zip((comparison["baseline"], comparison["candidates"][0]),
                                     (flat_baseline, flat_finish)):
            assert candidate["estimates"][sliced["toolpath"]]["time_native"] == sliced["review"]["estimates"]["time"], (
                "comparison and review disagree on the native total-time estimate")
        relief = surface_trial("relief-detail", "relief", {"ironing_type": "no ironing"})
        x_origin = relief["bounds"]["minimum_mm"][0]
        ridges = [path for path in relief["paths"] if 1.05 < path["to_mm"][2] < 1.65
                  and path["feature"] not in ("Prime tower", "Wipe tower", "Brim", "Skirt")]
        for low, high in ((8, 9), (13, 14)):
            assert any(low - 0.3 < path["to_mm"][0] - x_origin < high + 0.3 for path in ridges), (
                "raised relief ridge disappeared from actual toolpaths", low, high)
        assert not any(min(path["from_mm"][0], path["to_mm"][0]) < x_origin + 9.5
                       and max(path["from_mm"][0], path["to_mm"][0]) > x_origin + 12.5 for path in ridges), (
            "separate relief ridges merged in actual deposition")
        interface = surface_trial("support-interface", "bridge", {**common_overrides,
            "enable_support": "1", "support_type": "normal(auto)", "support_on_build_plate_only": "1",
            "support_filament": "1", "support_interface_filament": "2",
            "support_interface_top_layers": "3", "support_top_z_distance": "0.2"}, interface=True)
        interface_paths = [path for path in interface["paths"] if path["feature"] == "Support interface"]
        assert len(interface_paths) > 8 and {path["material"] for path in interface_paths} == {1}, (
            "assigned interface material did not produce actual interface paths", len(interface_paths))
        assert any(3.2 < path["to_mm"][2] < 4.01 for path in interface_paths), "supported underside layers were not exercised"
        assert {material for _, material in interface["changes"]} >= {0, 1}
        underside = request("review", {"slice": interface["handle"], "toolpath": interface["toolpath"],
            "first_layer": 16, "last_layer": 22, "features": ["Support interface"], "material": 1})
        assert underside["segments"] > 0 and underside["provenance"]
        reference = flat_baseline["state"]["provenance"]
        record = (root / reference["path"]).read_bytes()
        assert hashlib.sha256(record).hexdigest() == reference["sha256"]
        engine = json.loads(record)["data"]["engine"]
        assert engine["version"] == "2.5.1" and engine["source_revision"] == ENGINE_SOURCE_REVISION
        evidence = {"engine_source_revision": engine["source_revision"],
            "physical_result": "not_observed", "flat_top": [{"slice": result["handle"],
                "slice_provenance": result["state"]["provenance"], "review": result["review"]["provenance"],
                "estimates": result["review"]["estimates"]} for result in (flat_baseline, flat_finish)],
            "relief_review": relief["review"]["provenance"], "underside_review": underside["provenance"],
            "limits": ["Software fixture ironing parameters are not physically calibrated.",
                       "No surface roughness, fit performance, adhesion or dissimilar-material bond is established."]}
        (project / "native-preparation-evidence.json").write_text(json.dumps(evidence, indent=2))
        print("Native flat-top comparison estimates, ironing paths, separate relief ridges, fitting dimensions and support-interface transitions passed", json.dumps(evidence))
        def h2d_trial(name, source):
            handle = {"project_id": "native-slice", "output_dir": name}
            state = request("prepare", {**handle, "source": source, "plate": 1,
                "build_plate": "Textured PEI Plate", "printer": {"name": "Bambu Lab H2D 0.4 nozzle"},
                "process": {"name": "0.20mm Standard @BBL H2D"},
                "filaments": [{"name": "Bambu PLA Basic @BBL H2D"},
                              {"name": "Bambu PETG HF @BBL H2D 0.4 nozzle"}],
                "auto_arrange": False, "timeout_seconds": 300})
            deadline = time.monotonic() + 360
            while state["status"] == "running" and time.monotonic() < deadline:
                time.sleep(0.5)
                state = request("status", handle)
            if state["status"] != "completed":
                log = project / name / "build-log.json"
                raise RuntimeError(f"H2D slice failed: {state}; {log.read_text() if log.exists() else 'no log'}")
            toolpath = next(key for key in state["artifacts"] if key.endswith(".gcode"))
            lines = (project / name / toolpath).read_text().splitlines()
            assert {"; filament_type = PLA;PETG", "; filament_map = 2,1",
                    "; filament_nozzle_map = 1,0", "T0", "T1"} <= set(lines), name
            prefix = "; flush_volumes_matrix = "
            used_purge = [float(value) for line in lines if line.startswith(prefix)
                          for value in line.removeprefix(prefix).split(",")]
            package_path = project / name / "model.gcode.3mf"
            verify_package(package_path)
            with zipfile.ZipFile(package_path) as package:
                layout = ET.fromstring(package.read("Metadata/model_settings.config"))
                nozzle_map = layout.find("./plate/metadata[@key='filament_maps']")
                assert nozzle_map is not None and nozzle_map.get("value") == "2 1", name
                settings = json.loads(package.read("Metadata/project_settings.config"))
                stored_purge = list(map(float, settings["flush_volumes_matrix"]))
            assert len(used_purge) == len(stored_purge) == 8
            assert any(value > 0 for value in used_purge) and any(value > 0 for value in stored_purge)
            handles.append(handle)
            return used_purge, stored_purge

        baseline_purge, _ = h2d_trial("h2d-two-filaments", material_source.name)
        authored_source = project / "h2d-authored-purge.3mf"
        authored_purge = [0, 501, 371, 0, 0, 549, 447, 0]
        config_path = "Metadata/project_settings.config"
        with zipfile.ZipFile(project / "h2d-two-filaments/model.gcode.3mf") as original, \
                zipfile.ZipFile(authored_source, "w", zipfile.ZIP_DEFLATED) as output:
            settings = json.loads(original.read(config_path))
            settings["flush_volumes_matrix"] = list(map(str, authored_purge))
            for name in original.namelist():
                output.writestr(name, json.dumps(settings) if name == config_path else original.read(name))
        used_purge, stored_purge = h2d_trial("h2d-reslice", authored_source.name)
        assert stored_purge == authored_purge and used_purge != baseline_purge
        print("Native H2D nozzle assignments, package thumbnails, and purge volumes survived reslicing")
        worker.terminate()
        worker.wait(timeout=10)
        worker = start()
        for handle in handles:
            assert request("status", handle)["status"] == terminal_states.get(handle["output_dir"], "completed")
        assert comparison_path.read_bytes() == comparison_bytes, "restart replaced retained comparison evidence"
        print("Native A1/X1C slices, overrides, slot colours, vendor profiles, package thumbnails, "
              "actual toolpath images, and restart recovery passed")
    finally:
        worker.terminate()
        worker.wait(timeout=10)
