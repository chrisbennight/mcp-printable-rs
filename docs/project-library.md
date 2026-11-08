# Project library records

Each project keeps a record that a library card can show: its name and
description, tags, when it was created and last changed, and a cover picture.
Slices add a summary of what they were sliced for, and print records keep how
each print ended. The read-only `catalog` tool reads the library: `card`
gathers these records into one library card, `search` finds projects by their
cards, and `status` reports the search index. The `project` tool changes the
records with `update` and `cover`, and updates the index with `reindex`. An
optional read-only [web page](#web-page) shows the cards to people. Read
`printable://contracts/project/update`, `printable://contracts/project/cover`,
`printable://contracts/catalog/card` and `printable://contracts/catalog/search`
for the typed requests and results.

`catalog` finds only the user's own Printable projects. Use the
[model library](model-library.md) to find designs on MakerWorld, Printables or
Thingiverse.

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

Call `catalog` with:

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
| `slices` | The number of completed slices and the newest one's printer, process, plate count, plates with a thumbnail picture, filament types and colours, and finish time |
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

Call `catalog` with:

```json
{"action": "search", "params": {"text": "drawer organiser",
  "filters": {"printed": true, "material": "PLA"}, "sort": "best_match", "limit": 20}}
```

All parameters are optional. Search reads a search index built from the
library cards; use `card` for one project's current details. With an
embeddings service configured, `best_match` also finds projects by what the
text means and by a picture; see
[meaning and picture search](#meaning-and-picture-search).

### Concise and full results

Results are concise unless `detail` is `full`. A concise result holds
`project_id`, `name`, the first 200 characters of `description`, `printed`
(the number of completed prints), `created` and `last_printed` as UTC dates
such as `2026-06-03`, `formats` and `tags`, and `match` and `score` when the
search has `text` or a `picture`. Empty lists and absent values are left out:

```json
{"project_id": "vase", "name": "Spiral vase", "printed": 2,
  "last_printed": "2026-06-03", "created": "2026-05-28",
  "formats": ["stl", "gcode.3mf"], "match": "words", "score": 7.25}
```

With `"detail": "full"`, each result is the stored short card: it adds
`printers`, `materials`, `colours`, `last_outcome`, `cover_path`, the `dates`
object with every timestamp in Unix milliseconds, and `scores` with each
search method's own score when several took part. Both forms share `total`,
`next_offset`, `degraded` and `coverage`.

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
where higher is better. Results without `text` or `picture` have neither.

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
| `best_match` | Highest score first; the default with `text` or `picture`, and requires one of them |
| `newest` | Most recently created first; the default without `text` |
| `last_printed` | Most recently printed first; projects never printed come last |
| `name` | By name |

Ties are broken by name and then project ID, so pages are stable. `limit` is
1 to 50 (default 20) and `offset` is 0 to 1000. `total` counts every match.
`next_offset` is null on the last page and when the next page would start after
offset 1000; add words or filters to narrow a larger search. A search with
`picture` is always ordered by `best_match`; the other orders use word matching
only.

### Coverage and limits

`degraded` lists the search methods this request would use that are
unavailable or limited, each with a `code`, a `message` and a `reason`.
Meaning search applies to `best_match` with `text`, picture search to a
request with a `picture`, and re-ranking to `best_match` with `text` on a page
that starts before offset 40; a note about a method the request would not use
is left out. `catalog status` reports every method.

| Code | Meaning |
| --- | --- |
| `meaning_search_unavailable` | Searching by meaning is unavailable |
| `picture_search_unavailable` | Searching by picture is unavailable |
| `rerank_unavailable` | Re-ranking is unavailable or failed for this search; results keep their combined order |

| `reason` | Why |
| --- | --- |
| `not_configured` | The service's settings are not set |
| `turned_off` | `PRINTABLE_EMBEDDING_IMAGES` is off |
| `pictures_rejected` | The embeddings model refused pictures |
| `not_ready` | No vectors from the configured model exist yet, or the service now reports a different model |
| `unreachable` | The service could not be reached or failed |
| `busy` | The service answered `429` or `503` |
| `refused` | The service refused the request; a setting is probably wrong |
| `incompatible` | The service's answer could not be used |

Without an embeddings service, search matches words only: a `best_match`
search with text lists `meaning_search_unavailable` and, without a re-ranking
service, `rerank_unavailable`, with reason `not_configured`, and results are
the same as with no services at all. `coverage` reports how many of the
workspace's projects are indexed and how many failed. When some are missing, `degraded`
also contains `index_incomplete`, and results cover the indexed projects only.
When a search uses meaning or picture search while some projects still lack
vectors, `degraded` contains `meaning_index_incomplete` or
`picture_index_incomplete`, and those projects are found by words only.

## Meaning and picture search

Meaning search finds projects whose library card means something close to the
search text, even when they share no words with it. Picture search finds
projects whose picture looks like a query picture. Both need an embeddings
service, and re-ranking needs a re-ranking service. All three are optional and
off unless configured.

```json
{"action": "search", "params": {"text": "something to hold flowers",
  "combine": "standardised", "limit": 10}}
{"action": "search", "params": {"picture": "photos/broken-part.jpg"}}
```

### Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `PRINTABLE_EMBEDDING_URL` | unset | Base URL of an OpenAI-compatible embeddings API; requests go to `<base>/embeddings`, so `https://host/v1` works. HTTPS, or plain HTTP only to a private or loopback address or a single-label host name such as `http://embedder:8080/v1`. No credentials, query or fragment. Unset turns meaning and picture search off. |
| `PRINTABLE_EMBEDDING_MODEL` | none | The model name to request; required with the URL |
| `PRINTABLE_EMBEDDING_API_KEY` | unset | Sent as `Authorization: Bearer <key>` |
| `PRINTABLE_EMBEDDING_API_KEY_FILE` | unset | A file holding the key instead, such as a mounted secret; set this or the key, not both |
| `PRINTABLE_EMBEDDING_DIMENSIONS` | unset | Vector length to request, 1 to 8192, for models that can shorten their vectors |
| `PRINTABLE_EMBEDDING_QUERY_PREFIX` | empty | Text put before search text, kept exactly as set, such as `query: ` for E5 models |
| `PRINTABLE_EMBEDDING_DOCUMENT_PREFIX` | empty | Text put before project text, such as `passage: ` |
| `PRINTABLE_EMBEDDING_REVISION` | unset | A label of your choice; changing it embeds every project again |
| `PRINTABLE_EMBEDDING_IMAGES` | `off` | `on` sends pictures for picture search |
| `PRINTABLE_RERANK_URL` | unset | Base URL of a re-ranking API; requests go to `<base>/rerank`. The same address rules apply. |
| `PRINTABLE_RERANK_MODEL` | none | The re-ranking model; required with its URL |
| `PRINTABLE_RERANK_API_KEY` | unset | Sent as `Authorization: Bearer <key>` |
| `PRINTABLE_RERANK_API_KEY_FILE` | unset | A file holding the re-ranking key instead; set this or the key, not both |

The other embedding settings take effect only with `PRINTABLE_EMBEDDING_URL`,
and the re-ranking ones only with `PRINTABLE_RERANK_URL`. The server refuses
to start when a set value is invalid; the error names the setting, never its
value. Supply API keys through your deployment's secret mechanism. They are
sent only in the `Authorization` header and never appear in responses,
status, errors or logs. Requests are not redirected and do not use a proxy.

### What the services receive

Card text and pictures are sent to the configured services and nowhere else.
For each project the embeddings service receives the document prefix and the
project's name, tags, description, download sites, titles and designers, file
names, materials and printers, one per line, at most 2048 characters. Searches
send the query prefix and the search text. A re-ranking request contains the
search text and the same project text for the leading results.

Embeddings requests use OpenAI's request shape, with at most 32 inputs each:

```json
{"model": "nomic-embed-text", "input": ["passage: Desk organizer\n..."],
  "encoding_format": "float", "dimensions": 768}
```

`dimensions` is sent only when set. The answer's `data[].embedding` vectors
are used in `index` order, and its `model` names the model. Answers are read
up to 8 MiB and vectors up to 8192 numbers long; any other answer counts as
unusable.

Picture input is not part of the OpenAI API: it is an extension that some
multimodal servers accept, and it is off by default. With
`PRINTABLE_EMBEDDING_IMAGES=on`, each picture is decoded, reduced to at most
1024 pixels on its long side, encoded as PNG and sent as one input:

```json
{"model": "...", "input": [[{"type": "image_url",
  "image_url": {"url": "data:image/png;base64,..."}}]], "encoding_format": "float"}
```

A project's picture is its cover, or else the first plate thumbnail in its
newest slice's package. A search `picture` is a PNG, JPEG or WebP file in the
workspace of at most 16 MiB and 8192 pixels on a side; upload one with the
workspace file tools first. If the model refuses pictures, picture search is
reported unavailable with reason `pictures_rejected`, and no more pictures are
sent until the embedding settings or the model change, or until a pass runs
with the setting off.

Re-ranking requests use the shape that Cohere, Jina, vLLM and Text Embeddings
Inference share:

```json
{"model": "bge-reranker-v2-m3", "query": "drawer organiser",
  "documents": ["Desk organizer\n...", "..."], "top_n": 2}
```

The answer's `results[].index` and `relevance_score` reorder the documents.

### How results are combined

`best_match` gathers the projects that match the words (or, when none do, the
near matches), the 50 projects closest in meaning to the text, and the 50
projects whose pictures are closest to the query picture. Meaning and picture
scores are the cosine similarity of vectors, computed exactly against every
current vector after the filters. When more than 2000 projects match the
words, the 2000 best word matches take part; `total` still counts every match.

When only one method finds anything, results keep that method's own score, so
word search alone ranks exactly as it does without the services. Otherwise
`combine` sets how the methods' scores are joined:

| `combine` | Score |
| --- | --- |
| `standardised` | The default. Each method's scores are standardised across the results (the score minus the mean, divided by the standard deviation) and added. A project with no word match counts as word score 0; one without a vector counts as that method's lowest score. |
| `reciprocal_rank` | The sum of 1 / (60 + rank) over the methods that found the project, ranks starting at 1 |

Ties are broken by name and then project ID, so the same request gives the
same order. Each result's `match` is `words` or `near` when the words found it,
otherwise `meaning` or `picture`. Its `score` is the combined score, and
`scores` holds each method's own score: `words`, `meaning`, `picture` and
`rerank`.

With a re-ranking service, a `best_match` search with text sends the leading
40 results to it and puts them in its order, highest relevance first; later
results keep their combined order, and a page that starts at offset 40 or
later does not ask the service. If re-ranking fails, results keep their
combined order and `degraded` contains `rerank_unavailable`.

### Model identity and re-embedding

Vectors are kept in the search index with the identity of the model that made
them: the service address and the configured model, dimensions, revision label
and document prefix; the model name the service reports; the vector length;
and the vector of a fixed probe text. Each background pass first embeds the probe text once. When
any part of the identity differs, or the probe's vector has moved by more than
a cosine similarity of 0.001, every stored vector is discarded and each
project is embedded again. Until then, meaning and picture search report
`not_ready` rather than compare vectors from different models. Change
`PRINTABLE_EMBEDDING_REVISION` to embed everything again after replacing a
model behind an unchanged name.

Embedding runs in each background pass after the cards are updated, and in
`project reindex`. A pass embeds at most 256 project texts in
requests of up to 32, then at most 32 pictures in requests of up to 4, and
starts no request after two minutes; later passes continue. A project whose
text or picture changes is embedded again. Requests time out after 60
seconds. When the service answers `429` or `503`, passes, including
`project reindex`, make no embedding requests until the time in
its `Retry-After` header, at most an hour (a minute without one). Searches
still try, and a busy answer to a search holds back passes in the same way. A refusal with `401`, `403`, `404` or `405`, an unreachable
service and other server errors stop the pass and are reported by
`catalog status`. Other refusals and unusable answers count against the projects
in that request: each is tried again alone after 5 minutes, then 10, doubling
up to a day, or at once on `project reindex`.

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
is tried again when its files change or on `project reindex`. A project whose
newest slice summary cannot be read or recorded is searchable, but it is also
counted as failed so that `project reindex` tries the summary again.

| Request | Effect |
| --- | --- |
| `catalog` `status` | Storage kind (`file` or `memory`), indexed, pending and failed counts, up to 16 failures, the interval, the latest pass since the server started, which of meaning search, picture search and re-ranking are available (`methods`) and why not (`unavailable`), and under `embeddings` the model identity with embedded, pending and failed vector counts and up to 16 vector failures |
| `project` `reindex` with `"mode": "retry"` (the default) | Updates changed projects and tries failed cards and vectors again now, then reports the pass |
| `project` `reindex` with `"mode": "rebuild"` | Clears the index, including its vectors, and rebuilds every card now, then embeds what a pass allows and reports the pass |

```json
{"action": "reindex", "params": {"mode": "rebuild"}}
```

Only one pass runs at a time; a rebuild or retry requested during a background
pass starts when that pass ends. To rebuild the index file offline, stop the
server, delete `index.sqlite3` and any `index.sqlite3-wal` and
`index.sqlite3-shm` files beside it, and start the server again.

## Web page

People can browse and search the library in a read-only web page. It is off
unless `PRINTABLE_WEB_LISTEN` is set, and it has a listener of its own: it
never shares a port or any routes with the MCP endpoint.

The library page shows a grid of library cards with a search box, a sort order
and the [search filters](#filters). Each card shows the cover picture, whether
and how often the project was printed, its file formats, its name, the start of
its description, filament colours and materials, printers, tags, and when it
was created and last printed. A page holds 24 cards, and like `catalog search` the
pages reach only the first 1008 results; add words or filters to narrow a
larger search. The coverage and every unavailable search capability are shown
above the grid.

A project page shows the full card: the cover, description, tags and dates,
the newest slice, print outcomes per printer, model-site downloads, and up to
500 of the project's files with download links. The page builds the card from
recorded slice summaries only, as `catalog card` does. Download pages from model sites
are shown as plain links, and no page loads anything from another site. Dates
are shown in UTC.

The page cannot change anything. It answers only `GET` and `HEAD` requests and
has no editing and no public, guest or sharing links.

### Downloads from the page

A file link downloads one file from the project's directory. Paths with `.` or
`..` components, encoded slashes or backslashes, symbolic links, unsupported
file types and Printable's reserved records are refused. The server first
copies the file into the workspace's temporary storage, so the bytes cannot
change during the download, and then streams the copy. The copy counts against
`PRINTABLE_WORKSPACE_BUDGET_MIB` until the download ends. At most four
downloads run at once; another is refused with status 503 until one ends.
Files are sent as attachments with the media type `application/octet-stream`.

### Web page settings

| Setting | Meaning |
| --- | --- |
| `PRINTABLE_WEB_LISTEN` | Socket address of the page, such as `0.0.0.0:8080`. Unset turns the page off. It needs `PRINTABLE_WORKSPACE_ROOT` and a port different from the MCP port. |
| `PRINTABLE_WEB_TRUSTED_PROXIES` | Required with the listener. Comma-separated IP addresses or CIDR ranges of the reverse proxy that signs people in, such as `172.30.0.2` or `172.30.0.0/24`. |
| `PRINTABLE_WEB_USER_HEADER` | Required with the listener. The request header in which the proxy names the signed-in user, such as `X-Printable-User`. |
| `PRINTABLE_WEB_USERS` | Required with the listener. Comma-separated names of the users allowed to read the library. A name must equal the header value exactly, including letter case. |

The server refuses to start when the listener is set and one of the other
settings is missing or malformed. A range must not cover every address or set
bits after its prefix, and IPv4 addresses are written in IPv4 form. Without
`PRINTABLE_WEB_LISTEN` the other three settings are not read.

### Security model

Printable does not sign people in; it relies on the reverse proxy that does.
Each request is checked in this order:

1. The TCP peer address must be in `PRINTABLE_WEB_TRUSTED_PROXIES`. Any other
   peer is refused with status 403 before anything else is read.
   Forwarded-address headers are ignored.
2. The user header must appear exactly once and name a listed user. Otherwise
   the request is refused with status 403. Only the refusal is logged, never
   the header's value.
3. Methods other than `GET` and `HEAD` are refused with status 405.

Every response carries a content security policy that allows styles and
pictures only from the page itself and permits no scripts, no framing by other
sites and no form submissions elsewhere, together with
`X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. Pages and
downloads are sent with `Cache-Control: no-store`. Cover pictures may be kept by
the browser and are checked again on each view. Names, descriptions, tags,
site text and file names are escaped and shown as text.

Warning: anything that can connect to the listener from a trusted address can
claim to be any user. Configure the proxy to set the user header on every
request it forwards, replacing any value the browser sent. Put the listener on
a network that only the proxy can reach, bind it to the server's address on
that network rather than to every interface where possible, and do not publish
its port. If another service shares a trusted address or range, that service
can read the library as any listed user.

### Reverse proxy example

This nginx example asks a sign-in service whether each request is signed in and
passes the user name it returns to Printable. The address of the check, the
response header that carries the user name and the handling of people who are
not signed in depend on the sign-in service; see its documentation for nginx
`auth_request`.

```nginx
server {
    listen 443 ssl;
    server_name library.example.com;
    # Certificate settings omitted.

    location = /signed-in {
        internal;
        proxy_pass http://sign-in:9000/verify;
        proxy_pass_request_body off;
        proxy_set_header Content-Length "";
        proxy_set_header X-Original-URI $request_uri;
    }

    location / {
        auth_request /signed-in;
        auth_request_set $printable_user $upstream_http_x_user;
        # Replaces any X-Printable-User header the browser sent.
        proxy_set_header X-Printable-User $printable_user;
        proxy_pass http://server:8080;
    }
}
```

When the variable is empty, nginx sends no user header and Printable refuses
the request. With Compose, a network that only the server and the proxy join
keeps other services away from the listener:

```yaml
services:
  server:
    networks:
      control: {}
      frontend: {}
      library:
        ipv4_address: 172.30.0.3
    environment:
      PRINTABLE_WEB_LISTEN: 172.30.0.3:8080
      PRINTABLE_WEB_TRUSTED_PROXIES: 172.30.0.2
      PRINTABLE_WEB_USER_HEADER: X-Printable-User
      PRINTABLE_WEB_USERS: alice,bob
  proxy:
    networks:
      library:
        ipv4_address: 172.30.0.2

networks:
  library:
    ipam:
      config:
        - subnet: 172.30.0.0/24
```

The proxy's own published HTTPS port is how people reach the page; the
server's port 8080 is not published.

## Storage and backups

The project record, its update lock and its covers live under the reserved
`.printable` directory and cannot be written as ordinary artifacts. Back up
`.printable` together with the `projects` directory.
