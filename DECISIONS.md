# Decisions

> Historical record: this file describes earlier capability delivery or the
> private lab deployment. Use [the current project guide](README.md) for
> independent installation and supported behavior. Current work is tracked in
> GitHub issues.

Running log of active design decisions. The capability and delivery sequence
is [`PLAN.md`](PLAN.md).

## 2026-09-08 — Native observation and a coherent agent interface

The approved [product refactor](docs/product-refactor.md) is the next delivery
authority. It replaces the accumulated public catalog with thirteen unprefixed
workflow tools while preserving existing capability through typed action
arguments. Job execution and artifact transfer remain separate responsibilities.

One GUI-backed Blender process owns live editable state and native observations.
Separate bounded workers render immutable checkpoints without replacing the live
scene or synchronizing edits back. GPU contention remains subject to measured
admission; multiple processes do not promise unlimited concurrency. The existing
container/workspace isolation and exact-input manufacturing evidence remain
requirements.

Native viewport drawing, editor-region capture, and controlled diagnostics have
different fidelity contracts. Observations identify state and method; stale or
unsupported output must never masquerade as successful visual evidence. Python
remains the general action interface, with explicit context where Blender
operators require it. Agent task success, investigation quality, recovery, and
measured text/image usage determine whether the refactor succeeds.

The [evaluation plan](acceptance/agent-workflows.md) distinguishes existing
regression evidence from pending measurements. This decision authorizes delivery;
it does not describe the currently shipped runtime as GUI-backed.

## 2026-08-31 — Printable validates one shared gateway bearer directly

- An authorized client and Printable receive the same `PRINTABLE_MCP_BEARER`
  through protected configuration. Printable requires the exact `Authorization: Bearer` credential
  on every `/mcp` request; there are no users, roles, sessions, or secondary
  service credentials in the application.
- `/healthz` and `/readyz` remain unauthenticated private operator probes.
  Raw artifact downloads retain their existing short-lived, one-use capability
  bearer, which is scoped to one immutable snapshot rather than service
  identity.
- The gateway connects directly to Printable on their dedicated private
  network. No reverse-proxy sidecar or third-party proxy image sits in the
  request path, and neither the MCP nor Blender port is published on the host.
- The existing DNS-rebinding allowlist still admits only the direct service
  authority used by the gateway. Secret values never enter images, source,
  logs, health output, or MCP responses.

## 2026-08-31 — Artifact delivery uses the governed MCP file handoff

- Artifact-producing tools continue to return confined workspace paths. A
  caller explicitly invokes `printable_workspace_publish` when bytes must leave
  Printable, so routine modeling and rendering do not eagerly incur gateway
  storage or transfer latency.
- Publication snapshots the confined artifact once and returns a structured
  `FileValue` containing a private URI, media type, size, and SHA-256 integrity
  metadata. Artifact bytes never enter JSON, text, image content, or base64;
  the gateway fetches the raw immutable byte sequence over a bounded HTTP
  stream and rewrites the reference into its own authenticated namespace.
- Printable implements `files/authorizeDownload` with a short-lived, rotating
  bearer credential. Each download consumes the snapshot and authority once.
  Retained and actively streamed snapshots share one process-wide capacity
  bound, have a per-file byte bound, and expire when a handoff is abandoned.
- This transfer path covers every supported artifact type, including STL, PNG,
  BLEND, and MP4. The legacy bounded base64 read tool remains available for
  compatible small non-video workflows but is not part of gateway delivery.

## 2026-07-25 — Production images track supported upstream runtimes

- Runtime and Blender images use the current Debian stable release rather than
  an oldstable base whose regular security support has ended. Both base indexes
  remain digest-pinned.
- The headless image uses the current stable Blender 5.2.0 Linux x64 archive
  and verifies Blender's published checksum before extraction.
- Blender's archive currently bundles an older `urllib3` than the release
  policy permits. The image replaces only that package with the current
  established PyPI wheel, pinned by its registry URL and SHA-256. It retains
  the runtime modules Blender needs while removing unused embedded Python
  package-management entrypoints and metadata from the production image.
- Base refreshes must pass the existing public product smoke, GPU coexistence
  proof, and exact-image release scan. Scanner exceptions are not a substitute
  for a supported runtime.
- High and critical findings block promotion when Grype identifies a fix.
  Every finding without a vendor remediation remains visible in exact-image
  workflow output with its severity and fix state, and every CISA KEV blocks
  regardless of fix state. This keeps the gate actionable without globally
  ignoring vulnerability identifiers or permanently fencing every supported
  Linux distribution on upstream-unfixed reports.

## 2026-07-23 — Readiness and release promotion are product gates

- `/healthz` remains process liveness. `/readyz` is the unauthenticated,
  sanitized dependency boundary: it returns only ready/busy/blocked codes and
  booleans for Blender, workspace, OpenSCAD, FFmpeg, recovery integrity, and
  active work. It never returns addresses, paths, configuration values, or
  diagnostics.
- Readiness never queues behind a render. A nonblocking Blender lane probe
  reports an already-admitted request as healthy busy work; an idle lane still
  performs a real bridge exchange. FFmpeg and Blender probes run concurrently
  so cold readiness is bounded by the slower probe rather than their sum.
  Concurrent readiness probes fail closed rather than treating one another as
  product work. OpenSCAD discovery requires an executable file, and readiness
  caches a bounded version-identity probe for that configured binary.
- Both exact runtime-image digests must pass architecture, non-root identity,
  health command, entrypoint, source revision, role label, and embedded
  credential checks before pair promotion. Credential inspection covers both
  sensitive key names and credential-shaped values in image configuration and
  build history while allowing runtime environment references. Baked
  environment keys, values, and labels must also match the exact first-party
  runtime contract, so an unknown opaque configuration field fails closed.
