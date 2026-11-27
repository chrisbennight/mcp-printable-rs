# Photograph geometry on a GPU and on the processor

Date: 2026-11-27. Measured for
[epic 338](https://github.com/chrisbennight/mcp-printable-rs/issues/338):
the time, memory and disk reads of `photo.analyze`, `photo.relief` and
`photo.lithophane` on a 1024 px photograph; the default relief route compared
with the former Marigold-only route on the depth benchmark's scenes; and
`photo.object` on the benchmark's photographs. Measured values, per-run
records and the scene scores are in
[photo-geometry-gpu-cpu.json](photo-geometry-gpu-cpu.json).

## Outcome

| Criterion | Result |
| --- | --- |
| Each action under 15 minutes on a 1024 px photograph, GPU and CPU routes | **Pass** once the worker can run the default analysis (slowest: 54 s on the CPU) |
| The default GPU analysis runs in the installation the overlay describes | **Miss**: it fails in `compose.photo.yaml` as published, for two reasons below |
| Default relief route against the Marigold-only route | **Analysis depth: better** on surface angle, **not distinguishable** on relief height. **Written relief: not compared like for like** (no Marigold relief was written); the default relief heightmap scores worse than Marigold depth on surface angle because the relief compresses slopes on purpose |
| Every relief STL passes the hard checks | **Pass**: 22 of 22 |
| `photo.object`: every result passes the strict print check, with separate shells reported | **Miss**: no object was delivered; the solid rebuild failed on all 20 requests, so no shells were reported |

### The default GPU analysis fails in the published overlay

With the published images and `compose.photo.yaml` unchanged, the default
shallow-subject analysis on a GPU (`photo.analyze` with `subject: shallow`,
and therefore `photo.relief`) failed with `resource_limit` within 20 s, every
time it was tried. Two independent causes were found:

1. **Memory limit.** The overlay limits the photograph worker to 10 GiB with
   no swap. The analysis process grew past that and the kernel stopped it
   (a cgroup out-of-memory kill, recorded in the kernel log). With the limit
   raised, the same analysis peaked at 10.3 GiB of anonymous memory in the
   worker and the deep-subject analysis at 10.5 GiB. Allowing executable
   files in `/tmp` alone, with the 10 GiB limit kept, still ended in an
   out-of-memory kill.
2. **Executable temporary files.** MoGe-3 uses Triton, which compiles a small
   CUDA helper module on first use and loads it from its cache under the
   job's home directory. The worker places that directory in `/tmp`, which
   the overlay mounts as a tmpfs without `exec`, so loading the module fails
   (`failed to map segment from shared object`). This was reproduced by
   running the worker's own `analyze_photo.py` request by hand inside the
   container; with the memory limit raised to 32 GiB the analysis still
   failed until `/tmp` allowed executable mappings.

The CPU route is not affected: with the overlay's limits unchanged and
`PRINTABLE_RECON_DEVICE=cpu`, a shallow analysis of another photograph
succeeded in 48.1 s with 3.6 GiB of peak memory.

All other measurements below were made with a local override that changes
only those two settings for the photograph worker: a 32 GiB memory limit
(`mem_limit` and `memswap_limit`) and `/tmp` as a 1 GiB tmpfs with `exec`.
Fixing either cause belongs in a product change, not in this evidence.

## Environment

- One NVIDIA GeForce RTX 4090 (24 GiB), driver 595.99.02; a 24-core
  (32-thread) x86-64 desktop processor; 64 GB of RAM; models on a local
  NVMe SSD. Linux, Docker Engine with the Compose plugin and the NVIDIA
  Container Toolkit. Nothing else used the GPU during the runs; Printable's
  own Blender processes held about 0.4 GiB of it throughout.
- The published image set for source revision
  `1194f64c5b71cff950c7d36b78f913ca7f0ba496`, from the image-set record
  `ghcr.io/chrisbennight/mcp-printable-release@sha256:65111008324eda934778b999c24c8301721d13e653f9844003eef0dea60a0133`:

  | Role | Digest |
  | --- | --- |
  | server | `sha256:561ad5c2b7c266e2de7c4811cf9a1ffd3708aafdbb29c8527891131c922311ef` |
  | Blender | `sha256:3900724767525decea413886e5ce2480da118d4c39659bc85459476dd01dff4f` |
  | CAD | `sha256:15d451cebc5fb02c59043439920511f76dbf70eb90e94493ae8fa12d4151bc7d` |
  | slicer | `sha256:8a97e6014b1bee00b7363fa66169b1772a4371aa08f3c04c992c3644a5aecd06` |
  | photograph (reconstruction) | `sha256:a45b45495293889e5c2868c44b47e717572ab99c75278aaee44dda24ae40833e` |

- `compose.yaml` and `compose.photo.yaml` from that revision, bound to
  loopback, plus the local override described above, the geometry model cache
  (`PRINTABLE_RECON_GEOMETRY_CACHE`) on a disk volume, the converted
  Marigold V2 model at `/models/marigold` and the Pixal3D files at
  `/models/pixal3d`, both read-only. Other settings were left at their
  defaults, including `PRINTABLE_RECON_CPU_THREADS=4` and the overlay's
  8 CPUs. Models were downloaded and verified by the worker before any
  measured run; download time is not included.
- Requests went through the server's MCP HTTP interface with the
  `Client` class of [the direct client](../scripts/printable_client.py).
  Photographs and benchmark files were copied into the workspace volume
  beforehand.

How the resources were measured:

- **Wall time**: from sending the request until the result, or for a job
  until `status` reported it finished, polled every 2 s; so a job's wall time
  can be up to 2 s longer than its work, plus admission and result
  publication (the cold shallow GPU run spent 8.7 s outside the job's own
  15.4 s; that time was not broken down). Results also report the job's own
  `seconds` and each model's time.
- **GPU memory**: the worker's `gpu_peak_allocated_mib` (PyTorch's peak
  allocation) and, sampled every 0.5 s with `nvidia-smi`, the whole GPU's
  peak used memory less the 0.4 GiB Printable's Blender held beforehand.
