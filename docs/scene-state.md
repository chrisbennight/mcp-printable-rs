# Scene state and stale edits

Blender observations and synchronous modeling results carry a compact
`scene_state` object containing `generation` and `revision`. Status exposes it
under `blender.scene_state`; visual composites retain the state of their source
render. A missing state from an older backend is not a valid precondition.

Agents can pass that object as `expected_scene` in Blender inspection, editing,
Python execution, scene checkpoint/import/export, and synchronous rendering
parameters. The workflow tools reuse the same parameter types. Blender evaluates
pending scene updates and checks the expectation on its main thread before the
requested handler runs. A mismatch returns `stale_scene_state` and the current
state without entering the handler. Refresh the relevant observation before
deciding whether the edit still makes sense.

Generation changes on clear, checkpoint replacement, file load, and process
restart. Revision advances before an operation that may mutate the model, so a
failed operation can invalidate an earlier expectation even when it ultimately
changed nothing. General Python is conservatively treated as mutation. Successful
commands complete dependency evaluation before returning their state.

Persistent Blender callbacks also observe file loads and dependency updates
outside managed commands. The revision is an optimistic concurrency marker,
not a geometry hash or transactional rollback guarantee. A timeout can have an
unknown mutation outcome; inspect or recover before retrying. Arbitrary Python
must still finish its background work before returning and must not disable the
bridge's state observers.

This contract protects model assumptions without returning a full scene dump.
It does not replace artifact digests or manufacturing certificates. Durable
render checkpoint binding is delivered with worker isolation.
[Editor context selection](editor-context.md) provides explicit Python targets.
[Native observations](native-observation.md) carry distinct view configuration
identity alongside model state. Targeted post-edit observations remain part of
the [complete product refactor](product-refactor.md).

The [Blender application handler API](https://docs.blender.org/api/current/bpy.app.handlers.html)
defines the persistent file-load and dependency-update callbacks used here.
The integration corpus verifies stale rejection and new generation after restore
through the actual bridge; unit and MCP handler tests exercise parameter
forwarding and structured error metadata.
