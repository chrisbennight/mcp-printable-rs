# Project collections

A collection groups related projects, like a folder. For example, every mount
designed for one machine can sit in a "Makera Z1 Multiboard" collection, and the
library page then shows one card for the collection instead of one card for
each mount. A project belongs to at most one collection, or to none. Collections
are one level deep: a collection holds projects, never other collections.

Collections are a label on top of projects. Grouping, moving, ungrouping or
deleting never moves, renames or deletes a project, its files, its revisions
or its library records. Projects keep their IDs and their `projects/<id>`
directories.

A [part family](part-families.md) is something else: it organizes variants of a
design inside one project. The project page shows family categories under
"Categories".

## Group existing projects

Create a collection and place projects in it in one request:

```json
{"action": "create", "params": {
  "collection_id": "makera-z1-multiboard",
  "name": "Makera Z1 Multiboard",
  "description": "Multiboard mounts for the Makera Z1",
  "project_ids": ["z1-drill-holder", "z1-bit-tray", "z1-hose-clip"]
}}
```

`collection_id` uses lowercase letters, digits, underscores and hyphens, up to
128 characters. Collection IDs and project IDs are separate, so a collection may
share an ID with a project. Names are 1 to 256 characters without control
characters; descriptions are optional, up to 4096 characters. Repeating the same
`create` returns the existing collection unchanged, so its `project_ids` apply
only when the collection is first created; the same ID with a different name or
description is refused. A workspace can have up to 1000 collections.

Add more projects later with `add_projects`, up to 500 per request:

```json
{"action": "add_projects", "params": {
  "collection_id": "makera-z1-multiboard",
  "project_ids": ["z1-cable-chain"]
}}
```

A request either applies completely or changes nothing. It is refused when a
project ID does not exist, or when a project is already in another collection.
Set `"move": true` to take such projects out of their current collection.
Projects already in the requested collection are accepted and left alone.

`remove_projects` makes projects ungrouped. Projects that are already ungrouped
are accepted; a project in a different collection is refused. `update` changes a
collection's name or description; its ID never changes.

`delete` refuses a collection that still has projects unless
`"ungroup_projects": true` is set, which leaves those projects ungrouped. Deleting
never deletes a project.

## Read collections

`list` returns one entry per collection, sorted by name: its ID, name,
description, project count, up to four sample projects by ID and name, and times. It also
reports how many projects are in no collection. `get` returns one collection and
its projects in project ID order, up to `limit` (100 by default).

The project tool reports each project's collection, or `null`, in `get`, `list`,
`create` and `update`. `project list` accepts `collection_id` to list one
collection's projects, or `"ungrouped": true` to list projects in no collection;
the filter applies before `limit`. `project create` accepts `collection_id` to
start a new project in an existing collection. An unknown collection, or a
project already in a different collection, is refused before the project is
created.

`catalog search` accepts a `collection` filter with a collection ID. Searches use
the current collections, so a project that was just moved is found in its new
collection without waiting for the search index.

## Changes from several clients

Every change returns the collections' `revision`, which increases with each
accepted change. `update`, `add_projects`, `remove_projects` and `delete`
accept the revision you read as `expected_revision`, and refuse the change when
someone else changed the collections in the meantime. `create` takes no
revision; repeating it is safe. Changes that
would not change anything keep the revision. When two changes arrive at once,
one is refused with a "being changed by another request" error; read the
collections again before retrying.

## Web page

On the [library web page](project-library.md#web-page), the page without a
search shows collections first, each as one card with up to four of its
projects' pictures and its project count. The project grid below them shows only
projects outside collections. A collection card opens the collection's page,
which lists its projects with the same sort order and pages as the library. A
project's page links to its collection.

Typing a search or choosing a filter looks through every project, in
collections or not, and shows no collection cards.

## Storage and backups

All collections and memberships are stored together in
`.printable/collections/index.json`, which is replaced as a whole with each
change. A project can therefore never be in two collections, and a change that
moves several projects or deletes a collection cannot be half applied. Back it
up with the rest of `.printable`. A workspace without the file has no
collections, and every project is ungrouped.

If the file is damaged, or names a collection that does not exist, the server
reports an error rather than guessing. Restore it from a backup, or remove it to
start again with every project ungrouped. If a project's record is removed by
hand while it is in a collection, `get` marks it `missing`, and `remove_projects`
takes it out of the collection. A member whose record cannot be read is listed
with a null name.

Public request and response schemas are available at
`printable://contracts/collection/create`, `printable://contracts/collection/get`,
`printable://contracts/collection/list`, `printable://contracts/collection/update`,
`printable://contracts/collection/add_projects`,
`printable://contracts/collection/remove_projects` and
`printable://contracts/collection/delete`.
