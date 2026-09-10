# Blender editing context

Use `inspect` with `action: "editing_state"` to inspect mode, frame, active
object, scene, and view layer. Its `section` selects paginated `editors` or
`selection`; `offset` and `limit` bound the returned list. Editor records provide
the window index, area type, index among areas of that type, available regions,
and dimensions.

`blender_execute` accepts an optional `context` object:

```json
{"area_type": "VIEW_3D", "window": 0, "area_index": 0, "region_type": "WINDOW"}
```

The bridge resolves this selector against current Blender windows and executes
Python inside `bpy.context.temp_override`. Missing windows, editors, or regions
fail before the source runs. Optional `expected_mode` checks the current mode;
it does not change it. Without a selector, execution uses the existing context,
including background execution for data-API operations.

Explicitly targeted execution returns compact `context.before` and
`context.after` summaries. Selection and mode changes made by the source are
ordinary Blender edits. The override restores its temporary UI target on normal
exit or Python failure; it is not rollback for changes made by the source.
Blender itself cannot restore UI handles destroyed by loading a file or removing
the targeted screen. Inspect again after such changes.

Selectors describe the current layout, not persistent editor identities. Reuse
them while that layout is relevant; inspect again after changing screens or
restoring a scene. No fallback silently substitutes another editor. General
Python can configure editor contents and use Blender's native operators without
adding a separate tool for every UI action.

This is the explicit-context portion of the
[product refactor](product-refactor.md). [Native observations](native-observation.md)
reuse these targets and report their view configuration and redraw evidence.
Targeted post-edit observations remain a separate capability.

The [Blender context API](https://docs.blender.org/api/current/bpy.types.Context.html)
defines temporary overrides and their lifetime limits; the
[operator documentation](https://docs.blender.org/api/current/bpy.ops.html)
describes how operators use window, area, and region context.
