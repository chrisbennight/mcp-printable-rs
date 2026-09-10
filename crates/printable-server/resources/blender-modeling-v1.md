# Blender modeling workflow

Use `blender_execute` to create and revise geometry with Blender's
`bpy`, `bmesh`, and `mathutils` APIs. Curves, bevels, arrays, custom meshes,
materials, and Geometry Nodes are available through Python. Execute the code
through the tool; the user does not need to paste it into Blender. Code Mode
can compose these calls and filter their results, while `bpy` runs inside the
Blender container. No external asset service is required.

Combined tools put operation options inside `params` beside their `action`.
`blender_execute` takes its code and execution options directly.

## Inspect only what the next edit needs

Start with `inspect` with `action: "scene"`, for example:

```json
{"action":"scene","params":{"name_contains":"Handle","object_type":"MESH","include_transforms":false,"limit":20}}
```

Name matching is a case-insensitive literal substring. Type and collection
names are exact. A collection filter matches direct membership, not child
collections, and only objects in the active scene are searched. `object_count`
is the whole scene count, not the number matching the filters. `next_offset`
is a cursor in scene order before filtering: reuse it with the same filters.
A scan can return an empty page with a continuation; only a null continuation
means it reached the end. Pages are live observations, not a snapshot across
concurrent edits. Restart inspection after changing the scene.

Use `inspect` with `action: "object"` with `section` set to `summary`, `materials`,
`modifiers`, or `hierarchy`. Summary preserves transforms, dimensions, and base
mesh counts. Other sections take `offset` and `limit` (default 20, maximum
100). Hierarchy has separate `children` and `collections` pages; follow their
continuations independently. Material slots identify materials; Geometry Nodes
modifiers identify their node groups. Modifier details identify the stack and
viewport/render enablement without dumping every property.

Use `inspect` with `action: "node_tree"` with a material name (`kind: "material"`) or a
Geometry Nodes group name (`kind: "geometry"`). Read `section: "nodes"` for
names, types, mute state, active outputs, and nested group references; read
`section: "links"` for connections and socket identifiers. Each section has
independent pagination. Groups are not expanded recursively. This is authored
topology, not evaluated geometry or an assertion that every node is connected.
For a particular value, execute a small targeted query, such as:

```python
obj = bpy.data.objects["Handle"]
result = {"bevel_width": obj.modifiers["EdgeBreak"].width}
```

Use returned identities rather than guessing node names or socket positions.
Never clear an existing material graph just to inspect or change one input.

## Build and revise an editable part

Checkpoint existing work with `scene` with `action: "checkpoint"` before a risky
edit. The following example creates one beveled handle without clearing the
scene. Coordinates are explicit millimetre-valued model coordinates; confirm
the intended units before combining with an existing scene.

```python
name = "Handle"
if bpy.data.objects.get(name) is not None:
    raise ValueError("Handle already exists; inspect it before editing")
bpy.ops.mesh.primitive_cube_add(size=1)
obj = bpy.context.object
obj.name = name
obj.dimensions = (80, 18, 12)
bpy.ops.object.transform_apply(location=False, rotation=False, scale=True)
bevel = obj.modifiers.new("EdgeBreak", "BEVEL")
bevel.width = 2
bevel.segments = 4
bpy.context.view_layer.update()
result = {"object": obj.name, "dimensions": list(obj.dimensions),
          "bevel_width": bevel.width, "bevel_segments": bevel.segments}
```

Revise only the intended parameter, then return the observed value:

```python
obj = bpy.data.objects["Handle"]
bevel = obj.modifiers["EdgeBreak"]
bevel.width = 3
bpy.context.view_layer.update()
result = {"object": obj.name, "bevel_width": bevel.width}
```

The execution namespace is fresh each call; scene objects persist, local Python
variables do not. Keep related operations in one bounded script. Set `result`
to concise finite JSON containing affected names, measured values, and checks
actually performed. Avoid printing mesh arrays, every object, or every node
property. Large reports belong in workspace JSON artifacts; return their paths
and publish them with `artifact` with `action: "publish"` when delivery is needed.
The execution response limits protect runtime memory, not a target context
budget. A script's own observations are not independent print certification.

## Review and deliver

Return targeted measurements in the edit's `result`. For optional visual
feedback, call `view` with `action: "native"` and copy the edit response's
`scene_state` into `params.expected_scene`. Code Mode can compose both calls and
return only the observations needed for the next decision. If another edit
intervenes, the view rejects the stale expectation; inspect the new state before
continuing rather than replaying the mutation. Use `inspect` with
`action: "editing_state"` to select an available editor. Choose viewport drawing
for controlled viewpoints or editor capture for actual editor overlays; read
the returned fidelity and convergence metadata.

Render the selected part with `render` with `action: "product"` using the engineering
profile, then use a studio profile or `render` with `action: "gallery"` when judging
shape and materials from several angles. Inspect a modest preview after a
meaningful geometry/material change, and request a close view when details are
unclear. A successful tool response does not establish visual quality. Reuse
structured observations when they already answer a numerical question instead
of rereading the whole scene after every edit.

Save an editable `.blend` with `scene` with `action: "checkpoint"`. Export intended mesh
objects with `scene` with `action: "export"`, then validate the exported artifact with
`validate_mesh`. Appearance, modifier settings, and base mesh counts
do not certify the evaluated/exported solid. Assembly clearance requires its
separate analysis workflow. Render smoothing must not substitute for actual
modeled edge breaks.

On an execution timeout after delivery, do not retry the mutation. Wait for
healthy status and inspect the scene or restore the checkpoint. A watchdog
restart loses unsaved scene state. Long renders belong in durable render jobs.
