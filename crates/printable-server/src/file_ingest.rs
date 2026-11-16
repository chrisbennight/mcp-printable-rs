//! Native raw-byte uploads consumed by workspace artifact operations.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
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
use printable_workspace::{ALLOWED_ARTIFACT_SUFFIXES, ArtifactMeta, Workspace};
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
const MAX_RECEIPTS: usize = 128;
const RETENTION: Duration = Duration::from_secs(3600);
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
    Cancelled,
    Ready(Received),
    Failed,
    CommitUncertain { path: String },
    Committed(IngestParams, IngestResult),
}

struct Transfer {
    token_hash: [u8; 32],
    expected_size: Option<u64>,
    expected_digest: Option<[u8; 32]>,
    expires: Instant,
    state: Arc<tokio::sync::Mutex<TransferState>>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    cancellation: CancellationToken,
}

pub struct IncomingFiles {
    workspace: Arc<Workspace>,
    max_bytes: u64,
    max_staged: usize,
    transfers: Mutex<HashMap<String, Arc<Transfer>>>,
    capacity: Arc<Semaphore>,
    // Only expiry batches are counted here; admission uses the semaphore.
    pending_cleanup: Arc<AtomicUsize>,
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
            transfers: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(max_staged)),
            pending_cleanup: Arc::new(AtomicUsize::new(0)),
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
        let max_receipts = MAX_RECEIPTS.max(self.max_staged);
        if transfers.len() >= max_receipts {
            transfers.retain(|_, transfer| {
                Arc::strong_count(transfer) != 1
                    || !transfer.state.try_lock().is_ok_and(|state| {
                        matches!(*state, TransferState::Failed | TransferState::Cancelled)
                    })
            });
        }
        if transfers.len() >= max_receipts {
            return Err(ToolError::FileUpload {
                code: "upload_receipt_capacity",
                message: "incoming file receipt capacity is full; wait for receipt expiry".into(),
            });
        }
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
                token_hash,
                expected_size: params.size,
                expected_digest,
                expires: Instant::now() + RETENTION,
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
        let transfer = transfers
            .get(id)
            .filter(|transfer| transfer.expires > Instant::now())
            .ok_or(ToolError::UploadNotFound)?;
        let Ok(state) = transfer.state.try_lock() else {
            return Ok(serde_json::json!({"state": "unknown"}));
        };
        Ok(match &*state {
            TransferState::Prepared => serde_json::json!({"state": "prepared"}),
            TransferState::Receiving => serde_json::json!({"state": "receiving"}),
            TransferState::Cancelling => serde_json::json!({"state": "cancelling"}),
            TransferState::Cancelled => serde_json::json!({"state": "cancelled"}),
            TransferState::Failed => serde_json::json!({"state": "failed"}),
            TransferState::CommitUncertain { path } => serde_json::json!({
                "state": "commit_uncertain", "path": path,
                "message": "Inspect the destination before authorizing another transfer; this receipt cannot be retried."
            }),
            TransferState::Ready(received) => serde_json::json!({
                "state": "ready", "size_bytes": received.size, "sha256": received.sha256
            }),
            TransferState::Committed(_, result) => {
                serde_json::json!({"state": "committed", "artifact": result})
            }
        })
    }

    pub fn capabilities(&self) -> serde_json::Value {
        // Global status must not wait behind publication or delay cancellation.
        let transfers = match self.transfers.try_lock() {
            Ok(transfers) => Some(transfers),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(_)) => panic!("incoming file registry poisoned"),
        };
        let counts = transfers.and_then(|transfers| {
            if self.pending_cleanup.load(Ordering::Acquire) != 0 {
                return None;
            }
            transfers.values().try_fold((0, 0), |mut counts, transfer| {
                let state = transfer.state.try_lock().ok()?;
                match &*state {
                    TransferState::Receiving | TransferState::Cancelling => counts.0 += 1,
                    TransferState::Ready(_) => counts.1 += 1,
                    _ => {}
                }
                Some(counts)
            })
        });
        serde_json::json!({
            "max_bytes": self.max_bytes,
            "max_staged": self.max_staged,
            "available_staging_slots": self.capacity.available_permits(),
            "active_receivers": counts.map(|counts| counts.0),
            "staged_files": counts.map(|counts| counts.1),
            "lifetime_seconds": RETENTION.as_secs(),
            "max_receipts": MAX_RECEIPTS.max(self.max_staged),
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
                // Managed scratch cleanup can wait for a publication's storage
                // lock. Keep its state and slot owned until that cleanup exits.
                let cleanup = tokio::task::spawn_blocking(move || {
                    drop(received);
                    transfer
                        .permit
                        .lock()
                        .expect("upload permit poisoned")
                        .take();
                    *state = TransferState::Cancelled;
                });
                // The control request must release admission while cleanup
                // waits; the cleanup task still owns the state and slot.
                tokio::spawn(async move {
                    if cleanup.await.is_err() {
                        tracing::error!("incoming file cancellation cleanup task failed");
                    }
                });
                return Ok(serde_json::json!({"state": "cancelling", "cancelled": false}));
            }
            TransferState::Prepared => {
                *state = TransferState::Cancelled;
                transfer.cancellation.cancel();
                transfer
                    .permit
                    .lock()
                    .expect("upload permit poisoned")
                    .take();
                ("cancelled", true)
            }
            TransferState::Receiving | TransferState::Cancelling => {
                *state = TransferState::Cancelling;
                transfer.cancellation.cancel();
                ("cancelling", false)
            }
            TransferState::Cancelled => ("cancelled", true),
            TransferState::Failed => ("failed", false),
            TransferState::CommitUncertain { .. } => ("commit_uncertain", false),
            TransferState::Committed(..) => ("committed", false),
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
            "upload_receipt_capacity" => (
                "temporarily_unavailable",
                "receipt_capacity",
                "incoming file receipt capacity is full; retain the gateway file and retry after receipt expiry",
                true,
            ),
            "upload_not_found" => (
                "file_unavailable",
                "upload_not_found",
                "file upload is unavailable or expired",
                false,
            ),
            "completion_unknown" => (
                "file_unavailable",
                "completion_unknown",
                "file cancellation outcome is unknown; inspect transfer status before retrying",
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
        if reason == "receipt_capacity" {
            data["capacity"] = MAX_RECEIPTS.max(self.max_staged).into();
        }
        if stage == "authorize_upload" {
            data["publication_attempted"] = false.into();
        }
        if reason == "upload_capacity_exhausted" {
            // Commits and cancellations can release staging promptly, so pacing
            // stays independent of the authorization lifetime.
            data["retry_after_seconds"] = 5.into();
        } else if reason == "receipt_capacity" {
            let now = Instant::now();
            if let Some(expires) = self
                .transfers
                .lock()
                .expect("incoming file registry poisoned")
                .values()
                .map(|transfer| transfer.expires)
                .min()
            {
                data["retry_after_seconds"] = expires
                    .saturating_duration_since(now)
                    .as_secs()
                    .max(1)
                    .into();
            }
        }
        rmcp::ErrorData::invalid_params(message, Some(data))
    }

    pub async fn ingest(&self, params: IngestParams) -> Result<IngestResult, ToolError> {
        let transfer = self
            .get(&params.file)
            .map_err(|error| ingest_error(error, None))?;
        let mut state = Arc::clone(&transfer.state).lock_owned().await;
        let workspace = Arc::clone(&self.workspace);
        let max_bytes = self.max_bytes;
        tokio::task::spawn_blocking(move || {
            if transfer.expires <= Instant::now() {
                return Err(ingest_error(ToolError::UploadNotFound, None));
            }
            match &*state {
                TransferState::Committed(previous, result) if previous == &params => {
                    return Ok(result.clone());
                }
                TransferState::Ready(_) => {}
                TransferState::CommitUncertain { .. } => {
                    return Err(ingest_error(ToolError::Validation(
                        "previous publication outcome is uncertain; inspect the destination before authorizing another transfer".into(),
                    ), Some(true)));
                }
                _ => {
                    let publication_attempted = matches!(*state, TransferState::Committed(..));
                    return Err(ingest_error(ToolError::Validation(
                        "file is not ready or was committed to another destination".into(),
                    ), Some(publication_attempted)));
                }
            }
            if let Err(error) = workspace.validate_public_mutation_path(&params.path) {
                *state = TransferState::Failed;
                transfer.permit.lock().expect("upload permit poisoned").take();
                return Err(ingest_error(error.into(), Some(false)));
            }
            // Publication can precede a fallible directory sync. Once attempted,
            // only a successful receipt permits a repeat request to return success.
            // Declare the permit first so staging cleanup finishes before release.
            let _permit = transfer
                .permit
                .lock()
                .expect("upload permit poisoned")
                .take();
            let TransferState::Ready(received) = std::mem::replace(
                &mut *state,
                TransferState::CommitUncertain { path: params.path.clone() },
            ) else {
                unreachable!()
            };
            let artifact = workspace.commit_generated_artifact_bounded(
                &params.path,
                received.file.path(),
                params.overwrite,
                max_bytes,
            ).map_err(|error| {
                // Workspace quota admission precedes destination publication.
                // Other filesystem errors can follow publication or directory sync.
                let publication_attempted = !matches!(
                    error,
                    printable_workspace::WsError::StorageBudgetExceeded
                );
                if !publication_attempted {
                    *state = TransferState::Failed;
                }
                ingest_error(error.into(), Some(publication_attempted))
            })?;
            let result = IngestResult {
                artifact,
                sha256: received.sha256.clone(),
            };
            *state = TransferState::Committed(params, result.clone());
            Ok(result)
        })
        .await
        .map_err(|error| ingest_error(ToolError::Io(std::io::Error::other(error)), None))?
    }

    fn prune(&self) {
        let expired = {
            let mut transfers = self
                .transfers
                .lock()
                .expect("incoming file registry poisoned");
            let now = Instant::now();
            let expired = transfers
                .extract_if(|_, transfer| transfer.expires <= now)
                .map(|(_, transfer)| transfer)
                .collect::<Vec<_>>();
            if !expired.is_empty() {
                self.pending_cleanup.fetch_add(1, Ordering::Release);
            }
            expired
        };
        if expired.is_empty() {
            return;
        }
        let pending_cleanup = Arc::clone(&self.pending_cleanup);
        let cleanup = move || {
            // The retired owners retain their permits through scratch cleanup.
            drop(expired);
            pending_cleanup.fetch_sub(1, Ordering::Release);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(cleanup);
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
        } else {
            *state = TransferState::Receiving;
            None
        }
    };
    if let Some(status) = rejection {
        // Expiry may concurrently retire the registry's owner, even for a
        // rejected request. Release any last staging owner off the executor.
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
        // Finalization can own the last staging reference after cancellation
        // or expiry. Its resource destruction belongs on the blocking pool.
        tokio::task::spawn_blocking(move || {
            let _files = files;
            let cancelled = transfer.cancellation.is_cancelled();
            if cancelled || transfer.expires <= Instant::now() {
                drop(result);
                *state = if cancelled { TransferState::Cancelled } else { TransferState::Failed };
                transfer.permit.lock().expect("upload permit poisoned").take();
                return if cancelled { StatusCode::GONE } else { StatusCode::REQUEST_TIMEOUT }.into_response();
            }
            match result {
                Ok(received) => {
                    let receipt = serde_json::json!({"state": "ready", "size_bytes": received.size, "sha256": received.sha256});
                    *state = TransferState::Ready(received);
                    (StatusCode::CREATED, Json(receipt)).into_response()
                }
                Err(status) => {
                    *state = TransferState::Failed;
                    transfer.permit.lock().expect("upload permit poisoned").take();
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
            let deadline = transfer.expires.min(Instant::now() + IDLE_TIMEOUT);
            let frame = tokio::select! {
                _ = transfer.cancellation.cancelled() => return Err(StatusCode::GONE),
                frame = tokio::time::timeout_at(
                    deadline.into(),
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
            while files.pending_cleanup.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expired transfer cleanup completes");
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
        assert_eq!(files.cancel(uri).await.unwrap()["cancelled"], true);
        assert_eq!(files.cancel(uri).await.unwrap()["cancelled"], true);
        let status = files.status(uri).await.unwrap();
        assert_eq!(status["state"], "cancelled");
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
    async fn cancelled_transfers_do_not_exhaust_retained_receipts() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = IncomingFiles::with_capacity(workspace, 1024, 1);
        for _ in 0..=MAX_RECEIPTS {
            let authorization = authorize(&files, "source.3mf");
            let uri = authorization["file"]["uri"].as_str().unwrap();
            assert_eq!(files.cancel(uri).await.unwrap()["cancelled"], true);
        }
        assert_eq!(files.capacity.available_permits(), 1);
        assert!(files.transfers.lock().unwrap().len() <= MAX_RECEIPTS);
    }

    #[tokio::test]
    async fn quota_refusals_do_not_exhaust_receipts_and_recover_after_budget_is_freed() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let files = Arc::new(IncomingFiles::with_capacity(
            Arc::clone(&workspace),
            1024,
            1,
        ));
        for _ in 0..=MAX_RECEIPTS {
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
            assert_eq!(files.status(uri).await.unwrap()["state"], "failed");
            assert_eq!(files.capacity.available_permits(), 1);
            assert!(!root.path().join("model.3mf").exists());
        }
        assert!(files.transfers.lock().unwrap().len() <= MAX_RECEIPTS);
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
        assert_eq!(files.status(uri).await.unwrap()["state"], "committed");
        assert_eq!(files.capacity.available_permits(), 1);
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
        assert_eq!(files.status(uri).await.unwrap()["state"], "failed");
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
        assert_eq!(files.status(&uri).await.unwrap()["state"], "unknown");
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while files.status(&uri).await.unwrap()["state"] != "cancelled" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation finishes after the storage lock is released");
        let result = files.cancel(&uri).await.unwrap();
        assert_eq!(result["cancelled"], true);
        assert_eq!(files.status(&uri).await.unwrap()["state"], "cancelled");
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
        assert_eq!(files.status(&uri).await.unwrap()["state"], "cancelled");
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
            Arc::get_mut(transfer).unwrap().expires = Instant::now();
        }
        let (release, blocker) = block_storage(root.path());
        let started = Instant::now();
        files.prune();
        assert!(started.elapsed() < Duration::from_secs(5));
        let snapshot = files.capabilities();
        assert_eq!(snapshot["available_staging_slots"], 0);
        assert!(snapshot["active_receivers"].is_null());
        assert!(snapshot["staged_files"].is_null());
        assert!(files.status(uri).await.is_err());
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
            Arc::get_mut(transfer).unwrap().expires = Instant::now();
        }
        authorize(&files, "replacement.3mf");
        assert_eq!(files.transfers.lock().unwrap().len(), 1);
        assert_eq!(files.pending_cleanup.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn publication_error_consumes_receipt_without_overwriting_later_changes() {
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
        let status = files.status(uri).await.unwrap();
        assert_eq!(status["state"], "commit_uncertain");
        assert_eq!(status["path"], "result.stl");
        let cancellation = files.cancel(uri).await.unwrap();
        assert_eq!(cancellation["state"], "commit_uncertain");
        assert_eq!(cancellation["cancelled"], false);
        assert_eq!(files.capacity.available_permits(), MAX_TRANSFERS);
        std::fs::remove_dir(&destination).unwrap();
        std::fs::write(&destination, b"later edit").unwrap();
        assert!(files.ingest(params).await.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"later edit");
    }

    #[tokio::test]
    async fn missing_ingest_receipt_does_not_claim_a_publication_outcome() {
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
            Arc::get_mut(transfer).unwrap().expires = Instant::now();
        }
        assert!(files.status(uri).await.is_err());
        files.prune();
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
