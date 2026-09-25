use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use printable_server::cad::{CadRequest, CadWorker};
use printable_server::worker_health::{NativeEngine, WorkerReadiness, probe_native};
use printable_workspace::Workspace;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address: std::net::SocketAddr = std::env::var("PRINTABLE_CAD_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8001".into())
        .parse()
        .context("PRINTABLE_CAD_LISTEN must be an IP address and port")?;
    anyhow::ensure!(
        address.port() != 0,
        "PRINTABLE_CAD_LISTEN must use a nonzero port"
    );
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        let mut probe = address;
        if address.ip().is_unspecified() {
            probe.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()?
            .get(format!("http://{probe}/readyz"))
            .send()
            .await?
            .error_for_status()?;
        return Ok(());
    }
    tracing_subscriber::fmt().with_target(false).init();
    let root = PathBuf::from(
        std::env::var("PRINTABLE_WORKSPACE_ROOT")
            .context("PRINTABLE_WORKSPACE_ROOT is required")?,
    );
    let worker = Arc::new(CadWorker {
        workspace: Arc::new(Workspace::open(Some(&root), None)?),
        python: std::env::var_os("PRINTABLE_CAD_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/opt/cad/bin/python".into()),
        script: std::env::var_os("PRINTABLE_CAD_SCRIPT")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/opt/printable/cad/build.py".into()),
        admission: Semaphore::new(1),
    });
    let mut command = tokio::process::Command::new(&worker.python);
    command
        .arg("-I")
        .arg("-B")
        .arg(&worker.script)
        .arg("--version");
    let readiness = WorkerReadiness::new(
        NativeEngine::CadQuery,
        probe_native(command)
            .await
            .map(|version| version.trim().to_owned()),
    );
    let health_worker = Arc::clone(&worker);
    let app = Router::new()
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route(
            "/readyz",
            get(move || {
                let worker = Arc::clone(&health_worker);
                let readiness = readiness.clone();
                async move {
                    let readiness = readiness.runtime(
                        worker.workspace.ready() && worker.script.is_file(),
                        worker.admission.available_permits() == 0,
                    );
                    let status = if readiness.available() {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    };
                    (status, Json(readiness))
                }
            }),
        )
        .route("/build", post(build))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(worker);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn build(
    State(worker): State<Arc<CadWorker>>,
    Json(request): Json<CadRequest>,
) -> (StatusCode, Json<Value>) {
    match worker.build(request).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code":error.code(),"error":error.to_string()})),
        ),
    }
}
