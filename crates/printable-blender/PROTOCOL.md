# Blender bridge wire protocol

The internal production contract between the Rust `printable-blender` client
and the first-party Blender add-on. The existing framing is retained because
it is simple, bounded, and already implemented. Client and add-on changes that
alter it ship together behind an explicit compatibility version.
The current compatibility version is `0.5.0`; it adds selective scene and object
inspection plus paginated material and Geometry Nodes topology. Version `0.4.0`
extended the isolated product presentation contract to multi-view batches and durable jobs. Version `0.3.0`
introduced isolated product still rendering, and version `0.2.0` introduced
the required per-process response identity used by durable mechanical
transactions.

## Transport

- Plain TCP. Interactive/local mode defaults to `127.0.0.1:9876`. Production
  binds inside the Blender container and is reachable only as `blender:9876`
  on the private internal control network; no host port is published.
- **One fresh connection per command.** The client connects, sends one request,
  reads one response, and closes. This lets multiple MCP clients share one
  Blender bridge without any of them holding the socket open. The client does
  not pool or reuse connections.
- Access is serialized process-wide: the addon executes commands on Blender's
  single main thread, so the client holds one lock across the connect→send→recv
  exchange (and across a multi-command `transaction`).

## Framing

Length-prefixed JSON:

```
+-----------------------------+------------------------------+
| 4-byte length (big-endian)  | UTF-8 JSON payload (N bytes)  |
+-----------------------------+------------------------------+
```

- The length prefix is an unsigned 32-bit big-endian integer giving the byte
  length of the JSON payload that follows.
- The payload is a single UTF-8 JSON object.
- **Inbound frame cap:** the client bounds the declared length it will read
  (default 64 MiB) and errors on anything larger, rather than attempting an
  unbounded allocation on a malformed/hostile length. (The addon caps its own
  inbound frames at 50 MiB; the headroom preserves a bounded compatibility
  margin for structured command results.)

## Request

```json
{ "id": "<uuid>", "command": "<name>", "params": { ... } }
```

- `id`: a fresh UUID string per request. The addon echoes it on the response;
  the client rejects a response whose `id` does not match as a protocol error
  (the envelope is not the answer to this request).
- `command`: one of the registered handler names (see Commands).
- `params`: a JSON object of command arguments (`{}` when none).

## Response

Success:

```json
{ "id": "<uuid>", "status": "success", "result": <any>, "addon_version": "X.Y.Z",
  "bridge_instance_id": "<process-uuid>" }
```

Error:

```json
{ "id": "<uuid>", "status": "error", "error": "<message>",
  "traceback": "<python traceback>", "addon_version": "X.Y.Z",
  "bridge_instance_id": "<process-uuid>" }
```

- `status` is `"success"` or `"error"`.
- On success, `result` carries the command's return value (shape is
  per-command; most tool passthroughs serialize it verbatim). Any JSON value is
  a legal result, **including `null`** — a present `"result": null` is accepted
  as `null`. Only a success envelope that *omits* the `result` key is malformed;
  the client treats that (absent key, not a null value) as a protocol error.
- On error, `error` is the human message and `traceback` (optional) is the full
  Python traceback. The client discards traceback contents at the protocol
  boundary, logs only that suppression occurred, and surfaces `error` to the
  caller.
- `addon_version` is the addon's `bl_info` version, stamped on **every**
  response (success, error, and timeout). See Version handshake.
- `bridge_instance_id` is a required UUID generated once when the bridge
  process imports the protocol module and stamped on every response. A client
  transaction pins the first identity it observes and fails if a later exchange
  reports a different identity, so connect-per-command traffic cannot silently
  cross a Blender restart.
- Queue/request timeouts return `status: "error"` with a useful `error`. An
  `execute_code` execution deadline closes the connection by terminating the
  Blender process; the in-container supervisor then clears stale health markers
  and starts a fresh Blender process.

## Version handshake

The container/server and the installed addon version independently. The addon
stamps its version in every response; the client records it on first contact
and, once, computes a mismatch warning comparing it against the client's own
compatibility version. The production client always configures the current
compatibility version. It observes `addon_version` before validating the rest
of the typed response, so a legacy envelope missing a newly required field can
still produce a useful mismatch warning through `printable_status`. The warning
is surfaced exactly once. A `None`/absent `addon_version` means the addon
predates version reporting and is itself a warning condition.

