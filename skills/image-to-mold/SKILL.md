---
name: image-to-mold
description: Turn images, logos, or SVG artwork into calibrated relief depth maps and printable mold geometry through Printable. Use for positive masters, silicone casting trays, and direct negative cavities. Photograph reliefs are interpretations unless measured depth is supplied.
---

# Image to mold

Create reproducible relief geometry from the supplied artwork, with dimensions
and clearances suited to the intended casting process. A depth map defines one
height per XY location; it cannot describe undercuts or hidden surfaces.

## Establish the design

Locate and inspect the requested source and version. Prefer original SVG paths
for logos and lettering; retain the raster reference when it carries shading or
texture. Inspect an existing project before creating a replacement. Record source
identities and preserve earlier deliverables when making a revision.

Use the conversation to establish the finished part size, casting sequence,
relief depth, and construction. Ask only about consequential missing choices.
For a positive master that will produce a flexible silicone negative and then a
positive cast, keep the artwork readable on the master. Verify orientation through
the actual casting sequence for other constructions; do not mirror automatically.
Discover printer details when slicing or printing is requested.

Choose between a positive master, a master with an integral casting tray, and a
direct negative cavity. An integral tray holds liquid mold material around the
master. A direct cavity receives casting material itself and needs a defined
opening, remaining floor thickness, and removal direction. Simply negating the
heights does not design a usable cavity. If the form needs undercuts, use suitable
split geometry or a different modeling method.

## Author calibrated heights

For flat artwork, identify feature regions and assign deliberate heights. Image
brightness describes color and lighting, not necessarily relief. Preserve exact
text masks, counters, and separate strokes. Use bevels or rounded profiles when
they improve release without erasing useful detail. Keep foreground features and
shallow background texture independently adjustable.

For photographs, use supplied measured depth when available. Otherwise author or
infer a relief and label it as an interpretation, rather than recovered physical
geometry. Preserve calibrated depth data instead of replacing it with a display
preview or an image generated for appearance.

Save quantitative heights as a 16-bit grayscale PNG and record this decoding
equation, or an equally explicit alternative:

`height_mm = height_min_mm + pixel_value / 65535 * height_range_mm`

Record physical width and height, height origin, range, and whether the map
describes the master surface or cavity. For edge-inclusive samples, pitch is
physical width divided by sample count minus one. Record different sampling
conventions explicitly. Re-read the saved map as 16-bit data and check the
decoded heights, clipping, and peak against the intended values.

Image rows usually increase downward; model Y often increases upward. Document
the conversion and verify an asymmetric feature or readable lettering on the
model. Keep a human-readable preview separate from the numeric depth map.

## Build through Printable

Load `printable://modeling/blender-v1` for modeling and scene preconditions.
Discover the needed actions through the tool catalog and `printable://contracts`;
use the current contracts rather than inventing parameters. Clients without MCP
resource support use the schemas advertised in `tools/list`.
Use existing project, artifact, checkpoint, export, and rendering operations.
Retain an editable builder and feature parameters in the project.

Build in millimetres with a closed base and a declared release direction.
Choose mesh sampling to preserve the smallest useful feature, checking any
interpolation against the original outlines. Checkpoint before changing shared
scene state and use `expected_scene` where supported. Put large arrays and meshes
in files rather than tool responses or model context.

For a tray with a positive master, calculate these quantities using the same
model Z origin:

`master_peak_z = tray_floor_z + master_body_height + actual_relief_peak`

`silicone_cover = fill_z - master_peak_z`

`freeboard = tray_rim_z - fill_z`

Provide a suitable side gap, mold backing, tray floor, wall thickness, and removal
access. Measure minimum gaps and thickness across the full profiles. Check reverse
slopes on the master perimeter and tray walls. Cover and freeboard must remain
positive and suitable for the intended mold. When estimating fill volume,
subtract the displaced master volume from the tray interior below the fill level.

For example, a floor at Z = 3 mm, a 5 mm master body, and a 2.2 mm relief place
the master peak at Z = 10.2 mm. A fill level of 17.8 mm and rim at 20 mm provide
7.6 mm of silicone cover and 2.2 mm of freeboard. These values illustrate the
calculation; select dimensions for the actual design and material.

When raising maximum relief, change the intended feature heights and recompute
tray clearances. Extra height cannot recover a stroke that is too narrow for the
chosen printing process.

## Validate the delivered geometry

Check finite coordinates, closed oriented topology, boundary and nonmanifold
edges, degenerate faces, positive volume, physical bounds, bed contact, relief
direction, and remaining base thickness. Interpret Euler characteristic against
the design; intentional holes need not produce a value of two. For a tray, also
measure side gaps, wall thickness, fill level, cover, and freeboard. Report a
relevant property as unmeasured when the available check cannot establish it.

Re-read the exported STL or 3MF and verify that its geometry matches the validated
source. Render the imported export or the same verified vertices, using
`printable://render/product-v1` for presentation guidance. Preserve this
relationship with hashes and numerical comparison when needed. A smooth shaded
preview does not prove that the delivered mesh or printer preserves the detail.

Closed mesh topology does not establish that an FDM tray is leakproof, that a
material is suitable for food contact, or that the casting process releases
successfully. Describe those properties as tested only when there is applicable
physical evidence.

## Slice and print when requested

Use native slicing with the actual printer, nozzle, material, plate, and explicit
placement. A positive master with a tray normally sits flat on its base with the
relief facing up. Select layer height and wall generation for the useful detail.
Inspect actual first-layer, background, lettering, and peak-feature toolpaths.
Explain important features that merge or disappear and propose enlargement or
simplification when needed. Retain source hashes, profiles, overrides, estimates,
and unresolved native warnings. Keep changed slices in new output paths.

Read `printable://printing/workflow-v1` before physical dispatch. Reuse existing
authorization and current setup answers; creating a model or slice does not itself
authorize starting a printer. Map logical filament slots to observed physical
tray IDs. A camera can supplement readiness information but may not show the
whole bed or identify its surface.

After an authorized start, verify the intended job in current telemetry. Report
preparation, running, pause, failure, or completion as observed; an accepted
request or queue entry alone does not confirm physical printing. Reconcile an
uncertain mutation outcome before retrying.

## Deliver reproducible files

Deliver the calibrated depth map, printable geometry, actual-geometry render,
editable source, and a compact design record. Include source and output hashes,
decoding equation, axis convention, casting sequence, measured dimensions and
clearances, validation evidence, and limitations. Add a sliced 3MF and estimates
when requested.

Use `artifact.publish` for immutable file handoff and the client's download
mechanism to retrieve and verify the bytes. Inspect current transfer limits.
When a smaller lossless representation is needed, preserve numeric arrays and
reconstruct the export with verified geometry; for NumPy archives, disable pickle
loading. Do not reduce mesh resolution just to fit transport limits. Keep the
immutable file identity distinct from a mutable project path.
