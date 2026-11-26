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

These models are configured, prepared and reported by the worker. Tools that
run them on your photographs are not yet available.

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
| `PRINTABLE_RECON_CPU_THREADS` | 4 | CPU threads for every photograph script |

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
under `converted/`. Model runs never download anything.

## Status

The `status` tool's `photo_geometry` section shows each model's preparation
state: `not_prepared`, `preparing`, `prepared`, `failed` or `interrupted`. The
worker reads this from the cache on each readiness check without loading any
model. If the models are not configured or the check cannot run,
`photo_geometry` reports `"available": false` and the worker's other
photograph jobs are unaffected.

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
