//! Shared admission and conservative reclamation of service-owned temporary data.
use super::*;
use rustix::fs::FlockOperation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const CONTROL: &str = ".printable/storage";
const SCRATCH: &str = ".printable/storage/scratch";
const SCAN_LIMIT: usize = 100_000;

#[derive(Debug, Serialize)]
pub struct StorageUsage {
    pub format_version: u32,
    pub budget_bytes: Option<u64>,
    pub logical_bytes: u64,
    pub reserved_remaining_bytes: u64,
    pub charged_bytes: u64,
    pub by_project: BTreeMap<String, u64>,
    pub project_groups_truncated: bool,
    pub by_class: BTreeMap<String, u64>,
    pub complete: bool,
    pub scan_limit: usize,
    pub accounting: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CleanupEntry {
    pub id: String,
    pub bytes: Option<u64>,
    pub state: &'static str,
    pub reason: &'static str,
}

#[derive(Serialize, Deserialize)]
struct Reservation {
    bytes: u64,
    purpose: String,
}

/// A shared lease protects this scratch directory until the last owner drops.
/// Failed automatic cleanup leaves an inactive directory visible as pending;
/// only explicit cleanup or a later owner can confirm its removal.
#[derive(Debug)]
pub struct ManagedScratch {
    workspace: Workspace,
    id: String,
    path: PathBuf,
    lease: Option<OwnedFd>,
    cleanup_on_drop: bool,
}

impl ManagedScratch {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ManagedScratch {
    fn drop(&mut self) {
        // Lock order is controller then lease everywhere. Releasing our shared
        // lease before trying cleanup permits other owners to keep it protected.
        drop(self.lease.take());
        if self.cleanup_on_drop {
            let _ = self
                .workspace
                .cleanup_scratch(std::slice::from_ref(&self.id));
        }
    }
}

impl Workspace {
    fn storage_lock(&self) -> Result<OwnedFd, WsError> {
        let dir = self.walk_to(&split_rel(CONTROL), CONTROL, Missing::Create)?;
        let lock = openat(
            dir.as_fd(),
            "controller.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::from_bits_truncate(0o600),
            CONTROL,
        )?;
        if FileType::from_raw_mode(rustix::fs::fstat(&lock).map_err(errno_io)?.st_mode)
            != FileType::RegularFile
        {
            return Err(WsError::NotRegularFile);
        }
        rustix::fs::flock(&lock, FlockOperation::LockExclusive).map_err(errno_io)?;
        Ok(lock)
    }

    /// Startup configuration is retained once for all workers sharing this root.
    /// A lower budget stops new writes; it never removes retained artifacts.
    pub fn configure_storage_budget(&self, bytes: Option<u64>) -> Result<(), WsError> {
        let _lock = self.storage_lock()?;
        let dir = self.walk_to(&split_rel(CONTROL), CONTROL, Missing::Error)?;
        write_json(&dir, "budget.json", &bytes)
    }

    fn storage_budget(&self) -> Result<Option<u64>, WsError> {
        let dir = self.walk_to(&split_rel(CONTROL), CONTROL, Missing::Error)?;
        read_json(&dir, "budget.json").map(|v| v.flatten())
    }

    pub fn storage_usage(&self) -> Result<StorageUsage, WsError> {
        let _lock = self.storage_lock()?;
        self.storage_usage_locked()
    }

