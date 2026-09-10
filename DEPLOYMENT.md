# Deployment

**Status:** the paired private stack, dedicated NFS workspace, and governed MCP
gateway catalog are deployed on `server`. The Rust foundation image,
first-party Blender bridge, headless image, and paired image promotion exist in
this repository. Production GPU proof on the RTX 4060 Ti is complete and recorded in
[`evidence/blender-gpu-smoke-2026-07-18.md`](evidence/blender-gpu-smoke-2026-07-18.md).
The remaining delivery sequence is tracked in [`PLAN.md`](PLAN.md).

The target runs on Linux/amd64 `server` as two first-party containers: the Rust
MCP service and a persistent headless Blender 5.2.0 service. This repository
owns both image sources and their compatible release workflow. The Komodo
stack and MCP gateway wiring live in `../docker-home`.

## Target topology

```text
MCP gateway -> printable-server
                    |  OpenSCAD/FFmpeg subprocesses
                    |  /workspace
                    |  private blender_control TCP
                    v
            Blender headless :9876
                    |  /workspace
                    v
             RTX 4060 Ti
```

- No MCP or Blender application port is published on the host.
- Blender joins only the internal `blender_control` network.
- Rust joins `blender_control` and the private gateway network. It does
  not join the public Traefik network and receives no Docker socket.
- Printable requires the one shared gateway bearer on `/mcp`; both services
  obtain it from Infisical at deploy time. No reverse-proxy sidecar is present.
- Both application containers mount the same dedicated confined workspace at
  `/workspace`; no broad NAS root is mounted.
- `/healthz` proves Rust process liveness and never probes dependencies.
  `/readyz` returns `200 ready`, `200 busy`, or `503 blocked` from sanitized
  booleans and stable codes for Blender, the confined workspace, OpenSCAD,
  FFmpeg, and durable recovery. A render that owns Blender's serialized lane is
  healthy busy work rather than a failed probe. Blender container health still
  proves its listener/main-thread pump is alive. `printable_status` remains the
  authenticated diagnostic surface. An overdue command marks the single
  Blender execution lane unhealthy until the main-thread pump becomes
  available again.

## Blender runtime

The Blender image uses the official Linux x64 Blender 5.2.0 archive pinned by
checksum. It starts the in-container supervisor under tini with:

```text
/usr/bin/tini -- /opt/printable/scripts/run-headless-blender
```

The supervisor launches Blender with the background, factory-startup,
auto-execution-disabled, offline flags and `launcher.py`. Blender arms execution
deadlines in the supervisor over an inherited pipe, so recovery does not depend
on the Blender interpreter or GIL. The Linux supervisor is a child subreaper,
and each Blender launch owns a dedicated process group. Watchdog and
forced-shutdown cleanup first kill that group, then kill and reap every adopted
descendant before relaunch. This contains helpers even when caller code creates
a new session or process group. Watchdog exit 75 starts fresh Blender in the
same isolated container; other Blender failures or incomplete descendant
cleanup exit the container for runtime recovery. On container shutdown, the
supervisor applies the configured shutdown grace to cleanup, so caller code
cannot block deployment or host recovery.

An explicit-code timeout is an unknown outcome. Caller-defined finalizers may
run during interpreter frame teardown after the watchdog is disarmed and can
leave the main thread unhealthy; a concurrent bridge connection may instead
cause a conservative, unnecessary Blender restart. If `printable_status` does
not return healthy after the caller budget and restart grace, restart only the
Blender container, confirm healthy status, and inspect or restore the caller's
checkpoint. Never automatically retry the destructive request.

The add-on's TCP thread performs only framing, queueing, and response I/O. An
explicit main-thread loop drains requests and performs all `bpy` work.
Before handlers start, the process applies the 1 GiB artifact cap as an OS file
size limit, so Blender cannot exhaust staging space with an oversized output
before the artifact is validated and promoted. Output parents are created,
descriptor-validated, and held before any Blender mutation or render begins.

Production GPU settings:

```yaml
runtime: nvidia
environment:
  NVIDIA_VISIBLE_DEVICES: all
  NVIDIA_DRIVER_CAPABILITIES: compute,utility,graphics
  PRINTABLE_BLENDER_RENDER_DEVICE: OPTIX
```

GPU access is non-exclusive. Printable queues within its own bounded execution
lane or returns an explicit busy/resource failure; it never manages unrelated
GPU processes or containers.

`render_still` and `render_views` accept an `engine` of `EEVEE` (the default)
or `CYCLES`. Cycles renders accept `samples` from 1 through 4096 and use the
configured CPU or OptiX device. EEVEE rejects `samples` because its sampling
controls are not the Cycles contract. A render accepts a positive caller-selected
`timeout_seconds` work budget, defaults to one hour, and has no configured
maximum. Queueing and response delivery retain the ordinary request allowance.
The Blender supervisor independently enforces the admitted render deadline, so
a stuck native render restarts Blender instead of wedging the service forever.
`bridge_status` reports the configured device and the Cycles devices selected
at startup.

`render_views` runs 1–36 validated view destinations as one serialized request
with one caller-selected budget. The batch is limited to 67,108,864 aggregate
source pixels, which bounds staged files, uncompressed render surface, response
metadata, and workspace exposure. Each RGB8 source is limited to 8,388,608
pixels so it remains within the 25 MiB confined snapshot path used for contact
sheet composition. It frames geometry enabled for rendering in the active view
layer with a temporary orthographic camera, uses temporary area lights only
when the scene has none, restores the prior camera and render settings, and
promotes source PNGs only after all frames render. The supervisor covers the
complete batch. Its response includes evaluated world-space bounds, including
collection instances, for dimensioned review workflows.

`render_diagnostic` creates non-destructive cross-section or printability
heatmap geometry from evaluated dependency-graph instances. It restores source
visibility, camera, lights, and render settings and removes every temporary
mesh/material before promoting its staged PNG. It counts evaluated meshes before
allocating diagnostic materials or BMeshes, then caps vertices, edges, faces,
face loops, and bounded attribute/weight entries copied into BMesh. String
attributes are refused because their payload is not element-count bounded.
Callers can hide unrelated objects or select up to 1,000 source-object names
when a larger scene exceeds that bound.
Cross-section contours are tessellated as one nested set so through-bores and
other hollow profiles remain open and section area excludes their holes. The
supervisor deadline starts before object discovery, mesh preflight, and bounds
measurement, covering the complete variable Blender operation.
Each result includes exact world bounds and section or overhang face/area
analysis. Diagnostics require mesh objects; callers convert, hide, or exclude
other renderable types so resource preflight never allocates a non-mesh
conversion before its size is known.

The public `printable_render_preview` MCP tool always writes the PNG into the
shared confined workspace. It additionally returns PNGs up to 1 MiB as MCP
image content when `include_inline` is true. This is a response-memory and
gateway-transfer bound, not a render-size restriction: larger PNGs succeed and
return their artifact path, media type, dimensions, and size.

`printable_render_gallery`, `printable_render_dimensions`, and
`printable_render_turntable` retain source PNGs in a unique confined batch
directory and persist a labeled contact sheet. `printable_render_cross_section`
and `printable_render_printability_heatmap` retain one unique full-resolution
diagnostic source and persist a labeled result.
`printable_compare_renders` snapshots and decodes two confined PNGs into a
labeled BEFORE/AFTER artifact without using Blender. Composite surfaces are
capped at 8,388,608 pixels so raw RGB plus encoder overhead stays within the
25 MiB generated-artifact commit cap. Gallery and turntable contact sheets
automatically fit within that surface while retaining requested source
resolution. Input decoding is separately capped at 16,777,216 pixels and
64 MiB of decoder allocation. A decoded source is immediately resized to its
fitted tile or panel, and all review-image composition shares
one process-wide memory permit. This bounds aggregate synchronous memory while
leaving high-resolution individual views and `printable_render_preview`
available. Every labeled review image uses the same optional 1 MiB inline-image
contract as preview rendering.