- Release scanning uses the fleet-pinned Grype version and the first-party
  fix-available high/critical plus all-CISA-KEV fail-closed policy. The KEV
  feed must contain well-formed CVE identifiers, and a scan or feed error
  blocks promotion.
- The paired smoke requires `/readyz` before running the generic product
  workflow. Bracket compile/validation/import/render is the release baseline;
  enclosure and grip prove presentation breadth, and the hinge remains only the
  articulated complete-arc/video fixture.

## 2026-07-23 — Acceptance follows complete product workflows

- The acceptance corpus is reusable product source, not private test geometry.
  Enclosure, bracket, grip, and articulated examples use only the same public
  OpenSCAD, assembly, Blender import, presentation, and durable-job contracts
  available to callers.
- The hinge remains one generic composition and rotational-clearance fixture.
  Its fixed and moving exports do not create a hinge-specific server module or
  weaken the product kit into a single-purpose API.
- Release acceptance measures and decodes user-visible results. It rejects
  non-printable compiled geometry, blank images, indistinguishable named
  profiles, mutated presentation sources, incomplete frame sequences, and an
  MP4 that cannot be fully decoded with the requested timing.
- Pixel output is not treated as deterministic across GPUs. Structural image
  checks require dimensions, visible contrast, and meaningful average color
  difference while the renderer's deterministic metadata owns exact camera,
  lighting, material, shading, and color-management choices.
- Compile-time wall and moving-clearance fields remain honest. Kit-local
  constructive guards do not become a global wall claim, and the articulated
  video cannot begin until complete-arc analysis passes against separately
  exported rigid bodies. The durable job independently reproduces and stores
  that certificate before frame one.

## 2026-07-23 — Product design profiles compose a trusted generic OpenSCAD kit

- `product_v1` is a product-agnostic hard-surface FDM vocabulary, not a
  hinge generator. Its rounded panels and shells, capsules, transitions, ribs,
  bosses, patterns, and support-free horizontal bores are intended to compose
  enclosures, brackets, grips, fixtures, housings, and mechanisms through the
  same public OpenSCAD tools.
- A complete profile supplies explicit millimetre manufacturing and form
  values with `+Z` build direction. The server validates those values before
  queue admission, serializes them as reserved `pbl_*` argv definitions, and
  applies the same profile to compile, render, and cross-section. Compile uses
  the requested overhang threshold for actual final-STL analysis.
- Caller source passes the existing confinement gate before the server stages a
  trusted wrapper, bundled kit, and already-confined caller file in one private
  directory. This makes the kit available without weakening the prohibition on
  caller-controlled `include` or `use`.
- Caller source may invoke public kit modules and read profile definitions, but
  cannot declare modules or functions in the public `pbl_*` or internal
  `_pbl_*` namespaces. Token-aware validation runs before caller import
  snapshots, so untrusted source cannot replace trusted constructive guards.
- Kit modules guard constructive local invariants and bound curve tessellation
  from feature and nozzle scale. Those guards do not prove global wall
  thickness for arbitrary caller geometry. Compile reports global wall as
  `not_certified` and moving clearance as `not_run`; moving products still
  require separate rigid-body exports and assembly analysis.
- Shell corner offsets use one shared facet count and compensate for each
  facet's apothem. The final polygonal corner wall therefore meets the requested
  minimum; straight regions retain a small conservative tessellation allowance.
- Bored bosses preserve their requested inner and outer diameters, use one
  shared facet count, and reject dimensions whose facet-normal separation is
  below the profile minimum wall.
- Horizontal-bore roofs derive from 98% of the caller-selected overhang policy.
  Bed chamfers use the smaller of that angle and 45 degrees so their edge break
  stays compact. Both remain within the policy after tessellation; stricter
  profiles intentionally make the bore roof taller instead of silently
  reverting to a 45-degree printer assumption.

## 2026-07-21 — Mechanical animation binds certification to the rendered checkpoint

- `mechanical_rotation` is a distinct durable-job kind rather than a safety
  claim added to general timeline animation. Its contract names every mesh in
  the active scene as fixed or moving and supplies the pivot, axis, positive
  angular travel, timeline, and non-negative clearance threshold.
- The accepted scene subset is deliberately strict: every mechanical object is
  a visible mesh with no existing hierarchy, object or mesh animation,
  constraints, rigid-body state, modifiers, shape keys, or instancing. This
  includes render visibility inherited through the complete collection tree and
  excludes geometry or transforms that could change independently of the
  analyzed rigid motion. Cameras and lights are the only accepted non-mesh scene
  objects; collection instancers and every other potentially renderable object
  type are rejected because their geometry would be absent from the certificate.
- While one recovery-fenced Blender transaction owns the serialized lane, the
  worker loads the immutable job checkpoint, exports the complete fixed and
  moving groups in shared world coordinates, explicitly linearizes the stored
  axis-angle F-curves, and reads their actual values back. The Rust worker
  snapshots those reserved STLs under the
  caller's byte budget, acquires the process-wide geometry lane, and passes the
  immutable snapshot paths directly to the memory-limited assembly process; the
  server does not retain duplicate STL byte buffers.
