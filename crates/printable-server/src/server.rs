//! The axum router: `/healthz` liveness, `/readyz` dependency readiness, and
//! the streamable-HTTP `/mcp` endpoint.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::de::{MapAccess, SeqAccess, Visitor};
use tokio_util::sync::CancellationToken;
use tower::{ServiceBuilder, limit::GlobalConcurrencyLimitLayer};
use tower_http::trace::TraceLayer;

use printable_blender::{BlenderClient, ClientOptions};
use printable_workspace::Workspace;

use crate::{
    config::{BearerSecret, Settings},
    file_transfer,
    health::HealthResponse,
    mcp::PrintableServer,
    upload::CHUNK_MAX_DECODED,
};

/// Maximum accepted request body. The largest legitimate request carries one
/// chunk (or a single-shot write) of at most [`CHUNK_MAX_DECODED`] bytes, which
/// base64-encodes to ~`ceil(cap/3)*4` characters, plus a small JSON-RPC
/// envelope. Cap the body just above that so an oversized body is rejected (413)
/// before rmcp buffers and deserializes it — matching the per-request payload
/// cap the tool layer enforces, so no request retains a large body. Larger
/// artifacts arrive as a sequence of these bounded chunks, never one big body.
const MAX_REQUEST_BODY_BYTES: usize = CHUNK_MAX_DECODED.div_ceil(3) * 4 + 64 * 1024;

/// Maximum concurrent in-flight requests across all sessions. A request holds
/// its body (≤ [`MAX_REQUEST_BODY_BYTES`]) from parse until it completes, so the
/// per-request body cap alone leaves aggregate retained memory unbounded in the
/// number of queued requests. Bounding the count bounds that sum
/// (≤ `MAX_INFLIGHT_REQUESTS * MAX_REQUEST_BODY_BYTES`, ~45 MiB). Set generously
/// relative to a single-tenant deployment (one live session plus its calls) so
/// it never gates normal use; it exists so a hostile burst cannot sum to
/// unbounded memory. Acquired outside the body-limit layer, before the body is
/// read.
const MAX_INFLIGHT_REQUESTS: usize = 32;
const DUPLICATE_JSON_MEMBER: &str = "duplicate JSON object member";

/// Build the router. `/mcp` is the stateful streamable-HTTP transport, guarded
/// by browser Origin policy, the shared bearer, and the `Host` allowlist,
/// and wired to `cancel` so graceful shutdown tears sessions down; `/healthz`
/// is static liveness and `/readyz` is a sanitized live dependency probe.
///
/// Fallible: opening the confined workspace validates `PRINTABLE_WORKSPACE_ROOT`
/// (a non-directory is a startup config error). The backend handles are
/// constructed once here and shared across sessions; the streamable-HTTP factory
/// clones the `Arc`-backed server per session.
pub fn build_router(settings: &Settings, cancel: CancellationToken) -> anyhow::Result<Router> {
    let workspace = Arc::new(
        Workspace::open(
            settings.workspace_root.as_deref(),
            settings.blender_workspace_root.as_deref(),
        )
        .context("open confined workspace")?,
    );
    let blender = Arc::new(BlenderClient::new(
        settings.blender_host.clone(),
        settings.blender_port,
        ClientOptions::default(),
    ));
    let server = PrintableServer::new(workspace, blender, Arc::new(settings.clone()));
    let readiness_server = server.clone();
    let incoming_files = server.incoming_files();
    incoming_files.spawn_reaper(cancel.child_token());
    let published_files = server.published_files();
    published_files.spawn_reaper(cancel.child_token());

    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(cancel.child_token())
            .with_allowed_hosts(settings.allowed_hosts.clone()),
    );

    // Scope the body + concurrency limits to `/mcp` (not the whole router), so a
    // burst of slow tool calls holding every permit can never starve the
    // `/healthz` liveness probe, which must answer within the healthcheck
    // deadline. Concurrency is outermost so its permit is held while the body is
    // read (bounding aggregate retained request memory). The JSON boundary
    // enforces the per-request body limit before rebuilding the request for rmcp.
    let mcp = ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(
            settings.allowed_origins.clone(),
            require_allowed_origin,
        ))
        .layer(middleware::from_fn_with_state(
            settings.mcp_bearer.clone(),
            require_mcp_bearer,
        ))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_INFLIGHT_REQUESTS))
        .layer(middleware::from_fn(validate_json_request))
        .service(mcp_service);

    let downloads = Router::new()
        .route(
            "/file-transfers/download/{file_id}",
            get(file_transfer::download),
        )
        .with_state(published_files);

    let uploads = Router::new()
        .route(
            "/file-transfers/upload/{file_id}",
            axum::routing::put(crate::file_ingest::upload),
        )
        .with_state(incoming_files);

    Ok(Router::new()
        .merge(downloads)
        .merge(uploads)
        .route("/healthz", get(healthz))
        .route(
            "/readyz",
            get(move || {
                let server = readiness_server.clone();
                async move { readyz(server).await }
            }),
        )
        .nest_service("/mcp", mcp)
        .layer(TraceLayer::new_for_http()))
}

