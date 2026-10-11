# FLAME head models

[FLAME](https://flame.is.tue.mpg.de/) is a statistical 3D head model from the
Max Planck Institute for Intelligent Systems. It combines a linear identity
shape space with an articulated neck, jaw and eyes, pose-dependent corrective
shapes and expression shapes. This page explains how to prepare the published
model files for Printable.

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

It then refuses a model unless:

- every array's data length matches its shape and element type;
- faces reference template vertices and pose correctives cover each non-root joint;
- skinning weights sum to one for every vertex;
- the kinematic tree lists each parent before its child;
- the stored joint locations equal the joint regressor applied to the template,
  which confirms that the arrays and the sparse regressor decoded correctly.

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
