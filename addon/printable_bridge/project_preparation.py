"""Prepare an explicitly staged native project without modifying its sources.

This module runs only in a disposable, script-disabled Blender process. Native
library loading is not a filesystem sandbox; the process must have no secrets
or unrelated mounts, just like the existing Blender execution boundary.
"""

import math
import os
from pathlib import Path

from .project_packing import ProjectPackingError, pack_loaded_file


class ProjectInputs:
    def __init__(self, original_root: Path, staged_root: Path, files: list[str]):
        self.original = Path(os.path.abspath(original_root))
        self.staged = staged_root.resolve(strict=True)
        self.files = frozenset(files)
        if not 1 <= len(files) <= 256 or len(self.files) != len(files):
            raise ProjectPackingError("native preparation requires 1–256 unique staged files")
        for name in files:
            candidate = self.staged / name
            if (not name or Path(name).is_absolute()
                    or any(part in {"", ".", ".."} or part.startswith(".")
                           for part in name.split("/"))
                    or "\\" in name or candidate.is_symlink()
                    or candidate.resolve(strict=True) != candidate or not candidate.is_file()):
                raise ProjectPackingError("native preparation inputs must be regular staged files")

    def relative(self, raw: str) -> str:
        if not raw or not Path(raw).is_absolute():
            raise ProjectPackingError("native dependency did not resolve to an absolute file")
        normalized = Path(os.path.normpath(raw))
        for root in (self.staged, self.original):
            if normalized.is_relative_to(root):
                name = normalized.relative_to(root).as_posix()
                if name in self.files:
                    return name
        raise ProjectPackingError("native dependency is outside the explicitly selected project inputs")

    def path(self, raw: str) -> Path:
        return self.staged / self.relative(raw)


def _open(bpy, path):
    result = bpy.ops.wm.open_mainfile(filepath=str(path), load_ui=False, use_scripts=False)
    if "FINISHED" not in result:
        raise ProjectPackingError("native project load did not finish")


def _library_paths(bpy, inputs):
    references = bpy.utils.blend_paths(absolute=True, packed=False, local=False)
    if len(references) > 10_000:
        raise ProjectPackingError("project preparation exceeds 10000 file references")
    for raw in references:
        inputs.relative(raw)
    return sorted({inputs.relative(bpy.path.abspath(library.filepath))
                   for library in bpy.data.libraries
                   if library.parent is None and library.packed_file is None
                   and not library.is_archive})


def _rebase_loaded_file(bpy, inputs):
    # Reload one library at a time: reload can invalidate every linked ID and
    # library reference obtained before that call. Packed dependencies are
    # already prepared and must not be replaced with original library bytes.
    for _ in range(256):
        pending = next((library for library in bpy.data.libraries
                        if library.parent is None and library.packed_file is None
                        and not library.is_archive
                        and bpy.path.abspath(library.filepath) != str(
                            inputs.path(bpy.path.abspath(library.filepath)))), None)
        if pending is None:
            break
        pending.filepath = str(inputs.path(bpy.path.abspath(pending.filepath)))
        pending.reload()
    else:
        raise ProjectPackingError("native library relocation exceeds the input limit")

    for collection in (bpy.data.images, bpy.data.fonts, bpy.data.sounds,
                       bpy.data.movieclips, bpy.data.cache_files, bpy.data.volumes):
        for block in collection:
            if block.library is not None or not block.filepath or block.filepath == "<builtin>":
                continue
            if getattr(block, "packed_file", None) is not None:
                continue
            if isinstance(block, bpy.types.Image):
                if block.source in {"GENERATED", "VIEWER"}:
                    continue
                if block.source != "FILE":
                    raise ProjectPackingError("image sequences, movies and tiled images need explicit preparation")
            block.filepath = str(inputs.path(bpy.path.abspath(block.filepath)))
            if isinstance(block, bpy.types.Image):
                block.reload()


def prepare_staged_project(bpy, original_root: Path, staged_root: Path,
                           files: list[str], entrypoint: str):
    if not bpy.app.background:
        raise ProjectPackingError("project preparation requires isolated background Blender")
    inputs = ProjectInputs(original_root, staged_root, files)
    if entrypoint not in inputs.files or not entrypoint.endswith(".blend"):
        raise ProjectPackingError("native entrypoint must be a selected blend file")

    visiting = set()
    visited = set()
    order = []

    def discover(name):
        if name in visiting:
            raise ProjectPackingError("cyclic linked libraries require explicit preparation")
        if name in visited:
            return
        if not name.endswith(".blend"):
            raise ProjectPackingError("native library must be a selected blend file")
        visiting.add(name)
        _open(bpy, inputs.staged / name)
        for dependency in _library_paths(bpy, inputs):
            discover(dependency)
        visiting.remove(name)
        visited.add(name)
        order.append(name)

    discover(entrypoint)
    for name in order:
        _open(bpy, inputs.staged / name)
        _rebase_loaded_file(bpy, inputs)
        pack_loaded_file(bpy, inputs.staged, inputs.staged / name,
                         library=name != entrypoint)
    units = bpy.context.scene.unit_settings
    scale = float(units.scale_length)
    if not math.isfinite(scale) or scale <= 0:
        raise ProjectPackingError("native project has invalid length units")
    return {"engine": {"name": "blender", "version": bpy.app.version_string},
            "entrypoint": entrypoint, "prepared_libraries": len(order) - 1,
            "units": {"system": units.system, "length_unit": units.length_unit,
                      "scale_length": scale},
            "registered_external_files": 0,
            "limitations": ["Arbitrary script, driver, add-on and network dependencies are not inspected."]}
