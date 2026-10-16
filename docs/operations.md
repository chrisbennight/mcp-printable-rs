# Operate and recover Printable

Keep the matching image set, Compose configuration, workspace backup, and credential
recovery process together in your operational records. Store the bearer
separately from model archives. Record exact image digests and the source
revision; a mutable tag is not a rollback record.

## Check health and active work

`/healthz` reports process liveness. `/readyz` and the MCP `status` tool report
dependencies, render-worker availability, and recovery integrity. A busy worker
is distinct from an unavailable worker. Live modeling can remain available
while the background worker is occupied.

CAD and slicer checks distinguish `not_configured`, `ready`, `busy`,
`unavailable`, and `incompatible`. An omitted optional endpoint does not block
overall readiness. A configured worker that cannot answer the current readiness
protocol blocks the aggregate probe, while each other capability keeps its own
state. The MCP `status` result also includes the native engine version and the
slicer's loaded profile counts. Blender status reports `busy` immediately when
its healthy operation lane is occupied, without waiting for that work to end.

Native workers check their installed engine during startup in a private
temporary directory: CAD imports its build modules and reports the CadQuery
version; the slicer checks its supported command-line interface. These checks
do not build or slice a model. Worker `/readyz` then checks workspace readiness
and admission occupancy without repeating native startup. Restart the worker
after changing its installation. `/healthz` remains process liveness; the
container health checks use worker `/readyz`. Readiness is an observation, not
a reservation or a guarantee that the next job will succeed.

Use `job` actions `list` and `get` to inspect work before maintenance. Cancel
unneeded jobs explicitly, then wait for their recorded terminal state. Running
cancellation is cooperative at a frame boundary. A lost connection or timeout
does not prove that a scene mutation failed; inspect state before retrying.
CAD builds have their own retained handles and are not render jobs in `job.list`.
Use `cad_build.status` or `cad_build.cancel` with the original project and output
directory. Disconnecting a waiting client does not cancel admitted work. Keep
output directories, including `state.json`, `report.json`, and `failure.json`;
after a worker restart, unfinished state is reported as interrupted. Stop the
CAD worker with the other services before backup.

## Stop services

The server and CAD, reconstruction, and slicer workers handle both SIGTERM
(the usual container-stop signal) and SIGINT. They stop accepting new HTTP
connections and drain accepted HTTP requests. A synchronous CAD build,
reconstruction, depth or object request can finish during this drain. The MCP server also
cancels its transport sessions and background readiness checks; this does not
prove that a Blender mutation already delivered to the separate bridge was
undone. Inspect that scene after restart before deciding whether to retry.

Background CAD builds and slicing return admission records before execution
finishes. They are not held open by an HTTP request, so an idle HTTP drain can
end while they are still running. Runtime teardown stops their owned native
children; their retained state can remain unfinished. After restart,
`cad_build.status` and `slice.status` report this state as `interrupted` rather
than replaying the operation or treating process exit as successful work.
Completed reports and artifacts remain available. Check retained outputs before
submitting work again, using a new output directory for a deliberate revision.

Choose the container grace period to cover the synchronous work you intend to
drain. The application does not add a separate fixed shutdown deadline. For
example, with the same Compose project and configuration used at startup:

```sh
docker compose stop --timeout 60
```

The timeout is an operator-selected example, not a completion guarantee.
Configure `stop_grace_period` in a local Compose override when a different
default is needed. Cancel or wait for background jobs before planned
maintenance. Save the live Blender checkpoint first and stop its separate
services as well before backing up shared storage.

If the grace period expires, Docker can force termination with SIGKILL.
SIGKILL cannot run application cleanup, and killing only a service process on
the host does not guarantee its native children stop. Use the enclosing
container's lifecycle controls and confirm all affected containers have
stopped before touching shared files. Record the requested signal, configured
grace period, observed exit status or forced termination, original work
handles, and post-restart state in operational evidence. Exit status zero means
the service shut down gracefully; only the retained work result establishes
whether an operation completed. An unfinished record by itself does not
identify the stop signal or certify that no partial output exists.

## Back up

Save a Blender checkpoint before stopping if the live scene matters. Drain or
cancel active work and stop all application services using the same Compose
project and configuration that started them. Confirm they are stopped before
copying the workspace; a live filesystem copy can mix metadata and artifacts
from different points in a job.

With the supplied default project name, the volume is `printable_workspace`.
Confirm its actual name with `docker volume ls --filter label=com.docker.compose.project=printable`.
The following example archives the stopped volume through an image from your
recorded image set. Set `server_image` to its immutable server reference first:

```sh
set -eu
umask 077
backup_dir=$(mktemp -d ./printable-backup.XXXXXXXX)
docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --user 10001:10001 \
  --mount type=volume,source=printable_workspace,target=/workspace,readonly \
  --entrypoint /bin/tar "$server_image" -C /workspace -cf - . \
  > "$backup_dir/workspace.tar"
sha256sum "$backup_dir/workspace.tar" > "$backup_dir/workspace.sha256"
```

