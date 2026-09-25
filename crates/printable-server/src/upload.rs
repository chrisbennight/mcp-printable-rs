//! Process-wide chunked-upload registry backing the streaming write tools.
//!
//! rmcp buffers each request body whole before dispatch, so a single-shot write
//! of a large base64 payload retains that whole payload per in-flight request —
//! an unbounded aggregate under concurrency. The streaming path caps every
//! request to one small chunk: `write_begin` opens a private staging file,
//! `write_chunk` appends a bounded decoded chunk to it (the base64 body never
//! arrives whole), and `write_commit` promotes the completed staging file into
//! the workspace through the same atomic path as a server-generated artifact.
//!
//! Abandoned uploads are reclaimed lazily by an idle timeout; each upload's
//! staging directory is removed when its registry entry is dropped. The registry
//! is shared across all MCP sessions, so its bounds (concurrent-upload cap,
//! per-upload size cap) protect process memory and staging disk regardless of
//! how many clients are connected.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

use printable_workspace::{ArtifactMeta, MAX_TRANSFER_BYTES, Workspace, WsError};

use crate::error::ToolError;
use crate::tools::blocking;

/// Largest decoded payload a single request (`write_chunk` or single-shot
/// `write`) accepts. Bounds any one request's base64 body to ~4/3 of this, so a
/// burst of concurrent writes cannot buffer an unbounded aggregate before
/// dispatch; larger artifacts are streamed as a sequence of these chunks.
pub const CHUNK_MAX_DECODED: usize = 1024 * 1024;

/// Largest number of uploads open at once, across all sessions. Bounds staging
/// disk (each upload stages up to [`MAX_TRANSFER_BYTES`]) and registry size.
pub const MAX_CONCURRENT_UPLOADS: usize = 8;

/// An upload with no `begin`/`chunk` activity for this long is reclaimable on the
/// next `begin`. Its staging directory is removed when the entry is dropped.
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// One upload's mutable state. Guarded by an async mutex so a client's chunks
/// serialize (and `commit` waits for an in-flight chunk) without blocking other
/// uploads. `committed` fences a chunk that raced a `commit`: once the artifact
/// is published, a later chunk that already held the upload handle must fail
/// rather than append bytes the published artifact excludes.
struct UploadState {
    written: u64,
    last_activity: Instant,
    committed: bool,
}

/// One in-flight upload. Dropping it removes the staging directory (`_dir`),
/// whether the upload was committed, superseded, or reclaimed by the timeout.
///
/// `state` is `Arc`-wrapped so its guard can be taken *owned* and moved into the
/// blocking append/commit job. `spawn_blocking` detaches: a guard held in the
/// async future would be dropped on cancellation while the append ran on,
/// letting a later chunk/commit interleave with the orphaned append. Owned into
/// the job, the guard — and the append and `written` update it protects —
/// release only when the job itself finishes.
struct Upload {
    _dir: printable_workspace::ManagedScratch,
    staging: PathBuf,
    target: String,
    overwrite: bool,
    state: Arc<AsyncMutex<UploadState>>,
}

/// Process-wide registry of chunked uploads. The outer mutex guards the map;
/// each upload's own async mutex serializes that upload's chunks. The map is
/// `Arc`-wrapped so `commit` can drop its own registry entry *inside* the
/// detached blocking job — never in the cancellable async future before the
/// upload reaches that job — so a cancelled commit can never orphan the entry.
pub struct UploadRegistry {
    uploads: Arc<Mutex<HashMap<String, Arc<Upload>>>>,
}