## Deadlines

The client carries a total per-command budget and derives the remaining time
before each phase (connect, send, recv), setting it as the socket timeout so a
stall in any phase fails within budget rather than hanging. Default budget 120 s
per command; the readiness probe used by `printable_status` uses a short budget
(5 s). `execute_code`, `render_still`, `render_product`, `render_views`,
`render_diagnostic`, and the internal `job_render_*`,
`job_measure_sequence_bounds`, `job_save_checkpoint`, and
`job_restore_checkpoint` commands first acquire the
serialized lane within the ordinary budget and fail without sending if Blender
remains busy. After admission each starts a fresh deadline containing the
complete caller work budget plus the ordinary transport budget. Once the
handler completes, response delivery uses the ordinary transport allowance;
very large work budgets are never converted into socket timeout values.

The per-endpoint client state also carries a durable-recovery fence. Ordinary
commands check it before waiting and again after acquiring the serialized lane,
then fail before opening a connection while a job's pre-session restore is not
durably committed.
`bridge_status` remains available for diagnosis. Only the durable job worker
uses the recovery transaction that can cross this fence; restart reconstruction
sets the fence before recovered work is scheduled.

## Retry

Exactly one automatic retry, and only when connection **establishment** fails
(refused/reset) **before any request bytes are written**. Once a request frame
has been sent, the client never retries — commands such as `execute_code` and
`boolean` are not idempotent, and a resend could double-apply a mutation.
A timeout after delivery is an unknown outcome: cancellation or queue
contention can exhaust the response budget before Blender's independent
execution watchdog ends. The admitted script may continue mutating until it
finishes or Blender restarts. Callers wait for a healthy status and inspect the
scene or restore a checkpoint before continuing; they never automatically
retry the timed-out operation.

## Commands

The first-party registry in `addon/printable_bridge/handlers.py` is
authoritative. The executable background smoke exercises its delivered
modeling and file capabilities: `bridge_status`, `get_scene_info`,
`get_object_info`, `get_node_tree_info`, `clear_scene`, `create_primitive`, `boolean`,
`rename_object`, `animate_rotation`, `restore_checkpoint`, `export_stl`,
`import_stl`, `save_blend`, `render_still`, `render_product`, `render_views`,
`render_diagnostic`, and `execute_code`.

`render_still` accepts `path`, optional `width` and `height`, and an optional
`engine` (`EEVEE`, the default, or `CYCLES`). Cycles additionally accepts
`samples` from 1 through 4096. Its positive caller-selected `timeout_seconds`
defaults to one hour and has no configured maximum. The full work budget begins
after serialized-lane admission and is enforced by the independent supervisor
watchdog. The result reports the Blender engine identifier, configured render
device, graphics backend when Blender exposes it, applied sample count,
dimensions, artifact path, `image/png` media type, and exact artifact size.

`render_product` accepts a confined PNG path, one or more selected source
object names, and a typed `engineering`, `studio_neutral`, or `studio_dark`
presentation. It renders complete evaluated instances of those source objects
in a disposable scene, frames their world-space bounds with a fixed 15%
margin, and reports effective camera, lighting, color management, ground,
materials, shading, and instance count. Existing materials are preserved;
material-less meshes receive a neutral fallback and explicit overrides win.
Smooth-by-angle changes copied presentation geometry only, and no bevel
modifier is introduced. Cleanup and source-state equality are verified before
the staged artifact is promoted. The result includes the PNG's SHA-256 digest
so the server can reject a destination replaced before its confined snapshot.
Width and height default to 1024×768. Engine, samples, work budget, device,
graphics, and artifact metadata follow the `render_still` contract.

