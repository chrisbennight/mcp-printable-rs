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
| `wall_thickness` | `unmeasured`; no general wall-thickness solver runs. |
| `dimensional_requirements` | `unmeasured`; no target dimensions or tolerances were supplied to this analysis. |
| `build_envelope` | `unmeasured`; no printer envelope was supplied. |
| `material_process` | `unmeasured`; no material/process qualification runs. |
| `physical_performance` | `physical_test_required`; geometric analysis cannot establish actual fit, strength, or service performance. |

The assessment is `failed` if solid topology, finite dimensions, or the
support-free orientation criterion fails. Support warnings remain in `issues`; a failure of the
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
remains unmeasured. A tilted closed box can pass solid topology while failing
the support-free orientation criterion. Neither result proves suitability for
the intended physical use.

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
