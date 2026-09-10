# Product presentation v1

`render` with `action: "product"` turns selected evaluated Blender geometry into a
repeatable product image without changing the source scene. It includes every
visible evaluated collection instance whose source object is named in
`objects`, frames their complete world-space bounds with a fixed 15% margin,
and always writes a confined PNG workspace artifact.

## Profiles

| Profile | Camera | Environment | Lighting | Default shading |
|---|---|---|---|---|
| `engineering` | orthographic | neutral, no ground | broad key and fill | `preserve` |
| `studio_neutral` | 70 mm perspective | neutral seamless ground | soft key, fill, and rim | `smooth_by_angle` |
| `studio_dark` | 85 mm perspective | dark navy seamless ground | key, fill, and rim | `smooth_by_angle` |

Engineering and neutral use Khronos PBR Neutral color management. Dark uses
AgX. The view azimuth orbits from +X toward +Y, and elevation is measured above
the XY plane. Engineering accepts elevations from -89 through 89 degrees.
Grounded studio profiles accept 0 through 89 degrees so an opaque ground can
never hide the selected product. Camera, lights, world, optional ground,
evaluated mesh copies, and presentation materials exist only in a disposable
presentation scene.

## Galleries, turntables, and durable jobs

`render` with `action: "gallery"` and `render` with `action: "turntable"` accept this same
optional `presentation`. Omitting it preserves their engineering-review
behavior. A presented batch converts each requested direction into a profile
camera view, holds one caller budget across the batch, restores the live scene,
and promotes no individual PNG until every view and cleanup check succeeds.

`job` with `action: "submit"` persists the presentation with still, turntable,
timeline-animation, or `mechanical_rotation` work and replays it after restart.
Stills and turntables measure the restored checkpoint's current evaluated
bounds once, persist that envelope before frame one, and reuse it for every
view and restart replay. General timeline animations preserve the authored
camera by default and use stable first-frame bounds for lighting and an
optional ground. Set `auto_frame_sequence` to use a separate positive
`auto_frame_sequence_timeout_seconds` budget to evaluate the union of every
rendered frame and frame that union instead. A preserved authored camera
must remain above an enabled studio ground plane at every rendered frame; a
violating frame fails visibly instead of producing a ground-occluded image.

Mechanical presentation is subordinate to manufacturing evidence. The worker
restores the immutable checkpoint, exports and certifies the original fixed
and moving bodies over the complete requested rotation, and produces zero
frames when that certificate is blocked or inconclusive. Only after a passing
certificate does it frame a conservative complete-rotation envelope. It does
not sample presentation frames for that envelope and does not add a mesh ground
plane. Presentation metadata never replaces or modifies the stored clearance
certificate.

## Materials and shading

Existing effective source materials are preserved for both Blender `DATA`- and
`OBJECT`-linked slots. Their material references are copied onto the
instance-specific presentation mesh; missing effective slots receive the
reported neutral fallback. A mesh containing both real and empty slots appears
in both `preserved_objects` and fallback `objects`, accurately reporting the
mixed outcome. Explicit `materials` assignments take precedence over every slot
and may assign each selected source object at most once. Colors are
display-referred sRGB triples from 0 through 1; metallic and roughness also
range from 0 through 1.

`smooth_by_angle` smooths copied presentation geometry at 30 degrees. It is a
visualization choice, not a geometry or printability change. The renderer never
adds a bevel modifier: visible edge breaks must already exist in the validated
product geometry. Blender application handlers are suspended for the complete
presentation operation and restored before artifact promotion, preventing
live-scene callbacks from mutating source product data.

## Response and artifact contract

The response records the effective camera, lens or orthographic scale, target,
light roles and powers, color management, world and ground, preserved,
fallback, and overridden material assignments, shading, evaluated instance
count, and framing bounds. `source_state_verified` and `cleanup_verified` must
both be true before the staged PNG is promoted. The reported SHA-256 digest is
verified against the server's confined snapshot so a concurrent replacement
cannot be returned as the requested render. Durable presented frames receive
the same size, digest, complete-PNG, and requested-dimension verification before
their completed progress advances.

Width and height are individually bounded at 8192 pixels and their product is
bounded at 16,777,216 pixels. RGB8 output has one consistent 64 MiB budget in
Blender and in the server's confined verification snapshot. Before allocating
the disposable presentation scene, Blender measures evaluated instance,
topology, copied-attribute, and material-slot totals. The response reports
those totals. Dense scenes fail with an actionable subset/reduction error
instead of multiplying geometry until the container exhausts memory.

Small PNGs can also be returned inline when `include_inline` is true. The
workspace artifact is produced at every size. Existing preview and diagnostic
render tools retain their original behavior.

For portable acceptance, decode the workspace PNG at its requested dimensions,
require visible contrast, and compare named profiles with a structural or
perceptual difference metric. Do not require pixel-identical output across GPU
drivers. Compare the authored scene before and after product stills, galleries,
turntables, and durable restoration; presentation metadata is not a substitute
for verifying source preservation.
