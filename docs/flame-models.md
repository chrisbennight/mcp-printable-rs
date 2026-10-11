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

The `head` workflow has four actions:

- `models` lists the mounted variants and their basis sizes.
- `create` adds a head mesh to the Blender scene.
- `update` re-evaluates an existing head in place, keeping its triangles.
- `prepare_print` writes a bust STL to the workspace without using Blender.

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
is life size. The response reports the variant and the posed joint locations in
millimetres. Updates are refused for objects the head tool did not create,
objects created from another variant, and heads whose vertex count has changed.
An update replaces every vertex position, so other edits to the mesh are lost.
Heads in Edit Mode are refused, because Blender would overwrite the update when
the head leaves Edit Mode.

## Prepare a bust for printing

A FLAME head is an open surface with separate eyeballs, so it cannot be printed
as it is. `prepare_print` takes the same `variant`, `identity`, `expression`
and `pose` as `create` and writes a single closed solid:

1. The mouth and neck openings are closed.
2. The head is turned upright, with the face toward -y, and scaled so that it
   stands `height_mm` tall.
3. The surface is smoothed and refined to `edge_length_mm`, and the eyeballs
   are united with the head.
4. The neck is cut flat just above its opening, so the bust rests on z = 0.

The geometry work runs in the isolated geometry worker under
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB`.

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
| `max_triangles` | 1,500,000 | 10,000 to 3,000,000 |
| `overwrite` | false | |

The response contains the written artifact, a `bust` report and the mesh
validation report. `bust` gives the triangle count, size, volume, the edge
length used, and `chordal_deviation_p999_mm`: the distance between the facets
and the smooth surface they approximate, at the 99.9th percentile. The edge
length grows above the request only when needed to stay within
`max_triangles`. `discarded_fragments` counts separate slivers of negligible
volume that the union left where an eyeball meets an eyelid; they are removed.

With the defaults, each of the FLAME 2020 generic, FLAME 2023 and FLAME 2023
Open models produced one watertight, manifold solid of 340,000 to 390,000
triangles in under 10 seconds, with a 99.9th-percentile deviation of 0.022 mm.
That is well below the detail a 0.2 mm nozzle reproduces. A 300 mm bust at
the triangle limit stayed within the default 1 GiB worker memory limit.

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
