use std::ffi::CString;
use std::io::{Read, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{Access, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use serde::Serialize;

use crate::error::WsError;
use crate::media::{allowed_suffix, media_type_for};
use crate::{MAX_LIST_LIMIT, MAX_LIST_SCAN_ENTRIES, MAX_TRANSFER_BYTES};

mod storage;
pub use storage::{CleanupEntry, ManagedScratch, StorageUsage};

pub const RESERVED_WORKSPACE_ROOT: &str = ".printable";

/// Metadata for one workspace artifact. `path` is normalized and relative to
/// the configured workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactMeta {
    pub path: String,
    pub size_bytes: u64,
    pub media_type: &'static str,
    pub modified_ns: i64,
}

/// Workspace status snapshot for the `printable_status` tool.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceStatus {
    pub confined: bool,
    pub root: Option<String>,
    pub blender_root: Option<String>,
    pub max_transfer_bytes: u64,
    pub max_list_scan_entries: usize,
}

/// A private, immutable copy of a workspace artifact. The backing temporary
/// directory is removed when the snapshot is dropped.
#[derive(Debug)]
pub struct Snapshot {
    _tempdir: ManagedScratch,
    path: PathBuf,
    meta: ArtifactMeta,
    source_stat: Stat,
}

impl Snapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> &ArtifactMeta {
        &self.meta
    }
}

#[derive(Debug)]
struct Confined {
    /// A pinned handle to the canonical root directory. Every operation walks
    /// from this fd with `openat(O_NOFOLLOW)`, so containment does not depend
    /// on re-checking string paths and every component is refused if it is a
    /// symlink — atomically, with no check-then-open window.
    root_fd: OwnedFd,
    root: PathBuf,
}

/// Confined workspace rooted at `PRINTABLE_WORKSPACE_ROOT`, or an unconfined
/// stand-in when no root is configured (artifact operations then fail with
/// [`WsError::Unconfined`]).
#[derive(Debug)]
pub struct Workspace {
    confined: Option<Confined>,
    blender_root: Option<PathBuf>,
}

impl Workspace {
    /// Open a workspace. `blender_root` is the host-side path Blender sees for
    /// the same directory; it is stored verbatim (it usually does not exist
    /// locally). Requiring `root` alongside `blender_root` is the settings
    /// layer's responsibility.
    pub fn open(root: Option<&Path>, blender_root: Option<&Path>) -> Result<Self, WsError> {
        let confined = match root {
            None => None,
            Some(r) => {
                let canonical = std::fs::canonicalize(r)
                    .map_err(|_| WsError::RootNotDirectory(r.display().to_string()))?;
                if !canonical.is_dir() {
                    return Err(WsError::RootNotDirectory(canonical.display().to_string()));
                }
                let root_fd = rustix::fs::open(
                    &canonical,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|_| WsError::RootNotDirectory(canonical.display().to_string()))?;
                Some(Confined {
                    root_fd,
                    root: canonical,
                })
            }
        };
        Ok(Workspace {
            confined,
            blender_root: blender_root.map(Path::to_path_buf),
        })
    }

    pub fn confined(&self) -> bool {
        self.confined.is_some()
    }

    pub fn ready(&self) -> bool {
        let Some(confined) = &self.confined else {
            return false;
        };
        rustix::fs::fstat(&confined.root_fd).is_ok()
            && rustix::fs::accessat(
                &confined.root_fd,
                ".",
                Access::READ_OK | Access::WRITE_OK | Access::EXEC_OK,
                AtFlags::EACCESS,
            )
            .is_ok()
    }

    pub fn status(&self) -> WorkspaceStatus {
        WorkspaceStatus {
            confined: self.confined(),
            root: self.confined.as_ref().map(|c| c.root.display().to_string()),
            blender_root: self.blender_root.as_ref().map(|p| p.display().to_string()),
            max_transfer_bytes: MAX_TRANSFER_BYTES,
            max_list_scan_entries: MAX_LIST_SCAN_ENTRIES,
        }
    }

    /// Validate an ordinary artifact destination before a caller performs
    /// expensive work or allocates upload staging. Server-owned state under
    /// [`RESERVED_WORKSPACE_ROOT`] is never a public mutation destination.
    pub fn validate_public_mutation_path(&self, path: &str) -> Result<(), WsError> {
        self.require_confined()?;
        let comps = normalize(path)?;
        require_mutation_scope(&comps, MutationScope::Public)?;
        let name = comps.last().ok_or(WsError::UnsupportedArtifactType)?;
        allowed_suffix(name).ok_or(WsError::UnsupportedArtifactType)?;
        Ok(())
    }

