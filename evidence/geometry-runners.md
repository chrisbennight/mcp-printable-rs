# Photograph geometry runners against the depth benchmark

Date: 2026-11-26. Host: the RTX 4090 and 32-thread CPU used for
[the depth benchmark](depth-benchmark.md). Image: the photograph worker built
from this branch (`docker build --target recon-runtime`).

`scripts/depth_benchmark/check_worker.py` ran `recon/estimate_geometry.py` once
per photograph, each in its own process as the worker starts it, on the 24
benchmark inputs at 1024 px. It compared every output with the benchmark's
saved maps for the same model, where the benchmark saved one; "Photographs
run" counts every run, including those without a saved map to compare. "Depth" is the largest per-image median relative
difference, after a scale fit for relative depth. "Mask" is the smallest
intersection over union with the benchmark mask. "Focal" is the largest
relative difference in focal length. "Normals" is the largest median angle.

| Model | Device | Photographs run | Depth | Mask | Focal | Normals | Median seconds |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Depth Anything 3 mono large | CUDA | 14 of 24 | 0.084 % | | | | 0.28 |
| Depth Anything 3 mono large | CPU | 24 of 24 | 0.017 % | | | | 5.3 |
| Depth Pro 2 H+ | CUDA | 13 of 24 | 0 | 1.0 | 0 | | 0.35 |
| Depth Pro 2 H+ | CPU | 24 of 24 | 0 | 1.0 | 0 | | 8.6 |
| Depth Pro 2 L | CUDA | 14 of 24 | 0.003 % | 1.0 | 0 | | 0.30 |
| Depth Pro 2 L | CPU | 24 of 24 | 0 | 1.0 | 0 | | 4.5 |
| MoGe-2 ViT-L normal | CPU | 24 of 24 | 0 | 1.0 | 0 | 0.27° | 5.7 |

The runners reproduce the benchmark's outputs.

The CUDA runs in this table were held to each model's benchmark peak as a hard
limit, and the photographs that did not run stopped with an out-of-memory error
under that limit. The runner no longer applies a limit.

Not run through the worker on a GPU: MoGe-3 ViT-g and ViT-L, MoGe-2 on CUDA,
Marigold V2 normals, and Depth Pro 2 H+ at 2048 px.
