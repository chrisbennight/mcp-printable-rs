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

An optional comparison workflow keeps a baseline and a few justified candidates
in separate output directories. It records source identity, profiles, overrides,
orientation, estimates, native warnings, review references and selection reasons.
Calibration coupons are useful when their physical result could change a
consequential choice; preparing them does not authorize printing them.

The worker retains request and resolved profile settings, source/output hashes
and provenance; completed slices may also have a compact summary. The client
saves the additional purpose and decision rationale through existing project
artifact operations. This is a client workflow, not a new server-owned planning
record or optimization engine. Resolved profile files alone do not prove every
effective per-object setting in an imported 3MF.

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
valid values, calibrated performance or hardware support. `slice.settings`
provides category, current inherited value and overrideability, but not a
complete typed definition with units, enums and native defaults.

## Further capabilities to add

These are proposed follow-ups, not advertised server behavior. Prefer extending
the existing workflow's typed actions to adding an unrelated tool for each setting.

| Priority | Capability | Evidence needed before advertising it |
| --- | --- | --- |
| High | Read-only source-model and 3MF preparation inspection | Report source identity, units, object transforms, plates, material indices, effective settings and preservation limitations. Exercise uploaded STL, model-only 3MF, multi-material projects and sliced packages without executing imported scripts or changing the live scene. Reuse the existing package reader where appropriate; it currently supports completed-slice summaries, not a public source inspection action. |
| High | Rich native setting definitions and compatibility diagnostics | Generate setting types, units, allowed values/ranges and defaults from the pinned engine. Preserve profile overrides and native rejection details; check combinations only where authoritative rules exist. A syntactically valid value is not proof of physical suitability. |
| Medium | Retained preparation decisions and candidate comparison | Bind the user's objective, critical faces, assumptions and changes to exact source/settings/output identities. Compare available time/material estimates and unresolved warnings while keeping physical measurements separate. Reuse current slice handles and provenance rather than duplicate job lifecycles. |
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
