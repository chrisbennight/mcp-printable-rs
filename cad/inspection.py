"""Selected native CAD bounds and component metadata without mesh exports."""

import json
import math

import cadquery as cq
from OCP.Bnd import Bnd_Box
from OCP.BRepBndLib import BRepBndLib


def native_bounds(shape):
    box = Bnd_Box()
    BRepBndLib.AddOptimal_s(shape.wrapped, box, False, False)
    if box.IsVoid() or box.IsOpen():
        raise ValueError("CAD component has no finite bounds")
    values = box.Get()
    if not all(math.isfinite(value) for value in values):
        raise ValueError("CAD component has nonfinite bounds")
    return {"min": list(values[:3]), "max": list(values[3:]),
            "size": [values[axis + 3] - values[axis] for axis in range(3)]}


def inspect(assembly, names, request, output):
    selection = request.get("selection", {})
    ids = set(selection.get("component_ids", []))
    contains = (selection.get("name_contains") or "").casefold()
    region = selection.get("region_mm")
    frame = request.get("frame", "assembly")
    offset, limit = request.get("offset", 0), request.get("limit", 20)
    inventory, selected = [], []
    found = set()
    for component, name, location, _ in assembly:
        metadata = names[name]
        transform = location.wrapped.Transformation()
        row = {"id": name, "parent_id": name.rsplit("/", 1)[0] if "/" in name else None,
               **metadata,
               "transform": [[transform.Value(i, j) for j in range(1, 5)] for i in range(1, 4)]}
        inventory.append(row)
        found.add(name)
        if ids and name not in ids:
            continue
        if contains and not any(contains in str(value or "").casefold()
                                for value in (name, *metadata.values())):
            continue
        world_bounds = None
        if region:
            world_bounds = native_bounds(component.moved(location))
            if any(world_bounds["max"][axis] < region["min"][axis]
                   or world_bounds["min"][axis] > region["max"][axis] for axis in range(3)):
                continue
        selected.append((row, component, location, world_bounds))

    rows = []
    for row, component, location, world_bounds in selected[offset:offset + limit]:
        measured = (native_bounds(component) if frame == "component"
                    else world_bounds or native_bounds(component.moved(location)))
        rows.append({**row, "bounds_mm": measured})
    report = {"cadquery_version": cq.__version__, "units": "mm", "frame": frame,
              "bounds_method": "native_geometry_without_triangulation",
              "component_count": len(inventory), "matched_count": len(selected),
              "offset": offset, "next_offset": offset + len(rows) if offset + len(rows) < len(selected) else None,
              "components": rows, "missing_component_ids": sorted(ids - found)}
    (output / "components.json").write_text(json.dumps({"nodes": names, "components": inventory}, allow_nan=False))
    (output / "report.json").write_text(json.dumps(report, allow_nan=False))
