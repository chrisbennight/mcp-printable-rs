# Generic product acceptance corpus

The [agent workflow evaluation plan](agent-workflows.md) extends this corpus
with native observation, state consistency, and context-efficiency outcomes.
It records which evidence exists and which comparisons are still pending.

These OpenSCAD sources exercise the public `product_v1` design contract as
complete product workflows:

- `enclosure.scad` combines a rounded shell, printable bosses, and reinforcing
  ribs.
- `bracket.scad` combines a rounded mounting plate, gussets, vertical fastener
  holes, and a support-free horizontal bore.
- `grip.scad` combines a capsule body, restrained taper, and repeated edge
  radii.
- `hinge.scad` exports `fixed` and `moving` rigid-body variants for conservative
  complete-arc assembly analysis. It is an articulated regression fixture, not
  a hinge-specific production API. Its source asserts the radial, axial, roof,
  and bed-side walls that can be proven constructively from its dimensions.

All dimensions are millimetres and `+Z` is the build direction. Run the sources
through `printable_scad_compile` with an explicit `product_v1`
`design_profile`; the trusted server wrapper supplies the kit and reserved
`pbl_*` profile definitions.

The release smoke compiles every product, checks measured topology, bed contact,
orientation, and overhang evidence, and imports the resulting STLs through the
public Blender tools. It decodes product stills, galleries, turntables, and the
certified mechanical video rather than treating file existence as success.

Kit-local wall invariants are constructive evidence, not a global triangle-mesh
wall certificate. The hinge's moving clearance is accepted only when
`printable_analyze_assembly` certifies the full requested rotation and the
durable mechanical renderer independently reproduces that certificate before
rendering any frame.
