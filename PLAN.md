# Printable production delivery plan

> Historical record: this file describes earlier capability delivery or the
> private lab deployment. Use [the current project guide](README.md) for
> independent installation and supported behavior. Current work is tracked in
> GitHub issues.

## Objective

The next approved product cycle is the [agent-driven interface and native
Blender feedback refactor](docs/product-refactor.md), tracked by
[epic #93](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/93).
Its target architecture replaces the background-only live runtime with one
authoritative GUI-backed Blender scene and immutable-checkpoint render workers.
The delivery history below describes the existing foundation; target behavior
is not a claim that the refactor is already deployed.

Deliver a fast, reliable, secure 3D-modeling and rendering MCP service backed
by persistent headless Blender on `server`. Product quality and useful
workflows define correctness.

Python remains only where Blender requires it: the in-process add-on and
headless launcher that call `bpy`. Those files become first-party source in
this repository. Server logic, workspace handling, geometry, scheduling, and
the MCP surface remain Rust.

## Product principles

- Test user-visible behavior and security invariants directly.
- Preserve existing names or wire shapes only when doing so saves migration
  work or protects an active workflow. There are no current consumers that
  require byte-for-byte compatibility.
- Prefer bounded queues, explicit status, and visible resource errors over
  hidden retries or uncontrolled concurrency.
- Measure performance on representative scenes; optimize demonstrated
  bottlenecks rather than incidental implementation details.
- A protective limit must name the resource or recovery path it protects and
  preserve a functional path through streaming, pagination, artifacts, or
  durable jobs. Convenience is not a reason to reject useful work.
- Keep Blender's authority inside its container and dedicated workspace. The
  service must not receive a Docker socket, host PID namespace, privileged
  mode, broad NAS mount, or control over unrelated workloads.
- Expose the complete Printable capability to every authenticated MCP gateway
  user. Runtime isolation, not a complicated per-tool policy matrix, provides
  the blast radius.

## Target architecture

```text
MCP user
   |
   v
local MCP gateway -> printable-server (Rust)
                         |  shared /workspace
                         |  private TCP :9876
                         v
               Blender 5.2.0 headless
                         |
               NVIDIA RTX 4060 Ti
```

- Production host: Linux/amd64 `server`.
- `printable-server` and Blender are separate containers.
- Blender is persistent; it is not started once per request.
- Blender is attached only to an internal control network. No Blender or MCP
  application port is published on the host.
- Rust is dual-homed only to the gateway network and Blender control
  network. It has no container-runtime control API.
- Both containers mount one dedicated confined workspace at `/workspace`.
- Blender uses the NVIDIA runtime non-exclusively. Printable never pauses,
  kills, or reconfigures other GPU consumers.
- Blender commands execute on Blender's main thread. Socket I/O may run on a
  Python thread but may never call `bpy` there.

## Current delivered foundation

- Streamable HTTP MCP server, distinct liveness/readiness endpoints, typed
  configuration, shared-bearer and host-header protection, and external wire
  smoke.
- Confined atomic workspace I/O, bounded chunked uploads, and governed raw-byte
  artifact publication through the gateway file-transfer handoff.
- Length-prefixed Blender client with deadlines, response-ID validation,
  connect-only retry, process-wide serialization, transactions, and a fake
  backend.
- OpenSCAD discovery, camera arguments, confined-source gate, bounded execution,
  and compile/render/cross-section MCP workflows.
- Imaging composition primitives and bundled font.
- Manifold CSG binding and initial geometry crate.
- Persistent first-party Blender 5.2.0 headless bridge and digest-addressed image.
- Paired Linux/amd64 Rust and Blender image build, product smoke, exact-image
  policy/Grype gates, and promotion pipeline.
- Recorded NVIDIA EEVEE and Cycles/OptiX proof on production `server`.
- Production tools: confined artifact transfer, typed Blender scene
  inspection and mutation, primitive/boolean modeling, checkpoint/restore,
  rigid pivot-axis animation authoring, STL import/export, `.blend` save,
  bounded explicit Blender Python,
  caller-budgeted EEVEE/Cycles preview and multi-view rendering, labeled
  gallery/turntable composites, dimensioned views, cross-sections, printability
  heatmaps, before/after comparison, actionable confined-STL solid/print
  validation, confined OpenSCAD compile/render/cross-section, durable
  still/turntable/animation jobs, and status.
- A discoverable generic FDM product-design resource and bundled OpenSCAD kit
  with explicit manufacturing/form profiles and truthful evidence boundaries.

## Delivery slices

Every slice ships through the normal PR, CI, and AERB loop. Slices may be
split when reviewability improves, but must not create a second implementation
of the same behavior.

### 1. First-party headless Blender bridge

Move the Blender add-on into this repository immediately and make it the sole
authoritative copy. Add:

- a pure framing/envelope module;
- an explicit request state machine so completion and timeout select exactly
  one response;
- a persistent main-thread headless queue pump;
- validated bind, port, timeout, and render-device configuration;
- descriptor-rooted workspace traversal with private input snapshots and
  atomic output promotion, so Blender never receives a caller-controlled path;
- readiness/liveness markers, socket-thread death detection, and bounded
  shutdown;
- safe defaults: factory startup, offline mode, disabled `.blend` autoexec,
  no audio, and loopback bind outside containers.

Run an executable background-mode spike as part of this slice using Blender
5.2.0. It must exercise scene read/clear/create, object readback, STL
import/export, `.blend` save, EEVEE still rendering, clean idle shutdown, and
busy shutdown. Tests assert protocol and lifecycle behavior.

Exit: a persistent CPU-capable headless bridge works without `DISPLAY`, Xvfb,
VNC, or a GUI session.

### 2. Production Blender image promotion and GPU proof

Harden and promote the first-party `mcp-printable-blender` image built from the
official Blender 5.2.0 Linux x64 archive pinned by checksum. It bakes in the
add-on and launcher and runs non-root under tini with a read-only root and
explicit writable paths.

CI proves CPU background startup, bridge traffic, scene mutation, render
output, and SIGTERM handling. A controlled smoke on `server` proves:

- EEVEE rendering with the NVIDIA graphics stack;
- Cycles OptiX rendering with observable GPU use;
- no `DISPLAY` or host port;
- visible failure when the required GPU is unavailable;
- coexistence with another GPU workload without attempting to manage it.

Exit: digest-addressed Blender artifact, recorded hardware capability evidence,
and a release gate that repeats the GPU smoke against the exact registry
digest before compatible-pair promotion.

Initial hardware evidence:
[`evidence/blender-gpu-smoke-2026-07-18.md`](evidence/blender-gpu-smoke-2026-07-18.md).
Every promoted digest receives a fresh proof in the release workflow.

### 3. Core modeling and file workflows

Add typed Rust command adapters and MCP tools for:

- scene and object inspection;
- scene clear/checkpoint/restore and object rename;
- explicit code execution inside the isolated Blender container;
- boolean modeling;
- STL import/export and `.blend` save;
- version/backend status.

Use direct fake-backend contract tests plus real headless integration tests.
Path-bearing operations must use the shared confined workspace; arbitrary host
paths are never accepted.

Exit: a user can construct, inspect, revise, save, import, and export a model
entirely through MCP.

### 4. Visual review and still rendering

Deliver preview screenshot, tiled/gallery, turntable, cross-section,
printability heatmap, dimensions, and before/after workflows. Keep small
previews inline; persist large outputs as workspace artifacts with metadata.

Delivered first increment: `printable_render_preview` creates a confined PNG
artifact through the headless bridge, supports EEVEE and CYCLES, gives callers
an unrestricted positive work budget (one-hour default), returns images up to
1 MiB inline, and leaves larger results as artifacts. Unit and container smoke
validate the response contract, decoded PNG dimensions, and non-uniform scene
content.

Delivered second increment: `printable_render_gallery` and
`printable_render_turntable` execute each view set as one caller-budgeted
Blender-lane operation, frame render-enabled geometry in the active view layer
with a temporary orthographic camera, restore scene camera/light/render state,
retain every source view, and produce labeled contact sheets through the Rust
imaging crate. Multi-view sources are RGB8 and individually bounded to the
confined snapshot contract;
higher-resolution single images remain available through preview rendering.
`printable_compare_renders` creates a labeled BEFORE/AFTER artifact from two
confined PNGs without using Blender. Composites stay within an explicit
decoded-pixel memory budget; contact sheets fit automatically while their
individual views retain requested resolution. Single-preview and source-view
artifacts preserve a functional path for higher-resolution inspection.

Delivered third increment: `printable_render_dimensions` measures evaluated
world-space bounds, including collection instances, and pairs exact metadata
with labeled front/right/top views. `printable_render_cross_section` builds a
temporary evaluated-mesh cutaway at a caller-selected world-axis plane and
caps the exposed section while preserving nested hollow contours.
`printable_render_printability_heatmap` classifies
evaluated faces by downward overhang angle relative to the build direction and
returns exact face and area totals for supported, warning, and severe regions.
Both diagnostic renders leave source objects untouched, retain a unique
full-resolution source artifact, and return a bounded labeled result. Their
evaluated meshes are counted before diagnostic materials or BMeshes are
allocated. The copied diagnostic set caps vertices, loose and connected edges,
faces, face loops, and bounded attribute/weight values. Explicit object subsets
or hidden unrelated objects preserve a functional path for larger scenes.
Cross-sections recheck the expanded topology after bisecting and capping, before
retaining combined buffers. Direction vectors are normalized without squared-norm
overflow before Blender state mutation.
Selected instances are processed through one transient BMesh at a time and
assembled into one diagnostic Mesh/Object pair, preventing instance count from
multiplying retained Blender datablocks.
Diagnostics require evaluated mesh
objects so preflight counts do not allocate an unbounded non-mesh conversion;
callers convert, hide, or exclude other renderable types.
The supervisor deadline covers object discovery, evaluated topology and
attribute preflight, bounds measurement, temporary geometry, and rendering.

Test decoded image format, dimensions, panel layout, useful content, response
caps, and failure behavior across CPU CI and NVIDIA server smoke. Pixel-exact
GPU output is not a contract.

Exit: complete iterative visual-review loop with bounded response memory.

### 5. Geometry, print validation, and OpenSCAD

Finish the Rust geometry core using parry3d and Manifold for topology, mass
properties, components, clearance, swept clearance, retention, intersection,
overhangs, and printability. Finish OpenSCAD compile/render/cross-section and
workspace-safe import flows behind a bounded subprocess semaphore.

Use analytic shapes, properties, metamorphic tests, adversarial meshes, and
representative printable assemblies as the acceptance corpus. Expected results
come from geometry and product intent.

Exit: actionable validation reports and dependable Blender/OpenSCAD workflows.

Delivered in this slice: `printable_validate_mesh` decodes confined ASCII or
binary STL artifacts, uses Parry topology/components and Manifold solid
validation, and reports bounds, defects, winding, mass properties, build-plate
contact, and support overhangs. `printable_analyze_assembly` snapshots two
watertight STL solids, distinguishes surface clearance from volumetric
interference, evaluates caller-selected design clearance, and continuously
sweeps optional linear or rotational rigid motion while distinguishing a
design-clearance limit from physical contact and retention. Rotational paths
must earn a conservative clearance certificate over the complete arc; bounded
work exhaustion fails closed instead of accepting sampled poses. Exact-start
boundaries block conservatively. Both read-only tools share one
process-wide geometry lane, while exact assembly CSG runs in a memory-limited
disposable worker. `printable_scad_compile`, `printable_scad_render`, and
`printable_scad_cross_section` snapshot literal workspace imports, queue the
complete staging-through-commit operation behind a bounded semaphore, execute
argv-only OpenSCAD with bounded diagnostics and caller-selected work budgets,
validate generated STL/PNG/SVG output, and atomically persist the result. The
three tools share typed, bounded, deterministic `-D` definitions and an
optional reserved product variant without exposing definition values in
responses. The compile response includes immediate solid/print validation;
PNGs follow the optional bounded inline-image contract. The production image
smoke executes all three OpenSCAD workflows through MCP.

### 6. Durable render and animation jobs

Add a bounded job subsystem for operations that outlive an MCP request:

- submit, status, list, artifact discovery, and cancel tools;
- explicit queued/running/succeeded/failed/cancelled states;
- progress and current-frame reporting;
- atomic job metadata and restart recovery;
- bounded queue depth and one Blender execution lane;
- visible busy, timeout, GPU OOM, and encoder failures.

Animations render to resumable frame sequences in a confined job directory.
FFmpeg runs as a bounded subprocess in the Printable runtime to encode video;
videos return as artifacts, never base64. Cancellation is cooperative between
frames—Printable does not asynchronously interrupt `bpy` or control other
containers. Add GPU encoding only if measurement shows CPU encoding is a
material bottleneck.

Exit: reliable still, turntable, and animation workflows with inspectable
progress and recoverable artifacts.

Delivered in this slice: the five durable-job tools stream an immutable
confined `.blend` checkpoint up to Blender's 1 GiB staging boundary without MCP
response buffering, admit work through one bounded process-wide
queue, and expose atomic queued/running/succeeded/failed/cancelled metadata.
Once admitted, the bounded source snapshot, staging, and queue handoff continue
independently from the MCP request while retaining the admission fence, so
disconnecting a submitter cannot create untracked snapshot I/O, strand logical
capacity, or let later jobs enter a not-yet-durable index snapshot.
One worker holds the serialized Blender lane for a complete job, commits each
numbered PNG before advancing progress, and resumes from the first unrecorded
frame after server restart. Still, orbiting turntable, and Blender timeline
animation jobs report current source frame, partial artifacts, recovery count,
and classified busy/timeout/GPU-memory/encoder failures. Ordinary queue
cancellation is immediate; a queued captured-but-unrestored recovery remains
scheduled until session restoration is durable. Running cancellation takes
effect after the active Blender frame, while FFmpeg is terminated and reaped
directly. Packaged CPU `libx264` encodes
MP4 artifacts under caller-selected runtime and byte budgets; encode and full
decode validation share one caller-selected deadline. FFmpeg verifies the
complete output's frame count and duration before
atomic publication. Video never enters base64 content. Timeline work retains
Blender's complete supported frame interval, without a convenience frame-count
ceiling.

The worker persists the completed Blender outcome independently from session
recovery errors. A failed restore therefore retries restoration only, including
after restart, and applies the original success, cancellation, or failure once
the live session is safe. A recovered job retries a failed running-state
metadata commit behind the fence before contacting Blender. Invalid recovery
metadata blocks new admission and
all recovered mutation work until operator repair and restart.
Blocking atomic metadata writes retain process-wide serialization even when an
awaiting caller is cancelled, preventing an older write from replacing newer
checkpoint or restoration state.
Workspace publication synchronizes every newly created parent-directory entry
as well as the final artifact entry before a durable operation succeeds.

Typed `printable_rigid_rotation_animate` authoring creates a world-space pivot
controller for one rigid group, preserves every target's world transform while
parenting, and inserts linear axis-angle keyframes. It rejects existing parent
or child links, object animation data, constraints, rigid-body state, and
invalid motion inputs before mutation. Clearance remains a separate analysis
contract for ordinary timeline animation. The `mechanical_rotation` durable-job
kind provides the bound surface: it requires every scene mesh to be classified
as fixed or moving under a strict rigid-scene contract, exports those groups
from the immutable checkpoint after loading it, rejects instancers and all
non-mesh scene types except cameras and lights, requires a stable direct-render
path through the enabled active view layer for every mechanical mesh, authors
the requested motion, and persists a fully validated continuous-clearance
report before any frame can render. Every response carries a per-process bridge
identity that the serialized transaction pins across its fresh TCP exchanges. A bridge
restart, contradictory report, blocked path, or uncertified path therefore
fails closed at zero completed frames instead of rendering a different session.

### 7. Generic FDM product design

Provide a product-agnostic OpenSCAD vocabulary for hard-surface FDM products,
driven by explicit millimetre manufacturing and form profiles rather than
hidden printer assumptions. Rounded panels and shells, capsules, transitions,
ribs, bosses, patterns, and support-free horizontal bores must compose
enclosures, brackets, grips, fixtures, housings, and mechanisms without
product-specific generators. Local constructive invariants are enforced, while
global arbitrary-mesh wall thickness and moving clearance remain explicitly
uncertified until their dedicated proof workflows run.

Exit: the same public profile and modules work through compile, render, and
cross-section, and final-STL overhang analysis uses the selected profile.

Delivered in this slice: `product_v1` is bundled behind a trusted wrapper that
loads only after caller source confinement. All three OpenSCAD tools accept the
same complete profile, serialize its values as reserved typed definitions, and
publish the exact profile in their response. Compile measures topology, bounds,
build-plate contact, and the selected overhang policy while distinguishing
constructive kit guards from `not_certified` global wall thickness and
`not_run` moving clearance. The discoverable
`printable://design/product-v1` resource documents the modules, evidence
boundaries, product-form guidance, and representative rounded enclosure,
support-free bracket, and grip compositions.

### 8. Product-studio still rendering

Add deterministic disposable presentation scenes with engineering,
studio-neutral, and studio-dark profiles. Auto-frame complete evaluated bounds,
preserve source materials unless an explicit override wins, apply shading only
for presentation, and report the exact camera, lighting, materials, shading,
and color-management choices. Render-only geometry changes are forbidden:
visible edge breaks belong in the validated product.

Exit: a caller can produce useful engineering and product-review stills without
mutating the live scene, and the capability is discoverable through
`printable://render/product-v1`.

Delivered in this slice: `printable_render_product` renders selected evaluated
geometry and collection instances through disposable engineering,
studio-neutral, or studio-dark scenes. The bridge validates object/material
mappings before mutation, uses fixed 15% complete-bounds framing, preserves
source materials with explicit fallback/override precedence, reports every
effective presentation choice, and promotes no artifact unless cleanup and
source-state verification succeed. Product edge breaks remain part of the
validated geometry; smooth-by-angle is explicitly presentation-only. The
grounded profiles reject below-ground views, output uses one disclosed
16,777,216-pixel/64 MiB render-and-verification budget, and evaluated geometry
is resource-preflighted before presentation allocation. The
bridge compatibility contract is now `0.3.0`, and
`printable://render/product-v1` documents the public workflow.

### 9. Presentation-aware galleries and durable animation

Reuse one presentation contract and one Blender setup/cleanup path across
galleries, turntables, and durable render jobs. Legacy calls retain existing
output. Static product work auto-frames complete bounds; general animation
preserves the authored camera unless sequence-wide framing is explicitly
budgeted. Mechanical presentation remains subordinate to certification:
clearance is proven against original checkpoint geometry before presentation,
and blocked or inconclusive motion produces zero frames.

Exit: stills and videos replay the same persisted presentation after restart
without changing source geometry or a stored clearance certificate.

Delivered in this slice: galleries and contact-sheet turntables optionally use
the same typed presentation schema and isolated Blender setup/cleanup path as
product stills, while omission retains their prior orthographic output.
Durable still, turntable, timeline-animation, and mechanical jobs persist and
replay presentation metadata. Timeline animation preserves an authored camera
by default or measures complete sequence bounds under a separate caller
budget. Mechanical work certifies the original checkpoint geometry before
presentation, produces zero frames when blocked or inconclusive, and frames a
conservative complete-rotation envelope without a mesh ground plane. Strong
framing metadata is persisted before frame one for presented stills and
turntables and replayed unchanged across every view and restart.
Source/cleanup attestations, frame identity, and presentation profile are
validated before durable progress advances. Presented frames also pass
confined size, digest, complete-PNG, and dimension verification before
completion is recorded. Preserved authored cameras are rejected if they fall
at or below an enabled studio ground plane. The bridge compatibility contract
advanced to `0.4.0` for this delivery.

### 10. Multi-product acceptance corpus

Exercise only public generic contracts with a rounded enclosure, support-free
mounting bracket, tapered/capsule grip, and an articulated hinge used solely as
one rotational-clearance fixture. Acceptance is behavioral: useful decoded
artifacts, manifold printable geometry within the requested policy, honest wall
and clearance evidence, visibly distinct presentation profiles, source-scene
preservation, and a complete-arc certificate before mechanical rendering.

Exit: reverting any product capability breaks an owning user-visible workflow,
without relying on pixel-exact GPU output or hinge-specific production code.

Delivered in this slice: reusable OpenSCAD sources under `acceptance/products`
compose the public kit into an enclosure, bracket, grip, and two-body
articulated fixture. The external MCP smoke compiles and measures every product,
imports the resulting STLs, decodes product stills, a gallery, a turntable, and
the final mechanical video, requires named profiles to differ structurally,
and compares Blender scene state around presentation. The hinge variants pass
the public conservative complete-arc analysis before the durable mechanical job
is submitted; the job must independently retain that certificate, restore the
source scene, emit the exact frame sequence, and produce a fully decodable MP4.
Wall and clearance reporting remains scoped to actual evidence.

### 11. Release pairing, CI, and operational validation

Release the Rust and Blender images as an explicitly compatible pair of
immutable registry digests. Commit-scoped tags are discovery pointers;
promotion occurs only after:

- Rust fmt, clippy, tests, fuzz/security gates, and image smoke;
- Blender pure tests and CPU background integration smoke;
- image configuration and vulnerability checks;
- server-side GPU capability smoke for release candidates;
- end-to-end MCP tests across the real two-container topology.

The CI image job and opt-in publisher call the same shared-workspace durable
render smoke, so neither path can promote a pair under a weaker compatibility
gate. Main CI then moves one production discovery pointer to the verified pair
record; deployment resolves both digest-qualified runtime images from that
record before it starts either service.

Linux/amd64 is the sole release and CI target, matching production `server`.

Exit: repeatable builds, compatible image promotion, and actionable failures.

### 12. Paired private deployment on `server`

Add the production Compose/Komodo wiring in `docker-home`:

- internal Blender control and MCP gateway networks;
- dedicated workspace volume;
- NVIDIA runtime for Blender only;
- non-root, dropped capabilities, no-new-privileges, read-only roots, bounded
  memory/process settings, and no host application ports;
- one Infisical-backed gateway bearer validated by the Rust service without
  copying its value;
- health/readiness checks that distinguish busy from dead.

Resolve the promoted pair to both immutable child digests before replacing a
running container. Validate the new stack in an isolated project and workspace
first. The gateway is the only remote caller, Printable validates its shared
bearer directly, and no application or Blender control port is published.

Exit: the exact verified pair is healthy and ready on the private server
networks.

### 13. MCP gateway cutover

Attach the gateway directly to the private Printable network, retain the
existing bearer and user policy, and change only the upstream URL. Through the
real gateway,
verify the complete catalog and resources, a generic bracket compile and
validation, a product still, artifact retrieval, and the hinge fixture's
complete-arc certificate before video submission. Any catalog, readiness,
clearance, or real-call failure restores the saved upstream and previous
immutable pair.

Exit: authenticated production requests traverse the local MCP gateway to the
headless server stack as the sole Printable production path.

### 14. Reliability, recovery, and performance closeout

Exercise representative assemblies, high-resolution galleries, OptiX scenes,
frame sequences, concurrent clients, restarts, Blender crashes, full queues,
workspace pressure, GPU contention, and GPU OOM. Record cold/warm render times,
peak RAM/VRAM, output size, and recovery results. Fix demonstrated defects and
keep failures bounded, actionable, and free of partially promoted artifacts.

Exit: all product acceptance criteria below pass through the real gateway.

### 15. Retire the obsolete Mac/public path

After the reliability soak and explicit approval for any DNS deletion, remove
the obsolete Mini-native Blender path, Mini-specific deploy workflow, and
unused public route. Preserve deployment history and rollback instructions, and
document every item retained by the decommissioning sweep.

Exit: the private server stack is the only active Printable path and no
recoverable history is discarded.

## Product acceptance criteria

- Users can create, inspect, revise, validate, render, import, export, save,
  and animate general-purpose hard-surface FDM products through MCP.
- Product profiles are explicit millimetre inputs; arbitrary wall and moving
  clearance claims are never inferred from those inputs.
- Engineering and product-studio artifacts communicate the product clearly
  without changing validated geometry.
- All authenticated gateway users see and can invoke the complete catalog.
- Blender survives normal long-running use without a GUI session; restart and
  failure states are visible and recoverable.
- Every queue, request body, frame, subprocess, artifact, and workspace scan
  has an enforced bound.
- Mutating requests are never automatically retried after bytes are sent.
- GPU contention queues within Printable or fails clearly; Printable never
  manages unrelated processes or containers.
- Blender and MCP application ports are private, the runtime has no Docker
  socket or broad host mount, and high-authority code execution remains inside
  the dedicated container/workspace blast radius.
- Still and animation artifacts are useful, reproducible, and discoverable;
  large media never travels as base64.
- CI, release promotion, Komodo deployment, and gateway integration are
  reproducible from committed configuration with no embedded secrets.

## Ordering

```text
headless bridge -> Blender image/GPU proof -> modeling/files -> visual renders
                                            \-> geometry/OpenSCAD
visual renders + core modeling -> durable jobs/animation
generic design -> product stills -> presentation jobs -> product corpus
all capability slices -> paired release -> server deploy -> gateway cutover
gateway cutover -> reliability/performance closeout -> obsolete-path retirement
```

The remaining operational sequence is supported pair promotion and private
deployment verification, authenticated gateway cutover, reliability and
performance closeout, then obsolete-path retirement.


## Selective Blender modeling inspection

The modeling interface adds a discoverable Blender workflow resource and
bounded inspection for filtered scene objects, material slots, modifiers,
hierarchy, and authored material/Geometry Nodes topology. Existing explicit
Python execution remains the modeling engine; Code Mode composes calls and
filters results. Saved scripts and broad helper libraries remain deferred
until concrete workloads justify them. Delivery requires public MCP smoke
coverage of creation, revision, inspection, and rendering from the guide,
followed by paired-image deployment and live verification.
