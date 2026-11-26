# Depth benchmark

Scores depth and normal models for Printable's photograph workflows against
scenes with exact depth and normals, and records what each model costs on a
GPU, under a GPU memory limit and on CPU. Results choose the photograph
worker's default models: quality first, then the memory that quality needs.
The latest results are in
[evidence/depth-benchmark.md](../../evidence/depth-benchmark.md).

## Run it

Requirements: Docker, about 60 GB of disk for model downloads, and Python 3
on the host. NVIDIA container support is needed for the `gpu` and `budget`
tiers; without an NVIDIA GPU the default stages render the scenes on the CPU
and run only the `cpu` tier.

```sh
scripts/depth_benchmark/run.sh ~/depth-benchmark
```

This downloads the photographs, builds the benchmark image, renders the
scenes, runs every model on the `gpu`, `budget` and `cpu` tiers, measures
2048-pixel timings on each tier and writes `results/report.md`, `results/metrics.json`
and comparison sheets under `~/depth-benchmark`. Stages can be run one at a
time, for example `run.sh ~/depth-benchmark report`.

| Setting | Meaning |
| --- | --- |
| `BENCH_MODEL_CACHE` | Model download cache (default `<data-dir>/models`) |
| `BENCH_MARIGOLD_DIR` | Converted Marigold V2 model; prepare it as in [docs/photo-depth.md](../../docs/photo-depth.md). Marigold is skipped without it. |
| `BENCH_GPU_BUDGET_GB` | GPU memory limit for the `budget` tier (default 3) |
| `BENCH_CPU_THREADS` | CPU threads for the `cpu` tier (default: all) |
| `BENCH_GPU_LOCK` | Optional file held with `flock` while the GPU is in use, for shared hosts |
| `BENCH_SIDE` | Scene render size and input size in pixels (default 1024); use a separate data directory for each size |

The `cpu` tier runs with no GPU visible. Every model scores the same three
scenes there; faster models also run two photographs. Each model has a time
limit per image. A model that fails, exits with an error or reaches its time
limit is recorded as a failure, and a stage that cannot run at all (scenes,
inputs or report) stops the run with a non-zero status. Running a tier replaces
that tier's earlier results.

Model weights are fetched at pinned revisions and checked against SHA-256
digests in `checkpoints.py` before they are loaded.

## What is measured

- **Scenes** (`scenes.py`, Blender 5.3): a relief plaque, a shallow relief, a
  coin, an engraved coin, raised and engraved lettering, a polished medal, a
  head on a pedestal, objects on a table and thin wires. Depth and normals come
  from the Position and Normal passes, converted to the camera frame.
- **Photographs** (`photos.json`): fifteen Wikimedia Commons images with their
  licences and SHA-1 digests, downloaded at run time and verified.
- **Scores** (`scoring.py`, NumPy only, tested in `scripts/tests`):
  - depth error after scale and offset alignment;
  - median angle of the model's normals and of normals computed from its depth;
  - relief height error in each scene's relief region, after the scale and
    offset a user would choose and removal of a best-fit plane;
  - depth-edge F1;
  - focal length error against the render camera and against EXIF;
  - means over scenes with 95 % bootstrap intervals.
- **Cost**: time, peak GPU memory, peak resident memory and bytes read from
  storage per image, at the shared size and at 2048 pixels; times exclude
  loading and one warm-up image. A model is reported within 15 minutes per map
  only when every expected image completed.
- **Fusion**: depth for broad shape combined with integrated model normals for
  detail, compared with depth alone. Fusion uses no ground truth: each depth
  model's own units and Depth Pro 2 L's focal-length estimate.

Outputs follow one format (`common.py`): each model writes depth, its kind
(metric, relative or log-relative), optional normals, focal length and
valid-pixel mask per image, so new models need only a runner.
