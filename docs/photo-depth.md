# Depth from a photograph

The `photo` tool's `depth` action estimates how far each pixel of a workspace
photograph is from the camera, and optionally which way each surface faces.
The estimates come from
[Marigold V2](https://github.com/kpbm-ai/marigoldv2), which reproduces fine
detail such as hair, fur and sharp edges. Use them with the
[image-to-mold skill](image-to-mold.md) to turn a photograph into a relief, a
mold master or a displacement map.

The action runs in the optional photograph worker, the same service that
reconstructs FLAME heads, on its GPU. The `photo` tool is advertised only
while the worker reports its converted Marigold V2 model present.

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
the VAE and the activations, rather than the whole model. This design is
expected to stay within a few gigabytes at the default size. That figure has
not been measured on a GPU: this repository's tests run on the processor with
a miniature model. Read `peak_gpu_memory_bytes` from your own first results.

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
carries PyTorch built for CUDA 12.8. When the worker's device is `auto` and no GPU is
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
