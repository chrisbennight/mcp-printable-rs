"""Independent native geometric questions with explicit per-answer failures."""

import math

import cadquery as cq
from OCP.BRepAdaptor import BRepAdaptor_Surface
from OCP.BRepAlgoAPI import BRepAlgoAPI_Section
from OCP.gp import gp_Dir, gp_Pln, gp_Pnt


def vector(value):
    return [value.X(), value.Y(), value.Z()]


def feature(face, index):
    kind = face.geomType()
    if kind not in {"PLANE", "CYLINDER"}:
        return None
    surface = BRepAdaptor_Surface(face.wrapped)
    native = surface.Plane() if kind == "PLANE" else surface.Cylinder()
    result = {"face_index": index, "kind": kind.lower(), "origin_mm": vector(native.Location()),
              "direction": vector(native.Axis().Direction()), "area_mm2": face.Area()}
    if kind == "CYLINDER":
        result["radius_mm"] = native.Radius()
    return result


def answer(question, components, frame, native_bounds, resolve_target):
    kind = question["kind"]
    pair = kind in {"distance", "alignment"}
    ids = []
    measured_frame = "assembly" if pair else frame
    result = {"id": question["id"], "kind": kind, "status": "unavailable", "frame": measured_frame,
              "component_ids": ids, "method": "native_geometry_without_triangulation",
              "evidence": None, "error": None}

    def shape_for(target):
        identity = resolve_target(target)
        ids.append(identity)
        if identity not in components:
            raise ValueError(f"component not found: {identity}")
        shape, location = components[identity]
        if measured_frame == "assembly":
            shape = shape.moved(location)
        index = target.get("face_index")
        if index is not None:
            faces = shape.Faces()
            if index >= len(faces):
                raise ValueError("face index is absent from this retained component")
            shape = faces[index]
        return shape

    try:
        if kind == "features":
            shape = shape_for(question)
            features = [value for i, face in enumerate(shape.Faces()) if (value := feature(face, i)) is not None]
            offset, limit = question.get("offset", 0), question.get("limit", 20)
            page = features[offset:offset + limit]
            evidence = {"features": page, "matched_count": len(features),
                        "next_offset": offset + len(page) if offset + len(page) < len(features) else None}
        elif kind == "distance":
            first, second = shape_for(question["first"]), shape_for(question["second"])
            evidence = {"distance_mm": first.distance(second),
                        "interpretation": "minimum separation; zero alone does not distinguish contact from overlap"}
        elif kind == "alignment":
            if question["first"].get("face_index") is None or question["second"].get("face_index") is None:
                raise ValueError("alignment requires explicit planar or cylindrical face indices from features")
            first, second = shape_for(question["first"]), shape_for(question["second"])
            if not isinstance(first, cq.Face) or not isinstance(second, cq.Face):
                raise ValueError("alignment requires explicit planar or cylindrical face indices from features")
            a, b = feature(first, question["first"]["face_index"]), feature(second, question["second"]["face_index"])
            if a is None or b is None:
                raise ValueError("alignment supports planes and cylinders")
            dot = sum(x * y for x, y in zip(a["direction"], b["direction"]))
            evidence = {"angle_degrees": math.degrees(math.acos(min(1.0, abs(dot)))),
                        "first": a, "second": b,
                        "interpretation": "unoriented plane normals or cylinder axes; angle alone does not establish coaxiality"}
        elif kind == "validity":
            shape = shape_for(question)
            evidence = {"valid": shape.isValid(), "solid_count": len(shape.Solids()),
                        "interpretation": "native geometric validity, not manufacturing or physical qualification"}
        elif kind == "section":
            shape = shape_for(question)
            plane = gp_Pln(gp_Pnt(*question["origin_mm"]), gp_Dir(*question["normal"]))
            operation = BRepAlgoAPI_Section(shape.wrapped, plane, False)
            operation.Build()
            if not operation.IsDone():
                raise ValueError("native section failed")
            section = cq.Shape.cast(operation.Shape())
            edges = section.Edges()
            evidence = {"edge_count": len(edges), "length_mm": sum(edge.Length() for edge in edges),
                        "bounds_mm": native_bounds(section) if edges else None,
                        "origin_mm": question["origin_mm"], "normal": question["normal"]}
        else:
            raise ValueError("unsupported native measurement question")
        import json
        json.dumps(evidence, allow_nan=False)
        result.update(status="measured", evidence=evidence)
    except (ValueError, RuntimeError) as error:
        result["error"] = str(error)
    return result
