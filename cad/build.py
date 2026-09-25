"""Native CadQuery work inside the dedicated, credential-free CAD container."""

import json
import math
from pathlib import Path
import runpy
import sys

import cadquery as cq
from cadquery.occ_impl.assembly import toCAF
from OCP.Message import Message_ProgressRange
from OCP.BRepAdaptor import BRepAdaptor_Surface
from OCP.RWGltf import RWGltf_CafWriter
from OCP.RWMesh import RWMesh_CoordinateSystem_Zup
from OCP.TCollection import TCollection_AsciiString
from OCP.TColStd import TColStd_IndexedDataMapOfStringString

sys.path.insert(0, str(Path(__file__).resolve().parent))
from step import import_step


def bounds(shape):
    box = shape.BoundingBox()
    return {
        "min": [box.xmin, box.ymin, box.zmin],
        "max": [box.xmax, box.ymax, box.zmax],
        "size": [box.xlen, box.ylen, box.zlen],
    }


def vertical_holes(shape):
    """Measure complete vertical cylindrical inner walls, not fastener fit."""
    faces = shape.Faces()
    if not shape.isValid() or not shape.Solids() or len(faces) > 4096:
        return {"status": "unmeasured", "holes": []}
    holes = []
    for face in faces:
        if face.geomType() != "CYLINDER":
            continue
        surface = BRepAdaptor_Surface(face.wrapped)
        cylinder = surface.Cylinder()
        axis = cylinder.Axis()
        direction = axis.Direction()
        if abs(abs(direction.Z()) - 1.0) > 1e-7:
            continue
        u0, u1, v0, v1 = face.uvBounds()
        if abs((u1 - u0) - 2 * math.pi) > 1e-6:
            continue
        normal, point = face.normalAt((u0 + u1) / 2, (v0 + v1) / 2)
        center = axis.Location()
        radial_dot = normal.x * (point.x - center.X()) + normal.y * (point.y - center.Y())
        if radial_dot >= 0:
            continue
        hole = {"center_mm": [center.X(), center.Y()], "radius_mm": cylinder.Radius()}
        if not all(math.isfinite(n) for n in [*hole["center_mm"], hole["radius_mm"]]):
            return {"status": "unmeasured", "holes": []}
        if not any(
            max(abs(a - b) for a, b in zip(hole["center_mm"], previous["center_mm"])) < 1e-7
            and abs(hole["radius_mm"] - previous["radius_mm"]) < 1e-7
            for previous in holes
        ):
            holes.append(hole)
    return {"status": "measured", "holes": holes}


def assembly_result(result):
    if isinstance(result, cq.Assembly):
        return result
    if isinstance(result, (cq.Workplane, cq.Shape)):
        return cq.Assembly(result, name="model")
    raise ValueError("script must assign a CadQuery Workplane, Shape, or Assembly to result")


def export_glb(assembly, path, linear, angular):
    _, document = toCAF(assembly, True, True, linear, angular)
    writer = RWGltf_CafWriter(TCollection_AsciiString(str(path)), True)
    converter = writer.ChangeCoordinateSystemConverter()
    converter.SetInputLengthUnit(0.001)
    converter.SetInputCoordinateSystem(RWMesh_CoordinateSystem_Zup)
    return writer.Perform(document, TColStd_IndexedDataMapOfStringString(), Message_ProgressRange())


def original_names(node, parent=""):
    path = f"{parent}/{node.name}" if parent else node.name
    result = {path: {
        "source_name": node.metadata.get("source_name", node.name),
        "occurrence_name": node.metadata.get("occurrence_name", node.name),
        "product_name": node.metadata.get("product_name"),
    }}
    for child in node.children:
        result.update(original_names(child, path))
    return result


def build(request, directory):
    source = directory / request["source"]
    source_metadata = {"resolved_length_unit": "mm"}
    if request["action"] == "model":
        sys.path.insert(0, str(directory / "inputs"))
        sys.path.insert(0, str(source.parent))
        namespace = runpy.run_path(
            str(source),
            init_globals={"parameters": request["parameters"], "cq": cq},
        )
        assembly = assembly_result(namespace.get("result"))
    elif request["action"] == "import_step":
        assembly, source_metadata = import_step(source)
    else:
        raise ValueError("unsupported CAD action")

    shape = assembly.toCompound()
    if shape.isNull() or not shape.Vertices():
        raise ValueError("CAD result contains no measurable geometry")
    measured = bounds(shape)
    if not all(math.isfinite(v) for values in measured.values() for v in values):
        raise ValueError("CAD result has nonfinite dimensions")

    components = []
    names = original_names(assembly)
    for component, name, location, color in assembly:
        transform = location.wrapped.Transformation()
        components.append({
            "name": name,
            **names[name],
            "transform": [[transform.Value(i, j) for j in range(1, 5)] for i in range(1, 4)],
            "bounds_mm": bounds(component.moved(location)),
            "color": list(color.toTuple()) if color else None,
            "solids": len(component.Solids()),
        })

    output = directory / "output"
    output.mkdir()
    assembly.export(str(output / "model.step"), exportType="STEP", unit="MM")
    if not shape.exportStl(
        str(output / "model.stl"),
        tolerance=request["linear_tolerance_mm"],
        angularTolerance=request["angular_tolerance_rad"],
        relative=False,
    ):
        raise ValueError("STL export failed")
    if not export_glb(
        assembly,
        str(output / "model.glb"),
        request["linear_tolerance_mm"],
        request["angular_tolerance_rad"],
    ):
        raise ValueError("GLB export failed")
    (output / "components.json").write_text(json.dumps({"nodes": names, "components": components}, allow_nan=False))
    report = {
        "cadquery_version": cq.__version__,
        "source": source_metadata,
        "units": "mm",
        "bounds_mm": measured,
        "solid_count": len(shape.Solids()),
        "component_count": len(components),
        "valid": shape.isValid(),
        "vertical_holes": vertical_holes(shape),
        "meshing": {
            "linear_tolerance_mm": request["linear_tolerance_mm"],
            "angular_tolerance_rad": request["angular_tolerance_rad"],
            "stl_relative_tolerance": False,
        },
        "diagnostics": [
            "Original source is retained; STEP contains geometry, not parametric design history.",
            "No explicit healing is requested; native reader and exporter diagnostics are retained in the build log.",
        ],
    }
    for filename in ("model.step", "model.stl", "model.glb", "components.json"):
        path = output / filename
        if path.stat().st_size == 0:
            raise ValueError(f"empty CAD artifact: {filename}")
    (output / "report.json").write_text(json.dumps(report, allow_nan=False))


if __name__ == "__main__":
    job = Path(sys.argv[1]).resolve()
    build(json.loads((job / "request.json").read_text()), job)
