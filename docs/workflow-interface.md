# Workflow interface

The [product refactor](product-refactor.md) defines the final advertised catalog.
The public server advertises fifteen workflow tools. Earlier prefixed
operation names remain internal handler identifiers and are not accepted as
public tool calls. The release smoke exercises the combined workflows over MCP;
deployment and gateway discovery must move together at cutover.
[Native observation](native-observation.md)
is available when the backend runs normal Blender with a private display.
Blender operations accept optional [scene preconditions](scene-state.md) and
return compact scene-state metadata through the shared handlers.
Presented renders support optional [exposure and illumination controls](presentation-controls.md).

## Requests

For on-demand contract discovery, read `printable://contracts` for the compact
tool/action index, then `printable://contracts/{tool}/{action}` for one action
or `printable://contracts/{tool}` for a direct-parameter tool. For example,
`printable://contracts/view/section` describes cutaway requests without loading
native viewport, dimensions, or overhang parameters. Input schemas are derived
from the advertised tool schemas and include only their reachable definitions.
The response explicitly identifies its output schema as tool-wide; it does not
pretend that the current output schema is action-specific. Contract resources
do not invoke operations or grant authorization, and whole-tool annotations
remain whole-tool annotations. Direct MCP and gateway Code Mode continue to
invoke the advertised tool names.

Combined tools take an `action` and an operation-specific `params` object.
Each action's parameters are derived from the same Rust type used by its handler;
unknown actions, unrelated fields, and wrong parameter types fail before dispatch.
Existing handler-level range, state, and file checks still run before mutation.

| Tool | Actions or direct parameters |
| --- | --- |
| `status` | Optional direct `detail` (default false) |
| `inspect` | `scene`, `object`, `node_tree`, `editing_state`, `dependencies` |
| `edit` | `primitive`, `boolean`, `rename`, `rigid_rotation` |
| `blender_execute` | Direct `code`, `timeout_seconds`, optional `context` and `expected_scene` |
| `scene` | `open_project`, `attach_cad`, `clear`, `checkpoint`, `restore`, `import`, `export` |
| `scad_build` | `mesh`, `image`, `section` |
| `cad_build` | `model`, `import_step` |
| `view` | `native`, `dimensions`, `section`, `overhangs` |
| `render` | `scene`, `product`, `gallery`, `turntable` |
| `compare_renders` | Existing direct image-comparison parameters |
| `validate_mesh` | Existing direct mesh-validation parameters |
| `analyze_assembly` | Existing direct assembly-analysis parameters |
| `job` | `submit`, `get`, `list`, `artifacts`, `cancel` |
| `project` | `create`, `get`, `list`, `resolve`, `files`, `export_files` |
| `artifact` | `stat`, `list`, `read`, `write`, `publish`, `ingest`, `transfer_status`, `upload_begin`, `upload_chunk`, `upload_commit` |

For example, a concise object search is:

```json
{
  "action": "scene",
  "params": {"name_contains": "Bracket", "include_transforms": false, "limit": 10}
}
```

Pass that request to `inspect`. Follow its existing `next_offset` behavior,
including after an empty page. Saving a recoverable scene uses `scene` with
`action: "checkpoint"` and `params: {"path": "design.blend"}`. Import and export
currently support STL; consolidating their names does not imply new formats.

[Editing context](editor-context.md) describes paginated editor and selection
inspection, explicit Python UI targets, and their lifetime semantics.

Job identifiers refer to render execution. Upload identifiers refer to staged
file transfer. Neither tool accepts the other's lifecycle actions. Mixed-action
tools use conservative mutating annotations for the whole tool; selecting a
read action does not create a separate MCP authorization boundary.

[Render-worker isolation](render-worker.md) describes checkpoint execution and
the migration from temporary live-session rendering to a separate worker.

## Results and delivery

[Project CAD builds](cad-build.md) retain source inputs, assembly structure and
numerical reports without replacing the live Blender scene.

