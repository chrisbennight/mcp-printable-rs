//! Governed publication of workspace artifacts through the MCP file handoff.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rmcp::model::RequestMetaObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use printable_workspace::{Snapshot, Workspace};

use crate::{error::ToolError, upload::random_hex_id};

pub const PUBLISH_TOOL: &str = "printable_workspace_publish";
const FILE_URI_PREFIX: &str = "mcp-file://printable/";
const DOWNLOAD_PATH_PREFIX: &str = "/file-transfers/download/";
const CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const MAX_PUBLISHED_FILES: usize = 2;
const MAX_PUBLISHED_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const PUBLISHED_FILE_TTL: Duration = Duration::from_secs(5 * 60);
const DOWNLOAD_GRANT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishParams {
    /// Supported artifact path under the confined workspace. The published
    /// bytes are an immutable snapshot and may be larger than the MCP body cap.
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDigest {
    algorithm: &'static str,
    value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileValue {
    pub uri: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub digest: FileDigest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizeDownloadParams {
    uri: String,
}

pub fn client_supports_http_download(meta: &RequestMetaObject) -> bool {
    client_supports_http_transfer(meta, "download")
}

pub fn client_supports_http_transfer(meta: &RequestMetaObject, direction: &str) -> bool {
    let Some(files) = meta
        .get(CLIENT_CAPABILITIES_META_KEY)
        .and_then(|capabilities| capabilities.get("files"))
    else {
        return false;
    };
    files.get(direction).and_then(serde_json::Value::as_bool) == Some(true)
        && files
            .get("transports")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|transports| transports.iter().any(|transport| transport == "http"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeDownloadResult {
    file: FileValue,
    download: FileTransferDescriptor,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileTransferDescriptor {
    transport: &'static str,
    method: &'static str,
    url: String,
    headers: BTreeMap<String, String>,
}

struct DownloadGrant {
    token_hash: [u8; 32],
    expires_at: Instant,
}

struct PublishedEntry {
    file: FileValue,
    snapshot: Snapshot,
    expires_at: Instant,
    grant: Option<DownloadGrant>,
    _capacity: OwnedSemaphorePermit,
}

/// Process-wide immutable snapshots awaiting import by the authenticated MCP
/// client. Entries are bounded by count and file size, expire automatically,
/// and are consumed by one authorized download.
pub struct PublishedFiles {
    workspace: Arc<Workspace>,
    entries: Mutex<HashMap<String, PublishedEntry>>,
    capacity: Arc<Semaphore>,
}

impl PublishedFiles {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            entries: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(MAX_PUBLISHED_FILES)),
        }
    }

    pub async fn publish(&self, params: PublishParams) -> Result<FileValue, ToolError> {
        self.prune_expired();
        let permit = Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| ToolError::TooManyPublishedFiles(MAX_PUBLISHED_FILES))?;
        let workspace = Arc::clone(&self.workspace);
        let snapshot = tokio::task::spawn_blocking(move || {
            let snapshot =
                workspace.snapshot_artifact_bounded(&params.path, MAX_PUBLISHED_FILE_BYTES)?;
            let mut file = std::fs::File::open(snapshot.path())?;
            let mut digest = Sha256::new();
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                digest.update(&chunk[..read]);
            }
            Ok::<_, ToolError>((snapshot, digest.finalize(), permit))
        })
        .await
        .map_err(|error| ToolError::Io(std::io::Error::other(error.to_string())))??;
        let (snapshot, digest, permit) = snapshot;

        loop {
            let id = random_hex_id()?;
            let meta = snapshot.meta();
            let file = FileValue {
                uri: format!("{FILE_URI_PREFIX}{id}"),
                name: meta
                    .path
                    .rsplit_once('/')
                    .map_or_else(|| meta.path.clone(), |(_, name)| name.to_string()),
                mime_type: meta.media_type.to_string(),
                size: meta.size_bytes,
                digest: FileDigest {
                    algorithm: "sha-256",
                    value: URL_SAFE_NO_PAD.encode(digest),
                },
            };
            let mut entries = self
                .entries
                .lock()
                .expect("published file registry poisoned");
            if entries.contains_key(&id) {
                continue;
            }
            entries.insert(
                id,
                PublishedEntry {
                    file: file.clone(),
                    snapshot,
                    expires_at: Instant::now() + PUBLISHED_FILE_TTL,
                    grant: None,
                    _capacity: permit,
                },
            );
            return Ok(file);
        }
    }

    pub fn authorize_download(
        &self,
        params: AuthorizeDownloadParams,
        base_url: &reqwest::Url,
    ) -> Result<serde_json::Value, String> {
        let id = params
            .uri
            .strip_prefix(FILE_URI_PREFIX)
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .ok_or_else(|| "unknown or expired Printable file URI".to_string())?;
        let url = base_url
            .join(&format!(
                "{}{id}",
                DOWNLOAD_PATH_PREFIX.trim_start_matches('/')
            ))
            .map_err(|_| "file authorization unavailable".to_string())?;
        let token = random_token().map_err(|_| "file authorization unavailable".to_string())?;
        let token_hash = Sha256::digest(token.as_bytes()).into();
        let file = {
            let mut entries = self
                .entries
                .lock()
                .expect("published file registry poisoned");
            let now = Instant::now();
            prune_locked(&mut entries, now);
            let entry = entries
                .get_mut(id)
                .ok_or_else(|| "unknown or expired Printable file URI".to_string())?;
            entry.grant = Some(DownloadGrant {
                token_hash,
                expires_at: download_grant_expiry(now, entry.expires_at),
            });
            entry.file.clone()
        };
        let result = AuthorizeDownloadResult {
            file,
            download: FileTransferDescriptor {
                transport: "http",
                method: "GET",
                url: url.to_string(),
                headers: BTreeMap::from([("Authorization".to_string(), format!("Bearer {token}"))]),
            },
        };
        serde_json::to_value(result).map_err(|_| "file authorization unavailable".to_string())
    }

    pub fn spawn_reaper(self: &Arc<Self>, cancel: CancellationToken) {
        let published = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => published.prune_expired(),
                }
            }
        });
    }

    fn prune_expired(&self) {
        let mut entries = self
            .entries
            .lock()
            .expect("published file registry poisoned");
        prune_locked(&mut entries, Instant::now());
    }
}

