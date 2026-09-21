//! Tool-layer errors.
//!
//! One `thiserror` enum with a machine-readable `.code()` (house style),
//! delegating to the sibling crates' own codes. A tool error is rendered into a
//! recoverable MCP result (`is_error`), not a JSON-RPC error, so the agent can
//! read the code and message and adjust.

/// An error from a tool invocation.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("CAD build failed: {0}")]
    Cad(String),
    /// The arguments failed to deserialize into the tool's typed parameters.
    #[error("invalid arguments: {0}")]
    Validation(String),
    /// A typed printer service failure; mutation uncertainty must not be retried blindly.
    #[error(transparent)]
    Printer(#[from] bambuddy_api::ApiError),
    /// `data_base64` was not valid base64.
    #[error("data_base64 is not valid base64")]
    InvalidBase64,
    /// A single request's decoded payload exceeds the per-request limit. Large
    /// artifacts must use the chunked upload tools instead of one buffered body.
    #[error(
        "request payload exceeds {0} bytes; upload larger artifacts with write_begin/write_chunk/write_commit"
    )]
    PayloadTooLarge(usize),
    /// The `upload_id` names no open upload (never begun, already committed, or
    /// reclaimed after its idle timeout).
    #[error("no such upload: unknown or expired upload_id")]
    UploadNotFound,
    /// The concurrent-upload cap is reached; commit or abandon an upload first.
    #[error("too many concurrent uploads (max {0})")]
    TooManyUploads(usize),
    /// The retained immutable-publish cap is reached. Published files expire
    /// automatically, so callers can retry after an earlier handoff completes.
    #[error("too many artifacts awaiting file transfer (max {0}); retry shortly")]
    TooManyPublishedFiles(usize),
    /// A filesystem error staging or committing an upload.
    #[error("upload io error: {0}")]
    Io(#[from] std::io::Error),
    /// Serializing a tool response failed (effectively unreachable for our
    /// types; carried rather than panicked).
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    /// A confined-workspace error (surfaces the workspace's own message/code).
    #[error(transparent)]
    Workspace(#[from] printable_workspace::WsError),
    /// A Blender bridge error (surfaces the client's own message/code).
    #[error(transparent)]
    Blender(#[from] printable_blender::BlenderError),
    /// A mesh decoding or geometry-analysis error.
    #[error(transparent)]
    Geometry(#[from] printable_geom::GeometryError),
    /// Confined OpenSCAD source validation or import snapshotting failed.
    #[error(transparent)]
    ScadGate(#[from] printable_scad::GateError),
    /// OpenSCAD process execution failed.
    #[error(transparent)]
    Scad(#[from] printable_scad::ScadError),
    /// The isolated exact-geometry worker failed or returned a domain error.
    #[error("assembly geometry worker failed: {message}")]
    GeometryWorker { code: &'static str, message: String },
    /// Durable render-job admission, lookup, persistence, or worker failure.
    #[error("{0}")]
    Job(String),
}

impl ToolError {
    /// A stable machine-readable code for the error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            ToolError::Cad(_) => "cad_build",
            ToolError::Validation(_) => "validation",
            ToolError::Printer(bambuddy_api::ApiError::AmbiguousOutcome) => {
                "printer_outcome_unknown"
            }
            ToolError::Printer(bambuddy_api::ApiError::Rejected(_)) => "printer_rejected",
            ToolError::Printer(_) => "printer",
            ToolError::InvalidBase64 => "invalid_base64",
            ToolError::PayloadTooLarge(_) => "payload_too_large",
            ToolError::UploadNotFound => "upload_not_found",
            ToolError::TooManyUploads(_) => "too_many_uploads",
            ToolError::TooManyPublishedFiles(_) => "too_many_published_files",
            ToolError::Io(_) => "io",
            ToolError::Serde(_) => "serde",
            ToolError::Workspace(e) => e.code(),
            ToolError::Blender(e) => e.code(),
            ToolError::Geometry(e) => e.code(),
            ToolError::ScadGate(e) => e.code(),
            ToolError::Scad(e) => e.code(),
            ToolError::GeometryWorker { code, .. } => code,
            ToolError::Job(message) if message == "render job not found" => "job_not_found",
            ToolError::Job(message) if message.starts_with("render job queue is full") => {
                "job_queue_full"
            }
            ToolError::Job(_) => "job",
        }
    }
}
