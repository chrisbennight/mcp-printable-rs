"""Native proof of project recovery and placed CAD assembly attachment."""

import json
from pathlib import Path
import struct


def write_fixture(workspace_root):
    broken = Path(workspace_root, "projects/smoke-beta/broken.blend")
    broken.parent.mkdir(parents=True, exist_ok=True)
    broken.write_bytes(b"invalid checkpoint")
    output = Path(workspace_root, "projects/smoke-alpha/cad/output")
    output.mkdir(parents=True, exist_ok=True)
    binary = struct.pack("<9f", 0, 0, 0, 0.01, 0, 0, 0, 0.02, 0)
    document = {
        "asset": {"version": "2.0"}, "scene": 0, "scenes": [{"nodes": [2]}],
        "nodes": [{"name": "assembly", "children": [1], "translation": [0, 0.003, 0]},
                  {"name": "part", "mesh": 0, "translation": [0.004, 0, 0]},
                  {"children": [0]}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
        "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3,
                       "type": "VEC3", "min": [0, 0, 0], "max": [0.01, 0.02, 0]}],
        "bufferViews": [{"buffer": 0, "byteLength": len(binary)}],
        "buffers": [{"byteLength": len(binary)}],
    }
    encoded = json.dumps(document).encode()
    encoded += b" " * (-len(encoded) % 4)
    data = struct.pack("<5I", 0x46546C67, 2, 28 + len(encoded) + len(binary), len(encoded), 0x4E4F534A)
    data += encoded + struct.pack("<2I", len(binary), 0x004E4942) + binary
    (output / "model.glb").write_bytes(data)
    (output / "components.json").write_text(json.dumps({"nodes": {
        "assembly": {"source_name": "Vendor assembly", "product_name": "Assembly product"},
        "assembly/part": {"source_name": "Occurrence", "occurrence_name": "Occurrence",
                          "product_name": "Part product"},
    }}))


def run(host, port):
    from integration_smoke import command

    def observe():
        return command(host, port, "get_scene_info", {})["scene_state"]

    original = "smoke/before-project-scenes.blend"
    command(host, port, "save_blend", {"path": original})
    command(host, port, "open_project", {"project_id": "smoke-alpha", "mode": "empty",
                                         "discard_current": True, "expected_scene": observe()})
    command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "from project_scene_smoke import write_fixture\n"
        "write_fixture(workspace_root)\n"
        "bpy.context.scene.world=bpy.data.worlds.new('Alpha world')\n"
        "bpy.data.scenes.new('Alpha alternate')\n"
        "bpy.context.scene.frame_end=381\n"
        "bpy.context.scene.unit_settings.scale_length = 0.01\n"})
    attached = command(host, port, "attach_cad", {"project_id": "smoke-alpha",
        "path": "projects/smoke-alpha/cad/output/model.glb", "expected_scene": observe()})
    if attached["cad"]["named_nodes"] != 2 or attached["cad"]["unmatched_names"] != 0:
        raise RuntimeError("CAD attachment lost original assembly names")
    stale_alpha = observe()
    measured = command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "obj = bpy.data.objects['part']\n"
        "points = [obj.matrix_world @ vertex.co for vertex in obj.data.vertices]\n"
        "result = {'min':[min(p[i] for p in points) for i in range(3)],\n"
        "'max':[max(p[i] for p in points) for i in range(3)],\n"
        "'parent':obj.parent.name, 'product':obj.get('printable_cad_product_name')}\n"})["result"]
    for key, expected in (("min", [4, 0, 3]), ("max", [14, 0, 23])):
        if any(abs(left - right) > 0.0001 for left, right in zip(measured[key], expected, strict=True)):
            raise RuntimeError(f"CAD {key} does not preserve millimetre scale, axes, and placements")
    if measured["parent"] != "assembly" or measured["product"] != "Part product":
        raise RuntimeError("CAD attachment lost hierarchy or source identity")
    try:
        command(host, port, "open_project", {"project_id": "smoke-beta", "mode": "checkpoint",
            "checkpoint": "projects/smoke-beta/broken.blend", "discard_current": True,
            "expected_scene": observe()})
    except RuntimeError:
        if observe().get("project_id") != "smoke-alpha":
            raise RuntimeError("rejected checkpoint erased the retained project's identity")
    else:
        raise RuntimeError("invalid checkpoint unexpectedly opened")
    command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "bpy.context.window.scene=bpy.data.scenes['Alpha alternate']\n"})
    command(host, port, "open_project", {"project_id": "smoke-beta", "mode": "empty",
        "save_current_to": "projects/smoke-alpha/scene.blend", "expected_scene": observe()})
    command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "assert 'Alpha world' not in bpy.data.worlds\n"
        "assert 'Alpha alternate' not in bpy.data.scenes\n"
        "assert bpy.context.scene.frame_end != 381\n"
        "bpy.ops.mesh.primitive_cube_add(size=7)\nbpy.context.object.name='part'\n"})
    beta = observe()
    try:
        command(host, port, "attach_cad", {"project_id": "smoke-alpha",
            "path": "projects/smoke-alpha/cad/output/model.glb", "expected_scene": stale_alpha})
    except RuntimeError as error:
        if "stale" not in str(error):
            raise
    else:
        raise RuntimeError("stale CAD work reached a different project")
    if observe() != beta:
        raise RuntimeError("stale attachment changed the current scene")
    command(host, port, "open_project", {"project_id": "smoke-alpha", "mode": "checkpoint",
        "checkpoint": "projects/smoke-alpha/scene.blend",
        "save_current_to": "projects/smoke-beta/scene.blend", "expected_scene": observe()})
    recovered = command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "obj=bpy.data.objects['part']\nresult={'product':obj.get('printable_cad_product_name'),\n"
        "'parent':obj.parent.name,'source_exists':__import__('pathlib').Path(workspace_root,\n"
        "'projects/smoke-alpha/cad/output/model.glb').is_file()}\n"})["result"]
    if recovered != {"product": "Part product", "parent": "assembly", "source_exists": True}:
        raise RuntimeError("project checkpoint did not recover the CAD scene and retained artifact")
    command(host, port, "open_project", {"project_id": "smoke-beta", "mode": "checkpoint",
        "checkpoint": "projects/smoke-beta/scene.blend", "discard_current": True,
        "expected_scene": observe()})
    recovered_beta = command(host, port, "execute_code", {"expected_scene": observe(), "code":
        "result=list(bpy.data.objects['part'].dimensions)\n"})["result"]
    if any(abs(value - 7) > 0.0001 for value in recovered_beta):
        raise RuntimeError("same-named objects were mixed between projects")
    command(host, port, "restore_checkpoint", {"path": original, "expected_scene": observe()})