`render_views` accepts 1–36 entries containing a unique confined `.png` path,
label, and non-zero three-component camera direction, plus the same engine,
quality, dimensions, and caller-budget fields as `render_still`. The aggregate
surface is capped at 67,108,864 pixels to bound staged files and uncompressed
render exposure. Each view is capped at 8,388,608 pixels and encoded as RGB8 so
its worst-case PNG remains within the server's 25 MiB confined snapshot path.
Blender validates and stages every path before scene mutation, uses one
watchdog deadline for the batch, frames geometry enabled for rendering in the
active view layer with a temporary orthographic camera, and restores the
original scene camera and render settings. No view is promoted until every
frame has rendered and scene cleanup succeeds. The result reports validated
metadata for each source PNG plus the common backend, device, graphics, and
sample metadata. The same response includes evaluated world-space bounds used
by dimensioned review workflows.
When `presentation` is omitted, this behavior and response remain unchanged.
When it is present, the batch uses the same typed profiles, evaluated-geometry
copying, material precedence, application-handler isolation, and cleanup checks
as `render_product`; each direction becomes the
profile camera view and the response reports the effective presentation for
every image. Grounded profiles reject below-ground directions before scene
mutation.

Publication starts only after every view renders and scene cleanup succeeds.
Each presented view uses create-only atomic publication. A filesystem failure
or destination conflict during publication can leave earlier views in place;
the error reports the partial outcome. Inspect the requested paths before a
retry. Published paths are not removed because another writer may replace them.

Durable jobs use `job_save_checkpoint`, `job_restore_checkpoint`,
`job_render_still`, `job_render_frame`, `job_render_views`,
`job_render_product`, and `job_measure_sequence_bounds`. No MCP tool dispatches
these command names, and deployment exposes the bridge only on the private
control network. Render paths must be under
`.printable/jobs/`. `job_save_checkpoint` copies the live Blender session to a
reserved durable checkpoint before the job source is opened. The worker uses
`job_restore_checkpoint` both to load the immutable source and to restore the
pre-job live session before releasing Blender's serialized lane. Once that
session restoration is durably recorded, restart recovery performs only
post-Blender work and never dispatches another internal job command. Their
public counterparts reject that reserved namespace, so ordinary typed
file/render tools cannot replace immutable job checkpoints, metadata, frames,
or videos.
The internal commands share the same render behavior and caller-selected
budgets; `job_render_frame` and `job_render_product` can select one Blender
timeline frame. Product job frames can use the profile camera, preserve a
copied authored camera, or frame validated caller-supplied world bounds.
`job_restore_checkpoint` additionally reports the restored scene's integer
current frame so the server can measure and persist one static framing envelope
for presented stills and turntables before rendering.
When a studio ground is enabled, a preserved camera at or below that ground
fails before rendering rather than producing an occluded success.
`job_measure_sequence_bounds` evaluates an inclusive stepped timeline under a
separate positive budget, suspends application handlers during every frame
change, restores the original frame, and returns one strong world-space bounds
record. Durable mechanical jobs never sample presentation frames for
certification or framing: the server certifies the original fixed and moving
geometry first, then supplies a conservative complete-rotation envelope and
disables the presentation ground plane.
Read and list operations remain available for job artifacts. Explicit
`execute_code` retains its documented high authority and is not a workspace
sandbox.

`render_diagnostic` accepts one confined `.png` path, mode `cross_section` or
`overhang`, optional unique source-object names, a non-zero camera direction,
and the same engine, quality, dimensions, and caller-budget fields as
`render_still`. Cross-section mode accepts a world axis and optional position
strictly inside the selected bounds. Overhang mode accepts a non-zero build
direction and a threshold from 0 through 90 degrees. The handler copies
dependency-graph instances through transient BMeshes into one temporary mesh;
source objects,
materials, visibility, camera, lights, and render settings are restored before
the staged output is promoted. Evaluated meshes are counted before diagnostic
materials or BMeshes are allocated. Preflight caps the vertices, edges, faces,
face loops, and bounded attribute/weight values copied by `BMesh.from_mesh`;
string attributes are refused because their payload is not element-count
bounded. Hiding unrelated objects or passing an object subset keeps larger
scenes functional. Subset discovery retains only the requested names; selected
instances are processed through one transient BMesh at a time and assembled
into one diagnostic Mesh/Object pair. Cross-section cut contours are tessellated
together so nested contours remain holes and are excluded from section area.
Expanded cross-section vertex, edge, face, and loop totals are checked before
combined buffers are retained. Direction vectors are normalized with an
overflow-safe magnitude calculation before render state is changed.
Returned tessellation indices must be integers inside the flattened input-contour
range before they are mapped back to cut vertices. Coincident coordinates in
disconnected topology are therefore never joined by coordinate matching.
The supervisor deadline is armed before object discovery, preflight, and bounds
measurement, covering every variable Blender scan. Orientation-reversing
instance transforms have their face winding corrected before normal classification.
Finite transforms are rejected as singular only when their determinant is zero.
Diagnostics require mesh objects; callers convert, hide, or exclude other
renderable types so preflight never allocates a non-mesh conversion before its
size is known.
Results report source and rendered world bounds plus exact section face/area or
supported, warning, and severe overhang face/area metadata. Consumers validate
that heatmap bounds match their source and that cross-section bounds stay inside
the source on the retained side of the cut plane.

