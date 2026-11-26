# Depth benchmark, 26 November 2026

Measured with [the depth benchmark](../scripts/depth_benchmark/README.md) to
choose the photograph worker's depth, normal, mask and focal-length models.
Quality decides the default; memory and time decide only the fallbacks.
Full numbers for every tier are in [depth-benchmark.json](depth-benchmark.json),
where each table is stored as a list of columns and a list of rows.

## Environment

- One NVIDIA RTX 4090 (24 GB) and a 32-thread CPU, Linux.
- Container `pytorch/pytorch:2.14.1-cuda13.0-cudnn9-devel`, the photograph
  worker's PyTorch and CUDA versions; Blender 5.3.0 for the scenes.
- Marigold V2 ran through `recon/printable_recon/marigold.py` after
  conversion with `recon/convert_marigold.py`, reading from a local SSD.
- Model code at the revisions pinned in the benchmark's Dockerfile; Python
  packages as listed in [depth-benchmark-packages.txt](depth-benchmark-packages.txt);
  weights at the revisions and SHA-256 digests pinned in
  [checkpoints.py](../scripts/depth_benchmark/checkpoints.py) and recorded in
  the JSON.
- Inputs at 1024 pixels on the longest side: nine rendered scenes with exact
  depth and normals, and fifteen photographs. Tiers: `gpu` (no limit),
  `budget` (3 GB allocator limit) and `cpu` (no GPU visible). On the `cpu`
  tier every model scores the same three scenes (relief, coin, head); models
  other than Marigold V2 also run two photographs.

