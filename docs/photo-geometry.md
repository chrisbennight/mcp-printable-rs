# Photograph geometry models

The photograph worker can carry a set of geometry models that each read one
photograph and estimate some of: depth, which pixels are measured surface (the
subject mask), the camera's focal length in pixels, and which way each surface
faces (normals, in the camera's frame).

| Name | Model | Outputs |
| --- | --- | --- |
| `da3-mono-large` | Depth Anything 3 mono large | relative depth |
| `dp2-h` | Depth Pro 2 H+ | metric depth, focal length |
| `dp2-l` | Depth Pro 2 L | metric depth, focal length |
| `moge3-vitg` | MoGe-3 ViT-g | mask, metric depth, normals |
| `moge3-vitl` | MoGe-3 ViT-L | mask, metric depth, normals |
| `moge2-vitl-normal` | MoGe-2 ViT-L normal | mask, metric depth, normals |
| `marigold-v2` | Marigold V2 normals | normals |

The worker never picks a model for you and does not limit the GPU memory a run
may use. The caller chooses the model, the device and the processing size,
guided by the recommendations below. A run that needs more GPU memory than is
free fails with a `resource_limit` error, and each run reports its peak GPU
memory, so the cost of a choice stays visible.

The `photo` tool's [`analyze` action](#analyse-a-photograph) runs them on
your photographs, and its [`relief` action](#build-a-relief) builds a
printable relief from the result.

## Recommendations

All figures come from [the depth benchmark](../evidence/depth-benchmark.md):
one RTX 4090, a 32-thread CPU, nine rendered scenes and fifteen photographs.
Shallow subjects are reliefs, coins, lettering and artwork, where surface
direction matters; deep subjects are objects, figures and heads, where overall
height matters. Surface error is the angle between true normals and normals
derived from the predicted depth; relief error is the height error inside a
relief region, as a percentage of its depth; head height error is in
millimetres on the one rendered head scene.

| Job | Recommended | Measured quality | Cost per photograph |
| --- | --- | --- | --- |
| Depth, shallow subjects | `da3-mono-large` at 1024 px | 3.8° surface error over nine scenes; 15.3 % relief error over the six shallow scenes | GPU 0.07 s, 2.4 GB; CPU 13 s, 3.8 GB of RAM |
| Depth, deep subjects, GPU | `dp2-h` at 2048 px | 21.1 mm head height error (34.5 mm at 1024 px); provisional, one scene | GPU 0.40 s, 8.6 GB on photographs, 10.1 GB peak on 2048 x 1536 scenes |
| Depth, deep subjects, small GPU | `dp2-l` at 1024 px | 37.6 mm head height error | GPU 0.05 s, 3.1 GB; 2.4 GB peak under a 3 GB allocator limit |
| Depth, deep subjects, CPU | `dp2-h` at 1024 px | 33.2 mm head height error | CPU 14 s, 7.4 GB of RAM |
| Subject mask | `moge3-vitg`, then `moge3-vitl`, then `moge2-vitl-normal` | Provisional: masks were saved but not yet scored | ViT-g GPU 0.56 s, 6.7 GB; ViT-L GPU 0.26 s, 2.5 GB; MoGe-2 GPU 0.06 s, 2.3 GB or CPU 7.3 s, 3.5 GB of RAM |
| Focal length | `dp2-l` | 8.3 % mean error against EXIF on four photographs | Same as its depth run above |
| Normal map (optional) | `marigold-v2` at 1024 px | 4.0° normal error, the most accurate measured | GPU 6.2 s, 2.0 GB |

Notes on these choices:

- **2048 px does not help shallow subjects.** Relief error was unchanged and
  surface direction got worse (Depth Anything 3 mono large 3.8° to 4.9°;
  Marigold V2 normals 4.0° to 7.6°). Use 2048 px only for deep subjects with
  Depth Pro 2 H+ on a GPU.
- **MoGe-3 needs an NVIDIA GPU.** It depends on Triton and does not load on
  the CPU; on a CPU-only worker use `moge2-vitl-normal` for the mask.
- **Every Depth Pro 2 size shares one field-of-view estimator**, so `dp2-l`
  gives the same focal length as `dp2-h` at lower cost. MoGe models measured
  20.4 % to 32.3 % focal-length error.
- **Fusing depth with normals did not beat depth alone.** Depth Anything 3
  mono large's depth alone gave the best surface direction of any combination,
  and fusing normals made deep-subject height worse (MoGe-3 ViT-g with its
  normals: 37.3 mm on the head scene against 34.6 mm without).
- **Marigold V2 is not needed for depth.** Use it only when a normal map is
  itself wanted. It is slow on the CPU (303 s for its depth-and-normals pass
  at 1024 px) and the benchmark recommends no CPU normal-map route.

Times exclude model loading and are medians; GPU memory is peak allocator
memory after loading. On the CPU, memory is peak resident memory. The
deep-subject and mask recommendations rest on few measurements and may change
as more scenes are measured.

## Enable the models

Set `PRINTABLE_RECON_GEOMETRY_CACHE` in the photograph worker's environment to
a persistent directory writable by uid 10001. The settings are listed in
[`.env.example`](../.env.example):

| Setting | Default | Meaning |
| --- | --- | --- |
| `PRINTABLE_RECON_GEOMETRY_CACHE` | unset (disabled) | Where downloaded and converted model files are kept |
| `PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS` | 900 | Deadline for one model run |
| `PRINTABLE_RECON_GEOMETRY_PREPARATION_TIMEOUT_SECONDS` | 86400 | Deadline for downloading and converting models |
| `PRINTABLE_RECON_CPU_THREADS` | 4 | CPU threads for photograph scripts; TRELLIS.2 generation keeps its own eight |

`PRINTABLE_RECON_DEVICE=cpu` keeps the geometry models off the GPU entirely;
a run that asks for the GPU is then refused.

## Model files

Model weights are not part of the image. The worker image lists each model's
pinned files in `recon/models/geometry.json`. The first preparation of a model
downloads each file from its publisher, at the recorded revision and SHA-256
digest, into the cache directory. Downloads use the operator's
`HF_ENDPOINT` and `HF_TOKEN` when set and can resume after an interruption.
Only a complete, verified copy is published as a snapshot under
`snapshots/`; later preparations reuse it without contacting the network.
Marigold V2 is also converted once into the streamed layout the worker reads,
under `converted/`. A `photo` analyze job prepares the models it needs before
it runs them, so the first analysis that uses a model downloads it; there is
no separate preparation command. Model runs never download anything.

## Status

The `status` tool's `photo_geometry` section shows each model's preparation
state: `not_prepared`, `preparing`, `prepared`, `failed` or `interrupted`. The
worker reads this from the cache on each readiness check without loading any
model. If the models are not configured or the check cannot run,
`photo_geometry` reports `"available": false` and the worker's other
photograph jobs are unaffected.

## Analyse a photograph

The `analyze` action prepares, in one request, what later photograph
workflows need: depth for broad shape, normals for detail, a subject mask, a
focal length, and a confidence map with summary scores. It is offered while
the geometry models are configured; a model that is not yet prepared is
downloaded inside the analysis job.

```json
{"action": "analyze", "params": {
  "image": "photos/coin.jpg",
  "subject": "shallow",
  "normals": true
}}
```

Every model parameter is optional and defaults to the
[recommendations](#recommendations). Name a model to choose differently, for
example `"depth_model": "dp2-l", "side": 1024, "mask_model": "moge3-vitl"` for
a deep subject on a GPU with about 3.5 GB free (Depth Pro 2 L allocates
about 3.1 GB when nothing limits it).

| Parameter | Values | Default |
| --- | --- | --- |
| `image` | workspace JPEG, PNG or WebP | required; transparency is flattened onto black |
| `subject` | `shallow` (reliefs, coins, lettering, artwork) or `deep` (heads, figures, objects) | required |
| `depth_model` | `da3-mono-large`, `dp2-h`, `dp2-l`, `moge3-vitg`, `moge3-vitl`, `moge2-vitl-normal` | `da3-mono-large` for shallow subjects, `dp2-h` for deep ones |
| `side` | `1024` or `2048`: the longest side the depth model processes the photograph at | `2048` for deep subjects on a GPU, otherwise `1024` |
| `mask_model` | `moge3-vitg`, `moge3-vitl`, `moge2-vitl-normal` | `moge3-vitg` on a GPU (the benchmark's best; about 6.7 GB), `moge2-vitl-normal` on the CPU |
| `focal_model` | `dp2-h` or `dp2-l` | the depth model when it is a Depth Pro 2 model, which then serves both from one run; otherwise `dp2-l` |
| `normals` | `false` or `true` | `false`: normals are derived from depth |
| `normals_model` | `marigold-v2`, with `normals: true` | `marigold-v2` |
| `device` | `auto`, `cuda` or `cpu` | `auto`: `cuda` unless the worker is configured with `PRINTABLE_RECON_DEVICE=cpu` |
| `quality` | `best` or `fast` | `best`; see below |
| `focal_length_px` | 10 to 100000 | none: a known focal length in pixels of the photograph, used instead of `focal_model` |

Only the depth model uses `side`. The other models process the photograph at
1024 pixels, unless one is the same model as the depth model, which then runs
once for both. MoGe-3 needs a GPU, so it is refused with `device: cpu`.

Depth from a MoGe model reads painted artwork as the flat object it is;
Depth Anything 3 invents sculpted depth for a painting, which suits reliefs
made from artwork. Choosing `"depth_model": "moge3-vitl", "mask_model":
"moge3-vitl"` runs one model for depth and mask.

`best` runs the depth model twice more: on the photograph mirrored left to
right and at three quarters of its size, and uses the results as consistency
checks. `fast` skips those two passes, so its confidence map has fewer
checks. Use it for previews.

### Results and the cache

The first request for a photograph and settings returns a `job_id`. Follow it
with `status`, find it with `list` and stop it with `cancel`, as for object
jobs. The job downloads any model files it needs, waits for the GPU, runs the
models one at a time in one process, freeing each before loading the next,
and publishes the result. The whole run must finish within
`PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS` for each model it loads. A run that
needs more GPU memory than is free fails with a `resource_limit` error; choose
a smaller model or side, or `device: cpu`.

The result is stored under `.printable/photo-analysis/<cache_key>/` in the
workspace, which only the server writes. The key is the SHA-256 of the
photograph's SHA-256, the settings with their defaults filled in (so it covers
the chosen models, side and device), the model list and the analysis code. A
later request with the same photograph and settings returns the stored result
at once, without loading a model, and reports `"cache": {"hit": true}`. A
stored map that no longer matches the SHA-256 in its manifest is not used; the
analysis runs again.

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
- `settings`: the request with every default filled in;
- `runs`: the model, device and processing size of each run, the roles it
  served and whether it had the extra passes;
- `timings` and, on a GPU, `gpu_peak_allocated_mib`: the cost of the choice;
- `depth`: the model the depth came from and whether it is `metric` or
  `relative`;
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

## Build a relief

The `relief` action turns a photograph into a printable relief in one request:
an STL, a 16-bit heightmap and a JSON report. It is offered with `analyze`.

```json
{"action": "relief", "params": {
  "image": "photos/coin.jpg",
  "path": "relief/coin.stl",
  "preset": "coin",
  "width_mm": 40
}}
```

The relief reads a shallow-subject analysis of the photograph, with the
[recommended](#recommendations) Depth Anything 3 mono large depth. When that
analysis is not yet cached, the response is the analysis job and a `next`
instruction instead of a relief:
follow the job with `status`, and repeat the same request once it succeeds,
which then reads the analysis from its cache. Give `analysis`, the `cache.key`
of a finished analysis, in place of `image` to use one directly.

The relief itself is built synchronously in the memory-isolated geometry
worker, so the request may take a while for a large analysis; cancelling the
request stops it. Before the worker runs, every map it reads is copied and
checked against the SHA-256 in the analysis manifest; an analysis whose maps
have changed is refused.

| Parameter | Values | Default |
| --- | --- | --- |
| `image` or `analysis` | a workspace photograph, or an analysis cache key | one is required |
| `path` | a new workspace `.stl` | required; `NAME.height.png` and `NAME.report.json` are written beside it |
| `overwrite` | `true` to replace existing files | `false` |
| `preset` | `plaque`, `coin`, `mold` or `artwork` | required |
| `width_mm` | 5 to 1000; a coin's diameter | required |
| `height_mm` | 5 to 1000; not for a coin | the image's proportions |
| `relief_depth_mm` | 0.2 to 50: lowest to highest point of the relief | the preset's |
| `base_mm` | 0.4 to 50: the flat base under the relief | the preset's |
| `border_mm` | 0, or `nozzle_mm` to 100: a flat border level with the relief's top, added outside `width_mm` and `height_mm`, or a coin's rim inside its diameter | the preset's |
| `quality` | `best` or `fast`, passed to the analysis | `best` |
| `nozzle_mm` | 0.1 to 2: the nozzle; samples are kept at least half its width apart, and the narrow-detail check uses it | 0.4 |
| `advanced` | see below | none |

The presets set these starting values. None has been measured on printed
reliefs yet; adjust them for the subject and the printer.

| Preset | Outline | Relief depth | Base | Border | Compression | Mirrored and inverted |
| --- | --- | --- | --- | --- | --- | --- |
| `plaque` | rectangle | 3 mm | 2 mm | none | 1 | no |
| `coin` | round | 1.2 mm | 2 mm | 1.5 mm rim | 2 | no |
| `mold` | rectangle | 3 mm | 3 mm | 5 mm wall | 1 | yes |
| `artwork` | rectangle | 3 mm | 2 mm | none | 3 | no |

- A **coin** uses the central square of the photograph. Its face is the
  circle inscribed in that square, inside a raised rim `border_mm` wide and
  level with the relief's top. The square plate's corners outside the circle
  stay at the base; cut the outline round when slicing or modelling if the
  coin must be round.
- A **mold** is the relief mirrored and inverted, so a cast taken from it
  reads the right way round, inside a wall level with its top. Its narrow
  detail is checked on the cavity it prints.
- **artwork** names Depth Anything 3 mono large, which gives painted artwork
  a sculpted reading, and compresses its invented depth harder. For a flat
  reading of the artwork, name a MoGe model in `advanced.depth_model`.

`advanced` holds the relief geometry's other choices:

| Field | Values | Default |
| --- | --- | --- |
| `integration` | `depth_only`, `anchored_normals` or `frankot_chellappa` | `depth_only`, the best measured route for shallow subjects; the normal methods are not yet measured |
| `compression` | 0 to 100; 0 keeps slopes in proportion | the preset's |
| `detail_gain` | 0 to 10: multiplier for fine detail | 1 |
| `mirror`, `invert` | `true` or `false` | the preset's |
| `depth_model` | as for `analyze` | the analysis default, or the preset's depth model |
| `normals` | `true` analyses with Marigold V2 normals, for the normal methods | `false` |

The analysis model choices and `quality` apply only with `image`. The stages,
their options and their checks are described under
[Relief geometry](photo-depth.md#relief-geometry). The analysis's subject mask
decides which pixels are the subject; the rest becomes the flat background.

### Relief results

The result holds the following; the saved report holds the same except
`artifacts`:

- `settings`: every choice with the preset's values filled in;
- `analysis`: the cache key, the photograph's SHA-256, the size of its maps,
  the depth model and kind, the analysis settings, the focal length, its
  scores and warnings;
- `relief`: the solid's size in millimetres and triangle count, the sample
  count and spacing, the integration and compression reports, the checks
  from `check_relief`, and the heightmap's scale;
- `artifacts`: the STL, the heightmap and the report.

`checks.passed` is true when the solid is sound and the relief is at least
one 0.2 mm layer high. A relief that fails is not written; the request
returns `relief_not_printable`. Ridges and grooves narrower than the nozzle
still print, with less detail, so they are reported in `narrow_ridges` and
`narrow_grooves` without failing the check.

The heightmap is a 16-bit greyscale PNG of the relief, including a coin's rim
but not a rectangular border, before mirroring or inversion, with the image's top row at the top. A pixel value `p`
is a height of `p / 65535 * full_scale_mm` above the relief's lowest point,
where `full_scale_mm` is the relief depth, recorded in the report.

The three files are published in turn: the STL, the heightmap, then the
report. After a storage error, inspect the paths before repeating the request.
A rendered preview is not produced; render the STL with the `view` or `render`
tools.

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
