use crate::media::allowed_suffix_list;
use crate::{MAX_LIST_LIMIT, MAX_TRANSFER_BYTES};

/// Workspace errors. Display texts are stable operator messages; `code()` is
/// the stable machine identifier for the tool layer.
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("workspace tools require PRINTABLE_WORKSPACE_ROOT")]
    Unconfined,
    #[error("PRINTABLE_WORKSPACE_ROOT is not a directory: {0}")]
    RootNotDirectory(String),
    #[error("path escapes PRINTABLE_WORKSPACE_ROOT")]
    PathEscapes,
    #[error("path is reserved for Printable internal state")]
    ReservedPath,
    #[error("workspace path changed or symbolic link escapes PRINTABLE_WORKSPACE_ROOT")]
    SymlinkRefused,
    #[error("refusing to replace a symbolic link")]
    RefusingSymlinkReplace,
    #[error("unsupported artifact type; allowed: {}", allowed_suffix_list())]
    UnsupportedArtifactType,
    #[error("workspace artifact is not a regular file")]
    NotRegularFile,
    #[error("artifact already exists: {0}")]
    AlreadyExists(String),
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("decoded artifact exceeds {MAX_TRANSFER_BYTES} bytes")]
    WriteTooLarge,
    #[error("artifact exceeds MCP transfer limit of {MAX_TRANSFER_BYTES} bytes")]
    ReadTooLarge,
    #[error("video artifacts are path-addressable and cannot be read as base64")]
    NonTransferableArtifact,
    #[error("artifact exceeds caller-selected snapshot limit of {0} bytes")]
    SnapshotTooLarge(u64),
    #[error("workspace artifact changed while being read")]
    ChangedWhileReading,
    #[error("limit must be between 1 and {MAX_LIST_LIMIT}")]
    InvalidLimit,
    #[error("workspace I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl WsError {
    pub fn code(&self) -> &'static str {
        match self {
            WsError::Unconfined => "unconfined",
            WsError::RootNotDirectory(_) => "root_not_directory",
            WsError::PathEscapes => "path_escapes",
            WsError::ReservedPath => "reserved_path",
            WsError::SymlinkRefused => "symlink_refused",
            WsError::RefusingSymlinkReplace => "symlink_replace",
            WsError::UnsupportedArtifactType => "unsupported_type",
            WsError::NotRegularFile => "not_regular_file",
            WsError::AlreadyExists(_) => "already_exists",
            WsError::NotFound(_) => "not_found",
            WsError::WriteTooLarge => "write_too_large",
            WsError::ReadTooLarge => "read_too_large",
            WsError::NonTransferableArtifact => "non_transferable_artifact",
            WsError::SnapshotTooLarge(_) => "read_too_large",
            WsError::ChangedWhileReading => "changed_while_reading",
            WsError::InvalidLimit => "invalid_limit",
            WsError::Io(_) => "io",
        }
    }
}
