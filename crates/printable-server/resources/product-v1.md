# Generic FDM product design kit (`product_v1`)

`product_v1` is a millimetre-based OpenSCAD kit for hard-surface and prismatic
products such as enclosures, brackets, grips, fixtures, housings, and mechanisms.
The build direction is always `+Z`. Select it by passing a complete
`design_profile` to any OpenSCAD compile, render, or cross-section call. Profile
values are explicit inputs; Printable does not infer a printer or material.

The server validates the profile before queue admission, confines the caller
source, then stages a trusted wrapper that loads the bundled kit and that
already-confined source. Caller `include` and `use` directives remain forbidden.
Caller source may invoke public `pbl_*` modules and read profile definitions,
but cannot declare modules or functions under the public `pbl_*` or internal
`_pbl_*` namespaces. This prevents caller code from replacing trusted
constructive guards after the kit loads.

## Profile

```json
{
  "kit": "product_v1",
  "manufacturing": {
    "nozzle_diameter_mm": 0.4,
    "layer_height_mm": 0.2,
    "minimum_wall_mm": 2.0,
    "moving_clearance_mm": 0.35,
    "maximum_overhang_degrees": 45.0
  },
  "form": {
    "primary_radius_mm": 3.0,
    "secondary_radius_mm": 1.5,
    "edge_break_mm": 0.5,
    "transition_length_mm": 8.0
  }
}
```

Every number must be positive and finite, the overhang angle cannot exceed
90 degrees, and radii must satisfy `primary >= secondary >= edge_break`.
Profile values enter OpenSCAD through reserved `pbl_*` definitions.
Omitting a module's nominal dimensions produces a profile-scaled starter shape;
explicit dimensions remain caller controlled and are checked against the same
constructive invariants.

## Modules

- `pbl_roundrect_2d(size=[x,y], radius=pbl_primary_radius_mm)` — centered
  rounded 2D outline.
- `pbl_panel(size=[x,y,z], radius=..., edge_break=...)` — rounded panel with
  profile-safe bed chamfers rather than a downward-facing bed fillet.
- `pbl_shell(size=[x,y,z], wall=pbl_minimum_wall_mm, radius=...,
  edge_break=..., open_top=true)` — facet-aware shell whose final polygonal
  corner wall cannot be below the requested wall or profile minimum. Straight
  regions can be conservatively thicker by the small tessellation allowance.
  When the wall exceeds the outside corner radius, the inner corner becomes
  square instead of making the complete profile unusable. The starter shell
  wall may also exceed the profile minimum when that is necessary to preserve
  the requested edge break.
- `pbl_capsule(length, diameter, height, edge_break=...)` — capsule panel for
  handles, grips, feet, and soft-industrial silhouettes.
- `pbl_tapered_transition(length=pbl_transition_length_mm,
  start_size=[y,z], end_size=[y,z], radius=...)` — controlled rounded
  transition along X. Use it between bodies; do not use its rounded lower edge
  as a bed-contact datum.
- `pbl_rib(length, height, thickness=pbl_minimum_wall_mm)` — triangular
  reinforcement in the X/Z plane with a constructively guarded thickness.
- `pbl_boss(height, outer_diameter, bore_diameter=0)` — vertical boss; a bored
  boss preserves both requested diameters and rejects inputs whose final
  polygon-facet separation would fall below the profile minimum annular wall.
- `pbl_horizontal_bore_cutter(length, diameter, axis="x"|"y")` — teardrop bore
  cutter with a roof derived from the selected overhang policy for support-free
  horizontal holes.
- `pbl_linear_pattern(count, spacing, axis="x"|"y"|"z")` — bounded repetition
  of child geometry.

Curve tessellation derives from feature size and nozzle diameter and is clamped
to 12–96 segments. This keeps circles visually useful without allowing a large
aesthetic radius to create unbounded facet work.

Horizontal-bore roofs use 98% of the selected overhang angle, leaving a
conservative classification margin after STL tessellation. Bed chamfers use
the smaller of that angle and 45 degrees, preserving a compact edge break while
remaining within the selected policy. A stricter overhang policy produces a
taller bore roof; leave at least
`(diameter / 2) / sin(0.98 * maximum_overhang_degrees)` above the bore center.

## Product form guidance

Start with the silhouette and a small hierarchy of large, medium, and edge
radii. Reuse those radii across related parts instead of tuning every corner
independently. Align bosses, ribs, vents, holes, and split lines to a common
spacing rhythm. Use the profile transition length for deliberate blends between
major masses; avoid decorative micro-features that disappear at the selected
nozzle and layer scale. Carry part lines and dominant edges continuously across
an assembly. Keep functional features—interfaces, clearances, fasteners, datum
surfaces—separate from decorative detail so each can be revised honestly.

Bed-contacting edges should be flat or chamfered. Rounded lower edges grow
outward before they are supported and should be reserved for geometry already
supported by a body below.

## Rounded enclosure example

```scad
union() {
    pbl_shell(size=[60, 40, 20], wall=pbl_minimum_wall_mm);
    pbl_linear_pattern(count=2, spacing=36, axis="x")
        translate([-18, -10, 0])
            pbl_boss(height=8, outer_diameter=9, bore_diameter=3);
    translate([-20, 0, pbl_minimum_wall_mm - 0.2])
        pbl_rib(length=12, height=8.2);
}
```

