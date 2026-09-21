"""Import every XCAF root and instance, including repeated component names."""

import cadquery as cq
from cadquery.occ_impl.shapes import isSubshape
from OCP.IFSelect import IFSelect_RetDone
from OCP.Interface import Interface_Static
from OCP.Quantity import Quantity_ColorRGBA
from OCP.STEPCAFControl import STEPCAFControl_Reader
from OCP.TCollection import TCollection_ExtendedString
from OCP.TColStd import TColStd_SequenceOfAsciiString
from OCP.TDataStd import TDataStd_Name
from OCP.TDF import TDF_Label, TDF_LabelSequence
from OCP.TDocStd import TDocStd_Document
from OCP.XCAFDoc import XCAFDoc_DocumentTool, XCAFDoc_ColorSurf, XCAFDoc_ColorGen


def label_name(label):
    attribute = TDataStd_Name()
    if label.FindAttribute(TDataStd_Name.GetID_s(), attribute):
        return attribute.Get().ToExtString() or None
    return None


def import_step(path):
    reader = STEPCAFControl_Reader()
    reader.SetColorMode(True)
    reader.SetNameMode(True)
    Interface_Static.SetCVal_s("xstep.cascade.unit", "MM")
    if reader.ReadFile(str(path)) != IFSelect_RetDone:
        raise ValueError("STEP reader could not read the source")
    lengths = TColStd_SequenceOfAsciiString()
    angles = TColStd_SequenceOfAsciiString()
    solid_angles = TColStd_SequenceOfAsciiString()
    reader.Reader().FileUnits(lengths, angles, solid_angles)
    units = [lengths.Value(i).ToCString() for i in range(1, lengths.Length() + 1)]
    document = TDocStd_Document(TCollection_ExtendedString("printable"))
    if not reader.Transfer(document):
        raise ValueError("STEP reader could not transfer geometry")
    shapes = XCAFDoc_DocumentTool.ShapeTool_s(document.Main())
    colors = XCAFDoc_DocumentTool.ColorTool_s(document.Main())
    unmatched_colors = 0

    def color(label):
        rgba = Quantity_ColorRGBA()
        for kind in (XCAFDoc_ColorSurf, XCAFDoc_ColorGen):
            if colors.GetColor_s(label, kind, rgba):
                return cq.Color(rgba.GetRGB().Red(), rgba.GetRGB().Green(), rgba.GetRGB().Blue(), rgba.Alpha(), False)
        return None

    def component(label, index, ancestors):
        nonlocal unmatched_colors
        target = label
        if shapes.IsReference_s(label):
            target = TDF_Label()
            if not shapes.GetReferredShape_s(label, target):
                raise ValueError("STEP contains an unresolved component reference")
        if any(target.IsEqual(ancestor) for ancestor in ancestors):
            raise ValueError("STEP contains a cyclic assembly reference")
        occurrence_name = label_name(label)
        product_name = label_name(target)
        original_name = occurrence_name or product_name or "component"
        name = original_name.replace("/", "_") + f"_{index}"
        location = cq.Location(shapes.GetLocation_s(label))
        node = cq.Assembly(name=name, loc=location, color=color(label) or color(target), metadata={"source_name": original_name, "occurrence_name": occurrence_name, "product_name": product_name})
        if shapes.IsAssembly_s(target):
            children = TDF_LabelSequence()
            shapes.GetComponents_s(target, children)
            for i in range(1, children.Length() + 1):
                node.add(component(children.Value(i), i, ancestors + [target]))
        else:
            native = shapes.GetShape_s(target)
            if native.IsNull():
                raise ValueError("STEP component contains no shape")
            node.obj = cq.Shape.cast(native).located(cq.Location())
            subshapes = TDF_LabelSequence()
            shapes.GetSubShapes_s(target, subshapes)
            for i in range(1, subshapes.Length() + 1):
                sublabel = subshapes.Value(i)
                subcolor = color(sublabel)
                if subcolor:
                    colored = cq.Shape.cast(shapes.GetShape_s(sublabel)).moved(cq.Location(native.Location()).inverse)
                    if colored.isSame(node.obj):
                        node.color = subcolor
                    else:
                        targets = colored.Faces() if isinstance(colored, cq.Compound) else [colored]
                        for target_shape in targets:
                            if isSubshape(target_shape, node.obj):
                                node.addSubshape(target_shape, color=subcolor)
                            else:
                                unmatched_colors += 1
        return node

    roots = TDF_LabelSequence()
    shapes.GetFreeShapes(roots)
    if roots.IsEmpty():
        raise ValueError("STEP contains no free shapes")
    assembly = cq.Assembly(name="model")
    for i in range(1, roots.Length() + 1):
        assembly.add(component(roots.Value(i), i, []))
    return assembly, {"declared_length_units": units, "resolved_length_unit": "mm", "root_count": roots.Length(), "unmatched_color_shapes": unmatched_colors}
