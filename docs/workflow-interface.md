# Workflow interface

The [product refactor](product-refactor.md) defines the final advertised catalog.
The public server advertises exactly thirteen workflow tools. Earlier prefixed
operation names remain internal handler identifiers and are not accepted as
public tool calls. The release smoke exercises the combined workflows over MCP;
deployment and gateway discovery must move together at cutover.
[Native observation](native-observation.md)
is available when the backend runs normal Blender with a private display.
Blender operations accept optional [scene preconditions](scene-state.md) and
return compact scene-state metadata through the shared handlers.
Presented renders support optional [exposure and illumination controls](presentation-controls.md).

## Requests

Combined tools take an `action` and an operation-specific `params` object.
Each action's parameters are derived from the same Rust type used by its handler;
unknown actions, unrelated fields, and wrong parameter types fail before dispatch.
Existing handler-level range, state, and file checks still run before mutation.

| Tool | Actions or direct parameters |
| --- | --- |
| `status` | Optional direct `detail` (default false) |
| `inspect` | `scene`, `object`, `node_tree`, `editing_state` |
| `edit` | `primitive`, `boolean`, `rename`, `rigid_rotation` |
| `blender_execute` | Direct `code`, `timeout_seconds`, optional `context` and `expected_scene` |
| `scene` | `clear`, `checkpoint`, `restore`, `import`, `export` |
| `scad_build` | `mesh`, `image`, `section` |
| `view` | `native`, `dimensions`, `section`, `overhangs` |
| `render` | `scene`, `product`, `gallery`, `turntable` |
| `compare_renders` | Existing direct image-comparison parameters |
| `validate_mesh` | Existing direct mesh-validation parameters |
| `analyze_assembly` | Existing direct assembly-analysis parameters |
| `job` | `submit`, `get`, `list`, `artifacts`, `cancel` |
| `artifact` | `list`, `read`, `write`, `publish`, `upload_begin`, `upload_chunk`, `upload_commit` |

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