The controlled host proof runs only on `server`, where the NVIDIA runtime and
an unrelated GPU workload are present:

```sh
scripts/smoke-blender-gpu \
  gitea.cacahuate.org/bennight/mcp-printable-blender:sha-<commit>@sha256:<digest>
```

The smoke launches only its own temporary containers. It verifies fail-closed
startup without GPU access, EEVEE and Cycles/OptiX output, observable Blender
GPU memory, continued visibility of a pre-existing GPU process, no `DISPLAY`,
no published port, and no Docker socket in the Blender container. It never
signals, stops, or inspects the owning container of the peer workload.

## Runtime environment

| Variable | Production value | Purpose |
|---|---|---|
| `PRINTABLE_HTTP_HOST` | `0.0.0.0` | Listen inside the private container network. |
| `PRINTABLE_HTTP_PORT` | `8000` | Streamable HTTP MCP and liveness port. |
| `PRINTABLE_MCP_BEARER` | shared Infisical secret | Exact bearer required on `/mcp`; the gateway is the only remote caller. |
| `PRINTABLE_ALLOWED_HOSTS` | actual gateway host forms | DNS-rebinding guard for `/mcp`. |
| `BLENDER_HOST` | `blender` | Internal service DNS name. |
| `BLENDER_PORT` | `9876` | Private bridge port. |
| `PRINTABLE_BLENDER_BIND` | `0.0.0.0` | Listen only inside the private Blender control network. |
| `PRINTABLE_BLENDER_RENDER_DEVICE` | `OPTIX` | Required production render backend. |
| `PRINTABLE_BLENDER_REQUEST_TIMEOUT_SECONDS` | `120` | Ordinary request, queue, and response allowance; caller-selected Blender work time is additive. |
| `PRINTABLE_BLENDER_QUEUE_CAPACITY` | `16` | Bounded main-thread queue. |
| `PRINTABLE_BLENDER_STATE_DIR` | `/run/printable-blender` | Writable tmpfs for health markers. |
| `PRINTABLE_WORKSPACE_ROOT` | `/workspace` | Confined Rust artifact root. |
| `PRINTABLE_BLENDER_WORKSPACE_ROOT` | `/workspace` | Identity path mapping in Blender. |
| `OPENSCAD_BIN` | `/usr/local/bin/openscad-headless` | In-container OpenSCAD wrapper. |
| `PRINTABLE_SCAD_CONCURRENCY` | `2` | Complete concurrent OpenSCAD workflows, from import snapshots through artifact commit. |
| `FFMPEG_BIN` | `/usr/bin/ffmpeg` | Packaged CPU video encoder for durable turntable and animation jobs. |
| `PRINTABLE_RENDER_JOB_QUEUE_DEPTH` | `16` | Process-wide queued plus running durable render jobs. |
| `PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB` | `1024` | Per-call address-space budget for disposable exact assembly CSG. |

All headless bridge variables and defaults are listed in `.env.example`.
Deployment configuration uses the production values above rather than the
loopback and CPU development defaults.

