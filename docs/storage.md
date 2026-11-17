# Storage accounting and cleanup

Call `artifact` with `{"action":"usage","params":{}}` to inspect logical file
bytes, the configured budget, unused live reservations, and totals by project
and artifact class. `charged_bytes` adds logical bytes and unused reservations;
the same bytes in a managed temporary directory are not counted twice.
Project grouping is capped at 1,000 named groups, with further groups combined
as `_other_projects` and `project_groups_truncated: true`.

Set `PRINTABLE_WORKSPACE_BUDGET_MIB` to a positive integer on the server. The
provided Compose file forwards this setting. The server writes the policy into
the shared workspace on startup, so CAD and slicer workers observe the same
limit without separate settings. An unset value disables the service limit.
Lowering it does not delete files or stop already admitted work. Use one server
configuration per workspace on a filesystem whose advisory locks are shared by
every process that uses it: a local filesystem, or an NFS version 4.1 mount with
locking enabled (do not mount it with `nolock`).

Managed artifact writes check capacity under a shared filesystem lock, held
through the atomic copy and commit. Concurrent processes cannot spend that
capacity independently. When no budget is configured, long copies are not
serialized by the budget lock. Managed temporary directories reserve capacity
before work begins and retain a live ownership lock until their last owner
finishes. This covers workspace snapshots (including publication and printer
inputs), CAD and slicer staging, OpenSCAD staging, chunked and native uploads,
and video encoding. Upload copying retains ownership even when its awaiting
request disappears.

Native CAD, slicer, OpenSCAD, and video-encoding processes inherit their staging
leases. If a Rust worker exits while its native child survives, that child keeps
the reservation and cleanup protection until it exits. Native programs must not
close inherited lease descriptors while they still use managed storage.

Snapshots and native input staging reserve their observed source size. Native
uploads reserve their declared size; chunked uploads check capacity under the
shared budget lock for each append. Native output sizes are determined by the
backend. Final retained copies need additional room while their temporary
sources still exist. The
`storage_budget_exceeded` error is an admission result, not a claim that the
filesystem is already full. Native upload HTTP requests return 507 when their
reservation cannot be admitted.

Incoming-file staging also has a configurable count limit:
`PRINTABLE_FILE_UPLOAD_MAX_STAGED` defaults to 20. Each upload streams to managed
disk storage and reserves its declared size, or its configured maximum when
the size is unknown. A count limit does not reserve disk by itself; configure
`PRINTABLE_WORKSPACE_BUDGET_MIB` for the workspace's shared byte budget. Final
publication can temporarily require both the staged source and retained copy.
Read `status.workspace.file_upload` for the native incoming-file limit,
available staging slots and current receivers. The legacy
`workspace.max_transfer_bytes` value applies to inline artifact I/O.

Cancel an abandoned incoming delivery through the gateway's delivery lifecycle.
Completed cancellation releases its staging; cancelling a receive waits until
its admitted write has ended. Printable tracks only unfinished uploads and
publication or cleanup still using resources. Completed work leaves no upload
receipt or history entry and does not consume admission capacity. Recover an
uncertain ingest by inspecting the current destination artifact and comparing
its size and SHA-256 with the source. Missing upload status does not prove that
publication failed. Gateway storage, gateway active transfers, and Printable
staging have separate lifetimes and limits; a gateway upload
being ready does not mean its destination has been ingested. See
[incoming-file recovery](workflow-interface.md#recover-or-cancel-a-gateway-delivery).

After a crash, a directory without a live ownership lock no longer reserves
unused capacity. Its actual bytes remain charged until removed. Every managed
temporary directory is disposable when unowned; it never serves as a retained
revision, checkpoint, job result, or delivery record. Normal owner release
attempts cleanup. Failure leaves the directory available for reconciliation.

Call `artifact` with `{"action":"cleanup_preview","params":{}}`. Each entry
contains an identifier, observed bytes, and a reason for `protected` or
`pending`. To remove selected pending entries, use
`{"action":"cleanup","params":{"ids":["<identifier from preview>"]}}`.
Identifiers are validated before deletion. A preview grants no deletion right:
cleanup acquires the shared controller lock and checks live ownership again.
A new owner makes the entry protected. `deleted` means absence was observed or
directory removal and parent synchronization succeeded. Failure or uncertain
durability remains `pending`; retry to reconcile. Deletion results use null
bytes when no size was measured. Preview and deletion have no fixed entry-count
ceiling. Invalid identifiers and actual filesystem errors remain errors.

Cleanup only traverses managed temporary directories and never follows symbolic
links. All public artifacts and retained internal data stay protected, including
active job checkpoints, revision snapshots, transfer source files, delivery
records, project records and cover pictures. Back up the whole `.printable`
directory with the project files; the [project library](project-library.md)
describes the records it holds. These conservative rules can leave substantial old render output on
disk. There is no age-based whole-workspace collector. Follow the
[operations guide](operations.md#storage-and-cleanup) before removing retained
data or externally referenced Blender assets.

Accounting, inspection, and cleanup have no fixed entry-count or directory-depth
ceiling. `scan_limit` in usage and `max_list_scan_entries` in workspace status
are null. Inaccessible, changing, or unsupported names can make the scan
incomplete; a configured budget then refuses admission. Symlinks are not
followed. Hard links count per
name. Logical sizes exclude filesystem metadata, allocation overhead, storage
outside the workspace, backups, and small unmanaged geometry-worker temporary
files. Live native writes can change observed usage during a scan or exceed a
reservation. Authorized Python and Blender can write directly, so this is a
service admission policy, not a security quota against a hostile OS principal.
Use filesystem/container quotas and monitor free blocks and inodes separately.

The isolated workspace tests cover competing reservations, lost-owner recovery,
disk-pressure rejection and recovery, retained-file preservation, a new owner
after preview, large inventories, deep cleanup, and symlink confinement. They
do not operate on production workspaces.
