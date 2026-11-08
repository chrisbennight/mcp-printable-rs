# Read mesh manufacturing evidence

`validate_mesh` analyzes an STL snapshot interpreted in millimetres.
`scad_build` with `action: "mesh"` returns the same analysis of its output.
Use `report.assessment` for validation, or `validation.assessment` for an
OpenSCAD build. The assessment has scope `mesh_geometry`; it does not approve
a part for fabrication or use.

Each criterion has a `status`. Checked criteria also have an `evidence` JSON
pointer relative to the containing mesh report, so callers can inspect the
measurements without duplicating them in a summary.

| Criterion | What the result establishes |
| --- | --- |
| `solid_topology` | `passed` for accepted solid geometry, otherwise `failed`; see `topology` and the repair recommendations in `issues`. |
| `finite_dimensions` | Finite bounds were measured. This does not compare them to a design requirement or establish the STL's original units. |
| `support_free_orientation` | `passed` when the selected build direction and angle policy detect no support requirement, otherwise `failed`; inspect `overhang` and its threshold. Bridging and actual support design are not evaluated. |
| `wall_thickness` | `unmeasured` unless a minimum wall is supplied and the mesh is a valid solid. Then `failed` when a measured surface point is thinner than the minimum, `unmeasured` when no point could be measured, and otherwise `passed`; see `wall_thickness` and [measure wall thickness](#measure-wall-thickness). |
| `dimensional_requirements` | `unmeasured` unless dimensional requirements are supplied. Then `failed` when any requirement is outside its tolerance, `unmeasured` when a feature it uses could not be fitted, and otherwise `passed`; see `dimensions` and [check dimensions](#check-dimensions). |
| `build_envelope` | `unmeasured`; no printer envelope was supplied. |
| `material_process` | `unmeasured`; no material/process qualification runs. |
| `physical_performance` | `physical_test_required`; geometric analysis cannot establish actual fit, strength, or service performance. |

The assessment is `failed` if solid topology, finite dimensions, the
support-free orientation criterion, a measured wall thickness, or a dimensional
requirement fails. Support warnings remain in `issues`; a failure of the
support-free criterion calls for reorientation, support planning, or a design
change, and does not assert that the part can never be manufactured. Otherwise
the assessment is `incomplete`, because unmeasured and physical criteria remain.
It never reports full manufacturing qualification.

For example, validate an exported part through the direct client:

```sh
python3 scripts/printable_client.py call validate_mesh examples/quickstart/validate.json
```

Inspect `report.solid_geometry`, then the individual criteria and their
evidence. A thin closed box can pass solid topology while wall thickness
remains unmeasured; supply a minimum wall to measure it. A tilted closed box can pass solid topology while failing
the support-free orientation criterion. Neither result proves suitability for
the intended physical use.

## Measure wall thickness

Pass `minimum_wall_mm` to `validate_mesh` to measure a valid solid's wall
thickness against the thinnest wall your printer and material can make. An
OpenSCAD build with a product `design_profile` uses the profile's
`minimum_wall_mm` automatically.

```json
{"path": "models/bracket.stl", "minimum_wall_mm": 1.2}
```

Points are spread over the whole surface so that every part of it lies within
the sample spacing of a point; the spacing starts at the minimum wall. From
each point, one ray travels into the material along the inward face normal
and a ring of rays surrounds it. A point's reading is the
shortest distance at which an accepted ray leaves the solid. A ray that
leaves through a surface turned more than 60 degrees from facing back against
the point's inward direction is not counted, because it reached a neighbouring
face across an edge rather than crossing a wall. Edges sharper than 60 degrees,
such as knife edges and pointed rib tips, do read thin near the edge. The solid is measured after overlapping shells are united, so buried
surfaces do not read as walls.

`report.wall_thickness` contains:

| Field | Meaning |
| --- | --- |
| `thickness_mm` | Area-weighted `minimum`, `p5`, `p50`, `p95`, and `maximum` of the readings. |
| `thin_area_mm2`, `thin_region_count` | Surface area with readings below the minimum, and the number of thin regions. A region is a cluster of thin readings within about two sample spacings of each other, so both faces of a thin wall form one region and patches parted by a wider band of thicker surface stay apart. |
| `thin_regions` | The largest thin regions with area, thinnest reading, its location, and bounds. |
| `sample_spacing_mm`, `samples` | How densely the surface was sampled: every surface point lies within the spacing of a sample. The spacing widens on large parts so that large faces do not multiply the work. Every triangle keeps at least one point, so on a dense mesh the work grows with the triangle count, like the rest of the analysis. |
| `unmeasured_samples`, `measured_area_share` | Points where every ray left through a glancing surface, and the share of surface measured. |

A wall is usually reported from both of its faces. Where a wall meets a face
end-on, such as the rim on top of a thin side wall, that face reads the wall's
height; the side wall's own faces report its thickness. Sampling can miss a
feature narrower than the sample spacing. A `passed` result means no measured
point was thinner than the minimum; it does not prove the part prints or is
strong enough.

## Check dimensions

Pass `dimensions` to `validate_mesh` to measure features of any STL, whether
it came from OpenSCAD, Blender, CadQuery or a download. Name each feature,
give its shape (`plane`, `cylinder` or `sphere`) and a seed point on or near
its surface, then list requirements as a nominal value and a tolerance either
side:

```json
{"path": "models/bracket.stl", "dimensions": {
  "features": [
    {"name": "bore", "shape": "cylinder", "seed_mm": [24, 0, 5], "direction": [0, 0, 1]},
    {"name": "top", "shape": "plane", "seed_mm": [10, 10, 10]},
    {"name": "bottom", "shape": "plane", "seed_mm": [10, 10, 0]}
  ],
  "requirements": [
    {"measure": "diameter", "features": ["bore"], "nominal": 8, "tolerance": 0.1},
    {"measure": "distance", "features": ["top", "bottom"], "nominal": 10, "tolerance": 0.05},
    {"measure": "angle", "features": ["bore", "top"], "nominal": 90, "tolerance": 0.5}
  ]
}}
```

Each feature grows from the face nearest its seed across shared edges. A face
joins while its corners lie on the fitted shape, within three times the fit's
spread or the coordinates' rounding, whichever is larger, and its normal agrees
with the shape's to within 45 degrees. The shape
is refitted to the region's vertices until the region stops changing, so the
fit stops where the surface leaves the shape, such as at an edge or where a
chamfer or fillet begins. A side tapering by less than 1 degree from a
cylinder's axis counts as part of the cylinder. A seed beside an
edge can land on the neighbouring face. Give `direction`, a plane's outward
normal or a cylinder's axis, and the region starts from the nearest face that
agrees with it. `exclude_beyond_sigma` (1, 2 or 3) refits without vertices
farther than that many standard deviations from the first fit, and reports how
many were left out.

`report.dimensions.features` gives each fit, or `unmeasured_reason` when the
surface around the seed does not have the requested shape:

| Field | Meaning |
| --- | --- |
| `seed_distance_mm` | Distance from the seed to the face the region grew from. A large value means the seed missed the intended surface. |
| `faces`, `area_mm2`, `region` | The faces fitted, their area, and their bounds. |
| `points`, `excluded_points`, `sigma_mm` | The vertices fitted, those left out, and their root-mean-square distance from the shape. |
| `deviation_mm`, `form_error_mm` | The region surface's largest distance inside and outside the shape, and their difference. Distances run along a plane's normal, or away from a cylinder's axis or a sphere's centre. |
| `plane`, `cylinder`, `sphere` | The fitted shape. A cylinder reports its axis point midway along the region, axis direction, radius, diameter, length, and `arc_degrees`, how much of the circle its faces cover. A radius fitted to a small arc is less certain. |

A tessellated curve has its vertices on the designed circle and flat faces
between them that cut inside it. The fitted radius therefore reports the
designed size, and `deviation_mm` reports the facets' departure: on a hole the
flats come closer to the axis than the vertices do. A diameter requirement
passes only when the whole range in `range`, from the facets' diameter to the
vertices' diameter, lies within the tolerance. An 8 mm hole built with
OpenSCAD's default resolution has thirteen sides and spans about 7.77 to 8 mm,
so it fails 8 mm plus or minus 0.1 mm; raise `$fn` or lower `$fs` to pass.

`report.dimensions.requirements` gives each check's `status` (`passed`,
`failed` or `unmeasured`), `measured` value, and `range` for diameters.
Distance is measured to a plane when either feature is one, otherwise to a
cylinder's axis, otherwise between sphere centres. Between two planes or two
axes it is the mean of each centre's distance to the other. An angle runs from
0 for parallel to 90 for perpendicular, between two planes, two axes, or an
axis and a plane. `nominal_far_from_measured` marks a failure far outside the
tolerance, which more often means a mistyped nominal, such as a radius given
for a diameter, or a seed on the wrong feature than a manufacturing error.
Requirements outside their tolerance add a `dimensions_out_of_tolerance`
warning, and features that could not be fitted add `features_unmeasured`.
These are measurements of the mesh, not of a printed part.

## Check a design revision's requirements

Pass `requirements` to check a mesh against the requirements declared in a
project design revision, whatever made the mesh: OpenSCAD, Blender, an import
or a CAD export. Name the project, and optionally a revision identity; without
one the project's current revision is used:

```json
{"path": "projects/bracket/build/bracket.stl", "requirements": {"project_id": "bracket"}}
```

The result adds `revision`, the identity checked, and `requirements`, the same
assessment a CAD build of that revision returns: each requirement's `status`
and the `evidence` it was decided from, as a pointer into
`requirement_evidence`. That object gives the mesh's size in `bounds_mm`,
whether it is a valid solid in `valid`, and, when the revision declares
`hole_pattern` requirements, the holes measured around the declared positions
in `vertical_holes`.

Each declared hole is fitted as a cylinder from the inward-facing wall face,
within twice the declared radius of the declared centre, whose distance from
that centre is nearest the declared radius. It counts when its axis is within 1
degree of vertical and its wall covers the whole circle and faces the axis, so
bosses and partial bores do not count. The fitted centre and radius are matched
against the declaration, so a hole that moved or changed size fails rather than
being found where it was declared. A tessellated hole's radius is that of the
circle through its vertices, the designed size for OpenSCAD's tessellation. A
polygonal hole such as a hexagon counts at the radius through its corners,
because the mesh does not say whether it was meant to be round. Holes are
measured only on a valid solid; otherwise `hole_pattern` stays `unmeasured`.

`dimensions` and `build_envelope` compare the mesh's bounds. `clearance` stays
`unmeasured` and `physical_test` stays `physical_test_required`, as for CAD
builds. The tool checks the mesh it is given, not how the mesh was made, so
build the mesh from the revision's source and parameter values for the result
to describe that revision.

## Check mould draft

Pass `draft` to `validate_mesh` to check whether a master or cavity releases
from a mould. Give the pull direction: the direction the material on the other
side of the surface moves away. For a master resting on its base, that is the
direction the mould lifts off it. For a cavity or printed mould, it is the
direction the cast part comes out.

```json
{"path": "models/master.stl", "draft": {"pull_direction": [0, 0, 1], "minimum_draft_degrees": 1}}
```

A face's draft is the angle it leans off the pull. It is positive when the face
slides free, zero for a wall parallel to the pull, and negative when the face
leans back and catches on the mould. `report.draft` sorts faces into four groups:

| Group | Meaning |
| --- | --- |
| `base` | Faces on the part's lowest plane facing against the pull, which rest on the tray floor or the mould's open face. |
| `released` | Faces with at least `minimum_draft_degrees` of draft (default 1). |
| `shallow` | Faces leaning less than the minimum, including straight walls. |
| `undercut` | Faces leaning back against the pull. |

Each group reports its face count and area. `shallow_patches` and
`undercut_patches` list the largest connected patches, with area, most negative
and mean draft, centre, and bounds. Undercuts add a `mould_undercuts` warning
but do not change the assessment: a flexible material can release a small
undercut, so the caller decides. For a two-part mould, `parting_offset_mm`
gives the parting plane's distance along the pull from the part's lowest
point. Faces below it are judged against the opposite direction, and there is
no `base` group. A face crossing the plane is split there, so each part is
judged against its own half, and no patch spans both halves. A face lying on
the plane belongs to the half it faces.

## Compare with a reference

Pass `compare` to `validate_mesh` to measure how far the mesh's surface lies
from a reference mesh, and where. Use it to check how far a repaired, rebuilt
or hollowed mesh moved from its source, to compare two revisions by shape, or
to compare a scan of a printed part with its design:

```json
{"path": "scans/bracket-scan.stl", "compare": {
  "reference_path": "models/bracket.stl", "alignment": "best_fit", "tolerance_mm": 0.2
}}
```

With `alignment: "none"`, the default, both meshes are taken to share a
frame. With `best_fit`, the mesh is rotated and moved onto the reference, but
never scaled, so a size error such as shrinkage shows as deviation. The fit
starts from the meshes as given and from every rotation that maps one mesh's
principal axes onto the other's, refines each by point-to-plane iterative
closest point with outliers left out, and keeps the pose with the smallest
distances over all its points, each capped and measured to surface facing the
point's way, so a pose that turns a surface over loses. A part with a rotational or
mirror symmetry can fit equally well in more than one pose.

Points spread over the mesh's surface are each measured against the nearest
reference surface that faces the same way, so a point on one side of a thin
wall is not measured against the other side. Deviation is measured along that
surface's outward normal: positive outside it, where the mesh has extra
material, and negative inside it, where material is missing. A point with no reference surface facing its way
within `max_distance_mm`, a tenth of the reference's diagonal by default, is
unmeasured. So is a point whose search for that surface would pass more than
1,024 reference faces turned the other way, which keeps the work in proportion
to the mesh when its faces turn away from the reference's, as with reversed
normals.

| Field | Meaning |
| --- | --- |
| `alignment` | The method, the rotation and translation that carry the mesh into the reference's frame, and the rotation angle. A best fit also reports its points, the points matched in its last step, and their RMS distance from the reference. |
| `sample_spacing_mm`, `samples` | How densely the surface was sampled, and the points measured. The spacing widens on large meshes so that large faces do not multiply the work. Every triangle keeps at least one point, so on a dense mesh the work grows with the triangle count, like the rest of the analysis. |
| `unmeasured_samples`, `measured_area_share` | Points with no reference surface facing their way in reach, or whose search for it gave up, and the share of the surface measured. |
| `deviation_mm` | Area-weighted minimum, 5th, 50th and 95th percentiles, maximum, mean and RMS of the measured deviation. |
| `within_tolerance_share` | Share of the measured surface within `tolerance_mm` either side. |
| `region_count`, `regions` | Clusters of points beyond the tolerance, outside and inside the reference separately, largest first. Points within about two sample spacings join one cluster, so both faces of a wall that came out thicker form one region. Each gives its area, largest deviation, where that lies, its centre, and its bounds, in the reference's frame. |

Regions add a `deviation_beyond_tolerance` warning. The response also names
the reference in `reference_artifact`. A comparison measures surfaces; it does
not say which mesh is right.

## Compatibility

`printable` remains equal to `solid_geometry` for existing callers. It is
marked deprecated in the output schema; its historical truth value has not
changed. New code should use `solid_geometry` for that geometric question and
`assessment` for the scope and outstanding checks. Full and selected contract
resources expose these fields, and normal results retain every criterion.
Historical saved reports without `assessment` supply no additional
qualification evidence; revalidate their exact source artifact when needed.

The OpenSCAD product kit also returns constructive manufacturing evidence.
Keep its local guards and assembly-clearance results alongside the mesh
assessment. Neither a nominal wall parameter nor a rendered image can turn an
unmeasured global requirement into a pass.
