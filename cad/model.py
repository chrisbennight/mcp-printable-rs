"""Versioned native geometry and assembly metadata; never executable Python."""

from io import BytesIO
import json
from zipfile import ZIP_STORED, ZipFile

import cadquery as cq
import OCP
from OCP.gp import gp_Trsf


def save_model(assembly, source, path):
    # Binary BREP preserves exact topology without triangulation or STEP translation.
    with ZipFile(path, "w", compression=ZIP_STORED) as archive:
        count = 0
        shapes = {}

        def save_node(node):
            nonlocal count
            shape = node.obj
            if isinstance(shape, cq.Workplane):
                shape = cq.Compound.makeCompound(s for s in shape.vals() if isinstance(s, cq.Shape))
            transform = node.loc.wrapped.Transformation()
            row = {"name": node.name, "location": [transform.Value(i,j) for i in range(1,4) for j in range(1,5)],
                   "color": list(node.color.toTuple()) if node.color else None,
                   "material": ({"name": node.material.name, "description": node.material.description,
                                 "density": node.material.density, "densityUnit": node.material.densityUnit}
                                if node.material else None),
                   "metadata": {key: node.metadata.get(key) for key in
                                ("source_name", "occurrence_name", "product_name") if key in node.metadata},
                   "shape": None, "attributes": []}
            if shape is not None:
                identity = id(node.obj)
                index = shapes.get(identity)
                if index is None:
                    index = count
                    count += 1
                    data = BytesIO()
                    if not shape.exportBin(data):
                        raise ValueError("native model retention failed")
                    archive.writestr(f"shape-{index}.bin", data.getvalue())
                    shapes[identity] = index
                row["shape"] = index
                # Index every topology type so native subshape names/colors survive reuse.
                has_attributes = any(getattr(node, f"_subshape_{key}") for key in ("names", "colors", "layers"))
                for kind in (("Compounds", "CompSolids", "Solids", "Shells", "Faces", "Wires", "Edges", "Vertices") if has_attributes else ()):
                    for number, subshape in enumerate(getattr(shape, kind)()):
                        values = {}
                        for key in ("names", "colors", "layers"):
                            value = getattr(node, f"_subshape_{key}").get(subshape)
                            if value is not None:
                                values[key] = list(value.toTuple()) if key == "colors" else value
                        if values:
                            row["attributes"].append({"kind": kind, "index": number, **values})
            row["children"] = [save_node(child) for child in node.children]
            return row

        manifest = {"format": "printable_native_cad", "version": 1,
                    "cadquery_version": cq.__version__, "ocp_version": OCP.__version__, "units": "mm",
                    "source": source, "assembly": save_node(assembly)}
        archive.writestr("manifest.json", json.dumps(manifest, allow_nan=False))


def load_model(path):
    with ZipFile(path) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        if (manifest.get("format") != "printable_native_cad" or manifest.get("version") != 1
                or manifest.get("cadquery_version") != cq.__version__ or manifest.get("ocp_version") != OCP.__version__
                or manifest.get("units") != "mm"):
            raise ValueError("retained CAD model is incompatible; inspect its evidence or import source with the current worker")
        shapes = {}

        def load_node(row):
            index = row["shape"]
            if index is not None and (type(index) is not int or index < 0):
                raise ValueError("invalid retained native shape identifier")
            if index is not None and index not in shapes:
                shapes[index] = cq.Shape.importBin(BytesIO(archive.read(f"shape-{index}.bin")))
            shape = None if index is None else shapes[index]
            location = gp_Trsf()
            location.SetValues(*row["location"])
            node = cq.Assembly(shape, name=row["name"], loc=cq.Location(location),
                               color=cq.Color(*row["color"]) if row["color"] else None,
                               material=cq.Material(**row["material"]) if row["material"] else None,
                               metadata=row["metadata"])
            for attribute in row["attributes"]:
                kind = attribute["kind"]
                if kind not in {"Compounds", "CompSolids", "Solids", "Shells", "Faces", "Wires", "Edges", "Vertices"}:
                    raise ValueError("invalid retained native topology type")
                subshape = getattr(shape, kind)()[attribute["index"]]
                node.addSubshape(subshape, name=attribute.get("names"), layer=attribute.get("layers"),
                                 color=cq.Color(*attribute["colors"]) if "colors" in attribute else None)
            for child in row["children"]:
                node.add(load_node(child))
            return node

        return load_node(manifest["assembly"]), manifest["source"]
