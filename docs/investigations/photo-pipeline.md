# Investigation: photographs to printable geometry

Status: draft for discussion. This page records research and proposals; none of
it describes implemented behavior. It will be split into GitHub issues once the
proposals are agreed.

## Summary

Printable already turns photographs into three kinds of output: relative depth
and normal maps (`photo.depth`), a generated object mesh (`photo.object`, with
Pixal3D or TRELLIS.2), and a fitted FLAME head with a watertight bust
(`head.reconstruct` and `head.photo_to_bust`). The three routes share no
preparation or finishing steps. Only the bust route returns a solid that is
ready for slicing; the others leave scale, relief construction and repair to the
client agent.

The proposals below aim to:

1. Add fast metric depth, a valid-pixel mask and a reliable focal length
   estimate alongside Marigold V2's high-detail normals, as chosen by the
   [evaluation](#evaluation-results).
2. Give every photograph route the same preparation (subject mask, depth,
   normals, focal length) and the same finishing (exact solid rebuild and print
   checks) that busts already have.
3. Add a typed photo-to-relief action and, later, a sculpt refinement that fits
   generated geometry to the photograph and reports how closely it matches.

The service is non-commercial, so model choices here are made on quality alone.
Each model's own terms still apply to obtaining and redistributing its files.

## Sources reviewed

- [gokayfem/image-to-sculpt](https://github.com/gokayfem/image-to-sculpt), its
  skill and reference files, and the related skills by the same author:
  [video-to-blender](https://github.com/gokayfem/video-to-blender),
  [video-to-physics](https://github.com/gokayfem/video-to-physics),
  [shader-skills](https://github.com/gokayfem/shader-skills) and
  [video-to-css](https://github.com/gokayfem/video-to-css).
- [apple/DepthPro2](https://huggingface.co/apple/DepthPro2) model card.
- [microsoft/MoGe](https://github.com/microsoft/MoGe) (MoGe-3, August 2026).
- Published comparisons of image-to-3D generators for printing, depth-map
  relief tools, and MCP servers for CAD, Blender, slicing and printers.
- Discussions in r/3Dprinting, r/ClaudeAI, r/StableDiffusion and r/comfyui.

These are desk research, except for the depth and normal models, which were
run and measured as described in [Evaluation results](#evaluation-results).

## What the reference skills do well

image-to-sculpt turns one image into a watertight sculpt and measures the
result against that image. Its useful ideas:

- **The photograph governs what it shows.** Metric depth, focal length, normals
  and a subject mask come from the photograph. Generated views only fill in
  surfaces the photograph does not see, and their depth is never used.
- **Generated views are registered and screened.** Each view's camera angle is
  found by search rather than trusted from the prompt; views requested at +45°,
  +90° and −90° were found at roughly +20°, −115° and +35°. A view that
  disagrees with the others is rejected with a recorded reason.
- **One view is held out.** It never shapes the surface and is reported
  separately, as a check on consistency.
- **Fitting happens in many small recorded passes**, from overall forms to
  surface to detail. Outlines and depth move the shape through a stiff solve;
  normals move it through a flexible one. An exact signed distance field is
  rebuilt after every step, which avoids stair-stepping and ring artefacts.
- **Several measurements, never one score.** For each view: silhouette overlap
  (IoU), outline distance in pixels, median normal angle, roughness relative to
  the reference, and depth error. A sheet places the reference, its normals,
  the sculpt, the normal error and the silhouette difference side by side.
- **Every step is recorded** and replayed in Blender as a timelapse and
  turntable.

The other skills share the same method: measure the reference, build an
editable result, check it with numbers and images, and say plainly what does
not match.

## Depth models

| Model | Output | Strengths | Notes |
| --- | --- | --- | --- |
| Marigold V2 (current) | Relative log depth; normals with a second pass | Strong detail from a diffusion prior | About 20B parameters; about 66 s for depth and normals at 1024 × 688 on a 16 GB card; no scale or focal length |
| MoGe-3 ViT-g (1.25B) and ViT-L (370M) | Metric point map, depth, normals, intrinsics and valid-pixel mask in one pass | Sparse 3D refinement aimed at fine and thin structures | Released August 2026; three refinement steps by default |
| Depth Pro 2 (S, B, L, H+; H+ is 872M) | Metric depth and focal length; single images and video | Sharpest object boundaries and frame-to-frame stability in Apple's reported results | No normals; sky and far background can return infinite or very large depth |
| Depth Anything 3 (monocular and metric) | Relative or metric depth; multi-view variants | Widely used for reliefs; multi-view and video input | Used with MoGe-2 normals by the Relief Forge tool |

No published benchmark compares MoGe-3 and Depth Pro 2 directly; their papers
report different measurements. The choice should come from our own tests.

Relief tools that produce good results combine depth and normals: depth sets
the large-scale shape and front-to-back order, normals supply fine detail. Depth
alone gives soft reliefs; integrated normals alone drift into tilted or bowl
shapes.

## Image-to-3D generators and printability

An independent test in September 2026 sent four difficult photographs (thin
spikes, a through handle, wire glasses, a robot with separate parts) through
five generators and checked the raw meshes with the same conditions Bambu Studio
uses for its warning. Nine of twenty failed:

| Generator | Failed | Typical defect |
| --- | --- | --- |
| TripoSR | 4 of 4 | Open edges and many shells from marching cubes |
| TRELLIS | 1 of 4 | A pinch where two surfaces share a grid cell |
| TRELLIS.2 | 4 of 4 | Thousands of non-manifold edges on thin parts |
| SAM 3D Objects | 0 of 4 | Clean shells that float apart or overlap |
| Hunyuan3D 3.1 (hosted) | 0 of 4 | Clean; repaired on the service side |

All twenty passed after repair; four needed a full volumetric rebuild. Pixal3D
is built on the TRELLIS.2 backbone, so both of Printable's object backends
should be expected to need a solid rebuild.

## Market and user feedback

- **Competing tools cover one stage each.** Code CAD servers (build123d-mcp,
  openscad-mcp, SOMA, text-to-cad), Blender bridges (BlenderMCP, about 34k
  stars, and the Blender Foundation's server), printer servers
  (bambu-printer-mcp, mcp-3d-printer-server) and photo-relief web tools (Relief
  Forge, DepthCarve, Depthue). Printable covers modeling, measurement,
  rendering, slicing and printing on one self-hosted GPU.
- **Printers distrust raw AI geometry.** Common replies on r/3Dprinting: it
  works for boxes and lids, fails as complexity grows, and its output is hard
  to check. Users asking for image-to-print tools often want results that do not
  look generated.
- **Photo reliefs have clear demand.** HueForge, DepthCarve and Depthue sell
  relief and lithophane workflows, and a tactile relief of a dog carved for a
  blind owner drew about 30,000 votes on r/woodworking.
- **Verification is the differentiator.** Benchmarks of code CAD agents show
  that runnable output is often geometrically wrong; executable checks improve
  results more than images alone. Printable's existing measurement and evidence
  approach fits this need.

## Current state in Printable

- `photo.depth` returns a 16-bit relative log depth map and an optional normal
  map. Transparency becomes black; there is no subject mask parameter.
- Relief and mold construction is done by the client agent following the
  `image-to-mold` skill: it calibrates heights, compresses large depth steps,
  writes a height map and builds the mesh with `blender_execute` or OpenSCAD
  `surface()`.
- `photo.object` writes the generator's surface as an STL that is "never checked
  or repaired"; the guide recommends a manual voxel remesh in Blender. When the
  focal length is not given or found in EXIF, the default field of view is 20°.
- The exact winding-number remesher in `printable-geom` (`remesh.rs`) and the
  plinth, hollowing and printer settings of `head.prepare_print` are used only
  for busts.
- Normal maps are never turned into geometry.
- `validate_mesh` can compare a mesh with a reference mesh and report missing
  and extra material, which can score the comparisons below.

## Proposals

### Improve existing capability

1. **Geometry backends.** Measured results are in
   [Evaluation results](#evaluation-results). Keep Marigold V2 for the finest
   surface normals; add MoGe-3 for fast depth, normals and a valid-pixel mask;
   add the Depth Pro 2 field-of-view estimator for focal length. Keep the
   relative map for compatibility.
2. **Depth and normal fusion.** Produce one height field that takes large-scale
   shape from depth and fine detail from integrated model normals, without
   integrating across depth jumps. For shallow reliefs such as coins, let the
   normals carry almost all of the shape.
3. **Subject mask.** Add automatic background removal (BiRefNet is already in
   the object stack), alpha input, and an optional text-selected subject.
4. **Camera data for objects.** Pass the Depth Pro 2 focal length estimate to
   `photo.object` instead of the 20° default. Do not size objects from metric
   depth automatically: every model misjudged the distance of close-up
   subjects by several times, so physical size must still come from the user.
5. **Server-side solid rebuild for objects.** Run the exact remesher on
   generated meshes, merge or report separate shells and floating parts, and
   report thin walls before returning the STL.
6. **Comparison with the photograph.** Render the result from the estimated
   camera and report silhouette IoU, outline distance, normal angle error and
   depth error side by side, with a comparison sheet.

### Refactor and streamline

1. **One photograph preparation step** that records mask, depth, normals, focal
   length and the working image with their hashes, reused by depth, relief,
   object and head routes.
2. **Relief construction in Rust.** Move height calibration, step compression,
   base and border into `printable-geom` so the skill only chooses parameters.
3. **General print preparation.** Offer the bust's exact remesh, plinth,
   hollowing with drain hole, and printer settings for any mesh; busts then use
   the shared action.
4. **Smaller tool surface.** Consider merging `printer` and `print`, and folding
   `render` into `job`.
5. **Split the largest modules** (`jobs.rs` and `tools.rs`, each over 7,000
   lines) along ownership boundaries.

### New features

1. **Photo to relief** with typed presets: plaque, coin or medallion,
   lithophane (flat or curved), and mold (inverted master or cavity). Inputs in
   millimetres; outputs a checked STL and a 16-bit height map.
2. **Sculpt refinement.** Start from the Pixal3D or TRELLIS.2 mesh, then fit it
   to the photograph's normals, outline and depth in recorded passes, with the
   photograph governing visible surfaces. Each pass is a checkpoint, so existing
   render jobs can produce a build timelapse and turntable.
3. **Several photographs or a turntable video** of one object, with camera
   angles found by registration and one view held out.
4. **Generated extra views**, screened against each other before use.
5. **Print finishing:** shell merging, splitting tall models with alignment
   pegs, orientation suggestions, and later mapping photograph colours to
   filament slots.

### Suggested order

1. Photograph preparation with the chosen metric depth model and masking.
2. General print preparation and solid rebuild for generated objects.
3. Photo to relief.
4. Comparison with the photograph.
5. Sculpt refinement.
6. Multiple photographs, video, and generated views.

## Benchmark plan

This plan supports the first proposal: choose the depth and normal backend by
measurement rather than published claims.

### Hardware

A workstation with an NVIDIA RTX 4090 (24 GB). Models run outside the
Printable containers in an isolated Python environment, so results compare the
models themselves; integration performance is measured separately later.

### Models

- Marigold V2 as currently configured (baseline)
- MoGe-3 ViT-g and ViT-L
- Depth Pro 2 H+ and L, with its focal length estimator
- Depth Anything 3 monocular and metric
- Fusion candidates: best depth combined with MoGe-3 normals, and with Marigold
  normals

### Test images

Use cases, with three to five images each:

| Use case | Examples | What matters |
| --- | --- | --- |
| Portrait relief | Faces, pets | Facial detail, no bowl or tilt |
| Object photo | Figurine, product on a table | Edges, scale, focal length |
| Flat artwork | Logo, coin, embossed card | Flat regions stay flat |
| Thin structures | Plants, wire, fingers | Thin parts kept separate |
| Difficult surfaces | Glossy ceramic, metal | Robustness to reflections |
| Known geometry | Printed parts with known dimensions, and rendered scenes with exact depth | Measured accuracy |

Objects of known size, and Blender renders with exact depth and normals, give
reference values. The other photographs are judged visually and for
consistency.

### Measurements

- Depth error against references: absolute relative error and the share of
  pixels within 5 % (metric models) after scale alignment for relative models.
- Focal length error against EXIF or the render camera.
- Normal angle error against rendered references: median and share under 15°.
- Edge sharpness: boundary F1 against rendered depth edges.
- Relief quality: build a relief from each map with the same settings and
  compare it with the reference relief using `validate_mesh` comparison.
- Runtime and peak GPU memory at 1024 and 2048 pixels.
- A comparison sheet per image: input, depth, normals, relief render, and error
  maps, so results are judged by eye as well as by numbers.

### Outputs

A results page under `evidence/` with the sheets and tables, the exact model
revisions and settings, and a recommendation for the default backend and
fusion method. Results apply to these images and settings; they are not a
general ranking.

## Evaluation results

Measured on 25 November 2026 on one NVIDIA RTX 4090 (24 GB), in a container
built from `pytorch/pytorch:2.14.1-cuda13.0-cudnn9-devel`, the PyTorch and CUDA
versions Printable's photograph worker uses. Marigold V2 ran through
Printable's own worker code (`recon/printable_recon/marigold.py`) after
conversion with `recon/convert_marigold.py`, reading the converted model from a
local SSD. Each model ran once per image; there was no other GPU work.

### What was measured

- **Five rendered scenes with exact depth and normals** (Blender 5.3, Cycles):
  a 17 mm stone relief plaque, a 2.2 mm silver coin, a sculpted head on a
  pedestal, objects on a table, and thin wire structures. Every model received
  the same 1024 × 768 image.
- **Fifteen photographs** from Wikimedia Commons, judged by eye and, where the
  file records it, against the EXIF focal length: a portrait, a dog, two busts,
  a porcelain figurine, a blurry phone photo of a toy, a silver coin, a stone
  relief, a glossy teapot, a wine glass, a dandelion, a bicycle, a sneaker,
  chess pieces and a painting. Inputs were reduced to 1024 pixels on the
  longest side.
- **Models:** Marigold V2 (baseline), MoGe-3 ViT-g and ViT-L, MoGe-2 ViT-L with
  normals, Depth Pro 2 H+ and L, Depth Anything 3 nested giant (1.1) and mono
  large. Depth Anything 3 ran at 1008 pixels instead of its 504-pixel default.

Depth is compared after the alignment a user would apply anyway: scale and
offset (in log depth for Marigold). Relief error is measured within the
relief, coin and head regions after removing a best-fit plane.

### Averages over the rendered scenes

| Model | Depth error | Normals from depth | Model normals | Relief error | Edge F1 | Focal error, renders | Focal error, photos | Time at 1024 px | GPU memory at 1024 px | Time at 2048 px | GPU memory at 2048 px |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Marigold V2 (current) | 1.00 % | 10.2° | 3.3° | 11.6 % | 0.98 | none | none | 6.1 s | 1.9 GB | 13.5 s | 2.5 GB |
| MoGe-3 ViT-L | 0.99 % | 9.5° | 3.6° | 10.7 % | 0.97 | 35 % | 23 % | 0.26 s | 2.5 GB | 0.36 s | 2.5 GB |
| MoGe-3 ViT-g | 1.06 % | 8.5° | 4.0° | 8.1 % | 0.98 | 36 % | 32 % | 0.60 s | 6.7 GB | 0.66 s | 6.7 GB |
| MoGe-2 ViT-L normal | 1.09 % | 9.3° | 4.0° | 10.2 % | 0.92 | 34 % | 20 % | 0.06 s | 2.3 GB | 0.08 s | 2.3 GB |
| Depth Pro 2 H+ | 1.34 % | 12.0° | none | 25.8 % | 0.99 | 19 % | 8 % | 0.09 s | 5.2 GB | 0.40 s | 8.6 GB |
| Depth Pro 2 L | 1.01 % | 10.2° | none | 12.4 % | 0.99 | 19 % | 8 % | 0.05 s | 3.1 GB | 0.22 s | 6.5 GB |
| Depth Anything 3 nested giant | 1.48 % | 8.0° | none | 10.2 % | 0.85 | 33 % | 38 % | 0.25 s | 8.7 GB | 1.30 s | 13.5 GB |
| Depth Anything 3 mono large | 1.03 % | 4.4° | none | 12.2 % | 0.92 | none | none | 0.07 s | 2.4 GB | 0.37 s | 4.8 GB |

Depth error is mean absolute relative error. Normal columns are median angle
errors on smooth surfaces. Relief error is RMS height error as a percentage of
the region's relief depth. Edge F1 covers the head, table and thin scenes.
Focal error is the mean absolute error, on the renders and on the four
photographs with an EXIF focal length. MoGe resizes its input internally, so
its time barely changes with image size.

### Model normals by scene (median error)

| Scene | Marigold V2 | MoGe-3 ViT-L | MoGe-3 ViT-g | MoGe-2 ViT-L |
| --- | --- | --- | --- | --- |
| Coin (2.2 mm relief) | **3.2°** | 4.0° | 8.3° | 6.0° |
| Relief plaque | 2.0° | **1.8°** | 2.6° | 2.6° |
| Head | **3.4°** | 5.8° | 5.0° | 5.2° |
| Table | 5.4° | 4.7° | **1.7°** | 3.8° |
| Thin structures | 2.5° | **1.7°** | 2.5° | 2.3° |

### Fusion of depth and normals

| Scene | Depth only (best) | Best fusion |
| --- | --- | --- |
| Relief plaque (17.3 mm) | Depth Anything 3 nested: 0.64 mm, 5.6° | Same depth with MoGe-3 ViT-g normals above 4 px: 0.66 mm, 3.2° |
| Coin (2.2 mm) | MoGe-3 ViT-g: 0.24 mm, 23.7° | Marigold V2 normals integrated alone: 0.31 mm, 3.8° |
| Head (412 mm deep) | Depth Pro 2 H+: 34.5 mm, 15.0° | No improvement in height error; Marigold depth and normals at 16 px: 39 mm, 10.2° |

Integrating normals improves surface direction everywhere and is essential at
coin scale, where depth alone gets the height right but the surface wrong.
For deep subjects with occlusions, depth alone gives the best height.

### Findings

1. **Plain depth accuracy does not separate the models.** After the same
   alignment, all eight are within 1.0–1.5 % mean error. Differences are in
   normals, fine detail, artefacts, focal length and speed.
2. **Marigold V2 gives the most detailed surfaces.** Its normals had the lowest
   average error and, on photographs, the crispest lettering, wrinkles and
   engraving. Its depth, however, gives poor surface directions on deep scenes
   (13° on the head, table and thin scenes), so it should not supply broad
   shape. At 6–14 seconds per image on this GPU it is 10–50 times slower than
   the alternatives.
3. **MoGe-3 is the best fast all-rounder.** ViT-L gave nearly Marigold-level
   normals in a quarter of a second with 2.5 GB, a valid-pixel mask and clean
   surfaces. ViT-g gave the best relief shape among single models.
4. **Depth Pro 2 is best for focal length and edges, not surfaces.** Its focal
   estimate was within 1–14 % of EXIF on the photographs; the others were off
   by up to 95 %. Its depth edges were sharpest, but its surfaces were noisy, and
   H+ turned the raised coin head into a hollow on both the render and the
   photograph.
5. **Depth Anything 3 is good at shape but has artefacts.** Nested giant gave the
   best relief-plaque height and mono large the best normals from depth, but both
   showed a visible grid texture, and nested giant needed 13.5 GB at 2048 pixels.
6. **Metric scale is not reliable for small subjects.** Every metric model put
   the coin and relief several times too far away, on renders and on the coin
   photograph (0.4–1.3 m). Real-world size must come from the user.
7. **Artwork needs a choice.** Marigold V2 and MoGe treat a painting as nearly
   flat, which is physically right. Depth Pro 2 and Depth Anything 3 invent a
   sculpted face, which is what a portrait relief from artwork needs.
8. **Masks come free.** MoGe-3's valid-pixel mask separated the subject from
   plain museum backgrounds.

### Recommendation

The larger [depth benchmark](../../evidence/depth-benchmark.md) (nine scenes,
three execution tiers) replaces this section's model choices; its routes are:

- Marigold V2 for normals and, fused with those normals, for depth: the best
  surface quality measured, at 2.0 GB of GPU memory or about 6.5 minutes per
  image on CPU.
- MoGe-3 ViT-L for the subject mask on the GPU, MoGe-2 ViT-L on CPU.
- The Depth Pro 2 field-of-view estimator for focal length, including for
  `photo.object`.
- Build reliefs by fusing depth (broad shape) with integrated normals (detail),
  without integrating across depth jumps, letting normals dominate as relief
  depth gets shallower.
- Offer an "interpret as sculpture" option for artwork that uses Depth Pro 2 or
  Depth Anything 3 depth.
- Never derive physical size from metric depth without the user's confirmation.

### Limits

- The rendered scenes are computer-generated, and there are only five. The
  photographs have no true depth and were judged by eye, except for focal
  length.
- One run per image; timings exclude model loading and include one warm-up.
- Default settings were used except Depth Anything 3's processing size; Depth
  Pro 2 was not given EXIF focal lengths, so its own estimator was tested.
- Not tested: printed parts of known size, video input, or Pixal3D and
  TRELLIS.2 object generation.
