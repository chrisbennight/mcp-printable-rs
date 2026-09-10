# Native Blender observations

`view` with `action: "native"` observes the authoritative Blender UI. It accepts
the same editor selector as [Python execution](editor-context.md), optional
scene preconditions, and an explicit capture method:

- `viewport` draws the current 3D view through Blender's GPU viewport API.
  It captures scene appearance using that view's shading and projection.
  Editor chrome is absent and overlay fidelity is not guaranteed.
- `editor` redraws and captures the selected editor, including its visible
  controls and editing overlays. It can capture mesh editing, node editors,
  image/UV editors, and other available Blender editors. Pixels come from the
  supervisor-owned private X11 display after redraw, including visible popups
  or overlapping UI. `capture_source` and `occlusion` state this explicitly.

The selected method never silently falls back. Background Blender reports
native observation as unavailable; the private UI runtime is required.

For example:

```json
{
  "action": "native",
  "params": {
    "method": "viewport",
    "max_size": 640,
    "view": {"axis": "FRONT", "perspective": "ORTHO", "shading": "SOLID"}
  }
}
```

Optional `view` settings persist in the targeted viewport. They accept an axis
or a quaternion in Blender's w/x/y/z order, location, distance, perspective,
shading, overlays, and X-ray. They change inspection configuration without
changing model geometry or advancing the managed model revision. General
Python remains available for other editor controls and framing operators.

Each response identifies model generation/revision, frame and subframe, current
editing context, view configuration, capture method, graphics device, redraw
method, dimensions, and timing. `view_state.configuration_sha256` covers the
reported view configuration and context. The PNG's `sha256` identifies its
bytes. These serve different purposes: reuse reported viewport options to
reproduce an inspection direction, and use the image digest when comparing
retained evidence.

Dependency evaluation precedes capture. Viewport drawing runs explicitly;
editor capture forces a window draw and swap first, then reads the selected
editor rectangle from the private display. Off-display editors and unsupported
pixel formats fail explicitly. The runtime disables the startup splash before
the UI event loop begins. Progressive material and
rendered views report convergence as `not_reported`, because the API does not
establish that sampling or shader compilation has converged. Capture success
does not establish convergence.

`max_size` bounds the returned image's longest side while preserving aspect
ratio. Editor responses retain source dimensions and a `resized` flag. Choose
enough resolution for the investigation; small images may hide topology or
node labels. The server verifies PNG completeness, dimensions, size, and digest
before returning it. Inline images remain optional and subject to the existing
byte limit; image bytes are not a model-independent token measure.

Omitting `path` creates a new PNG under `observations/`. A matching `.png.json`
sidecar retains the observation metadata, allowing agents to keep history in
artifacts and retrieve only relevant observations. Providing a path deliberately
reuses that destination. Artifact publication and file handoff use the existing
`artifact` workflow.

The [complete product refactor](product-refactor.md) also includes isolated
render workers, targeted post-edit feedback, output-contract refinement,
measured agent evaluation, and production cutover.

Grounding:

- [Blender GPU drawing](https://docs.blender.org/api/current/gpu.types.html)
- [Blender window coordinate conversion](https://github.com/blender/blender/blob/blender-v5.2-release/source/blender/windowmanager/intern/wm_window.cc)
- [Community report of black screenshots on virtual displays](https://github.com/digitable-lol/blender-mcp)
- [Window redraw operators](https://docs.blender.org/api/current/bpy.ops.wm.html)
- [Viewport state and matrix updates](https://docs.blender.org/api/current/bpy.types.RegionView3D.html)
- [Community Blender MCP implementation](https://github.com/ahujasid/blender-mcp/blob/main/addon.py)