![Each model's normals rendered with side lighting, one row per scene](../docs/images/depth-benchmark-scenes.jpg)

Each row is a scene; each column shows a model's surface lit from the upper
left (its own normals where it has them, otherwise normals from its depth).

## Quality on the rendered scenes (GPU)

Means over nine scenes; 95 % bootstrap intervals in brackets. Lower is better
except edge F1.

| Model | Model normals, deg | Normals from depth, deg | Relief height error, % | Depth error, % | Edge F1 |
| --- | --- | --- | --- | --- | --- |
| Marigold V2 | **4.0** (2.6-5.6) | 8.1 (5.3-10.8) | 16.0 (11.5-20.3) | 0.83 | 0.99 |
| MoGe-3 ViT-L | 5.4 (3.0-8.2) | 11.7 (7.0-17.5) | 15.1 (10.7-20.6) | 0.99 | 0.97 |
| MoGe-3 ViT-g | 5.9 (3.2-9.3) | 9.4 (5.3-14.5) | 15.2 (9.8-21.1) | 0.92 | 0.98 |
| MoGe-2 ViT-L normal | 6.0 (3.5-9.1) | 14.5 (7.8-23.5) | **12.7** (9.3-16.9) | 1.04 | 0.92 |
| MoGe-2 ViT-B normal | 7.6 (3.9-12.4) | 13.6 (8.3-20.6) | 16.5 (10.5-24.6) | 1.29 | 0.94 |
| MoGe-2 ViT-S normal | 9.6 (5.2-15.2) | 18.0 (11.2-26.0) | 20.5 (15.5-25.5) | 1.51 | 0.88 |
| Depth Anything 3 mono large | none | **3.8** (2.9-4.9) | 15.0 (11.0-18.7) | **0.75** | 0.93 |
| Depth Anything 3 nested giant | none | 9.2 (6.2-12.7) | 14.3 (9.9-18.4) | 1.02 | 0.86 |
| Depth Anything 3 base | none | 9.6 (6.5-12.6) | 23.5 (16.9-31.7) | 1.48 | 0.76 |
| Depth Anything 3 small | none | 13.0 (8.8-17.4) | 23.8 (20.2-27.5) | 2.06 | 0.68 |
| Depth Pro 2 H+ | none | 14.5 (9.8-19.4) | 23.5 (12.3-38.9) | 1.11 | **0.99** |
| Depth Pro 2 L | none | 14.4 (9.6-19.8) | 14.9 (11.0-18.5) | 1.02 | 0.99 |
| Depth Pro 2 B | none | 18.9 (13.6-25.0) | 17.6 (12.9-22.0) | 1.66 | 0.98 |
| Depth Pro 2 S | none | 28.8 (21.1-36.8) | 20.3 (14.9-25.3) | 2.43 | 0.97 |

Relief height error is measured in each relief region after the scale and
offset a user would choose and removal of a best-fit plane, as a percentage of
the region's relief depth. Depth error is after scale and offset alignment.
Alignment never reverses depth: a prediction with near and far swapped scores
as a flat one. The `budget` tier gives the same quality for every model that
fits; the `cpu` tier's three-scene means are in the JSON.

## Combining depth and normals

Fusion takes broad shape from a depth map and detail from integrated normals;
near depth jumps it uses the depth map's own gradients instead of the normals,
but one integration still spans the whole image, so the two sides of an
occlusion are not fully separated; sigma is the blur, in pixels,
separating the two. "Normals dominate" is the widest blur: it keeps the depth
map's mean level, its per-pixel scale and its gradients at depth jumps, and
takes everything else from the normals, so it still depends on the depth model.
It
uses no ground truth: each depth model's own units and Depth Pro 2 L's
focal-length estimate. The result is then scored like a relief the user
scales. Means over the seven scenes with a relief region; 95 % intervals in
brackets.

| Tier | Depth | Normals | Surface error, deg | Height error, mm |
| --- | --- | --- | --- | --- |
| gpu | Depth Anything 3 mono large | none (depth only) | **3.8** (2.6-5.6) | 8.1 (0.4-23.1) |
| gpu | MoGe-3 ViT-g | none (depth only) | 6.1 (3.9-8.7) | 5.5 (0.4-15.2) |
| gpu | MoGe-3 ViT-g | MoGe-3 ViT-g, sigma 4 | 5.7 (3.3-8.6) | 5.9 (0.4-16.4) |
| gpu | MoGe-3 ViT-g | Marigold V2, sigma 4 | 5.7 (3.3-8.5) | 5.9 (0.4-16.4) |
| gpu | Depth Anything 3 nested giant | Marigold V2, normals dominate | 4.4 (1.9-7.6) | 10.5 (0.5-29.6) |
| gpu | Depth Pro 2 H+ | Marigold V2, normals dominate | 5.1 (2.1-9.5) | 7.3 (0.5-20.7) |
| gpu | Depth Anything 3 nested giant | MoGe-3 ViT-g, sigma 16 | 5.1 (2.7-8.5) | 9.0 (0.4-25.9) |
| gpu | Marigold V2 | none (depth only) | 8.6 (5.0-13.2) | 9.0 (0.7-25.4) |
| gpu | Marigold V2 | Marigold V2, sigma 4 | 7.6 (4.6-11.2) | 8.4 (0.7-23.6) |
| budget | MoGe-3 ViT-L | MoGe-3 ViT-L, sigma 4 | 7.6 (4.2-11.6) | 7.2 (0.4-20.4) |
| budget | Depth Pro 2 L | none (depth only) | 8.6 (5.3-12.2) | 5.9 (0.4-16.5) |
| budget | Depth Pro 2 L | Marigold V2, sigma 4 | 7.8 (4.3-11.8) | 6.6 (0.4-18.6) |
| cpu | Depth Anything 3 mono large | none (depth only) | 5.1 (2.7-9.1) | 18.2 (0.4-53.2) |
| cpu | Depth Pro 2 L | none (depth only) | 6.6 (3.9-10.6) | 13.1 (0.4-37.6) |
| cpu | Depth Pro 2 L | Marigold V2, sigma 4 | 6.3 (3.8-10.1) | 14.8 (0.4-42.6) |
| cpu | Marigold V2 | Marigold V2, sigma 4 | 9.2 (4.2-14.9) | 18.2 (0.6-52.8) |

The `cpu` rows are three scenes, not seven, so they compare only with each
other. Marigold V2's depth carries no fixed scale or exponent, so in its own
units its normals and depth disagree in magnitude; without ground truth to
reconcile them, its own fusion is the weakest of the leading pairs. With
MoGe-3 ViT-g depth at sigma 4 the choice of normal model makes no measurable
difference. Depth Anything 3 mono large gives the best surface direction from
its depth alone, better than any fusion, but worse height on the deep head
scene; on the six shallow scenes its relief error (15.3 %, 10.6-19.8) ties the
leaders (MoGe-3 ViT-g 16.3 %, Depth Pro 2 L 15.8 %). Height intervals are wide
because the head scene's errors, in millimetres, dominate them; no pair
separates on height.

## Input size: 1024 or 2048 pixels

The nine scenes were rendered again at 2048 x 1536 and every model ran on the
GPU at 2048 pixels (Depth Anything 3 processing at 2044). Paired by scene,
2048 pixels gave no measured quality gain for shallow subjects and made some
models worse:

- Marigold V2's normals: 4.0 to 7.6 degrees (change +3.6, interval +2.4 to
  +4.8); its relief error rose from 16.0 % to 21.8 %.
