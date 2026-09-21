"""Verify selected OpenSCAD geometry through the public MCP contract."""

import math
import uuid


SOURCE = '''part = is_undef(pbl_variant) ? "assembly" : pbl_variant;
if (part == "clip") cube([5, 15, 3.6]);
else cube([164, 122, 98.6]);
'''


def run(client):
    prefix = "scad-variant-" + uuid.uuid4().hex
    cases = [
        ("selected", SOURCE, {"variant": "clip"}, [5, 15, 3.6]),
        ("default", SOURCE, {}, [164, 122, 98.6]),
        ("declared", 'pbl_variant = "assembly";\n' + SOURCE,
         {"variant": "clip"}, [5, 15, 3.6]),
        ("product", SOURCE, {"variant": "clip", "design_profile": {
            "kit": "product_v1",
            "manufacturing": {"nozzle_diameter_mm": 0.4, "layer_height_mm": 0.2,
                              "minimum_wall_mm": 2, "moving_clearance_mm": 0.35,
                              "maximum_overhang_degrees": 50},
            "form": {"primary_radius_mm": 3, "secondary_radius_mm": 1.5,
                     "edge_break_mm": 0.5, "transition_length_mm": 8},
        }}, [5, 15, 3.6]),
    ]
    for name, source, selection, expected in cases:
        result = client.call("scad_build", {"action": "mesh", "params": {
            "source": source, "path": prefix + "-" + name + ".stl", **selection,
        }})
        actual = result["validation"]["bounds"]["dimensions_mm"]
        if len(actual) != 3 or not all(math.isclose(value, target, abs_tol=1e-4)
                                       for value, target in zip(actual, expected)):
            raise ValueError("OpenSCAD " + name + " generated incorrect variant geometry")
        if result["definitions"]["variant_applied"] != bool(selection):
            raise ValueError("OpenSCAD variant metadata differs from the requested selection")
    print("SCAD_VARIANT_CLIENT_OK: selected, default, declared and product geometry")
