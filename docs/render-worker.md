# Isolated checkpoint rendering

New durable render jobs require `PRINTABLE_RENDER_WORKER_HOST` and use a
separate background Blender process. The live Blender connection remains
available for modeling and observations while the worker renders. Completion,
cancellation, and restart recovery never save or restore the live scene.
The job still requires a caller-created `.blend` checkpoint; submission does
not implicitly save the current live scene.

The server snapshots and hashes the source file before enqueueing. Job metadata
retains its SHA-256 and byte size under `source_snapshot`; the worker verifies
the staged checkpoint's digest before loading it. A subsequent overwrite
of the caller's checkpoint cannot change the queued job's source. Reserved job
artifacts, per-frame progress, manufacturing certificates, bounded storage,
FFmpeg encoding, and file publication retain their existing contracts. One
worker processes one job at a time; the admission queue remains bounded by
`PRINTABLE_RENDER_JOB_QUEUE_DEPTH`.

## Runtime identity and recovery

The worker container sets `PRINTABLE_BLENDER_ROLE=render_worker` and
`PRINTABLE_BLENDER_MODE=background`. The server uses
`PRINTABLE_RENDER_WORKER_HOST` and optional `PRINTABLE_RENDER_WORKER_PORT`
(default 9876). Before loading a checkpoint, each job checks the worker's role
and background mode within the same transaction as its rendering commands.
A wrong endpoint fails before loading source. The bridge's process identity
check detects a worker restart between transaction commands. There is no
fallback to the live endpoint.

Job metadata reports `execution.mode: isolated_worker` and whether Blender
frame work has finished. Recovery resumes at the first unrecorded frame. A
frame whose output reached disk before its progress commit may be rerendered;
this affects only the worker and reserved artifacts. Once Blender completion
is persisted, recovery can finish encoding without loading the source again.
Cancellation remains cooperative between frames and terminates an active
FFmpeg encoder. Worker failures and malformed worker job metadata block or
fail render work without claiming that live modeling failed.

Separate processes do not provide separate physical GPUs. The deployment must
validate native observation latency and RAM/VRAM use during worker rendering on
the shared NVIDIA device. The runtime does not stop other GPU users or claim
unlimited concurrency.

`status.render_jobs.worker` reports actual role-checked readiness. An occupied
worker returns promptly with `status: busy` and its last verified availability;
it does not wait behind a render. Missing configuration, connection failure,
and a role mismatch remain distinct. `/readyz` includes sanitized worker
availability, while `/healthz` and MCP remain accessible for live modeling and
recovery when the worker is unavailable.

## Checkpoint boundary

The immutable input is the snapshotted `.blend` file. External files that were
not packed into it are not snapshotted by submission. For self-contained image
and library dependencies, use Blender's packing operations before saving the
checkpoint. Blender documents that some media, including video dependencies,
cannot be packed; callers using such references must retain their files for
the job's lifetime. This release does not claim an immutable archive of all
external dependencies. See Blender's [packed-data documentation](https://docs.blender.org/manual/en/latest/files/blend/packed_data.html).

## Migration

Legacy session rendering remains only for recovery of historical jobs. New
submission requires a configured worker and established isolation, even on a
fresh workspace. Before configuring the worker,
drain existing jobs and complete any outstanding live-session restoration.
Unfinished legacy jobs block worker migration rather than being replayed
against either endpoint. Known outstanding live restoration retains its
existing mutation fence. Terminal historical job records remain readable.

Migration writes `.printable/render-isolation.json` only after readable history
proves that legacy work is drained. Unreadable history or an invalid migration
record preserves the live mutation fence until that obligation can be resolved.
After the migration record is established, removing worker configuration does
not permit new legacy jobs: render submission reports the missing worker.
Unreadable isolated-job metadata then blocks rendering without fencing live
modeling. Do not restore legacy job data over an established migration record;
a data rollback must coordinate both histories.

Durable metadata version 3 distinguishes isolated execution from legacy
session records. Version 2 records can be read during migration. A previous
binary that does not support version 3 must not be used as a rollback over
new job metadata without a coordinated data rollback.

The [paired-container smoke](../scripts/smoke-release-pair) provisions a separate
worker and uses the public MCP surface to edit the live scene during a
certified mechanical render. It checks that the edit and live scene revision
survive completion, validates the video and certificate, and verifies that no
live-session checkpoint was captured or restored.
