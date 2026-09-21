//! End-to-end coverage for the streamable-HTTP `/mcp` endpoint, `/healthz`
//! liveness, and sanitized `/readyz`, served by `server::build_router`.
//!
//! This binds the real router on a loopback ephemeral port and speaks the
//! stateful streamable-HTTP handshake a client must perform:
//!
//!   initialize → notifications/initialized → tools/list → tools/call → resources/list
//!
//! It asserts the MCP *contract*, not rmcp internals: the server identifies as
//! `printable_blender`, the full release catalog is listed, `printable_status` runs
//! end-to-end (with Blender down), an unknown tool is a JSON-RPC error,
//! delivered product guidance is readable, unpublished guidance stays
//! unavailable, and an oversized request
//! body is rejected by the transport limit. This is what would break if a
//! future rmcp bump silently changed the JSON wire shape or session handshake.
//!
//! Side-effect free: everything binds on `127.0.0.1`; no credentials, external
//! network, or filesystem are touched.

use std::collections::BTreeSet;

use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use printable_server::config::{BearerSecret, Settings};
use printable_server::server::build_router;
use printable_server::upload::CHUNK_MAX_DECODED;

const TEST_MCP_BEARER: &str = concat!(
    "0123456789abcdef",
    "0123456789abcdef",
    "0123456789abcdef",
    "0123456789abcdef"
);

/// The MCP wire protocol revision the client negotiates. Read from rmcp so the
/// test tracks the dependency's supported set rather than pinning a literal.
fn protocol_version() -> &'static str {
    ProtocolVersion::LATEST.as_str()
}

/// Settings for the test router — built directly (not from the environment), so
/// the test never touches process-global state. Port 0 is unused (the listener
/// binds its own ephemeral port); the `Host` allowlist admits the loopback
/// address the client dials.
fn test_settings(blender_port: u16) -> Settings {
    Settings {
        http_host: "127.0.0.1".to_string(),
        http_port: 0,
        blender_host: "127.0.0.1".to_string(),
        blender_port,
        render_worker_host: None,
        render_worker_port: 9876,
        workspace_root: None,
        blender_workspace_root: None,
        openscad_bin: Some(std::path::PathBuf::from("/bin/false")),
        scad_concurrency: 2,
        ffmpeg_bin: std::path::PathBuf::from("ffmpeg"),
        render_job_queue_depth: 16,
        geometry_worker_bin: None,
        geometry_worker_memory_bytes: 1024 * 1024 * 1024,
        mcp_bearer: BearerSecret::parse(TEST_MCP_BEARER.to_string()).expect("test bearer is valid"),
        allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        allowed_origins: Vec::new(),
    }
}

/// Reserve then release a loopback port so a connection to it is refused
/// immediately — makes the `printable_status` Blender probe fail fast instead of
/// waiting out its 5-second deadline.
async fn closed_blender_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve a port");
    listener.local_addr().expect("local addr").port()
    // listener dropped here → the port is now closed (connection refused).
}

/// A running server bound on a loopback ephemeral port. Holding it keeps the
/// serve task alive; dropping (via `shutdown`) tears it down.
struct TestServer {
    base: String,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn shutdown(self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

async fn start() -> TestServer {
    let settings = test_settings(closed_blender_port().await);
    start_with_settings(settings).await
}

async fn start_with_settings(settings: Settings) -> TestServer {
    let cancel = CancellationToken::new();
    let router = build_router(&settings, cancel.clone()).expect("router builds");

    // Bind synchronously before spawning so the client can connect immediately
    // with no readiness race.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    TestServer {
        base: format!("http://{addr}"),
        cancel,
        handle,
    }
}

/// POST one JSON-RPC message to `/mcp` with the MCP content-negotiation headers,
/// forwarding the session id once the handshake has assigned one.
async fn post(
    http: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    body: &Value,
) -> reqwest::Response {
    let mut req = http
        .post(url)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {TEST_MCP_BEARER}"),
        )
        .header("mcp-protocol-version", protocol_version())
        .json(body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    req.send().await.expect("request reaches the server")
}

/// Pull the JSON-RPC response object out of a `/mcp` response body, which the
/// stateful transport frames as SSE (`data: {…}`) or, when negotiated, bare
/// JSON. Returns the first framed object that is a JSON-RPC response.
fn extract_jsonrpc(content_type: &str, body: &str) -> Value {
    if content_type.contains("text/event-stream") {
        let mut data = String::new();
        for line in body.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            } else if line.is_empty() && !data.is_empty() {
                if let Ok(v) = serde_json::from_str::<Value>(&data)
                    && (v.get("result").is_some() || v.get("error").is_some())
                {
                    return v;
                }
                data.clear();
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data)
            && (v.get("result").is_some() || v.get("error").is_some())
        {
            return v;
        }
        panic!("no JSON-RPC response found in SSE stream: {body}");
    }
    serde_json::from_str(body.trim()).expect("response body is JSON")
}