- MoGe-3 and MoGe-2: unchanged within a quarter of a degree, as they resize
  their input internally; MoGe-3 ViT-g fused at sigma 4 scored 5.8 degrees and
  5.6 mm.
- Depth Anything 3 and Depth Pro 2: relief error on the shallow scenes
  unchanged; surface direction from depth worse (Depth Anything 3 mono large
  3.8 to 4.9 degrees), partly because finer pixels make differentiated depth
  noisier.
- Deep subjects: on the head scene only, Depth Pro 2 H+ height error fell from
  34.5 to 21.1 mm and Depth Anything 3 mono large from 53 to 44 mm; over all
  seven relief scenes the change includes zero (Depth Pro 2 H+ -1.9 mm,
  -5.8 to +0.1).

Shallow subjects are therefore processed at 1024 pixels on the longest side;
larger photographs are reduced first and the result scaled to the output.
Deep subjects on a GPU use Depth Pro 2 H+ at 2048 pixels, the best height
measured on the head scene.

## Focal length

Mean absolute error against EXIF on the four photographs that record a 35 mm
equivalent focal length: Depth Pro 2 8.3 % (every size, which share one
field-of-view estimator), MoGe-2 ViT-L 20.4 %, MoGe-3 ViT-L 22.6 %, MoGe-3
ViT-g 32.3 %, Depth Anything 3 nested giant 37.6 %. On the scenes, every
metric model misjudged the distance of close-up subjects by several times.

## Cost

Median per image, excluding model loading and one warm-up image; peak GPU
memory is allocator memory. Storage reads were zero because model files were
already in the page cache; a cold Marigold V2 read is about 13 GB per map, as
[the depth guide](../docs/photo-depth.md) states.

| Model | GPU, 1024 px | GPU, 2048 px | Within 3 GB | CPU, 1024 px | CPU, 2048 px |
| --- | --- | --- | --- | --- | --- |
| MoGe-3 ViT-g | 0.56 s, 6.7 GB | 0.65 s, 6.7 GB | no | does not run | does not run |
| MoGe-3 ViT-L | 0.26 s, 2.5 GB | 0.35 s, 2.5 GB | yes, both sizes | does not run | does not run |
| MoGe-2 ViT-L normal | 0.06 s, 2.3 GB | 0.07 s, 2.3 GB | yes, both sizes | 7.3 s, 3.5 GB | 5.5 s, 3.5 GB |
| Depth Pro 2 L | 0.05 s, 3.1 GB | 0.22 s, 6.5 GB | 1024 px only (2.4 GB) | 8.1 s, 4.3 GB | 29 s, 7.4 GB |
| Depth Pro 2 H+ | 0.09 s, 5.2 GB | 0.40 s, 8.6 GB (10.1 GB on the 2048 x 1536 scenes) | no | 14 s, 7.4 GB | 47 s, 9.5 GB |
| Depth Anything 3 mono large | 0.07 s, 2.4 GB | 0.35 s, 4.8 GB | 1024 px only | 13 s, 3.8 GB | 68 s, 6.2 GB |
| Depth Anything 3 nested giant | 0.26 s, 8.7 GB | 1.3 s, 13.5 GB | no | 60 s, 13.4 GB | 282 s, 13.7 GB |
| Marigold V2 (depth and normals) | 6.2 s, 2.0 GB | 12.4 s, 2.5 GB | 1024 px; one of three 2048 px photographs ran out | 303 s, 4.0 GB | 935 s, 5.8 GB |