    fn storage_usage_locked(&self) -> Result<StorageUsage, WsError> {
        let mut usage = StorageUsage {
            format_version: 1,
            budget_bytes: self.storage_budget()?,
            logical_bytes: 0,
            reserved_remaining_bytes: 0,
            charged_bytes: 0,
            by_project: BTreeMap::new(),
            by_class: BTreeMap::new(),
            project_groups_truncated: false,
            complete: true,
            scan_limit: SCAN_LIMIT,
            accounting: "Logical file bytes plus unused live reservations. Hard links count per name. Symlinks are not followed. Native writes, external temporary directories, filesystem metadata and allocation overhead are outside the enforced service boundary.",
        };
        let mut scratch_bytes = BTreeMap::<String, u64>::new();
        let mut stack = vec![String::new()];
        let mut scanned = 0;
        while let Some(prefix) = stack.pop() {
            let dir = match self.walk_to(&split_rel(&prefix), &prefix, Missing::Error) {
                Ok(v) => v,
                Err(_) => {
                    usage.complete = false;
                    continue;
                }
            };
            for entry in Dir::read_from(&dir).map_err(errno_io)? {
                let entry = entry.map_err(errno_io)?;
                let raw = entry.file_name();
                if matches!(raw.to_str(), Ok(".") | Ok("..")) {
                    continue;
                }
                scanned += 1;
                if scanned > SCAN_LIMIT {
                    usage.complete = false;
                    stack.clear();
                    break;
                }
                let Ok(name) = raw.to_str() else {
                    usage.complete = false;
                    continue;
                };
                let path = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                let stat = match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(v) => v,
                    Err(_) => {
                        usage.complete = false;
                        continue;
                    }
                };
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::Directory if split_rel(&path).len() <= 64 => stack.push(path),
                    FileType::Directory => usage.complete = false,
                    FileType::RegularFile => {
                        let bytes = stat.st_size.max(0) as u64;
                        usage.logical_bytes = usage.logical_bytes.saturating_add(bytes);
                        let parts = split_rel(&path);
                        let mut project =
                            if parts.first().is_some_and(|s| s == "projects") && parts.len() > 2 {
                                parts[1].as_str()
                            } else {
                                "_workspace"
                            };
                        if usage.by_project.len() >= 1000 && !usage.by_project.contains_key(project)
                        {
                            usage.project_groups_truncated = true;
                            project = "_other_projects";
                        }
                        *usage.by_project.entry(project.to_owned()).or_default() += bytes;
                        let class = if let Some(rest) = path.strip_prefix(&format!("{SCRATCH}/")) {
                            let id = rest.split('/').next().expect("nonempty suffix");
                            *scratch_bytes.entry(id.to_owned()).or_default() += bytes;
                            "temporary"
                        } else if path.starts_with(".printable/") {
                            "retained_internal"
                        } else {
                            match Path::new(name).extension().and_then(|s| s.to_str()) {
                                Some("stl" | "step" | "stp" | "glb" | "3mf" | "blend") => "models",
                                Some("png" | "jpg" | "jpeg" | "mp4") => "media",
                                Some("py" | "scad") => "source",
                                Some("gcode") => "toolpaths",
                                _ => "other",
                            }
                        };
                        *usage.by_class.entry(class.to_owned()).or_default() += bytes;
                    }
                    _ => {}
                }
            }
        }
        for (id, actual) in scratch_bytes {
            let dir = self.scratch_directory(&id)?;
            let (lease, active) = inspect_lease(&dir)?;
            if active {
                match read_json::<Reservation>(&dir, "reservation.json")? {
                    Some(reservation) => {
                        usage.reserved_remaining_bytes = usage
                            .reserved_remaining_bytes
                            .saturating_add(reservation.bytes.saturating_sub(actual))
                    }
                    None => usage.complete = false,
                }
            }
            drop(lease);
        }
        usage.charged_bytes = usage
            .logical_bytes
            .saturating_add(usage.reserved_remaining_bytes);
        Ok(usage)
    }

    fn require_storage_capacity(&self, bytes: u64) -> Result<(), WsError> {
        if self.storage_budget()?.is_none() {
            return Ok(());
        }
        let usage = self.storage_usage_locked()?;
        if !usage.complete {
            return Err(WsError::StorageAccountingIncomplete);
        }
        if usage
            .budget_bytes
            .is_some_and(|limit| usage.charged_bytes.saturating_add(bytes) > limit)
        {
            return Err(WsError::StorageBudgetExceeded);
        }
        Ok(())
    }

