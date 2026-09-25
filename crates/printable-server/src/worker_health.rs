//! Bounded native-worker readiness, separate from process liveness.

use std::{os::unix::process::CommandExt, process::Stdio, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    NotConfigured,
    Ready,
    Busy,
    Unavailable,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NativeEngine {
    CadQuery,
    OrcaSlicer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct ProfileCounts {
    pub printer: usize,
    pub process: usize,
    pub filament: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkerReadiness {
    pub protocol_version: u32,
    pub configured: bool,
    pub state: CapabilityState,
    pub engine: NativeEngine,
    pub version: Option<String>,
    pub profile_counts: Option<ProfileCounts>,
}

impl WorkerReadiness {
    pub fn new(engine: NativeEngine, version: Option<String>) -> Self {
        let state = match version.as_deref() {
            Some(value) if valid_version(value) => CapabilityState::Ready,
            Some(_) => CapabilityState::Incompatible,
            None => CapabilityState::Unavailable,
        };
        let version = version.filter(|value| valid_version(value));
        Self {
            protocol_version: 1,
            configured: true,
            state,
            engine,
            version,
            profile_counts: None,
        }
    }

    pub fn available(&self) -> bool {
        matches!(self.state, CapabilityState::Ready | CapabilityState::Busy)
    }

    pub fn runtime(mut self, workspace_ready: bool, busy: bool) -> Self {
        if !workspace_ready {
            self.state = CapabilityState::Unavailable;
        } else if self.available() {
            self.state = if busy {
                CapabilityState::Busy
            } else {
                CapabilityState::Ready
            };
        }
        self
    }

    fn valid_for(&self, engine: NativeEngine) -> bool {
        self.protocol_version == 1
            && self.configured
            && self.engine == engine
            && self.state != CapabilityState::NotConfigured
            && self
                .version
                .as_ref()
                .is_none_or(|version| valid_version(version))
            && (!self.available()
                || (self.version.is_some()
                    && (engine != NativeEngine::OrcaSlicer
                        || self.profile_counts.is_some_and(|counts| {
                            counts.printer > 0 && counts.process > 0 && counts.filament > 0
                        }))))
    }
}

fn valid_version(version: &str) -> bool {
    version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version.len() <= 64
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-+_".contains(&c))
}

/// Read only a small worker-owned capability response. Connection errors and
/// backend bodies never become public diagnostic strings.
pub async fn probe(endpoint: Option<&str>, engine: NativeEngine) -> WorkerReadiness {
    let mut unavailable = WorkerReadiness::new(engine, None);
    let Some(endpoint) = endpoint else {
        unavailable.configured = false;
        unavailable.state = CapabilityState::NotConfigured;
        return unavailable;
    };
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return unavailable;
    };
    let Ok(mut response) = client
        .get(format!("{}/readyz", endpoint.trim_end_matches('/')))
        .send()
        .await
    else {
        return unavailable;
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        unavailable.state = CapabilityState::Incompatible;
        return unavailable;
    }
    if !status.is_success() && status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return unavailable;
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= 16 * 1024 => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Ok(Some(_)) => {
                unavailable.state = CapabilityState::Incompatible;
                return unavailable;
            }
            Err(_) => return unavailable,
        }
    }
    match serde_json::from_slice::<WorkerReadiness>(&bytes) {
        Ok(value) if value.valid_for(engine) && (status.is_success() || !value.available()) => {
            value
        }
        _ => {
            unavailable.state = CapabilityState::Incompatible;
            unavailable
        }
    }
}

/// Check the installed native command once during worker startup, without
/// touching the shared workspace. The process group and its private runtime
/// files are cleaned up even if the bounded probe is cancelled.
pub async fn probe_native(mut command: Command) -> Option<String> {
    let directory = tempfile::tempdir().ok()?;
    command
        .current_dir(directory.path())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", directory.path())
        .env("OMP_NUM_THREADS", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().ok()?;
    let _group = ProbeProcessGroup(child.id()?);
    let mut stdout = child.stdout.take()?.take(1024 * 1024 + 1);
    let run = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > 1024 * 1024 || !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(bytes).ok()
    };
    tokio::time::timeout(Duration::from_secs(15), run)
        .await
        .ok()?
}

struct ProbeProcessGroup(u32);