## Support-free mounting bracket example

```scad
difference() {
    union() {
        pbl_panel(size=[50, 24, 4]);
        translate([-18, -6, 3.8]) cube([36, 12, 24.2]);
        translate([-18, 0, 3.8]) pbl_rib(length=12, height=12.2);
    }
    translate([0, 0, 17])
        pbl_horizontal_bore_cutter(length=40, diameter=8, axis="x");
}
```

## Pocket or insert for an existing part

To hold an existing part in a tool holder, drawer insert, or bin, cut a pocket
from the part's outline. Import the part's STL into the build, project its
widest outline onto the build plate, grow that outline by the clearance, and
cut it from a block:

```openscad
clearance = 0.5;  // gap on every side, in millimetres
depth = 12;       // pocket depth
base = 3;         // material under the part
wall = 3;         // material around the pocket

module outline()
    offset(delta = clearance)
        projection(cut = false) import("parts/tool.stl");

difference() {
    linear_extrude(height = base + depth) offset(r = wall) outline();
    translate([0, 0, base]) linear_extrude(height = depth + 1) outline();
}
```

`projection(cut = false)` gives the part's whole silhouette from above, not a
section at one height. Orient the part first so that it drops straight into the
pocket. A through hole in the part leaves a matching post, smaller by the
clearance, that locates the part. To leave the floor flat instead, close the
holes by writing `offset(delta = -r) offset(delta = r)` before the projection.
This grows the outline by `r` and then shrinks it back, so holes and notches
narrower than `2 * r` close; choose `r` larger than the largest hole radius.

Check the fit before printing. Build the part as it will sit, raised by the
base and the clearance, and run `analyze_assembly` with the pocket as the
fixed part:

```openscad
translate([0, 0, base + clearance]) import("parts/tool.stl");
```

Expect `relation: "separated"` and `clearance_mm` close to the chosen
clearance. Then validate the pocket with `validate_mesh` and a `minimum_wall_mm`
for the printer, and render it. Interference or a clearance well below the
chosen value means the outline did not capture the part, for example because
it was not oriented to drop straight in.

## Manufacturing evidence

Compile measures final-STL topology, bounds, build-plate contact, overhang
using the profile's actual `maximum_overhang_degrees`, and wall thickness
against the profile's `minimum_wall_mm`. Kit modules reject local wall, rib,
boss, radius, and shell arguments that violate their constructive invariants.

Read `validation.assessment` alongside these measurements. Its criteria use
`passed`, `failed`, `unmeasured`, and `physical_test_required`. A valid solid
with no detected support requirement still has an `incomplete` assessment:
geometry alone does not establish fit, material/process suitability, or
physical performance. The compatibility field `printable` retains its original
meaning as an alias for `solid_geometry`.

`global_minimum_wall.status` repeats the `wall_thickness` criterion: `passed`
when no point sampled over the final surface is thinner than the profile
minimum, `failed` with the thin regions listed in `validation.wall_thickness`,
or `unmeasured` when the output is not a valid solid or no point could be
measured. Sampling can miss a
feature narrower than the reported spacing, so a product that requires wall
proof must still encode exact constructive assertions.
`moving_clearance.status` remains `not_run`; moving products must export each
rigid body separately and pass `analyze_assembly`.

Compile does not check named dimensions. To check a hole's diameter, a
thickness, or an angle on the built STL, run `validate_mesh` with `dimensions`.
Fit planes, cylinders and spheres around seed points taken from the design,
and give each requirement a nominal and a tolerance. A diameter passes only
when the faceted surface stays within the tolerance, so a coarse `$fn` can fail
a hole that must fit a pin or screw.

## Complete product workflow

Use the same public sequence for enclosures, brackets, grips, mechanisms, and
other hard-surface products. First agree the requirements with the user: the
dimensions that must hold, the minimum wall and overhang the printer can make,
and any clearances. Choose dimensional tolerances from how the part is printed
and what the printer can hold, never finer than about three times the
measurement noise, such as the chord error of tessellated curves.

1. Compile the final printable variant with an explicit profile and require
   `validation.assessment.criteria.solid_topology.status` and
   `validation.assessment.criteria.support_free_orientation.status` to be
   `passed`. Inspect bounds, bed contact, and all remaining criteria before
   deciding what additional measurement or testing the product requires.
2. For an interface or moving assembly, compile every rigid body in shared
   millimetre coordinates and run `analyze_assembly`; never substitute
   `moving_clearance_mm` for that result.
3. Import the validated STL artifacts into Blender and render engineering and
   studio views. Presentation can change camera, light, material, and copied
   shading, but it cannot repair the product mesh.
4. For articulated video, save the classified fixed/moving scene and submit a
   `mechanical_rotation` job. Treat any completed frame without a stored passing
   full-arc certificate as invalid.

After each group of changes, compile and measure again, and work on the worst
remaining problem first. Report each conclusion as measured, partly supported,
or inferred, and name the result behind a measured one. Report an assumed
design value, such as the drawn 6 mm bore, beside the measured value rather
than in its place. An unmeasured criterion is not a pass. When a requirement is
not met, end with "not done" and the remaining gaps.

The repository's reference corpus applies this workflow to a rounded enclosure,
a support-free mounting bracket, a capsule/taper grip, and one articulated
fixture. The examples prove composition across products; they do not add
product-specific server generators.