- The report and exact analysis-artifact metadata are committed into durable job
  state before rendering starts. A blocked path, an incomplete certificate, a
  malformed worker response, a start clearance at or below the requested
  threshold, or bounded-work exhaustion fails before frame one.
  Admission also requires every mechanical mesh to have a stable direct-render
  path through the enabled active view layer; excluded, holdout, indirect-only,
  hidden, and animated collection paths cannot satisfy visibility.
  The validator checks every nested part, bounds, static, witness, interval,
  endpoint, retention, and rotation invariant rather than trusting only the
  headline pass flag. Every bridge response carries a per-process UUID; the
  transaction pins it through preparation, analysis, and frame rendering even
  though each exchange opens a fresh connection. A bridge restart therefore
  fails the generation instead of rendering an uncertified new session. Restart
  recovery repeats preparation and certification after each source reload in a
  new generation directory, so a new export never replaces bytes named by the
  last durable certificate.
  The required response identity was introduced in bridge compatibility
  version `0.2.0`; production configures the current expected version and
  surfaces a mismatch even when a legacy envelope is rejected for omitting the
  identity.

## 2026-07-23 — Presentation follows product evidence and survives restart

- Gallery, turntable, and durable render surfaces reuse the typed product
  presentation schema. Calls that omit it keep their prior rendering behavior;
  diagnostic dimensions, sections, and heatmaps are not restyled.
- Static presented work frames evaluated bounds. General timeline animation
  preserves the authored camera by default while stable bounds lock lights and
  ground; explicit `auto_frame_sequence` measures every requested frame under
  its own positive caller budget and persists the union before rendering.
- Mechanical presentation is downstream of clearance certification. The
  original checkpoint geometry is exported and certified before presentation
  metadata is prepared. A blocked or inconclusive certificate is durably
  recorded and produces zero frames.
- A passing mechanical job uses a conservative sphere envelope around the
  moving body's complete fixed-pivot rotation, unioned with fixed-body bounds.
  It never relies on presentation-frame sampling and never adds a mesh ground
  plane that was outside the certified assembly.
- Durable records store presentation and stable framing bounds. Recovery
  validates those bounds, resumes at the next unfinished frame, and emits the
  same camera behavior and framing request. Records from the preceding schema
  deserialize with presentation disabled.
- Presented stills and turntables measure the restored checkpoint's current
  evaluated bounds once and durably store that envelope before frame one.
  Every view and restart replay uses the stored bounds rather than independently
  fitting a self-consistent frame.
- Product job responses must attest the requested frame, dimensions, path,
  profile, camera behavior, stable bounds, cleanup, and source-state equality
  before durable progress advances. The server also snapshots each presented
  frame, matches its positive size and SHA-256, and completely decodes the PNG
  at the requested dimensions before recording it complete.
- A preserved authored camera must remain above an enabled studio ground at
  every rendered frame. The bridge rejects a violating evaluated camera before
  rendering, and the server independently rejects contradictory response
  metadata, so a grounded profile cannot succeed with the product occluded by
  its presentation plane.
- The bridge compatibility contract advances to `0.4.0`.

## 2026-07-23 — Product rendering is isolated presentation, not model repair

- `printable_render_product` operates on copied evaluated geometry in a
  disposable scene, so collection instances and modifiers are visible without
  changing the source scene.
- Engineering, neutral-studio, and dark-studio profiles have fixed cameras,
  color management, environments, and soft-light roles. Complete selected
  bounds receive a fixed 15% margin, making framing repeatable across products.
- Existing materials are preserved, material-less meshes receive a neutral
  fallback, and typed explicit overrides win. Empty slots are treated as
  missing materials even when a mesh still has a nonzero slot count; mixed
  real/empty slots preserve the real materials, fill the empty slots, and
  report both outcomes. Effective material references are resolved from the
  evaluated object's slots, so both `DATA` and `OBJECT` links retain their
  appearance on the instance-specific presentation mesh. Smooth-by-angle is
  applied only to copied presentation meshes.
- Product presentation suspends every Blender application-handler list across
  evaluated-geometry capture, presentation construction, rendering, and
  cleanup. The exact handler lists are restored and source state is rechecked
  before artifact promotion, so render-time scene callbacks cannot mutate the
  live product as a side effect.
- The renderer never adds a bevel modifier. Edge breaks that communicate form
  must exist in the validated product geometry rather than being invented for
  a still.
- A staged PNG is promoted only after presentation datablocks are removed and
  the source scene signature and datablock counts match their pre-render state.
  The bridge reports a SHA-256 digest that the Rust server checks against its
  confined snapshot, so a concurrent destination replacement fails instead of
  being returned under the original response. The bridge protocol advances to
  `0.3.0` with this command and its response contract.
- Grounded studio profiles reject below-ground elevations rather than emitting
  an apparently successful ground-only image. Engineering views remain free to
  inspect the product from below because they have no ground plane.
- Product output admits up to 16,777,216 pixels and uses the same 64 MiB budget
  for Blender output and the server's internal verification snapshot. Evaluated
  instance, topology, attribute, and material-slot totals are preflighted before
  presentation-scene allocation and reported with successful renders.

## 2026-07-21 — Rigid rotation authoring owns the pivot hierarchy

- `printable_rigid_rotation_animate` accepts one rigid group of named Blender
  objects with no existing hierarchy, object animation data, constraints, or
  rigid-body state, plus a world-space pivot, normalized right-hand-rule axis,
  positive angular travel, and timeline range. It creates a uniquely named
  Empty controller and authors linear axis-angle keyframes, so callers do not
  need raw Python parenting or quaternion component curves.
- Every target world matrix is snapshotted before parenting. The bridge derives
  `matrix_parent_inverse` from the controller's evaluated world matrix and does
  not rewrite `matrix_world`, because that would discard the parent inverse.
  An identity parent inverse at an offset pivot would add the pivot translation
  to the child and is never used.
- Duplicate or missing targets, controller collisions, existing target
  hierarchy or motion, invalid vectors, and invalid frame ranges fail before
  mutation.
  A keyframe failure restores interpolation preferences, target hierarchy and
  transforms, the original timeline frame, and removes the controller; an
  incomplete rollback fails visibly.