Use `artifact` with `action: "stat"` to inspect one artifact without reading its
contents, copying a snapshot, or enumerating a directory. Pass `params.path`
as a workspace-relative path, or add `params.project_id` to resolve it within
that existing project. The response retains the normalized workspace `path`,
size, media type, and modification time; project-scoped requests also return
`project_id` and `project_path`. This works for supported large files and videos
without the base64 read limit. It does not contact a modeling backend.

The `identity: "mutable_path"` marker is deliberate: metadata describes the file
at the time of inspection, not immutable bytes or a promise about later reads.
Use publication when a preserved byte snapshot is required. Missing files,
unsupported types, symlinks, non-regular files, and invalid project paths fail
instead of being reported as empty artifacts. A directory listing remains the
operation for discovering unknown filenames.

Targeted post-edit feedback uses the existing composition boundary: return
measurements from `blender_execute`, then optionally call `view` against that
response's `scene_state` using `expected_scene`. Code Mode can perform the loop
and filter its output. A stale observation fails before capture and never
causes the edit to be retried. No additional tool or duplicate job/transfer
lifecycle is needed for this workflow.

Tool results carry JSON objects in `structuredContent` alongside the existing
text representation. Workspace listings wrap their array under `entries` in
both representations. Other
results retain their existing object shape. Clients and Code Mode can consume the typed
JSON value without parsing JSON inside text. Workflow discovery includes output
schemas for artifact metadata, scene identity, progress, and each tool's result
shape. Additional operation-specific evidence remains available in those objects.
Text uses compact JSON; a host may still count both representations. Actual
context usage must be measured in the consuming agent.

Routine `job` queries use `action: "get"` with `params: {"job_id": "..."}`.
They return progress, failure, cancellation, execution, and recovery information
without repeating the saved specification or full mechanical report. Add
`detail: true` inside `params` to retrieve the complete retained record, including
source identity and certification evidence. `status` likewise defaults to
readiness and capability information; direct `detail: true` includes backend
command/device inventory and runtime configuration metadata. Compact responses
do not remove the detailed retrieval path.

Execution failures carry `isError: true` and matching structured/text error
objects with the invoked tool name and existing machine-readable error code.
Unknown tool names remain protocol errors. An execution timeout with an unknown
mutation outcome must not be retried automatically.

Artifact publication retains the existing governed handoff: use `artifact`
with `action: "publish"` and a confined path. The result contains immutable file
metadata; raw bytes flow through `files/authorizeDownload` and its short-lived,
one-use download authority. Renaming the invocation does not bypass that
capability check, copy video into base64, or change snapshot identity.

Rendering retains bounded inline PNG content alongside artifact paths. The
`render` scene action uses the same image preparation as the legacy preview
tool. Product/diagnostic preservation and mechanical-certification requirements
remain those of the underlying delivered operations.

## Incoming files

Upload and download authorization share `PRINTABLE_DOWNLOAD_BASE_URL`, including
its HTTPS scheme and reverse-proxy path prefix. The existing setting name is
retained. Without a configured base, both use the request's allowed HTTP Host;
forwarded headers do not choose the transfer destination.

Use `artifact` with `action: "ingest"` and `params` containing `file` (a
gateway file URI), `path` (the workspace destination), and optional `overwrite`.
The file field advertises native upload forwarding. The gateway transfers bytes
through `files/authorizeUpload` and the authorized HTTP PUT before dispatching
the artifact request. Bytes do not pass through tool arguments.

Printable stages at most two incoming files, each bounded by
`PRINTABLE_FILE_UPLOAD_MAX_MIB` (default 1024 MiB). It checks the declared size
and SHA-256 digest before atomic workspace publication. STEP/STP and Python
source are accepted as stored artifacts; this alone does not execute or convert
them. The existing base64 chunk operations retain their smaller limits.

