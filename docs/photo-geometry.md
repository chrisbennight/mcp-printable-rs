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

These models are configured, prepared and reported by the worker. Tools that
run them on your photographs are not yet available.

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

The `status` tool's `photo_geometry` section shows, for each role, the active
model, its device, the processing size in pixels (`side_px`), whether it is a
fallback and why (`fallback_reasons`), and the model's preparation state:
`not_prepared`, `preparing`, `prepared`, `failed` or `interrupted`. A role with
no usable option shows `unavailable` and the reasons. The worker reads this
from the registry and the cache on each readiness check without loading any
model. If the check cannot run, `photo_geometry` reports `"available": false`
and the worker's other photograph jobs are unaffected.

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
