"""Native packing primitives for an isolated Blender export process.

The caller supplies a private, snapshotted project tree and prepares linked
libraries in dependency order. This module must not run in the live bridge.
"""

from pathlib import Path


class ProjectPackingError(ValueError):
    pass


def pack_loaded_file(bpy, project_root: Path, output: Path, *, library: bool):
    if not bpy.app.background:
        raise ProjectPackingError("project packing requires isolated background Blender")
    root = project_root.resolve(strict=True)
    target = output.resolve(strict=False)
    if not target.is_relative_to(root) or target.suffix != ".blend":
        raise ProjectPackingError("packed output must be a blend file inside the staged project")

    references = bpy.utils.blend_paths(absolute=True, packed=False, local=False)
    if len(references) > 10_000:
        raise ProjectPackingError("project packing exceeds 10000 file references")
    for raw in references:
        path = Path(raw)
        if not path.is_absolute() or not path.resolve(strict=False).is_relative_to(root):
            raise ProjectPackingError("project contains an external dependency outside its staged files")
        if path.is_symlink() or not path.is_file():
            raise ProjectPackingError("project dependency is missing or requires unsupported packing")
    if any(block.is_missing for block in bpy.data.user_map()):
        raise ProjectPackingError("project contains missing linked data")

    if "FINISHED" not in bpy.ops.file.pack_all():
        raise ProjectPackingError("native asset packing did not finish")
    if "FINISHED" not in bpy.ops.file.pack_libraries():
        raise ProjectPackingError("native library packing did not finish")
    if bpy.utils.blend_paths(absolute=True, packed=False, local=False):
        raise ProjectPackingError("project still has external dependencies; prepare libraries and unsupported assets before exporting")

    if library:
        # Library-only objects may have no scene users. A normal main-file save
        # can omit them; native partial writing retains the selected ID graph.
        # WindowManager is runtime window state, not a serializable library ID.
        identifiers = {block for block in bpy.data.user_map()
                       if not isinstance(block, bpy.types.WindowManager)}
        bpy.data.libraries.write(str(target), identifiers, path_remap="RELATIVE_ALL")
    else:
        result = bpy.ops.wm.save_as_mainfile(filepath=str(target), check_existing=False)
        if "FINISHED" not in result:
            raise ProjectPackingError("native project save did not finish")
    if not target.is_file() or target.stat().st_size == 0:
        raise ProjectPackingError("native packing produced no project file")
    return {"engine": "blender", "version": bpy.app.version_string,
            "registered_external_files": 0, "library": library,
            "limitations": ["Arbitrary script, driver, add-on and network dependencies are not inspected."]}
