# Documentation example: bracket revision and slicing

On 11 November 2026 the public MCP tools built the
[CadQuery bracket source](../examples/workflows/bracket.py) at widths 40 and
60 mm in a separate documentation project. The native worker reported CadQuery
2.8.0 and valid single-solid geometry in millimetres. Both builds passed their
requested `printable_part` delivery checks, including the target dimensions.

| Measurement | Width 40 | Width 60 |
| --- | --- | --- |
| Bounds size, mm | [40, 30, 24] | [60, 30, 24] |
| Vertical hole centers, XY mm | [25, 8], [25, 22] | [25, 8], [25, 22] |
| Hole radii, mm | 2, 2 | 2, 2 |
| Solid count | 1 | 1 |

OpenSCAD generated the [40 mm](../docs/images/bracket-40.png) and
[60 mm](../docs/images/bracket-60.png) views from the exported STLs through the
project `cover` model-view action. They are independently framed views of the
actual output geometry. No picture edits or generated UI were used.

OrcaSlicer 2.4.2 prepared the 40 mm STL with these selections:

- Printer: `Bambu Lab X1 Carbon 0.4 nozzle`.
- Process: `0.20mm Standard @BBL X1C`.
- Filament: `Bambu PLA Basic @BBL X1C`, slot color `#3B82B8`.
- Physical surface: `Textured PEI Plate`; automatic arrangement enabled.

Preparation completed with G-code and a `.gcode.3mf` package. `slice.review`
rendered layer 20 of 120 at Z = 4 mm into the
[published preview](../docs/images/bracket-toolpath.png), using a 900-pixel
image. Its source digest matched the slice's retained G-code digest. The image
was downloaded through the governed file mechanism with size/digest verification.

Reproduce with the [functional-part guide](../docs/workflows/functional-parts.md)
and [slicing guide](../docs/workflows/model-to-printer.md). Their project name
differs from the capture project's name; source and geometric parameters are
the same. Use a fresh project/output location rather than overwriting a run.
The [retained result projection](documentation-workflows.json) records reports,
artifact identities, resolved slice setup, and toolpath-review identity for
this run. It excludes operator addresses, credentials, and unrelated projects.

This run used remote software backends without changing the live Blender scene.
It did not send the package to Bambuddy or start a physical print. It establishes
the recorded software results, not strength, fastener fit, adhesion, print
completion, an independent human trial, or performance on other machines.

## Portrait reconstruction

The input was the 500-pixel thumbnail of Ferdinand Schmutzer's 1921 Einstein
photograph, [identified as public domain by Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Einstein_1921_by_F_Schmutzer.jpg).
Its SHA-256 is `ac29d64cf735eaf6ff44059b1f17089f0696172c3cbebf8f4e12d9944f55afd1`.
The checked-in JPEG was uploaded unchanged. `head.photo_to_bust` used the
mounted `flame2020_generic` model with an FDM printer block and
`detail.relief_mm: 0.3`, writing displacement and fit images and a bust STL.
The default height produced a 100 mm bust.

The returned mesh was a single watertight, consistently oriented manifold
component without reported self-intersections. Its assessment failed the
support-free-orientation criterion, with support recommended. The bust also
reported thin surface details. These warnings were retained rather than
interpreting solid topology as complete print qualification.

The [fit image](../docs/images/einstein-fit.png) is the reconstruction's actual
diagnostic. The [bust image](../docs/images/einstein-bust.png) is an OpenSCAD
model view of its actual STL. Neither was locally edited. Hair and moustache
were not reconstructed, and no physical likeness test or print was performed.
The result demonstrates the generated geometry and its reported checks.

An earlier attempt used Julia Margaret Cameron's 1868
[Darwin portrait](https://commons.wikimedia.org/wiki/File:Charles_Darwin_photograph_by_Julia_Margaret_Cameron,_1868.jpg),
also identified as public domain. The dark, side-facing portrait returned
`no_face`; no bust was produced. Its image hash and error are retained in the
JSON projection. It was not treated as a successful example or repeatedly retried.

## Image-generated object

The actual 40 mm bracket view was uploaded as the image input to Pixal3D.
The request selected resolution 512, longest side 40 mm, target 100,000
triangles, and background admission. The retained job completed and returned
an STL with reported bounds of 40 × 32.657 × 36.680 mm. These are the generator's
axes, not a recovered CAD coordinate system. The source CAD bounds are
40 × 30 × 24 mm; choosing the longest side did not preserve the source's
dimensions or orientation.

The [output picture](../docs/images/estimated-bracket.png) is an OpenSCAD view
of that generated STL. This input was a render, not a photograph. The raw
output was not repaired, mesh-qualified, sliced, or printed. Use the example to
understand the distinction between inferred shape and source-based CAD, not as
evidence that an arbitrary photograph yields a dimensionally correct part.

## Relief master and casting tray

`scad_build.mesh` built both variants of
[flower-mold.scad](../examples/workflows/flower-mold.scad). The master measured
46 × 46 × 5 mm and the tray 58 × 58 × 14 mm. Both passed solid-topology and
support-free-orientation criteria; their assessments remain incomplete because
other manufacturing and physical criteria were unmeasured.

`scad_build.image` generated the 700-pixel OpenSCAD previews from the same source
and variants. `scad_build.section` on the master at Z = 4 mm generated the SVG.
The source uses a 2 mm tray floor, a 5 mm master placed on it, and a rim at
14 mm, leaving 7 mm above the master peak. These are source-defined dimensions,
not a thickness or silicone-volume measurement.

The example demonstrates a positive master integrated into a casting tray.
It does not establish material compatibility, leak resistance, release, or a
successful casting. No physical print or casting was attempted.
