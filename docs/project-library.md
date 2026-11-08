# Project library records

Each project keeps a record that a library card can show: its name and
description, tags, when it was created and last changed, and a cover picture.
Slices add a summary of what they were sliced for, and print records keep how
each print ended. `card` gathers these into one library card, and `search` finds
projects by their cards. Read `printable://contracts/project/update`,
`printable://contracts/project/cover`, `printable://contracts/project/card` and
`printable://contracts/project/search` for the typed requests.

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

Records written before version 2 stay at format version 1 and are not
rewritten by reading them. They report `record_version: 0`, no tags or cover,
and creation and update times taken from the record file's modification time
with `created_estimated: true`. The first `update` or `cover` call rewrites
such a record as version 2 and keeps that estimated creation time.

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
| `"automatic"` | The first plate thumbnail of the newest sliced 3MF; otherwise a view of the newest STL; otherwise no cover |
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

## Library cards

```json
{"action": "card", "params": {"project_id": "desk"}}
```

A card is built from the project's current records each time it is requested.
Building one never contacts a printer backend or a model site. It reports:

| Field | Source |
| --- | --- |
| `name`, `description`, `tags`, `record_version` | The project record; descriptions longer than 4096 characters are cut and `description_truncated` is true |
| `dates.created_unix_ms`, `dates.created_estimated` | The project record |
| `dates.updated_unix_ms` | The newest of the record's last change and the modification times of the project's files |
| `dates.downloaded_unix_ms` | The earliest retrieval time in the project's `source.json` records |
| `dates.last_printed_unix_ms` | The recording time of the newest completed print |
| `cover` | The stored cover's path, media type and size, or null |
| `file_count`, `formats`, `has_3mf` | File names in the project directory, counted by format: `3mf`, `gcode.3mf` (sliced), `gcode`, `stl`, `step`, `scad`, `blend`, `obj`, `glb`, `gltf`, `ply`, `off`, `dxf`, `svg`, `py` (CadQuery sources) and `cadmodel` (CAD build models); `has_3mf` includes sliced 3MF files |
| `downloads` | Site, model ID, page URL, title, designer, license name and retrieval time from each `source.json` |
| `slices` | The number of completed slices and the newest one's printer, process, plate count, filament types and colours, and finish time |
| `prints` | Counts by outcome, completed prints per printer, the latest outcome and the last printed time |
| `printers`, `materials`, `colours` | Names from the print records and the newest slice, merged without regard to letter case; hexadecimal colours are shown as `#RRGGBB` |
| `unreadable` | Up to 16 records that could not be read, each with an error code; the rest of the card is still built |

Lists of printers, materials, colours, per-printer counts and slice filaments
hold at most 16 entries, and a `truncated` flag reports when more exist. A card
reads at most 16 `source.json` records (the first by path) and the 256 most
recently modified `state.json` files, which slices and CAD builds both keep;
only slice states count as slices. `downloads.truncated` and `slices.truncated`
report when more exist. Each `state.json`, `summary.json`, `source.json` and
print record must be at most 1 MiB. The newest slice's recorded `summary.json` is used. When the
newest slice predates summaries, the background index summarises its package
once and records `summary.json` beside it; until then a `card` request lists
that summary under `unreadable` rather than copying the package. Titles,
designers and file names come from sites and files, so treat them as untrusted
text.

## Search

```json
{"action": "search", "params": {"text": "drawer organiser",
  "filters": {"printed": true, "material": "PLA"}, "sort": "best_match", "limit": 20}}
```

All parameters are optional. Search reads a search index built from the
library cards; use `card` for one project's current details.

### Words

`text` holds at most 256 characters and 16 words. A word is a run of letters
and digits; everything else separates words and is never read as query syntax.
Every word must match the start of a word in the project's name, tags,
description, download sites, titles and designers, model and print file names,
materials or printers, ignoring letter case and accents. `organ` therefore finds
`organizer`. A match in the name ranks above a match in the tags, then the
description, then the download source, then the file names, materials and
printers. Only the first 256 model and print file names of a project, in path
order, are searched.

