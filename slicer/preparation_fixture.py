"""Software-only native slicing fixtures. These profiles must never be printed."""

import json
from pathlib import Path
import re
import shutil


PRINTER = "Printable software fixture (no physical printer)"
PROCESS = "Printable software fixture process"
PLA = "Printable software fixture PLA"
PETG = "Printable software fixture PETG"


def extruded_fixture(destination, kind):
    """Write original, closed software-test geometry in millimetres.

    The bridge has an accessible 12 mm underside; the relief has two 1 mm
    ridges separated by 4 mm. The flat fitting face remains 20 by 10 mm.
    """
    polygons = {
        "bridge": [(0, 0), (4, 0), (4, 4), (16, 4), (16, 0), (20, 0), (20, 6), (0, 6)],
        "relief": [(0, 0), (20, 0), (20, 1), (14, 1), (14, 1.6), (13, 1.6),
                   (13, 1), (9, 1), (9, 1.6), (8, 1.6), (8, 1), (0, 1)],
        "flat": [(0, 0), (20, 0), (20, 2), (0, 2)],
    }
    polygon = polygons[kind]
    indices = list(range(len(polygon)))
    caps = []

    def side(a, b, c):
        return ((b[0] - a[0]) * (c[1] - a[1])
                - (b[1] - a[1]) * (c[0] - a[0]))

    while len(indices) > 3:
        for offset, b in enumerate(indices):
            a, c = indices[offset - 1], indices[(offset + 1) % len(indices)]
            if side(polygon[a], polygon[b], polygon[c]) <= 0:
                continue
            if any(all(side(polygon[x], polygon[y], polygon[p]) >= 0
                       for x, y in ((a, b), (b, c), (c, a)))
                   for p in indices if p not in (a, b, c)):
                continue
            caps.append((a, b, c))
            indices.pop(offset)
            break
        else:
            raise AssertionError("fixture polygon cannot be triangulated")
    caps.append(tuple(indices))
    count = len(polygon)
    vertices = [(x, y, z) for y in (0, 10) for x, z in polygon]
    triangles = list(caps) + [tuple(index + count for index in reversed(face)) for face in caps]
    for a in range(count):
        b = (a + 1) % count
        triangles.extend(((a, a + count, b + count), (a, b + count, b)))
    lines = ["solid printable_software_fixture"]
    for face in triangles:
        lines.extend(("facet normal 0 0 0", "outer loop"))
        lines.extend("vertex " + " ".join(map(str, vertices[index])) for index in face)
        lines.extend(("endloop", "endfacet"))
    lines.append("endsolid printable_software_fixture")
    Path(destination).write_text("\n".join(lines) + "\n")
    return {"kind": kind, "bounds_mm": {"min": [0, 0, 0], "max": [20, 10, max(z for _, z in polygon)]},
            "triangles": len(triangles), "physical_result": "not_observed"}


def create_software_profiles(packaged_root, destination):
    """Retain the BBL tower algorithm while replacing unmodeled firmware scripts.

    The temporary child profiles make coordinate/path assertions possible without
    treating proprietary firmware macros as simulated motion. Native production
    profiles and public G-code override restrictions are unchanged.
    """
    packaged_root = Path(packaged_root)
    destination = Path(destination)
    shutil.copytree(packaged_root, destination)
    library = packaged_root.parent / "OrcaFilamentLibrary"
    if library.is_dir():
        shutil.copytree(library, destination.parent / "OrcaFilamentLibrary")

    def write(category, name, parent, settings):
        data = {"type": {"machine": "machine", "process": "process", "filament": "filament"}[category],
                "name": name, "inherits": parent, "from": "user", "instantiation": "true",
                "description": "Software regression fixture; never send its output to a printer.",
                **settings}
        (destination / category / f"{name}.json").write_text(json.dumps(data))

    write("machine", PRINTER, "Bambu Lab X1 Carbon 0.4 nozzle", {
        "machine_start_gcode": "; software fixture, never print\nG90\nM83\nG92 E0\nG1 X0 Y0 Z0 F1200\n",
        "machine_end_gcode": "; software fixture end\nM400\n",
        "change_filament_gcode": "T[next_extruder]\nG92 E0\n",
        "before_layer_change_gcode": "", "layer_change_gcode": "", "time_lapse_gcode": "",
        "change_extrusion_role_gcode": "", "printing_by_object_gcode": "",
        "wrapping_detection_gcode": "", "file_start_gcode": "",
        "use_relative_e_distances": "1",
    })
    write("process", PROCESS, "0.20mm Standard @BBL X1C", {
        "compatible_printers": [PRINTER], "process_change_extrusion_role_gcode": "",
    })
    for name, parent in [(PLA, "Bambu PLA Basic @BBL X1C"),
                         (PETG, "Bambu PETG HF @BBL X1C")]:
        write("filament", name, parent, {
            "compatible_printers": [PRINTER], "filament_start_gcode": ["; software fixture material"],
            "filament_end_gcode": [""], "filament_change_extrusion_role_gcode": [""],
        })
    return {"printer": PRINTER, "process": PROCESS, "pla": PLA, "petg": PETG}


