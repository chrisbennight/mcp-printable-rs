# Project library records

Each project keeps a record that a library card can show: its name and
description, tags, when it was created and last changed, and a cover picture.
Slices add a summary of what they were sliced for. Read
`printable://contracts/project/update` and `printable://contracts/project/cover`
for the typed requests.

## Project records

`project create` writes `.printable/projects/<project_id>.json` in format
version 2:

| Field | Meaning |
| --- | --- |
| `name`, `description` | Set at creation; `update` replaces the description |
| `record_version` | Starts at 1 and increases with every accepted change |
| `created_unix_ms` | Creation time in Unix milliseconds |
| `updated_unix_ms` | Time of the latest accepted change |
| `tags` | Up to 32 tags, omitted when empty |
| `cover` | The stored cover picture, omitted when there is none |

Records written before version 2 stay at format version 1 and are read
unchanged. They report `record_version: 0` and have no dates, tags or cover.
The first `update` or `cover` call rewrites such a record as version 2. Its
`created_unix_ms` is then estimated from the record file's modification time,
and `created_estimated: true` says so.

Creating a project again with the same name and description returns the
current record. A different name or description is refused; use `update` to
change the description.

## Edit the description and tags

```json
{"action": "update", "params": {"project_id": "desk", "expected_record_version": 3,
  "description": "Three-part drawer organizer", "tags": ["desk", "storage"]}}
```

Pass the `record_version` from the record you read. If the stored record has
changed since then, the edit is refused and nothing is written; read the
project again and reapply the change. Either field may be omitted, but not both.
Tags are trimmed and must be 1 to 64 characters without control characters.
Tags that differ only in letter case are merged, keeping the first spelling.

Cooperating server processes take the project's update lock while they change a
record. An edit that finds the lock held is refused as busy rather than queued.

## Cover pictures

```json
{"action": "cover", "params": {"project_id": "desk", "expected_record_version": 4,
  "source": "automatic"}}
```

`source` is one of:

| Source | Picture |
| --- | --- |
| `{"image": {"path": "photos/finished.png"}}` | A PNG, JPEG or WebP file in the project |
| `{"slice_thumbnail": {"package": "slice-1/model.gcode.3mf", "plate": 1}}` | A plate thumbnail stored in a sliced 3MF; `plate` defaults to 1 |
| `{"model_view": {"model": "parts/body.stl"}}` | An isometric 512-pixel OpenSCAD view of an STL |
| `"automatic"` | The first plate thumbnail of the newest sliced 3MF; otherwise a view of the newest STL |
| `"none"` | Remove the cover |

Paths are relative to the project directory. A model view runs OpenSCAD under
`timeout_seconds` (one hour by default) and needs an STL path without quotes or
backslashes. Pictures are decoded before they are stored and must be at most
16 MiB and 8192 pixels on a side.

The picture is copied to `.printable/covers/<project_id>/<sha256>.<ext>`, so the
card keeps it when the original file is replaced or deleted. The record's
`cover` names the stored file, its digest, media type, dimensions and origin.
Stored covers are never rewritten, and replacing a cover keeps the earlier
file. A cover request whose record version check fails can leave its new
picture stored without being referenced.

## Slice summaries

A completed slice records `summary.json` beside its outputs, and `slice status`
returns it. The [slicing guide](slicing.md) lists its contents and how older
slices are summarised.

## Storage and backups

The project record, its update lock and its covers live under the reserved
`.printable` directory and cannot be written as ordinary artifacts. Back up
`.printable` together with the `projects` directory.
