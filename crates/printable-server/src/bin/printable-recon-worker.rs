//! HTTP service that runs the photograph jobs: FLAME head reconstruction and
//! Marigold V2 depth estimation, one at a time.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use printable_server::recon::{
    DepthRequest, ReconConfig, ReconRequest, ReconWorker, worker_message,
};
use printable_workspace::Workspace;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address: std::net::SocketAddr = std::env::var("PRINTABLE_RECON_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8004".into())
        .parse()
        .context("PRINTABLE_RECON_LISTEN must be an IP address and port")?;
    anyhow::ensure!(
        address.port() != 0,
        "PRINTABLE_RECON_LISTEN must use a nonzero port"
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
            .timeout(Duration::from_secs(3))
            .build()?
            .get(format!("http://{probe}/healthz"))
            .send()
            .await?
            .error_for_status()?;
        return Ok(());
    }
    tracing_subscriber::fmt().with_target(false).init();
    let shutdown = printable_server::shutdown::signal().context("register shutdown signals")?;
    let root = PathBuf::from(
        std::env::var("PRINTABLE_WORKSPACE_ROOT")
            .context("PRINTABLE_WORKSPACE_ROOT is required")?,
    );
    let path = |name: &str, default: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| default.into())
    };
    let seconds = |name: &str, default: u64| match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|seconds| (1..=3600).contains(seconds))
            .with_context(|| format!("{name} must be between 1 and 3600")),
        Err(_) => Ok(default),
    };
    let timeout = seconds("PRINTABLE_RECON_TIMEOUT_SECONDS", 300)?;
    // Depth estimation reads the whole transformer from disk once per map.
    let depth_timeout = seconds("PRINTABLE_RECON_DEPTH_TIMEOUT_SECONDS", 900)?;
    let device = std::env::var("PRINTABLE_RECON_DEVICE").unwrap_or_else(|_| "auto".into());
    anyhow::ensure!(
        matches!(device.as_str(), "auto" | "cpu" | "cuda"),
        "PRINTABLE_RECON_DEVICE must be auto, cpu or cuda"
    );
    let worker = Arc::new(ReconWorker::new(
        Arc::new(Workspace::open(Some(&root), None)?),
        ReconConfig {
            python: path("PRINTABLE_RECON_PYTHON", "/opt/recon/bin/python"),
            script: path(
                "PRINTABLE_RECON_SCRIPT",
                "/opt/printable/recon/reconstruct.py",
            ),
            checkpoints: path("PRINTABLE_RECON_CHECKPOINTS", "/models/recon"),
            landmarker: path(
                "PRINTABLE_RECON_LANDMARKER",
                "/opt/printable/recon/face_landmarker.task",
            ),
            segmenter: path(
                "PRINTABLE_RECON_SEGMENTER",
                "/opt/printable/recon/selfie_multiclass_256x256.tflite",
            ),
            device,
            timeout: Duration::from_secs(timeout),
            depth_script: path(
                "PRINTABLE_RECON_DEPTH_SCRIPT",
                "/opt/printable/recon/estimate_depth.py",
            ),
            depth_model: path("PRINTABLE_RECON_DEPTH_MODEL", "/models/marigold"),
            depth_timeout: Duration::from_secs(depth_timeout),
        },
    ));
    let app = Router::new()
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/readyz", get(readiness))
        .route("/reconstruct", post(reconstruct))
        .route("/depth", post(depth))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(worker);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Ready when the workspace is usable and the files of at least one job are
/// present; the answer says which jobs can run.
async fn readiness(State(worker): State<Arc<ReconWorker>>) -> (StatusCode, Json<Value>) {
    let (ready, answer) = worker.readiness().await;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(answer))
}

async fn reconstruct(
    State(worker): State<Arc<ReconWorker>>,
    Json(request): Json<ReconRequest>,
) -> (StatusCode, Json<Value>) {
    match worker.reconstruct(request).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code": error.code(), "message": worker_message(&error)})),
        ),
    }
}

async fn depth(
    State(worker): State<Arc<ReconWorker>>,
    Json(request): Json<DepthRequest>,
) -> (StatusCode, Json<Value>) {
    match worker.estimate_depth(request).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code": error.code(), "message": worker_message(&error)})),
        ),
    }
}
