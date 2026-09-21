"""Native Blender evidence for dependency inventory and editable packed files.

Run with factory-startup Blender in an isolated container, never a live scene.
"""

import json
import hashlib
from pathlib import Path
import sys
import tempfile
import zipfile

import bpy

sys.path.insert(0, str(Path(__file__).resolve().parent))
from printable_bridge.project_dependencies import inspect_dependencies
from printable_bridge.project_bundle import export_blender_bundle
from printable_bridge.workspace import SecureWorkspace


def run(path_remap):
    with tempfile.TemporaryDirectory(prefix="printable-export-") as temporary:
        root = Path(temporary)
        project = root / "projects" / "organic"
        textures = project / "textures"
        textures.mkdir(parents=True)
        library_path = project / "organic-library.blend"
        main_path = project / "organic.blend"

        bpy.ops.wm.read_factory_settings(use_empty=True)
        bpy.context.scene.unit_settings.system = "METRIC"
        bpy.context.scene.unit_settings.length_unit = "MILLIMETERS"
        bpy.context.scene.unit_settings.scale_length = 0.001

        image = bpy.data.images.new("Leaf color", width=8, height=8)
        image.generated_color = (0.1, 0.5, 0.2, 1.0)
        image.filepath_raw = str(textures / "leaf.png")
        image.file_format = "PNG"
        image.save()
        image.source = "FILE"
        material = bpy.data.materials.new("Leaf surface")
        material.use_nodes = True
        texture = material.node_tree.nodes.new("ShaderNodeTexImage")
        texture.image = image
        material.node_tree.links.new(texture.outputs["Color"],
                                     material.node_tree.nodes.get("Principled BSDF").inputs["Base Color"])

        bpy.ops.object.metaball_add(type="BALL", location=(2, 3, 4))
        organic = bpy.context.object
        organic.name = "Organic study"
        organic.data.elements.new().co = (0.8, 0, 0)
        organic.data.materials.append(material)
        bpy.data.libraries.write(str(library_path), {organic}, path_remap=path_remap)
        bpy.data.objects.remove(organic, do_unlink=True)
        with bpy.data.libraries.load(str(library_path), link=True) as (source, target):
            target.objects = ["Organic study"]
        linked = target.objects[0]
        bpy.context.scene.collection.objects.link(linked)
        bpy.ops.wm.save_as_mainfile(filepath=str(main_path), check_existing=False)
        if path_remap == "RELATIVE_ALL":
            bpy.ops.file.make_paths_relative()
            bpy.ops.wm.save_as_mainfile(filepath=str(main_path), check_existing=False)

        before = inspect_dependencies(bpy, root, "organic", {"limit": 100})
        paths = {entry["project_path"] for entry in before["items"]}
        if "organic-library.blend" not in paths or "textures/leaf.png" not in paths:
            raise RuntimeError("native inventory did not retain linked library and texture paths")
        if any(entry["state"] != "requires_snapshot" for entry in before["items"]):
            raise RuntimeError("contained native fixture was reported outside its project")
        if before["units"]["scale_length"] != bpy.context.scene.unit_settings.scale_length:
            raise RuntimeError("dependency inventory lost native units")

        # Run against private copies; original files remain byte-identical.
        exported = root / "exported.blend"
        workspace = SecureWorkspace(root, root / "runtime")
        files = ["organic.blend", "organic-library.blend", "textures/leaf.png"]
        live_filepath = bpy.data.filepath
        try:
            result = export_blender_bundle(
                workspace, root, bpy.app.binary_path, project_id="organic", files=files,
                entrypoint="organic.blend", output_path="exports/organic.zip", timeout_seconds=60,
            )
            if bpy.data.filepath != live_filepath:
                raise RuntimeError("isolated preparation changed the caller's live file")
            with zipfile.ZipFile(root / result["path"]) as archive:
                manifest = json.loads(archive.read("manifest.json"))
                for record in manifest["files"]:
                    content = archive.read(record["path"])
                    if len(content) != record["size_bytes"] or hashlib.sha256(content).hexdigest() != record["sha256"]:
                        raise RuntimeError("native bundle manifest does not match its retained files")
                exported.write_bytes(archive.read("prepared/organic.blend"))
            packed = manifest["preparation"]
        finally:
            workspace.close()
        # Remove only this fixture's external sources, then reopen the packed file.
        library_path.unlink()
        (textures / "leaf.png").unlink()
        bpy.ops.wm.open_mainfile(filepath=str(exported), load_ui=False, use_scripts=False)
        after = inspect_dependencies(bpy, root, "organic", {"limit": 100})
        if after["total"] != 0:
            raise RuntimeError("packed fixture still has registered external dependencies")
        recovered = bpy.data.objects.get("Organic study")
        if recovered is None or recovered.type != "META" or len(recovered.data.elements) != 2:
            raise RuntimeError("packed project did not preserve editable organic geometry")
        if tuple(round(value, 5) for value in recovered.location) != (2, 3, 4):
            raise RuntimeError("packed project changed object placement")
        recovered_image = recovered.data.materials[0].node_tree.nodes.get("Image Texture").image
        if len(recovered_image.pixels) != 8 * 8 * 4 or not recovered_image.has_data:
            raise RuntimeError("packed project did not recover linked texture pixels")
        red, green, _blue, alpha = recovered_image.pixels[:4]
        if green <= 2 * red or alpha < 0.99:
            raise RuntimeError("recovered image pixels do not contain the original texture")
        print(json.dumps({"result": "ORGANIC_PACKED_REOPEN_OK",
                          "source_path_mode": path_remap,
                          "blender_version": bpy.app.version_string,
                          "before": before, "after": after, "packing": packed}, allow_nan=False))


if __name__ == "__main__":
    run("ABSOLUTE")
    run("RELATIVE_ALL")