    /// Atomically reserve an ordinary workspace directory. Returns false for
    /// an existing directory; symlinks and reserved paths remain refused.
    pub fn create_public_directory(&self, path: &str) -> Result<bool, WsError> {
        self.require_confined()?;
        let comps = normalize(path)?;
        require_mutation_scope(&comps, MutationScope::Public)?;
        let name = comps.last().ok_or(WsError::ReservedPath)?;
        let rel = comps.join("/");
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Create)?;
        match rustix::fs::mkdirat(
            parent.as_fd(),
            name.as_str(),
            Mode::from_bits_truncate(0o755),
        ) {
            Ok(()) => {
                rustix::fs::fsync(parent.as_fd()).map_err(errno_io)?;
                Ok(true)
            }
            Err(Errno::EXIST) => {
                drop(self.walk_to(&comps, &rel, Missing::Error)?);
                Ok(false)
            }
            Err(error) => Err(errno_io(error)),
        }
    }

    /// Atomically write `bytes` to the workspace-relative `path`, creating
    /// missing parent directories. `overwrite: false` never replaces an
    /// existing artifact (hard-link commit, first writer wins); `overwrite:
    /// true` replaces a regular-file target atomically but refuses a symlink.
    pub fn write_artifact(
        &self,
        path: &str,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<ArtifactMeta, WsError> {
        self.write_artifact_scoped(path, bytes, overwrite, MutationScope::Public)
    }

    /// Write server-owned state under [`RESERVED_WORKSPACE_ROOT`]. This
    /// capability refuses ordinary artifact paths so internal callers cannot
    /// accidentally bypass the public namespace boundary.
    pub fn write_reserved_artifact(
        &self,
        path: &str,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<ArtifactMeta, WsError> {
        self.write_artifact_scoped(path, bytes, overwrite, MutationScope::Reserved)
    }

    fn write_artifact_scoped(
        &self,
        path: &str,
        bytes: &[u8],
        overwrite: bool,
        scope: MutationScope,
    ) -> Result<ArtifactMeta, WsError> {
        self.require_confined()?;
        let comps = normalize(path)?;
        require_mutation_scope(&comps, scope)?;
        let rel = comps.join("/");
        let name = comps
            .last()
            .ok_or(WsError::UnsupportedArtifactType)?
            .clone();
        let suffix = allowed_suffix(&name).ok_or(WsError::UnsupportedArtifactType)?;
        if bytes.len() as u64 > MAX_TRANSFER_BYTES {
            return Err(WsError::WriteTooLarge);
        }
        let _storage = self.admit_storage_write(bytes.len() as u64)?;
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Create)?;

        let temp_name = temp_name()?;
        let temp_fd = openat(
            parent.as_fd(),
            &temp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
            &rel,
        )?;

        let commit = (|| -> Result<Stat, WsError> {
            let mut temp = std::fs::File::from(temp_fd);
            temp.write_all(bytes)?;
            temp.sync_all()?;
            let st = rustix::fs::fstat(&temp).map_err(errno_io)?;
            if overwrite {
                // Security invariant (airtight): `renameat` atomically repoints
                // the directory entry to our freshly-created regular file and
                // never follows or writes through a symlink, so an overwrite can
                // never create or modify a file outside the root.
                //
                // The pre-check is a best-effort *policy* — "refuse to replace a
                // symlink" — for the common, non-adversarial case (and its
                // stable error string). It is deliberately not race-free:
                // a symlink installed in the window between this statat and the
                // renameat is safely *replaced* rather than refused. That
                // outcome is benign (the symlink entry is removed, its target
                // untouched); there is no atomic "rename-only-if-not-a-symlink"
                // primitive, and none is needed for the security property.
                if let Ok(existing) =
                    rustix::fs::statat(parent.as_fd(), name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                    && FileType::from_raw_mode(existing.st_mode) == FileType::Symlink
                {
                    return Err(WsError::RefusingSymlinkReplace);
                }
                rustix::fs::renameat(
                    parent.as_fd(),
                    temp_name.as_str(),
                    parent.as_fd(),
                    name.as_str(),
                )
                .map_err(errno_io)?;
            } else {
                match rustix::fs::linkat(
                    parent.as_fd(),
                    temp_name.as_str(),
                    parent.as_fd(),
                    name.as_str(),
                    AtFlags::empty(),
                ) {
                    Ok(()) => {}
                    Err(Errno::EXIST) => return Err(WsError::AlreadyExists(rel.clone())),
                    Err(e) => return Err(errno_io(e)),
                }
                rustix::fs::unlinkat(parent.as_fd(), temp_name.as_str(), AtFlags::empty())
                    .map_err(errno_io)?;
            }
            rustix::fs::fsync(parent.as_fd()).map_err(errno_io)?;
            Ok(st)
        })();

        match commit {
            Ok(st) => Ok(meta_from(rel, suffix, &st)),
            Err(error) => Err(cleanup_temp(parent.as_fd(), &temp_name, error)),
        }
    }

    /// Commit a server-generated file (e.g. an OpenSCAD compile output in a
    /// private tempdir) into the workspace through the same atomic path as
    /// [`Workspace::write_artifact`]. The source size is checked against the
    /// transfer cap **before** the file is read, so an oversized artifact is
    /// rejected without loading it into memory.
    pub fn commit_generated_artifact(
        &self,
        path: &str,
        source: &Path,
        overwrite: bool,
    ) -> Result<ArtifactMeta, WsError> {
        self.commit_generated_artifact_bounded(path, source, overwrite, MAX_TRANSFER_BYTES)
    }

    /// Atomically commit a generated file with a caller-selected positive size
    /// budget. Unlike MCP reads and uploads, this path streams bytes and can
    /// therefore persist large media without retaining it in process memory.
    /// The destination remains discoverable through workspace listings, while
    /// reading it through MCP still observes [`MAX_TRANSFER_BYTES`].
    pub fn commit_generated_artifact_bounded(
        &self,
        path: &str,
        source: &Path,
        overwrite: bool,
        max_bytes: u64,
    ) -> Result<ArtifactMeta, WsError> {
        self.commit_generated_artifact_scoped(
            path,
            source,
            overwrite,
            max_bytes,
            MutationScope::Public,
        )
    }

    /// Commit server-owned generated media under
    /// [`RESERVED_WORKSPACE_ROOT`] without relaxing public mutation rules.
    pub fn commit_reserved_generated_artifact_bounded(
        &self,
        path: &str,
        source: &Path,
        overwrite: bool,
        max_bytes: u64,
    ) -> Result<ArtifactMeta, WsError> {
        self.commit_generated_artifact_scoped(
            path,
            source,
            overwrite,
            max_bytes,
            MutationScope::Reserved,
        )
    }

    fn commit_generated_artifact_scoped(
        &self,
        path: &str,
        source: &Path,
        overwrite: bool,
        max_bytes: u64,
        scope: MutationScope,
    ) -> Result<ArtifactMeta, WsError> {
        self.require_confined()?;
        if max_bytes == 0 {
            return Err(WsError::WriteTooLarge);
        }
        let comps = normalize(path)?;
        require_mutation_scope(&comps, scope)?;
        let rel = comps.join("/");
        let name = comps
            .last()
            .ok_or(WsError::UnsupportedArtifactType)?
            .clone();
        let suffix = allowed_suffix(&name).ok_or(WsError::UnsupportedArtifactType)?;
        let mut input = std::fs::File::from(
            rustix::fs::open(
                source,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(errno_io)?,
        );
        let before = input.metadata()?;
        if !before.file_type().is_file() {
            return Err(WsError::NotRegularFile);
        }
        if before.len() > max_bytes {
            return Err(WsError::WriteTooLarge);
        }
        let _storage = self.admit_storage_write(before.len())?;
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Create)?;
        let temp_name = temp_name()?;
        let temp_fd = openat(
            parent.as_fd(),
            &temp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
            &rel,
        )?;

        let commit = (|| -> Result<Stat, WsError> {
            let mut output = std::fs::File::from(temp_fd);
            let copied = std::io::copy(
                &mut Read::by_ref(&mut input).take(before.len().saturating_add(1)),
                &mut output,
            )?;
            if copied > max_bytes {
                return Err(WsError::WriteTooLarge);
            }
            if copied != before.len() {
                return Err(WsError::ChangedWhileReading);
            }
            output.sync_all()?;
            let after = input.metadata()?;
            if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                return Err(WsError::ChangedWhileReading);
            }
            let st = rustix::fs::fstat(&output).map_err(errno_io)?;
            if overwrite {
                if let Ok(existing) =
                    rustix::fs::statat(parent.as_fd(), name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                    && FileType::from_raw_mode(existing.st_mode) == FileType::Symlink
                {
                    return Err(WsError::RefusingSymlinkReplace);
                }
                rustix::fs::renameat(
                    parent.as_fd(),
                    temp_name.as_str(),
                    parent.as_fd(),
                    name.as_str(),
                )
                .map_err(errno_io)?;
            } else {
                match rustix::fs::linkat(
                    parent.as_fd(),
                    temp_name.as_str(),
                    parent.as_fd(),
                    name.as_str(),
                    AtFlags::empty(),
                ) {
                    Ok(()) => {}
                    Err(Errno::EXIST) => return Err(WsError::AlreadyExists(rel.clone())),
                    Err(error) => return Err(errno_io(error)),
                }
                rustix::fs::unlinkat(parent.as_fd(), temp_name.as_str(), AtFlags::empty())
                    .map_err(errno_io)?;
            }
            rustix::fs::fsync(parent.as_fd()).map_err(errno_io)?;
            Ok(st)
        })();

        match commit {
            Ok(st) => Ok(meta_from(rel, suffix, &st)),
            Err(error) => Err(cleanup_temp(parent.as_fd(), &temp_name, error)),
        }
    }

    /// Inspect a regular artifact without reading its bytes or creating a snapshot.
    /// The metadata describes a mutable file at the time of the descriptor stat.
    pub fn stat_artifact(&self, path: &str) -> Result<ArtifactMeta, WsError> {
        self.require_confined()?;
        let comps = normalize(path)?;
        let rel = comps.join("/");
        let name = comps.last().ok_or(WsError::UnsupportedArtifactType)?;
        let suffix = allowed_suffix(name).ok_or(WsError::UnsupportedArtifactType)?;
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Error)?;
        let fd = openat(
            parent.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
            &rel,
        )?;
        let stat = rustix::fs::fstat(&fd).map_err(errno_io)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(WsError::NotRegularFile);
        }
        Ok(meta_from(rel, suffix, &stat))
    }

    pub fn read_artifact(&self, path: &str) -> Result<(ArtifactMeta, Vec<u8>), WsError> {
        self.require_confined()?;
        let comps = normalize(path)?;
        let rel = comps.join("/");
        let name = comps
            .last()
            .ok_or(WsError::UnsupportedArtifactType)?
            .clone();
        let suffix = allowed_suffix(&name).ok_or(WsError::UnsupportedArtifactType)?;
        if suffix == ".mp4" {
            return Err(WsError::NonTransferableArtifact);
        }
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Error)?;

        // O_NOFOLLOW: a symlinked final component is refused atomically by the
        // kernel. O_NONBLOCK: opening an allowed-suffix FIFO returns immediately
        // instead of blocking for a writer, so the regular-file check below is
        // reached (O_NONBLOCK is a no-op for reads of a regular file).
        let fd = openat(
            parent.as_fd(),
            &name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
            &rel,
        )?;
        let mut file = std::fs::File::from(fd);
        let st = rustix::fs::fstat(&file).map_err(errno_io)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(WsError::NotRegularFile);
        }
        if st.st_size as u64 > MAX_TRANSFER_BYTES {
            return Err(WsError::ReadTooLarge);
        }
        let mut buf = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_TRANSFER_BYTES + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_TRANSFER_BYTES {
            return Err(WsError::ReadTooLarge);
        }
        Ok((meta_from(rel, suffix, &st), buf))
    }

    /// List allowed-suffix artifacts under the workspace-relative directory
    /// `path` (empty for the root), recursively, skipping symlinks. Scanning
    /// silently stops at [`MAX_LIST_SCAN_ENTRIES`]. Results are sorted by path
    /// for deterministic tool output.
    pub fn list_artifacts(&self, path: &str, limit: usize) -> Result<Vec<ArtifactMeta>, WsError> {
        self.list_with_scan_cap(path, limit, MAX_LIST_SCAN_ENTRIES)
    }

    /// Private so the `MAX_LIST_SCAN_ENTRIES` ceiling cannot be bypassed by a
    /// caller passing a larger `scan_cap`; the only public listing entry point
    /// is [`Workspace::list_artifacts`], which always uses the fixed cap.
    fn list_with_scan_cap(
        &self,
        path: &str,
        limit: usize,
        scan_cap: usize,
    ) -> Result<Vec<ArtifactMeta>, WsError> {
        if !(1..=MAX_LIST_LIMIT).contains(&limit) {
            return Err(WsError::InvalidLimit);
        }
        self.require_confined()?;
        let comps = normalize(path)?;
        let rel = comps.join("/");
        // Validate the target directory exists (listing a missing directory is
        // an error, not an empty result). The fd is dropped immediately.
        drop(self.walk_to(&comps, &rel, Missing::Error)?);

        let mut scanned = 0usize;
        let mut found: Vec<ArtifactMeta> = Vec::new();
        // The traversal stack holds directory PATHS, not open descriptors: each
        // directory is re-opened (by walking from the root) when it is popped,
        // so at most O(depth) descriptors are open at once. Holding an fd per
        // discovered directory instead could retain thousands of descriptors on
        // a wide tree and exhaust the process fd table.
        let mut stack: Vec<String> = vec![rel];
        'scan: while let Some(prefix) = stack.pop() {
            let dir_comps = split_rel(&prefix);
            let dir_fd = match self.walk_to(&dir_comps, &prefix, Missing::Error) {
                Ok(fd) => fd,
                // Raced away since discovery — skip. Any other openat failure
                // (EMFILE, EACCES, …) propagates rather than silently omitting
                // a subtree.
                Err(WsError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let dir = Dir::read_from(&dir_fd).map_err(errno_io)?;
            for entry in dir {
                if scanned >= scan_cap {
                    break 'scan;
                }
                let entry = entry.map_err(errno_io)?;
                let raw = entry.file_name();
                // `.`/`..` are exactly two trivial entries per directory; skip
                // them without spending the cap.
                if matches!(raw.to_str(), Ok(".") | Ok("..")) {
                    continue;
                }
                // Count every other entry against the cap BEFORE the UTF-8
                // filter — otherwise a directory full of non-UTF-8 names could
                // force an unbounded walk past MAX_LIST_SCAN_ENTRIES.
                scanned += 1;
                let Ok(name) = raw.to_str() else {
                    continue;
                };
                // Stat without following: symlinks are skipped, not traversed.
                let Ok(st) = rustix::fs::statat(dir_fd.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
                else {
                    continue;
                };
                let ftype = FileType::from_raw_mode(st.st_mode);
                let child_rel = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                match ftype {
                    FileType::Directory => stack.push(child_rel),
                    FileType::RegularFile => {
                        if let Some(suffix) = allowed_suffix(name) {
                            found.push(ArtifactMeta {
                                path: child_rel,
                                size_bytes: st.st_size as u64,
                                media_type: media_type_for(suffix),
                                modified_ns: mtime_ns(&st),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found.truncate(limit);
        Ok(found)
    }

    /// Copy the artifact into a private tempdir and verify it did not change
    /// while being copied. Integrity is tied to a **single open descriptor**:
    /// the file is opened once (`O_NOFOLLOW`), and both the copy and the
    /// before/after `fstat` act on that same fd — so a rename-substitute-restore
    /// race on the pathname cannot swap in different bytes. The transfer cap is
    /// enforced before copying.
    pub fn snapshot_artifact(&self, path: &str) -> Result<Snapshot, WsError> {
        self.snapshot_bounded_with_hook(path, MAX_TRANSFER_BYTES, || {})
    }

    /// Copy an artifact into a private immutable snapshot using a positive
    /// caller-selected byte budget. This streams large server-internal inputs
    /// without relaxing the fixed MCP read/upload limit.
    pub fn snapshot_artifact_bounded(
        &self,
        path: &str,
        max_bytes: u64,
    ) -> Result<Snapshot, WsError> {
        self.snapshot_bounded_with_hook(path, max_bytes, || {})
    }

    /// Test seam: `between` runs after the copy and before the re-verify.
    #[doc(hidden)]
    pub fn snapshot_with_hook(
        &self,
        path: &str,
        between: impl FnOnce(),
    ) -> Result<Snapshot, WsError> {
        self.snapshot_bounded_with_hook(path, MAX_TRANSFER_BYTES, between)
    }

    fn snapshot_bounded_with_hook(
        &self,
        path: &str,
        max_bytes: u64,
        between: impl FnOnce(),
    ) -> Result<Snapshot, WsError> {
        self.require_confined()?;
        if max_bytes == 0 {
            return Err(WsError::SnapshotTooLarge(0));
        }
        let comps = normalize(path)?;
        let rel = comps.join("/");
        let name = comps
            .last()
            .ok_or(WsError::UnsupportedArtifactType)?
            .clone();
        let suffix = allowed_suffix(&name).ok_or(WsError::UnsupportedArtifactType)?;
        let parent = self.walk_to(&comps[..comps.len() - 1], &rel, Missing::Error)?;

        // O_NONBLOCK so an allowed-suffix FIFO does not block the open (the
        // regular-file check below rejects it); no-op for a regular file.
        let fd = openat(
            parent.as_fd(),
            &name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
            &rel,
        )?;
        let mut file = std::fs::File::from(fd);
        let before = rustix::fs::fstat(&file).map_err(errno_io)?;
        if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
            return Err(WsError::NotRegularFile);
        }
        if before.st_size as u64 > max_bytes {
            return Err(snapshot_too_large(max_bytes));
        }

        let tempdir = self.scratch(before.st_size as u64, "snapshot")?;
        let dst = tempdir.path().join(&name);
        let mut out = std::fs::File::create(&dst)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut file).take((before.st_size as u64).saturating_add(1)),
            &mut out,
        )?;
        if copied > max_bytes {
            return Err(snapshot_too_large(max_bytes));
        }
        if copied != before.st_size as u64 {
            return Err(WsError::ChangedWhileReading);
        }
        out.sync_all()?;

        between();

        // Re-stat the SAME descriptor: detects modification of the exact inode
        // that was copied, independent of any pathname games.
        let after = rustix::fs::fstat(&file).map_err(errno_io)?;
        if !stat_unchanged(&before, &after) {
            return Err(WsError::ChangedWhileReading);
        }
        Ok(Snapshot {
            _tempdir: tempdir,
            path: dst,
            meta: meta_from(rel, suffix, &before),
            source_stat: before,
        })
    }

    /// Verify that the original path still identifies the snapshotted regular
    /// file without modification. Check every source after collecting a group
    /// of snapshots to detect changes while the group was being assembled.
    pub fn verify_snapshot_source(&self, snapshot: &Snapshot) -> Result<(), WsError> {
        self.require_confined()?;
        let path = &snapshot.meta.path;
        let comps = normalize(path)?;
        let name = comps.last().ok_or(WsError::UnsupportedArtifactType)?;
        let parent = self.walk_to(&comps[..comps.len() - 1], path, Missing::Error)?;
        let fd = openat(
            parent.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
            path,
        )?;
        let current = rustix::fs::fstat(&fd).map_err(errno_io)?;
        if FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
            || current.st_ino != snapshot.source_stat.st_ino
            || current.st_dev != snapshot.source_stat.st_dev
            || !stat_unchanged(&current, &snapshot.source_stat)
        {
            return Err(WsError::ChangedWhileReading);
        }
        Ok(())
    }

    /// Resolve a workspace-relative path to a local absolute path for display
    /// and for handing to local subprocesses. When `must_exist` is set, the
    /// final component is required to exist and is **refused if it is a
    /// symlink**, so a following path is never handed to a subprocess.
    ///
    /// Note: a path returned here is opened later by a separate process, so the
    /// no-symlink guarantee is best-effort against a concurrent post-return
    /// swap. Security-critical subprocess reads must go through
    /// [`Workspace::snapshot_artifact`] (which copies through a pinned fd); the
    /// Blender addon independently re-confines its own file operations.
    pub fn resolve(&self, path: &str, must_exist: bool) -> Result<PathBuf, WsError> {
        match &self.confined {
            Some(c) => {
                let comps = normalize(path)?;
                let rel = comps.join("/");
                if must_exist {
                    self.require_existing_nonsymlink(&comps, &rel)?;
                }
                Ok(if rel.is_empty() {
                    c.root.clone()
                } else {
                    c.root.join(&rel)
                })
            }
            None => {
                let p = Path::new(path);
                let abs = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    std::env::current_dir()?.join(p)
                };
                let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
                if must_exist && !abs.exists() {
                    return Err(WsError::NotFound(path.to_string()));
                }
                Ok(abs)
            }
        }
    }

    /// Map a workspace-relative path onto the path Blender sees for the same
    /// file: the configured Blender-side root when set, otherwise the local
    /// root. Same final-symlink refusal as [`Workspace::resolve`].
    pub fn to_blender_path(&self, path: &str, must_exist: bool) -> Result<String, WsError> {
        match &self.confined {
            Some(c) => {
                let comps = normalize(path)?;
                let rel = comps.join("/");
                if must_exist {
                    self.require_existing_nonsymlink(&comps, &rel)?;
                }
                let base = self.blender_root.as_deref().unwrap_or(&c.root);
                Ok(if rel.is_empty() {
                    base.display().to_string()
                } else {
                    base.join(&rel).display().to_string()
                })
            }
            None => self
                .resolve(path, must_exist)
                .map(|p| p.display().to_string()),
        }
    }

    /// The Blender-side workspace root, for the addon's re-confinement
    /// authority parameter on export/import/save commands.
    pub fn blender_authority_root(&self) -> Option<String> {
        self.confined.as_ref().map(|c| {
            self.blender_root
                .as_deref()
                .unwrap_or(&c.root)
                .display()
                .to_string()
        })
    }

    fn require_confined(&self) -> Result<&Confined, WsError> {
        self.confined.as_ref().ok_or(WsError::Unconfined)
    }

    /// Walk to the directory named by `comps`, opening each component
    /// `O_NOFOLLOW` (so a symlinked component is refused atomically). Returns an
    /// owned fd for the target directory.
    fn walk_to(&self, comps: &[String], rel: &str, missing: Missing) -> Result<OwnedFd, WsError> {
        self.walk_to_with_sync(comps, rel, missing, |parent| {
            rustix::fs::fsync(parent).map_err(errno_io)
        })
    }

    fn walk_to_with_sync<F>(
        &self,
        comps: &[String],
        rel: &str,
        missing: Missing,
        mut sync_parent: F,
    ) -> Result<OwnedFd, WsError>
    where
        F: for<'fd> FnMut(BorrowedFd<'fd>) -> Result<(), WsError>,
    {
        let confined = self.require_confined()?;
        // Owned handle to the root ("." is never a symlink).
        let mut cur = openat(
            confined.root_fd.as_fd(),
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
            rel,
        )?;
        for comp in comps {
            cur = match openat(
                cur.as_fd(),
                comp,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
                rel,
            ) {
                Ok(fd) => fd,
                Err(WsError::NotFound(_)) if missing == Missing::Create => {
                    match rustix::fs::mkdirat(
                        cur.as_fd(),
                        comp.as_str(),
                        Mode::from_bits_truncate(0o755),
                    ) {
                        Ok(()) => {}
                        Err(Errno::EXIST) => {}
                        Err(e) => return Err(errno_io(e)),
                    }
                    sync_parent(cur.as_fd())?;
                    openat(
                        cur.as_fd(),
                        comp,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                        Mode::empty(),
                        rel,
                    )?
                }
                Err(e) => return Err(e),
            };
        }
        Ok(cur)
    }

    /// Require that the final component exists and is not a symlink.
    fn require_existing_nonsymlink(&self, comps: &[String], rel: &str) -> Result<(), WsError> {
        if comps.is_empty() {
            return Ok(()); // the root itself
        }
        let parent = self.walk_to(&comps[..comps.len() - 1], rel, Missing::Error)?;
        let name = &comps[comps.len() - 1];
        match rustix::fs::statat(parent.as_fd(), name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Symlink => {
                Err(WsError::SymlinkRefused)
            }
            Ok(_) => Ok(()),
            Err(Errno::NOENT) => Err(WsError::NotFound(rel.to_string())),
            Err(e) => Err(errno_io(e)),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Missing {
    Create,
    Error,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum MutationScope {
    Public,
    Reserved,
}

/// `openat` with the given flags, mapping rustix errno to the workspace error
/// surface. `ELOOP`/`EMLINK` (symlink under `O_NOFOLLOW`) and `ENOTDIR`
/// (a non-directory where a directory was required) both map to the symlink
/// refusal; `ENOENT` maps to not-found.
fn openat(
    dir: BorrowedFd,
    name: &str,
    flags: OFlags,
    mode: Mode,
    rel: &str,
) -> Result<OwnedFd, WsError> {
    // Reject interior NUL rather than letting it truncate the path.
    let cname = CString::new(name).map_err(|_| WsError::PathEscapes)?;
    rustix::fs::openat(dir, cname.as_c_str(), flags, mode).map_err(|e| map_component_errno(e, rel))
}

fn map_component_errno(e: Errno, rel: &str) -> WsError {
    match e {
        Errno::LOOP | Errno::MLINK | Errno::NOTDIR => WsError::SymlinkRefused,
        Errno::NOENT => WsError::NotFound(rel.to_string()),
        other => errno_io(other),
    }
}

fn errno_io(e: Errno) -> WsError {
    WsError::Io(std::io::Error::from_raw_os_error(e.raw_os_error()))
}

/// Split an already-normalized internal relative path (as produced by
/// [`normalize`] and re-joined) back into components. Empty for the root.
fn split_rel(rel: &str) -> Vec<String> {
    rel.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Normalize a caller-supplied workspace-relative path into components.
/// Absolute paths and any `..` that would climb past the root are refused;
/// `..` that stays inside the root is folded away, so `a/../n.stl` is legal and
/// resolves to `n.stl`.
fn normalize(path: &str) -> Result<Vec<String>, WsError> {
    if path.starts_with('/') {
        return Err(WsError::PathEscapes);
    }
    let mut parts: Vec<String> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(WsError::PathEscapes);
                }
            }
            other => parts.push(other.to_string()),
        }
    }
    Ok(parts)
}

fn require_mutation_scope(parts: &[String], scope: MutationScope) -> Result<(), WsError> {
    let reserved = parts
        .first()
        .is_some_and(|part| part == RESERVED_WORKSPACE_ROOT);
    if reserved == (scope == MutationScope::Reserved) {
        Ok(())
    } else {
        Err(WsError::ReservedPath)
    }
}

fn snapshot_too_large(max_bytes: u64) -> WsError {
    if max_bytes == MAX_TRANSFER_BYTES {
        WsError::ReadTooLarge
    } else {
        WsError::SnapshotTooLarge(max_bytes)
    }
}

fn cleanup_temp(dir: BorrowedFd<'_>, temp_name: &str, original: WsError) -> WsError {
    match rustix::fs::unlinkat(dir, temp_name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => original,
        Err(cleanup) => WsError::Io(std::io::Error::other(format!(
            "{original}; failed to remove temporary artifact: {cleanup}"
        ))),
    }
}

fn stat_unchanged(a: &Stat, b: &Stat) -> bool {
    a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
        && a.st_ctime == b.st_ctime
        && a.st_ctime_nsec == b.st_ctime_nsec
}

// rustix's `Stat` time fields differ by backend: `st_mtime` is `i64` on both,
// but `st_mtime_nsec` is `i64` (c_long) on macOS and `u64` on the Linux
// backend — so `i64::from` won't compile cross-platform. `as i64` handles both
// (a nanosecond count is always in range); the redundant-cast lint fires only
// on the target where a field is already `i64`, hence the allow.
#[allow(clippy::unnecessary_cast)]
fn mtime_ns(st: &Stat) -> i64 {
    (st.st_mtime as i64) * 1_000_000_000 + (st.st_mtime_nsec as i64)
}

fn meta_from(rel: String, suffix: &'static str, st: &Stat) -> ArtifactMeta {
    ArtifactMeta {
        path: rel,
        size_bytes: st.st_size as u64,
        media_type: media_type_for(suffix),
        modified_ns: mtime_ns(st),
    }
}

fn temp_name() -> Result<String, WsError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| WsError::Io(std::io::Error::other(e.to_string())))?;
    let mut name = String::with_capacity(11 + 32);
    name.push_str(".printable-");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(name, "{b:02x}");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the private scan_cap directly (the public API only exposes the
    // fixed MAX_LIST_SCAN_ENTRIES ceiling, so a small cap can't be tested there).
    #[test]
    fn scan_cap_truncates_without_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::open(Some(dir.path()), None).unwrap();
        for i in 0..60 {
            ws.write_artifact(&format!("f{i:03}.stl"), b"x", false)
                .unwrap();
        }
        // A cap below the entry count truncates the walk without error. The
        // internal storage directory also consumes a scanned entry.
        let capped = ws.list_with_scan_cap("", 1000, 50).unwrap();
        assert!(!capped.is_empty() && capped.len() <= 50);
        assert!(capped.iter().all(|entry| entry.path.starts_with('f')));
        // The public API with the real ceiling returns everything.
        assert_eq!(ws.list_artifacts("", 1000).unwrap().len(), 60);
    }

    #[test]
    fn every_created_directory_entry_synchronizes_its_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::open(Some(dir.path()), None).unwrap();
        let comps = ["one", "two", "three"].map(str::to_string);
        let mut syncs = 0;
        let created = ws
            .walk_to_with_sync(&comps, "one/two/three", Missing::Create, |_| {
                syncs += 1;
                Ok(())
            })
            .unwrap();
        drop(created);
        assert_eq!(syncs, 3);

        let mut existing_syncs = 0;
        let existing = ws
            .walk_to_with_sync(&comps, "one/two/three", Missing::Create, |_| {
                existing_syncs += 1;
                Ok(())
            })
            .unwrap();
        drop(existing);
        assert_eq!(existing_syncs, 0);
    }
}