    // The guard spans the complete atomic write, including its temporary copy.
    // Other processes cannot independently spend the same available capacity.
    pub(super) fn admit_storage_write(&self, bytes: u64) -> Result<Option<OwnedFd>, WsError> {
        let lock = self.storage_lock()?;
        // Unlimited installations do not serialize long artifact copies. Work
        // already admitted before a later budget change may finish normally.
        if self.storage_budget()?.is_none() {
            return Ok(None);
        }
        self.require_storage_capacity(bytes)?;
        Ok(Some(lock))
    }

    pub fn scratch(&self, bytes: u64, purpose: &str) -> Result<ManagedScratch, WsError> {
        let _lock = self.storage_lock()?;
        let reservation = bytes
            .checked_add(4096)
            .ok_or(WsError::StorageBudgetExceeded)?;
        self.require_storage_capacity(reservation)?;
        let id = temp_name()?.trim_start_matches(".printable-").to_owned();
        let parent = self.walk_to(&split_rel(SCRATCH), SCRATCH, Missing::Create)?;
        rustix::fs::mkdirat(&parent, id.as_str(), Mode::from_bits_truncate(0o700))
            .map_err(errno_io)?;
        let dir = self.scratch_directory(&id)?;
        let lease = lease_file(&dir)?;
        rustix::fs::flock(&lease, FlockOperation::LockShared).map_err(errno_io)?;
        write_json(
            &dir,
            "reservation.json",
            &Reservation {
                bytes: reservation,
                purpose: purpose.chars().take(64).collect(),
            },
        )?;
        rustix::fs::fsync(&parent).map_err(errno_io)?;
        self.scratch_handle(id, lease)
    }

    /// Acquire another live reference under the same lock cleanup uses. A stale
    /// preview cannot remove a directory once this lease has been acquired.
    pub fn retain_scratch(&self, id: &str) -> Result<ManagedScratch, WsError> {
        let _lock = self.storage_lock()?;
        let dir = self.scratch_directory(id)?;
        let reservation: Reservation =
            read_json(&dir, "reservation.json")?.ok_or(WsError::StorageAccountingIncomplete)?;
        let (probe, active) = inspect_lease(&dir)?;
        drop(probe);
        if !active {
            self.require_storage_capacity(
                reservation
                    .bytes
                    .saturating_sub(tree_bytes(&dir, 0, &mut 0)?),
            )?;
        }
        let lease = lease_file(&dir)?;
        rustix::fs::flock(&lease, FlockOperation::LockShared).map_err(errno_io)?;
        self.scratch_handle(id.to_owned(), lease)
    }

    fn scratch_handle(&self, id: String, lease: OwnedFd) -> Result<ManagedScratch, WsError> {
        let confined = self.require_confined()?;
        let root_fd = rustix::io::dup(&confined.root_fd).map_err(errno_io)?;
        Ok(ManagedScratch {
            workspace: Workspace {
                confined: Some(Confined {
                    root_fd,
                    root: confined.root.clone(),
                }),
                blender_root: self.blender_root.clone(),
            },
            path: confined.root.join(SCRATCH).join(&id),
            id,
            lease: Some(lease),
            cleanup_on_drop: true,
        })
    }