CPU memory is peak resident memory. MoGe-3 depends on Triton and does not
load without an NVIDIA GPU. Marigold V2 on the CPU exceeds 15 minutes per map
at 2048 pixels.

## Chosen routes

Routes depend on the subject: shallow subjects (reliefs, coins, lettering,
artwork) need surface direction; deep subjects (objects, figures) need overall
height. The deep-subject route rests on one scene, the head, and is
provisional until more deep scenes are measured.

| Role | GPU | Within 3 GB of GPU memory | CPU only |
| --- | --- | --- | --- |
| Shallow subjects (1024 px) | Depth Anything 3 mono large depth | the same | the same (13 s per map) |
| Deep subjects | Depth Pro 2 H+ depth at 2048 px | Depth Pro 2 L depth at 1024 px | Depth Pro 2 H+ depth at 1024 px (14 s per map) |
| Subject mask | MoGe-3 ViT-g | MoGe-3 ViT-L | MoGe-2 ViT-L normal |
| Focal length | Depth Pro 2 field-of-view estimator | the same | the same |
| Normal-map output (optional) | Marigold V2 at 1024 px | the same | none |

On the head scene, height error was 21.1 mm for Depth Pro 2 H+ at 2048
pixels, against 34.5 mm at 1024 pixels and 34.6 mm for MoGe-3 ViT-g depth;
fusing normals made deep height worse (MoGe-3 ViT-g at sigma 4: 37.3 mm).
Within 3 GB, Depth Pro 2 L depth scored 37.6 mm against 46.8 mm for MoGe-3
ViT-L fused with its normals. On the CPU, Depth Pro 2 H+ scored 33.2 mm.
Depth Pro 2 also supplies focal length from the same pass on these routes.

Models run one at a time, so a route's peak is its largest model's peak over
every input. The GPU routes peak at 10.1 GB for deep subjects (Depth Pro 2 H+
at 2048 pixels, on the 2048 x 1536 scenes) and 6.7 GB for shallow ones (the
MoGe-3 ViT-g mask; the depth model needs 2.4 GB). The 3 GB routes peak at
2.7 GB (the MoGe-3 ViT-L mask). The CPU routes take under 25 seconds of
inference per map, plus about 8 seconds for the mask, and peak at 7.4 GB of
resident memory (Depth Pro 2 H+). Peak GPU memory is measured after
the model is loaded, so it includes the resident weights but not transient
loading. Marigold V2's own normals remain the most accurate normal maps
measured (4.0 degrees at 1024 pixels) and are kept for normal-map output; it
is not needed for depth.

The subject-mask choice is provisional: masks are saved with each prediction
but not yet scored, and will be measured when `photo.analyze` is built.

## Limits

- Nine computer-generated scenes, seven with a relief region; intervals
  overlap for most close pairs, and height error does not separate the leading
  combinations.
- The CPU tier scores three scenes per model.
- The 2048-pixel comparison covers the GPU tier only; its scenes are separate
  renders of the same geometry. `compare_sides.py` reproduces it from the two
  runs' `metrics.json` files.
- One deep scene: the deep-subject route is provisional.
- The engraved coin (1 mm engraving) was poorly recovered by every model.
- Photographs were judged visually except for focal length.
- One run per image; DSINE was not measured because its weights are published
  only through a file-sharing link, without a verifiable checksum.
