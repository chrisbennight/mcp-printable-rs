"""Bounded metadata for Blender's registered external file references."""

import math
import os
from pathlib import Path

from .inspection import InspectionError, page_arguments


def inspect_dependencies(bpy, workspace_root, project_id, params):
    offset, limit = page_arguments(params)
    # Blender resolves references relative to their owning library, not merely
    # the current main file. Do not reproduce that resolution in Python.
    paths = bpy.utils.blend_paths(absolute=True, packed=False, local=False)
    if len(paths) > 10_000:
        raise InspectionError("scene dependency inventory exceeds 10000 references")
    project_root = Path(os.path.abspath(workspace_root)) / "projects" / project_id
    items = []
    for index, raw in enumerate(paths[offset:offset + limit], start=offset):
        entry = {"index": index, "project_path": None, "state": "external"}
        if isinstance(raw, str) and raw and Path(raw).is_absolute():
            normalized = Path(os.path.normpath(raw))
            try:
                relative = normalized.relative_to(project_root)
            except ValueError:
                pass
            else:
                if relative.parts and not any(part.startswith(".") for part in relative.parts):
                    entry.update(project_path=relative.as_posix(), state="requires_snapshot")
        items.append(entry)
    units = bpy.context.scene.unit_settings
    scale = float(units.scale_length)
    if not math.isfinite(scale) or scale <= 0:
        raise InspectionError("scene has invalid length units")
    end = offset + len(items)
    return {
        "project_id": project_id,
        "engine": {"name": "blender", "version": bpy.app.version_string},
        "units": {"system": units.system, "length_unit": units.length_unit,
                  "scale_length": scale},
        "scope": "blender_registered_external_files",
        "items": items,
        "total": len(paths),
        "next_offset": end if end < len(paths) else None,
        "limitations": [
            "Packed data is omitted; this does not verify packed contents.",
            "Project-relative references still require confined snapshots; existence and safety are not inferred.",
            "Arbitrary script, driver, add-on and network dependencies are not inspected.",
            "File sequences, caches and other native formats may require additional preparation.",
        ],
    }