    fn scratch_directory(&self, id: &str) -> Result<OwnedFd, WsError> {
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(WsError::InvalidScratchId);
        }
        let path = format!("{SCRATCH}/{id}");
        self.walk_to(&split_rel(&path), &path, Missing::Error)
    }

    pub fn cleanup_preview(&self) -> Result<Vec<CleanupEntry>, WsError> {
        let _lock = self.storage_lock()?;
        let parent = self.walk_to(&split_rel(SCRATCH), SCRATCH, Missing::Create)?;
        let mut entries = Vec::new();
        for entry in Dir::read_from(&parent).map_err(errno_io)? {
            let entry = entry.map_err(errno_io)?;
            let name = entry.file_name();
            if matches!(name.to_str(), Ok(".") | Ok("..")) {
                continue;
            }
            if entries.len() >= 1000 {
                return Err(WsError::StorageAccountingIncomplete);
            }
            let id = name.to_str().map_err(|_| WsError::InvalidScratchId)?;
            let dir = self.scratch_directory(id)?;
            let (_lease, active) = inspect_lease(&dir)?;
            let bytes = tree_bytes(&dir, 0, &mut 0)?;
            entries.push(CleanupEntry {
                id: id.into(),
                bytes: Some(bytes),
                state: if active { "protected" } else { "pending" },
                reason: if active {
                    "live owner lease"
                } else {
                    "abandoned service temporary data; deletion not yet confirmed"
                },
            });
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(entries)
    }

    pub fn cleanup_scratch(&self, ids: &[String]) -> Result<Vec<CleanupEntry>, WsError> {
        if ids.len() > 1000 {
            return Err(WsError::InvalidLimit);
        }
        for id in ids {
            if id.len() != 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(WsError::InvalidScratchId);
            }
        }
        let _lock = self.storage_lock()?;
        let parent = self.walk_to(&split_rel(SCRATCH), SCRATCH, Missing::Create)?;
        let mut result = Vec::new();
        for id in ids {
            let dir = match self.scratch_directory(id) {
                Ok(dir) => dir,
                Err(WsError::NotFound(_)) => {
                    result.push(CleanupEntry {
                        id: id.clone(),
                        bytes: Some(0),
                        state: "deleted",
                        reason: "absence confirmed",
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let (_lease, active) = inspect_lease(&dir)?;
            if active {
                result.push(CleanupEntry {
                    id: id.clone(),
                    bytes: None,
                    state: "protected",
                    reason: "live owner acquired or retained a lease",
                });
                continue;
            }
            let deleted = remove_contents(&dir, 0, &mut 0)
                .and_then(|()| {
                    rustix::fs::unlinkat(&parent, id.as_str(), AtFlags::REMOVEDIR).map_err(errno_io)
                })
                .and_then(|()| rustix::fs::fsync(&parent).map_err(errno_io));
            result.push(CleanupEntry {
                id: id.clone(),
                bytes: None,
                state: if deleted.is_ok() {
                    "deleted"
                } else {
                    "pending"
                },
                reason: if deleted.is_ok() {
                    "directory removal and parent synchronization confirmed"
                } else {
                    "cleanup failed or durability is uncertain; inspect and retry"
                },
            });
        }
        Ok(result)
    }
}

fn lease_file(dir: &OwnedFd) -> Result<OwnedFd, WsError> {
    let fd = openat(
        dir.as_fd(),
        "lease",
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::from_bits_truncate(0o600),
        SCRATCH,
    )?;
    if FileType::from_raw_mode(rustix::fs::fstat(&fd).map_err(errno_io)?.st_mode)
        != FileType::RegularFile
    {
        return Err(WsError::NotRegularFile);
    }
    Ok(fd)
}

fn inspect_lease(dir: &OwnedFd) -> Result<(Option<OwnedFd>, bool), WsError> {
    // Absence cannot hide an owner: creation and ownership acquisition also
    // hold the controller lock. Cleanup must not need a free inode to proceed.
    let fd = match openat(
        dir.as_fd(),
        "lease",
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        SCRATCH,
    ) {
        Ok(fd) => fd,
        Err(WsError::NotFound(_)) => return Ok((None, false)),
        Err(error) => return Err(error),
    };
    if FileType::from_raw_mode(rustix::fs::fstat(&fd).map_err(errno_io)?.st_mode)
        != FileType::RegularFile
    {
        return Err(WsError::NotRegularFile);
    }
    let active = match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => false,
        Err(Errno::WOULDBLOCK) => true,
        Err(e) => return Err(errno_io(e)),
    };
    Ok((Some(fd), active))
}

fn read_json<T: serde::de::DeserializeOwned>(
    dir: &OwnedFd,
    name: &str,
) -> Result<Option<T>, WsError> {
    let fd = match openat(
        dir.as_fd(),
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        CONTROL,
    ) {
        Ok(fd) => fd,
        Err(WsError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(WsError::NotRegularFile);
    }
    if file.metadata()?.len() > 4096 {
        return Err(WsError::StorageAccountingIncomplete);
    }
    serde_json::from_reader(file.take(4097))
        .map(Some)
        .map_err(|_| WsError::StorageAccountingIncomplete)
}

fn write_json<T: Serialize>(dir: &OwnedFd, name: &str, value: &T) -> Result<(), WsError> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let temp = temp_name()?;
    let fd = openat(
        dir.as_fd(),
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_bits_truncate(0o600),
        CONTROL,
    )?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes)?;
    file.sync_all()?;
    rustix::fs::renameat(dir, temp.as_str(), dir, name).map_err(errno_io)?;
    rustix::fs::fsync(dir).map_err(errno_io)
}

fn tree_bytes(dir: &OwnedFd, depth: usize, scanned: &mut usize) -> Result<u64, WsError> {
    if depth > 64 {
        return Err(WsError::StorageAccountingIncomplete);
    }
    let mut bytes = 0;
    for entry in Dir::read_from(dir).map_err(errno_io)? {
        let entry = entry.map_err(errno_io)?;
        let name = entry.file_name();
        if matches!(name.to_str(), Ok(".") | Ok("..")) {
            continue;
        }
        *scanned += 1;
        if *scanned > SCAN_LIMIT {
            return Err(WsError::StorageAccountingIncomplete);
        }
        let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_io)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let fd = rustix::fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(errno_io)?;
            bytes += tree_bytes(&fd, depth + 1, scanned)?;
        } else if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile {
            bytes += stat.st_size.max(0) as u64;
        }
    }
    Ok(bytes)
}