fn prune_locked(entries: &mut HashMap<String, PublishedEntry>, now: Instant) {
    entries.retain(|_, entry| entry.expires_at > now);
}

fn download_grant_expiry(now: Instant, entry_expires_at: Instant) -> Instant {
    (now + DOWNLOAD_GRANT_TTL).min(entry_expires_at)
}

fn random_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub async fn download(
    State(published): State<Arc<PublishedFiles>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let (source, media_type, size) = {
        let mut entries = published
            .entries
            .lock()
            .expect("published file registry poisoned");
        prune_locked(&mut entries, Instant::now());
        let authorized = entries.get(&id).is_some_and(|entry| {
            entry.grant.as_ref().is_some_and(|grant| {
                grant.expires_at > Instant::now()
                    && constant_time_eq(&grant.token_hash, &token_hash)
            })
        });
        if !authorized {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let entry = entries.remove(&id).expect("authorized entry exists");
        let stream = match snapshot_stream(entry.snapshot, entry._capacity) {
            Ok(stream) => stream,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        (stream, entry.file.mime_type, entry.file.size)
    };
    let mut response = Body::from_stream(source).into_response();
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        media_type
            .parse()
            .expect("workspace media types are valid HTTP header values"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        size.to_string()
            .parse()
            .expect("decimal content length is a valid HTTP header value"),
    );
    response
}

/// Each blocking read owns the snapshot and its optional admission guard.
/// Dropping an HTTP body cannot release storage while an offloaded read holds it.
pub(crate) fn snapshot_stream(
    snapshot: Snapshot,
    guard: impl Send + 'static,
) -> std::io::Result<impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send> {
    let file = std::fs::File::open(snapshot.path())?;
    Ok(futures_util::stream::try_unfold(
        (file, snapshot, guard),
        |(mut file, snapshot, guard)| async move {
            tokio::task::spawn_blocking(move || {
                let mut bytes = vec![0u8; 64 * 1024];
                let count = file.read(&mut bytes)?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                bytes.truncate(count);
                Ok(Some((bytes, (file, snapshot, guard))))
            })
            .await
            .map_err(std::io::Error::other)?
        },
    ))
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_body_keeps_queued_file_read_owned() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let workspace = Workspace::open(Some(root.path()), None).unwrap();
            workspace
                .write_artifact("source.stl", b"source", false)
                .unwrap();
            let snapshot = workspace.snapshot_artifact("source.stl").unwrap();
            let capacity = Arc::new(Semaphore::new(1));
            let guard = Arc::clone(&capacity).try_acquire_owned().unwrap();
            let (release, blocked) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || blocked.recv().unwrap());
            let body = Body::from_stream(snapshot_stream(snapshot, guard).unwrap());
            let mut reader = Box::pin(axum::body::to_bytes(body, 100));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(reader.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(reader);
            assert_eq!(capacity.available_permits(), 0);
            let protected = workspace.cleanup_preview().unwrap();
            assert_eq!(protected.len(), 1);
            assert_eq!(
                workspace
                    .cleanup_scratch(&[protected[0].id.clone()])
                    .unwrap()[0]
                    .state,
                "protected"
            );
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                while capacity.available_permits() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(workspace.cleanup_preview().unwrap().is_empty());
        });
    }

    #[tokio::test]
    async fn streamed_downloads_keep_storage_owned_until_consumed_or_dropped() {
        for consume in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
            workspace.configure_storage_budget(Some(40_000)).unwrap();
            let bytes =
                serde_json::to_vec(&serde_json::json!({"payload":"x".repeat(8000)})).unwrap();
            // Caller filenames must not overwrite the scratch controller metadata.
            workspace
                .write_artifact("reservation.json", &bytes, false)
                .unwrap();
            let published = Arc::new(PublishedFiles::new(Arc::clone(&workspace)));
            let file = published
                .publish(PublishParams {
                    path: "reservation.json".into(),
                })
                .await
                .unwrap();
            assert!(workspace.storage_usage().unwrap().complete);
            let grant = published
                .authorize_download(
                    AuthorizeDownloadParams {
                        uri: file.uri.clone(),
                    },
                    &"http://127.0.0.1:8000".parse().unwrap(),
                )
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                grant["download"]["headers"]["Authorization"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
            let response = download(
                State(Arc::clone(&published)),
                Path(file.uri.strip_prefix(FILE_URI_PREFIX).unwrap().to_owned()),
                headers,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(published.entries.lock().unwrap().is_empty());
            assert_eq!(published.capacity.available_permits(), 1);
            let protected = workspace.cleanup_preview().unwrap();
            assert_eq!(protected.len(), 1);
            assert_eq!(
                workspace
                    .cleanup_scratch(&[protected[0].id.clone()])
                    .unwrap()[0]
                    .state,
                "protected"
            );
            assert!(matches!(
                workspace.write_artifact("next.stl", &vec![0; 25_000], false),
                Err(printable_workspace::WsError::StorageBudgetExceeded)
            ));
            if consume {
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 10_000)
                        .await
                        .unwrap()
                        .as_ref(),
                    bytes.as_slice()
                );
            } else {
                drop(response);
            }
            assert!(workspace.cleanup_preview().unwrap().is_empty());
            assert_eq!(published.capacity.available_permits(), 2);
            workspace
                .write_artifact("next.stl", &vec![0; 25_000], false)
                .unwrap();
        }
    }

    #[test]
    fn download_grant_never_outlives_its_snapshot() {
        let now = Instant::now();
        let near_snapshot_expiry = now + Duration::from_secs(5);
        let distant_snapshot_expiry = now + DOWNLOAD_GRANT_TTL * 2;

        assert_eq!(
            download_grant_expiry(now, near_snapshot_expiry),
            near_snapshot_expiry
        );
        assert_eq!(
            download_grant_expiry(now, distant_snapshot_expiry),
            now + DOWNLOAD_GRANT_TTL
        );
    }
}
