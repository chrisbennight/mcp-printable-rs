# Image to mold skill

Printable bundles [image-to-mold](../skills/image-to-mold/SKILL.md) for an MCP
client to run with its modeling and image-analysis capabilities. It guides the
conversion of artwork into a calibrated relief depth map, a positive master or
negative cavity, and an optional silicone casting tray. The workflow includes
geometry validation, renders of the delivered geometry, and requested slicing.

## Discover and load

Call the `skill` tool to discover bundled instructions:

```json
{"action":"list","params":{}}
```

Load this skill with:

```json
{"action":"get","params":{"name":"image-to-mold"}}
```

The response includes complete `instructions` and metadata containing the name,
description, resource URI, and SHA-256 of the exact UTF-8 Markdown. MCP resource
clients can read the same bytes with `resources/read`:

```json
{"uri":"printable://skills/image-to-mold/SKILL.md"}
```

Discovery and loading require no modeling or printer backend. The file is
self-contained and needs no external skill catalog or installation. Loading
instructions does not create geometry, run depth estimation, or control a printer;
the client performs the workflow through Printable's existing public tools.

## Scope and limits

The client preserves artwork identity and casting orientation, assigns feature
heights, and records the physical scale of a 16-bit depth map. It calculates tray
clearances from the actual relief peak, validates exported solids, and renders
the same geometry supplied for printing. Slicing uses the requested setup and
reviews actual toolpaths, including features too narrow to reproduce.

A height map cannot represent undercuts. Photograph relief is an interpretation
unless measured depth is supplied. Mesh validation does not establish physical
leak resistance, material compatibility, or successful release. Physical printing
uses the existing [printing workflow](printer-integration.md) and requires
authorization covering that action.

MCP regression tests verify discovery, exact retrieval, digest integrity, resource
agreement, output schemas, and availability without modeling or printer services.