Durable job state and media live under `.printable/jobs/` in the shared
workspace. Ordinary uploads and typed render/OpenSCAD destinations cannot
mutate that namespace; the server job runner and its internal Blender commands
hold the dedicated write capability. The namespace remains mounted in both
containers so Blender can consume immutable checkpoints and promote frames.
New directory entries and final artifact entries are parent-synchronized before
the server reports a durable commit. After logical admission, staging and queue
handoff also continue independently from the submitting MCP request. The
detached continuation owns the bounded source snapshot and retains admission
through the blocking copy, durable handoff, or terminal outcome.
From pre-job checkpoint capture until the live-session restore is durably
recorded, ordinary Blender mutations fail visibly behind an endpoint-wide
recovery fence. `printable_status.render_jobs.recovery_fenced` reports that
condition. Session-restore failures remain nonterminal, retain the latest
failure in job status, and retry without an attempt ceiling using bounded
backoff. The original completed Blender outcome is durable independently, so
these retries restore only the live session and cannot rerender or hide a
deterministic frame failure. A recovered job that cannot persist its transition
to running remains fenced and retries only that commit before contacting
Blender. If restoration succeeds but its metadata write
fails, the worker releases the Blender lane, keeps the fence closed, and
retries only that atomic metadata commit until durable; restart replays
restoration behind the fence.
Cancelling a queued recovery that still owes restoration records the request
without removing its scheduled work. It becomes terminal only after the live
session is restored and the restore marker is durable.
Cancellation of a request awaiting metadata persistence cannot release the
write-ordering guard; the blocking atomic write retains it until completion.
A fence that remains set requires operator inspection of that failure
and the retained pre-job checkpoint; recovery resumes automatically when the
Blender bridge can restore it. Invalid or unreadable recovery metadata sets
`printable_status.render_jobs.recovery_integrity.status` to `blocked` and keeps
the mutation fence closed. New durable submissions are rejected and recovered
job work is not scheduled while this block exists. Repair or remove the corrupt
retained record only after determining whether its pre-job checkpoint must be
restored, then restart Printable to reconstruct recovery state.
The FFmpeg readiness check performs an actual one-frame CPU `libx264` encode,
so deployment smoke fails if the image contains FFmpeg without the required
encoder. Generated MP4s are also fully decoded before publication, with frame
count and duration checked against the durable job sequence. Encoding and that
decode validation share one caller-selected runtime deadline.

## Release and cutover

The Rust and Blender images are built with matching commit-scoped
`sha-<commit>` tags and source-revision/role labels. Before promotion, both
exact digests must be Linux/amd64, run as their expected non-root user, expose
the reviewed health command and entrypoint, carry the expected labels, and
contain no credential-valued environment, label, or build-history assignment.
The fleet-pinned Grype release scans both exact digests under the same
fix-available high/critical and all-CISA-KEV fail-closed policy used for
first-party images. Vendor-tracked findings without a remediation remain
visible in exact-image workflow output with their severity and fix state; they
do not turn a supported upstream image into an unshippable permanent failure
unless CISA identifies active exploitation.
After both CPU image smokes and security gates succeed, the exact published
Blender digest must pass the server-side NVIDIA EEVEE and Cycles/OptiX smoke. A
release-pair record then captures both immutable registry digests. Komodo is
triggered only after the verified record becomes the single
`mcp-printable-release:production` discovery pointer. The deployment resolves
that record's labels to both digest-qualified image references before Compose
starts; the mutable pair pointer is never the compatibility identity. CI
reaches `server` with the shared deployment SSH credentials injected from the
repo-scoped Infisical path; the key and pinned host material are removed from
the runner after validation.

Pull-request CI and the opt-in local publisher invoke the same release-pair
smoke before promotion. It starts both production images on one internal Docker
network with one shared named workspace, requires `/readyz`, then drives the
generic bracket compile/validation/import/product-render workflow. The same
acceptance continues through enclosure and grip presentation and uses the hinge
only for complete-arc clearance certification and mechanical video. Neither
application container receives the Docker socket. Keep `KOMODO_STACK_ID` unset
until the isolated private stack delivered in `docker-home` is ready for
automatic promotion.

The existing Printable runtime is unused, so no compatibility shadow or
parallel writable deployment is maintained. The new stack is first validated
under an isolated Compose project and workspace on `server`; the existing
runtime may then be stopped and the MCP gateway upstream replaced. Every
authenticated gateway user receives the complete Printable tool catalog.

## Trust boundary

`execute_code` is intentionally high authority inside Blender. Its blast
radius is the non-root Blender container, dedicated workspace, internal
network, and assigned GPU access. The container has no host PID namespace,
Docker socket, privileged mode, host application ports, broad mounts, or
outbound network route. Workspace confinement does not make arbitrary Python
safe; container isolation is the boundary. Execution is synchronous: native
and inherited subprocess output is response-bounded, returned code cannot
leave live processes behind, and a surviving caller thread forces Blender
recovery. Caller-defined finalizers remain an accepted availability risk during
interpreter teardown; persistent unhealthy status requires a Blender-container
restart and checkpoint inspection.
