"""Exercise editable project bundles and governed downloads through public MCP."""

import base64
import hashlib
import json
import math
import uuid
import zipfile


ORGANIC_SOURCE = '''
from pathlib import Path
project = Path(workspace_root) / "projects" / bpy.context.scene["printable_project_id"]
bpy.context.scene.unit_settings.system = "METRIC"
bpy.context.scene.unit_settings.length_unit = "MILLIMETERS"
bpy.context.scene.unit_settings.scale_length = 0.001
bpy.ops.object.metaball_add(type="BALL", location=(2, 3, 4))
organic = bpy.context.object
organic.name = "Organic export fixture"
organic.data.elements.new().co = (0.8, 0, 0)
image = bpy.data.images.new("Export texture", width=8, height=8)
image.generated_color = (0.1, 0.5, 0.2, 1.0)
image.filepath_raw = str(project / "texture.png")
image.file_format = "PNG"
image.save()
image.source = "FILE"
material = bpy.data.materials.new("Export surface")
material.use_nodes = True
texture = material.node_tree.nodes.new("ShaderNodeTexImage")
texture.image = image
material.node_tree.links.new(texture.outputs["Color"], material.node_tree.nodes.get("Principled BSDF").inputs["Base Color"])
organic.data.materials.append(material)
bpy.ops.wm.save_as_mainfile(filepath=str(project / "model.blend"), check_existing=False)
result = {"saved": True}
'''

ORGANIC_CHECK = '''
organic = bpy.data.objects.get("Organic export fixture")
image = organic.data.materials[0].node_tree.nodes.get("Image Texture").image
result = {"type": organic.type, "elements": len(organic.data.elements),
          "location": list(organic.location), "pixels": len(image.pixels),
          "color": list(image.pixels[:4]), "units": bpy.context.scene.unit_settings.scale_length}
'''


def run(client, output):
    output.mkdir(mode=0o700)

    def call(tool, action, params):
        return client.call(tool, {"action": action, "params": params})

    def scene_state():
        return call("inspect", "scene", {})["scene_state"]

    def upload(path, content):
        handle = call("artifact", "upload_begin", {"path": path})["upload_id"]
        for offset in range(0, len(content), 512 * 1024):
            call("artifact", "upload_chunk", {"upload_id": handle,
                "data_base64": base64.b64encode(content[offset:offset + 512 * 1024]).decode("ascii")})
        call("artifact", "upload_commit", {"upload_id": handle})

    def download_bundle(result, name):
        destination = output / name
        downloaded = client.download(result["artifact"]["path"], destination)
        if downloaded["sha256"] != result["sha256"]:
            raise ValueError("Project bundle download differs from its export digest")
        with zipfile.ZipFile(destination) as archive:
            manifest = json.loads(archive.read("manifest.json"))
            if manifest != result["manifest"]:
                raise ValueError("Project bundle manifest differs from the public result")
            for record in manifest["files"]:
                content = archive.read(record["path"])
                if len(content) != record["size_bytes"] or hashlib.sha256(content).hexdigest() != record["sha256"]:
                    raise ValueError("Project bundle retained file differs from its manifest")
        return destination

    project_id = "export-smoke-" + uuid.uuid4().hex
    project = call("project", "create", {"project_id": project_id, "name": "Native export integration"})
    before = scene_state()
    saved = project["root"] + "/before-export.blend"
    call("scene", "checkpoint", {"path": saved, "expected_scene": before})
    call("scene", "open_project", {"project_id": project_id, "mode": "empty",
                                    "discard_current": True, "expected_scene": scene_state()})
    client.call("blender_execute", {"expected_scene": scene_state(), "code": ORGANIC_SOURCE})
    original_scene = scene_state()
    result = call("project", "export_blender", {"project_id": project_id,
        "files": ["model.blend", "texture.png"], "entrypoint": "model.blend",
        "output_path": "exports/organic.zip", "timeout_seconds": 120})
    if scene_state() != original_scene:
        raise ValueError("Native project export changed the caller's live scene")
    package = download_bundle(result, "organic.zip")
    with zipfile.ZipFile(package) as archive:
        upload(project["root"] + "/restored.blend", archive.read("prepared/model.blend"))
    # Only fixture inputs are removed; reopening must rely on the downloaded bundle.
    client.call("blender_execute", {"expected_scene": scene_state(), "code": '''
from pathlib import Path
project = Path(workspace_root) / "projects" / bpy.context.scene["printable_project_id"]
(project / "model.blend").unlink()
(project / "texture.png").unlink()
'''})
    call("scene", "restore", {"path": project["root"] + "/restored.blend", "expected_scene": scene_state()})
    recovered = client.call("blender_execute", {"expected_scene": scene_state(), "code": ORGANIC_CHECK})["result"]
    if (recovered["type"] != "META" or recovered["elements"] != 2 or recovered["pixels"] != 256
            or recovered["location"] != [2, 3, 4] or not math.isclose(recovered["units"], 0.001, rel_tol=1e-6)
            or recovered["color"][1] <= 2 * recovered["color"][0]):
        raise ValueError("Downloaded native bundle lost editable geometry, units or texture content")
    call("scene", "restore", {"path": saved, "expected_scene": scene_state()})
    if scene_state().get("project_id") != before.get("project_id"):
        raise ValueError("Export fixture did not restore the caller's project identity")
    print("NATIVE_EXPORT_CLIENT_OK: packed editable project, texture, units, verified download", flush=True)

    source = b'result = cq.Workplane().box(parameters["width"], 20, 30)\n'
    settings = {"width": 42}
    upload(project["root"] + "/part.py", source)
    upload(project["root"] + "/settings.json", json.dumps(settings).encode())
    first = call("cad_build", "model", {"project_id": project_id, "source": "part.py",
                 "parameters": settings, "output_dir": "cad"})
    result = call("project", "export_files", {"project_id": project_id,
        "files": ["part.py", "settings.json", "cad/model.step", "cad/report.json"],
        "output_path": "exports/parametric.zip"})
    package = download_bundle(result, "parametric.zip")
    restored_id = "export-restored-" + uuid.uuid4().hex
    restored = call("project", "create", {"project_id": restored_id, "name": "Restored parametric integration"})
    with zipfile.ZipFile(package) as archive:
        upload(restored["root"] + "/part.py", archive.read("files/part.py"))
        parameters = json.loads(archive.read("files/settings.json"))
    rebuilt = call("cad_build", "model", {"project_id": restored_id, "source": "part.py",
                   "parameters": parameters, "output_dir": "rebuilt"})
    if (not rebuilt["report"]["valid"] or rebuilt["report"]["units"] != "mm"
            or not all(math.isclose(a, b, abs_tol=1e-5) for a, b in zip(
                first["report"]["bounds_mm"]["size"], rebuilt["report"]["bounds_mm"]["size"]))):
        raise ValueError("Downloaded parametric sources did not reproduce native dimensions")
    print("PARAMETRIC_EXPORT_CLIENT_OK: retained sources/settings, verified download, independent rebuild", flush=True)
