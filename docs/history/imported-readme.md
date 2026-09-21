# Historical documentation from the source import

This is the imported project description, retained for design history. It mixes
delivered behavior with private deployment assumptions. Use the current README
and linked guides for installation and supported behavior.

# mcp-printable-rs

Production Printable MCP service for AI-driven 3D modeling, rendering,
animation, and FDM print validation with Blender and OpenSCAD.
Streamable HTTP at `/mcp`.

Development now continues on [GitHub](https://github.com/chrisbennight/mcp-printable-rs).
The repository is initially private. See [continuous integration](../../docs/continuous-integration.md)
for the GitHub checks and the remaining release-porting boundary. The supported
deployment target is Linux/amd64 with NVIDIA; software graphics in CI is test
coverage, not a separate supported distribution.

**Status: active build.** The Rust MCP foundation, confined workspace,
Blender client, first-party UI/background bridge, pinned Blender image,
geometry/SCAD primitives, sanitized dependency readiness, digest-paired
release pipeline, exact-image security policy, and exact-artifact production
NVIDIA GPU gate are in place. Typed scene inspection, primitive and
boolean modeling, rigid pivot-axis animation authoring, checkpoint/restore,
STL import/export, and `.blend` save are
available through MCP alongside bounded explicit Blender Python execution and
caller-budgeted EEVEE/Cycles preview rendering, preset view galleries,
dimensioned orthographic views, non-destructive cross-sections, printability
heatmaps, turntable contact sheets, before/after image comparison, and
actionable STL solid/print validation plus assembly clearance, interference,
and directional-retention analysis. Confined OpenSCAD source can compile to an
immediately validated STL, render a named-view PNG, or export a Z-plane SVG
cross-section. Durable still, turntable, timeline-animation, and certified
mechanical-rotation jobs retain restart-recoverable frame sequences and
artifact-backed MP4 output. The shipped acceptance corpus composes the public
generic contracts into an enclosure, bracket, grip, and articulated fixture.
Any supported workspace artifact can be published as an immutable structured
file reference; Printable streams its raw bytes directly to the gateway under a
short-lived one-use authorization, without placing artifact bytes in base64 MCP
content.
`/mcp` requires the single shared gateway bearer configured through
`PRINTABLE_MCP_BEARER`; Printable validates it directly without a reverse
proxy sidecar.
`/healthz` is process liveness; `/readyz` distinguishes ready, healthy busy
work, and blocked product dependencies without exposing backend configuration.
The capability roadmap lives in [`PLAN.md`](../../PLAN.md);
repository conventions in [`AGENTS.md`](../../AGENTS.md).

The [product refactor](../../docs/product-refactor.md) records the complete delivery
intent and acceptance criteria. The [workflow interface](../../docs/workflow-interface.md)
lists all thirteen public tools and their typed actions. Public calls use these
unprefixed names; prior operation names are internal implementation details.
Deployment and gateway discovery are updated together, with live verification
tracked separately from implementation and merge.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/printable-server` | MCP server plus disposable exact-geometry worker |
| `crates/printable-blender` | TCP client for the Blender addon bridge |
| `crates/printable-geom` | Pure mesh geometry + printability analysis (parry3d + Manifold) |
| `crates/printable-scad` | OpenSCAD subprocess backend + confined-source gate |
| `crates/printable-workspace` | Confined workspace artifact I/O |
| `crates/printable-imaging` | Render compositing |
| `addon` | Blender add-on and persistent UI/background main-thread bridge |
| `blender` | Checksum-pinned Blender 5.2.0 UI/background image |
| `acceptance` | Reusable public-contract product sources and workflow guidance |

## Modeling guidance

Read the MCP resource `printable://modeling/blender-v1` for editable Python
modeling, compact structured observations, visual review, and artifact delivery.
`inspect` with `action: "scene"` supports name, type, and direct collection filters and can
omit transforms. `inspect` with `action: "object"` offers paginated materials, modifiers,
and hierarchy sections. `inspect` with `action: "node_tree"` pages through authored
material or Geometry Nodes topology without recursively dumping node groups.
See the [workflow guide](../../crates/printable-server/resources/blender-modeling-v1.md)
for cursor semantics and runnable examples.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
PYTHONPATH=addon python3 -m unittest discover -s addon/tests -v
python3 scripts/docgate
```

The [scene state contract](../../docs/scene-state.md) describes optional preconditions,
stale-edit rejection, and state metadata for modeling and rendering operations.
[Editing context](../../docs/editor-context.md) provides bounded editor/selection
inspection and explicit UI targets for Python.

[Native observations](../../docs/native-observation.md) provide viewport and editor
capture with bounded images, state metadata, and retained observation history.

The bridge starts Blender with factory settings, offline mode, disabled blend
file auto-execution, and no audio:

```sh
BLENDER_BIN=/path/to/blender scripts/run-headless-blender
```

`PRINTABLE_BLENDER_MODE=ui` starts normal Blender on a private display and
serves bridge commands through a persistent application timer. Each tick handles
at most one queued command, allowing Blender to redraw between commands. Idle
event-loop stalls make the bridge unhealthy. Command execution remains serialized,
and a long synchronous command still occupies the UI thread until it completes.
The default remains `background` until the private GPU display deployment is
verified. The [private display guide](../../docs/private-blender-display.md) describes
software and GPU backends, lifecycle, and integration checks. No remote desktop
is exposed.

The local launcher uses repository-local, gitignored state and workspace
directories by default, and it can be run from any working directory. It
defaults to `127.0.0.1:9876`. Container deployments set
`PRINTABLE_BLENDER_BIND=0.0.0.0` only on the private control network. Files are
accepted only as paths relative to `PRINTABLE_BLENDER_WORKSPACE_ROOT`; the
bridge refuses symlinked components, snapshots inputs into private staging,
and atomically promotes staged outputs without passing caller-controlled paths
to Blender.

`render` with `action: "scene"` renders the active camera to a confined PNG artifact;
when the scene has no camera or light, Blender creates a default isometric setup.
It defaults to a 512×512 EEVEE preview, supports CYCLES with a caller-selected
sample count, and gives the render a one-hour work budget by default. That
default is not a product maximum: callers may select any positive duration the
runtime can represent. The artifact is always preserved. When requested and no
larger than 1 MiB, the server also returns the decoded PNG as MCP image content;
larger results stay on the workspace with explicit metadata instead of
inflating a synchronous response.

`render` with `action: "product"` creates a repeatable product image from selected
evaluated geometry without altering the source scene. It includes collection
instances, frames complete world-space bounds with a fixed 15% margin, and
offers `engineering`, `studio_neutral`, and `studio_dark` profiles. Existing
effective `DATA`- and `OBJECT`-linked materials are preserved; empty material
slots receive a neutral fallback and typed explicit overrides win. Mixed
real/empty slots report both preservation and fallback honestly. The response
records the effective camera,
lights, color management, ground, material decisions, shading, and evaluated
geometry totals. Grounded studio cameras stay above the ground plane. A
16,777,216-pixel limit and matching 64 MiB render/verification budget keep
high-resolution output behavior predictable, while topology and instance
preflight rejects scenes that would multiply copied meshes beyond the
presentation budget. Smooth-by-angle affects copied presentation geometry
only, and the renderer never adds a bevel modifier. The PNG is promoted only
after cleanup and source-state verification. Blender application handlers are
suspended for the complete presentation operation and restored before
promotion, so live-scene callbacks cannot mutate source product data. See
`printable://render/product-v1` for the complete contract.

Without presentation, `render` with `action: "gallery"` renders any unique selection
of front, right, back, left, top, bottom, and isometric orthographic views, and
`render` with `action: "turntable"` renders 3–36 evenly spaced orthographic orbit
views at a caller-selected elevation. Each tool optionally accepts the same
`engineering`, `studio_neutral`, or
`studio_dark` presentation used by `render` with `action: "product"`; omitting it
retains the existing orthographic review output. Each tool runs its complete
set as one serialized, caller-budgeted Blender request, automatically frames
render-enabled geometry in the active view layer, restores the scene camera,
lights, and render settings, and commits no view until every render succeeds.
Presented batches additionally isolate application handlers, materials,
shading, lights, world, and ground inside disposable scenes. Individual PNGs
are retained under a unique
`visual/renders/<batch>/` directory and a
labeled contact sheet is written to the requested path. Contact sheets up to
1 MiB can be returned inline. If the requested source set would make the
contact sheet exceed the generated-artifact memory budget, only the sheet is
fitted down; every individual view remains at the requested resolution. Each
multi-view RGB8 source is capped at 8,388,608 pixels so it can always pass the
confined snapshot and decoder path; use `render` with `action: "scene"` for a larger
single image. Composition immediately resizes each decoded source to its fitted
tile and runs through one process-wide visual-memory lane, preventing concurrent
valid calls from multiplying full-resolution decode memory.

`view` with `action: "dimensions"` measures evaluated world-space scene bounds,
including collection instances, and returns exact bounds metadata with labeled
front, right, and top views. `view` with `action: "section"` creates a
temporary evaluated-geometry cutaway on a selected world axis and caps the cut
surface for inspection, including nested contours in hollow parts.
`view` with `action: "overhangs"` colors faces
green, amber, or red from their downward overhang angle relative to a selected
build direction and returns exact face and area totals for each category.
Cross-sections and heatmaps do not modify source objects. They preserve a
full-resolution source PNG under `visual/diagnostics/`, persist a labeled PNG
at the requested path, and use the same optional 1 MiB inline-image contract.
The add-on preflights evaluated mesh counts before allocating diagnostic
materials or BMeshes. The copied-diagnostic limit covers 1,000,000 vertices,
3,000,000 edges, 2,000,000 faces, 6,000,000 face loops, and 16,000,000 bounded
attribute/weight values. Callers can hide unrelated objects or pass an explicit
object subset instead of losing the workflow.
Cross-sections recheck the exact expanded vertex, edge, face, and loop totals
after bisecting and capping, before appending combined buffers. Direction vectors
are normalized with overflow-safe magnitude checks before scene mutation.
Explicit-subset discovery retains only the at-most-1,000 requested names.
Selected instances are processed through one transient BMesh at a time and
assembled into one bounded diagnostic Mesh/Object pair, so instance count does
not multiply retained Blender datablocks.
Diagnostics require mesh objects so the preflight never has to allocate an
unbounded conversion merely to discover its size; convert non-mesh geometry,
hide it, or select only mesh objects.
The independent Blender supervisor budget begins before object discovery,
evaluated-mesh preflight, and bounds measurement, so those scans cannot outlive
the caller budget and retain the serialized Blender lane.
Server validation requires heatmap bounds to match their source geometry and
cross-section bounds to stay inside the source on the retained side of the cut.

`edit` with `action: "rigid_rotation"` authors a mechanical rotation without
requiring caller-written Blender parenting code. It creates a named Empty at a
world-space pivot, normalizes the right-hand-rule axis, and inserts linear
axis-angle keyframes for a positive angular travel. One to 1000 named objects
can move as a rigid group. Every target must have no existing parent, object
children, animation data, constraints, rigid-body simulation, or rigid-body
constraint; competing target motion, duplicate names, missing objects, invalid
frame ranges, and a pivot controller name collision fail before scene mutation.
Parenting preserves each target's exact world matrix by deriving the parent
inverse from the controller's world matrix, avoiding the extra pivot translation
caused by an identity parent inverse. A failed keyframe insertion rolls the
hierarchy and transforms back.
This tool authors motion; it does not claim clearance. Export and analyze the
same fixed/moving geometry with `analyze_assembly` before treating the
animation as mechanically valid.

`validate_mesh` reads a confined ASCII or binary STL without changing
it and interprets STL coordinates as millimetres. It reports bounds, raw surface
area and signed volume, watertight boundary edges, winding, duplicate and
degenerate triangles, connected components, Manifold solid validity, volume,
center of mass, and optional material mass. Build-direction analysis separates
actual build-plate contact from unsupported downward faces, so a part's bottom
surface is not misreported as an overhang. Errors include repair guidance;
multiple disconnected solids and support-requiring overhangs are warnings
rather than false geometry failures. Cap-sized analyses share one process-wide
geometry lane so concurrent sessions queue instead of multiplying STL, BVH,
topology, and Manifold memory. The production image smoke uploads and validates
a real cube through MCP, exercising the packaged native geometry path.

`analyze_assembly` snapshots two confined watertight STL artifacts in
the same millimetre coordinate system. It reports nearest surface gap separately
from Manifold intersection volume, so containment is never mistaken for usable
clearance, and evaluates an optional caller-selected design clearance without
inventing a printer/material default. Optional rigid motion supports both a
normalized translation and a rotation around a world-space pivot and
right-hand-rule axis. Translation uses continuous Parry shape casting. Rotation
uses Parry proximity queries inside a conservative adaptive clearance
certificate over the complete arc; a path is never reported clear from sampled
animation frames. If the bounded certificate cannot prove the
requested envelope, the report fails closed with `clearance_not_certified`.
A positive clearance limit is distinguished from physical contact and does not
claim directional retention; `retained` is true only for initial interference
or zero-clearance contact. Motion starting exactly at its requested boundary is
conservatively blocked at zero. Initial interference is reported at zero because
rigid meshes cannot model press-fit deformation, compliance, friction, or
insertion force. Pair analysis uses the same serialized geometry lane as
single-mesh validation. Exact CSG runs in a disposable worker with a configurable
address-space budget, so generated boolean topology can fail visibly without
exhausting or terminating the MCP server; no work timeout is imposed. The
production image smoke exercises both rigid-motion paths through this isolated
MCP boundary.

`scad_build` with `action: "mesh"`, `scad_build` with `action: "image"`, and
`scad_build` with `action: "section"` execute inline OpenSCAD source without shell
interpolation. Literal `import()` and `surface()` workspace paths are copied to
private immutable snapshots before the process starts; dynamic paths and other
file-loading directives are rejected. All three tools accept the same optional
typed `defines` object plus a `variant` exposed as `pbl_variant`. Boolean,
finite-number, bounded-string, and bounded numeric-vector values are serialized
once into deterministic `-D` argv pairs; names beginning with `pbl_` are
reserved. Server-generated definition metadata reports only the sorted names
and count, never their values; ordinary OpenSCAD compiler diagnostics remain
caller-visible. A cross-section using definitions, a variant, or a design
profile compiles the fully defined model to one bounded intermediate STL before
projecting it, so top-level source defaults have the same OpenSCAD `-D`
override behavior in every parameterized workflow; both subprocesses share one
caller budget and concurrency permit. Calls without the optional
parameterization retain their original direct-projection path. Compile returns
the same actionable solid/print report as `validate_mesh` before
atomically committing the STL. Render accepts the seven named cameras, preview
or full mode, persists the PNG, and follows the same optional 1 MiB inline-image
contract as Blender renders. Cross-section projects the model at a
caller-selected Z plane and persists UTF-8 SVG. Calls queue behind
`PRINTABLE_SCAD_CONCURRENCY`; one permit covers snapshot staging, subprocess
execution, output validation, and atomic commit.
Source is capped at 1 MiB and generated artifacts at the workspace's 25 MiB
cap; the child inherits that per-file ceiling so oversized output cannot fill
temporary storage before rejection. Standard output and error are drained with
bounded diagnostics inside the same work budget as process execution. PNGs
must completely decode and SVGs must be complete well-formed documents before
publication. Work defaults to one hour but has no configured maximum; callers
may select any positive runtime-representable duration.

Passing a complete `design_profile` with `"kit": "product_v1"` gives all three
OpenSCAD workflows the same bundled, product-agnostic FDM vocabulary. It
includes rounded panels and shells, capsules, controlled tapered transitions,
ribs, bosses, bounded linear patterns, and support-free X/Y teardrop bore
cutters. Inputs are explicit millimetres with `+Z` as the build direction; the
profile supplies nozzle and layer scale, minimum local wall, moving-clearance
intent, overhang policy, and a reusable hierarchy of form radii. The trusted
kit wrapper is staged only after caller source has passed the normal confinement
gate, so caller `include` and `use` remain forbidden. Caller source can use
public kit modules and profile values but cannot declare modules or functions
in the public `pbl_*` or internal `_pbl_*` namespaces, so it cannot replace the
kit's constructive guards after the trusted include. Curve tessellation
derives from feature and nozzle size and remains bounded. See the discoverable
`printable://design/product-v1` resource for module signatures, enclosure,
bracket, and grip guidance, and aesthetic principles that apply across
hard-surface FDM products.

The compile response keeps manufacturing evidence scoped to what was actually
established. Final topology, bounds, bed contact, and overhangs are measured
using the requested profile. Kit-local shell, rib, boss, and radius invariants
are enforced when those modules are used. Arbitrary final-mesh minimum wall
remains `not_certified`, and moving clearance remains `not_run` until separately
exported rigid bodies pass `analyze_assembly`; a profile declaration
never substitutes for either proof.

### Reference product workflows

[`acceptance/products`](../../acceptance/products) contains complete OpenSCAD sources
for a rounded enclosure, a mounting bracket with a support-free horizontal
bore, a capsule/taper grip, and a two-body articulated fixture. They use only
the public `product_v1` modules and ordinary OpenSCAD composition. The hinge is
not a special production API: its `fixed` and `moving` variants demonstrate the
same companion-body workflow any moving product uses.

The external release smoke compiles all four products through MCP. It requires
measured watertight/manifold/oriented geometry, bed contact, and compliance
with the explicit overhang profile while retaining honest `not_certified`
global-wall and `not_run` compile-time clearance evidence. It then imports the
actual STLs into Blender, decodes engineering and studio PNGs, requires named
profiles to be visibly distinct without pixel-exact comparisons, renders a
bracket gallery and grip turntable, and verifies that presentation leaves each
source scene unchanged. The articulated workflow must pass complete-arc
analysis before its durable job starts; every video frame and the final MP4 are
fully decoded, with the stored certificate still attached.

`compare_renders` decodes two existing confined PNG artifacts and
creates a labeled BEFORE/AFTER composite without consuming Blender. Composite
surfaces are capped at 8,388,608 pixels. That keeps the raw RGB canvas plus PNG
encoder overhead inside the workspace's 25 MiB generated-artifact commit cap
while bounding decoded tiles and canvas memory. Callers needing larger
inspection images keep the individual view artifacts or render a
high-resolution single preview. Each input decode is separately capped at
16,777,216 pixels and 64 MiB of decoder allocation, so a small compressed file
cannot expand without limit.

Durable renders start from a streamed, immutable confined `.blend` checkpoint
so queued and resumed work never depends on mutable live-scene state. The
caller-selected snapshot budget supports Blender's existing 1 GiB staging
boundary without loading large scenes into MCP memory. `job` with `action: "submit"`
accepts still, orbiting turntable, Blender timeline animation, and
`mechanical_rotation` jobs. Any kind can persist the same optional product
presentation as still and gallery rendering. Presented stills and turntables
measure the restored checkpoint's current evaluated bounds once, persist that
envelope before frame one, and reuse it for every view and restart replay.
Presented timeline animations preserve the authored camera by default while
using stable first-frame bounds for lighting and ground;
`auto_frame_sequence` instead evaluates and frames the union of every requested
frame under its own positive caller budget. That union is persisted before
frame one and replayed after restart. A preserved
camera must remain above an enabled studio ground plane at each frame, avoiding
successful but ground-occluded output. Each presented frame is snapshotted,
digest-checked, and completely decoded at its requested dimensions before
durable progress advances. A mechanical job names every scene mesh as fixed or
moving, supplies a world-space pivot/axis/angle and clearance threshold, and is
admitted only for a strictly rigid scene with no hierarchy, competing
animation, constraints, rigid bodies, modifiers, shape keys, instancing, or
mechanical parts hidden directly or lacking a stable direct-render path through
the enabled active view layer. Excluded, holdout, indirect-only, hidden, or
animated collection paths do not qualify. Cameras and lights are the only
accepted non-mesh scene objects. While holding the serialized Blender
transaction, the worker
loads the immutable job checkpoint, exports the complete fixed and moving
groups in shared world coordinates, authors the owned linear rotation, and
passes immutable STL snapshot paths through the process-wide geometry lane to
the memory-limited continuous-clearance worker. It durably stores the STL
metadata plus complete report. The bridge stamps every response with a
per-process identity; preparation and every rendered frame must retain that
identity even though each transaction exchange uses a fresh TCP connection.
The server validates all nested part, static, witness, interval, endpoint, and
rotation-report invariants. A missing, contradictory, blocked, or
bounded-work-exhausted certificate, including a start at or below the requested
clearance, fails before frame one; only that same loaded, certified scene is
rendered. Mechanical presentation uses a conservative complete-rotation
envelope derived from the certified fixed/moving geometry, never a sampled
presentation timeline, and never adds an uncertified mesh ground plane.

After
logical admission, one request-independent staging task owns the bounded source
snapshot and the job, so a client disconnect cannot leave an untracked copy,
strand an unqueued record, or consume queue capacity. The task retains the
admission fence through the blocking snapshot until it commits metadata plus
the immutable source and hands the job to the worker, or records a terminal
outcome and releases admission. A single
process-wide worker owns Blender's serialized lane for a job, persists each PNG
frame under `.printable/jobs/<job-id>/frames/`, and atomically records completed
progress before advancing. This namespace is discoverable, and transferable
non-video artifacts are readable, but it is reserved from ordinary workspace
uploads and typed Blender/OpenSCAD output commands; only the server's internal
job capability can mutate it.
With `PRINTABLE_RENDER_WORKER_HOST` configured, a separate background Blender
worker renders the checkpoint while live modeling remains available. Jobs
verify the worker role before loading source and never checkpoint or restore
the live scene. See [render-worker isolation](../../docs/render-worker.md) for the
execution, dependency, and migration boundaries.

The following session-restoration behavior applies only to the legacy runtime
without a configured worker. Before loading the job source, it durably checkpoints the live Blender
session. It restores that session before releasing the serialized lane on
success, cancellation, or failure. The checkpoint and restoration state are
part of durable job metadata, so restart recovery completes an outstanding
restore before declaring a cancelled job terminal. Once restoration is
recorded, it is a durable phase boundary: recovery may repeat video encoding,
but never reopens the job source or replaces later live-session changes.
From checkpoint capture until that boundary is persisted, an endpoint-wide
recovery fence rejects ordinary Blender mutations. The fence is reconstructed
from job metadata before restart recovery is scheduled, eliminating the
external-restore/metadata-commit crash race instead of assuming commit ordering
can make two processes atomic. After Blender confirms restoration, the worker
releases its lane but keeps the endpoint fenced and retries only the restored
metadata marker until its atomic write is durable. A server crash during that
window replays the idempotent restoration from the retained checkpoint before
ordinary Blender access can resume. A restore failure remains a visible
nonterminal job condition and retries without an attempt ceiling. The completed
Blender outcome is persisted separately, so recovery retries only session
restoration and cannot rerender a deterministic failure or hide it behind later
success. A recovered job also retries its running-state metadata commit behind
the fence before contacting Blender. Restart requeues every
captured-but-unrestored record until the live
session is recovered. Invalid or unreadable recovery metadata blocks both new
job admission and recovered job scheduling, fails the Blender mutation surface
closed in that legacy runtime, and is reported through
`status.render_jobs.recovery_integrity`.
Atomic job metadata writes retain their process-wide serialization guard inside
the blocking filesystem operation, so cancellation of an awaiting MCP request
cannot let a stale write finish after newer recovery state.

`job` with `action: "get"`,
`job` with `action: "list"`, and `job` with `action: "artifacts"` expose queued,
running, succeeded, failed, and cancelled state, the current source frame,
restart count, partial frame sequences, and final MP4. A server restart resumes
at the first frame not recorded complete; a frame committed immediately before
a crash may be rendered again and atomically replaced. Cancellation is
immediate for ordinary queued jobs and cooperative after the active Blender
frame. A queued recovery that still owes live-session restoration records the
request but remains scheduled until restoration is durable, then becomes
terminal. Cancellation also terminates and reaps an active FFmpeg encoder.

Frame work time, aggregate PNG storage, encoded MP4 size, and encoding time are
positive caller-selected budgets with functional defaults, not product
maximums. The encoding budget is one deadline shared by FFmpeg encode and full
decode validation. Timeline ranges use Blender's complete supported frame
interval; turntable counts use the request type's finite range. Videos remain
workspace artifacts and are never base64 encoded at any size; list and
job-artifact tools keep their paths discoverable, and
`artifact` with `action: "publish"` hands an immutable snapshot to the gateway as a
raw byte stream. Job metadata is retained for the newest 1,000
jobs; older frame/video files remain confined workspace
artifacts and stay discoverable. CPU `libx264` encoding is the baseline, and
readiness performs a real one-frame encode rather than accepting an FFmpeg
binary that lacks the packaged encoder. Before publication, FFmpeg decodes the
complete generated video and its reported frame count and duration must match
the requested sequence. Atomic artifact commits synchronize both file entries
and every newly created ancestor-directory entry before reporting success. GPU
encoding is deferred until production
measurement shows it materially improves throughput.

`blender_execute` runs deliberate Python on Blender's main thread in
the isolated Blender container. The caller selects a positive execution budget;
the ordinary request budget remains available for queueing and transport, and
there is no configured maximum beyond the runtime's representable duration.
Captured output and JSON results are bounded so one response cannot exhaust the
process; set `result` in the script to return structured data. Before execution,
Blender arms an absolute deadline in its separate supervisor process. The
deadline therefore survives trace changes, GIL starvation, caught exceptions,
and stalled native calls. A timeout restarts Blender; checkpoint first because
unsaved scene state is lost on recovery. This is not a language sandbox, so
production relies on the container having no credentials, runtime socket,
privileged mode, broad host mount, or public network path.

CI and production target Linux/amd64 `server`.

## License

MIT — see [`LICENSE`](../../LICENSE).
