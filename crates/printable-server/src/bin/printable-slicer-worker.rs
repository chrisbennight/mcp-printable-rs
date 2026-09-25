use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use printable_server::slicing::{ENGINE_VERSION, SliceRequest, SliceWorker, profiles::Profiles};
use printable_workspace::Workspace;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address: std::net::SocketAddr = std::env::var("PRINTABLE_SLICER_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8003".into())
        .parse()?;
    anyhow::ensure!(address.port() != 0, "slicer listen port must be nonzero");
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        let mut probe = address;
        if probe.ip().is_unspecified() {
            probe.set_ip(if probe.is_ipv4() {
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
    let profiles = PathBuf::from(
        std::env::var("PRINTABLE_SLICER_PROFILES")
            .unwrap_or_else(|_| "/opt/orca/resources/profiles/BBL".into()),
    );
    let binary = PathBuf::from(
        std::env::var("PRINTABLE_SLICER_BIN").unwrap_or_else(|_| "/opt/orca/AppRun".into()),
    );
    let worker = Arc::new(SliceWorker::new(
        Arc::new(Workspace::open(Some(&root), None)?),
        Profiles::load(&profiles)?,
        binary,
    ));
    let readiness = worker.probe_engine().await;
    let health_worker = Arc::clone(&worker);
    let app = Router::new()
        .route(
            "/readyz",
            get(move || {
                let readiness = health_worker.readiness(&readiness);
                async move {
                    let status = if readiness.available() {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    };
                    (status, Json(readiness))
                }
            }),
        )
        .route(
            "/healthz",
            get(|| async {
                Json(json!({"status":"ok","engine":"OrcaSlicer","version":ENGINE_VERSION}))
            }),
        )
        .route("/slice", post(dispatch))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(worker);
    axum::serve(tokio::net::TcpListener::bind(address).await?, app).await?;
    Ok(())
}

async fn dispatch(
    State(worker): State<Arc<SliceWorker>>,
    Json(request): Json<SliceRequest>,
) -> (StatusCode, Json<Value>) {
    match worker.dispatch(request).await {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code":error.code(),"error":error.to_string()})),
        ),
    }
}
