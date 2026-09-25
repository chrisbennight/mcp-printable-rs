"""Exercise native CAD through the public MCP and artifact transfer contracts."""

import base64
import hashlib
import json
import math
import uuid


def run(client, output):
    output.mkdir(mode=0o700)
    project_id = "cad-smoke-" + uuid.uuid4().hex

    def call(tool, action, params):
        return client.call(tool, {"action": action, "params": params})

    before = call("inspect", "scene", {})["scene_state"]
    project = call("project", "create", {"project_id": project_id, "name": "CAD integration"})
    source = b'result = cq.Workplane().box(parameters["width"], 20, 30)\n'
    call("artifact", "write", {"path": project["root"] + "/part.py",
                               "data_base64": base64.b64encode(source).decode("ascii")})
    requests = [
        ("model", {"source": "part.py", "parameters": {"width": 42}, "output_dir": "builds/model"}),
        ("import_step", {"source": "builds/model/model.step", "output_dir": "builds/import"}),
    ]
    for action, parameters in requests:
        result = call("cad_build", action, {"project_id": project_id, **parameters,
            "qualification": {"policy": "printable_part", "dimensions_mm": [42, 20, 30], "solid_count": 1}})
        if result["completion"] != "completed" or result["qualification"]["status"] != "passed":
            raise ValueError("CAD export completed without meeting its declared delivery requirements")
        report = result["report"]
        if (report["units"] != "mm" or not report["valid"] or report["solid_count"] != 1
                or len(report["bounds_mm"]["size"]) != 3
                or not all(math.isclose(actual, expected, abs_tol=1e-5)
                           for actual, expected in zip(report["bounds_mm"]["size"], [42, 20, 30]))):
            raise ValueError("CAD public result has incorrect geometry or units")
        expected_paths = {result["build_directory"] + "/" + name
                          for name in ("model.step", "model.stl", "model.glb", "components.json")}
        if {item["artifact"]["path"] for item in result["artifacts"]} != expected_paths:
            raise ValueError("CAD public result has an incomplete artifact set")
        for index, item in enumerate(result["artifacts"]):
            destination = output / f"{action}-{index}"
            client.download(item["artifact"]["path"], destination)
            with destination.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if digest != item["sha256"]:
                raise ValueError("CAD downloaded artifact differs from the build report")
        report_path = output / f"{action}-report.json"
        client.download(result["build_directory"] + "/report.json", report_path)
        if json.loads(report_path.read_text()) != result:
            raise ValueError("CAD retained report differs from the public result")
    if call("inspect", "scene", {})["scene_state"] != before:
        raise ValueError("Independent CAD build changed the live Blender scene")
    print("CAD_CLIENT_OK: project build, STEP round trip, verified downloads, unchanged live scene")

    saved = project["root"] + "/before-attachment.blend"
    call("scene", "checkpoint", {"path": saved, "expected_scene": before})
    state = call("inspect", "scene", {})["scene_state"]
    call("scene", "open_project", {"project_id": project_id, "mode": "empty",
        "discard_current": True, "expected_scene": state})
    state = call("inspect", "scene", {})["scene_state"]
    if state.get("project_id") != project_id:
        raise ValueError("Public project scene did not bind the requested identity")
    attached = call("scene", "attach_cad", {"project_id": project_id,
        "path": "builds/import/model.glb", "expected_scene": state})
    if attached["object_count"] < 1 or attached["units"] != "mm":
        raise ValueError("Public CAD attachment returned no millimetre scene geometry")
    state = call("inspect", "scene", {})["scene_state"]
    measured = client.call("blender_execute", {"expected_scene": state, "code":
        "points = [obj.matrix_world @ vertex.co for obj in bpy.context.scene.objects "
        "if obj.type == 'MESH' for vertex in obj.data.vertices]\n"
        "result = [max(point[i] for point in points) - min(point[i] for point in points) "
        "for i in range(3)]\n"})["result"]
    if len(measured) != 3 or not all(math.isclose(actual, expected, abs_tol=1e-4)
                                   for actual, expected in zip(sorted(measured), [20, 30, 42])):
        raise ValueError("Public CAD attachment changed the native model dimensions")
    state = call("inspect", "scene", {})["scene_state"]
    call("scene", "restore", {"path": saved, "expected_scene": state})
    if call("inspect", "scene", {})["scene_state"].get("project_id") != before.get("project_id"):
        raise ValueError("Public checkpoint restore lost the original project identity")
    print("PROJECT_SCENE_CLIENT_OK: guarded CAD attachment, native dimensions, restored scene")


def verify_restored(client, output):
    for name in ("model", "import_step"):
        original = json.loads((output / f"{name}-report.json").read_text())
        restored_report = output / f"{name}-restored-report.json"
        client.download(original["build_directory"] + "/report.json", restored_report)
        if json.loads(restored_report.read_text()) != original:
            raise ValueError("Restored CAD build report differs from the original")
        for index, item in enumerate(original["artifacts"]):
            destination = output / f"{name}-restored-{index}"
            client.download(item["artifact"]["path"], destination)
            with destination.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if digest != item["sha256"]:
                raise ValueError("Restored CAD artifact differs from its recorded digest")
    print("CAD_RESTORE_OK: retained reports and artifact digests", flush=True)