Delivered scene/state commands are `get_scene_info`, `get_object_info`,
`get_node_tree_info`, `clear_scene`, `restore_checkpoint`, and `rename_object`. Delivered modeling,
animation, render, and file commands are `create_primitive`, `boolean`,
`animate_rotation`, `render_still`, `render_product`, `render_views`,
`render_diagnostic`, `export_stl`, `import_stl`, `save_blend`, and
`execute_code`.

`animate_rotation` accepts one to 1000 unique object names with no existing
parent or children, object animation data, constraints, rigid-body simulation,
or rigid-body constraint, a unique controller name, a three-component
world-space pivot, a finite non-zero right-hand-rule axis, a positive angle in
degrees, and start/end timeline frames. It creates an Empty pivot controller,
preserves each target's world matrix using the controller's actual inverse
transform, and inserts linear axis-angle keys. Competing target motion and every
invalid input fail before mutation. Partial keyframe failure rolls back the
parent links, transforms, timeline frame, interpolation preference, and
controller.

`execute_code` accepts explicit Python source and a caller-selected positive,
runtime-representable execution budget (default 120 seconds, no configured
maximum).
The bridge adds its ordinary request budget for queueing and transport, so the
work budget is not consumed before Blender begins. The namespace provides
`bpy`, `workspace_root`, and a `result` variable. The response returns `result`
as finite JSON capped at 1 MiB, captures Python, descriptor-level native, and
inherited subprocess stdout and stderr at 64 KiB each with truncation flags,
and reports elapsed milliseconds. Larger outputs belong in workspace artifacts.
Execution is synchronous: spawned processes must finish before the source
returns, and a surviving caller thread restarts Blender rather than escaping the
caller budget. It is intentionally a high-authority operation inside the
isolated Blender container, not a Python language sandbox. Before caller code
runs, Blender arms an absolute deadline in the separate supervisor process over
an inherited pipe. Tracing is only a fast path: trace changes, GIL starvation,
caught exceptions, and native calls cannot consume the supervisor deadline. A
timeout restarts Blender and loses unsaved scene state; callers checkpoint
before risky or long-running scripts.

Additional bridge commands become part of this contract only when added to the
first-party registry.

`printable_status` is a server-side tool, not an addon command — it probes via
`bridge_status` and combines the add-on envelope version with Blender and
render-device details from the result.


## Selective modeling inspection

`get_scene_info` accepts optional `name_contains`, `object_type`, `collection`,
and `include_transforms` filters. Defaults preserve the existing summary.
Name matching is case-insensitive; collection membership is direct. The cursor
tracks source scene order, scanning at most 10,000 objects per request, so an
empty filtered page can still have a continuation. Total count is scene-wide.

`get_object_info` adds `section` (`summary`, `materials`, `modifiers`, or
`hierarchy`). `get_node_tree_info` accepts a material or geometry group `name`,
`kind` (`material` or `geometry`), and `section` (`nodes` or `links`). Detail
pages default to 20 entries, accept up to 100, and return `items`, `total`, and
`next_offset`. Hierarchy returns separate child and collection pages. Inspection
is read-only and does not recursively expand groups or evaluate geometry.
The MCP workflow resource is `printable://modeling/blender-v1`.