impl Drop for ProbeProcessGroup {
    fn drop(&mut self) {
        // The native probe owns the process group created before exec.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::get};
    use serde_json::{Value, json};

    async fn server(status: StatusCode, body: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/readyz",
            get(move || {
                let body = body.clone();
                async move { (status, body) }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, task)
    }

    fn ready(engine: &str) -> Value {
        json!({"protocol_version":1,"configured":true,"state":"ready", "engine":engine,
            "version":"2.8.0", "profile_counts":null})
    }

    #[tokio::test]
    async fn reports_native_capabilities_without_running_a_job() {
        let absent = probe(None, NativeEngine::CadQuery).await;
        assert!(!absent.configured);
        assert_eq!(absent.state, CapabilityState::NotConfigured);
        let (endpoint, task) = server(StatusCode::OK, ready("CadQuery").to_string()).await;
        let cad = probe(Some(&endpoint), NativeEngine::CadQuery).await;
        assert_eq!(cad.state, CapabilityState::Ready);
        assert_eq!(cad.version.as_deref(), Some("2.8.0"));
        task.abort();

        let mut value = ready("OrcaSlicer");
        value["state"] = json!("busy");
        value["version"] = json!("2.4.2");
        value["profile_counts"] = json!({"printer":3,"process":4,"filament":5});
        let (endpoint, task) = server(StatusCode::OK, value.to_string()).await;
        let slicer = probe(Some(&endpoint), NativeEngine::OrcaSlicer).await;
        assert_eq!(slicer.state, CapabilityState::Busy);
        assert_eq!(slicer.profile_counts.unwrap().filament, 5);
        task.abort();
    }

    #[tokio::test]
    async fn rejects_stale_incomplete_or_wrong_worker_contracts() {
        let mut old = ready("CadQuery");
        old["protocol_version"] = json!(0);
        let mut diagnostic = ready("CadQuery");
        diagnostic["version"] = json!("http://private-worker:8001/path");
        let cases = [
            (
                StatusCode::NOT_FOUND,
                "old worker".to_owned(),
                NativeEngine::CadQuery,
            ),
            (StatusCode::OK, old.to_string(), NativeEngine::CadQuery),
            (
                StatusCode::OK,
                diagnostic.to_string(),
                NativeEngine::CadQuery,
            ),
            (
                StatusCode::OK,
                ready("CadQuery").to_string(),
                NativeEngine::OrcaSlicer,
            ),
            (
                StatusCode::OK,
                ready("OrcaSlicer").to_string(),
                NativeEngine::OrcaSlicer,
            ),
            (
                StatusCode::OK,
                "x".repeat(16 * 1024 + 1),
                NativeEngine::CadQuery,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ready("CadQuery").to_string(),
                NativeEngine::CadQuery,
            ),
        ];
        for (status, body, engine) in cases {
            let (endpoint, task) = server(status, body).await;
            let result = probe(Some(&endpoint), engine).await;
            assert_eq!(result.state, CapabilityState::Incompatible);
            let wire = serde_json::to_string(&result).unwrap();
            assert!(!wire.contains("private-worker"));
            assert!(!wire.contains(&endpoint));
            task.abort();
        }
    }

    #[tokio::test]
    async fn unavailable_and_failed_transport_are_not_configuration_success() {
        let mut value = ready("CadQuery");
        value["state"] = json!("unavailable");
        value["version"] = Value::Null;
        let (endpoint, task) = server(StatusCode::SERVICE_UNAVAILABLE, value.to_string()).await;
        let result = probe(Some(&endpoint), NativeEngine::CadQuery).await;
        assert!(result.configured);
        assert_eq!(result.state, CapabilityState::Unavailable);
        task.abort();
        task.await.unwrap_err();
        assert_eq!(
            probe(Some(&endpoint), NativeEngine::CadQuery).await.state,
            CapabilityState::Unavailable
        );
    }

    #[tokio::test]
    async fn stalled_worker_probe_has_a_deadline() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/readyz",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "never ready"
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            probe(Some(&endpoint), NativeEngine::CadQuery),
        )
        .await
        .unwrap();
        assert_eq!(result.state, CapabilityState::Unavailable);
        task.abort();
    }

    #[tokio::test]
    async fn native_probe_checks_execution_and_bounds_its_output() {
        let mut working = Command::new("/bin/sh");
        working.args(["-c", "printf '2.8.0\\n'"]);
        assert_eq!(probe_native(working).await.as_deref(), Some("2.8.0\n"));
        let mut failed = Command::new("/bin/sh");
        failed.args(["-c", "printf '2.8.0'; exit 1"]);
        assert!(probe_native(failed).await.is_none());
        let mut oversized = Command::new("/usr/bin/python3");
        oversized.args(["-c", "print('x' * (1024 * 1024 + 1))"]);
        assert!(probe_native(oversized).await.is_none());
    }
}
