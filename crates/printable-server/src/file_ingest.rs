//! Native raw-byte uploads consumed by workspace artifact operations.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use axum::{
    Json,
    body::{Body, HttpBody},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use printable_workspace::{ALLOWED_ARTIFACT_SUFFIXES, ArtifactMeta, Workspace, WsError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, upload::random_hex_id};

pub const INGEST_TOOL: &str = "printable_workspace_ingest";
const URI_PREFIX: &str = "mcp-file://printable/upload/";
const MAX_TRANSFERS: usize = 20;
const GRANT_LIFETIME: Duration = Duration::from_secs(3600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizeUploadParams {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub digest: Option<ExpectedDigest>,
    #[serde(rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IngestParams {
    /// File URI returned by the native file upload handoff.
    #[schemars(extend("x-mcp-file" = {"transferModes": ["upload"], "accept": ALLOWED_ARTIFACT_SUFFIXES}))]
    pub file: String,
    /// Destination path relative to the confined workspace.
    #[schemars(schema_with = "ingest_path_schema")]
    pub path: String,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferStatusParams {
    /// Printable-private URI returned by files/authorizeUpload.
    pub uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleParams {
    pub uri: String,
    #[serde(rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}

fn ingest_path_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<String>();
    let alternatives = ALLOWED_ARTIFACT_SUFFIXES
        .iter()
        .map(|suffix| format!("\\{suffix}"))
        .collect::<Vec<_>>()
        .join("|");
    schema.insert(
        "pattern".into(),
        serde_json::json!(format!("({alternatives})$")),
    );
    schema
}

#[derive(Clone, Serialize)]
pub struct IngestResult {
    #[serde(flatten)]
    pub artifact: ArtifactMeta,
    pub sha256: String,
}

struct Received {
    file: Arc<StagedUpload>,
    size: u64,
    sha256: String,
    expires: Instant,
}

struct StagedUpload {
    file: tempfile::NamedTempFile,
    _scratch: printable_workspace::ManagedScratch,
}

impl StagedUpload {
    fn path(&self) -> &std::path::Path {
        self.file.path()
    }
}

enum TransferState {
    Prepared,
    Receiving,
    Cancelling,
    Ready(Received),
    Publishing { path: String },
    Consumed,
}

struct Transfer {
    id: String,
    token_hash: [u8; 32],
    expected_size: Option<u64>,
    expected_digest: Option<[u8; 32]>,
    grant_expires: Instant,
    state: Arc<tokio::sync::Mutex<TransferState>>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    cancellation: CancellationToken,
}

pub struct IncomingFiles {
    workspace: Arc<Workspace>,
    max_bytes: u64,
    max_staged: usize,
    transfers: Arc<Mutex<HashMap<String, Arc<Transfer>>>>,
    capacity: Arc<Semaphore>,
}

/// Created only on a blocking worker, before its staged-file owner. The file
/// owner drops first, including during unwinding, before tracking and capacity
/// are released. A cancelled requesting future does not own this cleanup.
struct WorkCompletion {
    transfers: Arc<Mutex<HashMap<String, Arc<Transfer>>>>,
    transfer: Arc<Transfer>,
}

impl Drop for WorkCompletion {
    fn drop(&mut self) {
        *self.transfer.state.blocking_lock() = TransferState::Consumed;
        retire_transfer(&self.transfers, &self.transfer);
    }
}

fn retire_transfer(transfers: &Mutex<HashMap<String, Arc<Transfer>>>, transfer: &Arc<Transfer>) {
    let mut registry = transfers.lock().expect("incoming file registry poisoned");
    if registry
        .get(&transfer.id)
        .is_some_and(|current| Arc::ptr_eq(current, transfer))
    {
        registry.remove(&transfer.id);
    }
    drop(registry);
    transfer
        .permit
        .lock()
        .expect("upload permit poisoned")
        .take();
}

impl IncomingFiles {
    pub fn new(workspace: Arc<Workspace>, max_bytes: u64) -> Self {
        Self::with_capacity(workspace, max_bytes, MAX_TRANSFERS)
    }

    pub fn with_capacity(workspace: Arc<Workspace>, max_bytes: u64, max_staged: usize) -> Self {
        Self {
            workspace,
            max_bytes,
            max_staged,
            transfers: Arc::new(Mutex::new(HashMap::new())),
            capacity: Arc::new(Semaphore::new(max_staged)),
        }
    }

    pub fn authorize(
        &self,
        params: AuthorizeUploadParams,
        base_url: &reqwest::Url,
    ) -> Result<serde_json::Value, ToolError> {
        if !self.workspace.confined() {
            return Err(ToolError::Validation(
                "file upload requires a confined workspace".into(),
            ));
        }
        if params.size.is_some_and(|size| size > self.max_bytes) {
            return Err(ToolError::FileUpload {
                code: "file_too_large",
                message: format!("file upload exceeds {} bytes", self.max_bytes),
            });
        }
        let expected_digest = params
            .digest
            .map(|digest| {
                if digest.algorithm != "sha-256" {
                    return Err(ToolError::Validation("file digest must use sha-256".into()));
                }
                URL_SAFE_NO_PAD
                    .decode(&digest.value)
                    .ok()
                    .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                    .ok_or_else(|| {
                        ToolError::Validation("file digest must be a base64url SHA-256".into())
                    })
            })
            .transpose()?;
        if params
            .name
            .as_ref()
            .is_some_and(|name| name.len() > 1024 || name.chars().any(char::is_control))
            || params.mime_type.as_ref().is_some_and(|mime| {
                mime.len() > 256 || mime.parse::<axum::http::HeaderValue>().is_err()
            })
        {
            return Err(ToolError::Validation("invalid file metadata".into()));
        }
        if params.name.as_ref().is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            !ALLOWED_ARTIFACT_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix))
        }) {
            return Err(printable_workspace::WsError::UnsupportedArtifactType.into());
        }
        self.prune();
        let permit = Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| ToolError::TooManyUploads(self.max_staged))?;
        let id = random_hex_id()?;
        let token = random_hex_id()?;
        let token_hash = Sha256::digest(token.as_bytes()).into();
        let uri = format!("{URI_PREFIX}{id}");
        let mut file = serde_json::json!({"uri": uri});
        if let Some(name) = params.name {
            file["name"] = name.into();
        }
        if let Some(mime) = params.mime_type {
            file["mimeType"] = mime.into();
        }
        if let Some(size) = params.size {
            file["size"] = size.into();
        }
        if let Some(digest) = expected_digest {
            file["digest"] = serde_json::json!({"algorithm": "sha-256", "value": URL_SAFE_NO_PAD.encode(digest)});
        }
        let mut transfers = self
            .transfers
            .lock()
            .expect("incoming file registry poisoned");
        if transfers.contains_key(&id) {
            return Err(ToolError::Validation(
                "file identity collision; authorize again".into(),
            ));
        }
        let upload_url = base_url
            .join(&format!("file-transfers/upload/{id}"))
            .map_err(|_| ToolError::Validation("invalid file transfer base URL".into()))?;
        transfers.insert(
            id.clone(),
            Arc::new(Transfer {
                id,
                token_hash,
                expected_size: params.size,
                expected_digest,
                grant_expires: Instant::now() + GRANT_LIFETIME,
                state: Arc::new(tokio::sync::Mutex::new(TransferState::Prepared)),
                permit: Mutex::new(Some(permit)),
                cancellation: CancellationToken::new(),
            }),
        );
        Ok(serde_json::json!({
            "file": file,
            "upload": {
                "transport": "http", "method": "PUT",
                "url": upload_url.as_str(),
                "headers": BTreeMap::from([("Authorization", format!("Bearer {token}"))])
            },
            "lifecycle": {
                "statusMethod": "files/transferStatus",
                "cancelMethod": "files/cancelUpload"
            }
        }))
    }

    fn get(&self, uri: &str) -> Result<Arc<Transfer>, ToolError> {
        let id = uri
            .strip_prefix(URI_PREFIX)
            .ok_or(ToolError::UploadNotFound)?;
        self.prune();
        self.transfers
            .lock()
            .expect("incoming file registry poisoned")
            .get(id)
            .cloned()
            .ok_or(ToolError::UploadNotFound)
    }

    pub async fn status(&self, uri: &str) -> Result<serde_json::Value, ToolError> {
        let id = uri
            .strip_prefix(URI_PREFIX)
            .ok_or(ToolError::UploadNotFound)?;
        let transfers = match self.transfers.try_lock() {
            Ok(transfers) => transfers,
            Err(TryLockError::WouldBlock) => return Ok(serde_json::json!({"state": "unknown"})),
            Err(TryLockError::Poisoned(_)) => panic!("incoming file registry poisoned"),
        };
        let transfer = transfers.get(id).ok_or(ToolError::UploadNotFound)?;
        let Ok(state) = transfer.state.try_lock() else {
            return Ok(serde_json::json!({"state": "unknown"}));
        };
        Ok(match &*state {
            TransferState::Prepared => serde_json::json!({"state": "prepared"}),
            TransferState::Receiving => serde_json::json!({"state": "receiving"}),
            TransferState::Cancelling => serde_json::json!({"state": "cancelling"}),
            TransferState::Publishing { path } => serde_json::json!({
                "state": "publishing", "path": path,
            }),
            TransferState::Ready(received) => serde_json::json!({
                "state": "ready", "size_bytes": received.size, "sha256": received.sha256
            }),
            TransferState::Consumed => return Err(ToolError::UploadNotFound),
        })
    }

    pub fn capabilities(&self) -> serde_json::Value {
        // Global status must not wait behind publication or delay cancellation.
        let transfers = match self.transfers.try_lock() {
            Ok(transfers) => Some(transfers),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(_)) => panic!("incoming file registry poisoned"),
        };
        let active = transfers.as_ref().map(|transfers| transfers.len());
        let counts = transfers.and_then(|transfers| {
            transfers
                .values()
                .try_fold((0, 0, 0), |mut counts, transfer| {
                    let state = transfer.state.try_lock().ok()?;
                    match &*state {
                        TransferState::Receiving => counts.0 += 1,
                        TransferState::Ready(_) => counts.1 += 1,
                        TransferState::Publishing { .. } => counts.2 += 1,
                        // Cleanup can own staged bytes outside the state value.
                        TransferState::Cancelling | TransferState::Consumed => return None,
                        _ => {}
                    }
                    Some(counts)
                })
        });
        serde_json::json!({
            "max_bytes": self.max_bytes,
            "max_staged": self.max_staged,
            "available_staging_slots": self.capacity.available_permits(),
            "active_transfers": active,
            "active_receivers": counts.map(|counts| counts.0),
            "staged_files": counts.map(|counts| counts.1),
            "active_publications": counts.map(|counts| counts.2),
            "grant_lifetime_seconds": GRANT_LIFETIME.as_secs(),
            "receive_idle_timeout_seconds": IDLE_TIMEOUT.as_secs(),
            "completed_uploads_retained": false,
            "supported_suffixes": ALLOWED_ARTIFACT_SUFFIXES,
        })
    }

    pub async fn cancel(&self, uri: &str) -> Result<serde_json::Value, ToolError> {
        let transfer = self.get(uri)?;
        let Ok(mut state) = Arc::clone(&transfer.state).try_lock_owned() else {
            let state = if transfer.cancellation.is_cancelled() {
                "cancelling"
            } else {
                "unknown"
            };
            return Ok(serde_json::json!({"state": state, "cancelled": false}));
        };
        let (name, cancelled) = match &*state {
            TransferState::Ready(_) => {
                let received = std::mem::replace(&mut *state, TransferState::Cancelling);
                transfer.cancellation.cancel();
                let TransferState::Ready(received) = received else {
                    unreachable!()
                };
                drop(state);
                self.cleanup(transfer, received);
                return Ok(serde_json::json!({"state": "cancelling", "cancelled": false}));
            }
            TransferState::Prepared => {
                *state = TransferState::Consumed;
                transfer.cancellation.cancel();
                retire_transfer(&self.transfers, &transfer);
                ("cancelled", true)
            }
            TransferState::Receiving | TransferState::Cancelling => {
                *state = TransferState::Cancelling;
                transfer.cancellation.cancel();
                ("cancelling", false)
            }
            TransferState::Publishing { .. } => ("publishing", false),
            TransferState::Consumed => return Err(ToolError::UploadNotFound),
        };
        Ok(serde_json::json!({"state": name, "cancelled": cancelled}))
    }

    pub fn protocol_error(&self, error: ToolError, stage: &str) -> rmcp::ErrorData {
        let (category, reason, message, retryable) = match error.code() {
            "too_many_uploads" => (
                "temporarily_unavailable",
                "upload_capacity_exhausted",
                "incoming file staging capacity is full; retain the gateway file and retry after capacity is released",
                true,
            ),
            "upload_not_found" => (
                "file_unavailable",
                "upload_not_found",
                "file upload is no longer available; inspect the known destination before retrying",
                false,
            ),
            "unsupported_type" => (
                "invalid_file_input",
                "unsupported_type",
                "file name has an unsupported artifact suffix; upload the original supported file without splitting it",
                false,
            ),
            "file_too_large" => (
                "invalid_file_input",
                "file_too_large",
                "file exceeds the configured incoming upload size limit",
                false,
            ),
            "storage_budget_exceeded" => (
                "quota_exhausted",
                "storage_budget_exceeded",
                "workspace storage budget is exhausted",
                false,
            ),
            _ => (
                "invalid_file_input",
                "invalid_file_input",
                "invalid file upload request",
                false,
            ),
        };
        let mut data = serde_json::json!({
            "error": category, "reason": reason, "stage": stage,
            "retryable": retryable,
            "max_bytes": self.max_bytes, "capacity": self.max_staged,
        });
        if stage == "authorize_upload" {
            data["publication_attempted"] = false.into();
        }
        if reason == "upload_capacity_exhausted" {
            // Commits and cancellations can release staging promptly, so pacing
            // stays independent of the authorization lifetime.
            data["retry_after_seconds"] = 5.into();
        }
        rmcp::ErrorData::invalid_params(message, Some(data))
    }

    pub async fn ingest(&self, params: IngestParams) -> Result<IngestResult, ToolError> {
        let transfer = self
            .get(&params.file)
            .map_err(|error| ingest_error(error, None))?;
        let mut state = Arc::clone(&transfer.state).lock_owned().await;
        let expired = match &*state {
            TransferState::Ready(received) => received.expires <= Instant::now(),
            TransferState::Consumed => return Err(ingest_error(ToolError::UploadNotFound, None)),
            other => {
                let publication_attempted = if matches!(other, TransferState::Publishing { .. }) {
                    None
                } else {
                    Some(false)
                };
                return Err(ingest_error(ToolError::Validation(
                    "file is not ready; query unfinished work and inspect the known destination before retrying".into(),
                ), publication_attempted));
            }
        };
        let workspace = Arc::clone(&self.workspace);
        let max_bytes = self.max_bytes;
        let validation_error = workspace.validate_public_mutation_path(&params.path).err();
        let next = if expired || validation_error.is_some() {
            TransferState::Cancelling
        } else {
            TransferState::Publishing {
                path: params.path.clone(),
            }
        };
        let TransferState::Ready(received) = std::mem::replace(&mut *state, next) else {
            unreachable!()
        };
        drop(state);
        let transfers = Arc::clone(&self.transfers);
        tokio::task::spawn_blocking(move || {
            let _completion = WorkCompletion {
                transfers,
                transfer,
            };
            let received = received;
            if expired {
                return Err(ingest_error(ToolError::UploadNotFound, None));
            }
            if let Some(error) = validation_error {
                return Err(ingest_error(error.into(), Some(false)));
            }
            let artifact = workspace
                .commit_generated_artifact_bounded(
                    &params.path,
                    received.file.path(),
                    params.overwrite,
                    max_bytes,
                )
                .map_err(|error| {
                    // These typed validation, admission and destination rejections
                    // precede publication. Generic I/O can follow publication or sync.
                    let publication_attempted = !matches!(
                        error,
                        WsError::Unconfined
                            | WsError::PathEscapes
                            | WsError::ReservedPath
                            | WsError::SymlinkRefused
                            | WsError::RefusingSymlinkReplace
                            | WsError::UnsupportedArtifactType
                            | WsError::NotRegularFile
                            | WsError::AlreadyExists(_)
                            | WsError::NotFound(_)
                            | WsError::WriteTooLarge
                            | WsError::ChangedWhileReading
                            | WsError::StorageBudgetExceeded
                            | WsError::StorageAccountingIncomplete
                            | WsError::InvalidScratchId
                    );
                    ingest_error(error.into(), Some(publication_attempted))
                })?;
            Ok(IngestResult {
                artifact,
                sha256: received.sha256.clone(),
            })
        })
        .await
        .map_err(|error| ingest_error(ToolError::Io(std::io::Error::other(error)), None))?
    }

    fn prune(&self) {
        let expired = {
            let transfers = self
                .transfers
                .lock()
                .expect("incoming file registry poisoned");
            let now = Instant::now();
            let mut expired = Vec::new();
            for transfer in transfers.values() {
                let Ok(mut state) = transfer.state.try_lock() else {
                    continue;
                };
                match &*state {
                    TransferState::Prepared if transfer.grant_expires <= now => {
                        *state = TransferState::Consumed;
                        expired.push((Arc::clone(transfer), None));
                    }
                    TransferState::Ready(received) if received.expires <= now => {
                        let TransferState::Ready(received) =
                            std::mem::replace(&mut *state, TransferState::Cancelling)
                        else {
                            unreachable!()
                        };
                        transfer.cancellation.cancel();
                        expired.push((Arc::clone(transfer), Some(received)));
                    }
                    _ => {}
                }
            }
            expired
        };
        for (transfer, received) in expired {
            if let Some(received) = received {
                self.cleanup(transfer, received);
            } else {
                retire_transfer(&self.transfers, &transfer);
            }
        }
    }

    fn cleanup(&self, transfer: Arc<Transfer>, received: Received) {
        let transfers = Arc::clone(&self.transfers);
        let cleanup = move || {
            let _completion = WorkCompletion {
                transfers,
                transfer,
            };
            // Bind after the completion guard so scratch cleanup also precedes
            // retirement when a blocking operation unwinds.
            let received = received;
            drop(received);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let task = runtime.spawn_blocking(cleanup);
                runtime.spawn(async move {
                    if task.await.is_err() {
                        tracing::error!("incoming file cleanup task failed");
                    }
                });
            }
            // Synchronous callers have no async executor to protect. The
            // registry lock has already been released before cleanup begins.
            Err(_) => cleanup(),
        }
    }

    pub fn spawn_reaper(self: &Arc<Self>, cancel: CancellationToken) {
        let files = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => files.prune(),
                }
            }
        });
    }
}