If no project matches every word, search looks for near matches in project
names instead. Each word of three or more characters is split into overlapping
three-letter pieces: `spirl` gives `spi`, `pir` and `irl`. A project is a near
match when its name contains at least half of all the pieces. Near matches are
marked `"match": "near"`, and their `score` is the share of pieces found. Word
matches are marked `"match": "words"`, and their `score` is a relevance value
where higher is better. Results without `text` have neither.

### Filters

Every filter given must match.

| Filter | Matches |
| --- | --- |
| `printed` | `true`: at least one completed print; `false`: none |
| `has_3mf` | `true`: the project has a 3MF file, sliced or not |
| `printer` | A printer name or model on the card |
| `material` | A filament type on the card, such as `PLA` |
| `colour` | A filament colour on the card; `#ff8800`, `FF8800` and `FF8800FF` are the same |
| `tag` | A project tag |
| `source` | A download site: `makerworld`, `printables` or `thingiverse` |
| `license` | A license name from a download record |
| `format` | A format from the card, such as `stl` or `gcode.3mf` |

Text filters are 1 to 128 characters and compare whole values without regard
to letter case. They use the
values shown on the card, so a project with more than 16 printers, materials or
colours can be filtered only by the first 16 in alphabetical order.

### Order and pages

| `sort` | Order |
| --- | --- |
| `best_match` | Highest score first; the default with `text`, and requires it |
| `newest` | Most recently created first; the default without `text` |
| `last_printed` | Most recently printed first; projects never printed come last |
| `name` | By name |

Ties are broken by name and then project ID, so pages are stable. `limit` is
1 to 50 (default 20) and `offset` is 0 to 1000. `total` counts every match.
`next_offset` is null on the last page and when the next page would start after
offset 1000; add words or filters to narrow a larger search.

### Coverage and limits

Every response lists unavailable search capabilities in `degraded`. Search
matches words only: searching by meaning (`meaning_search_unavailable`) and by
picture (`picture_search_unavailable`) are not available. `coverage` reports
how many of the workspace's projects are indexed and how many failed. When some
are missing, `degraded` also contains `index_incomplete`, and results cover the
indexed projects only.

## Search index

The search index is derived data. It holds nothing that cannot be rebuilt from
the project records, so it does not need backing up. An index written by a
different version of its layout is cleared and rebuilt.

| Setting | Default | Meaning |
| --- | --- | --- |
| `PRINTABLE_PROJECT_INDEX_DIR` | unset | An existing, writable, absolute directory on a local filesystem. The index is kept in `index.sqlite3` there across restarts. Unset keeps the index in memory and rebuilds it after each start. |
| `PRINTABLE_PROJECT_INDEX_INTERVAL_SECONDS` | `30` | Seconds between background passes, 0 to 86400. `0` turns background passes off. |

The directory must be on local disk, not a network share: the index uses
SQLite's write-ahead log, which needs shared memory that network filesystems do
not provide reliably. The server refuses to start when the directory is missing
or the log cannot be enabled. The server image contains an owner-only
`/var/lib/printable-index` directory for this purpose; a new named volume
mounted there takes its ownership, so it is writable by the server's user.

With background passes on, the first pass starts with the server and the next
ones follow at the interval.
Each pass lists the project records and compares a digest of each project's
record, file names, sizes and modification times, and print records with the
digest from the previous pass. Only changed projects have their cards rebuilt.
Projects are processed one at a time, and entries for deleted projects are
removed. Changes therefore appear in search within one interval. A project
whose record cannot be read keeps any earlier entry, is counted as failed, and
is tried again when its files change or on `index_retry`.

| Action | Effect |
| --- | --- |
| `index_status` | Storage kind (`file` or `memory`), indexed, pending and failed counts, up to 16 failures, the interval, and the latest pass since the server started |
| `index_rebuild` | Clears the index and rebuilds every card now, then reports the pass |
| `index_retry` | Updates changed projects and tries failed ones again now, then reports the pass |

Only one pass runs at a time; a rebuild or retry requested during a background
pass starts when that pass ends. To rebuild the index file offline, stop the
server, delete `index.sqlite3` and any `index.sqlite3-wal` and
`index.sqlite3-shm` files beside it, and start the server again.

## Storage and backups

The project record, its update lock and its covers live under the reserved
`.printable` directory and cannot be written as ordinary artifacts. Back up
`.printable` together with the `projects` directory.
