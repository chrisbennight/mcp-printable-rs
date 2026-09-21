"""Exercise packaged Orca slicing, toolpath previews, and restart recovery."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.request


def request(action, params):
    body = json.dumps({"action": action, "params": params}).encode()
    req = urllib.request.Request("http://127.0.0.1:8003/slice", body,
                                 {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as response:
        return json.load(response)


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
        for _ in range(100):
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
        handles = []
        for model, suffix, printer in [("a1", "A1", "Bambu Lab A1 0.4 nozzle"),
                                        ("x1c", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle"),
                                        ("x1c-reslice", "X1C", "Bambu Lab X1 Carbon 0.4 nozzle")]:
            profiles = request("profiles", {"category": "process", "printer": printer, "query": "0.20mm Standard"})
            assert profiles["total"] > 0
            inspected = request("settings", {"category": "filament", "profile": {"name": f"Bambu PLA Basic @BBL {suffix}"}, "query": "plate_temp"})
            assert any(row["key"] == "hot_plate_temp" for row in inspected["settings"])
            handle = {"project_id": "native-slice", "output_dir": model}
            state = request("prepare", {**handle, "source": "x1c/model.gcode.3mf" if model == "x1c-reslice" else "source.stl",
                "build_plate": "High Temp Plate" if model == "x1c" else "Textured PEI Plate",
                "printer": {"name": printer},
                "process": {"name": f"0.20mm Standard @BBL {suffix}"},
                "filaments": [{"name": f"Bambu PLA Basic @BBL {suffix}"}],
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
            review = request("review", {"slice": handle, "toolpath": toolpath, "first_layer": 1, "last_layer": 1})
            assert review["kind"] == "actual_toolpath" and review["segments"] > 0
            assert (root / review["image"]["path"]).read_bytes().startswith(b"\x89PNG")
            settings = json.loads((project / model / "settings.json").read_text())
            assert settings["printer"]["name"] == printer
            assert settings["printer"]["machine_start_gcode"]
            assert all("inherits" not in profile for profile in [settings["printer"], settings["process"], *settings["filaments"]])
            handles.append(handle)
        worker.terminate()
        worker.wait(timeout=10)
        worker = start()
        for handle in handles:
            assert request("status", handle)["status"] == "completed"
        print("Native A1/X1C slices, actual toolpath images, and restart recovery passed")
    finally:
        worker.terminate()
        worker.wait(timeout=10)
