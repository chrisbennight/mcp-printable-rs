# Lithophanes

A lithophane is a thin plate that shows an image when light shines through
it: thin parts let more light through and look bright, thick parts look dark.
The `photo` tool's `lithophane` action builds one from a workspace image. It
runs on the CPU in the memory-isolated geometry worker, needs no photograph
worker, GPU or model files, and is always offered.

```json
{"action": "lithophane", "params": {
  "image": "photos/cat.jpg",
  "path": "lithophanes/cat.stl",
  "width_mm": 100
}}
```

The result is a binary STL, `NAME.preview.png` showing how the plate should
look against an even backlight, and `NAME.report.json` with the settings, the
mapping used and the checks. The three files are published in that order;
after a storage error, inspect the paths before repeating the request.

## From brightness to thickness

1. The image is decoded, and each pixel is converted from sRGB to linear
   light and composited over white, so transparent parts count as white
   whatever colour they hide. Its brightness is
   `Y = 0.2126 R + 0.7152 G + 0.0722 B`, kept in 16 bits.
2. The brightness is turned upright as the image's EXIF orientation says,
   and cropped; `crop_px` and the proportions refer to the upright image.
3. The crop is resized with a triangle filter to samples half the nozzle's
   diameter apart, averaging in linear light the pixels each sample covers,
   so fine black-and-white detail shows as mid grey. The nozzle cannot draw
   finer detail, and this is the only smoothing applied.
4. `contrast`, when given, stretches each sample's tone about middle grey on
   the sRGB scale.
5. Light keeps a fraction `T = exp(-k t)` of its intensity through `t`
   millimetres of filament, where `k` is the filament's attenuation per
   millimetre (the Beer-Lambert law). With `T_dark` the transmission of the
   thickest plate and `T_bright` that of the thinnest, each sample's thickness
   is `t = -ln(T_dark + Y (T_bright - T_dark)) / k`, clamped to the thickness
   range.

Against an even backlight, each part of the plate is then as bright, on the
scale from the thickest plate to the thinnest, as the image is in linear
light. The plate cannot be darker than `T_dark`, so the report gives its
contrast ratio, `T_bright / T_dark`. A unit test checks the mapping on a grey
ramp: the simulated brightness, on the sRGB scale from 0 to 1, stays within
0.01 of the ramp, while making the thickness proportional to the grey value
misses by more than 0.05. This is a property of the model; how closely a
print follows it depends on the filament, the printer and the light, and has
not been measured.

## Which side faces the light

The relief faces the light and the flat face faces the viewer. A flat
lithophane lies with its flat face on Z = 0, the side to print on the bed, and
its image is mirrored so it reads the right way round from the flat face. A
curved lithophane is wrapped around a vertical cylinder whose axis is the Z
axis: the flat face is the outside, the relief faces the axis, where the light
goes, and the image's top is at the top. Stand it on its lower edge to print.

A curved plate is built flat with a flat-face vertex under every sample, then
bent: each sample's flat-face and relief vertices land on one radial line, so
every cell's flat face becomes a chord one sample spacing wide. A chord falls
inside the cylinder by its sag, `R (1 - cos(s / 2R))` for spacing `s` and
radius `R`. While the sag is at most a hundredth of the minimum thickness, and
the radius exceeds the maximum thickness, the bent plate is a closed solid
without self-intersections by construction, and its wall is never thinner
than `min_thickness_mm` less that sag: up to 1% under the minimum on a curved
plate. A tighter curve is refused with `relief_not_printable` and the
smallest radius that fits; the report gives the curve's `chord_sag_mm` and its
`minimum_wall_mm`, the thinnest thickness less the sag. The flat face of a
curved plate has as many triangles as its relief, so its STL is about twice
the size of a flat plate's.

## Parameters

