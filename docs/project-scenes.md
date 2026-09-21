# Project scenes and CAD attachment

The existing `scene` workflow manages Blender's live scene and its project
association. General Blender modeling remains available. Creating or resolving a
project does not switch Blender, and CAD conversion never acquires the live
Blender lane. Project organization is not a hostile-code security boundary.

Create the project using `project`, inspect Blender, then call `scene` with
`action: "open_project"`. Its parameters are:

| Parameter | Meaning |
| --- | --- |
| `project_id` | Existing project to open |
| `expected_scene` | Complete current Blender scene observation |
| `mode` | `empty` starts an empty scene; `checkpoint` restores a saved scene; `adopt` associates an unbound current scene |
| `checkpoint` | Project-relative `.blend` path, required only for `checkpoint` |
| `save_current_to` | Optional workspace-relative backup of the current scene; a bound scene's backup must remain in its own project |
| `discard_current` | Explicit permission to replace the current scene without a backup; defaults to false |
| `timeout_seconds` | Work budget, defaults to 600, accepts 1–1800 |

Switching requires either a backup path or explicit discard. Adopting cannot
relabel another project's live scene. The restore input is snapshotted before
saving the previous scene; the backup cannot overwrite the input. Checkpoints
retain the project association across file restore, including render-worker
restores. Process restart still creates a new scene generation, so callers must
inspect again. A backup or retained checkpoint supplies the recovery path.

The returned `scene_state` contains the bound `project_id` alongside generation
and revision. Pass it to later modeling and rendering calls. A stale state, a
different project, or a precondition omitting a bound project's identity fails
before the handler runs. Ordinary calls without a scene precondition retain their
existing generic behavior; they do not claim project-specific concurrency safety.

## Use CAD geometry in Blender

Call `scene` with `action: "attach_cad"` and `params` containing the current
`project_id`, complete `expected_scene`, and project-relative `path` to the
`output/model.glb` emitted by `cad_build`. The adjacent `components.json` must
contain its complete node-name map. The same work-budget parameter applies.

Blender imports a confined snapshot. External GLB buffer/image references are
rejected. Assembly parents, placements, and materials remain in the imported
hierarchy. Original occurrence/product names are retained as object properties
even when Blender must disambiguate display names. The importer converts glTF
metres into Printable's millimetre coordinates while accounting for Blender's
scene-unit conversion. This follows Blender's
[glTF importer](https://github.com/blender/blender/blob/main/scripts/addons_core/io_scene_gltf2/blender/imp/blender_gltf.py).

The result returns source identity, object/root counts, bounded root names, and
the number of matched and unmatched source-name records. Use `inspect` to explore
the complete imported hierarchy. CAD source and exported artifacts remain
independent of presentation edits, and a failed attachment does not remove them.
The source SHA-256 identifies the original GLB bytes, before adding Blender
metadata to the private import snapshot.

A timeout or partial native import has an uncertain mutation outcome. Inspect
the scene or restore a checkpoint before another mutation; do not blindly retry.
A failed project restore reports the observed project and scene state for
recovery, rather than claiming the requested project opened successfully.

The native container smoke exercises an asymmetric placed assembly, original
parent/part names, non-default Blender units, stale attachment rejection, and
same-named objects recovered from two distinct project checkpoints. Real vendor
and combined deployed acceptance remain tracked by the unified epic.