pub fn ingest_error(source: ToolError, publication_attempted: Option<bool>) -> ToolError {
    ToolError::FileIngest {
        source: Box::new(source),
        publication_attempted,
    }
}

pub async fn upload(
    State(files): State<Arc<IncomingFiles>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(transfer) = files.get(&format!("{URI_PREFIX}{id}")) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let rejection = if !bool::from(hash.ct_eq(&transfer.token_hash)) {
        Some(StatusCode::UNAUTHORIZED)
    } else {
        let mut state = transfer.state.lock().await;
        if !matches!(*state, TransferState::Prepared) {
            Some(StatusCode::CONFLICT)
        } else if transfer.grant_expires <= Instant::now() {
            *state = TransferState::Consumed;
            retire_transfer(&files.transfers, &transfer);
            Some(StatusCode::UNAUTHORIZED)
        } else {
            *state = TransferState::Receiving;
            None
        }
    };
    if let Some(status) = rejection {
        // A rejected concurrent request may own the last transfer reference.
        return tokio::task::spawn_blocking(move || {
            drop(transfer);
            status.into_response()
        })
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    tokio::spawn(async move {
        let result = receive(body, &transfer, files.max_bytes, Arc::clone(&files.workspace)).await;
        let mut state = Arc::clone(&transfer.state).lock_owned().await;
        // Finalization can own the last staging reference after cancellation.
        // Resource destruction and terminal retirement belong on the blocking pool.
        tokio::task::spawn_blocking(move || {
            let cancelled = transfer.cancellation.is_cancelled();
            match result {
                Ok(received) if !cancelled => {
                    let response = serde_json::json!({"state": "ready", "size_bytes": received.size, "sha256": received.sha256});
                    *state = TransferState::Ready(received);
                    (StatusCode::CREATED, Json(response)).into_response()
                }
                result => {
                    *state = TransferState::Cancelling;
                    drop(state);
                    let _completion = WorkCompletion {
                        transfers: Arc::clone(&files.transfers), transfer,
                    };
                    let result = result;
                    let status = if cancelled { StatusCode::GONE } else {
                        match &result { Err(status) => *status, Ok(_) => unreachable!() }
                    };
                    drop(result);
                    status.into_response()
                }
            }
        }).await.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }).await.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn receive(
    mut body: Body,
    transfer: &Transfer,
    max_bytes: u64,
    workspace: Arc<Workspace>,
) -> Result<Received, StatusCode> {
    let reservation = transfer.expected_size.unwrap_or(max_bytes);
    let (staged, mut writer) = tokio::task::spawn_blocking(move || {
        let scratch = workspace
            .scratch(reservation, "native_upload")
            .map_err(|error| {
                if matches!(error, printable_workspace::WsError::StorageBudgetExceeded) {
                    StatusCode::INSUFFICIENT_STORAGE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            })?;
        let file = tempfile::NamedTempFile::new_in(scratch.path())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let writer = file
            .reopen()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok::<_, StatusCode>((
            Arc::new(StagedUpload {
                file,
                _scratch: scratch,
            }),
            writer,
        ))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    let result = async {
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        loop {
            let frame = tokio::select! {
                _ = transfer.cancellation.cancelled() => return Err(StatusCode::GONE),
                frame = tokio::time::timeout(
                    IDLE_TIMEOUT,
                    std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
                ) => frame.map_err(|_| StatusCode::REQUEST_TIMEOUT)?,
            };
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
            if let Ok(bytes) = frame.into_data() {
                size = size
                    .checked_add(bytes.len() as u64)
                    .ok_or(StatusCode::PAYLOAD_TOO_LARGE)?;
                if size > max_bytes
                    || transfer
                        .expected_size
                        .is_some_and(|expected| size > expected)
                {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                digest.update(&bytes);
                let owner = Arc::clone(&staged);
                writer = tokio::task::spawn_blocking(move || {
                    let _owner = owner;
                    writer.write_all(&bytes)?;
                    Ok::<_, std::io::Error>(writer)
                })
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            }
        }
        let hash: [u8; 32] = digest.finalize().into();
        if transfer
            .expected_size
            .is_some_and(|expected| size != expected)
            || transfer
                .expected_digest
                .is_some_and(|expected| expected != hash)
        {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        let owner = Arc::clone(&staged);
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            writer.sync_all()
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok((size, hash))
    }
    .await;
    match result {
        Ok((size, hash)) => Ok(Received {
            file: staged,
            size,
            sha256: URL_SAFE_NO_PAD.encode(hash),
            expires: Instant::now() + GRANT_LIFETIME,
        }),
        Err(status) => {
            tokio::task::spawn_blocking(move || drop(staged))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Err(status)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_storage(
        root: &std::path::Path,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        use std::os::fd::AsRawFd;

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(".printable/storage/controller.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let (release, released) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            // A separate thread releases the test lock even if an executor
            // regression prevents the async test from making progress.
            let _ = released.recv_timeout(Duration::from_secs(10));
            drop(lock);
        });
        (release, blocker)
    }

    async fn wait_for_cleanup(files: &IncomingFiles) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !files.transfers.lock().unwrap().is_empty()
                || files.capacity.available_permits() != files.max_staged
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unfinished transfer cleanup completes");
    }

    fn authorize(files: &IncomingFiles, name: &str) -> serde_json::Value {
        files
            .authorize(
                serde_json::from_value(serde_json::json!({"name":name,"size":3})).unwrap(),
                &reqwest::Url::parse("http://localhost/").unwrap(),
            )
            .unwrap()
    }

    async fn receive_test_file(
        files: Arc<IncomingFiles>,
        authorization: &serde_json::Value,
        body: Body,
    ) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            authorization["upload"]["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
        upload(
            State(files),
            Path(
                authorization["file"]["uri"]
                    .as_str()
                    .unwrap()
                    .strip_prefix(URI_PREFIX)
                    .unwrap()
                    .to_owned(),
            ),
            headers,
            body,
        )
        .await
    }

    #[tokio::test]
    async fn configured_capacity_can_be_reclaimed_without_expiry() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::with_capacity(workspace, 1024, 3);
        let authorizations = (0..3)
            .map(|_| authorize(&files, "model.3mf"))
            .collect::<Vec<_>>();
        let error = files
            .authorize(
                serde_json::from_value(serde_json::json!({"name":"model.3mf"})).unwrap(),
                &reqwest::Url::parse("http://localhost/").unwrap(),
            )
            .unwrap_err();
        let error = files.protocol_error(error, "authorize_upload");
        let data = error.data.unwrap();
        assert_eq!(data["error"], "temporarily_unavailable");
        assert_eq!(data["reason"], "upload_capacity_exhausted");
        assert_eq!(data["capacity"], 3);
        assert_eq!(data["publication_attempted"], false);
        let retry_after = data["retry_after_seconds"].as_u64().unwrap();
        assert!(retry_after > 0 && retry_after <= 30);
        let uri = authorizations[0]["file"]["uri"].as_str().unwrap();
        let status = files.status(uri).await.unwrap();
        assert_eq!(status["state"], "prepared");
        assert_eq!(files.cancel(uri).await.unwrap()["cancelled"], true);
        assert!(files.cancel(uri).await.is_err());
        assert!(files.status(uri).await.is_err());
        let contract = crate::resources::contracts::read(
            "printable://contracts/artifact/transfer_status",
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        let validator = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
        assert!(validator.is_valid(&status));
        authorize(&files, "replacement.3mf");
        assert_eq!(files.capabilities()["available_staging_slots"], 0);
    }

    #[tokio::test]
    async fn capacity_snapshot_does_not_wait_for_publication_or_registry_mutation() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::with_capacity(workspace, 1024, 3);
        let authorization = authorize(&files, "source.3mf");
        let transfer = files
            .get(authorization["file"]["uri"].as_str().unwrap())
            .unwrap();
        let state = transfer.state.lock().await;
        let snapshot = files.capabilities();
        assert_eq!(snapshot["available_staging_slots"], 2);
        assert!(snapshot["active_receivers"].is_null());
        assert!(snapshot["staged_files"].is_null());
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            files.status(authorization["file"]["uri"].as_str().unwrap()),
        )
        .await
        .expect("status does not wait for publication")
        .unwrap();
        assert_eq!(status["state"], "unknown");
        let cancellation = tokio::time::timeout(
            Duration::from_secs(5),
            files.cancel(authorization["file"]["uri"].as_str().unwrap()),
        )
        .await
        .expect("busy cancellation does not wait for publication")
        .unwrap();
        assert_eq!(cancellation["state"], "unknown");
        assert_eq!(cancellation["cancelled"], false);
        assert!(!transfer.cancellation.is_cancelled());
        let contract = crate::resources::contracts::read(
            "printable://contracts/artifact/transfer_status",
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        let validator = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
        assert!(validator.is_valid(&status));
        drop(state);
        assert_eq!(files.capabilities()["staged_files"], 0);
        let registry = files.transfers.lock().unwrap();
        let snapshot = files.capabilities();
        assert_eq!(snapshot["available_staging_slots"], 2);
        assert!(snapshot["active_receivers"].is_null());
        assert!(snapshot["staged_files"].is_null());
        let status = futures_util::FutureExt::now_or_never(
            files.status(authorization["file"]["uri"].as_str().unwrap()),
        )
        .expect("registry contention does not suspend status")
        .unwrap();
        assert_eq!(status["state"], "unknown");
        drop(registry);
    }

    #[tokio::test]
    async fn cancelled_transfers_retire_tracking_and_release_capacity() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::with_capacity(workspace, 1024, 1);
        for _ in 0..160 {
            let authorization = authorize(&files, "source.3mf");
            let uri = authorization["file"]["uri"].as_str().unwrap();
            assert_eq!(files.cancel(uri).await.unwrap()["cancelled"], true);
            assert!(files.transfers.lock().unwrap().is_empty());
            assert!(files.status(uri).await.is_err());
        }
        assert_eq!(files.capacity.available_permits(), 1);
        assert!(files.transfers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn more_than_128_successful_imports_leave_no_completed_uploads() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::with_capacity(
            Arc::clone(&workspace),
            1024,
            1,
        ));
        let expected_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(b"abc"));
        for index in 0..160 {
            let authorization = authorize(&files, "source.3mf");
            let uri = authorization["file"]["uri"].as_str().unwrap();
            assert_eq!(
                receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                    .await
                    .status(),
                StatusCode::CREATED
            );
            // The byte grant is already consumed before artifact publication.
            assert_eq!(
                receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                    .await
                    .status(),
                StatusCode::CONFLICT
            );
            let path = format!("model-{index}.3mf");
            let params = IngestParams {
                file: uri.into(),
                path: path.clone(),
                overwrite: false,
            };
            let result = files.ingest(params.clone()).await.unwrap();
            assert_eq!(result.artifact.size_bytes, 3);
            assert_eq!(result.sha256, expected_hash);
            assert_eq!(std::fs::read(root.path().join(&path)).unwrap(), b"abc");
            assert!(files.transfers.lock().unwrap().is_empty());
            assert_eq!(files.capacity.available_permits(), 1);
            assert!(files.status(uri).await.is_err());
            assert!(files.cancel(uri).await.is_err());
            // A lost response is recovered from the current artifact, not replay.
            let error = files.ingest(params).await.err().unwrap();
            assert!(matches!(
                error,
                ToolError::FileIngest {
                    publication_attempted: None,
                    ..
                }
            ));
            assert_eq!(workspace.stat_artifact(&path).unwrap().size_bytes, 3);
            assert_eq!(
                receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let snapshot = files.capabilities();
        assert_eq!(snapshot["active_transfers"], 0);
        assert_eq!(snapshot["completed_uploads_retained"], false);
        assert!(snapshot.get("max_receipts").is_none());
    }

    #[tokio::test]
    async fn lost_ingest_response_and_expired_grant_do_not_hide_in_flight_publication() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        workspace
            .configure_storage_budget(Some(1024 * 1024))
            .unwrap();
        let files = Arc::new(IncomingFiles::with_capacity(
            Arc::clone(&workspace),
            1024,
            1,
        ));
        let authorization = authorize(&files, "source.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap().to_owned();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        {
            let mut transfers = files.transfers.lock().unwrap();
            Arc::get_mut(
                transfers
                    .get_mut(uri.strip_prefix(URI_PREFIX).unwrap())
                    .unwrap(),
            )
            .unwrap()
            .grant_expires = Instant::now();
        }
        let (release, blocker) = block_storage(root.path());
        let ingest_files = Arc::clone(&files);
        let params = IngestParams {
            file: uri.clone(),
            path: "known.3mf".into(),
            overwrite: false,
        };
        let request_params = params.clone();
        let request = tokio::spawn(async move { ingest_files.ingest(request_params).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while files.status(&uri).await.unwrap()["state"] != "publishing" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publication starts");
        request.abort();
        assert!(matches!(request.await, Err(error) if error.is_cancelled()));
        files.prune();
        let status = files.status(&uri).await.unwrap();
        assert_eq!(status["state"], "publishing");
        assert_eq!(status["path"], "known.3mf");
        let contract = crate::resources::contracts::read(
            "printable://contracts/artifact/transfer_status",
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        assert!(
            jsonschema::validator_for(&contract["outputSchema"])
                .unwrap()
                .is_valid(&status)
        );
        assert_eq!(files.capabilities()["active_publications"], 1);
        assert_eq!(files.capacity.available_permits(), 0);
        let cancellation = files.cancel(&uri).await.unwrap();
        assert_eq!(cancellation["state"], "publishing");
        assert_eq!(cancellation["cancelled"], false);
        let error = files.ingest(params.clone()).await.err().unwrap();
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: None,
                ..
            }
        ));
        // Absence while the worker is blocked does not establish failure.
        assert!(!root.path().join("known.3mf").exists());
        release.send(()).unwrap();
        wait_for_cleanup(&files).await;
        blocker.join().unwrap();
        assert_eq!(files.capacity.available_permits(), 1);
        assert!(files.status(&uri).await.is_err());
        assert!(files.cancel(&uri).await.is_err());
        let current = std::fs::read(root.path().join("known.3mf")).unwrap();
        assert_eq!(current, b"abc");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(&current)),
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"abc"))
        );
        std::fs::write(root.path().join("known.3mf"), b"later").unwrap();
        let error = files.ingest(params).await.err().unwrap();
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: None,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(root.path().join("known.3mf")).unwrap(),
            b"later"
        );
    }

    #[tokio::test]
    async fn quota_refusals_retire_tracking_and_recover_after_budget_is_freed() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::with_capacity(
            Arc::clone(&workspace),
            1024,
            1,
        ));
        for _ in 0..160 {
            workspace.configure_storage_budget(None).unwrap();
            let authorization = authorize(&files, "source.3mf");
            let uri = authorization["file"]["uri"].as_str().unwrap();
            assert_eq!(
                receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                    .await
                    .status(),
                StatusCode::CREATED
            );
            workspace.configure_storage_budget(Some(0)).unwrap();
            let error = files
                .ingest(IngestParams {
                    file: uri.into(),
                    path: "model.3mf".into(),
                    overwrite: false,
                })
                .await
                .err()
                .unwrap();
            assert_eq!(error.code(), "storage_budget_exceeded");
            assert!(matches!(
                error,
                ToolError::FileIngest {
                    publication_attempted: Some(false),
                    ..
                }
            ));
            assert!(files.status(uri).await.is_err());
            assert!(files.transfers.lock().unwrap().is_empty());
            assert_eq!(files.capacity.available_permits(), 1);
            assert!(!root.path().join("model.3mf").exists());
        }
        assert!(files.transfers.lock().unwrap().is_empty());
        workspace.configure_storage_budget(None).unwrap();
        let authorization = authorize(&files, "replacement.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        files
            .ingest(IngestParams {
                file: uri.into(),
                path: "model.3mf".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("model.3mf")).unwrap(),
            b"abc"
        );
        assert!(files.status(uri).await.is_err());
        assert!(files.transfers.lock().unwrap().is_empty());
        assert_eq!(files.capacity.available_permits(), 1);
    }

    #[tokio::test]
    async fn destination_conflicts_retire_tracking_without_replacing_existing_bytes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::with_capacity(workspace, 1024, 1));
        std::fs::write(root.path().join("existing.3mf"), b"existing").unwrap();
        for _ in 0..160 {
            let authorization = authorize(&files, "source.3mf");
            let uri = authorization["file"]["uri"].as_str().unwrap();
            assert_eq!(
                receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                    .await
                    .status(),
                StatusCode::CREATED
            );
            let error = files
                .ingest(IngestParams {
                    file: uri.into(),
                    path: "existing.3mf".into(),
                    overwrite: false,
                })
                .await
                .err()
                .unwrap();
            assert_eq!(error.code(), "already_exists");
            assert!(matches!(
                error,
                ToolError::FileIngest {
                    publication_attempted: Some(false),
                    ..
                }
            ));
            assert!(files.status(uri).await.is_err());
            assert!(files.transfers.lock().unwrap().is_empty());
            assert_eq!(files.capacity.available_permits(), 1);
            assert_eq!(
                std::fs::read(root.path().join("existing.3mf")).unwrap(),
                b"existing"
            );
        }
        assert!(files.transfers.lock().unwrap().is_empty());
        let authorization = authorize(&files, "replacement.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        files
            .ingest(IngestParams {
                file: uri.into(),
                path: "replacement.3mf".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("replacement.3mf")).unwrap(),
            b"abc"
        );
        assert!(files.status(uri).await.is_err());
        assert!(files.transfers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_names_and_destinations_do_not_strand_capacity() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::with_capacity(workspace, 1024, 1));
        let error = files
            .authorize(
                serde_json::from_value(serde_json::json!({"name":"split.part"})).unwrap(),
                &reqwest::Url::parse("http://localhost/").unwrap(),
            )
            .unwrap_err();
        assert_eq!(error.code(), "unsupported_type");
        assert_eq!(files.capacity.available_permits(), 1);
        let authorization = authorize(&files, "SOURCE.3MF");
        let uri = authorization["file"]["uri"].as_str().unwrap();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(files.capabilities()["staged_files"], 1);
        let error = files
            .ingest(IngestParams {
                file: uri.into(),
                path: "split.part".into(),
                overwrite: false,
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.code(), "unsupported_type");
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: Some(false),
                ..
            }
        ));
        assert!(files.status(uri).await.is_err());
        assert!(files.transfers.lock().unwrap().is_empty());
        assert_eq!(files.capacity.available_permits(), 1);
        authorize(&files, "replacement.3mf");
    }

    #[tokio::test]
    async fn staged_cancellation_keeps_status_responsive_while_storage_is_locked() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        workspace
            .configure_storage_budget(Some(1024 * 1024))
            .unwrap();
        let files = Arc::new(IncomingFiles::with_capacity(workspace, 1024, 1));
        let authorization = authorize(&files, "source.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap().to_owned();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        let (release, blocker) = block_storage(root.path());
        let started = Instant::now();
        let cancellation = tokio::time::timeout(Duration::from_secs(5), files.cancel(&uri))
            .await
            .expect("cancellation releases control admission before storage cleanup")
            .unwrap();
        assert_eq!(cancellation["state"], "cancelling");
        assert_eq!(cancellation["cancelled"], false);
        let close_admission = CancellationToken::new();
        let admission = crate::admission::Admission::new(close_admission.clone());
        let request = serde_json::json!({"method":"files/cancelUpload"});
        let repeated = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::future::join_all((0..16).map(|_| async {
                let _permit = admission
                    .request(&request)
                    .expect("cancellation leaves control admission available");
                files.cancel(&uri).await
            })),
        )
        .await
        .expect("repeated cancellations do not occupy the control lane");
        for response in repeated {
            let response = response.unwrap();
            assert_eq!(response["state"], "cancelling");
            assert_eq!(response["cancelled"], false);
        }
        close_admission.cancel();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(files.capabilities()["available_staging_slots"], 0);
        assert_eq!(files.status(&uri).await.unwrap()["state"], "cancelling");
        assert_eq!(files.capabilities()["active_transfers"], 1);
        release.send(()).unwrap();
        wait_for_cleanup(&files).await;
        assert!(files.cancel(&uri).await.is_err());
        assert!(files.status(&uri).await.is_err());
        assert_eq!(files.capacity.available_permits(), 1);
        blocker.join().unwrap();
    }

    #[tokio::test]
    async fn cancelling_an_active_receive_stops_it_and_releases_capacity() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        workspace
            .configure_storage_budget(Some(1024 * 1024))
            .unwrap();
        let files = Arc::new(IncomingFiles::with_capacity(workspace, 1024, 1));
        let authorization = authorize(&files, "source.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap().to_owned();
        let grant_deadline = Instant::now() + Duration::from_secs(1);
        {
            let mut transfers = files.transfers.lock().unwrap();
            Arc::get_mut(
                transfers
                    .get_mut(uri.strip_prefix(URI_PREFIX).unwrap())
                    .unwrap(),
            )
            .unwrap()
            .grant_expires = grant_deadline;
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = Arc::clone(&started);
        let stream = futures_util::stream::poll_fn(move |_| {
            signal.notify_one();
            std::task::Poll::Pending::<Option<Result<axum::body::Bytes, std::io::Error>>>
        });
        let receiver_files = Arc::clone(&files);
        let receiver = tokio::spawn(async move {
            receive_test_file(receiver_files, &authorization, Body::from_stream(stream)).await
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("receiver starts polling the body");
        // Advance to the exact grant deadline after the real HTTP receive has
        // started. Expiry must not hide its admitted I/O or free its slot.
        tokio::time::sleep_until(tokio::time::Instant::from_std(grant_deadline)).await;
        files.prune();
        assert_eq!(files.status(&uri).await.unwrap()["state"], "receiving");
        assert_eq!(files.capabilities()["active_receivers"], 1);
        assert_eq!(files.capacity.available_permits(), 0);
        let (release, blocker) = block_storage(root.path());
        let started = Instant::now();
        let cancellation = files.cancel(&uri).await.unwrap();
        assert_eq!(cancellation["state"], "cancelling");
        tokio::task::yield_now().await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!receiver.is_finished());
        assert_eq!(files.status(&uri).await.unwrap()["state"], "cancelling");
        assert_eq!(files.capabilities()["available_staging_slots"], 0);
        let contract = crate::resources::contracts::read(
            "printable://contracts/artifact/transfer_status",
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        let validator = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
        assert!(validator.is_valid(&serde_json::json!({"state": cancellation["state"]})));
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .expect("cancelled receiver exits")
                .unwrap()
                .status(),
            StatusCode::GONE
        );
        assert!(files.status(&uri).await.is_err());
        assert_eq!(files.capacity.available_permits(), 1);
        blocker.join().unwrap();
        authorize(&files, "replacement.3mf");
    }

    #[tokio::test]
    async fn expiry_retires_staging_without_blocking_status_or_freeing_its_slot_early() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        workspace
            .configure_storage_budget(Some(1024 * 1024))
            .unwrap();
        let files = Arc::new(IncomingFiles::with_capacity(workspace, 1024, 1));
        let authorization = authorize(&files, "source.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap();
        assert_eq!(
            receive_test_file(Arc::clone(&files), &authorization, Body::from("abc"))
                .await
                .status(),
            StatusCode::CREATED
        );
        {
            let mut transfers = files.transfers.lock().unwrap();
            let transfer = transfers
                .get_mut(uri.strip_prefix(URI_PREFIX).unwrap())
                .unwrap();
            let mut state = transfer.state.try_lock().unwrap();
            let TransferState::Ready(received) = &mut *state else {
                panic!("file is staged")
            };
            received.expires = Instant::now();
        }
        let (release, blocker) = block_storage(root.path());
        let started = Instant::now();
        files.prune();
        assert!(started.elapsed() < Duration::from_secs(5));
        let snapshot = files.capabilities();
        assert_eq!(snapshot["available_staging_slots"], 0);
        assert_eq!(snapshot["active_transfers"], 1);
        assert!(snapshot["active_receivers"].is_null());
        assert!(snapshot["staged_files"].is_null());
        assert_eq!(files.status(uri).await.unwrap()["state"], "cancelling");
        release.send(()).unwrap();
        wait_for_cleanup(&files).await;
        assert_eq!(files.capabilities()["available_staging_slots"], 1);
        assert_eq!(files.capabilities()["staged_files"], 0);
        blocker.join().unwrap();
        authorize(&files, "replacement.3mf");
    }

    #[test]
    fn synchronous_authorization_can_reclaim_expired_prepared_transfers() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::with_capacity(workspace, 1024, 1);
        let authorization = authorize(&files, "source.3mf");
        let uri = authorization["file"]["uri"].as_str().unwrap();
        {
            let mut transfers = files.transfers.lock().unwrap();
            let transfer = transfers
                .get_mut(uri.strip_prefix(URI_PREFIX).unwrap())
                .unwrap();
            Arc::get_mut(transfer).unwrap().grant_expires = Instant::now();
        }
        authorize(&files, "replacement.3mf");
        assert_eq!(files.transfers.lock().unwrap().len(), 1);
        assert_eq!(files.capacity.available_permits(), 0);
    }

    #[tokio::test]
    async fn publication_error_retires_tracking_without_overwriting_later_changes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::new(workspace, 1024));
        let authorization = files
            .authorize(
                serde_json::from_value(serde_json::json!({"size": 3})).unwrap(),
                &reqwest::Url::parse("http://localhost/").unwrap(),
            )
            .unwrap();
        let uri = authorization["file"]["uri"].as_str().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            authorization["upload"]["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
        assert_eq!(
            upload(
                State(Arc::clone(&files)),
                Path(uri.strip_prefix(URI_PREFIX).unwrap().to_owned()),
                headers,
                Body::from("old")
            )
            .await
            .status(),
            StatusCode::CREATED
        );

        // Force a filesystem publication error, then model a later destination edit.
        let destination = root.path().join("result.stl");
        std::fs::create_dir(&destination).unwrap();
        let params = IngestParams {
            file: uri.into(),
            path: "result.stl".into(),
            overwrite: true,
        };
        let error = files.ingest(params.clone()).await.err().unwrap();
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: Some(true),
                ..
            }
        ));
        assert!(files.status(uri).await.is_err());
        assert!(files.cancel(uri).await.is_err());
        assert!(files.transfers.lock().unwrap().is_empty());
        assert_eq!(files.capacity.available_permits(), MAX_TRANSFERS);
        std::fs::remove_dir(&destination).unwrap();
        std::fs::write(&destination, b"later edit").unwrap();
        let error = files.ingest(params).await.err().unwrap();
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: None,
                ..
            }
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), b"later edit");
    }

    #[tokio::test]
    async fn missing_ingest_handle_does_not_claim_a_publication_outcome() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::new(workspace, 1024);
        let error = files
            .ingest(IngestParams {
                file: format!("{URI_PREFIX}missing"),
                path: "model.3mf".into(),
                overwrite: false,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ToolError::FileIngest {
                publication_attempted: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn partial_oversized_and_expired_transfers_cannot_publish() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::new(workspace, 4));
        for (declared, body, expected) in [
            (4, "xx", StatusCode::UNPROCESSABLE_ENTITY),
            (3, "xxxx", StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let authorization = files
                .authorize(
                    serde_json::from_value(serde_json::json!({"size":declared})).unwrap(),
                    &reqwest::Url::parse("http://localhost/").unwrap(),
                )
                .unwrap();
            let uri = authorization["file"]["uri"].as_str().unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                authorization["upload"]["headers"]["Authorization"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
            let result = upload(
                State(Arc::clone(&files)),
                Path(uri.strip_prefix(URI_PREFIX).unwrap().to_owned()),
                headers,
                Body::from(body),
            )
            .await;
            assert_eq!(result.status(), expected);
            assert!(
                files
                    .ingest(IngestParams {
                        file: uri.into(),
                        path: "bad.step".into(),
                        overwrite: false
                    })
                    .await
                    .is_err()
            );
            assert!(!root.path().join("bad.step").exists());
        }
        let authorization = files
            .authorize(
                serde_json::from_value(serde_json::json!({})).unwrap(),
                &reqwest::Url::parse("http://localhost/").unwrap(),
            )
            .unwrap();
        let uri = authorization["file"]["uri"].as_str().unwrap();
        assert!(
            files
                .ingest(IngestParams {
                    file: uri.into(),
                    path: "../escape.step".into(),
                    overwrite: false
                })
                .await
                .is_err()
        );
        {
            let mut transfers = files.transfers.lock().unwrap();
            let transfer = transfers
                .get_mut(uri.strip_prefix(URI_PREFIX).unwrap())
                .unwrap();
            Arc::get_mut(transfer).unwrap().grant_expires = Instant::now();
        }
        files.prune();
        assert!(files.status(uri).await.is_err());
        wait_for_cleanup(&files).await;
        assert_eq!(files.capacity.available_permits(), MAX_TRANSFERS);
        assert!(
            files
                .authorize(
                    serde_json::from_value(serde_json::json!({"size":5})).unwrap(),
                    &reqwest::Url::parse("http://localhost/").unwrap()
                )
                .is_err()
        );
    }
}