| Parameter | Values | Default |
| --- | --- | --- |
| `image` or `test_strip` | a workspace `.jpg`, `.jpeg`, `.png` or `.webp` image, with a lowercase suffix, or `true` for a [test strip](#measure-your-filament) | one is required |
| `path` | a new workspace `.stl` | required; `NAME.preview.png` and `NAME.report.json` are written beside it |
| `overwrite` | `true` to replace existing files | `false` |
| `width_mm` or `height_mm` | 10 to 300; the other follows the image's proportions | one is required |
| `shape` | `flat` or `curved` | `flat` |
| `radius_mm` or `arc_degrees` | curved only: 10 to 1000 mm, or 1 to 359 degrees of the whole plate, border included | 120 degrees |
| `min_thickness_mm` | 0.4 to 5: the brightest parts | 0.8 |
| `max_thickness_mm` | 0.6 to 10, at least 0.2 above the minimum: the darkest parts | 3 |
| `filament` | `white_pla`, `natural_pla` or `white_petg` | `white_pla` |
| `attenuation_per_mm` | 0.05 to 10: a measured `k`, in place of `filament` | none |
| `border_mm` | 0, or `nozzle_mm` to 50: a frame as thick as the darkest parts, rounded to whole samples | 0 |
| `crop_px` | `[left, top, width, height]` in image pixels | the whole image |
| `contrast` | 0.1 to 4; 1 leaves the tones unchanged | 1 |
| `nozzle_mm` | 0.2 to 1: sets the sample spacing and the narrow-detail check | 0.4 |

The defaults are recommended starting points, not measured optima:

- **0.8 mm minimum** is four 0.2 mm layers: thin enough to glow, thick
  enough to print without gaps.
- **3 mm maximum** gives white PLA's starting attenuation a contrast ratio of
  about 27, so dark parts look nearly black against a lamp.
- **Flat** prints without supports. A **120 degree** curve stands on its own
  and spreads a small light source evenly.
- **No border** keeps the plate to the image. A border of a few millimetres
  stiffens a large flat plate and gives it an edge to stand or mount by.

The samples are half a nozzle apart, so a 100 mm wide lithophane with a 0.4 mm
nozzle has 501 samples across. A plate needing more than 4,194,304 samples is
refused, counting the border at the grid's own spacing; make it or its border
smaller, or use a larger nozzle. The image may be at most 64 MiB and 8,192
pixels on a side, and its decoded pixels must fit in 512 MiB, which keeps the
decoded image, its brightness and a turned copy well within the geometry
worker's memory.

## Filament attenuation

| `filament` | `k` per mm |
| --- | --- |
| `white_pla` | 1.5 |
| `natural_pla` | 0.7 |
| `white_petg` | 1.0 |

These are documented starting values, not measurements: filaments of the same
name differ, and so do backlights. For the best tones, measure your own.

## Measure your filament

`test_strip: true` builds a strip of ten equal steps from the minimum
thickness to the maximum, thinnest at the left as seen from the flat face, with
the same settings otherwise. Its width over its height is 5; give `width_mm` or
`height_mm` as for an image.

```json
{"action": "lithophane", "params": {
  "test_strip": true,
  "path": "lithophanes/strip.stl",
  "width_mm": 80
}}
```

1. Print the strip with the filament, layer height and printer you will use.
2. Photograph it against your backlight with a fixed exposure, in a dark room,
   so that no step is pure white.
3. Average the pixel values of each step and convert them to linear light:
   for an 8-bit value `p`, `v = p / 255`, linear is `v / 12.92` when
   `v <= 0.04045` and `((v + 0.055) / 1.055) ^ 2.4` otherwise.
4. Fit a straight line to the natural logarithm of each step's linear
   brightness against its thickness. `k` is minus the slope.
5. Pass `k` as `attenuation_per_mm` for your lithophanes.

The report's `test_strip.steps` lists each step's thickness and the brightness
the current `k` predicts, relative to the thinnest step, for comparison.

## Results

The result holds the following; the saved report holds the same except
`artifacts`:

- `settings`: every choice with its default filled in;
- `image`: the image's workspace path and SHA-256, or `null` for a test strip;
- `lithophane`: the solid's size in millimetres and triangle count, the
  image's size, the border's width along each side, the image's sample count
  and spacing, the thinnest and thickest points (a border is as thick as the
  maximum thickness), the shape (with the radius, arc, chord sag and minimum
  wall of a curve), the tone mapping
  (its formulas, `attenuation_per_mm` and where it came from, the
  transmission range and the contrast ratio), the checks, what the preview
  shows and, for a test strip, its steps;
- `artifacts`: the STL, the preview and the report.

The checks are those of a [relief](photo-geometry.md#relief-results): the
plate is closed, manifold and free of self-intersections by construction,
and narrow ridges and grooves are reported without failing the check.
`thickness_within_range` confirms every sample lies within the thickness
range, and `passed` requires it as well as a sound solid. A plate that fails
is not written; the request returns `relief_not_printable`. A curved plate
is checked flat and kept sound by its construction, described under
[Which side faces the light](#which-side-faces-the-light).

The preview is an 8-bit sRGB image of the plate's brightness against an even
backlight, relative to the thinnest part, as seen from the flat face, one pixel
per sample. It shows the limited contrast: the thickest parts are
`1 / contrast_ratio` as bright, not black. The border is not shown.

A 100 mm wide lithophane from a 1024 x 1024 pixel image took about 0.2
seconds in the geometry worker on the CPU, a curved one about 0.4 seconds and
a 200 mm flat one under a second, each using under 100 MiB, in
[one recorded run](../evidence/lithophane-cpu-time.md) on an Apple silicon
Linux container; amd64 times were not measured. The STL is large: about 25 MB
for the 100 mm flat plate and 50 MB curved.