- **RAM**: the photograph worker container's peak anonymous memory, sampled
  every 0.5 s from its cgroup. Its `memory.current` additionally counts file
  cache and is in the JSON. Relief and lithophane finish faster than one
  sample, so their memory is the server container's cgroup `memory.peak`
  after a fresh server start, which includes file cache.
- **Disk reads**: block bytes read by the worker container (cgroup
  `io.stat`) during the run. "Cold" runs first dropped the model files from
  the page cache (`posix_fadvise` on the mounted model files only) and used a
  photograph not analysed before; "warm" runs had the models in memory.

## Time and resources for a 1024 px photograph

The photograph is the benchmark's stone relief (08-stone-relief), 674 x 1024
pixels; the cold runs used the marble bust (03-marble-bust), 1024 px on its
longest side. The relief is the `plaque` preset, 100 mm wide; the lithophane
is the default flat plate, 100 mm wide.

| Action, route, page cache | Wall time | Job time, by model | GPU: PyTorch peak / whole GPU | Peak RAM | Disk reads |
| --- | --- | --- | --- | --- | --- |
| `analyze` shallow, GPU, warm | 20.1 s | 15.4 s: Depth Anything 3 2.1 s (three passes), MoGe-3 ViT-g 12.4 s, Depth Pro 2 L 0.5 s | 6,816 / 8,497 MiB | 10.3 GiB | none |
| `analyze` shallow, GPU, cold | 24.1 s | 15.4 s | 6,794 / 8,351 MiB | 10.2 GiB | 7.2 GiB |
| `analyze` deep, GPU, warm | 22.1 s | 15.8 s: Depth Pro 2 H+ at 2048 px 2.7 s, MoGe-3 ViT-g 11.9 s | 8,287 / 10,823 MiB | 10.5 GiB | 1.9 GiB |
| `analyze` deep, GPU, cold | 24.1 s | 16.3 s | 8,803 / 11,623 MiB | 10.6 GiB | 8.0 GiB |
| `relief` from the cached GPU analysis | 0.30 s (7.4 s for the marble bust) | | none | 81 MiB (server) | not measured |
| `analyze` shallow, CPU, cold | 48.1 s | 43.2 s: Depth Anything 3 24.2 s, MoGe-2 ViT-L normal 10.9 s, Depth Pro 2 L 7.7 s | none | 3.4 GiB | 3.8 GiB |
| `analyze` shallow, CPU, second photograph | 54.1 s | 48.5 s | none | 2.9 GiB | 2.6 GiB |
| `analyze` deep, CPU, cold | 54.1 s | 48.7 s: Depth Pro 2 H+ at 1024 px 37.5 s, MoGe-2 ViT-L normal 10.9 s | none | 4.8 GiB | 4.6 GiB |
| `relief` from the cached CPU analysis | 0.29 s | | none | not measured | not measured |
| `lithophane`, CPU only | 0.21 s | | none | 81 MiB (server) | not measured |