/// Read the full JSON-RPC envelope (`result` or `error`) from a response.
async fn rpc_envelope(resp: reqwest::Response) -> Value {
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let text = resp.text().await.expect("read response body");
    extract_jsonrpc(&ct, &text)
}

/// Unwrap a JSON-RPC envelope's `result`, panicking on a JSON-RPC `error`.
async fn rpc_result(resp: reqwest::Response) -> Value {
    let envelope = rpc_envelope(resp).await;
    assert!(
        envelope.get("error").is_none(),
        "unexpected JSON-RPC error: {envelope}"
    );
    envelope
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("response has neither result nor error: {envelope}"))
}

#[tokio::test]
async fn origin_policy_rejects_browser_requests_by_default_on_every_mcp_method() {
    let server = start().await;
    let http = reqwest::Client::new();
    let unauthenticated = http
        .get(format!("{}/mcp", server.base))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .expect("origin rejection before authentication");
    assert_eq!(unauthenticated.status().as_u16(), 403);
    for method in [
        reqwest::Method::POST,
        reqwest::Method::GET,
        reqwest::Method::DELETE,
        reqwest::Method::OPTIONS,
    ] {
        for origin in ["https://app.example.com", "null", "", "not-an-origin"] {
            let response = http
                .request(method.clone(), format!("{}/mcp", server.base))
                .bearer_auth(TEST_MCP_BEARER)
                .header("Origin", origin)
                .send()
                .await
                .expect("origin request");
            assert_eq!(response.status().as_u16(), 403, "{method}: {origin:?}");
        }
    }
    server.shutdown().await;
}

#[tokio::test]
async fn allowed_origin_does_not_bypass_host_or_bearer_and_must_be_single_and_exact() {
    let mut settings = test_settings(closed_blender_port().await);
    settings.allowed_origins = vec!["https://app.example.com".to_string()];
    let server = start_with_settings(settings).await;
    let http = reqwest::Client::new();
    let request = || {
        http.post(format!("{}/mcp", server.base))
            .header("Accept", "application/json, text/event-stream")
            .json(
                &json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion": protocol_version(), "capabilities":{},
                    "clientInfo":{"name":"origin-test","version":"1"}
                }}),
            )
    };
    for origin in [None, Some("https://app.example.com")] {
        let mut req = request().bearer_auth(TEST_MCP_BEARER);
        if let Some(origin) = origin {
            req = req.header("Origin", origin);
        }
        assert_eq!(req.send().await.expect("initialize").status().as_u16(), 200);
    }
    for origin in [
        "http://app.example.com",
        "https://app.example.com:444",
        "https://app.example.com.evil.test",
        "https://app.example.com/path",
        "https://app.example.com https://evil.test",
        "null",
    ] {
        assert_eq!(
            request()
                .bearer_auth(TEST_MCP_BEARER)
                .header("Origin", origin)
                .send()
                .await
                .expect("disallowed origin")
                .status()
                .as_u16(),
            403
        );
    }
    assert_eq!(
        request()
            .bearer_auth(TEST_MCP_BEARER)
            .header("Origin", "https://app.example.com")
            .header("Origin", "https://evil.test")
            .send()
            .await
            .expect("duplicate origins")
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        request()
            .header("Origin", "https://app.example.com")
            .send()
            .await
            .expect("missing bearer")
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        request()
            .bearer_auth(TEST_MCP_BEARER)
            .header("Origin", "https://app.example.com")
            .header("Host", "evil.test")
            .send()
            .await
            .expect("bad host")
            .status()
            .as_u16(),
        403
    );
    server.shutdown().await;
}

