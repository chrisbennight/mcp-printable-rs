//! The axum routers: `/healthz` liveness, `/readyz` dependency readiness, and
//! the streamable-HTTP `/mcp` endpoint, plus the optional read-only library
//! web page, which has a router and listener of its own.

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
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use printable_blender::{BlenderClient, ClientOptions};
use printable_workspace::Workspace;

use crate::{
    admission::{Admission, retain_response},
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

const DUPLICATE_JSON_MEMBER: &str = "duplicate JSON object member";

/// The routers for the server's listeners.
pub struct Routers {
    /// The MCP endpoint, file transfers and health routes.
    pub mcp: Router,
    /// The read-only project library page, present when `PRINTABLE_WEB_LISTEN`
    /// is set. It never shares a listener with the MCP endpoint.
    pub web: Option<Router>,
}

/// Build the MCP router; see [`build_routers`].
pub fn build_router(settings: &Settings, cancel: CancellationToken) -> anyhow::Result<Router> {
    Ok(build_routers(settings, cancel)?.mcp)
}

/// Build the routers. `/mcp` is the stateful streamable-HTTP transport, guarded
/// by browser Origin policy, the shared bearer, and the `Host` allowlist,
/// and wired to `cancel` so graceful shutdown tears sessions down; `/healthz`
/// is static liveness and `/readyz` is a sanitized live dependency probe.
///
/// Fallible: opening the confined workspace validates `PRINTABLE_WORKSPACE_ROOT`
/// (a non-directory is a startup config error). The backend handles are
/// constructed once here and shared across sessions; the streamable-HTTP factory
/// clones the `Arc`-backed server per session. The web page shares the
/// workspace and search index but none of the MCP routes or middleware.
pub fn build_routers(settings: &Settings, cancel: CancellationToken) -> anyhow::Result<Routers> {
    let workspace = Arc::new(
        Workspace::open(
            settings.workspace_root.as_deref(),
            settings.blender_workspace_root.as_deref(),
        )
        .context("open confined workspace")?,
    );
    if workspace.confined() {
        workspace
            .configure_storage_budget(settings.workspace_budget_bytes)
            .context("configure shared workspace storage budget")?;
    }
    settings
        .project_index
        .open()
        .context("open the project search index")?;
    let blender = Arc::new(BlenderClient::new(
        settings.blender_host.clone(),
        settings.blender_port,
        ClientOptions::default(),
    ));
    if let Some(printers) = &settings.printers {
        crate::printers::ledger::spawn(
            Arc::clone(printers),
            Arc::clone(&workspace),
            cancel.child_token(),
        );
    }
    // Without a confined workspace there are no projects to index.
    if workspace.confined() {
        crate::projects::index::spawn(
            Arc::clone(&settings.project_index),
            Arc::clone(&workspace),
            cancel.child_token(),
        );
    }
    let web = settings.web.as_ref().map(|web| {
        crate::web::router(
            web.clone(),
            Arc::clone(&workspace),
            Arc::clone(&settings.project_index),
        )
    });
    let server = PrintableServer::new(workspace, blender, Arc::new(settings.clone()));
    let readiness_server = server.clone();
    let incoming_files = server.incoming_files();
    incoming_files.spawn_reaper(cancel.child_token());
    let published_files = server.published_files();
    published_files.spawn_reaper(cancel.child_token());
    if let Some(recon) = &settings.recon {
        Arc::clone(recon).watch(cancel.child_token());
        server.announce_tool_changes(cancel.child_token());
    }

    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(cancel.child_token())
            .with_allowed_hosts(settings.allowed_hosts.clone()),
    );

    // Admission bounds body readers, retained tool work, and subscriptions
    // separately. Health routes stay outside those lanes, and control requests
    // retain capacity when ordinary calls occupy the backend.
    let admission = Admission::new(cancel.child_token());
    let mcp = ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(
            settings.allowed_origins.clone(),
            require_allowed_origin,
        ))
        .layer(middleware::from_fn_with_state(
            settings.mcp_bearer.clone(),
            require_mcp_bearer,
        ))
        .layer(middleware::from_fn_with_state(
            admission,
            validate_json_request,
        ))
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

    let mcp = Router::new()
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
        .layer(TraceLayer::new_for_http());
    Ok(Routers { mcp, web })
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

async fn validate_json_request(
    State(admission): State<Admission>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() == Method::GET {
        let Some(permit) = admission.subscription() else {
            return busy_response();
        };
        return retain_response(next.run(request).await, permit);
    }
    if request.method() != Method::POST {
        return next.run(request).await;
    }

    let Some(body_permit) = admission.body() else {
        return busy_response();
    };
    let (mut parts, body) = request.into_parts();
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

    let payload = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(payload) => payload,
        Err(_) => {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    };
    let Some(permit) = admission.request(&payload) else {
        return busy_response();
    };
    drop(payload);
    drop(body_permit);
    parts.extensions.insert(permit.retained());
    retain_response(
        next.run(Request::from_parts(parts, Body::from(bytes)))
            .await,
        permit,
    )
}

fn busy_response() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({
        "error": "server_busy", "message": "server admission is full; inspect active work before retrying"
    }))).into_response()
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
    use std::{collections::HashMap, fs, net::SocketAddr};

    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;

    use super::{build_router, build_routers};
    use crate::config::Settings;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        let mut values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        values.insert(
            "PRINTABLE_MCP_BEARER".into(),
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            )
            .into(),
        );
        Settings::from_lookup(move |key| values.get(key).cloned()).expect("valid settings")
    }

    fn get(path: &str) -> Request<Body> {
        let mut request = Request::get(path)
            .header("x-printable-user", "alice")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
        request
    }

    #[tokio::test]
    async fn the_web_page_has_its_own_router_only_when_configured() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let root = workspace.path().to_str().expect("UTF-8 path");
        let cancel = CancellationToken::new();
        let off = build_routers(
            &settings(&[("PRINTABLE_WORKSPACE_ROOT", root)]),
            cancel.child_token(),
        )
        .expect("routers");
        assert!(off.web.is_none());
        let on = build_routers(
            &settings(&[
                ("PRINTABLE_WORKSPACE_ROOT", root),
                ("PRINTABLE_WEB_LISTEN", "127.0.0.1:8080"),
                ("PRINTABLE_WEB_TRUSTED_PROXIES", "127.0.0.1"),
                ("PRINTABLE_WEB_USER_HEADER", "X-Printable-User"),
                ("PRINTABLE_WEB_USERS", "alice"),
            ]),
            cancel.child_token(),
        )
        .expect("routers");
        let web = on
            .web
            .expect("a web router when PRINTABLE_WEB_LISTEN is set");
        // The MCP router behaves the same either way and serves no web pages.
        for mcp in [off.mcp, on.mcp] {
            let health = mcp
                .clone()
                .oneshot(get("/healthz"))
                .await
                .expect("response");
            assert_eq!(health.status(), StatusCode::OK);
            for path in ["/", "/static/app.css", "/projects/desk"] {
                let page = mcp.clone().oneshot(get(path)).await.expect("response");
                assert_eq!(page.status(), StatusCode::NOT_FOUND, "{path}");
            }
        }
        // The web router serves pages and none of the MCP routes.
        let page = web.clone().oneshot(get("/")).await.expect("response");
        assert_eq!(page.status(), StatusCode::OK);
        for path in ["/healthz", "/readyz", "/mcp", "/file-transfers/download/x"] {
            let response = web.clone().oneshot(get(path)).await.expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        cancel.cancel();
    }

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
