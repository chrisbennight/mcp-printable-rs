# Photograph geometry models

The photograph worker can carry a set of geometry models that each read one
photograph and estimate one kind of information about it:

| Role | What it estimates | Models, best first |
| --- | --- | --- |
| `depth.shallow` | Relative depth for shallow subjects such as reliefs, coins and flat objects, where it measured most accurate | Depth Anything 3 mono large |
| `depth.deep` | Metric depth for deep subjects such as heads and whole objects | Depth Pro 2 H+, Depth Pro 2 L |
| `mask` | Which pixels are measured surface rather than background or sky | MoGe-3 ViT-g, MoGe-3 ViT-L, MoGe-2 ViT-L normal |
| `focal` | The camera's focal length in pixels | Depth Pro 2 L |
| `normals` | Which way each surface faces, in the camera's frame | Marigold V2 normals |

The `photo` tool's `analyze` action runs them on a photograph, as
[Analyse a photograph](#analyse-a-photograph) describes.

## Enable the models

Set `PRINTABLE_RECON_GEOMETRY_CACHE` in the photograph worker's environment to
a persistent directory writable by uid 10001. The settings are listed in
[`.env.example`](../.env.example):

| Setting | Default | Meaning |
| --- | --- | --- |
| `PRINTABLE_RECON_GEOMETRY_CACHE` | unset (disabled) | Where downloaded and converted model files are kept |
| `PRINTABLE_RECON_GPU_BUDGET_MIB` | 8192 | GPU memory one model run may allocate, 256 to 262144 MiB |
| `PRINTABLE_RECON_RAM_BUDGET_MIB` | unset (no limit) | Host memory a route may need, 512 to 1048576 MiB |
| `PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS` | 900 | Deadline for one model run |
| `PRINTABLE_RECON_GEOMETRY_PREPARATION_TIMEOUT_SECONDS` | 86400 | Deadline for downloading and converting models |
| `PRINTABLE_RECON_CPU_THREADS` | 4 | CPU threads for every photograph script |

`PRINTABLE_RECON_DEVICE=cpu` keeps the geometry models off the GPU entirely.

## How a route is chosen

The worker image includes a model registry, `recon/models/geometry.json`. For
each role it lists options in order of measured quality. Each option names a
model, a device (`cuda` or `cpu`), the longest side in pixels the photograph
is processed at, and the time, peak GPU memory and peak host memory measured
for it on a benchmark set.

The worker takes the first option that fits:

- A GPU option fits when the worker may use the GPU and the option's GPU
  memory limit is within `PRINTABLE_RECON_GPU_BUDGET_MIB`. Each limit is the
  benchmark's peak allocated memory plus 35 % for memory the allocator keeps
  in reserve.
- Any option fits only when its measured host memory peak is within
  `PRINTABLE_RECON_RAM_BUDGET_MIB`, if that is set.

An option after the first is a fallback, and the worker records why each
better option was passed over. With the default 8192 MiB budget, for example,
`depth.deep` runs Depth Pro 2 H+ at 1024 px rather than 2048 px, and `mask`
runs MoGe-3 ViT-L rather than ViT-g. With 3072 MiB, only Marigold V2 normals
stay on the GPU and the other roles run on the processor. When no option fits, the role
is unavailable and the reason for each option is reported.

The GPU budget is a hard limit on the PyTorch memory allocator in each model
process. An allocation past it fails that run with a `resource_limit` error
instead of taking memory another program may be using. Memory the CUDA driver
uses for its own context is not counted.

## Model files

Model weights are not part of the image. The first preparation downloads each
pinned file from its publisher, at the revision and SHA-256 digest recorded in
the registry, into the cache directory. Downloads use the operator's
`HF_ENDPOINT` and `HF_TOKEN` when set and can resume after an interruption.
Only a complete, verified copy is published as a snapshot under
`snapshots/`; later preparations reuse it without contacting the network.
Marigold V2 is also converted once into the streamed layout the worker reads,
under `converted/`. Model runs never download anything.

## Status

The `status` tool's `analyze` section reports whether the action is
configured and ready. Its `photo_geometry` section shows, for each role, the active
model, its device, the processing size in pixels (`side_px`), whether it is a
fallback and why (`fallback_reasons`), and the model's preparation state:
`not_prepared`, `preparing`, `prepared`, `failed` or `interrupted`. A role with
no usable option shows `unavailable` and the reasons. The worker reads this
from the registry and the cache on each readiness check without loading any
model. If the check cannot run, `photo_geometry` reports `"available": false`
and the worker's other photograph jobs are unaffected.

## Analyse a photograph

The `analyze` action prepares, in one request, what later photograph
workflows need: depth for broad shape, normals for detail, a subject mask, a
focal length, and a confidence map with summary scores.

```json
{"action": "analyze", "params": {
  "image": "photos/coin.jpg",
  "subject": "shallow",
  "quality": "best",
  "normals": true
}}
```

| Parameter | Values | Meaning |
| --- | --- | --- |
| `image` | workspace JPEG, PNG or WebP | The photograph; transparency is flattened onto black |
| `subject` | `shallow` or `deep`, required | `shallow` uses the `depth.shallow` route for reliefs, coins and artwork; `deep` uses the `depth.deep` route for heads, objects and scenes |
| `quality` | `best` (default) or `fast` | See below |
| `interpretation` | `sculpture` (default) or `physical`, shallow subjects only | `sculpture` keeps Depth Anything 3, which invents sculpted depth for a painting; `physical` takes depth from the `mask` model (MoGe), which reads artwork as flat |
| `normals` | `false` (default) or `true` | `true` adds a Marigold V2 normal map; otherwise normals are derived from depth |
| `focal_length_px` | 10 to 100000 | A known focal length in pixels of the photograph; the `focal` model estimates it when omitted |

Each route is chosen as [How a route is chosen](#how-a-route-is-chosen)
describes. With the default budget a shallow subject is processed at 1024
pixels; a deep subject is processed at 2048 pixels when the GPU budget allows
Depth Pro 2 H+ at that size.

`best` takes each role's highest-quality route within the budgets and runs the
depth model twice more: on the photograph mirrored left to right and at three
quarters of its size. `fast` takes the route with the shortest measured time
that fits for each role and skips those two passes, so its confidence map has
fewer checks. Use it for previews.

### Results and the cache

The first request for a photograph and settings returns a `job_id`. Follow it
with `status`, find it with `list` and stop it with `cancel`, as for object
jobs. The job downloads any model files it needs, waits for the GPU, runs the
models one at a time in one process, freeing each before loading the next,
and publishes the result. The whole run must finish within
`PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS` for each model it loads.

The result is stored under `.printable/photo-analysis/<cache_key>/` in the
workspace, which only the server writes. The key is the SHA-256 of the
photograph's SHA-256, the settings with their defaults filled in, the worker's
GPU and RAM budgets, the model registry and the analysis code. A later request
with the same photograph and settings returns the stored result at once,
without loading a model, and reports `"cache": {"hit": true}`. A stored map
that no longer matches the SHA-256 in its manifest is not used; the analysis
runs again.

| File | Format | Contents |
| --- | --- | --- |
| `depth.tiff` | 32-bit floating-point greyscale TIFF | Distance along the optical axis, larger is farther: metres for a metric model, an unscaled value for a relative one. Pixels outside the mask hold the largest depth inside it |
| `normals.png` | 16-bit RGB PNG | Unit normals, x right, y up, z towards the camera; each component `c` is stored as `(c + 1) / 2 * 65535` |
| `mask.png` | 16-bit greyscale PNG | 65535 on the subject, 0 elsewhere |
| `confidence.png` | 16-bit greyscale PNG | Confidence from 0 to 1, stored as `p / 65535` |
| `manifest.json` | JSON | The description below, written last |

Every map is 16-bit or floating point from the models to the files, and the
worker refuses a map stored with fewer bits. The maps have the size the depth
model processed the photograph at, reported as `width` and `height`.

The result, which is also the manifest, reports:

- `artifacts`: each map's file, format, bit depth, channels, conventions,
  workspace artifact and SHA-256;
- `routes` and `runs`: the model, device and processing size of each role,
  whether it is a fallback and why, and which runs had the extra passes;
- `depth`: the role and model the depth came from and whether it is `metric`
  or `relative`;
- `focal_length`: `px` at the maps' size and `photograph_px` at the
  photograph's, with `source` `given` or `estimated` and the model that
  estimated it;
- `scores` and `warnings`, described below;
- `metric_estimate`, for metric depth only: the median distance of the
  subject in metres, always with a warning. It is a learned estimate that is
  often wrong by a large factor for close-ups; never use it as a physical size;
- `image_sha256`, `cache` with its `key`, `directory` and `hit`, and the
  `manifest` artifact.

### Confidence and scores

Each check compares two estimates pixel by pixel:

- `flip_consistency`, best quality only: depth of the mirrored photograph,
  mirrored back, against the depth;
- `rescale_consistency`, best quality only: depth of the reduced photograph,
  enlarged back, against the depth;
- `model_disagreement`, when the mask model is not the depth model: the mask
  model's depth against the chosen depth;
- `normal_agreement`, with `normals: true`: Marigold V2 normals against normals
  derived from depth.

Depths are compared after the scale and shift that best align them, so a
relative model is not penalised for its unknown scale. A relative depth is also
aligned to the mask model's depth before normals are derived from it. Each
depth score reports `median_relative_difference` and `agreement`, the share of
subject pixels within 5%; the normal score reports `median_angle_degrees` and
the share within 15 degrees. `mask_fraction` and `mean_confidence` summarise the
mask and the map.

Inside the mask, a pixel's confidence is the lowest of its checks: a depth
check falls from 1 at no difference to 0 at a 10% difference, and the normal
check from 1 at 0 degrees to 0 at 45 degrees. Outside the mask confidence is 0.
When no check ran, the map is the mask alone and a warning says so.

The checks measure whether the models agree with themselves and each other,
not whether they are right. How well the scores separate good regions from
bad ones on real photographs has not yet been measured.

## Licences of bundled code

The image includes model code from these projects, under
`/opt/printable/recon/vendor`, with their licence texts in
`/usr/share/doc/printable/geometry`:

| Project | Licence | Notice file |
| --- | --- | --- |
| MoGe | MIT | `MoGe-MIT.txt` |
| utils3d-moge | MIT | `utils3d-moge-MIT.txt` |
| FlexGEMM (copy used by MoGe) | MIT | `FlexGEMM-MoGe-MIT.txt` |
| Depth Pro 2 code | Apple sample code licence | `DepthPro2-code.txt` |
| Depth Pro 2 model | Apple model licence | `DepthPro2-model.txt` |
| Depth Anything 3 | Apache-2.0 | `DepthAnything3-Apache-2.0.txt` |

Downloaded weights carry their publishers' own terms. Check each model's terms
before using its results.
