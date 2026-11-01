"""Selected native CAD bounds and component metadata without mesh exports."""

import json
import math

import cadquery as cq
from OCP.Bnd import Bnd_Box
from OCP.BRepBndLib import BRepBndLib
from questions import answer


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


def matches(name, metadata, component, location, selection):
    ids = selection.get("component_ids", [])
    if ids and name not in ids:
        return False, None
    contains = (selection.get("name_contains") or "").casefold()
    if contains and not any(contains in str(value or "").casefold() for value in (name, *metadata.values())):
        return False, None
    region = selection.get("region_mm")
    bounds = native_bounds(component.moved(location)) if region else None
    if region and any(bounds["max"][axis] < region["min"][axis]
                      or bounds["min"][axis] > region["max"][axis] for axis in range(3)):
        return False, bounds
    return True, bounds


def inspect(assembly, names, request, output, progress=None, export_glb=None):
    selection = request.get("selection", {})
    ids = set(selection.get("component_ids", []))
    frame = request.get("frame", "assembly")
    offset, limit = request.get("offset", 0), request.get("limit", 20)
    inventory, selected, components = [], [], {}
    found = set()
    for component, name, location, _ in assembly:
        metadata = names[name]
        transform = location.wrapped.Transformation()
        row = {"id": name, "parent_id": name.rsplit("/", 1)[0] if "/" in name else None,
               **metadata,
               "transform": [[transform.Value(i, j) for j in range(1, 5)] for i in range(1, 4)]}
        inventory.append(row)
        components[name] = (component, location)
        found.add(name)
        matched, world_bounds = matches(name, metadata, component, location, selection)
        if matched:
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
              "components": rows, "missing_component_ids": sorted(ids - found), "answers": [], "exports": []}
    (output / "components.json").write_text(json.dumps({"nodes": names, "components": inventory,
        "selected_component_ids": [row["id"] for row, *_ in selected]}, allow_nan=False))
    if progress:
        progress.advance("measurement", "components.json")

    def resolve_target(target):
        identity, selector = target.get("component_id"), target.get("selection")
        if identity is not None and selector is not None:
            raise ValueError("choose a component identity or selection, not both")
        if identity is not None:
            candidates = [identity] if identity in components else []
        elif selector is not None:
            candidates = [name for name, (shape, location) in components.items()
                          if matches(name, names[name], shape, location, selector)[0]]
        else:
            candidates = [row["id"] for row, *_ in selected]
        if len(candidates) != 1:
            raise ValueError(f"measurement target matched {len(candidates)} occurrences; select exactly one")
        return candidates[0]

    def save_measurements():
        temporary = output / "measurements.json.tmp"
        temporary.write_text(json.dumps({"completion": "partial", "scope": "cad_measurements", **report}, allow_nan=False))
        temporary.replace(output / "measurements.json")
        if progress:
            progress.advance("measurement", "measurements.json")

    save_measurements()
    for question in request.get("questions", []):
        report["answers"].append(answer(question, components, frame, native_bounds, resolve_target))
        save_measurements()
    formats = request.get("exports", [])
    if formats:
        from delivery import deliver
        report["exports"] = deliver(assembly, selected, request, output, progress, export_glb,
                                    missing_ids=report["missing_component_ids"])
    (output / "report.json").write_text(json.dumps(report, allow_nan=False))