The whole-GPU figure is the sampled peak less the 416 MiB in use before the
run. Disk reads were counted for the photograph worker container only, so the
relief and lithophane rows, which run in the server and its geometry worker,
do not include the server's own reads of the cached maps or the image, and the
CPU relief row has no server memory figure. The relief has 767,250 triangles and the lithophane 769,778.

Every relief and lithophane passed its checks. The relief is built from the
cached analysis, so a relief request for a photograph that has not been
analysed takes the analysis time plus the relief time: about 20 s on the GPU
and 48 s on the processor here. Lithophane needs no analysis and no GPU.

MoGe-3 ViT-g, the default GPU mask model, takes about four fifths of the
GPU analysis time. Part of that may be Triton recompiling its kernels: the
compile cache lives in each job's temporary home directory and is removed
with it. This was not separated in these measurements.

## Default relief route against the Marigold-only route

Before #339, Printable's photograph depth and normals came from Marigold V2
(`photo.depth`); that is the Marigold-only route. The default relief route now analyses a shallow subject with Depth Anything 3 mono large
depth (best quality), a MoGe-3 ViT-g mask and a Depth Pro 2 L focal length,
and builds the relief by `depth_only` integration with the `plaque` preset's
compression of 1.

Each of the nine scenes' 1024 x 768 benchmark inputs was copied into the
workspace and run through:

- **default route**: `photo.relief` (`plaque`, 204.8 mm wide so the relief has
  one sample per input pixel) with no other settings; and again with
  `advanced.compression` 0, to separate the relief's deliberate slope
  compression from the analysis;
- **Marigold-only route**: `photo.depth` with `normals_path`, which runs
  Marigold V2 for depth and normals at 1024 px.

