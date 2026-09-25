//! Confined workspace I/O.
//!
//! All artifact reads and writes walk from a pinned root directory fd, opening
//! every path component with `openat(O_NOFOLLOW)` (via `rustix`) so the kernel
//! refuses a symlinked component atomically at open time — with no
//! check-then-open window, and refusing even symlinks that would stay in-root.
//! cap-std is deliberately not used: its sandbox permits in-root symlinks,
//! whereas this confinement refuses every caller-controlled symlink. Writes
//! are atomic (`O_EXCL` temp + fsync + link-or-rename); reads and snapshots act
//! on a single pinned descriptor so their integrity checks can't be defeated by
//! a pathname swap.
//!
//! The atomic-write primitive is public so sibling crates (SCAD compile
//! output, render persistence) share the same hardened path.
//!
//! Error codes and texts are a stable operator/tool API. Base64 handling (and
//! its `data_base64 is not valid base64` message) belongs to the tool layer,
//! not this crate: the API here is byte-oriented.
//!
//! # Threat model
//!
//! This layer is an **input boundary**: it defends against a caller supplying a
//! traversal (`../`), an absolute path, or a symlink to escape the configured
//! root. That defense is airtight — every component is opened `O_NOFOLLOW`, so
//! no caller-supplied path is ever followed outside the root, and reads/
//! snapshots are pinned to a descriptor. Durable writes synchronize each newly
//! created directory entry and the final artifact entry before returning.
//!
//! Ordinary mutation capabilities reject the reserved `.printable` namespace.
//! Server-owned durable state uses separate reserved-only mutation methods, so
//! typed uploads and generated outputs cannot replace job checkpoints,
//! metadata, frames, or videos. Reads and listings remain available.
//!
//! It deliberately does **not** defend against a **concurrent writer running as
//! the same OS principal inside the workspace**. Such an actor could, in a
//! narrow window, swap a directory entry between a check and its use (e.g.
//! race a symlink into an overwrite destination, or substitute the random
//! `O_EXCL` temp name). These races are not confinement escapes — `renameat`
//! never writes *through* a symlink (it replaces the entry, target untouched),
//! and the temp name is a 128-bit secret — and they grant nothing an
//! already-authorized workspace writer does not already have. Closing them
//! would require tying every commit to the written inode, for which there is no
//! portable unprivileged primitive across supported Linux and Darwin targets
//! (`AT_EMPTY_PATH`/`O_TMPFILE` are Linux-specific and may require additional
//! authority). Stronger enforcement requires a separate identity or OS
//! sandbox.
//!
//! Unix-only: confinement is built on `O_NOFOLLOW`/`O_DIRECTORY` semantics.

#[cfg(not(unix))]
compile_error!("printable-workspace requires a Unix platform (O_NOFOLLOW/O_DIRECTORY confinement)");

mod error;
mod media;
mod workspace;

pub use error::WsError;
pub use media::{ALLOWED_ARTIFACT_SUFFIXES, media_type_for};
pub use workspace::{
    ArtifactMeta, CleanupEntry, ManagedScratch, Snapshot, StorageUsage, Workspace, WorkspaceStatus,
};

/// Hard cap on a single artifact transfer.
pub const MAX_TRANSFER_BYTES: u64 = 25 * 1024 * 1024;

/// Directory-scan bound for listings. Reaching it returns the bounded prefix
/// rather than spending unbounded time on caller-controlled directory trees.
pub const MAX_LIST_SCAN_ENTRIES: usize = 10_000;

/// Inclusive upper bound on the `limit` argument to
/// [`Workspace::list_artifacts`] (the lower bound is 1).
pub const MAX_LIST_LIMIT: usize = 1000;
