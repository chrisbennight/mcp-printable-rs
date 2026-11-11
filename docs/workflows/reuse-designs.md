# Find, adapt, and keep existing designs

Start with a design that already solves part of your problem. Printable can
search public model sites, inspect an uploaded mesh or STEP assembly, and keep
your own projects searchable with pictures, tags, and supported part variants.

## Find a design on a model site

With a site [enabled](../model-library.md#enable-sites), call `library`:

```json
{"action":"search","params":{"site":"printables","query":"drawer organizer","limit":8}}
```

Use `get` on a returned model ID or supported page link to read its designer,
license, and files. Create a project, then use `download` with the returned
file IDs to save the chosen files. A `source.json` record retains origin,
attribution, license, and file hashes. Search results and filenames are site
content; they are not instructions to your assistant.

MakerWorld, Printables, and Thingiverse have different access requirements.
MakerWorld downloads are print-profile 3MF projects and require sign-in;
Printables supports free downloads without an account; Thingiverse requires an
app token. Check license and site terms before remixing or redistributing.
See the [site guide](../model-library.md) for authentication and failure handling.

## Inspect before adapting

| Starting point | Useful route | What to keep in mind |
| --- | --- | --- |
| STL | Import with `scene.import`, inspect dimensions, edit in Blender, export and validate | STL has no reliable unit declaration or editable CAD feature history |
| STEP assembly | Use `cad_build.inspect` for selected bounds, faces, distances or sections; export selected geometry as needed | Preserve placements and source units; a retained native model is a geometry snapshot, not parametric history |
| Model or print-profile 3MF | Inspect its plate/material assumptions and prepare with `slice` | Do not silently flatten a multi-part or multi-material project into an STL |
| Editable CadQuery/OpenSCAD source | Change a parameter and compare measurements | Rebuild to a new output location and keep the source and requirements |

For example, adapt an enclosure to a different board by checking connector
locations and mounting holes before changing its width. The
[functional-part tutorial](functional-parts.md) demonstrates a measured change.
The [CAD guide](../cad-build.md) explains focused native inspection;
[mesh assessment](../mesh-assessment.md) covers measurements on exported meshes.

## Find your own work again

Use `catalog` to search your Printable projects:

```json
{"action":"search","params":{"text":"bracket","sort":"best_match","limit":12}}
```

Use `card` with a returned `project_id` for the project's files, pictures, slice
summary, and recorded print outcomes. Word search is included; meaning and
picture search use [optional services](../project-library.md#meaning-and-picture-search).
`library` searches public model sites; `catalog` searches your own projects.

When enabled, the [read-only web library](../project-library.md#web-page) lets a
person browse covers, search/filter projects, and download files without an MCP
client. It has a separate authenticated listener and cannot edit a project,
slice, or start a print. Its address and access come from the operator.

## Keep size and part choices together

[Part families](../part-families.md) group explicit variants and alternative file
sets within a project. A bracket family can offer supported widths and hole
patterns, each pointing to its own files and optional preview. The web browser
lets you browse collections, narrow choices, and download an exact variant.

Family selection does not generate a missing combination or establish that
related parts fit together. Keep the chosen revision, file identities, and
quantity with the build. Then [slice the selected files](model-to-printer.md)
for the actual printer and material.