impl UploadRegistry {
    pub fn new() -> Self {
        Self {
            uploads: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Open a new upload for `target`, returning its opaque id. Rejects on an
    /// unconfined server, reserved internal destinations, and a full
    /// concurrent-upload registry (after reclaiming timed-out uploads).
    pub async fn begin(
        &self,
        workspace: &Arc<Workspace>,
        target: String,
        overwrite: bool,
    ) -> Result<String, ToolError> {
        // Validate every destination invariant before allocating staging or a
        // registry entry; commit repeats the checks against the final path.
        workspace.validate_public_mutation_path(&target)?;
        // Create the staging dir + empty file off the async workers, before
        // taking the registry lock (so the lock is never held across I/O).
        let workspace = Arc::clone(workspace);
        let (dir, staging) = blocking(move || {
            let dir = workspace.scratch(MAX_TRANSFER_BYTES, "chunked_upload")?;
            let staging = dir.path().join("blob");
            std::fs::File::create(&staging)?;
            Ok::<_, ToolError>((dir, staging))
        })
        .await?;

        let upload = Arc::new(Upload {
            _dir: dir,
            staging,
            target,
            overwrite,
            state: Arc::new(AsyncMutex::new(UploadState {
                written: 0,
                last_activity: Instant::now(),
                committed: false,
            })),
        });

        let id = random_hex_id()?;
        // Reclaim timed-out uploads, enforce the cap, and insert atomically. On a
        // full registry the new `upload` (and any `evicted` entries) drop *after*
        // the lock is released, so their staging-dir removals happen off-lock.
        let evicted;
        {
            let mut map = self.uploads.lock().expect("upload registry mutex poisoned");
            evicted = collect_expired(&mut map, Instant::now());
            if map.len() >= MAX_CONCURRENT_UPLOADS {
                drop(map);
                drop(evicted);
                return Err(ToolError::TooManyUploads(MAX_CONCURRENT_UPLOADS));
            }
            map.insert(id.clone(), upload);
        }
        drop(evicted);
        Ok(id)
    }

    /// Append one decoded chunk to an open upload, returning the running total.
    /// Chunks for one upload are serialized; the cumulative size is capped at
    /// [`MAX_TRANSFER_BYTES`].
    pub async fn chunk(&self, upload_id: &str, decoded: Vec<u8>) -> Result<u64, ToolError> {
        if decoded.len() > CHUNK_MAX_DECODED {
            return Err(ToolError::PayloadTooLarge(CHUNK_MAX_DECODED));
        }
        let upload = self.get(upload_id)?;
        // In tests, widen the window between the lookup and the lock so a
        // concurrent commit can be interleaved deterministically.
        #[cfg(test)]
        tokio::task::yield_now().await;
        // Serialize this upload's chunks with an OWNED guard moved into the
        // blocking job, so the guard, the append, and the `written` update all
        // release together only when the job finishes — a cancelled request
        // cannot orphan a half-applied append (see `Upload`).
        let guard = Arc::clone(&upload.state).lock_owned().await;
        // Move the guard *and* `upload` (which owns the staging TempDir) into the
        // blocking job, so the guard, the append, the `written` update, and the
        // staging directory all release together only when the job finishes — a
        // cancelled request can neither orphan a half-applied append nor drop the
        // staging dir mid-write.
        blocking(move || {
            let mut state = guard;
            // A committed upload rejects further chunks: appending after the
            // artifact was published would add bytes it excludes.
            if state.committed {
                return Err(ToolError::UploadNotFound);
            }
            let new_total = state.written + decoded.len() as u64;
            if new_total > MAX_TRANSFER_BYTES {
                return Err(WsError::WriteTooLarge.into());
            }
            // A prior append may have written a partial prefix before erroring
            // (write_all can write some bytes then fail), leaving the file longer
            // than `written`. Roll it back to the tracked length so it holds only
            // fully-appended chunks, then append this one.
            truncate(&upload.staging, state.written)?;
            append(&upload.staging, &decoded)?;
            state.written = new_total;
            state.last_activity = Instant::now();
            Ok(new_total)
        })
        .await
    }

    /// Promote a completed upload's staging file into the workspace atomically,
    /// removing the registry entry on success. Waits for any in-flight chunk, then
    /// commits through the workspace's server-generated-artifact path (which
    /// enforces path confinement, the transfer cap, and overwrite/symlink rules).
    pub async fn commit(
        &self,
        upload_id: &str,
        workspace: &Arc<Workspace>,
    ) -> Result<ArtifactMeta, ToolError> {
        // Clone the handle *without* removing the registry entry, so the entry —
        // and the staging TempDir it owns — is retained until the commit actually
        // runs. If this future is cancelled while awaiting the per-upload lock or
        // the workspace permit below, the registry still holds the upload: the
        // staged data survives and the caller can retry. The entry is dropped only
        // inside the detached blocking job, on success, so a cancelled commit can
        // never orphan it (either the job runs and removes it, or it never does).
        let upload = self.get(upload_id)?;
        let guard = Arc::clone(&upload.state).lock_owned().await;
        let ws = Arc::clone(workspace);
        let uploads = Arc::clone(&self.uploads);
        let id = upload_id.to_string();
        blocking(move || {
            let mut state = guard;
            // A prior commit already published this upload (its detached job may
            // have completed even after that request was cancelled): reject the
            // duplicate rather than committing twice.
            if state.committed {
                return Err(ToolError::UploadNotFound);
            }
            // Discard any bytes a prior append left after erroring mid-write, so
            // the committed artifact contains only complete chunks.
            truncate(&upload.staging, state.written)?;
            let meta =
                ws.commit_generated_artifact(&upload.target, &upload.staging, upload.overwrite)?;
            // On success only: fence any racing chunk and drop the registry entry,
            // atomically with the commit in this detached job. On failure the entry
            // is kept so the caller can retry.
            state.committed = true;
            uploads
                .lock()
                .expect("upload registry mutex poisoned")
                .remove(&id);
            Ok(meta)
            // `upload` (and its TempDir) drops here, after the source is read.
        })
        .await
    }

    /// Clone the `Arc` for an open upload, or report it unknown/expired.
    fn get(&self, upload_id: &str) -> Result<Arc<Upload>, ToolError> {
        self.uploads
            .lock()
            .expect("upload registry mutex poisoned")
            .get(upload_id)
            .cloned()
            .ok_or(ToolError::UploadNotFound)
    }
}

impl Default for UploadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove uploads idle since before `now - UPLOAD_IDLE_TIMEOUT`, returning their
/// entries so their staging directories are dropped by the caller *after* the
/// lock is released. An upload another task is operating on — one with a chunk in
/// flight, or one just cloned from the map — is kept. `now` is a parameter so the
/// timeout is testable without waiting.
fn collect_expired(map: &mut HashMap<String, Arc<Upload>>, now: Instant) -> Vec<Arc<Upload>> {
    let expired: Vec<String> = map
        .iter()
        .filter(|(_, up)| {
            // Never evict an upload another task holds. A concurrent chunk/commit
            // clones the Arc under this same map lock before it takes the state
            // lock, so `strong_count > 1` means an operation is in flight (its
            // state may still be unlocked, in the window before it locks) —
            // evicting it would let that op append to or commit a detached entry
            // whose bytes are then dropped. Only a truly abandoned entry (the map
            // its sole owner) is reclaimable.
            if Arc::strong_count(up) > 1 {
                return false;
            }
            match up.state.try_lock() {
                Ok(state) => {
                    now.saturating_duration_since(state.last_activity) >= UPLOAD_IDLE_TIMEOUT
                }
                Err(_) => false,
            }
        })
        .map(|(id, _)| id.clone())
        .collect();
    expired.iter().filter_map(|id| map.remove(id)).collect()
}

/// Append bytes to the staging file (`O_APPEND`). Durability is deferred to
/// commit, which fsyncs through the workspace's atomic write.
fn append(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Truncate the staging file to `len` bytes, discarding any trailing bytes a
/// prior partially-completed append left behind. `len` is the tracked `written`
/// count, which is never greater than the file (appends only grow it), so this
/// only ever shortens the file.
fn truncate(path: &Path, len: u64) -> Result<(), ToolError> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    Ok(())
}

/// A 128-bit random, hex-encoded identifier used for unguessable upload ids and
/// collision-resistant visual-render batch paths. Uses the same CSPRNG as the
/// workspace's temporary names.
pub(crate) fn random_hex_id() -> Result<String, ToolError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ToolError::Io(std::io::Error::other(e.to_string())))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_upload(workspace: &Workspace, last_activity: Instant) -> Arc<Upload> {
        let dir = workspace
            .scratch(MAX_TRANSFER_BYTES, "test_upload")
            .unwrap();
        let staging = dir.path().join("blob");
        std::fs::File::create(&staging).expect("touch staging file");
        Arc::new(Upload {
            _dir: dir,
            staging,
            target: "x.stl".to_string(),
            overwrite: false,
            state: Arc::new(AsyncMutex::new(UploadState {
                written: 0,
                last_activity,
                committed: false,
            })),
        })
    }

