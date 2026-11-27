# Preparing models for FDM printing

The bundled [prepare-fdm-print skill](../skills/prepare-fdm-print/SKILL.md) guides
an MCP client from an uploaded or generated model to a deliberately chosen,
reviewed slice. It reuses the current project, artifact, modeling, geometry and
[native slicing](slicing.md) tools. Loading it does not prepare files or start a
printer, and no modeling or printer backend is needed to read it.

## Agent discovery

The primary delivery path is the callable `skill` tool. The `slice` description
directs profile-selection tasks to `prepare-fdm-print`; the agent calls `skill`
through its MCP client or Gateway and receives the complete, self-contained
instructions. No repository checkout, Markdown file access or central Gateway
skill installation is required. This page records the design and evidence for
maintainers; it is not the runtime instruction source agents must find.

The skill is listed by `skill` with `action: "list"` and loadable with this
tool argument:

```json
{"action":"get","params":{"name":"prepare-fdm-print"}}
```

Resource clients can read the same bytes at
`printable://skills/prepare-fdm-print/SKILL.md`. The metadata includes a SHA-256
digest. Clients without resource support use `skill.get` and the schemas from
`tools/list`; direct MCP use needs no gateway-specific skill installation.

Discovery remains concise. The decision rules live in the skill, while exact
parameter schemas remain in the advertised tool schemas and selected contract
resources. Initialization instructions are a supplementary route: an aggregator
need not put an upstream server's instructions into an agent's context.

MCP resources are application-driven, so resource publication alone does not
establish that an agent will see the content. MCP prompts are user-controlled
and are not the primary route for an automatic uploaded-model workflow. Tool
discovery and a normal read-only tool call provide a model-accessible path, while
resources remain an alternative for hosts that support them. Instructions guide
decisions; required compatibility and safety checks remain server validation.

After deployment, verify the complete path through the Gateway: a slicing search
returns the preparation route in the tool description; the selected skill tool
schema accepts the request above; `list` advertises the skill; and `get` returns
the complete instructions with the deployed digest. Confirm this without local
file access. Direct-server tests do not prove Gateway admission, refreshed
metadata, search ranking or retrieval. A separate behavioral evaluation must
check whether a fresh agent follows the guidance on representative tasks.

## Ordinary uploaded-model workflow

1. Ingest the model into a project and preserve its original identity. Check
   dimensions and units. Preserve a 3MF's arrangement and material assumptions
   rather than silently replacing it with an STL.
2. Establish the intended use and critical faces. Reuse printer/nozzle,
   filament and build-surface choices already supplied; ask about consequential
   missing facts. A request without special requirements starts from a compatible
   standard profile rather than an automatic parameter sweep.
3. Inspect applicable geometry and select orientation for the actual loads,
   fit, surface appearance, supports and bed contact. Retain an explicit
   orientation when preparing the slice.
4. Discover compatible printer, process and filament profiles. Inspect relevant
   inherited settings, then make motivated overrides for the user's objective.
5. Prepare to a new output directory, retain the handle, read native warnings
   and inspect the completed slice's actual toolpaths at relevant layers.
6. Deliver the chosen package, useful previews and a compact preparation record.
   Separate predicted benefits and estimates from measured geometry and physical
   results. Physical dispatch uses the existing printing workflow.