#[tokio::test]
async fn healthz_returns_the_exact_liveness_body() {
    let server = start().await;
    let http = reqwest::Client::new();

    let resp = http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .expect("healthz request");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(ct.contains("application/json"), "content-type: {ct}");
    let body = resp.text().await.expect("healthz body");
    assert_eq!(body, r#"{"status":"ok","server":"printable_blender"}"#);

    server.shutdown().await;
}

#[tokio::test]
async fn readyz_fails_closed_without_leaking_dependency_configuration() {
    let server = start().await;
    let response = reqwest::get(format!("{}/readyz", server.base))
        .await
        .expect("readyz request");
    assert_eq!(response.status().as_u16(), 503);
    let body: Value = response.json().await.expect("readyz JSON");

    assert_eq!(body["status"], json!("blocked"));
    assert_eq!(body["checks"]["blender"], json!(false));
    assert_eq!(body["checks"]["workspace"], json!(false));
    assert_eq!(body["checks"]["openscad"], json!(false));
    assert_eq!(body["work"]["queued"], json!(false));
    assert_eq!(body["work"]["running"], json!(false));
    assert!(
        body["codes"]
            .as_array()
            .is_some_and(|codes| codes.contains(&json!("blender_unavailable")))
    );

    let wire = serde_json::to_string(&body).expect("serialize readyz");
    for forbidden in [
        "workspace_root",
        "blender_workspace_root",
        "binary",
        "host",
        "port",
        "error",
    ] {
        assert!(!wire.contains(forbidden), "leaked {forbidden}: {wire}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_requires_the_exact_shared_bearer() {
    let server = start().await;
    let mcp = format!("{}/mcp", server.base);
    let http = reqwest::Client::new();

    let missing = http
        .post(&mcp)
        .send()
        .await
        .expect("missing bearer request");
    assert_eq!(missing.status().as_u16(), 401);

    let wrong = http
        .post(&mcp)
        .header(reqwest::header::AUTHORIZATION, "Bearer wrong")
        .send()
        .await
        .expect("wrong bearer request");
    assert_eq!(wrong.status().as_u16(), 401);

    let wrong_case = http
        .post(&mcp)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", TEST_MCP_BEARER.to_uppercase()),
        )
        .send()
        .await
        .expect("case-changed bearer request");
    assert_eq!(wrong_case.status().as_u16(), 401);

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_handshake_lists_tools_calls_status_and_resources() {
    let server = start().await;
    let mcp = format!("{}/mcp", server.base);
    let http = reqwest::Client::new();

    // 1. initialize — the response header carries the session id the stateful
    //    transport binds every subsequent request to.
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version(),
            "capabilities": {},
            "clientInfo": {"name": "mcp-endpoint-test", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    let resp = post(&http, &mcp, None, &init).await;
    assert!(
        resp.status().is_success(),
        "initialize should succeed, got {}",
        resp.status()
    );
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("stateful transport assigns a session id on initialize");
    let init_result = rpc_result(resp).await;
    assert_eq!(
        init_result["serverInfo"]["name"].as_str(),
        Some("printable_blender"),
        "serverInfo.name must identify Printable: {init_result}"
    );
    assert_eq!(
        init_result["serverInfo"]["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "serverInfo.version must be the crate version: {init_result}"
    );
    assert!(
        init_result.get("protocolVersion").is_some(),
        "initialize result missing protocolVersion: {init_result}"
    );
    assert!(
        init_result.get("capabilities").is_some(),
        "initialize result missing capabilities: {init_result}"
    );
    assert!(
        init_result["capabilities"]["resources"].is_object(),
        "resources capability must be advertised with the product kit: {init_result}"
    );
    assert!(
        init_result["instructions"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "initialize result missing instructions: {init_result}"
    );
    let instructions = init_result["instructions"]
        .as_str()
        .expect("instructions checked above");
    assert!(
        instructions.len() <= 1600,
        "initial guidance exceeds its text budget"
    );
    for required in [
        "expected_scene",
        "Never retry",
        "checkpoint",
        "artifacts",
        "printable://modeling/blender-v1",
        "printable://design/product-v1",
        "printable://render/product-v1",
    ] {
        assert!(
            instructions.contains(required),
            "missing recovery or guidance link: {required}"
        );
    }

    // 2. notifications/initialized — a notification (no id); the transport 2xxs
    //    it and the session becomes usable.
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let resp = post(&http, &mcp, Some(&session), &initialized).await;
    assert!(
        resp.status().is_success(),
        "initialized notification should be accepted, got {}",
        resp.status()
    );

    // 3. tools/list — the exact release catalog is advertised.
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let result = rpc_result(post(&http, &mcp, Some(&session), &list).await).await;
    let names: BTreeSet<&str> = result["tools"]
        .as_array()
        .expect("tools/list result carries a tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "status",
            "inspect",
            "edit",
            "blender_execute",
            "scene",
            "scad_build",
            "view",
            "render",
            "compare_renders",
            "validate_mesh",
            "analyze_assembly",
            "job",
            "project",
            "artifact",
        ]),
        "tool catalog: {result}"
    );

    // 4. tools/call printable_status — runs end-to-end. Blender is down (its port
    //    was reserved then closed), so status reports it unavailable rather than
    //    erroring; the tool result is not a JSON-RPC error and not `isError`.
    let call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "status", "arguments": {}},
    });
    let result = rpc_result(post(&http, &mcp, Some(&session), &call).await).await;
    assert_ne!(
        result["isError"],
        json!(true),
        "printable_status must not be a tool error: {result}"
    );
    let text = result["content"]
        .as_array()
        .and_then(|c| c.iter().find_map(|b| b.get("text").and_then(Value::as_str)))
        .expect("status result carries text content");
    let status: Value = serde_json::from_str(text).expect("status text content is JSON");
    assert_eq!(
        status["transport"],
        json!("streamable-http"),
        "status: {status}"
    );
    assert_eq!(
        status["blender"]["available"],
        json!(false),
        "blender is down: {status}"
    );
    assert_eq!(
        status["workspace"]["confined"],
        json!(false),
        "unconfined: {status}"
    );

    // 5. tools/call unknown name — a JSON-RPC method-not-found error.
    let bad = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {"name": "nonexistent", "arguments": {}},
    });
    let envelope = rpc_envelope(post(&http, &mcp, Some(&session), &bad).await).await;
    assert_eq!(
        envelope["error"]["code"].as_i64(),
        Some(-32601),
        "unknown tool must be JSON-RPC method-not-found (-32601): {envelope}"
    );

    // 6. Duplicate JSON members are rejected before rmcp can coalesce them.
    let duplicate_definition = r#"{
        "jsonrpc":"2.0",
        "id":5,
        "method":"tools/call",
        "params":{
            "name":"scad_build",
            "arguments":{
                "action":"mesh",
                "params":{
                "source":"cube(1);",
                "path":"duplicate.stl",
                "defines":{"width":70,"\u0077idth":71}
                }
            }
        }
    }"#;
    let resp = http
        .post(&mcp)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {TEST_MCP_BEARER}"),
        )
        .header("mcp-protocol-version", protocol_version())
        .header("mcp-session-id", &session)
        .body(duplicate_definition)
        .send()
        .await
        .expect("duplicate request reaches the server");
    assert_eq!(
        resp.status().as_u16(),
        400,
        "duplicate definitions must fail at the raw JSON boundary"
    );

    // 7. resources/list — only delivered guidance is advertised.
    let rlist = json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"});
    let result = rpc_result(post(&http, &mcp, Some(&session), &rlist).await).await;
    let resources = result["resources"]
        .as_array()
        .expect("resources/list carries a resources array");
    assert_eq!(resources.len(), 4, "resource catalog: {result}");
    assert_eq!(
        resources[0]["uri"],
        json!("printable://modeling/blender-v1")
    );
    assert_eq!(resources[1]["uri"], json!("printable://design/product-v1"));
    assert_eq!(resources[2]["uri"], json!("printable://render/product-v1"));

    assert_eq!(resources[3]["uri"], json!("printable://contracts"));
    let contract_read = json!({"jsonrpc":"2.0","id":79,"method":"resources/read",
        "params":{"uri":"printable://contracts/view/section"}});
    let contract_result = rpc_result(post(&http, &mcp, Some(&session), &contract_read).await).await;
    let contract: serde_json::Value =
        serde_json::from_str(contract_result["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(contract["tool"], "view");
    assert_eq!(contract["action"], "section");
    jsonschema::validator_for(&contract["inputSchema"]).unwrap();

    let modeling_read = json!({"jsonrpc":"2.0","id":80,"method":"resources/read",
        "params":{"uri":"printable://modeling/blender-v1"}});
    let modeling = rpc_result(post(&http, &mcp, Some(&session), &modeling_read).await).await;
    assert_eq!(
        modeling["contents"][0]["text"],
        include_str!("../resources/blender-modeling-v1.md")
    );

    // 8. resources/read — delivered design guidance is complete and readable.
    let read = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "resources/read",
        "params": {"uri": "printable://design/product-v1"},
    });
    let result = rpc_result(post(&http, &mcp, Some(&session), &read).await).await;
    let text = result["contents"][0]["text"]
        .as_str()
        .expect("product resource text");
    for required in [
        "pbl_shell",
        "pbl_horizontal_bore_cutter",
        "maximum_overhang_degrees",
        "not_certified",
        "analyze_assembly",
    ] {
        assert!(
            text.contains(required),
            "resource omitted {required}: {text}"
        );
    }

    // 9. Product presentation guidance is published with the renderer.
    let read = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "resources/read",
        "params": {"uri": "printable://render/product-v1"},
    });
    let result = rpc_result(post(&http, &mcp, Some(&session), &read).await).await;
    let text = result["contents"][0]["text"]
        .as_str()
        .expect("product render resource text");
    for required in [
        "engineering",
        "studio_neutral",
        "studio_dark",
        "15% margin",
        "source_state_verified",
        "bevel modifier",
    ] {
        assert!(
            text.contains(required),
            "render resource omitted {required}: {text}"
        );
    }

    server.shutdown().await;
}