    #[test]
    fn collect_expired_evicts_idle_uploads_and_keeps_fresh_ones() {
        // Anchor both uploads at `base`, then evaluate the GC at a `now` far
        // enough ahead that only the older one is past the idle timeout. An
        // injected `now` keeps the test independent of machine uptime.
        let base = Instant::now();
        let now = base + UPLOAD_IDLE_TIMEOUT + Duration::from_secs(1);

        let mut map = HashMap::new();
        let root = TempDir::new().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        map.insert("stale".to_string(), staged_upload(&ws, base));
        map.insert("fresh".to_string(), staged_upload(&ws, now));

        let evicted = collect_expired(&mut map, now);

        assert_eq!(evicted.len(), 1, "exactly the idle upload is evicted");
        assert!(map.contains_key("fresh"), "a just-active upload is kept");
        assert!(!map.contains_key("stale"), "an idle upload is removed");
    }

    #[test]
    fn collect_expired_keeps_an_idle_upload_an_operation_still_holds() {
        // An upload past the idle timeout but with an outstanding Arc clone (a
        // chunk/commit that cloned it before locking) must not be reclaimed —
        // otherwise that operation would append to or commit a detached entry
        // whose bytes are then dropped.
        let base = Instant::now();
        let now = base + UPLOAD_IDLE_TIMEOUT + Duration::from_secs(1);

        let mut map = HashMap::new();
        let root = TempDir::new().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        map.insert("in_use".to_string(), staged_upload(&ws, base));
        // Simulate an in-flight operation holding the upload.
        let _in_flight = Arc::clone(map.get("in_use").expect("present"));

        let evicted = collect_expired(&mut map, now);

        assert!(evicted.is_empty(), "an in-use idle upload is not evicted");
        assert!(map.contains_key("in_use"), "the in-use upload is kept");
    }

