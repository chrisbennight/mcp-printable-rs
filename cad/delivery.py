"""Requested selected outputs share one loaded native model."""

import cadquery as cq


def selected_assembly(assembly, selected, frame):
    ids = {row["id"] for row, *_ in selected}
    if not ids:
        raise ValueError("no components matched the requested export scope")
    if frame == "component":
        if len(selected) != 1:
            raise ValueError("component-coordinate export requires exactly one occurrence")

    def prune(node, parent=""):
        path = f"{parent}/{node.name}" if parent else node.name
        result = cq.Assembly(node.obj, loc=node.loc, name=node.name, color=node.color, material=node.material, metadata=node.metadata)
        result._subshape_names.update(node._subshape_names)
        result._subshape_colors.update(node._subshape_colors)
        result._subshape_layers.update(node._subshape_layers)
        if path not in ids:
            result.obj = None
            result._subshape_names.clear()
            result._subshape_colors.clear()
            result._subshape_layers.clear()
        for child in node.children:
            retained = prune(child, path)
            if retained.obj is not None or retained.children:
                result.add(retained)
        return result

    result = prune(assembly)
    if frame == "component":
        result.loc = selected[0][2].inverse * result.loc
    return result


def deliver(assembly, selected, request, output, progress, export_glb, missing_ids):
    outcomes = []
    for format in dict.fromkeys(request["exports"]):
        result = {"format": format, "status": "failed", "filename": None, "error": None,
                  "component_count": len(selected), "component_scope": "components.json#/selected_component_ids", "frame": request.get("frame", "assembly"),
                  "units": "m" if format == "glb" else "px" if format == "svg" else "mm", "representation": "projected_preview" if format == "svg" else "tessellated" if format in {"stl", "glb"} else "native_exchange",
                  "linear_tolerance_mm": request.get("linear_tolerance_mm", 0.05) if format in {"stl", "glb"} else None,
                  "angular_tolerance_rad": request.get("angular_tolerance_rad", 0.1) if format in {"stl", "glb"} else None}
        try:
            if missing_ids:
                raise ValueError("requested component identifiers are missing; export scope was not silently reduced")
            chosen = selected_assembly(assembly, selected, request.get("frame", "assembly"))
            path = output / f"selected.{format}"
            if progress:
                progress.advance("requested_exports")
            linear, angular = request.get("linear_tolerance_mm", 0.05), request.get("angular_tolerance_rad", 0.1)
            if format == "step":
                chosen.export(str(path), exportType="STEP", unit="MM")
            elif format == "stl":
                if not chosen.toCompound().exportStl(str(path), tolerance=linear, angularTolerance=angular, relative=False):
                    raise ValueError("STL export failed")
            elif format == "glb":
                if not export_glb(chosen, path, linear, angular):
                    raise ValueError("GLB export failed")
            elif format == "svg":
                cq.exporters.export(chosen.toCompound(), str(path), exportType="SVG")
            else:
                raise ValueError("unsupported CAD delivery format")
            if not path.stat().st_size:
                raise ValueError("requested CAD output is empty")
            result.update(status="completed", filename=path.name)
            if progress:
                progress.advance("requested_exports", path.name)
        except (ValueError, RuntimeError) as error:
            result["error"] = str(error)
        outcomes.append(result)
    return outcomes