- Rotation authoring establishes animation semantics only. It is not evidence
  of collision clearance. Mechanical acceptance still comes from
  `printable_analyze_assembly`; a later job surface will bind that analysis to
  the exact scene snapshot it renders.

## 2026-07-20 — Assembly analysis keeps interference distinct from clearance

- `printable_analyze_assembly` snapshots two confined watertight STL solids in
  one shared millimetre coordinate system. It is read-only and returns both
  artifact metadata records alongside the fixed/moving report.
- Manifold computes volumetric intersection while Parry computes nearest
  surfaces and continuous translational shape casts. Rotational checks accept a
  world-space pivot, normalized right-hand-rule axis, and positive angular
  travel. An adaptive Lipschitz bound over Parry proximity queries is the
  pass/fail authority because nonlinear casting cannot target positive
  clearance and does not support this triangle-mesh pair. A contained solid
  can therefore report positive surface gap and volumetric interference without
  mislabeling the assembly as clear.
- Design clearance is caller-selected, not a service guess. Optional linear
  motion continuously sweeps the moving triangle mesh over the full requested
  distance; it does not sample positions. A positive design-clearance boundary
  is reported as a clearance threshold and does not claim physical retention;
  zero-clearance impact is reported as contact. Motion that starts exactly on
  its requested boundary is conservatively blocked at distance zero because the
  mesh kernel cannot reliably distinguish separating or tangent start contact
  without weakening or skipping part of the continuous envelope. Linear motion
  that starts strictly outside the envelope uses the exact caller target over
  the complete path, and may finish exactly on the boundary.
- Initial volumetric interference blocks rigid-motion analysis at distance zero.
  The tool does not pretend rigid meshes model press-fit deformation, material
  compliance, friction, or insertion force.
- Rotational analysis is bounded. It reports `clearance_not_certified` when the
  complete angular envelope cannot be proven within that work budget, so a
  visually plausible set of frames can never substitute for motion clearance.
- Pair analysis retains at most two independently 25 MiB-capped artifact
  snapshots and shares the process-wide geometry lane; concurrent callers queue
  without a convenience deadline. Exact Manifold CSG executes in a disposable
  child with a configurable address-space limit. Boolean-topology expansion
  therefore fails the individual call visibly instead of exhausting the MCP
  server. The production server-image smoke gates the worker, intersection,
  clearance, and continuous clearance-limit behavior through MCP.

## 2026-07-20 — STL validation separates solid defects from print warnings

- `printable_validate_mesh` analyzes an immutable confined snapshot of an ASCII
  or binary STL. STL coordinates are explicitly millimetres; optional density
  converts valid solid volume from cubic millimetres to grams.
- Parry owns half-edge topology, boundary detection, connected components, and
  center of mass. Manifold is the final solid-validity and volume/surface-area
  authority. The service does not silently delete, weld, reorient, or repair
  caller geometry while claiming to validate the original artifact.
- Open boundaries, shell winding inconsistent with containment depth,
  degenerate faces, duplicate faces, and Manifold rejection are solid errors
  with repair guidance. Correctly inward cavity shells remain valid. Multiple
  disconnected solid regions and support overhangs are warnings because
  multi-part exports and supported prints can be intentional.
- Overhang analysis uses the selected normalized build direction and excludes
  downward faces on the minimum build plane as bed contact. Its plane tolerance
  derives from STL's f32 coordinate precision and local mesh extent, so support
  classification is invariant under translation.
- Cap-sized artifacts already have a 25 MiB confined-transfer bound. One
  process-wide geometry permit prevents concurrent sessions from multiplying
  the expanded STL, BVH, topology, and Manifold memory peak; callers queue
  without an invented geometry deadline.
- The production server-image smoke uploads and validates a real cube through
  MCP so native geometry packaging, not only the Rust unit surface, gates the
  release.

## 2026-07-19 — Diagnostic views use disposable evaluated geometry

- Dimension views stream evaluated mesh vertices through each dependency-graph
  instance transform, then render front, right, and top in one serialized
  Blender operation. This avoids the overstatement caused by transforming a
  rotated object's local bounding box. Exact bounds remain structured metadata;
  labels are a visual aid, not the measurement source.
- Cross-sections and printability heatmaps process evaluated meshes through
  transient BMeshes into one temporary render Mesh/Object pair. Collection
  instances and modifiers are therefore represented without changing source
  meshes, modifiers, materials, visibility, camera, lights, or render settings.
  Cleanup must succeed before the staged PNG is promoted.
- Cross-sections bisect on a world-axis plane, discard the positive half, and
  tessellate all closed cut contours together so nested contours remain holes
  in the distinct cap material and reported section area. Blender's returned
  indices are validated and mapped through the flattened input contours, so
  coincident coordinates in disconnected topology are not silently joined.
  Heatmaps classify every evaluated face from its downward angle relative to the
  normalized build direction: through the selected threshold is supported, the
  next 15 degrees is warning, and steeper faces are severe. Exact face and
  world-area totals are returned with the render.
- A diagnostic render preflights every selected evaluated mesh before creating
  materials, BMeshes, or copied render meshes. The copied diagnostic set accepts
  at most 1,000,000 vertices, 3,000,000 edges, 2,000,000 faces, 6,000,000 face
  loops, and 16,000,000 bounded attribute/weight values; string attributes are
  refused because their payload is not bounded by the element count. This covers
  all topology and variable custom-data entries that `BMesh.from_mesh` copies.
  Large scenes remain usable by hiding unrelated objects or passing up to 1,000
  unique source-object names; every matching collection instance is included.
  Subset discovery retains only the requested-name set. Instances are processed
  through one transient BMesh at a time and assembled into one diagnostic
  Mesh/Object pair, so retained Blender datablock cardinality is constant.
  Cross-sections recheck exact expanded topology before retaining combined
  buffers. Direction vectors use overflow-safe normalization before mutation.