    #[tokio::test]
    async fn begin_rejects_reserved_job_state_before_allocating_an_upload() {
        let dir = TempDir::new().expect("workspace tempdir");
        let ws = Arc::new(Workspace::open(Some(dir.path()), None).expect("open workspace"));
        let reg = UploadRegistry::new();

        let error = reg
            .begin(&ws, ".printable/jobs/job/source.blend".to_string(), true)
            .await
            .expect_err("reserved destination must fail at begin");
        assert!(
            matches!(error, ToolError::Workspace(WsError::ReservedPath)),
            "got {error:?}"
        );
        assert!(
            reg.uploads.lock().expect("upload registry").is_empty(),
            "rejected destination must not consume registry capacity"
        );
    }

    #[tokio::test]
    async fn chunk_racing_commit_fails_loudly_without_silent_loss() {
        let dir = TempDir::new().expect("workspace tempdir");
        let ws = Arc::new(Workspace::open(Some(dir.path()), None).expect("open workspace"));
        let reg = Arc::new(UploadRegistry::new());

        let id = reg
            .begin(&ws, "race.stl".to_string(), false)
            .await
            .expect("begin");
        reg.chunk(&id, b"first".to_vec())
            .await
            .expect("first chunk");

        // A late chunk that loses the race with commit. The cfg(test) yield in
        // `chunk` parks it right after the registry lookup, so `commit` below
        // runs to completion (removing the id and publishing "first") before the
        // chunk acquires the state lock.
        let reg2 = Arc::clone(&reg);
        let id2 = id.clone();
        let late = tokio::spawn(async move { reg2.chunk(&id2, b"late".to_vec()).await });

        // Let the spawned chunk reach its yield (after the lookup, before lock).
        tokio::task::yield_now().await;

        let meta = reg.commit(&id, &ws).await.expect("commit");
        assert_eq!(
            meta.size_bytes,
            b"first".len() as u64,
            "commit publishes only the pre-commit bytes"
        );

        // The racing chunk fails loudly rather than silently appending after the
        // artifact was published.
        let late_result = late.await.expect("join late chunk");
        assert!(
            matches!(late_result, Err(ToolError::UploadNotFound)),
            "a post-commit chunk must fail, got {late_result:?}"
        );

        // The committed artifact is exactly the pre-commit content.
        let (_, bytes) = ws
            .read_artifact("race.stl")
            .expect("read committed artifact");
        assert_eq!(
            bytes, b"first",
            "no post-commit bytes leaked into the artifact"
        );
    }