def linear_deposition_paths(gcode):
    """Measure nominal linear paths in the minimal-script native fixture only.

    This independent test oracle does not simulate firmware or serve public
    requests. Width/height envelopes and physical clearance are separate checks.
    """
    position = [None, None, None]
    relative = False
    relative_e = True
    extrusion = 0.0
    feature = None
    material = None
    changes = []
    paths = []
    retracted = {}
    for number, line in enumerate(gcode.splitlines(), start=1):
        if line.startswith(("; FEATURE:", ";TYPE:")):
            feature = line.split(":", 1)[1].strip()
        words = line.split(";", 1)[0].split()
        if not words:
            continue
        command = words[0]
        if re.fullmatch(r"T\d+", command):
            material = int(command[1:])
            if material in (0, 1):
                changes.append((number, material))
            continue
        if command in ("G90", "G91"):
            relative = command == "G91"
            continue
        if command in ("M82", "M83"):
            relative_e = command == "M83"
            continue
        if command not in ("G0", "G1", "G00", "G01", "G92"):
            continue
        values = {word[0]: float(word[1:]) for word in words[1:]
                  if word[0] in "XYZE"}
        if command == "G92":
            assert not any(axis in values for axis in "XYZ"), "fixture unexpectedly resets its coordinate frame"
            extrusion = values.get("E", extrusion)
            continue
        before = list(position)
        for index, axis in enumerate("XYZ"):
            if axis in values:
                position[index] = ((position[index] + values[axis] if position[index] is not None else None)
                                   if relative else values[axis])
        amount = 0.0
        if "E" in values:
            amount = values["E"] if relative_e else values["E"] - extrusion
            extrusion = extrusion + values["E"] if relative_e else values["E"]
        debt = retracted.get(material, 0.0)
        if amount < 0:
            retracted[material] = debt - amount
        else:
            restored = min(amount, debt)
            retracted[material] = debt - restored
            amount -= restored
        if (amount > 0
                and all(value is not None for value in before + position)
                and before != position):
            paths.append({"line": number, "material": material, "feature": feature,
                          "from_mm": before, "to_mm": list(position)})
    return paths, changes


def tower_paths(gcode):
    """Require real repeated changes and measure native prime-tower centerlines."""
    deposition, changes = linear_deposition_paths(gcode)
    paths = [path for path in deposition if path["feature"] in ("Prime tower", "Wipe tower")]
    assert len(changes) >= 8 and {material for _, material in changes} == {0, 1}, (
        "fixture did not generate repeated actual material changes", changes)
    assert len(paths) >= 8 and {path["material"] for path in paths} == {0, 1}, (
        "fixture did not generate prime-tower paths for both materials", len(paths))
    points = [point for path in paths for point in (path["from_mm"], path["to_mm"])]
    return {"paths": paths, "toolchanges": changes,
            "minimum_mm": [min(point[axis] for point in points) for axis in range(3)],
            "maximum_mm": [max(point[axis] for point in points) for axis in range(3)]}
