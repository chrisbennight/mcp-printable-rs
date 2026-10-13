//! Linux server-process memory observations under retained MCP work.
//! Clients and the fake bridge run outside the measured server process.

#![cfg(target_os = "linux")]

use std::{
    fs::File,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use printable_blender::fake_addon::{FakeAddon, ResponseSpec};
use serde_json::{Value, json};
use tokio::{net::TcpListener, process::Child, task::JoinSet};

const PROTOCOL: &str = "2025-11-25";
const REQUEST_BUDGET: Duration = Duration::from_secs(20);
const FAKE_LIFETIME: Duration = Duration::from_secs(30);

struct ServerProcess {
    child: Child,
    pid: u32,
}

impl ServerProcess {
    fn spawn(root: &Path, port: u16, blender_port: u16, bearer: &str) -> Self {
        let log = File::create(root.join("server.log")).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_printable-server"));
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("RUST_LOG", "warn")
            .env("PRINTABLE_HTTP_HOST", "127.0.0.1")
            .env("PRINTABLE_HTTP_PORT", port.to_string())
            .env("PRINTABLE_ALLOWED_HOSTS", "127.0.0.1")
            .env("PRINTABLE_WORKSPACE_ROOT", root)
            .env("PRINTABLE_MCP_BEARER", bearer)
            .env("BLENDER_HOST", "127.0.0.1")
            .env("BLENDER_PORT", blender_port.to_string())
            .env("OPENSCAD_BIN", "/bin/false")
            .env("FFMPEG_BIN", "/bin/false")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command.spawn().expect("start isolated server");
        let pid = child.id().expect("server PID");
        Self { child, pid }
    }

    fn peak_rss_kib(&self) -> Result<u64, String> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid))
            .map_err(|error| format!("server process status unavailable: {error}"))?;
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .ok_or_else(|| "Linux did not report server peak resident memory".into())
    }

    fn kill_group(&mut self) {
        if self.child.id().is_some() {
            // The unreaped child owns this separate process group. Killing it
            // also cleans up any fixture subprocess still owned by the server.
            unsafe {
                libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }

    async fn stop(mut self) -> Result<Value, String> {
        self.kill_group();
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(|_| "server fixture did not exit after forced teardown".to_string())?
            .map_err(|error| format!("server fixture wait failed: {error}"))?;
        if status.signal() != Some(libc::SIGKILL) {
            return Err(format!(
                "server exited before deliberate teardown: {status}"
            ));
        }
        Ok(
            json!({"signal": "SIGKILL", "observed_signal": status.signal(),
            "purpose": "dispose blocked measurement fixture; no graceful-drain claim"}),
        )
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.kill_group();
    }
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    bearer: &str,
    session: Option<&str>,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let mut request = http
        .post(format!("{base}/mcp"))
        .bearer_auth(bearer)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .json(body);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request
        .send()
        .await
        .map_err(|error| format!("fixture HTTP request failed: {error}"))
}

async fn rpc_result(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = response
        .text()
        .await
        .map_err(|error| format!("fixture response body failed: {error}"))?;
    if !status.is_success() {
        return Err(format!("fixture request was rejected with HTTP {status}"));
    }
    let envelope: Value = if content_type.contains("text/event-stream") {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .find(|value| value.get("result").is_some() || value.get("error").is_some())
            .ok_or_else(|| "fixture stream had no JSON-RPC response".to_string())?
    } else {
        serde_json::from_str(&body)
            .map_err(|error| format!("fixture JSON-RPC decoding failed: {error}"))?
    };
    if envelope.get("error").is_some() {
        return Err("fixture request returned a JSON-RPC error".into());
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| "fixture response had no result".into())
}

async fn observe(offered: usize) -> Result<Value, String> {
    let fake = FakeAddon::spawn(|command, _| {
        assert_eq!(command, "execute_code");
        // The measurement ends before this finite delay. A broken fixture
        // cannot keep an unbounded fake exchange alive after its server exits.
        ResponseSpec::SuccessAfter {
            result: json!({"result": null, "stdout": "", "stderr": "", "elapsed_ms": 1}),
            addon_version: None,
            delay: FAKE_LIFETIME,
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).expect("fixture credential randomness");
    let bearer: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let server = ServerProcess::spawn(root.path(), port, fake.port(), &bearer);
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_BUDGET)
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if http
                .get(format!("{base}/healthz"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "isolated server did not become live".to_string())?;

    let initialize = post(
        &http,
        &base,
        &bearer,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": PROTOCOL, "capabilities": {},
            "clientInfo": {"name": "admission-memory-client", "version": "1"}}}),
    )
    .await?;
    let session = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "legacy initialize did not return a session".to_string())?
        .to_owned();
    rpc_result(initialize).await?;
    let initialized = post(
        &http,
        &base,
        &bearer,
        Some(&session),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await?;
    if initialized.status().as_u16() != 202 {
        return Err("initialized notification was not accepted".into());
    }
    initialized
        .bytes()
        .await
        .map_err(|error| error.to_string())?;
    let before = server.peak_rss_kib()?;
    let code = format!("#{}\nresult = None", "retained argument ".repeat(32 * 1024));
    let started = Instant::now();
    let mut requests = JoinSet::new();
    for id in 0..offered {
        let http = http.clone();
        let base = base.clone();
        let bearer = bearer.clone();
        let session = session.clone();
        let body = json!({"jsonrpc": "2.0", "id": id + 10, "method": "tools/call",
            "params": {"name": "blender_execute", "arguments": {
                "code": code, "timeout_seconds": 1}}});
        requests.spawn(async move { post(&http, &base, &bearer, Some(&session), &body).await });
    }
    let mut streams = Vec::new();
    let mut refused = 0;
    tokio::time::timeout(REQUEST_BUDGET, async {
        while let Some(result) = requests.join_next().await {
            let response = result.map_err(|error| error.to_string())??;
            match response.status().as_u16() {
                200 => streams.push(response),
                429 => {
                    let error: Value = response.json().await.map_err(|error| error.to_string())?;
                    if error["error"] != "server_busy" {
                        return Err("admission refusal lost its machine error".into());
                    }
                    refused += 1;
                }
                status => return Err(format!("unexpected workload HTTP status {status}")),
            }
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "workload responses did not arrive within the observation budget".to_string())??;
    tokio::time::timeout(Duration::from_secs(2), async {
        while fake.commands().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| "accepted work never reached the fake bridge".to_string())?;
    if streams.len() + refused != offered || fake.commands() != ["execute_code"] {
        return Err("workload did not retain the offered calls behind one blocked command".into());
    }
    let blocked_peak = server.peak_rss_kib()?;
    let control_started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), async {
        let health = http
            .get(format!("{base}/healthz"))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !health.status().is_success() {
            return Err("health observation was refused".into());
        }
        let status = rpc_result(
            post(
                &http,
                &base,
                &bearer,
                Some(&session),
                &json!({
                "jsonrpc": "2.0", "id": 1000, "method": "tools/call",
                "params": {"name": "status", "arguments": {}}}),
            )
            .await?,
        )
        .await?;
        if status["isError"] != false || !status["structuredContent"].is_object() {
            return Err("status observation did not return its structured contract".into());
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "health and status exceeded the two-second budget".to_string())??;
    let control_ms = control_started.elapsed().as_secs_f64() * 1000.0;
    let measured_peak = server.peak_rss_kib()?;
    // Preserve each kernel reading and use the greatest observed high-water
    // value; process RSS accounting can change between these observations.
    let observed_peak = before.max(blocked_peak).max(measured_peak);
    let mut measurement = json!({
        "outcome": "completed", "protocol": PROTOCOL, "offered_calls": offered,
        "accepted_calls": streams.len(), "refused_calls": refused, "completed_calls": 0,
        "argument_utf8_bytes": code.len(), "backend_deliveries_while_blocked": fake.commands().len(),
        "waiting_calls": streams.len().saturating_sub(fake.commands().len()),
        "server_peak_before_kib": before, "server_peak_while_blocked_kib": blocked_peak,
        "server_peak_after_control_kib": measured_peak,
        "server_peak_observed_kib": observed_peak,
        "server_peak_growth_kib": observed_peak.saturating_sub(before),
        "control_response_ms": control_ms, "offer_and_control_ms": started.elapsed().as_secs_f64() * 1000.0,
        "memory_scope": "Linux VmHWM of the real server subprocess; excludes clients and fake bridge",
        "workload_scope": "one fresh server and legacy session; no model provider, native engine or GPU",
        "fake_response_delay_ms": FAKE_LIFETIME.as_millis()
    });
    // Disposing this blocked fixture deliberately does not establish that
    // disconnecting or normal shutdown cancelled an accepted mutation.
    measurement["teardown"] = server.stop().await?;
    drop(streams);
    Ok(measurement)
}

#[tokio::test]
#[ignore = "opt-in Linux memory measurements; admission behavior is covered by endpoint tests"]
async fn increasing_concurrency_records_isolated_server_peak_memory() {
    let mut records = Vec::new();
    for offered in [1, 16, 64, 128] {
        let record = observe(offered).await.unwrap_or_else(
            |error| json!({"outcome": "failed", "offered_calls": offered, "error": error}),
        );
        println!("ADMISSION_MEMORY_SAMPLE={record}");
        records.push(record);
    }
    println!("ADMISSION_MEMORY_EVIDENCE={}", json!(records));
    assert!(
        records
            .iter()
            .all(|record| record["outcome"] == "completed"),
        "one or more admission memory observations failed"
    );
}