    #[tokio::test]
    async fn commit_does_not_consume_the_upload_until_it_succeeds() {
        // `commit` clones the entry (never removes it up front) and drops it only
        // inside the blocking job on success. So a commit that fails — here a
        // no-overwrite conflict — must leave the upload staged and usable, which is
        // also what protects a cancelled commit from discarding staged data. A
        // reversion to remove-before-commit would strand the upload on failure.
        let dir = TempDir::new().expect("workspace tempdir");
        let ws = Arc::new(Workspace::open(Some(dir.path()), None).expect("open workspace"));
        let reg = Arc::new(UploadRegistry::new());

        // Occupy the target so a no-overwrite commit conflicts.
        ws.write_artifact("taken.stl", b"existing", false)
            .expect("seed the target");

        let id = reg
            .begin(&ws, "taken.stl".to_string(), false)
            .await
            .expect("begin");
        reg.chunk(&id, b"data".to_vec()).await.expect("chunk");

        let err = reg
            .commit(&id, &ws)
            .await
            .expect_err("commit conflicts with the existing target");
        assert!(
            matches!(err, ToolError::Workspace(WsError::AlreadyExists(_))),
            "got {err:?}"
        );

        // The upload survived the failed commit: it is still registered and can
        // still be appended to (a reversion to remove-before-commit would have
        // discarded it, and this chunk would fail UploadNotFound).
        let total = reg
            .chunk(&id, b"more".to_vec())
            .await
            .expect("upload retained after a failed commit");
        assert_eq!(total, b"datamore".len() as u64);
    }

    #[tokio::test]
    async fn stray_bytes_from_a_partial_append_are_truncated_before_reuse() {
        // A partial (errored) append can leave bytes in the staging file that the
        // tracked `written` count excludes. The next chunk and commit must roll
        // the file back to `written` first, so those stray bytes never reach the
        // committed artifact.
        let dir = TempDir::new().expect("workspace tempdir");
        let ws = Arc::new(Workspace::open(Some(dir.path()), None).expect("open workspace"));
        let reg = Arc::new(UploadRegistry::new());

        let id = reg
            .begin(&ws, "heal.stl".to_string(), false)
            .await
            .expect("begin");
        reg.chunk(&id, b"good".to_vec()).await.expect("chunk"); // written = 4

        // Simulate a prior chunk whose append wrote a prefix then errored: extra
        // bytes are in the staging file but not reflected in `written`.
        let staging = {
            let map = reg.uploads.lock().expect("map");
            map.get(&id).expect("upload present").staging.clone()
        };
        append(&staging, b"XX").expect("inject stray bytes");

        // The next chunk truncates the stray bytes (back to `written`) before it
        // appends, and commit publishes only complete chunks.
        reg.chunk(&id, b"next".to_vec()).await.expect("next chunk");
        let meta = reg.commit(&id, &ws).await.expect("commit");
        assert_eq!(meta.size_bytes, b"goodnext".len() as u64);

        let (_, bytes) = ws.read_artifact("heal.stl").expect("read committed");
        assert_eq!(
            bytes, b"goodnext",
            "stray partial-append bytes must not reach the artifact"
        );
    }
}