Check the command's exit status before treating the archive as complete. Keep
the whole workspace, including `.printable`, its job index, job records, frame
files, immutable source snapshots, and render-isolation metadata. Retain external
assets referenced by checkpoints as well: not every Blender dependency can be
packed into a `.blend` file. Protect backups containing private models.

## Restore and roll back

Restore into a new dedicated volume first, using the recorded image set.
Initialize its root ownership to UID/GID 10001 as the supplied Compose setup
does, then extract the trusted archive as UID 10001 with no network and only
that volume writable. Keep the existing volume unchanged until recovery is
verified. Point a separate Compose project at the restored volume, use a
different loopback port, and confirm dependency readiness and retained jobs.
Restore a saved live-scene checkpoint explicitly and verify a representative
model and artifact download.

An image rollback and a data rollback are related operations. Current isolated
jobs use metadata version 3. Do not run an older binary that cannot read it over
new metadata. Do not overlay legacy jobs onto an established isolation record.
Use a coordinated older backup with its matching images when that is the only
supported rollback path. The [render-worker guide](render-worker.md#migration)
describes the legacy restoration fence.

Unreadable job metadata must remain visible as a recovery problem. Preserve a
copy before investigating; do not delete the index or isolation record to make
readiness appear green. In the isolated runtime, malformed job history blocks
rendering without pretending that live modeling failed. Legacy history may
retain a live-scene restoration obligation and therefore a mutation fence.

## Storage and cleanup

Use `artifact.usage` to inspect logical workspace bytes by project and artifact
class, live temporary reservations, and accounting completeness. Configure
`PRINTABLE_WORKSPACE_BUDGET_MIB` on the server to enable aggregate service
admission limits. The server retains that policy for workers sharing its
workspace; an unset setting disables the limit at server startup. Read
[storage accounting and cleanup](storage.md) for coverage and recovery.

Docker's default named volume can still fill its host filesystem. Native
Python, direct Blender writes, filesystem overhead, and other processes are
outside service enforcement. Use a dedicated filesystem or an operator-enforced
quota, monitor available bytes and inodes, and leave room for backups.

Job history retains the newest admitted records up to its configured source
limit, evicting terminal records first. Eviction does not delete their old
frame and video files. Download grants expire automatically, but that expiry
does not delete the original workspace artifact. Plan retention explicitly.

`artifact.cleanup_preview` identifies abandoned managed temporary directories;
`artifact.cleanup` rechecks ownership before deleting the selected identifiers.
Live temporary inputs and all retained files remain protected. Failed removal
stays pending until a later attempt confirms deletion. There is no automatic
whole-workspace garbage collector. For manual cleanup of retained artifacts,
stop all services and confirm active transfers and jobs are no longer running.
Back up first. Remove only artifacts you have identified as no longer needed,
including by checkpoints with external dependencies. Keep `.printable` intact
unless performing a coordinated full data restore. Do not use a broad age-based
delete command against a running workspace or delete a volume to fix disk
pressure. Restart and verify readiness and representative downloads afterward.

## Verification evidence and limits

The Linux [shutdown regression tests](../crates/printable-server/tests/shutdown_test.rs)
launch the real entrypoints with disposable workspaces and fake native engines.
They cover idle SIGTERM and SIGINT, accepted synchronous request drain, closed
new connections, completed CAD state after restart, and interrupted CAD/slicer
state after normal or forced stops without automatic replay. Forced host-process
tests explicitly clean up their identified fake children; they do not prove
Docker's container-wide process cleanup or real GPU work completion. These
tests are excluded on non-Linux hosts.

The [Docker shutdown evidence](../evidence/service-shutdown.md) records isolated
baseline and candidate container stops, accepted CAD request drain, background
interruption, forced termination, and retained results after restart. The
Linux/arm64 experiment uses fake engines and development binaries; it does not
establish Linux/amd64 release-image or NVIDIA workload performance.

The [job regression tests](../crates/printable-server/src/jobs.rs) cover isolated
recovery without live-scene replacement, next-frame restart, cancellation,
legacy restoration, unreadable history, and serialized metadata writes. The
installation integration additionally checks a completed job after server
restart. Its recovery option restores a real workspace into a fresh volume,
verifies the video digest and CAD reports and artifact digests, isolates damaged metadata, and exercises a full
dedicated filesystem followed by a successful write after space is freed.
See [the recorded recovery evidence](../evidence/installation-recovery.md).
These are controlled software-container checks, not a claim about arbitrary
host corruption or NVIDIA performance.

Record backup/restore and disk-pressure trials on an isolated volume before
depending on an installation. Never perform them on a live user's workspace.
Record host RAM, GPU model/driver, VRAM use, storage, workload, and sampling
method for performance results. The historical lab smoke and software CI do
not establish a minimum machine specification or a latency guarantee.
