# Printable documentation

Use Printable through an MCP client to make models, inspect and revise them,
render images and videos, and prepare and send prints. The server runs on your
own Linux NVIDIA host; your client provides the assistant and conversation.

## Choose a task

| I want to… | Start here | What I get |
| --- | --- | --- |
| Install and try Printable | [First model](getting-started.md) | A dimensioned bracket, STL, Blender checkpoint, and video |
| Make a part to specific dimensions | [Functional parts](workflows/functional-parts.md) | Editable source, measured changes, STEP and STL |
| Find or reuse a design | [Existing designs and project library](workflows/reuse-designs.md) | Imported models, attribution, searchable projects, and supported variants |
| Turn artwork or a photo into geometry | [Images, reliefs, molds, and busts](workflows/images-to-models.md) | A calibrated relief, mold geometry, object mesh, or portrait bust |
| Work in Blender | [Model, inspect, and render](workflows/blender.md) | Scene edits, previews, saved work, and animation |
| Slice and send a print | [Model to printer](workflows/model-to-printer.md) | Reviewed toolpaths, a printer-ready package, and optional physical printing |

## Engines and integrations

| Capability | What it helps you do | Setup |
| --- | --- | --- |
| Blender | General modeling, materials, native viewport/editor observations, stills and animation | Included in the standard installation, with a separate background render process |
| OpenSCAD | Build parameterized solids, images, and sections from source | Included in the server image |
| CadQuery and STEP | Build native CAD, inspect selected components/features, and export geometry | Native CAD worker in the standard installation |
| OrcaSlicer | Select profiles/settings, slice STL or 3MF, and review generated toolpaths | Native slicer worker in the standard installation; profile availability is not universal printer compatibility |
| Project library and part families | Find your own work, browse covers, and choose explicit design variants | MCP tools are included; the authenticated read-only web library is [optional](project-library.md#web-page). Meaning/picture search needs additional configured services |
| MakerWorld, Printables, Thingiverse | Find other people's designs and retain their files, source, and license | [Enable sites](model-library.md); authentication and download access differ by site |
| Bambuddy | Send printer-ready packages, review materials/hardware, stage/start jobs, and read status or camera frames | [Configure the printer integration](printer-integration.md); a printer is unnecessary for modeling or slicing |
| Marigold V2 | Estimate relative photo depth and normals for relief work | [Optional photograph worker and model files](photo-depth.md) |
| Pixal3D or TRELLIS.2 | Generate an object's shape from an image | [Optional object generator](photo-object.md); available models appear in status |
| FLAME and reconstruction models | Create parametric heads and prepare busts, optionally from photographs | [Separately obtained models](flame-models.md); reconstruction also needs the photograph worker |
| Client-run skills | Guide FDM preparation, image-to-mold work, and printer-camera assessment | Load through `skill`; the client runs the instructions and supplies any required vision or scheduling |
| Obico | Detect visible print failures through Bambuddy | [Optional service](print-failure-detection.md), configured in Bambuddy |

## Reference and operation

- [Installation](installation.md), [operations and backups](operations.md),
  [storage](storage.md), and [release images](releases.md).
- [Tool and action reference](workflow-interface.md), [native CAD](cad-build.md),
  [mesh measurements](mesh-assessment.md), and [slicing](slicing.md).
- [Projects and scene switching](project-scenes.md), [design revisions](project-revisions.md),
  [library records](project-library.md), and [part families](part-families.md).
- [Blender modeling guidance](../crates/printable-server/resources/blender-modeling-v1.md),
  [scene state](scene-state.md), and [render recovery](render-worker.md).
- [Printer setup](printer-integration.md), [printing instructions](../crates/printable-server/resources/printing-workflow-v1.md),
  and [retained design/print evidence](delivery-evidence.md).
- [Architecture](architecture.md), [contributing](../CONTRIBUTING.md),
  [documentation writing](writing.md), [CI](continuous-integration.md), and
  [security reporting](../SECURITY.md).

For help, open an issue in the
[maintained repository](https://github.com/chrisbennight/mcp-printable-rs/issues).
Include your revision or matching image set, the smallest reproducible example,
and expected versus observed behavior. Remove credentials and private paths.
