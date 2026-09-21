//! Native raw-byte uploads consumed by workspace artifact operations.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json,
    body::{Body, HttpBody},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use printable_workspace::{ArtifactMeta, Workspace};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, upload::random_hex_id};

pub const INGEST_TOOL: &str = "printable_workspace_ingest";
const URI_PREFIX: &str = "mcp-file://printable/upload/";
const MAX_TRANSFERS: usize = 2;
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
    /// File URI delivered by the gateway's native upload forwarding.
    #[schemars(extend("x-mcp-file" = {"transferModes": ["upload"]}))]
    pub file: String,
    /// Destination path relative to the confined workspace.
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

#[derive(Clone, Serialize)]
pub struct IngestResult {
    #[serde(flatten)]
    pub artifact: ArtifactMeta,
    pub sha256: String,
}

struct Received {
    file: tempfile::NamedTempFile,
    size: u64,
    sha256: String,
}

enum TransferState {
    Prepared,
    Receiving,
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
}

pub struct IncomingFiles {
    workspace: Arc<Workspace>,
    max_bytes: u64,
    transfers: Mutex<HashMap<String, Arc<Transfer>>>,
    capacity: Arc<Semaphore>,
}

impl IncomingFiles {
    pub fn new(workspace: Arc<Workspace>, max_bytes: u64) -> Self {
        Self {
            workspace,
            max_bytes,
            transfers: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(MAX_TRANSFERS)),
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
            return Err(ToolError::Validation(format!(
                "file upload exceeds {} bytes",
                self.max_bytes
            )));
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
        self.prune();
        let permit = Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| ToolError::TooManyUploads(MAX_TRANSFERS))?;
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
        if transfers.len() >= MAX_RECEIPTS {
            return Err(ToolError::TooManyUploads(MAX_RECEIPTS));
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
            }),
        );
        Ok(serde_json::json!({
            "file": file,
            "upload": {
                "transport": "http", "method": "PUT",
                "url": upload_url.as_str(),
                "headers": BTreeMap::from([("Authorization", format!("Bearer {token}"))])
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
        let transfer = self.get(uri)?;
        let state = transfer.state.lock().await;
        Ok(match &*state {
            TransferState::Prepared => serde_json::json!({"state": "prepared"}),
            TransferState::Receiving => serde_json::json!({"state": "receiving"}),
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

    pub async fn ingest(&self, params: IngestParams) -> Result<IngestResult, ToolError> {
        self.workspace.validate_public_mutation_path(&params.path)?;
        let transfer = self.get(&params.file)?;
        let mut state = Arc::clone(&transfer.state).lock_owned().await;
        let workspace = Arc::clone(&self.workspace);
        let max_bytes = self.max_bytes;
        tokio::task::spawn_blocking(move || {
            if transfer.expires <= Instant::now() {
                return Err(ToolError::UploadNotFound);
            }
            match &*state {
                TransferState::Committed(previous, result) if previous == &params => {
                    return Ok(result.clone());
                }
                TransferState::Ready(_) => {}
                TransferState::CommitUncertain { .. } => {
                    return Err(ToolError::Validation(
                        "previous publication outcome is uncertain; inspect the destination before authorizing another transfer".into(),
                    ));
                }
                _ => {
                    return Err(ToolError::Validation(
                        "file is not ready or was committed to another destination".into(),
                    ));
                }
            }
            // Publication can precede a fallible directory sync. Once attempted,
            // only a successful receipt permits a repeat request to return success.
            let TransferState::Ready(received) = std::mem::replace(
                &mut *state,
                TransferState::CommitUncertain { path: params.path.clone() },
            ) else {
                unreachable!()
            };
            let _permit = transfer
                .permit
                .lock()
                .expect("upload permit poisoned")
                .take();
            let artifact = workspace.commit_generated_artifact_bounded(
                &params.path,
                received.file.path(),
                params.overwrite,
                max_bytes,
            )?;
            let result = IngestResult {
                artifact,
                sha256: received.sha256.clone(),
            };
            *state = TransferState::Committed(params, result.clone());
            Ok(result)
        })
        .await
        .map_err(|error| ToolError::Io(std::io::Error::other(error)))?
    }

    fn prune(&self) {
        self.transfers
            .lock()
            .expect("incoming file registry poisoned")
            .retain(|_, transfer| transfer.expires > Instant::now());
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

pub async fn upload(
    State(files): State<Arc<IncomingFiles>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Ok(transfer) = files.get(&format!("{URI_PREFIX}{id}")) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if !bool::from(hash.ct_eq(&transfer.token_hash)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    {
        let mut state = transfer.state.lock().await;
        if !matches!(*state, TransferState::Prepared) {
            return StatusCode::CONFLICT.into_response();
        }
        *state = TransferState::Receiving;
    }
    tokio::spawn(async move {
    let result = tokio::time::timeout_at(
        transfer.expires.into(),
        receive(body, &transfer, files.max_bytes),
    )
    .await
    .unwrap_or(Err(StatusCode::REQUEST_TIMEOUT));
    let mut state = transfer.state.lock().await;
    match result {
        Ok(received) => {
            let receipt = serde_json::json!({"state": "ready", "size_bytes": received.size, "sha256": received.sha256});
            *state = TransferState::Ready(received);
            (StatusCode::CREATED, Json(receipt)).into_response()
        }
        Err(status) => {
            *state = TransferState::Failed;
            transfer
                .permit
                .lock()
                .expect("upload permit poisoned")
                .take();
            status.into_response()
        }
    }
    }).await.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn receive(
    mut body: Body,
    transfer: &Transfer,
    max_bytes: u64,
) -> Result<Received, StatusCode> {
    let staged = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut writer = tokio::fs::File::from_std(
        staged
            .reopen()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let frame = tokio::time::timeout(
            IDLE_TIMEOUT,
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .map_err(|_| StatusCode::REQUEST_TIMEOUT)?;
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
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            digest.update(&bytes);
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
    writer
        .flush()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    writer
        .sync_all()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Received {
        file: staged,
        size,
        sha256: URL_SAFE_NO_PAD.encode(hash),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(files.ingest(params.clone()).await.is_err());
        let status = files.status(uri).await.unwrap();
        assert_eq!(status["state"], "commit_uncertain");
        assert_eq!(status["path"], "result.stl");
        assert_eq!(files.capacity.available_permits(), MAX_TRANSFERS);
        std::fs::remove_dir(&destination).unwrap();
        std::fs::write(&destination, b"later edit").unwrap();
        assert!(files.ingest(params).await.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"later edit");
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
