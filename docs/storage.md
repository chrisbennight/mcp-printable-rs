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
configuration per workspace and a local filesystem supporting advisory locks.

Managed artifact writes check capacity under a shared filesystem lock, held
through the atomic copy and commit. Concurrent processes cannot spend that
capacity independently. When no budget is configured, long copies are not
serialized by the budget lock. Managed temporary directories reserve capacity
before work begins and retain a live ownership lock until their last owner
finishes. This covers workspace snapshots (including publication and printer
inputs), CAD and slicer staging, OpenSCAD staging, chunked and native uploads,
and video encoding. Upload copying retains ownership even when its awaiting
request disappears.

Reservations are conservative: snapshots use their observed source size;
uploads use their declared size or transfer limit; native staging uses fixed
estimates. CAD reserves five GiB plus its log allowance, and slicing reserves
three GiB plus its log allowance. These estimates do not bound arbitrary native
writes or every large multi-plate slice. Final retained copies need
additional room while their temporary sources still exist. A small budget can
therefore reject a small model before its exact output size is known. The
`storage_budget_exceeded` error is an admission result, not a claim that the
filesystem is already full. Native upload HTTP requests return 507 when their
reservation cannot be admitted.

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
bytes when no size was measured. Preview and deletion are bounded to 1,000
entries per operation; larger or damaged temporary inventories require
operator inspection with services stopped.

Cleanup only traverses managed temporary directories and never follows symbolic
links. All public artifacts and retained internal data stay protected, including
active job checkpoints, revision snapshots, transfer source files, and delivery
records. These conservative rules can leave substantial old render output on
disk. There is no age-based whole-workspace collector. Follow the
[operations guide](operations.md#storage-and-cleanup) before removing retained
data or externally referenced Blender assets.

Accounting scans at most 100,000 entries and 64 directory levels. Inaccessible,
changing, or unsupported names can make the scan incomplete; a configured
budget then refuses admission. Symlinks are not followed. Hard links count per
name. Logical sizes exclude filesystem metadata, allocation overhead, storage
outside the workspace, backups, and small unmanaged geometry-worker temporary
files. Live native writes can change observed usage during a scan or exceed a
reservation. Authorized Python and Blender can write directly, so this is a
service admission policy, not a security quota against a hostile OS principal.
Use filesystem/container quotas and monitor free blocks and inodes separately.

The isolated workspace tests cover competing reservations, lost-owner recovery,
disk-pressure rejection and recovery, retained-file preservation, a new owner
after preview, failed deletion, and symlink confinement. They do not operate on
production workspaces.