/// Unauthenticated process-liveness probe. Backend readiness is the
/// `printable_status` tool's concern, never this route's.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(HealthResponse::ok()))
}

async fn readyz(server: PrintableServer) -> Response {
    let readiness = server.readiness().await;
    let status = if readiness.available() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(readiness)).into_response()
}

async fn require_mcp_bearer(
    State(expected): State<BearerSecret>,
    request: Request,
    next: Next,
) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|candidate| expected.matches(candidate));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn require_allowed_origin(
    State(allowed): State<Vec<String>>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origins.next().is_some()
            || !allowed
                .iter()
                .any(|value| origin.as_bytes() == value.as_bytes()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn validate_json_request(request: Request, next: Next) -> Response {
    if request.method() != Method::POST {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the MCP limit",
            )
                .into_response();
        }
    };
    let may_contain_definitions = bytes
        .windows(b"\"defines\"".len())
        .any(|window| window == b"\"defines\"")
        || bytes.windows(2).any(|window| window == b"\\u");
    if may_contain_definitions
        && serde_json::from_slice::<UniqueJsonValue>(&bytes)
            .is_err_and(|error| error.to_string().contains(DUPLICATE_JSON_MEMBER))
    {
        return (StatusCode::BAD_REQUEST, DUPLICATE_JSON_MEMBER).into_response();
    }

    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

struct UniqueJsonValue;

impl<'de> serde::Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object members")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<UniqueJsonValue>()?.is_some() {}
        Ok(UniqueJsonValue)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut names = HashSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !names.insert(name) {
                return Err(serde::de::Error::custom(DUPLICATE_JSON_MEMBER));
            }
            map.next_value::<UniqueJsonValue>()?;
        }
        Ok(UniqueJsonValue)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tokio_util::sync::CancellationToken;

    use super::build_router;
    use crate::config::Settings;

    #[test]
    fn workspace_open_failure_retains_startup_context_and_io_cause() {
        let temporary = tempfile::tempdir().expect("workspace tempdir");
        let file = temporary.path().join("not-a-directory");
        fs::write(&file, b"not a workspace").expect("create non-directory workspace root");
        let mut settings = Settings::from_lookup(|key| {
            (key == "PRINTABLE_MCP_BEARER").then(|| {
                concat!(
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef"
                )
                .to_string()
            })
        })
        .expect("default settings");
        settings.workspace_root = Some(file);

        let error = build_router(&settings, CancellationToken::new())
            .expect_err("a workspace root must be a directory");

        assert_eq!(error.to_string(), "open confined workspace");
        assert!(
            error.chain().count() >= 2,
            "workspace context must retain its source error"
        );
    }
}