An authorized transfer expires after one hour; an idle body times out after one
minute. The private Printable URI can be queried with `transfer_status` and
`params.uri`. A successful ingest retains a receipt for the authorization
lifetime; repeating the same ingest returns that receipt without writing again.
It records the original commit, not a claim that another request has not since
changed the destination. Receipts are in memory and bounded; after server
restart or expiry, inspect the destination before authorizing a fresh upload.

A filesystem publication error leaves the receipt `commit_uncertain`, with its
destination path. The destination may already exist even though publication
reported an error. This receipt cannot write again: inspect the destination
before deciding whether to authorize another transfer. This conservative outcome
also applies when a filesystem error occurred before publication. Successful
receipts still return the original result without writing again.

## Shared projects

Use the `project` tool with `action: "create"` and `params` containing
`project_id`, `name`, and optional `description`. Choose a stable identifier such
as `sensor-enclosure`. Repeating the same creation returns the existing project;
conflicting metadata returns an error instead of replacing it. The `get` and
`list` actions discover projects after a server restart.

If a directory already exists without project metadata, creation requires
`adopt_existing: true` to deliberately associate its files with the project.
A failed metadata write can leave the newly reserved directory behind; inspect
it before retrying with adoption.

Each result contains a workspace-relative `root`, such as
`projects/sensor-enclosure`. The `resolve` action takes `project_id` and a
relative artifact `path`, for example `source/enclosure.scad`, and returns the
workspace path accepted by artifact, OpenSCAD, and Blender tools. The `files`
action lists a project's artifacts. Listings are bounded and report when the
result limit is reached; get/resolve remain available by identifier.

Project metadata is server-owned under `.printable/projects`. Existing files
outside projects remain available. Projects organize shared storage; they do not
isolate scripts that can access the shared volume. Creating or resolving a
project does not select or replace Blender's live scene. Scene binding and
CadQuery modeling use their separate workflow actions.

### Selected file bundles

Before collecting a Blender project, `inspect.dependencies` accepts `project_id`,
the latest `expected_scene`, and optional `offset`/`limit`. It inspects the bound
scene without changing it. Follow `next_offset` with the same scene observation.
The result includes Blender version, scene unit settings and registered external
file references. References within the project have a `project_path` and
`state: "requires_snapshot"`; other references are marked `external` without
returning their absolute paths. Existence, safe containment through symlinks,
and packed contents are not inferred from this metadata.

The inventory uses [Blender's native file-reference list](https://docs.blender.org/api/current/bpy.utils.html#bpy.utils.blend_paths),
including linked libraries and excluding packed data. It does not inspect
arbitrary script, driver, add-on or network dependencies. Sequences and caches
can require further native preparation. These limitations remain in the result;
an empty list is not a general portability certificate.

`project.export_files` takes `project_id`, an explicit `files` array of
project-relative paths, and a new `.zip` `output_path` in the same project.
It preserves the selected hierarchy under `files/` and writes `manifest.json`
with paths, sizes, media types, and SHA-256 hashes. Sources are copied into
confined snapshots and checked for changes before the archive is assembled.
An existing output is never overwritten. The selection is limited to 256 files
and 1 GiB total source bytes; bytes are streamed rather than buffered in chat.

This action exports exactly the selected files, not a verified portable native
project. Its manifest reports `scope: "selected_files"` and
`dependencies_inspected: false`. It does not discover or pack Blender external
libraries, textures, caches, OpenSCAD imports, or CAD dependencies. Include the
necessary sources, inputs and settings deliberately; native dependency packing
requires a separate verified preparation step.

Hidden paths, common credential filenames, traversal, symlinks, duplicate
selections and the output itself are rejected. Nothing is collected from other
projects, server metadata, or environment configuration. File contents are not
a secret-detection boundary: do not select files containing credentials, private
service configuration, or transient transfer grants for delivery.

Use the returned artifact path with `artifact.publish` for existing governed
download or service-to-service delivery. Export does not render again, sign S3
URLs, embed credentials, or initiate printing.
