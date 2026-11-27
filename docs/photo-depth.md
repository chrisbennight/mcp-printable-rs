# Depth from a photograph

The `photo` tool's `depth` action estimates how far each pixel of a workspace
photograph is from the camera, and optionally which way each surface faces.
The estimates come from
[Marigold V2](https://github.com/kpbm-ai/marigoldv2), which reproduces fine
detail such as hair, fur and sharp edges. Use them with the
[image-to-mold skill](image-to-mold.md) to turn a photograph into a relief, a
mold master or a displacement map.

The action runs in the optional photograph worker, the same service that
reconstructs FLAME heads, on its GPU. The `depth` action is advertised only
while the worker reports its converted Marigold V2 model present.
The same worker can also prepare a separate set of
[photograph geometry models](photo-geometry.md), with measured
recommendations for choosing among them.

The `depth` action keeps the request and result described here. For float
depth, a subject mask, a focal length and a confidence map that later relief,
lithophane and object work can share, use the `analyze` action described in
[Analyse a photograph](photo-geometry.md#analyse-a-photograph). It caches its
result, so a repeated request with the same photograph and settings returns at
once without running a model.

## What it writes

A request names a workspace photograph and the PNG files to write:

```json
{"action": "depth", "params": {
  "image": "photos/coin.jpg",
  "depth_path": "relief/coin-depth.png",
  "normals_path": "relief/coin-normals.png",
  "max_side": 1024
}}
```

- **Depth map.** A 16-bit greyscale PNG with the nearest point brightest. A
  pixel value `p` means a relative log depth of `far - (far - near) * p / 65535`,
  where the result reports `near` and `far`. Equal steps of relative log depth
  are equal ratios of distance. The scale and offset of the true distances are
  unknown, so the map is an estimate to calibrate, not a measurement.
- **Normal map**, when `normals_path` is given. A 16-bit RGB PNG of unit
  surface normals in the camera's frame: x to the right, y up and z towards
  the camera. Each component `c` is stored as `(c + 1) / 2 * 65535`. Making it
  runs the model a second time.

`max_side`, from 256 to 2048 pixels (default 1024), sets the longest side the
estimate is made at. Larger photographs are scaled down, never up, and both
sides are rounded to multiples of 16 pixels. The maps have the processed size,
which the result reports as `width` and `height`. Existing files are kept
unless `overwrite` is true. Any transparency in the photograph is flattened
onto black.

The result also reports the device, the time taken in `seconds`, the
photograph's SHA-256 in `image_sha256` and, on a GPU,
`peak_gpu_memory_bytes`: the most GPU memory PyTorch held during the job.

## Memory use

Marigold V2 is built on the Qwen-Image-Edit-2509 diffusion transformer, which
has about 20 billion parameters. Marigold V2 holds them as NF4 four-bit
weights and predicts in a single transformer pass. The worker never loads the
transformer whole:

- A one-time conversion rounds every linear weight to NF4, exactly as
  Marigold V2 held it in training, and stores each of the transformer's 60
  blocks in its own file.
- While the transformer runs, each block, about 340 million parameters, is
  read from disk, expanded to 16-bit weights on the GPU, evaluated and
  released. The next block is read from disk in the background meanwhile.
- Each task's low-rank adapter is applied beside the layers it adapts, as in
  training, rather than merged into their weights.
- The VAE that encodes the photograph and decodes the result works in
  overlapping tiles of 512 pixels.
- The worker runs one photograph job at a time, shared with head
  reconstruction. Each job runs in its own process, which returns all of its
  memory when it exits.

Peak GPU memory is therefore one block, the small layers outside the blocks,
the VAE and the activations, rather than the whole model. On an NVIDIA GeForce
RTX 4060 Ti with 16 GB, the depth and normal maps of a 1024 × 688 photograph
took 66 seconds together and peaked at 1.8 GB of GPU memory. Other GPUs and
photograph sizes differ; read `peak_gpu_memory_bytes` from your own results.

The price is disk reading. Each map reads the whole converted transformer and
the task's adapter, about 13 GB, once; a request with normals reads them
twice. The operating
system's file cache keeps it between requests when the host has memory to
spare. The worker process itself holds at most two blocks' stored weights,
a few hundred megabytes, besides the VAE and the photograph.

## Supply the model

The model files are not included. Both downloads are published under the
Apache License 2.0. Download the Qwen-Image-Edit-2509 transformer and VAE,
about 41 GB, and the Marigold V2 depth and normal checkpoints with their
prompts, about 4 GB. These commands use the Hugging Face command-line client:

```sh
hf download Qwen/Qwen-Image-Edit-2509 --include "transformer/*" --include "vae/*" \
  --local-dir downloads/Qwen-Image-Edit-2509
hf download huawei-bayerlab/marigold-v2-0 \
  depth/Log-stage2/trainables.safetensors normals/trainables.safetensors \
  qwen_text_embeddings/qwen_edit_2509_qwen_depth_realimg512_prompt_embeds.pt \
  qwen_text_embeddings/qwen_edit_2509_qwen_depth_realimg512_prompt_mask.pt \
  qwen_text_embeddings/qwen_edit_2509_qwen_normals_dummy512_prompt_embeds.pt \
  qwen_text_embeddings/qwen_edit_2509_qwen_normals_dummy512_prompt_mask.pt \
  --local-dir downloads/marigold-v2-0
```

Convert them once with the converter in the worker image. Build the image as
[Run the worker](flame-models.md#run-the-worker) describes, then run:

```sh
mkdir -p marigold
docker run --rm --network none --user "$(id -u):$(id -g)" \
  --entrypoint /opt/recon/bin/python \
  -v "$PWD/downloads:/downloads:ro" -v "$PWD/marigold:/output" \
  printable-recon:local /opt/printable/recon/convert_marigold.py \
  /downloads/Qwen-Image-Edit-2509 /downloads/marigold-v2-0 /output/marigold-v2
```

The converter reads one tensor at a time, so it needs little memory. It
writes the output under a temporary name and renames it once complete, and it
refuses to replace an existing directory. The converted directory takes about
16 GB; the downloads may be deleted afterwards. The prompts are PyTorch
pickles; the converter opens them with PyTorch's weights-only loader, which
builds tensors only and never runs code from the file.

## Run the worker

Mount the converted directory into the photograph worker, read-only, at
`/models/marigold`, or name another path with `PRINTABLE_RECON_DEPTH_MODEL`.
Add this to the `recon-worker` service from
[Run the worker](flame-models.md#run-the-worker):

```yaml
services:
  recon-worker:
    volumes:
      - ./marigold/marigold-v2:/models/marigold:ro
```

The FLAME checkpoints are not needed for depth: the worker offers whichever
jobs have their files present. Depth estimation needs a CUDA GPU: give the
worker container one, as the GPU reservation in
[Run the worker](flame-models.md#run-the-worker) does; the published image
carries PyTorch built for CUDA 13.0. When the worker's device is `auto` and no GPU is
visible, a depth request fails with `unavailable` rather than running for
hours on the processor; setting `PRINTABLE_RECON_DEVICE` to `cpu` forces the
processor.

Each depth request must finish within
`PRINTABLE_RECON_DEPTH_TIMEOUT_SECONDS`, 900 by default and at most 3600. The
`status` tool reports `depth.configured` and `depth.ready`, and the worker's
`/readyz` answer lists any missing model files under `missing.depth`.

## Turn depth into a relief

Load the [image-to-mold skill](image-to-mold.md) and follow its photograph
guidance. In short:

1. Estimate depth, and normals if they will help with fine detail.
2. Choose the part of the depth range that becomes the relief, and which
   depth becomes the flat background.
3. Map that range to heights in millimetres. A subject with large depth jumps
   needs its big steps compressed so that fine detail stays visible on a
   shallow relief.
4. Save the calibrated heights with the skill's 16-bit decoding rule, then
   build, validate and slice the geometry as usual.

## Relief geometry

The `relief` module of the `printable-geom` crate turns an analysis into a
printable relief solid. It is a library: no tool calls it yet, and it reads
and writes no files. It runs in four stages.

1. **Height field** (`integrate`). Depth alone is the default: each subject
   pixel's height is its distance from the subject's farthest point. Two
   other methods are available on request and are not yet measured against
   depth alone on the benchmark scenes:
   - `AnchoredNormals` takes fine detail from the normals and broad shape
     from the depth. It needs normals and a focal length. Below about
     `anchor_sigma_px` pixels (default 4) the normals decide the shape, and
     above it the depth does. Each depth value is weighted by its
     confidence, if one is given. The method follows bilateral normal
     integration: neither side of a pixel is pulled across an edge it does
     not continue over. It solves for log depth, so the depth map's unknown
     scale does not matter.
   - `FrankotChellappa` is a fast preview from the normals alone, with no
     depth anchor. Without a focal length it assumes an orthographic camera.

   Neighbouring pixels whose depths imply a slope steeper than
   `max_jump_slope` (default 10, about 84 degrees from facing the camera)
   lie on two sides of an occlusion. Anchored integration solves each side
   separately and the depth sets each side's level; the Frankot-Chellappa
   preview only removes the slope across the step and solves the whole
   image together. Without a focal length, the longest
   image side in pixels stands in for it. Analyses larger than `max_pixels`
   (default 2048 x 2048) are refused with the `relief_too_large` error code.
2. **Compression** (`compress`). Steps at the subject's outline and at depth
   jumps are removed. Slopes are divided by their mean, then compressed by
   `ln(1 + compression * s) / (compression * s)`, where `s` is the slope
   averaged over `detail_sigma_px` (default 4 pixels, at most 64). Detail finer than
   that is further multiplied by `detail_gain` (default 1). The slopes are
   integrated again with the background held flat, and the result is scaled
   to `relief_depth_mm` over the requested `width_mm` (and `height_mm`, or
   square pixels). The default compression of 1 is a starting point, not a
   measured optimum; 0 keeps slopes in proportion.
3. **Solid** (`relief_solid`). The relief faces +Z on a flat base on Z = 0,
   `base_mm` thick (default 2 mm). It can have a flat border, and it can be
   mirrored and inverted to make a casting mould (`SolidOptions::mould`).
   The image's top row is at the largest Y. A mirrored relief is an exact
   reflection of the unmirrored one. Every edge joins exactly two
   triangles, so the solid is watertight by construction.
4. **Checks** (`check_relief`). The solid is a height field over a regular
   grid, with vertical side walls and a flat bottom, so it is closed and
   cannot intersect itself when its top lies above its bottom. The check
   confirms that the top samples lie on a strictly increasing grid above
   the bottom and that the volume is positive, instead of running the mesh
   analysis's pairwise self-intersection search, which cannot finish on
   the largest reliefs. The relief's height range is compared with a
   minimum (default one 0.2 mm layer) and an optional maximum. Ridges and grooves narrower
   than the nozzle (default 0.4 mm) are reported where they would lose more
   than one layer of height; detail finer than the sample spacing is not
   assessed. A relief is a height field, so a mould made from it releases
   along the relief's axis without undercuts. For a mould, pass the inverted
   relief, as the `relief` example does, so the narrow detail is that of the
   cavity.

### Try it on saved maps

The `relief` example reads raw little-endian arrays and writes the height
field, the relief in millimetres and an STL:

```sh
cargo run --release -p printable-geom --example relief -- \
    --size 1024 768 --depth depth.f32 --normals normals.f32 \
    --mask mask.u8 --focal 1100 --method anchored --check \
    --heights-out heights.f32 --relief-out relief.f32 --stl relief.stl
```

Write the inputs from NumPy with `array.astype("<f4").tofile(path)` and
`mask.astype("u1").tofile(path)`. Normals are three floats per pixel, in the
convention above. Add `--log-depth` for a map of log depth. `--method` also
accepts `depth-only` (the default) and `frankot-chellappa`.

### Measured cost

`--synthetic 2048` runs the whole pipeline on a generated 2048 x 2048
scene: a sphere in front of a wall, with 0.4 % depth noise. One run in
release mode on an Apple M6 (arm64, macOS 27, single-threaded), measured
with `/usr/bin/time -l`, including writing the STL:

| Method | Height field | Compression | Total | Peak resident memory |
| --- | --- | --- | --- | --- |
| Depth alone | 0.04 s | 20.3 s | 20.6 s | 0.96 GB |
| Frankot-Chellappa | 0.14 s | 27.0 s | 27.4 s | 1.08 GB |
| Anchored normals | 1.9 s | 27.3 s | 29.5 s | 1.03 GB |

Building the solid took 0.02 s for 8.4 million triangles; without
writing the STL, the anchored run peaked at 0.67 GB. These numbers were
not measured on the supported Linux/amd64 platform. `check_relief`
certifies the solid from its construction, so it does not depend on the
mesh analysis's self-intersection search, which stops at its budget on
2048 x 2048 reliefs.

## Limits

- The maps are learned estimates. Reflective, transparent and very dark
  surfaces, strong shadows and flat artwork mislead them. A photograph of a
  flat print is estimated as a flat print.
- Depth is relative. Only calibration against known sizes or chosen heights
  turns it into millimetres.
- One photograph shows one side. A depth map holds one height per pixel and
  cannot describe undercuts or hidden surfaces.
- Detail finer than the processed pixels is lost. A larger `max_side` keeps
  more detail and takes longer and more GPU memory.