- Diagnostic analysis accepts evaluated mesh objects. A non-mesh conversion
  would have to allocate before its resulting vertex/face count was knowable,
  defeating the preflight invariant. Callers convert non-mesh geometry, hide
  it, or select only mesh objects.
- The independent execution watchdog is armed before renderable-name discovery,
  evaluated-mesh preflight, and bounds measurement. The caller budget therefore
  covers every variable Blender scan as well as temporary geometry and rendering.
- Rust accepts heatmap bounds only when they match the source geometry. A
  cross-section must remain inside its source bounds and on the retained side of
  its cut plane; a non-empty cap must touch that plane. The comparison allowance
  derives from Blender's single-precision mesh-coordinate storage.
- World transforms are baked into the disposable BMesh. A negative determinant
  reverses face winding, so orientation-reversing instances have their faces
  reversed again before normals are recalculated and classified. Every finite,
  nonzero determinant is accepted; scale alone does not make a transform singular.
- Blender writes the full-resolution diagnostic to a CSPRNG-named confined
  source path. Rust validates its backend contract, snapshots that exact path
  under the process-wide visual-memory permit, and writes the labeled artifact.
  Random sources prevent two calls using the same public destination from
  substituting one another's bytes. The source remains durable when a labeled
  result is fitted to the generated-artifact memory budget.

## 2026-07-19 — Multi-view review is a single Blender operation with durable sources

- Galleries and turntables submit every camera view as one serialized Blender
  command. Queue admission receives only the ordinary request allowance; once
  admitted, the complete set receives the caller's full render budget. Another
  session cannot interleave a scene mutation between frames.
- Blender validates and stages every destination before changing render state,
  arms one supervisor deadline for the batch, automatically frames evaluated
  geometry enabled for rendering in the active view layer with a temporary
  orthographic camera, and restores the original camera, render settings, and
  temporary lights on every exit path. It promotes view artifacts only after
  every frame renders successfully.
- Individual views use a CSPRNG-named confined batch directory and remain
  discoverable artifacts. The Rust service snapshots and decodes those views,
  then builds labeled gallery or turntable contact sheets with the bundled
  imaging font. Random batch paths prevent concurrent calls targeting the same
  composite name from substituting another call's view artifacts.
- A batch accepts at most 36 staged outputs and 67,108,864 pixels of aggregate
  render surface. Each source is capped at 8,388,608 pixels and encoded as
  RGB8, keeping worst-case PNG data within the workspace's 25 MiB snapshot cap
  before composition. Public contact sheets use the same pixel cap, keeping the
  raw RGB canvas plus encoder overhead under the generated-artifact cap while
  bounding simultaneous decoded tiles and PNG encoding memory. Each source is
  resized to its fitted tile immediately after decoding, so full-resolution
  decodes are not retained across the batch. Visual composition has one
  process-wide memory permit shared by gallery, turntable, and comparison
  calls. A contact sheet that would exceed the cap is fitted down while its
  individual sources retain the requested resolution. Higher-resolution work
  remains functional through single-preview rendering; durable jobs cover
  larger frame sequences.
- Before/after comparison operates only on confined PNG snapshots and does not
  consume the Blender lane. Its output uses the 8,388,608-pixel composite cap;
  decoding is limited to 16,777,216 pixels per input and 64 MiB of decoder
  allocation. The output is always an artifact and uses the same optional
  1 MiB inline response contract as other review images.

## 2026-07-20 — Durable renders consume immutable checkpoints and resumable frames

- Durable still, turntable, and timeline-animation jobs require a confined
  `.blend` checkpoint. Submission snapshots it into the random job directory,
  so queued execution and restart recovery never read mutable live-scene state.
  Snapshotting streams under a caller-selected budget up to Blender's existing
  1 GiB artifact boundary rather than inheriting the 25 MiB MCP transfer cap.
  Once a job consumes logical admission, a detached staging continuation owns
  the blocking source snapshot, metadata persistence, source commit, and
  physical queue handoff. Cancelling the MCP request cannot leave an untracked
  snapshot or strand the admitted record. Admission ownership travels through
  the blocking copy and remains fenced until staging either queues the job or
  records a terminal outcome, so later jobs cannot enter an index snapshot or
  start source I/O across that boundary.
- One process-wide worker holds Blender's serialized lane for a complete job.
  Each PNG is atomically committed before progress metadata advances; restart
  recovery resumes from the first frame not recorded complete. Re-rendering a
  frame after a crash safely replaces the same deterministic destination.