Each map was scored with `photo.compare` in `synthetic` mode against the
scene's `truth.zip`, written by
[the benchmark's report code](../scripts/depth_benchmark/report.py) from the
benchmark's own rendered passes. Surface error is the median angle between
the true normals and normals derived from the map; relief height error is
inside each scene's relief region, after scale, offset and plane removal, as
a percentage of the region's depth. Means are over scenes, with 95 %
bootstrap intervals from the benchmark's `bootstrap_mean`.

| Route and map | Surface error, nine scenes | Surface error, six shallow scenes | Relief height error, six shallow scenes |
| --- | --- | --- | --- |
| Default: analysis depth (Depth Anything 3 mono large) | **3.75°** (2.87-4.89) | **3.00°** (2.53-3.61) | **15.3 %** (10.7-19.8) |
| Default: relief heightmap, compression 1 | 9.79° (5.35-15.30) | 5.03° (3.34-6.75) | 16.6 % (14.6-18.6) |
| Default: relief heightmap, compression 0 | 6.28° (4.35-8.31) | 4.48° (2.94-6.21) | 15.9 % (13.1-18.5) |
| Marigold-only: depth | 8.13° (5.39-10.75) | 6.07° (3.72-9.11) | 16.9 % (11.9-20.9) |
| Marigold-only: its normal map | 4.01° (2.55-5.64) | 4.14° (2.13-6.38) | not applicable |
| #339 benchmark, Depth Anything 3 mono large | 3.75° | 3.00° | 15.3 % |
| #339 benchmark, Marigold V2 depth | 8.13° | 6.07° | 16.9 % |

- **The product reproduces the benchmark.** The default route's analysis
  depth scored exactly the benchmark's Depth Anything 3 mono large numbers on
  every scene: 3.75° over nine scenes and 15.3 % over the six shallow scenes,
  the 3.8° and 15.3 % #339 recorded. The Marigold route's maps scored the
  benchmark's Marigold V2 numbers to within 0.02° on every scene.
- **Cross-check.** The benchmark's own Depth Anything 3 prediction for the
  relief scene, saved as a TIFF and scored by `photo.compare`, gave 3.36° and
  5.69 %, the benchmark report's values for that scene.
- **Surface angle**: the default route's depth (3.75°, 2.87-4.89) is better
  than Marigold V2's depth (8.13°, 5.39-10.75); the intervals do not
  overlap. Marigold V2's own normal map (4.0°) is close to the default
  route's depth-derived normals, as #339 found.
- **Relief height**: 15.3 % (10.7-19.8) against 16.9 % (11.9-20.9). The
  intervals overlap, so this measurement does not show the default route to
  be better or worse on relief height; the difference is not established either way.
- **The written relief scores worse than its analysis.** The heightmap of the
  relief STL compresses slopes on purpose. With the default compression its
  surface error rose to 5.0° on the shallow scenes (9.8° over all nine, where
  the head, table and thin-wire scenes are far from reliefs) and its relief
  error to 16.6 %; with compression 0 to 4.5° and 15.9 %. The former route's
  relief would also have been compressed, but it has no product route to
  measure, so the comparison above is between the routes' depth maps.

All 18 scene reliefs passed `checks.passed`: a sound solid at least one layer
high. With the stone relief and the marble bust on the GPU route, the stone
relief on the CPU route and the stone relief built again for the memory
measurement, 22 of 22 relief STLs passed.

## Objects from the benchmark photographs

**Every request failed: no object was written.** The generators ran, but the
solid rebuild that every object passes through before delivery failed each
time, so no result reached the strict print check and no shell count was
reported. Each failure kept the generated surface beside the requested path,
as documented, under a `NAME.raw-ID.stl` name.

Pixal3D ran on all fifteen benchmark photographs at the defaults (resolution
1024, seed 42, `size_mm` 100, rebuild on), as retained jobs. After every one
of the fifteen Pixal3D results failed, TRELLIS.2 (defaults: resolution 1024,
simplified) was run on five photographs rather than all fifteen, to save
about an hour of GPU time: the first two in alphabetical order and three
clean single-object photographs.

| Model | Photographs | Failed in the rebuild with `geometry` | Failed with `geometry_resource_limit` | Wall time per job | Whole-GPU peak | Worker peak RAM | Disk reads per job |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Pixal3D | 15 | 12 | 3: bicycle spokes, chess clutter, dandelion | 34-74 s (median 38 s) | 3.6-5.7 GiB | 2.2-3.0 GiB | 17.7 GiB |
| TRELLIS.2 | 5: bicycle spokes, bust in gallery, marble bust, porcelain figurine, sneaker | 5 | 0 | 276-354 s | 4.0 GiB | 5.3 GiB | 164 GiB |

GPU figures are the sampled whole-GPU peak less the 416 MiB held before the
job; the failed jobs report no `peak_gpu_memory_bytes`. TRELLIS.2 reads its
transformer blocks from disk repeatedly by design.

- `geometry`: `the extracted surface is invalid: manifold3d status:
  NotManifold`. The rebuild meshes the generated surface's inside on a grid
  (`mesh_solid` in `crates/printable-geom/src/remesh.rs`) and the mesh it
  extracts is rejected by Manifold as not manifold. This happened on clean
  single-object photographs as well as on the expected-difficult ones.
- `geometry_resource_limit` on the three thin-structure or cluttered
  photographs. This status covers the geometry worker exceeding its default
  1 GiB memory budget (`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB`) and other
  unexpected worker exits, so the cause is not yet confirmed.
- The error message suggests `scene` `prepare_print` on the kept surface.
  On the marble bust's kept Pixal3D surface it failed the same way, both at
  the default facet size and at `edge_length_mm` 1.0.

Generation itself was quick: Pixal3D took under 75 s per photograph and
TRELLIS.2 under 6 minutes, within 15 minutes. The failure is in delivery, so
this criterion is not met until the rebuild handles generated surfaces.

## Limits

- One GPU model, one run per configuration; times on other GPUs, processors
  and disks differ. Warm and cold refer to the model files in the page cache,
  not to GPU kernels.
- Peak RAM was sampled every 0.5 s and can miss shorter peaks. The
  out-of-memory kills show that the default GPU analyses exceed 10 GiB.
- The scene comparison uses nine rendered scenes. Its intervals are wide;
  differences inside them are not established.
- The relief and object checks are geometric. No relief or object was
  printed.