A model with holes, inconsistent facing, self-intersections or floating debris,
such as a scan or a generated object, can be rebuilt as one checked solid with
`scene` `prepare_print` before slicing; see
[prepare a mesh for printing](mesh-assessment.md#prepare-a-mesh-for-printing).
It reports walls thinner than the nozzle and overhangs for the chosen printer.
Leave well-formed CAD exports alone: the rebuild rounds their sharp edges to the
chosen facet size.

The optional `slice.compare` action keeps a baseline and up to eight justified
completed candidates in separate output directories. It binds the objective,
named face requirements, load assumptions and rationale to exact slice and review
provenance. The retained comparison includes source identity, settings,
transform requests, engine identity, native warnings and available estimates.
It verifies historical source/output/review bytes before creating its report;
mutable public state and settings files cannot redefine those identities.
Changed current sources and different material, printer, shell settings or
declared physical conditions remain explicit. Native estimate strings retain
their original units and material-slot order; unsupported support/tower/flushing
breakdowns are null. No winner or numeric strength prediction is generated.
Calibration coupons are useful when their physical result could change a
consequential choice; preparing them does not authorize printing them.

## Orientation goals and plate review

The skill distinguishes minimum total material/support, strength under the
intended loads, and finish of named faces. Orca's
[automatic orientation](https://github.com/OrcaSlicer/OrcaSlicer/wiki/prepare_auto_orient)
uses overhangs, bottom contact, support interface and contour complexity. It does
not know the user's loads or which face is visible. A candidate with less
overhang area can still consume more total filament after supports, priming and
flushing; compare native consumption estimates where available.

A bracket needs both service and screw-installation loads considered. A relief
can trade shallow-slope stepping against support marks and a smaller bed contact.
A casting tray must retain its opening, fitting geometry and suitable base;
changing its geometry to make a tilted print easier requires design scope.
The existing source, modeling/export and slice operations support deliberate
candidate artifacts. The server has no objective-driven orientation optimizer.

For a multicolor plate, inspect generated support, brim and tower footprints as
well as object placement. Native arrangement is a useful candidate, while a
model-only plate preview omits the generated footprints. Use `inspect_layout`
to identify authored 3MF instances, plates, transforms, native material settings
and solid conflicts. Source-bound `prepare.layout` controls change selected
rigid instance transforms or a plate's tower XY while preserving the private
source snapshot. Automatic arrangement and orientation must be disabled when
those controls are present. The final native package is inspected separately;
unknown identity or normalization cannot establish placement preservation.

[Orca's tower documentation](https://github.com/OrcaSlicer/OrcaSlicer/wiki/multimaterial_settings_prime_tower)
distinguishes Type 1 and Type 2 controls. Bambu uses Type 1. The packaged fork's
[shared integration](https://github.com/chrisbennight/OrcaSlicer/blob/6320f7282263ac81d5a133c47e77e08191b409a2/src/libslic3r/GCode.hpp)
also consumes tower rotation; verify its generated paths instead of treating
upstream type labels as complete applicability evidence. Cone stabilization
and rotation are not universal stability fixes. Tower bed attachment, structural
bonding and toolhead contact are separate concerns. A wider brim need not solve
the latter two. Priming extrusion and color flushing also serve different
purposes, so flushing savings alone do not justify removing a tower.

[Extruder clearance](https://github.com/OrcaSlicer/OrcaSlicer/wiki/printer_basic_information_extruder_clearance)
and print order matter for sequential printing and descent to a tower that skips
sparse layers. Collision identification belongs to the preparing agent and
backend service. The agent must use available geometry, toolpaths, printer
envelopes and native diagnostics to flag collisions and correct or reject an
unsafe recommendation. An older slicer leaving clearance to its operator does
not transfer that analysis to Printable's user.

The current XY review is not a swept-toolhead check. When a required check is
unavailable, the agent retains the files, states the missing analysis and
withholds a print-ready recommendation. The backend now retains
[collision findings](slicing.md#collision-findings) from final native toolpaths:
bed exclusions, tower interference and explicit linear travel checked against
supplied machine-component envelopes. Missing inputs and unsupported motion
remain unresolved. Source-object solid intersections are a separate check in
the isolated geometry worker. Final deposition footprints include known line
width, height and end margins, grouped by native object label, material and role.
Distances between their conservative XY bounds are lower bounds; overlapping
bounds do not prove contact. Complete machine simulation remains unavailable.
Physical bed readiness remains a separate question. Motion advice also depends
on whether the bed moves in XY; see
[Orca's arrangement guidance](https://github.com/OrcaSlicer/OrcaSlicer/wiki/prepare_auto_arrange).

Public reports of a
[tower breaking above an attached brim](https://www.reddit.com/r/FlashForge/comments/1vdoq9e/prime_tower_being_knocked_over/)
and [failure despite strong bed attachment](https://www.reddit.com/r/BambuLab/comments/1gfp10k/prime_tower_getting_knocked_off_and_camera_is_too/)
motivate distinct diagnostic branches. They are anecdotes, not verified causes
or universal remedies. The guidance does not claim physical validation.

Upstream documentation changes independently of the packaged engine. Use the
shipped 2.5.1 setting catalog for exposed names and verify native behavior before
advertising a control or a clearance check. Printable's collision evidence
covers the checks described above. The upstream desktop
[collision-volume features](https://github.com/OrcaSlicer/OrcaSlicer/wiki/release_2_5_0_beta)
have no verified equivalent in Printable's headless workflow.

The worker retains request and resolved profile settings, source/output hashes
and provenance; completed slices may also have a compact summary. Use
`slice.compare` to retain the objective, critical faces, load assumptions and
candidate rationale alongside verified slice and review identities. Ordinary
project artifacts can retain additional calibration observations and design
context. Resolved profile files alone do not prove every effective per-object
setting in an imported 3MF.

## Decisions informed by the videos

The following are decision criteria, not measured performance claims for
Printable or universal defaults. Older tests used different machines, materials
and slicers; use current setting discovery to translate their lessons.

| Evidence | Guidance carried into preparation |
| --- | --- |
| [Switch and Click: surface finish](https://www.youtube.com/watch?v=UtTNvKvxROs) | Consider orientation, layer height, seams, ironing, texture and support contact before manual finishing. Use calibration rather than adopting 0.08 mm layers or a fixed extra support gap for every print. |
| [CNC Kitchen: orientation](https://www.youtube.com/watch?v=7aJ4dDyyf20) | Consider service and installation loads. A 45-degree tilt helped some specimens but did not improve every part; it also changed supports and failure modes. |
| [Layer-height tests](https://www.youtube.com/watch?v=fbSQvJJjw2Q) | Very thin layers did not automatically improve strength in the tested setup. Keep physical shell thickness comparable when changing layer height. |
| [Extrusion-width tests](https://www.youtube.com/watch?v=9YaJ0wSKKHA) | Wider lines can improve some strength/time outcomes but can erase fine geometry. Distinguish line width from flow multiplier and respect melt-flow capability. |
| [Temperature tests](https://www.youtube.com/watch?v=M3bwJx8ZkMA) | The temperature with the best appearance was not necessarily the strongest; hotter was not indefinitely better. Material conditioning and actual mass/dimensions matter. |
| [Pressure-advance experiment](https://www.youtube.com/watch?v=sHLGrTBLxLg) | Calibration depends on the material and setup. A sensor-based proof of concept is not a capability provided by setting a profile value. |
| [Brick-layer tests](https://www.youtube.com/watch?v=dDgA51zdfLc) | Increased flow also improved ordinary layers. Compare against that baseline and account for added mass and dimensional changes before attributing gains to staggered perimeters. |
| [Physical textures](https://www.youtube.com/watch?v=rTBkjR7JvzI), [follow-up](https://www.youtube.com/watch?v=Y_vkCFwDIBY) | Real mesh displacement changes geometry. Protect fitting faces and bed contact, control physical scale and amplitude, and validate the export. Use the updated workflow's mesh-quality and reproducibility lessons. |
| [Micro non-planar finishing](https://www.youtube.com/watch?v=0Hi9lBjZGIM) | Promising surface measurements do not resolve the demonstrated nozzle-interference and path-order problems. This is not ordinary ironing or an adaptive layer setting. |
| [Transparent-printing tests](https://www.youtube.com/watch?v=9qb25Gi4Jv0) | Flow, dryness, speed and cooling interact. Nominal solid infill and optical clarity do not certify leak resistance or mechanical performance. |
| [Beginner guidance](https://www.youtube.com/watch?v=B84CgaPdixc) | Preserve a working baseline and change settings for a reason. Material variants matter: matte appearance can conceal layers while sacrificing adhesion. |

Both reviews used extracted English transcript text; the Switch and Click review
also initially used automatic Arabic captions. Neither review verified the audio,
measured video imagery or performed physical tests. The English extraction itself
has inconsistent decimal transcription, reinforcing the need to check native
behavior rather than copy numerical recipes. The skill retains decision criteria
instead of treating transcription as a calibrated profile.

The current catalog exposes relevant process and filament overrides: wall and
shell thickness controls, feature-specific line widths, infill, ironing, fuzzy
skin, seam/scarf controls, support interfaces, temperatures, cooling, material
flow, pressure advance and volumetric-speed limits. Presence does not establish
calibrated performance or hardware support. `slice.settings` reports available
native definitions, their pinned source identity, profile value origins and
explicit unknown fields. Literal definitions support early range/choice checks;
dynamic combinations and effective output still require native validation.

## Failure diagnosis and controlled comparisons

The skill includes a tower decision sequence that distinguishes detachment,
breakage above an attached brim, intermaterial splitting, buildup and suspected
contact. Camera guidance retains the visible failure location and relevant
tower-entry, transition and late-print context without inferring hidden motion
or changing control authority. The Reddit reports cited above motivate these
branches; they do not establish the cause of a particular failure.

Named-face comparisons cover bed texture, horizontal top fill/ironing, vertical
wall order/seams and shallow lettering. A cover example preserves its fitting
rim and compares native time/material estimates and actual paths before making
a cosmetic choice. The bracket example compares load-aware orientation, shells
and core infill while preserving physical shell thickness and considering screw
installation. Neither example assigns a strength rating or claims a measured
surface result.

Calibration observations can be imported and retained through ordinary project
artifacts. Records identify source/profile/settings references, machine/nozzle,
filament variant/condition, test conditions, observed results and whether each
value is measured, estimated or inherited. Reuse is conditional on those facts;
a compatible standard baseline does not require a new calibration campaign.
The [current calibration guide](https://github.com/OrcaSlicer/OrcaSlicer/wiki/calibration_guide)
provides dependency guidance, while automatic firmware calibration and packaged
CLI support remain separate questions.

Troubleshooting keeps the original slice and failed or inconclusive trials.
Reports separate observation, hypothesis, intervention and result. Conflicting
[Grid-infill](https://www.reddit.com/r/BambuLab/comments/1q3any0/psa_for_new_users_avoid_using_the_stock_infill/)
and [retraction](https://www.reddit.com/r/BambuLab/comments/1cj0en2/psa_grid_infill_is_not_the_problem/)
reports motivate considering crossing paths, extrusion buildup and travel
behavior rather than prescribing a universal infill pattern or Z-hop value.
Support choices likewise compare contact, removal access, stability and cost.

## Further capabilities to add

These are proposed follow-ups, not advertised server behavior. Prefer extending
the existing workflow's typed actions to adding an unrelated tool for each setting.

| Priority | Capability | Evidence needed before advertising it |
| --- | --- | --- |
| High | Additional verified machine and coordinate-frame coverage | Extend the established source-solid and linear-path checks only with authoritative frame, firmware and hardware evidence. Preserve positive findings and explicit unresolved scope; source-model bounds and a native success code cannot establish every machine's clearance. |
| High | Broader combination diagnostics | Extend existing literal definitions and tower-generator applicability when authoritative rules are available. Preserve native rejection details. A syntactically valid value is not proof of physical suitability or compatibility across every material/hardware combination. |
| Medium | Native adaptive profiles and local modifiers | Author and inspect per-object layer profiles, seam/texture masks and support regions. Prove preservation through upload, reslicing and export with actual toolpath regressions. Printer minimum/maximum layer settings alone do not provide this. |
| Optional | Reusable physical-texture workflow or convenience action | Reuse Blender's geometry capability, retain scale/amplitude/masks and editable source, and verify output topology, dimensions and printability. Do not require a new texture service solely to copy a browser workflow. |
| Experimental | Brick layers, micro non-planar slicing and sensor calibration | Require a compatible native implementation, reproducible baseline comparisons and hardware-specific evidence. Verify path order, nozzle clearance and extrusion behavior where relevant. Do not reinterpret inner-seam staggering or enable arbitrary post-process scripts as a substitute. |

## MCP design basis

The API review consulted Grounded Docs' `mcp-api-design` library at
`0.0.0+published.20260906`, including
[Block's workflow-first guidance](https://engineering.block.xyz/blog/blocks-playbook-for-designing-mcp-servers),
[tool-description guidance](https://www.anthropic.com/engineering/writing-tools-for-agents)
and the MCP specifications for
[tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools),
[resources](https://modelcontextprotocol.io/specification/2026-07-28/server/resources)
and [prompts](https://modelcontextprotocol.io/specification/2026-07-28/server/prompts).
The resulting change keeps initialization and discovery small, loads detailed
guidance on demand, reuses typed public actions and structured results, and keeps
artifact bytes outside instructions. The read-only skill loader remains distinct
from mutating preparation and physical dispatch; whole-tool annotations retain
their existing meanings.

Protocol regression tests check discovery, exact retrieval, digest integrity,
resource agreement and output schemas with unavailable modeling/printer backends.
That verifies delivery of guidance, not an agent's compliance, an optimal profile
or physical print quality. Behavioral evaluation should use representative
uploaded models and explicit objectives before claiming better outcomes.
