# Part families and variants

Part families organize existing files without making every size or export a
separate project. A project acts as a collection; within it, families describe
recognizable designs, variants name supported choices, and file sets offer the
applicable exports. Ordinary projects work without any family records.

A bracket family might offer length and mounting pattern. An enclosure family
could offer board type and a boolean vent option. Attributes belong to each
family and have stable IDs, display labels, and a type: `text`, `number`, or
`boolean`. Numeric attributes may declare a unit. No global dimension schema or
geometry generator is required.

## Capture a family

Upload the files through `artifact` first. Read their bytes or otherwise verify
their SHA-256 identities; a pathname alone is not a source identity. Call
`project` with `family_put` and `expected_revision: null` for a new family:

```json
{
  "action": "family_put",
  "params": {
    "project_id": "mounts",
    "expected_revision": null,
    "family": {
      "id": "brackets",
      "name": "Mounting brackets",
      "category": "Brackets",
      "description": "Bracket length and hole-pattern options",
      "axes": [
        {"id": "length", "label": "Length", "kind": "number", "unit": "mm"},
        {"id": "pattern", "label": "Mounting pattern", "kind": "text"}
      ],
      "variants": [
        {
          "id": "short-two-hole",
          "name": "Short two-hole bracket",
          "attributes": {"length": 20, "pattern": "two-hole"},
          "notes": "Confirm fastener fit on a prototype",
          "representations": [
            {
              "id": "whole",
              "name": "Whole STL",
              "files": [{"path": "parts/bracket-20.stl", "sha256": "<verified SHA-256>"}]
            }
          ]
        }
      ]
    }
  }
}
```

Every variant specifies exactly the declared attributes, with matching types.
Each combination is unique. Only explicitly supplied combinations are available;
the server does not invent the product of all attribute values. A file set can
hold several files, such as parts of a split print. Whole, split, STL and STEP
exports can be alternative sets for one variant.

A family or variant may have a `preview` containing the project-relative `path`
and SHA-256 of a PNG, JPEG or WebP image. Family illustrations are representative;
only a variant's own image is described as its exact preview.

Optional `related` entries contain a `family_id` in the same project and a
`description`. They are reference suggestions, not verified assembly compatibility
or quantity requirements. They can refer to families added later in an import.

The response has `revision` and `record`. Editing requires the last read revision
as `expected_revision`; stale and concurrent edits are refused. Existing model
files are never rewritten. File identities are checked through confined streaming
reads rather than loading large models into memory.

## Browse and select

`project families` searches a project's families using family names, categories,
descriptions, variant names and attribute values:

```json
{"action":"families","params":{"project_id":"mounts","text":"two-hole","limit":24,"offset":0}}
```

Results contain family summaries rather than one card per variant. `total` and
`next_offset` describe the matching family page. `counts` covers the complete
project's family definitions and supported workspace artifacts, independently
of the search filter. Page limits are 1 to 100.

Read the complete current definition with `family`, or include `revision` to
reopen a retained definition:

```json
{"action":"family","params":{"project_id":"mounts","family_id":"brackets"}}
```

Select with partial attributes to narrow the available choices:

```json
{"action":"family_select","params":{"project_id":"mounts","family_id":"brackets","attributes":{"length":20}}}
```

The response contains matching variant summaries and the options still available.
Each option's values respect the other selected attributes. An unavailable
combination returns no matches, never a replacement. An unknown attribute is
refused. When exactly one variant matches, `selected` contains its file sets and
notes, and its source hashes are checked. Include `variant_id` and `revision` to
reopen an exact selection. Variant pages also support `limit` and `offset`.

Selected paths remain ordinary project files, so existing modeling, export and
slicing workflows apply. The returned digest identifies the observed bytes;
later modeling or slicing calls retain their own snapshot contracts. Family
selection does not start a build, generate geometry, slice, or print.

Retained revisions preserve definitions and file identities, not another copy of
every source model. If a source changes or disappears, exact selection fails
honestly; restore the original bytes or explicitly publish a new definition.
Back up `.printable/families` together with the project files. A failed write may
leave an unreferenced retained definition, but never advances the head early.

## Discovery and the web library

Family and variant terms join the project's derived search text. The main
catalog keeps its existing project cards. On the project page, families appear
before the ordinary files, and classified model files are kept within their
family rather than repeated as a flat list. Unclassified files remain accessible.

The project page presents family categories as collections with representative
pictures. Collection links open a filtered thumbnail grid; search and pagination
retain the selected collection. Family cards use the supplied family preview,
and families without one remain accessible with a labeled placeholder. Prepare
previews explicitly during authoring or ingestion; page visits never render a
model or modify records.
Thumbnail grids remain usable while model files download.

The read-only family browser has search and pagination. Open a family, choose
attribute values and submit the form to narrow the supported variants. No script
or additional external service is needed. A representative family picture stays
visible while choosing specifications, and a selected variant's own preview
takes precedence. Comparison columns show attribute labels and units.
Exact-variant links retain the family
revision; their downloads also check the selected file's SHA-256 against an
immutable transfer snapshot. Changing a file therefore cannot silently change
the bytes an old selection downloads.

The existing sign-in proxy, read-only methods, HTML escaping, workspace path
confinement and download admission apply to these pages and previews as well.

## Counts and assemblies

The family listing reports family and variant counts, unique classified file
paths, distinct declared file contents and repeated contents, plus the number
of supported workspace files outside the families. Shared references do not add
another physical file to these counts. Byte-identical content retains its
separate paths and provenance. `classified_file_identities` counts unique
path-and-hash pairs. If current families declare different hashes for one path,
`conflicting_files` flags that path and the web browser displays a notice.
Distinct-content counts retain all declared hashes; repeated contents count
additional path-and-hash pairs for the same bytes. Neither count depends on
family order. Counts describe the current records; an exact selection checks
that its bytes still match.

None of these counts is a mesh-body count or bill of materials. A build must
record selected variants, their retained revisions, chosen file sets and required
quantities. Similar names do not prove fit. Printer, material and layer settings
remain print-profile choices unless they actually change the design.

Public request and response schemas are available at
`printable://contracts/project/family_put`, `printable://contracts/project/family`,
`printable://contracts/project/families` and
`printable://contracts/project/family_select`.
