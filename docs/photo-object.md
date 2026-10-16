# An object from a photograph

The `photo` tool's `object` action generates the 3D shape of the one object,
figure or character in a workspace photograph and writes it as an STL. The
shape comes from [Pixal3D](https://github.com/TencentARC/Pixal3D), which
aligns the generated geometry with the picture's pixels. Only the shape is
generated: Pixal3D's colour stages are never run, because a single-colour
print does not need them and skipping them saves time and memory.

The action runs in the optional photograph worker, the same service that
estimates depth and reconstructs FLAME heads, on its GPU. The `photo` tool
offers `object` only while the worker reports every Pixal3D model file
present.

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

The result reports the triangle count written and the count generated, the
mesh's size in millimetres, the
resolution reached, the steps used, the voxel counts of the structure and last
shape stages, the seed, the field of view and where it came from, whether the
background was removed, the device, the time taken, each stage's time and,
on a GPU, its peak memory, the photograph's SHA-256 and the written
artifact.

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
  so their temporary tensors stay small. The image needs no compiled CUDA
  extensions beyond PyTorch's own.
- The worker runs one photograph job at a time, shared with depth
  estimation and head reconstruction. Each job runs in its own process,
  which returns all of its memory when it exits.

Peak GPU memory is therefore one network, its activations and the small
results passed between stages. These figures have not been measured on a GPU:
this repository's tests run on the processor with miniature models. Read each
stage's `peak_gpu_memory_bytes` in your own first results, and lower
`resolution` if a shared GPU runs short; a job that runs out of GPU memory
fails with `resource_limit` and leaves the worker ready for the next one.

The price is disk reading: each job reads its stages' weights from disk,
about 20 GB at resolutions above 512 and 14 GB at 512. The operating system's file
cache keeps them between jobs when the host has memory to spare.

## Supply the model

The model files are not included. Put them in one directory laid out like
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

Mount the directory into the photograph worker, read-only, at
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

The STL is the generator's surface, simplified but not checked or repaired.
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