- The worker saves the live Blender session to a durable, reserved checkpoint
  before loading the immutable job source. It restores the live session before
  releasing Blender's serialized lane on success, cancellation, or failure.
  Captured/restored state is persisted with the job so restart recovery
  completes an outstanding restore before making cancellation terminal. A
  recorded restore permanently ends Blender work for that job; recovery may
  repeat post-Blender encoding but cannot reopen the source or restore the
  stale session checkpoint over newer live work. A render failure is persisted
  with the restore boundary so recovery cannot encode an incomplete sequence.
  An endpoint-wide client fence rejects ordinary Blender mutations from durable
  checkpoint capture until restoration metadata commits. On server restart the
  fence is reconstructed synchronously from persisted jobs before recovery is
  scheduled. The job worker has the only fenced-lane bypass, so an uncertain
  external restore can be replayed safely without racing newer user work.
  A failed restore remains nonterminal and visible in job status, then retries
  without an attempt ceiling using bounded backoff. Restart also requeues any
  captured-but-unrestored record regardless of its prior terminal label, so a
  transient bridge failure cannot strand the endpoint behind the fence. The
  completed Blender outcome is stored independently from the current recovery
  error; same-process and restart retries restore only the live session, then
  apply the original success, cancellation, or failure without rerendering.
  If a requeued recovery cannot persist its transition to running, it remains
  nonterminal behind the fence and retries only that metadata commit before
  contacting Blender.
  Every atomic metadata write carries the process-wide persistence guard into
  its blocking filesystem operation. Cancelling an awaiting request therefore
  cannot release serialization while a stale snapshot can still rename over a
  newer checkpoint or restoration marker.
  After Blender reports a successful restore, the worker releases the Blender
  lane but leaves the fence closed and retries only the restored-marker commit
  until it is durable. It never repeats Blender restoration within that live
  process; a server crash before the commit safely replays restoration behind
  the reconstructed fence. Unreadable or invalid recovery metadata fails
  the endpoint closed, rejects new durable jobs, schedules no recovered
  mutation work, and reports an operator-visible integrity block instead of
  guessing that the live session is safe.
- Cancellation is immediate for ordinary queued work and cooperative after the
  active Blender frame while running. Queued cancellation removes the pending
  work item before the released logical admission slot becomes visible, so
  churn cannot exhaust a separate physical backlog. A captured-but-unrestored
  recovery job instead records cancellation and remains queued until it restores
  the live session durably; cancellation cannot discard the only scheduled
  restore behind the endpoint fence. FFmpeg encoding is separately terminated
  and reaped on cancellation or caller-selected timeout. Printable never
  signals Blender asynchronously or manages another container.
- Frame runtime, aggregate PNG storage, encoded video size, and encoder runtime
  are caller-selected positive budgets with functional defaults and no
  configured product maximum. Encode and full decode validation share one
  encoder deadline rather than receiving separate copies of the budget.
  Timeline ranges retain Blender's supported frame
  interval and turntable counts retain the finite `u32` request range; there is
  no convenience frame-count ceiling.
- Videos use packaged CPU `libx264`, remain path-addressable workspace MP4
  artifacts, and never enter base64 MCP content at any size. They remain
  discoverable through workspace listing and job-artifact metadata. Readiness
  performs a real one-frame `libx264` encode. Finalization re-decodes the
  complete MP4 and rejects it unless decoded frame count and duration match the
  requested sequence. GPU encoding is added only after
  production measurement identifies CPU encoding as a material bottleneck.
- Atomic job metadata retains the newest 1,000 jobs, matching bounded MCP list
  pagination. Eviction removes only the in-memory/history index; completed
  frames and videos remain confined workspace artifacts.
- Job checkpoints, metadata, frames, and videos live under
  `.printable/jobs/`. Ordinary workspace uploads and typed Blender/OpenSCAD
  output commands cannot mutate that reserved namespace; dedicated internal
  job commands can. Artifacts remain listable; transferable non-video artifacts
  remain readable through MCP. Explicit
  `printable_blender_execute` remains the accepted high-authority escape hatch
  inside the isolated Blender container and can intentionally alter the shared
  workspace, including internal state.

## 2026-07-19 — Preview renders are always artifacts with optional inline content

- `printable_render_preview` always commits a PNG to the confined workspace.
  Inline MCP image content is an additional convenience, never the only copy.
- Inline transfer is limited to 1 MiB of decoded PNG data. This bounds base64
  expansion, synchronous response memory, and gateway traffic. A larger image
  is not rejected: the render succeeds and returns its artifact metadata and
  path for downstream workspace workflows.
- Render work defaults to one hour so common CYCLES jobs have a functional
  starting budget. This is not a maximum; callers may choose any positive
  runtime-representable duration. Serialized admission and transport retain
  their ordinary allowances, so queue time does not consume the render budget.
- The add-on reports exact artifact size and media type. Before adding inline
  content, the server re-reads the confined artifact and verifies its size,
  PNG signature, and requested IHDR dimensions. A detected concurrent size or
  format mismatch fails visibly rather than pairing stale metadata with
  unrelated bytes.
- The independent Blender supervisor watchdog covers first-party still renders
  as well as explicit code, so a stalled native render cannot wedge Blender
  indefinitely.

## 2026-07-19 — Explicit Blender code uses the container as its security boundary

- Every authenticated gateway user may invoke bounded Python on Blender's main
  thread. This is an intentional high-authority modeling capability, not an
  accidental interpreter injection.
- The implementation does not claim to be a Python language sandbox. Its blast
  radius is the dedicated non-root Blender container, private control network,
  and confined workspace. Deployment must not inject credentials, a container
  runtime socket, host PID namespace, privileged mode, or broad host mounts.
- The caller selects a positive, runtime-representable execution
  budget; there is no configured product maximum. Serialized-lane admission
  gets the ordinary request budget and fails before sending when Blender stays
  busy; admitted work then receives its full caller budget plus transport time.
  Python, native, and inherited subprocess stdout and stderr share 64 KiB
  response caps, and structured JSON results are capped at 1 MiB so a response
  cannot exhaust process memory; larger outputs belong in workspace artifacts.
  Execution is synchronous: spawned processes must finish before return, and a
  surviving caller thread restarts Blender. Before caller code runs, Blender
  arms an absolute deadline in the separate supervisor process over an
  inherited pipe. Tracing is
  only a fast path; changing trace state, monopolizing the GIL, entering native
  code, or catching exceptions cannot consume the supervisor deadline. A timeout
  kills and restarts Blender. Unsaved scene state is lost, so callers checkpoint
  first. Response delivery uses the ordinary transport allowance rather than
  converting the work duration into a socket timeout.
