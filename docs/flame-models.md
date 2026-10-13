# FLAME head models

[FLAME](https://flame.is.tue.mpg.de/) is a statistical 3D head model from the
Max Planck Institute for Intelligent Systems. It combines a linear identity
shape space with an articulated neck, jaw and eyes, pose-dependent corrective
shapes and expression shapes. This page explains how to prepare the published
model files and generate heads with them in Printable.

## Obtain the model

Model files are not distributed with Printable. Register on the
[FLAME site](https://flame.is.tue.mpg.de/), accept its terms and download the
release you want. The converter accepts these published files:

| Release | File | Expression basis |
| --- | --- | --- |
| FLAME 2023 Open | `flame2023_Open.pkl` | SUPR head, recorded in the file |
| FLAME 2023 | `flame2023.pkl`, `flame2023_no_jaw.pkl` | FLAME |
| FLAME 2020 | `generic_model.pkl`, `female_model.pkl`, `male_model.pkl` | FLAME |

## Convert a model

The published files are Python pickles. Printable reads a converted
[safetensors](https://github.com/huggingface/safetensors) file instead. The
converter uses only the Python standard library:

```sh
python3 scripts/flame_model_convert.py flame2023_Open.pkl \
  /models/flame/flame2023_open.safetensors --variant flame2023_open
```

`--variant` is a lowercase label recorded in the output. The command prints the
output's SHA-256 and metadata as JSON. It writes through a temporary file in the
destination directory, so a failed conversion leaves any existing file in place.
Sources larger than 256 MiB are refused; the published files are about 53 MB.

Older releases do not record how many basis components describe expression.
When the basis has the published 400 components, the converter uses FLAME's
convention of 300 identity and 100 expression components. For any other size,
pass `--expression-components`.

Surface detail from a displacement map needs FLAME's texture layout. It is
published separately as `FLAME_texture.npz` in the "FLAME texture space"
download on the FLAME site. Add it with `--texture`:

```sh
python3 scripts/flame_model_convert.py generic_model.pkl \
  /models/flame/flame2020_generic.safetensors --variant flame2020_generic \
  --texture FLAME_texture.npz
```

The converter reads only the archive's `vt` and `ft` arrays, the texture
coordinates and the coordinates at each triangle's corners. It decodes them
as plain little-endian numbers and never unpickles anything. It refuses
arrays larger than 1 MiB, a layout without one textured triangle per model
triangle, coordinates outside the unit square and corners that reference a
missing coordinate. All FLAME releases share the same triangles, so the one
layout serves every release.

Some head features work on named parts of the head: the scalp, face,
forehead, nose, lips, neck, ears, eyeballs and the regions around the eyes.
FLAME publishes these as `FLAME_masks.pkl`, inside `FLAME_masks.zip` on the
FLAME site's download page. Add it with `--masks`:

```sh
python3 scripts/flame_model_convert.py generic_model.pkl \
  /models/flame/flame2020_generic.safetensors --variant flame2020_generic \
  --texture FLAME_texture.npz --masks FLAME_masks.pkl
```

The masks file is read with the same restricted loader as the model and may
be at most 4 MiB. Each region must be a list of integer vertex indices
within the model, with a short lowercase name. Repeated indices are dropped,
because the published lips region lists some vertices twice. The command's
output lists the regions it stored, and the `models` action reports them for
each mounted model. All FLAME releases number their vertices the same way,
so one masks file serves every release.

## What the converter checks

Loading a pickle can run arbitrary code. The converter resolves only the
symbols that published FLAME files use: NumPy array and type constructors, the
SciPy sparse matrix, the chumpy array and the built-in `set`. Each maps to an
inert stand-in that only records data, `set` accepts only the empty sets the
published files contain, and every other symbol is refused before it is
resolved. No code from the file runs.

The command also runs within CPU-time and memory limits far above what a
published model needs, so a malformed file fails instead of exhausting the
machine.

It then refuses a model unless:

- every array's data length matches its shape and element type;
- faces reference template vertices and pose correctives cover each non-root joint;
- skinning weights sum to one for every vertex;
- the kinematic tree lists each parent before its child;
- the stored joint locations equal the joint regressor applied to the template,
  which confirms that the arrays and the sparse regressor decoded correctly.

## Generate heads

Mount the directory of converted files into the server and point
`PRINTABLE_FLAME_MODEL_DIR` at it. In the Compose installation, keep
`compose.yaml` unchanged and create your own override file, for example
`compose.flame.yaml`:

```yaml
services:
  server:
    volumes:
      - ./flame-models:/models/flame:ro
    environment:
      PRINTABLE_FLAME_MODEL_DIR: /models/flame
```

Pass it after `compose.yaml`. Compose merges volumes from an override file by
container path, so the server keeps its shared workspace mount:

```sh
docker compose --env-file .dev/compose.env -f compose.yaml -f compose.flame.yaml up -d --wait --wait-timeout 600
```

The converted files must be readable by the container user.

Each valid file loads once at startup, keyed by its `variant`. When no model
loads, the `head` tool is not advertised. `status` lists the loaded models and
any file that was skipped, with the reason.

The `head` workflow has five actions, and a sixth when photo reconstruction is
available:

- `models` lists the mounted variants, their basis sizes, whether each has a
  texture layout and the regions it was converted with.
- `create` adds a head mesh to the Blender scene.
- `update` re-evaluates an existing head in place, keeping its triangles.
- `export` writes the head to the workspace as an OBJ file without using
  Blender.
- `prepare_print` writes a bust STL to the workspace without using Blender.
- `reconstruct` estimates a head from photographs; see
  [Reconstruct a head from photographs](#reconstruct-a-head-from-photographs).

```json
{"action": "create", "params": {
  "variant": "flame2023_open",
  "identity": [1.2, -0.4],
  "expression": [0.5],
  "pose": {"jaw": [0.15, 0.0, 0.0]},
  "name": "Bust"
}}
```

Coefficients are standard deviations of the heads the model was learned from,
each within ±5; omitted trailing coefficients are zero. Pose rotations are
axis-angle vectors in radians with a length of at most pi, in FLAME's frame:
y up and the face looking along +z, so the subject's left is +x. A positive jaw
rotation about +x opens the mouth. `variant` may be omitted when only one model
is mounted.

The mesh is in millimetres, like other Printable geometry, so an unscaled head
is life size. In Blender the new head stands upright: its object is rotated 90
degrees about x, so the crown points up +z and the face looks along -y, toward
Blender's front view, as the printed bust does. The mesh itself keeps FLAME's
frame, and `update` leaves the object's rotation as it finds it. The response
reports the variant and the posed joint locations in millimetres, in FLAME's
frame. Updates are refused for objects the head tool did not create,
objects created from another variant, and heads whose vertex count has changed.
An update replaces every vertex position, so other edits to the mesh are lost.
Heads in Edit Mode are refused, because Blender would overwrite the update when
the head leaves Edit Mode.

### Colour the head in Blender

When the model was converted with `--texture`, `create` gives the head a UV
layer named `FLAME` holding FLAME's texture coordinates. Pass `texture`, the
workspace path of a PNG colour map in FLAME's texture layout, to show it in
colour:

```json
{"action": "create", "params": {
  "variant": "flame2020_generic",
  "identity": [0.4, -1.1],
  "texture": "photos/colour.png",
  "name": "Subject"
}}
```

The colour map may be up to 12 MiB and 4096 pixels on each side, which is what
the Blender bridge can stage before loading it. A map whose pixels Blender
cannot decode is refused before the scene changes. The image is packed into the
scene, so the head keeps its colour without the workspace file. Its material feeds the image into the base colour of a
Principled BSDF, which the viewport and the render tools show. `update`
accepts `texture` too, to replace the colour map of a head created with
texture coordinates. It changes the material the head actually shows: when
another mesh uses that material, the head gets its own copy, and when a slot
linked to the object hides the colour material, a visible one is added.
Objects that share the head's mesh, such as linked duplicates, are the same
head: `update` moves their vertices and changes their colour together. The
material reads the `FLAME` layer by name, so another active UV layer does not
move the colour. The colour stays in Blender: `export` and `prepare_print`
write geometry only.

### Head appearance in Blender

New heads are shaded smooth. By default a subdivision modifier smooths them
further, and the neck opening, the opening with the longest rim, is closed
with a single face. The face uses the
existing vertices, so `update` still applies. Each face also gets a
`printable_eye` attribute: 1 on the eyeballs, which are the parts of the mesh
separate from the head, and 0 elsewhere. The head's material uses it to give
the skin and the eyes different finishes. Without a colour map the material
is a plain neutral colour; a colour map added later by `update` replaces it in
the same slot and keeps the finish the head was created with.

`create` accepts an optional `appearance` object:

| Field | Default | Range |
| --- | --- | --- |
| `subdivision` | 1 | 0 to 2 levels |
| `close_neck` | true | |
| `skin_roughness` | 0.5 | 0 to 1 |
| `skin_subsurface` | 0.1 | 0 to 1, the weight of light scattering beneath the skin |
| `subsurface_scale_mm` | 1 | 0 to 100 |
| `eye_roughness` | 0.05 | 0 to 1 |
| `bump` | none | a displacement map shown as bump detail; needs `texture` |

`bump` takes the `path` of a greyscale PNG displacement map in FLAME's texture
layout, its `scale_mm`, and an optional `strength` from 0 to 10 (default 1).
Use the map and `scale_mm` from `reconstruct`, so the Blender head shows the
same detail as a bust printed with that map:

```json
{"action": "create", "params": {
  "variant": "flame2020_generic",
  "identity": [0.4, -1.1],
  "texture": "photos/colour.png",
  "appearance": {"bump": {"path": "photos/detail.png", "scale_mm": 6.64}},
  "name": "Subject"
}}
```

The `portrait` profile of the product render tools lights heads with a large,
dim key light and a broad fill. In software renders of a head coloured from a
photograph, it kept pale skin from washing out where `studio_neutral` did not.

### Export a head

`export` takes the same `variant`, `identity`, `expression` and `pose` as
`create` and writes the head to a workspace path ending in `.obj`:

```json
{"action": "export", "params": {
  "variant": "flame2020_generic",
  "identity": [0.4, -1.1],
  "path": "heads/subject.obj"
}}
```

The file is a Wavefront OBJ of FLAME's own mesh, including the eyeballs, in
millimetres and in the same frame as `create`. When the model was converted
with `--texture`, each triangle corner also carries FLAME's texture
coordinates, so images laid out for FLAME, such as the displacement map from
`reconstruct`, line up with it in other software. `overwrite` replaces an
existing file. The response reports the artifact, the vertex and triangle
counts, whether texture coordinates were written, and the posed joint
locations.

For the FLAME 2020 generic model converted with `--texture`, the file holds
5,023 vertices, 5,118 texture coordinates and 9,976 triangles, about 0.6 MB.

## Prepare a bust for printing

A FLAME head is an open surface with separate eyeballs, so it cannot be printed
as it is. Its surface also passes through itself in places, at the lips and the
ears. `prepare_print` takes the same `variant`, `identity`, `expression` and
`pose` as `create` and writes a single closed solid:

1. The mouth and neck openings are closed. Openings are found from the mesh's
   boundary edges, and the lowest one is the neck.
2. The head is turned upright, with the face toward -y, and scaled to
   `height_mm`.
3. The surface is smoothed and refined to `edge_length_mm`. With a
   displacement map, each point of the refined surface is then raised along
   its normal by the map's value at that point, scaled with the head.
4. The union of the head and eyeballs is rebuilt as one valid solid. A point
   is inside when an upward ray from it crosses more outward-facing surface
   than inward-facing surface, counted with exact arithmetic. A new surface
   is extracted at that boundary by marching tetrahedra, which produces a
   closed surface that never touches or crosses itself. Each new vertex is
   placed exactly where its grid edge, followed from the outside end, first
   enters the solid. Surfaces buried inside the union, such as the hidden
   parts of the eyeballs and the folds of the lips, therefore do not pull the
   surface inward where they meet it.
5. The neck is cut flat just above its opening, so the bust rests on z = 0,
   and the height is corrected to exactly `height_mm`.
6. Optionally, a cylindrical plinth is added under the base.
7. Optionally, the bust is hollowed for resin printing. Everything deeper than
   the wall thickness is removed, and one or two drain holes open from the
   base into the lowest part of the cavity. A pocket that no drain hole
   reaches is left solid, so no liquid resin is sealed inside.

`pose.global` must be zero, because the bust stands on its neck. Turn or tilt
the head with `pose.neck`.

The geometry work runs in the isolated geometry worker under
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB`. The server validates the result,
including a check for self-intersections. A bust that fails validation is
reported as an error and not written.

```json
{"action": "prepare_print", "params": {
  "variant": "flame2023_open",
  "identity": [1.2, -0.4],
  "expression": [0.5],
  "path": "busts/head.stl"
}}
```

| Parameter | Default | Range |
| --- | --- | --- |
| `height_mm` | 100 | 20 to 500 |
| `edge_length_mm` | 0.4 | 0.1 to 5 |
| `max_triangles` | 750,000 | 10,000 to 3,000,000 |
| `plinth_height_mm` | none | 2 to 100, counted within `height_mm` |
| `plinth_diameter_mm` | 70% of the bust's widest extent | 10 to 500 |
| `hollow_wall_mm` | none (solid) | 1 to 20 |
| `drain_hole_diameter_mm` | 3.5 | 1 to 10 |
| `displacement` | none | see [Surface detail](#surface-detail) |
| `overwrite` | false | |

A solid bust suits FDM printing, where the slicer's infill already saves
material. Hollowing is for resin printers.

The default triangle limit is about the most the geometry worker can prepare
within its default 1 GiB memory limit. A hollow bust keeps a quarter of the
limit for its cavity. Raise
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB` with a higher limit; a request that
exceeds the worker's memory fails with `geometry_resource_limit` and writes
nothing.

The response contains the written artifact, a `bust` report and the mesh
validation report. `bust` gives the triangle count, size, volume and the edge
length used. The edge length grows above the request only when needed to
stay within `max_triangles`. When requested, `plinth` reports the plinth's
height and diameter, and `hollow` reports the wall thickness, the volume
removed and the drain hole positions on the base.

`estimated_deviation_p999_mm` estimates how far the flat facets sit from the
smooth surface they approximate, at the 99.9th percentile. It is an estimate,
not a measurement: each edge of length L whose two faces meet at an angle θ
contributes L·θ/8, the gap between a chord and a circular arc. Edges where
faces meet at 30 degrees or more are creases, such as the base rim, and are
left out. `discarded_fragments` counts separate specks of negligible volume
that were removed.

With the defaults, the FLAME 2020 generic, FLAME 2023 and FLAME 2023 Open
models each produced one watertight, manifold solid with no self-intersections
and 430,000 to 490,000 triangles. The estimated 99.9th-percentile deviation
was 0.041 to 0.043 mm, well below the detail a 0.2 mm nozzle reproduces. With
a 12 mm plinth and 2.5 mm walls, each was still one valid solid with its
cavity open through two drain holes. Each request took 8 to 11 seconds on a
busy four-core machine, including validation. A 300 mm hollow bust on a
plinth at the default triangle limit stayed within the default 1 GiB worker
memory limit.

### Surface detail

`displacement` adds fine detail such as wrinkles from a map over FLAME's
texture layout, such as the one `reconstruct` writes. The model must be
converted with `--texture`.

```json
{"action": "prepare_print", "params": {
  "variant": "flame2020_generic",
  "identity": [0.4, -1.1],
  "path": "busts/detail.stl",
  "displacement": {"path": "photos/detail.png", "offset_mm": -3.28, "scale_mm": 6.64}
}}
```

The map is an 8- or 16-bit greyscale PNG of 2 to 1024 pixels on each side.
A pixel at fraction f of the full range (p / 65535 for 16-bit images, p / 255
for 8-bit images) raises the surface by `offset_mm + scale_mm × f`
millimetres at life size. Negative values press the surface in. Column 0 is
u = 0 and row 0 is v = 1, and values between pixels are interpolated. The
raise is scaled by the same factor as the head, so a 100 mm bust of a head
about 270 mm tall shows the detail at about 37% of its life-size depth.
Values beyond 5% of the head's height are refused as not being detail.

Texture coordinates are carried through smoothing and refinement, so every
point of the refined surface takes its value from its own place in the map.
Where texture coordinates differ along an edge, the points on either side
move together by their average. The surfaces that close the mouth and neck
openings have no texture coordinates, so the detail fades to nothing across
them. The displaced surface is rebuilt in step 4 like any other, so the bust
is still one valid solid even where the detail folds the surface.

### Filter detail for printing

A map estimated from a photograph mixes fine detail, such as wrinkles and
creases, with broader shape. At bust scale the broad part prints as lumps,
while the fine part can be shallower than a layer. These optional
`displacement` fields shape the detail before it is applied, in this order:

| Field | Range | Effect |
| --- | --- | --- |
| `high_pass_mm` | 1 to 100 | Removes features wider than about this many millimetres on the life-size head, keeping finer detail. |
| `clip_percentile` | 50 to 100 | Limits the detail to this percentile of its magnitude, taken over the detail on the head's surface that remains after `high_pass_mm`, so a few extreme values do not dominate. |
| `gain` | 0 to 10, default 1 | Multiplies the detail. |
| `relief_mm` | 0.01 to 10 | Scales the detail so that its 95th percentile on the head's surface is this many millimetres on the finished bust, whatever its height. The detail is sampled at every triangle's corners and centre and weighted by the triangle's area. Not with `gain`. |

Texels whose magnitude is below 0.5% of the largest on the head's surface are not
detail.
Filtering changes only the detail and sets the other texels to exactly zero,
so their rounding noise is never scaled up as detail. Blurs are taken over the
head's surface, the texels inside a triangle of the texture layout, so the
gaps between the layout's pieces do not pull nearby detail toward zero. The
filter width is converted to texels using the model's own surface: the
median, over the texels that carry detail, of how many millimetres one texel
of their triangle covers.
The high-pass blur is three successive box blurs, which approximate a
Gaussian blur and cost the same whatever the width; a width narrower than a
texel removes nothing. `relief_mm` leaves a map unscaled when its detail is
shallower than a thousandth of a millimetre at life size.

```json
{"action": "prepare_print", "params": {
  "variant": "flame2020_generic",
  "identity": [0.4, -1.1],
  "path": "busts/detail.stl",
  "displacement": {"path": "photos/detail.png", "offset_mm": -3.07, "scale_mm": 6.64,
                   "high_pass_mm": 8, "clip_percentile": 99, "relief_mm": 0.25}
}}
```

The `bust` report adds `displacement`:

- `lowest_mm` and `highest_mm`, the deepest depression and the highest raise
  applied, in millimetres at the finished size;
- `percentiles_mm`, the 1st, 5th, 50th, 95th and 99th percentiles of the
  applied distance over the vertices where the map carries detail;
- `relief_scale`, the factor `relief_mm` multiplied the map by, or 1;
- `filter`, the millimetres per texel used and the filter settings applied.

On a 100 mm bust of the FLAME 2020 generic head with a map reconstructed from
one photograph, the unfiltered detail ran from -0.61 to +0.70 mm between
its 5th and 95th percentiles, and rendered as broad lumps on the cheeks and
chin. With `high_pass_mm` 8 and `clip_percentile` 99, that span was -0.12 to
+0.19 mm and the lumps were gone while forehead and cheek creases remained.
With `relief_mm` 0.15, 0.25 and 0.40 added, the applied 95th percentile was
0.154, 0.256 and 0.410 mm. Each request
took about 8 seconds, including a 1024-pixel map filtered with `high_pass_mm`
100.

A 100 mm bust from a photograph reconstructed with `reconstruct` was one
watertight, manifold solid with no self-intersections and 432,000 triangles.
The applied detail ranged from -1.2 to +1.2 mm, and the request took about
8 seconds on a busy four-core machine. Forehead lines and creases around the
eyes and mouth were clearly visible in renders of the result. On an FDM
print, expect detail shallower than the layer height to be lost.

## Reconstruct a head from photographs

The `reconstruct` action estimates a head from 1 to 8 photographs of one
person. It runs in an optional reconstruction worker:

- **Identity** comes from MICA, which recovers true head size. The ArcFace
  identity embeddings of all the photographs are averaged before MICA maps them
  to 300 identity coefficients.
- **Expression and jaw** come from SMIRK, applied to the first photograph.
- **Surface detail** comes from DECA, applied to the first photograph. Its
  detail decoder writes a 16-bit displacement map in FLAME's UV layout,
  conditioned on DECA's own estimate of the expression and jaw.

All three networks were trained with FLAME 2020, so their coefficients belong
to the FLAME 2020 generic model. Convert and mount `generic_model.pkl` from
the FLAME 2020 release alongside any other models, with the converter's
default split of 300 identity and 100 expression components. The server
recognises it by the digest of the published file and that split.

### Network checkpoints

The checkpoints are not included. Download them from the projects and place
these files in one directory:

| File | Source |
| --- | --- |
| `mica.tar` | [MICA](https://github.com/Zielon/MICA), "Pre-trained Models" |
| `SMIRK_em1.pt` | [SMIRK](https://github.com/georgeretsi/smirk), the pretrained model link |
| `deca_model.tar` | [DECA](https://github.com/yfeng95/DECA), the released model |
| `uv_face_eye_mask.png` | DECA's `data` directory |

The worker loads checkpoints with PyTorch's weights-only loader, which builds
tensors and plain data but never runs code from the file. Its image includes
two MediaPipe models: the face landmarker, which places the face crops and the
five ArcFace keypoints, and the multiclass selfie segmenter, which labels hair,
skin and clothing for the colour map.

### Run the worker

Each release publishes the worker as
`ghcr.io/chrisbennight/mcp-printable-recon:sha-<commit>`, and the image-set
record names it in its `org.printable.recon.image` and
`org.printable.recon.digest` labels. The published image is the build CI
tests: it carries CPU-only PyTorch and runs without a GPU. To use it, replace
`build` in the service below with that pinned image reference and drop the GPU
reservation.

To build the worker from a source checkout instead, add the service in your
own Compose override, next to the FLAME model mount:

```yaml
services:
  recon-worker:
    build:
      context: .
      target: recon-runtime
    image: printable-recon:local
    platform: linux/amd64
    user: "10001:10001"
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    pids_limit: 512
    mem_limit: 8g
    restart: unless-stopped
    networks: [control]
    tmpfs:
      - /tmp:rw,size=2g,uid=10001,gid=10001,mode=1770
    volumes:
      - workspace:/workspace
      - ./recon-checkpoints:/models/recon:ro
    environment:
      PRINTABLE_WORKSPACE_ROOT: /workspace
    depends_on:
      workspace-init:
        condition: service_completed_successfully
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              device_ids: ["${PRINTABLE_GPU_DEVICE:-0}"]
              capabilities: [gpu]
  server:
    volumes:
      - ./flame-models:/models/flame:ro
    environment:
      PRINTABLE_FLAME_MODEL_DIR: /models/flame
      PRINTABLE_RECON_ENDPOINT: http://recon-worker:8004
```

```sh
docker compose --env-file .dev/compose.env -f compose.yaml -f compose.flame.yaml build recon-worker
docker compose --env-file .dev/compose.env -f compose.yaml -f compose.flame.yaml up -d --wait --wait-timeout 600
```

The image installs PyTorch built for CUDA 12.8. Pass
`--build-arg TORCH_INDEX_URL=https://download.pytorch.org/whl/cpu` to build a
CPU-only image instead. The worker uses the GPU when one is visible and the CPU
otherwise; `PRINTABLE_RECON_DEVICE` forces one or the other. It runs one
reconstruction at a time and stops any that exceeds
`PRINTABLE_RECON_TIMEOUT_SECONDS` (default 300). `status` reports whether the
worker is configured and ready.

The `head` tool offers `reconstruct` only when a matching head model is mounted
and the worker reports every checkpoint present. The server checks every
30 seconds. When availability changes, initialized legacy sessions and current
clients subscribed to tool-list changes receive
`notifications/tools/list_changed` so that they list the tools again. See
[MCP compatibility](architecture.md#mcp-compatibility) for subscription and
reconnection requirements.

### Use the result

```json
{"action": "reconstruct", "params": {
  "images": ["photos/front.jpg", "photos/left.jpg", "photos/right.jpg"],
  "displacement_path": "photos/detail.png",
  "texture_path": "photos/colour.png"
}}
```

The response gives `variant`, `identity`, `expression` and `pose.jaw` in the
form `create`, `export` and `prepare_print` accept, so they can be passed
straight on.
`identity_agreement` gives each photograph's cosine similarity to the fused
identity; a photograph well below the others probably shows someone else or
an unusable view. `clamped_coefficients` counts values limited to ±5.

Photographs should show the face clearly, roughly front-on, in focus and
without heavy occlusion. Several views improve identity; the first photograph
alone sets the expression and detail.

`displacement` describes the map: a pixel value p means
`offset_mm + scale_mm × p / 65535` millimetres along the surface normal at life
size. Column 0 is u = 0 and row 0 is v = 1 in FLAME's texture layout. The map
holds DECA's learned detail within its face mask, conditioned on DECA's own
estimate of the expression and jaw. SMIRK's larger coefficients made DECA's
decoder exaggerate the detail, putting creases across the bridge of the nose.
DECA's renderer also adds a fixed offset, a lattice of ripples up to 1.6 mm
that follows FLAME's triangle layout and corrects shading; it is not surface
shape and is left out.

To raise this detail on a printed bust, pass the map's `artifact.path`,
`offset_mm` and `scale_mm` as `prepare_print`'s `displacement`, with the same
coefficients and variant. See [Surface detail](#surface-detail).

### Colour map

With `texture_path`, `reconstruct` also writes a 1024 by 1024 RGB PNG colour
map in FLAME's texture layout. It needs the FLAME 2020 generic model converted
with `--texture`, and `overwrite` applies to it as to the displacement map. The
colour comes from the photographs themselves:

1. SMIRK fits a head and an orthographic camera to each photograph. The server
   poses FLAME to match each fit.
2. The segmenter labels a square around each face, wide enough to take in the
   hair and neck, as background, hair, face skin, body skin, clothes or other.
3. Each point of the head takes the colour under it in each photograph where it
   faces the camera, is not hidden by nearer surface, is not within about 2% of
   the crop of the head's outline, and shows the right thing. The fitted
   outline never matches the real one exactly, and the background lies just
   beyond it. Skin takes colour only from pixels labelled skin, so hair over
   the forehead or ears and clothing over the neck are left out. The eyeballs
   take colour from anything but hair, clothing and background.
4. Photographs are blended by how squarely they see each point.
5. Surface that no photograph sees, such as the back of the head, is filled
   smoothly across the head's surface from the colour around it. Skin is
   therefore filled from skin alone. The fill follows the head rather than the
   texture layout, so it has no seams where the layout is cut apart.

`scalp` chooses how the scalp is coloured:

| Value | Scalp colour |
| --- | --- |
| `hair` (default) | the hair colour wherever the photographs do not show bare scalp |
| `skin` | as skin: from pixels of skin, and filled from skin |
| `photo` | whatever the photographs show there, hair included |

The hair colour is the median colour of the pixels labelled hair, averaged over
the photographs by how much hair each shows. `hair_colour`, an sRGB triple from
0 to 255, replaces it. `hair` and `skin` need to know where the scalp is, so
the FLAME 2020 generic model must be converted with `--masks`; without its
regions the whole head is coloured as skin, and the response says so.

The response adds `texture` with the artifact, its size and `seen`, the share
of the head's surface whose colour some photograph gave directly. `skin_only`
says whether every photograph was segmented. `scalp` reports the colouring
requested and applied, the hair colour used and the hair colour found, and a
`note` when the colouring applied differs from the one requested. The server reads the
photographs before the worker does and compares their SHA-256 digests with the
ones the worker fitted, so a photograph replaced during the reconstruction
stops the colour map instead of misplacing it. The server makes one colour map
at a time, reading one photograph at a time, so a request that asks for one
while another is being made is refused as busy. The map is for viewing and
rendering. Printed files carry no colour.

`lighting` chooses what becomes of the photographs' light and shade:

| Value | Effect |
| --- | --- |
| `remove` (default) | Each photograph's smooth shading is estimated over the skin and divided out, so a renderer's own lights are not added to the light already in the photograph. |
| `keep` | The map keeps the light and shade as photographed. |

The shading is fitted in linear light, separately for each colour channel, as
an ambient term plus one direction applied to the fitted head's surface
normals. A photograph with fewer than 200 skin samples keeps its lighting.
Dividing by the shading would amplify noise in deep shadow, so the shading is
never taken as less than a quarter of its average. Each channel's median over
the skin keeps its photographed value, which keeps the subject's skin tone.
Luminance above the skin's 97th percentile is brought down to it, which limits
specular highlights. `texture.lighting.shading` reports the shading removed
from each photograph, or null where it was kept.

From one front-on photograph of a person with long dark hair, the
reconstruction took about 30 seconds on a busy four-core CPU and the map under
a second more. 31% of the surface took its colour from the photograph. Before
skin was told apart from hair and clothing, 44% did, and the extra share was
hair over the ears and neck and clothing at the base of the neck, which
rendered as grey ears and a dark neck. With the segmentation, renders showed
skin-coloured ears and neck, the scalp in the hair colour found, and the
photograph's centre parting as bare scalp. Renders of the textured head
matched the photograph's eyes, brows, nose and mouth, and the filled back of
the head showed no seams.

On the same photograph, removing the lighting reduced the difference in
brightness between the forehead, the cheeks and the jaw from 45 to 18 levels
of 255. The forehead, the brightest of them, came down from 220 to 198. In
renders under the `portrait` profile, the far cheek no longer kept the
photograph's shadow, and the head took its shading from the render's lights.

On MICA's own demonstration photographs, identity from MediaPipe's keypoints
was within 0.2 to 0.4 mm on average, and 0.9 mm at most, of identity from the
face detector MICA was trained with. One photograph took about 40 seconds on a
busy four-core CPU. Repeated runs on the CPU gave identical results.

## Output format

Arrays are little-endian. Floating-point data is stored as float32.

| Tensor | Type | Shape |
| --- | --- | --- |
| `v_template` | F32 | vertices × 3 |
| `faces` | U32 | triangles × 3 |
| `shapedirs` | F32 | vertices × 3 × (identity + expression) |
| `posedirs` | F32 | vertices × 3 × 9 per non-root joint |
| `lbs_weights` | F32 | vertices × joints |
| `joint_regressor` | F32 | joints × vertices |
| `kintree_parents` | I32 | joints, with -1 for the root |
| `uv_coords` | F32 | texture coordinates × 2, with `--texture` only |
| `uv_faces` | U32 | triangles × 3, with `--texture` only |
| `region.<name>` | U32 | sorted vertex indices, one tensor per region, with `--masks` only |

The metadata records `format`, `variant`, `source_sha256`,
`identity_components`, `expression_components`, `expression_source`,
`skinning`, `pose_feature` and `units`. Identity components precede expression
components in `shapedirs`. Coordinates are in metres.

## Differences between releases

Comparing converted files from the published releases shows:

- FLAME 2023 and FLAME 2023 Open are identical except for the expression basis.
- FLAME 2023 and its no-jaw file differ only in the expression basis.
- All releases share the same triangles, skinning weights, joint regressor and
  kinematic tree.
- FLAME 2020 and FLAME 2023 have different templates and identity bases.

Identity coefficients therefore carry over between FLAME 2023 and FLAME 2023
Open, but expression coefficients do not. Evaluate coefficients with the
release they were fitted against.