fn remove_contents(dir: &OwnedFd, depth: usize, scanned: &mut usize) -> Result<(), WsError> {
    if depth > 64 {
        return Err(WsError::StorageAccountingIncomplete);
    }
    for entry in Dir::read_from(dir).map_err(errno_io)? {
        let entry = entry.map_err(errno_io)?;
        let name = entry.file_name();
        if matches!(name.to_str(), Ok(".") | Ok("..")) {
            continue;
        }
        *scanned += 1;
        if *scanned > SCAN_LIMIT {
            return Err(WsError::StorageAccountingIncomplete);
        }
        let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_io)?;
        let flags = if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = rustix::fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(errno_io)?;
            remove_contents(&child, depth + 1, scanned)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(dir, name, flags).map_err(errno_io)?;
    }
    rustix::fs::fsync(dir).map_err(errno_io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_reservations_share_one_budget_and_reconcile_lost_owners() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        ws.configure_storage_budget(Some(17_000)).unwrap();
        let barrier = Arc::new(Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let root = root.path().to_owned();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let ws = Workspace::open(Some(&root), None).unwrap();
                    barrier.wait();
                    ws.scratch(6000, "concurrent")
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for result in results.iter().filter(|r| r.is_err()) {
            assert!(matches!(result, Err(WsError::StorageBudgetExceeded)));
        }
        let mut live = results.into_iter().find_map(Result::ok).unwrap();
        assert!(ws.storage_usage().unwrap().reserved_remaining_bytes > 6000);
        // Closing the lease without cleanup models loss of the owning process.
        live.cleanup_on_drop = false;
        let id = live.id.clone();
        drop(live);
        let reopened = Workspace::open(Some(root.path()), None).unwrap();
        assert_eq!(
            reopened.storage_usage().unwrap().reserved_remaining_bytes,
            0
        );
        assert_eq!(reopened.cleanup_preview().unwrap()[0].state, "pending");
        assert_eq!(reopened.cleanup_scratch(&[id]).unwrap()[0].state, "deleted");
    }

    #[test]
    fn stale_preview_cannot_delete_new_owner_or_retained_evidence() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        for path in [
            ".printable/jobs/job/source.blend",
            ".printable/revisions/project/revision.json",
            ".printable/deliveries/receipt.json",
        ] {
            ws.write_reserved_artifact(path, b"retained", false)
                .unwrap();
        }
        ws.write_artifact("projects/p/checkpoint.blend", b"checkpoint", false)
            .unwrap();
        let mut scratch = ws.scratch(100, "transfer").unwrap();
        let id = scratch.id.clone();
        let path = scratch.path.clone();
        std::fs::write(path.join("snapshot.stl"), b"input").unwrap();
        scratch.cleanup_on_drop = false;
        drop(scratch);
        assert_eq!(ws.cleanup_preview().unwrap()[0].state, "pending");
        let active = ws.retain_scratch(&id).unwrap();
        assert_eq!(
            ws.cleanup_scratch(std::slice::from_ref(&id)).unwrap()[0].state,
            "protected"
        );
        assert_eq!(std::fs::read(path.join("snapshot.stl")).unwrap(), b"input");
        drop(active);
        assert!(!path.exists());
        assert_eq!(ws.cleanup_scratch(&[id]).unwrap()[0].state, "deleted");
        assert_eq!(
            ws.read_artifact("projects/p/checkpoint.blend").unwrap().1,
            b"checkpoint"
        );
        assert_eq!(
            ws.read_artifact(".printable/revisions/project/revision.json")
                .unwrap()
                .1,
            b"retained"
        );
        assert!(ws.cleanup_scratch(&["../jobs".into()]).is_err());
    }

    #[test]
    fn pressure_rejection_and_safe_cleanup_restore_normal_writes() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        ws.configure_storage_budget(Some(24_000)).unwrap();
        ws.write_artifact("projects/p/source.stl", &[0; 2000], false)
            .unwrap();
        let mut scratch = ws.scratch(12_000, "staging").unwrap();
        std::fs::write(scratch.path().join("data"), [0; 12_000]).unwrap();
        assert!(matches!(
            ws.write_artifact("new.stl", &[0; 12_000], false),
            Err(WsError::StorageBudgetExceeded)
        ));
        let usage = ws.storage_usage().unwrap();
        assert!(usage.complete);
        assert_eq!(usage.by_project["p"], 2000);
        assert!(usage.by_class["temporary"] >= 12_000);
        let id = scratch.id.clone();
        scratch.cleanup_on_drop = false;
        drop(scratch);
        assert_eq!(ws.cleanup_scratch(&[id]).unwrap()[0].state, "deleted");
        ws.write_artifact("new.stl", &[0; 12_000], false).unwrap();
        assert_eq!(ws.stat_artifact("new.stl").unwrap().size_bytes, 12_000);
    }

    #[test]
    fn failed_cleanup_stays_pending_and_symlinks_never_reach_external_data() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), b"outside").unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        let mut scratch = ws.scratch(0, "abandoned").unwrap();
        let id = scratch.id.clone();
        let path = scratch.path.clone();
        std::os::unix::fs::symlink(outside.path(), path.join("external")).unwrap();
        let mut deep = path.join("deep");
        for _ in 0..66 {
            deep.push("nested");
        }
        std::fs::create_dir_all(&deep).unwrap();
        scratch.cleanup_on_drop = false;
        drop(scratch);
        assert_eq!(
            ws.cleanup_scratch(std::slice::from_ref(&id)).unwrap()[0].state,
            "pending"
        );
        assert!(path.exists());
        std::fs::remove_dir_all(path.join("deep")).unwrap();
        assert_eq!(ws.cleanup_scratch(&[id]).unwrap()[0].state, "deleted");
        assert_eq!(
            std::fs::read(outside.path().join("keep")).unwrap(),
            b"outside"
        );
    }
}
