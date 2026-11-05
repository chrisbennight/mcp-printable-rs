# An object from a photograph

The `photo` tool's `object` action generates the 3D shape of the one object,
figure or character in a workspace photograph and writes it as an STL.
By default the shape comes from [Pixal3D](https://github.com/TencentARC/Pixal3D), which
aligns the generated geometry with the picture's pixels. Only the shape is
generated: Pixal3D's colour stages are never run, because a single-colour
print does not need them and skipping them saves time and memory.

The action runs in the optional photograph worker, the same service that
estimates depth and reconstructs FLAME heads, on its GPU. The `photo` tool
offers `object` while the worker reports a usable Pixal3D model or a configured
[TRELLIS.2 runtime](#trellis2). Status lists the available object models.

## What it writes

A request names a workspace photograph and the STL to write:

```json
{"action": "object", "params": {
  "image": "photos/figure.jpg",
  "mesh_path": "objects/figure.stl",
  "resolution": 1024,
  "size_mm": 120
}}
```

The STL is binary, in millimetres, with Z up and the subject facing -Y, as the
camera saw it. It is centred on the origin and stands on Z = 0. Its longest
side is `size_mm` long, 5 to 2000 mm (default 100). Existing files are kept
unless `overwrite` is true.

The other choices are:

- `model`: `pixal3d` (the default) or `trellis2`, when configured. Their image
  conditioning differs, so they can produce different plausible interpretations
  of the same photograph. Existing requests continue to use Pixal3D.
- `resolution`: 512, 1024 (default) or 1536 voxels across the surface. Higher
  resolutions keep finer detail and take longer, use more GPU memory and give
  larger meshes. At 1536 the shape is coarsened in steps of 128 towards 1024
  while it has 49,152 or more voxels in its last generation stage, as Pixal3D
  does.
- `steps`: sampling steps for each generation stage, 1 to 50. The default is
  the published pipeline's, 12. Fewer steps are faster and rougher.
- `seed`: the generator's noise (default 42). Another seed gives another
  plausible shape, which helps when the first one is wrong.
- `fov_degrees`: the horizontal field of view, 5 to 90 degrees, of the square
  that frames the subject. See [Camera](#camera).
- `target_triangles`: the triangles the mesh is simplified towards, 10,000
  to 4,000,000 (default 1,000,000, as Pixal3D's own export uses). See
  [Mesh size](#mesh-size).
- `simplify`: TRELLIS.2 only, true by default. False writes every generated
  triangle. Omit `target_triangles` when false. Full-detail files can exceed 1 GB.
- `background`: true runs Pixal3D as a retained job. By default Pixal3D returns
  its result in the same request. TRELLIS.2 always uses a retained job;
  `background: false` is rejected for that model.

The result reports the triangle count written and the count generated, the
mesh's size in millimetres, the
resolution reached, the steps used, the voxel counts of the structure and last
shape stages, the seed, the field of view and where it came from, whether the
background was removed, the device, the time taken, each stage's time and,
on a GPU, its peak memory, the photograph's SHA-256 and the written
artifact.

## Retained generation jobs

TRELLIS.2 returns a `job_id` promptly, including when the model files still need
downloading. The worker saves the photograph, request and resource settings
before admitting the job. Editing the original photograph or disconnecting the
client does not change or cancel it. Pixal3D supports the same workflow with
`background: true`.

Use the `photo` tool to inspect the job:

```json
{"action": "status", "params": {"job_id": "<returned job_id>"}}
```

Status reports the state and current phase, such as preparing models, waiting
for the GPU, generating or delivering. A successful job includes the result and
artifact. Add `detail: true` to inspect its retained request and source hash.
`list` accepts `offset` and `limit` to discover recent jobs. `cancel` accepts a
`job_id`; cancellation is available until publication starts. The job becomes
`cancelled` after its subprocess has been killed and reaped. Check the returned
`cancellable` and `cancellation_requested` fields rather than assuming that a
cancellation request deleted an output.

The worker runs one GPU operation at a time. Model downloads happen before
acquiring that GPU slot. `PRINTABLE_RECON_PHOTO_QUEUE_DEPTH` (default 8) bounds
the admitted queued and running object jobs; a full queue reports `busy`.
Retained photographs and job metadata count towards workspace storage usage.
An admission whose persistence cannot be confirmed returns `outcome_unknown`
with its job identifier and does not start generation. Inspect the retained
storage before submitting again.

Jobs survive client disconnects, but execution is never replayed automatically
after a worker restart. Unfinished work becomes `interrupted`; a restart during
publication reports `outcome_unknown`. Inspect `mesh_path` before submitting
again. Job history remains discoverable when generation models are absent.
These identifiers belong to `photo`; rendering jobs use the separate `job` tool.

## Mesh size

The generator's surface follows its voxels, so its triangle count grows with
the subject's detail and is expected to grow about fourfold with each
doubling of `resolution`. A detailed figure generated on the processor at
512 had 4.6 million triangles, a 230 MB STL, so at 1024 such a subject is
expected to have tens of millions. The worker therefore simplifies the surface before writing it:

- A surface above 4 million triangles is first coarsened by merging the
  vertices in each cell of a grid a few voxels across, which needs little
  memory and runs where the surface was made.
- Quadric edge collapse then brings it to `target_triangles`, keeping the
  shape's form where it removes triangles. It needs about 350 bytes of
  processor memory per triangle, so at most about 1.5 GB.
- Simplification stops early on parts it cannot simplify without folding the
  surface, so a low target may leave more triangles than asked. The result
  reports the count written.

On that figure, simplifying 4.6 million triangles to 1 million took about
2 seconds on the processor and kept its detail visibly intact. Raise
`target_triangles` for a large print with fine detail; lower it for a quick
look or a small print.

## TRELLIS.2

Select the optional shape generator explicitly:

```json
{"action": "object", "params": {
  "model": "trellis2",
  "image": "photos/figure.png",
  "mesh_path": "objects/figure-trellis.stl",
  "resolution": 1024,
  "size_mm": 120,
  "simplify": true
}}
```

This path uses the pinned [TRELLIS.2](https://github.com/microsoft/TRELLIS.2)
shape networks, DINOv3 image conditioning and Printable's existing BiRefNet
background removal. Texture generation does not run. A photograph with real
transparency supplies its own foreground mask. `fov_degrees` applies only to
Pixal3D and is rejected for TRELLIS.2.

Inactive transformer weights stay on disk. Each active block is read directly
into GPU memory and released after execution, without retaining a CPU weight
copy. Tiled decoding and chunked mesh extraction bound temporary tensors.
Background removal, generation and mesh delivery run in separate processes so
each releases its memory before the next begins. Simplified delivery coarsens
a preparation copy using disk-based sorting and bounded buffers before the
bounded final simplifier; full-detail
delivery streams every original triangle without coarsening. Neither path
repairs the generated surface or proves it is printable.
Tiled decoding can produce small numerical and topology differences from
untiled upstream inference. Inspect fine features before printing.

The default PyTorch allocation budget is 4096 MiB. This is an allocator budget,
not a limit on total driver memory, and Docker's GPU reservation does not impose
a VRAM limit. The runtime requires Linux/amd64 and an Ampere or newer NVIDIA GPU
with a driver compatible with the image's CUDA 13 build. Resolution, subject
complexity, driver overhead and other GPU users affect whether a job fits.
An out-of-memory failure is reported; the worker does not silently reduce
requested detail. The upstream cascade may still report a lower achieved
resolution when its token budget requires it.

Results report the peak process resident memory (`peak_ram_bytes`), peak PyTorch
GPU allocation (`peak_gpu_memory_bytes`), process times and inference stage
times. Process resident memory includes model loading and mapped checkpoint
pages. It does not measure the container's aggregate file cache or total driver
allocation; use host/container measurements when qualifying a resource profile.
CPU-only CI checks the software contracts and does not establish GPU performance.

### Persistent models and scratch storage

Enable the runtime by setting `PRINTABLE_RECON_TRELLIS_CACHE` in the photograph
worker. Map that directory and `PRINTABLE_RECON_TRELLIS_SCRATCH` (default
`/scratch/trellis2`) to disk volumes writable by uid 10001. Checkpoints are absent
from the image. On first use the worker downloads the files in
[the pinned model manifest](../recon/models/trellis2.json), verifies every size
and SHA-256, then publishes one complete snapshot. Failed downloads retain
partial files for retry; a complete verified snapshot can run offline.
The server's `status` tool reports preparation as `not_prepared`, `preparing`,
`prepared`, `failed` or `interrupted`, with the time of the last completed
verification when available. This status query does not hash the checkpoints
or contact the network. Every generation verifies them again before use, so
`prepared` describes the last verification rather than a guarantee that files
have not subsequently changed.

The manifest needs approximately 11 GB for shape and image model data. Allow
at least 20 GB of scratch capacity for high-detail generation and delivery,
plus space for the final workspace artifact and its atomic publication copy.
These are initial provisioning allowances, not a measured upper bound. Model
revisions have separate snapshots; keep room for both during upgrades. Fast
local SSD storage is recommended because disk streaming repeatedly rereads
transformer blocks. Slow disks can substantially increase job time. Large
arrays and output files must not use a RAM-backed `/tmp`.
The scratch volume also retains Triton and CUDA kernel caches between jobs.
The image supplies the compiler and headers Triton needs for its first-use
kernel launchers; the CUDA extension build toolkit stays in the build stage.

Only model preparation receives `HF_ENDPOINT`, an operator-configured
Hugging Face base URL or compatible proxy. Its default is
`https://huggingface.co`; URLs containing credentials, a query or a fragment
are rejected. Configure network access and sufficient download timeouts for
first use. Inference processes receive no Hub credentials and run offline.

The official [DINOv3 model](https://huggingface.co/facebook/dinov3-vitl16-pretrain-lvd1689m)
has separate license and access requirements. If needed, inject an authorized
`HF_TOKEN` through the deployment's secret mechanism, only into the worker's
process environment. A configured proxy may handle authorization itself. The
worker also reuses byte-identical DINOv3 and BiRefNet files from an existing
`PRINTABLE_RECON_OBJECT_MODEL` mount, copying and verifying them into its cache.
It never downloads Python code at run time.

[compose.photo.yaml](../compose.photo.yaml) is an optional overlay for the
standard installation. Set `PRINTABLE_RECON_IMAGE` to the matching immutable
reconstruction image from the same release record as the other services, then
use `docker compose -f compose.yaml -f compose.photo.yaml up -d`. The overlay
provides disk-backed named volumes, download egress, 8 CPUs and a 10 GiB memory
limit with no additional swap. These are configurable starting allocations;
qualify them against actual workloads and reserve disk capacity separately.
It adds no public worker port.

Worker settings include `PRINTABLE_RECON_TRELLIS_VRAM_MIB` (default 4096),
`PRINTABLE_RECON_TRELLIS_TIMEOUT_SECONDS` (default 21600 for inference and
delivery together), and `PRINTABLE_RECON_TRELLIS_PREPARATION_TIMEOUT_SECONDS`
(default 86400 for model preparation). The server reads process environment;
it does not load `.env.example` automatically.

## Photographs that work

- One subject, whole in the frame, against a background that contrasts with
  it. The background is removed with BiRefNet unless the photograph already
  has transparency, in which case its alpha channel marks the subject.
- Even light, no strong shadows, and a view that shows the subject's
  character. The sides the camera did not see are the model's invention.
- Toys, figures, sculptures, animals, furniture and everyday objects suit it.
  Very thin parts, transparent or mirror-like surfaces and scenes of several
  objects do not.

If the background removal finds no subject, the request fails with
`no_shape`. Try a photograph with a plainer background, or one that already
has transparency.

## Camera

Pixal3D places the subject in a cube seen through a pinhole camera, and the
field of view decides how much perspective the shape inherits from the
photograph. The worker chooses it in this order:

1. `fov_degrees` in the request.
2. The photograph's 35 mm equivalent focal length, from its Exif metadata,
   scaled to the part of the picture that frames the subject. Phone and camera
   photographs usually carry it; screenshots and edited images often do not.
3. Otherwise 20 degrees, the camera of the example rig Pixal3D ships. This is
   an assumption, and the result says so in `fov_source`.

A shape that looks stretched towards or away from the camera usually means
the field of view is wrong. Try again with a narrower view, such as 15 degrees,
or a wider one.

## Memory use

Pixal3D has three 1.3-billion-parameter flow transformers, two decoders, and
the image models DINOv3, NAF and BiRefNet. The worker never holds them
together:

- Each stage loads only its own network, runs it and releases it before the
  next one loads. The stages are background removal, image features, the
  structure, the shape at 512 and, above 512, the subdivision and the finer
  shape, and finally the surface.
- Each weight is read from disk on its own and converted straight to the
  precision it runs in on the GPU: bfloat16 for the transformers' blocks and
  float16 for the decoders, as Pixal3D runs them. The published checkpoints
  store 32-bit weights, so a transformer takes about 2.7 GB on the GPU
  instead of 5.5 GB. The process never holds a whole checkpoint in its own
  memory.
- DINOv3 runs once for each picture size and is released before the first
  transformer loads. The image features each voxel needs are sampled only at
  the voxels present, not over the whole grid.
- NAF's upsampling attends from each output pixel to the low-resolution
  features around it directly, without enlarging those features to the
  output size first.
- The sparse convolutions run on the voxels present, in plain PyTorch. They
  and the feed-forward layers work through the voxels or tokens in chunks,
  so their temporary tensors stay small. Pixal3D needs no compiled CUDA
  extensions beyond PyTorch's own.
- The worker runs one photograph job at a time, shared with depth
  estimation and head reconstruction. Each job runs in its own process,
  which returns all of its memory when it exits.

Peak GPU memory is therefore one network, its activations and the small
results passed between stages. On an NVIDIA GeForce RTX 4060 Ti with 16 GB, a
job at the default resolution of 1024 took 96 seconds. Its largest stage,
background removal, peaked at 3.3 GB of GPU memory, and each transformer stage
peaked between 2.9 and 3.3 GB. Other GPUs, photographs and settings differ, so
read each stage's `peak_gpu_memory_bytes` in your own results, and lower
`resolution` if a shared GPU runs short; a job that runs out of GPU memory
fails with `resource_limit` and leaves the worker ready for the next one.

The price is disk reading: each job reads its stages' weights from disk,
about 20 GB at resolutions above 512 and 14 GB at 512. The operating system's file
cache keeps them between jobs when the host has memory to spare.

## Supply the model

The Pixal3D model files are not included. Put them in one directory laid out like
this:

```text
pixal3d/
  pipeline.json
  ckpts/ss_flow_img_dit_1_3B_64_bf16.json, .safetensors
  ckpts/ss_dec_conv3d_16l8_fp16.json, .safetensors
  ckpts/slat_flow_img2shape_dit_1_3B_512_bf16.json, .safetensors
  ckpts/slat_flow_img2shape_dit_1_3B_1024_bf16.json, .safetensors
  ckpts/shape_dec_next_dc_f16c32_fp16.json, .safetensors
  dinov3/config.json
  dinov3/model.safetensors
  naf/naf_release.pth
  birefnet/model.safetensors
```

These commands fetch them with the Hugging Face command-line client and curl:

```sh
hf download TencentARC/Pixal3D pipeline.json \
  ckpts/ss_flow_img_dit_1_3B_64_bf16.json ckpts/ss_flow_img_dit_1_3B_64_bf16.safetensors \
  ckpts/ss_dec_conv3d_16l8_fp16.json ckpts/ss_dec_conv3d_16l8_fp16.safetensors \
  ckpts/slat_flow_img2shape_dit_1_3B_512_bf16.json ckpts/slat_flow_img2shape_dit_1_3B_512_bf16.safetensors \
  ckpts/slat_flow_img2shape_dit_1_3B_1024_bf16.json ckpts/slat_flow_img2shape_dit_1_3B_1024_bf16.safetensors \
  ckpts/shape_dec_next_dc_f16c32_fp16.json ckpts/shape_dec_next_dc_f16c32_fp16.safetensors \
  --local-dir pixal3d
hf download facebook/dinov3-vitl16-pretrain-lvd1689m config.json model.safetensors \
  --local-dir pixal3d/dinov3
hf download ZhengPeng7/BiRefNet model.safetensors \
  --revision e2bf8e4460fc8fa32bba5ea4d94b3233d367b0e4 --local-dir pixal3d/birefnet
mkdir -p pixal3d/naf
curl --fail --location --output pixal3d/naf/naf_release.pth \
  https://github.com/valeoai/NAF/releases/download/model/naf_release.pth
```

The download is about 19 GB. Check each model's licence before you use it:

- Pixal3D's checkpoints are published under the MIT License.
- DINOv3 is published under Meta's DINOv3 License, which Hugging Face asks
  you to accept before the download.
- NAF's weights are published under the Apache License 2.0.
- BiRefNet's weights from `ZhengPeng7/BiRefNet` are published under the MIT
  License. `briaai/RMBG-2.0` has the same network and may be used in its
  place; its licence does not allow commercial use.

`naf_release.pth` is a PyTorch pickle. The worker opens it with PyTorch's
weights-only loader, which builds tensors only and never runs code from the
file. The other files are safetensors and JSON. The worker runs no code from
the model directory: BiRefNet's network definition is pinned in the image at
a checked revision.

## Run the worker

Mount the Pixal3D directory into the photograph worker, read-only, at
`/models/pixal3d`, or name another path with `PRINTABLE_RECON_OBJECT_MODEL`.
Add this to the `recon-worker` service from
[Run the worker](flame-models.md#run-the-worker):

```yaml
services:
  recon-worker:
    volumes:
      - ./pixal3d:/models/pixal3d:ro
```

Object generation needs a CUDA GPU: give the worker container one, as the
GPU reservation in [Run the worker](flame-models.md#run-the-worker) does;
the published image carries PyTorch built for CUDA 13.0. When the worker's device is `auto` and no GPU is visible, a request fails
with `unavailable` rather than running for hours on the processor; setting
`PRINTABLE_RECON_DEVICE` to `cpu` forces the processor.

The worker writes the STL to its temporary directory before committing it to
the workspace, so give `/tmp` room for the largest mesh you expect. Each
request must finish within `PRINTABLE_RECON_OBJECT_TIMEOUT_SECONDS`, 1800 by
default and at most 3600. The `status` tool reports `object.configured` and
`object.ready`, and the worker's `/readyz` answer lists any missing model files
under `missing.object`.

## From the mesh to a print

The STL is the generator's surface, optionally simplified and never checked or repaired.
Like Pixal3D's own raw surface, it may have holes, small separate pieces,
edges shared by more than two triangles, triangles facing inconsistently
and self-intersections. Repair it in Blender before slicing:

1. Import it into the scene with the `scene` tool's `import` action.
2. Remesh it into a watertight solid with `blender_execute`, choosing a
   voxel size near your printer's detail, for example 0.2 mm for FDM. Use the
   object name the import reports:

   ```python
   import bpy
   shape = bpy.data.objects["figure"]
   remesh = shape.modifiers.new("Remesh", "REMESH")
   remesh.mode = "VOXEL"
   remesh.voxel_size = 0.2
   bpy.context.view_layer.objects.active = shape
   bpy.ops.object.modifier_apply(modifier=remesh.name)
   ```

   Voxel remeshing closes small holes, joins nearby pieces and removes
   self-intersections. A larger voxel size gives a lighter mesh and softens
   detail.
3. Export it with the `scene` tool's `export` action.
4. Check it with `validate_mesh`, and look at the overhang report before
   choosing supports.
5. Slice it with `slice`.

## Limits

- The shape is a learned estimate. Hidden sides are invented, thin parts may
  be thickened or lost, and text and fine patterns are not reproduced.
- Scale is unknown. `size_mm` sets the longest side; measure the subject to
  choose it.
- One photograph gives one shape. Try another seed or another photograph when
  the first result is wrong.
- Colour is not generated.
