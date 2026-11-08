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
| `dimensional_requirements` | `unmeasured`; no target dimensions or tolerances were supplied to this analysis. |
| `build_envelope` | `unmeasured`; no printer envelope was supplied. |
| `material_process` | `unmeasured`; no material/process qualification runs. |
| `physical_performance` | `physical_test_required`; geometric analysis cannot establish actual fit, strength, or service performance. |

The assessment is `failed` if solid topology, finite dimensions, the
support-free orientation criterion, or a measured wall thickness fails. Support warnings remain in `issues`; a failure of the
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

## Check mould draft

Pass `draft` to `validate_mesh` to check whether a master or cavity releases
from a mould. Give the direction the part leaves the mould:

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
judged against its own half, and no patch spans both halves.

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