- The tool is destructive, non-idempotent, and open-world. Callers checkpoint
  before risky mutations; requests are never retried after bytes are sent.
- A timeout after request delivery has an unknown mutation outcome. Cancellation
  or queue contention can exhaust the caller's response budget while Blender's
  independent execution watchdog still permits the admitted script to finish.
  The script may therefore continue changing the scene until it completes or
  the supervisor restarts Blender. This ambiguity is an accepted limitation of
  the synchronous execution surface. Callers must never retry automatically;
  they wait until Printable reports healthy, then inspect the scene or restore
  the checkpoint before issuing another destructive operation. The dedicated
  container and confined workspace bound the accepted failure domain.
- Caller-defined finalizers can run while Python releases the execution frame,
  after the external watchdog has been disarmed. A hostile finalizer can leave
  Blender unhealthy until the container is restarted. A concurrently admitted
  bridge connection can also be conservatively mistaken for caller-created
  background work and cause an unnecessary Blender restart. These are accepted
  availability limitations, not container escapes. If healthy status does not
  return, the operator restarts the Blender container and the caller restores
  the checkpoint; the timed-out operation is still never retried automatically.

## 2026-07-18 — Capability-first headless production architecture

- Product behavior, reliability, performance, and security define correctness.
- Code that calls Blender's `bpy` API remains Python by necessity, but the
  add-on and headless launcher move directly into this repository as
  first-party product code. There is one authoritative copy.
- The supported architecture uses separate persistent Rust MCP and Blender
  containers on Linux/amd64. Blender receives a supported GPU through the
  NVIDIA runtime non-exclusively. CI and release artifacts target Linux/amd64;
  exact-image qualification is required on the intended GPU host.
- The current length-prefixed TCP protocol remains because it is simple,
  bounded, and already implemented. It is an internal versioned boundary and
  may evolve with product needs.
- Blender attaches only to an internal control network and dedicated
  workspace. Neither container receives a Docker socket, privileged mode,
  broad NAS mount, host PID namespace, public application port, or control
  path to unrelated workloads.
- CI and the opt-in publisher use one release-pair smoke that starts both
  production images on an internal network and shared workspace, then completes
  a durable render through MCP. A digest-pinned pair record cannot be published
  through a weaker standalone-only validation path.
- Main CI moves one mutable `mcp-printable-release:production` discovery pointer
  only after the immutable pair record is verified. Deployment reads both image
  digests from that one record and starts digest-qualified images, avoiding two
  independently floating runtime tags. The local publisher creates immutable
  records but cannot move the production pointer.
- Workspace paths are traversed from a held root descriptor without following
  symlinks. Inputs are copied into private staging before `bpy` opens them;
  outputs are produced in private staging and atomically promoted through a
  held destination-directory descriptor.
- Every authenticated MCP gateway user gets the complete Printable catalog.
  Container/workspace isolation provides the blast radius; there is no
  per-tool user policy matrix.
- GPU pressure queues inside Printable's bounded execution lane or returns a
  visible busy/resource error. Printable never pauses, kills, reprioritizes,
  or reconfigures another workload.
- Validate cutover in an isolated workspace before replacing a deployed
  image set. Determine data compatibility and rollback from that deployment's
  current state; historical assumptions about service usage do not authorize it.
- Long renders and animations use durable jobs. Frames persist in confined
  job directories; progress is queryable; cancellation is cooperative between
  frames; FFmpeg runs as a bounded Printable subprocess; large media returns
  as an artifact rather than base64.

## 2026-07-18 — Bounded single-shot and chunked artifact uploads

Streamable HTTP buffers a request before dispatch, so permitting a full
25 MiB artifact as base64 in one call makes concurrent request memory too
large. The implemented write surface uses two bounded layers:

- `printable_workspace_write` accepts at most one 1 MiB decoded chunk.
- Larger files use `write_begin`, `write_chunk`, and `write_commit`.
- A global in-flight MCP request cap bounds aggregate retained body memory.
- Upload IDs come from a CSPRNG; active uploads and total bytes are bounded;
  idle uploads expire; chunks serialize per upload.
- Commit reuses the workspace crate's confined atomic promotion and does not
  consume the upload until promotion succeeds.

This call shape is intentional product design. It is safer and more reliable
than retaining a large single-request surface.

## 2026-07-16 — OpenSCAD gate is a security property

The source gate has one primary contract: accepted untrusted OpenSCAD must not
read an unsnapshotted caller path or invoke a forbidden file-loading directive.

- Identifiers use the ASCII OpenSCAD grammar. Over-confinement is an acceptable
  failure direction; under-confinement is not.
- Property tests assert that injected paths never survive accepted output and
  that forbidden directives are always rejected.
- A parser-independent assertion checks known injected paths so a shared lexer
  blind spot cannot hide on both sides of a test.
- Cargo fuzz holds the no-panic contract over arbitrary bytes.
- File references are snapshotted through the confined workspace before
  subprocess execution. Untrusted input reaches OpenSCAD through argv arrays,
  never a shell command.

## 2026-07-19 — OpenSCAD workflows retain capacity through artifact commit

- The product surface is compile-to-validated-STL, named-view PNG rendering,
  and caller-selected Z-plane SVG cross-section. These workflows cover useful
  modeling and review without exposing an unrestricted filesystem.
- A process-wide semaphore permit is acquired before import snapshotting and is
  retained through process execution, output validation, and atomic workspace
  commit. Concurrent calls queue without multiplying staged imports, processes,
  geometry validation, or generated-artifact memory beyond the configured
  capacity.