#[tokio::test]
async fn published_artifacts_stream_as_immutable_raw_bytes_with_one_use_authority() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest as _, Sha256};

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let mut settings = test_settings(closed_blender_port().await);
    settings.workspace_root = Some(workspace.path().to_path_buf());
    let server = start_with_settings(settings).await;
    let http = reqwest::Client::new();
    let mcp = format!("{}/mcp", server.base);

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version(),
            "capabilities": {},
            "clientInfo": {"name": "file-transfer-test", "version": "1"},
        },
    });
    let response = post(&http, &mcp, None, &init).await;
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .expect("initialize assigns a session");
    let _ = rpc_result(response).await;
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert!(
        post(&http, &mcp, Some(&session), &initialized)
            .await
            .status()
            .is_success()
    );

    for (index, (suffix, media_type)) in [
        ("stl", "model/stl"),
        ("png", "image/png"),
        ("blend", "application/octet-stream"),
        ("mp4", "video/mp4"),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("handoff-{index}.{suffix}");
        let original = format!("raw-{suffix}-artifact-bytes").into_bytes();
        std::fs::write(workspace.path().join(&path), &original).expect("seed artifact");

        let publish = json!({
            "jsonrpc": "2.0",
            "id": 10 + index * 2,
            "method": "tools/call",
            "params": {
                "name": "artifact",
                "arguments": {"action": "publish", "params": {"path": path}},
            },
        });
        let result = rpc_result(post(&http, &mcp, Some(&session), &publish).await).await;
        assert_eq!(result["isError"], json!(false), "publish result: {result}");
        let text_value: Value = serde_json::from_str(
            result["content"][0]["text"]
                .as_str()
                .expect("publication JSON text"),
        )
        .expect("publication descriptor JSON");
        assert_eq!(text_value, result["structuredContent"]);
        let schema = Value::Object(
            printable_server::tools::output::schema("artifact")
                .as_ref()
                .clone(),
        );
        jsonschema::validator_for(&schema)
            .expect("artifact output schema")
            .validate(&text_value)
            .expect("published descriptor follows the output contract");
        let file = &result["structuredContent"]["file"];
        let uri = file["uri"].as_str().expect("private file URI");
        assert!(uri.starts_with("mcp-file://printable/"));
        assert_eq!(file["name"], json!(path));
        assert_eq!(file["mimeType"], json!(media_type));
        assert_eq!(file["size"], json!(original.len()));
        assert_eq!(file["digest"]["algorithm"], json!("sha-256"));
        let declared_digest = URL_SAFE_NO_PAD
            .decode(file["digest"]["value"].as_str().expect("digest value"))
            .expect("digest is base64url metadata");
        assert_eq!(declared_digest, Sha256::digest(&original).as_slice());
        assert!(
            !result["content"].to_string().contains("data_base64"),
            "artifact bytes must not enter MCP content: {result}"
        );

        std::fs::write(workspace.path().join(&path), b"replacement bytes")
            .expect("replace source after publication");

        if index == 0 {
            let missing_capability = json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "files/authorizeDownload",
                "params": {"uri": uri},
            });
            let refusal =
                rpc_envelope(post(&http, &mcp, Some(&session), &missing_capability).await).await;
            assert!(
                refusal["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("declared HTTP download support")),
                "authorization without the file capability must fail: {refusal}"
            );

            let unsupported_transport = json!({
                "jsonrpc": "2.0",
                "id": 101,
                "method": "files/authorizeDownload",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/clientCapabilities": {
                            "files": {"download": true, "transports": ["https"]}
                        }
                    },
                    "uri": uri,
                },
            });
            let refusal =
                rpc_envelope(post(&http, &mcp, Some(&session), &unsupported_transport).await).await;
            assert!(
                refusal["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("declared HTTP download support")),
                "authorization without HTTP transport support must fail: {refusal}"
            );
        }

        let authorize = json!({
            "jsonrpc": "2.0",
            "id": 11 + index * 2,
            "method": "files/authorizeDownload",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/clientCapabilities": {
                        "files": {"download": true, "transports": ["https", "http"]}
                    }
                },
                "uri": uri,
            },
        });
        let authorization = rpc_result(post(&http, &mcp, Some(&session), &authorize).await).await;
        assert_eq!(authorization["file"], *file);
        assert_eq!(authorization["download"]["transport"], json!("http"));
        assert_eq!(authorization["download"]["method"], json!("GET"));
        let download_url = authorization["download"]["url"]
            .as_str()
            .expect("download URL");
        let bearer = authorization["download"]["headers"]["Authorization"]
            .as_str()
            .expect("download bearer");

        let response = http
            .get(download_url)
            .header(reqwest::header::AUTHORIZATION, bearer)
            .send()
            .await
            .expect("authorized raw-byte download");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(media_type)
        );
        assert_eq!(
            response.bytes().await.expect("download body").as_ref(),
            original
        );

        let replay = http
            .get(download_url)
            .header(reqwest::header::AUTHORIZATION, bearer)
            .send()
            .await
            .expect("replayed download request");
        assert_eq!(replay.status().as_u16(), 401);
    }

    server.shutdown().await;
}

