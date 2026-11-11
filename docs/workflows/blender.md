# Model, inspect, and render in Blender

Use Blender for organic or procedural shapes, curves, materials, complete scenes,
and animation. Printable exposes Blender Python as well as convenience modeling
operations, native observations, and reusable rendered views. A printer is optional.

## Inspect and save before editing

Read `inspect.scene` to find objects and the current scene identity. Use
`inspect.object` for selected details, `node_tree` for materials or Geometry Nodes,
and `editing_state` for selection and available editors. Follow pagination for
larger scenes instead of assuming the first page is complete.

Save a checkpoint through `scene`:

```json
{"action":"checkpoint","params":{"path":"before-edit.blend"}}
```

Use [project scenes](../project-scenes.md) when switching between projects;
switching requires an explicit backup or discard choice. Pass the returned
`scene_state` as `expected_scene` when an operation needs to apply only to the
state you just inspected. A rejected stale state calls for another inspection.

## Make an edit and look at it

For supported primitives, booleans, renaming, or rigid rotation, use `edit`.
For other modeling, materials, procedural node setup, or animation, use
`blender_execute` with deliberate Python. Its `result` should contain the small
measurements or identities needed next. This is native Blender scripting,
not a restricted language sandbox.

For example, ask your assistant to create a ribbed vase with a specified height,
keep its source and checkpoint, and show the rim and base before choosing a
render. The assistant can change a parameter, return dimensions, and capture
the resulting view. This is an example brief, not a recorded successful run.

Choose a view that answers the current question:

| Question | View |
| --- | --- |
| What is actually selected or open in Blender? | `view.native`, using a viewport or editor capture |
| Is the overall size right? | `view.dimensions`, checked against returned measurements |
| What is inside the shell? | `view.section` |
| Which surfaces may need support in this orientation? | `view.overhangs`, read with the mesh report |
| How does the material or silhouette look? | `render.scene` or `render.product` |

Native viewport and editor captures have different redraw, overlay, and render
convergence behavior. Read their [capture metadata](../native-observation.md)
when an observation is incomplete. A dimensioned render helps inspection;
[mesh measurements](../mesh-assessment.md) establish the reported geometric checks.

## Compare and present

Use `render.product` for a product view, `render.gallery` for multiple views,
and `render.turntable` for a contact sheet. Reuse the same camera and
[presentation settings](../presentation-controls.md) when comparing versions.
`compare_renders` combines existing PNGs; it cannot align different cameras.

Save the chosen scene and source. To render a video without tying up live
modeling, submit a checkpoint to `job` with the desired animation or turntable
settings. The separate worker renders an immutable snapshot; overwriting the
original checkpoint does not change the queued job. The processes still share
physical GPU resources.

Retain the returned `job_id`, read `job.get` for progress, and use `job.artifacts`
to find frame images and the completed video. Cancellation and restart recovery
follow the [render worker guide](../render-worker.md). Retrieve the files with
the client's file mechanism or the [direct client](../getting-started.md#retrieve-files).

## Reopen or recover

Use `scene.restore` on a saved checkpoint when you want to return to that state.
Use project export for supported [portable Blender bundles](../workflow-interface.md#shared-projects)
and keep external dependencies with the saved work. A live scene is not a backup.

If a delivered edit times out, it may already have changed the scene. Wait for
healthy status, then inspect or restore; never replay the mutation automatically.
An animation of moving parts does not prove clearance. Use
`analyze_assembly` for the supported rigid-motion checks before making a
mechanical claim.
