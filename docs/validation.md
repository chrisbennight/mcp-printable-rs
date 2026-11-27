# Validate photograph results

A relief, depth map or mesh made from a photograph is an estimate. Check it
with measurements before trusting how it looks. `photo.compare` measures a
result against other results, against the photograph it came from, or against
a synthetic scene whose depth and normals are known exactly. It reports each
measure on its own and never combines them into one score.

## Check in this order

1. **Hard checks first.** Use the checks that do not depend on judgement:
   the relief report's `checks`, `validate_mesh` for a mesh (solid, watertight,
   size, and comparison with a reference mesh), and slicing previews for
   printability. A failed hard check is a failure, whatever the result looks
   like.
2. **Then compare with the photograph.** Use `photo.compare` in `photo` mode
   to measure whether the result's outline, surface direction and shading
   match the photograph's analysis.
3. **Then look.** Render the result with `view` or `render`, put it beside the
   photograph with `compare_renders` or the comparison sheet, and judge what
   the numbers cannot: likeness, style and which details matter.

Never judge a result only by eye: a rendering can look right while the relief
is inverted, shifted or flattened. If the same check fails twice after
adjusting settings, change route (another depth model, `subject`, preset or
generator) rather than tuning the same one again.

## Modes

```json
{"action": "compare", "params": {
  "mode": "photo",
  "analysis": "<cache.key from photo.analyze or the relief report>",
  "image": "photos/coin.jpg",
  "inputs": ["relief/coin.height.png"],
  "path": "relief/coin.compare.json"
}}
```

| Mode | Compares | Needs |
| --- | --- | --- |
| `outputs` | Two to six inputs, each against the first: two models, two settings or two routes | `inputs` |
| `photo` | One to six inputs, each against a finished analysis and its photograph | `inputs`, `analysis`, `image` |
| `synthetic` | Zero to six inputs against the exact depth and normals of a mesh you supply | `scene`, and optionally `inputs` |

| Parameter | Values | Default |
| --- | --- | --- |
| `inputs` | Workspace files: a 16-bit greyscale PNG heightmap (larger is nearer, as `photo.relief` and `photo.depth` write them), a 32-bit floating-point TIFF depth map (larger is farther, as `photo.analyze` writes `depth.tiff`), a 16-bit RGB PNG normal map, or an STL, OBJ, PLY or GLB mesh | none |
| `kind` | `relative`, `metric` (metres) or `logrel` (log depth): how map values relate to distance, as the [depth benchmark](../scripts/depth_benchmark/README.md) aligns them | `relative`, the alignment that assumes least about the map |
| `path` | A new workspace `.json` report; `NAME.sheet.png`, and for `outputs` `NAME.diff-N.tiff`, go beside it | required |
| `overwrite` | `true` to replace those files | `false` |

All modes run on the CPU. A mesh is first rendered by the isolated
[render worker](render-worker.md): one ray through each pixel centre gives its
exact depth, surface normals and coverage. That render does not wait in or
jump the render job queue: while a render job is queued or running, the
request is refused as busy; retry when the jobs finish. Maps are not rendered.
The measures are computed by the photograph worker with the depth
benchmark's own scoring code, so the action is offered with `photo.analyze`.

### outputs

Every input is aligned to the first with the least-squares scale and offset
the benchmark uses for relative depth; a negative scale is not allowed, so an
inverted input aligns as a flat one. Each later input then reports:

- `correlation` of its values with the first's, before alignment; a negative
  value means one is inverted;
- `whole`: root mean square, mean, mean absolute and 95th-percentile absolute
  difference, in the first input's units, and the root mean square as a
  percentage of the first input's depth range;
- `regions`: the same in each ninth of the image, from `top left` to
  `bottom right`, so a difference confined to one area is visible.

`NAME.diff-N.tiff` holds each difference map as 32-bit floating point,
positive where the later input lies farther away; it is 0 outside pixels both
inputs cover. Inputs must have the same proportions; meshes are framed by
their own footprint, seen from above, at the size of the first map, or at
1024 x 1024 when every input is a mesh.

### photo

The input is resampled to the analysis's size. An input with other
proportions than the photograph, such as a coin's square heightmap, is
compared with the centre of the photograph cropped to its proportions, as the
coin preset crops it. Use a relief's heightmap rather than its STL: the
heightmap excludes a rectangular border, while an STL is framed by its whole
footprint. Each input reports:

- `silhouette_iou`: intersection over union of the input's raised area, the
  pixels standing more than 2 % of its height range above its lowest level,
  with the analysis's subject mask;
- `outline_distance`: mean distance in pixels between the two outlines,
  averaged both ways, its 95th percentile, and the mean as a percentage of
  the image diagonal;
- `normal_angle`: median angle and the share within 15 degrees between the
  input's surface normals and the analysis normals inside the mask. A relief
  compresses depth, so the input's heights are first scaled by the one
  non-negative factor, `height_scale`, that best matches the analysis
  slopes. An inverted relief fits with a scale of 0 and scores as flat;
- `shading`: the correlation of the photograph's brightness with the best
  single directional light on the input's normals, and the same for the
  analysis normals, `analysis_correlation`, as a reference for what this
  photograph allows. Shading cannot tell a relief from its inversion lit from
  the opposite side; use `normal_angle` for that.

The report's `analysis` holds the analysis's own label-free confidence
`scores` and `warnings`: how far its depth models agreed with themselves and
each other. Low agreement means the reference itself is uncertain there.

### synthetic

`scene` is a mesh in millimetres, Z up. The render worker renders it straight
down through a 50 mm lens on a 36 mm sensor at 1024 x 768, the benchmark's
frame, standing far enough back that its footprint fits with a 10 % margin.

With no `inputs`, the request writes the scene's photograph,
`NAME.scene.png`, rendered in Cycles through the same camera, and a sheet of
the exact depth and normals. Run any route on that photograph, then repeat
the request with the route's map, or several, as `inputs`.

Each input is resampled to the scene's size and scored with the benchmark's
scene scores (`scoring.scene_scores` in
[scoring.py](../scripts/depth_benchmark/scoring.py)): `absrel` depth error,
`edge_f1`, `depth_normal_deg` (normals derived from the depth),
`normal_deg` (an input's own normals, or a normal map), `relief_pct` and
`relief_rmse_mm`, and for metric input `scale_error_pct`. They are the
numbers the benchmark report gives for the same truth and prediction. The
relief region is every pixel the mesh covers, where a benchmark scene names
its own region, and the render's depth is in metres for these scores.

`edge_f1` penalises invented depth edges: at a jump ratio where the truth
has no edge, a prediction with edges scores 0 instead of being skipped.

## What compare does not do

- It does not run a route for you in `synthetic` mode: render the scene, run
  the route on its photograph, and score the route's map.
- It has no built-in scenes; supply a mesh, for example one made with
  `scad_build`.
- It does not find a mesh's pose in the photograph: a mesh is seen straight
  down, framed by its footprint, which suits reliefs.
- How well these measures predict a good print has not been measured.