#[tokio::test]
async fn workflow_artifact_upload_and_job_queries_have_separate_lifecycles() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let mut settings = test_settings(closed_blender_port().await);
    settings.workspace_root = Some(workspace.path().to_path_buf());
    let server = start_with_settings(settings).await;
    let http = reqwest::Client::new();
    let mcp = format!("{}/mcp", server.base);
    let response = post(
        &http,
        &mcp,
        None,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": protocol_version(), "capabilities": {},
                "clientInfo": {"name": "workflow-test", "version": "1"}}
        }),
    )
    .await;
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("session id")
        .to_owned();
    let _ = rpc_result(response).await;
    assert!(
        post(
            &http,
            &mcp,
            Some(&session),
            &json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            })
        )
        .await
        .status()
        .is_success()
    );

    let call = |id: u64, name: &str, arguments: Value| {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
    };
    let begun = rpc_result(
        post(
            &http,
            &mcp,
            Some(&session),
            &call(
                2,
                "artifact",
                json!({
                    "action": "upload_begin", "params": {"path": "model.stl"}
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(begun["isError"], false);
    let upload_id = begun["structuredContent"]["upload_id"]
        .as_str()
        .expect("upload handle");
    assert!(!workspace.path().join("model.stl").exists());

    let rejected = rpc_result(post(&http, &mcp, Some(&session), &call(3, "job", json!({
        "action": "upload_chunk", "params": {"upload_id": upload_id, "data_base64": "YWJj"}
    }))).await).await;
    assert_eq!(rejected["isError"], true);
    assert_eq!(rejected["structuredContent"]["tool"], "job");
    assert_eq!(rejected["structuredContent"]["error"]["code"], "validation");

    let chunk = rpc_result(post(&http, &mcp, Some(&session), &call(4, "artifact", json!({
        "action": "upload_chunk", "params": {"upload_id": upload_id, "data_base64": "YWJj"}
    }))).await).await;
    assert_eq!(chunk["isError"], false);
    assert_eq!(chunk["structuredContent"]["bytes_written"], 3);
    let committed = rpc_result(
        post(
            &http,
            &mcp,
            Some(&session),
            &call(
                5,
                "artifact",
                json!({
                    "action": "upload_commit", "params": {"upload_id": upload_id}
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(committed["isError"], false);
    assert_eq!(
        std::fs::read(workspace.path().join("model.stl")).expect("committed bytes"),
        b"abc"
    );

    let jobs = rpc_result(
        post(
            &http,
            &mcp,
            Some(&session),
            &call(
                6,
                "job",
                json!({
                    "action": "list", "params": {}
                }),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(jobs["isError"], false);
    let text: Value = serde_json::from_str(
        jobs["content"][0]["text"]
            .as_str()
            .expect("compatibility text"),
    )
    .expect("JSON compatibility content");
    assert_eq!(jobs["structuredContent"], text);
    assert_eq!(jobs["structuredContent"]["jobs"], json!([]));
    {
        let arguments = json!({"action": "list", "params": {"path": "."}});
        let listed =
            rpc_result(post(&http, &mcp, Some(&session), &call(7, "artifact", arguments)).await)
                .await;
        assert_eq!(listed["isError"], false);
        assert!(listed["structuredContent"].is_object());
        let text: Value =
            serde_json::from_str(listed["content"][0]["text"].as_str().expect("listing text"))
                .expect("listing JSON");
        assert_eq!(listed["structuredContent"], text);
        assert!(
            text["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .any(|entry| entry["path"] == "model.stl")
        );
    }
    let removed = rpc_envelope(
        post(
            &http,
            &mcp,
            Some(&session),
            &call(8, "printable_workspace_list", json!({"path": "."})),
        )
        .await,
    )
    .await;
    assert_eq!(removed["error"]["code"], -32601);
    server.shutdown().await;
}

#[tokio::test]
async fn oversized_request_body_is_rejected_by_the_transport_limit() {
    let server = start().await;
    let http = reqwest::Client::new();
    let mcp = format!("{}/mcp", server.base);

    // A body larger than the per-request cap (~4/3 * CHUNK_MAX_DECODED) must be
    // refused by the transport before it is buffered — regression-checking the
    // RequestBodyLimitLayer that motivates the chunked write surface. Twice the
    // chunk size always exceeds the cap regardless of its exact value, and the
    // limit fires before the session handshake, so no session id is needed.
    //
    // The layer rejects by Content-Length without draining the body, so early
    // rejection surfaces two ways depending on whether the client finishes
    // writing before the server resets: a 413 response, or a connection reset
    // mid-upload. Both are refusals; only an accepted body is a failure (without
    // the limit the server drains the body and answers with a non-413 status).
    let oversized = json!({ "filler": "A".repeat(2 * CHUNK_MAX_DECODED) });
    let result = http
        .post(&mcp)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {TEST_MCP_BEARER}"),
        )
        .header("mcp-protocol-version", protocol_version())
        .json(&oversized)
        .send()
        .await;
    match result {
        Ok(resp) => assert_eq!(
            resp.status().as_u16(),
            413,
            "an accepted oversized body must instead be rejected with 413"
        ),
        Err(err) => assert!(
            !err.is_timeout() && (err.is_request() || err.is_body()),
            "oversized body refused mid-upload; unexpected error: {err:?}"
        ),
    }

    server.shutdown().await;
}