- Literal `import()` and `surface()` paths are copied through the confined
  workspace into immutable private snapshots. Duplicate references share one
  snapshot. Dynamic paths and other file-loading directives fail before the
  subprocess starts.
- OpenSCAD is always invoked with an argv array. Standard output and error are
  drained concurrently and retained up to 64 KiB each. A caller-selected
  positive work budget defaults to one hour and has no configured maximum; on
  expiry the complete subprocess group is killed and the child is reaped before
  capacity is released, including the packaged Xvfb wrapper and descendants.
  Process exit and complete standard-stream drainage share that one budget, so
  a descendant cannot retain capacity by holding an inherited pipe open.
- Inline source is capped at 1 MiB, matching the bounded MCP request path.
  Generated files use the workspace's 25 MiB artifact cap, which is also
  installed as the subprocess's per-file limit before execution. STL is parsed
  and analyzed before commit, PNGs must completely decode at the requested
  dimensions, and cross-sections must be complete well-formed UTF-8 SVG
  documents. Invalid output is never published.
- The container smoke runs all three workflows against packaged OpenSCAD. A
  binary-presence status check alone is not delivery evidence. The standalone
  server-image smoke stops after the generic product workflow because it has no
  Blender control-plane peer; the paired-image smoke supplies a durable Blender
  source and continues through decoded product stills, a gallery, a turntable,
  and independently certified mechanical video.
- The paired-image harness creates a unique disposable workspace. Its direct
  Blender capability probe seeds the public source scene and exercises reserved
  mechanical-analysis output under a fixed smoke job id; the harness removes
  only that probe-owned reserved subtree before server startup so durable
  recovery inspects a clean job store rather than test-fixture debris.
- Product-kit no-argument geometry adapts its nominal spans and heights to the
  explicit form and manufacturing profile. A thick minimum wall may exceed the
  exterior corner radius; in that case the shell uses a square interior corner
  with a facet-aware offset instead of rejecting an otherwise usable profile.
  A starter shell may also thicken beyond the declared minimum wall to retain a
  larger profile edge break; caller-specified incompatible dimensions still
  fail their local constructive assertions.

## 2026-07-23 — OpenSCAD parameters are typed argv, not source interpolation

- Compile, render, and cross-section accept one shared typed definition map and
  optional product variant. Callers do not construct raw `-D` expressions.
- Definition names use the confined source gate's ASCII identifier grammar.
  The `pbl_` prefix is reserved for versioned server-owned product contracts.
- Booleans, finite numbers, bounded UTF-8 strings without control characters,
  and bounded flat numeric vectors serialize deterministically into discrete
  argv entries. Quotes and backslashes are escaped for OpenSCAD's string
  literal grammar; no shell parses them.
- Definition count, vector length, string bytes, variant characters, and total
  serialized argv bytes are bounded before OpenSCAD queue admission. Duplicate
  JSON names fail deserialization instead of silently replacing an earlier
  value.
- Server-generated definition metadata returns sorted names, count, and whether
  a variant was applied; it never echoes definition values. Ordinary OpenSCAD
  compiler diagnostics remain visible to the caller who supplied the source and
  values.
- A cross-section with definitions, a variant, or a design profile first
  compiles the fully defined caller model to a bounded private STL, then
  projects that immutable result. This preserves OpenSCAD's top-level `-D`
  override behavior instead of letting a projection scope shadow source
  defaults. Both subprocesses retain one permit and share one caller work
  budget; no intermediate artifact is promoted. Calls without the new optional
  parameterization retain the original direct-projection resource path and
  response shape.

## 2026-07-16 — Geometry CSG binding is `manifold-csg`

- `manifold-csg` 0.3.3 provides maintained f64 bindings to Manifold for
  booleans, intersection volume, cross-sections, and later repair.
- Default features are disabled to avoid an unnecessary TBB dependency.
- `manifold-csg-sys` builds Manifold from source with cmake, a C++ compiler,
  and git.
- Cube/sphere intersection and analytic-volume assertions were verified on
  both architectures when this binding was selected. Current release support
  is Linux/amd64 only.
- parry3d owns topology flags, BVH distance/raycast/containment, connected
  components, and mass properties. Do not hand-roll geometry kernels already
  provided by these libraries.

## 2026-07-14 — Rust MCP foundation

- Use rmcp 2.x with only the server and streamable-HTTP server features. Stdio
  is not supported.
- Follow the house hand-written `ServerHandler` pattern with public
  `list_tools_payload()` and `invoke_tool()` test shims, static tool metadata,
  and match-based dispatch.
- Derive JSON Schemas from typed schemars parameter structs. Validate bounds
  during deserialization rather than scattering checks through handlers.
- Use one concern per crate: server, Blender client, geometry, OpenSCAD,
  workspace, and imaging.
- Library errors use `thiserror` and stable machine-readable codes. `anyhow`
  is limited to the binary boundary. Logs use `tracing`; backend tracebacks are
  discarded at the client boundary, logs record only that suppression occurred,
  and callers receive only the user-facing add-on error.


## Selective Blender modeling inspection

Use existing Blender Python execution for modeling and compact structured
observations, with discoverable workflow guidance. Add bounded read-only scene,
object, and node topology inspection where transport-side projection avoids
shipping unnecessary data. Code Mode can compose and filter calls, but cannot
recover omitted Blender state or prevent a verbose upstream response by itself.
Keep saved script storage and a broad modeling helper library deferred pending
workload evidence. Bump the paired bridge compatibility version for the new
inspection commands. Public MCP container acceptance exercises the guide's
creation and revision examples and inspects their authored nodes and modifiers.
