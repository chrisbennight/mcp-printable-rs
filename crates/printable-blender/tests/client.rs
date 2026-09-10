//! Integration tests for the Blender bridge client, driven by the in-process
//! `FakeAddon`. Hermetic: loopback sockets only, no real Blender.

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use printable_blender::fake_addon::{FakeAddon, ResponseSpec};
use printable_blender::{
    BlenderClient, BlenderError, ClientOptions, Connector, Deadline, Params, Phase,
};
use serde_json::{Value, json};
use tokio::net::TcpStream;

fn opts() -> ClientOptions {
    ClientOptions {
        client_version: Some("0.2.5".into()),
        ..Default::default()
    }
}

fn client(host: String, port: u16) -> BlenderClient {
    BlenderClient::new(host, port, opts())
}

fn deadline() -> Deadline {
    Deadline::new(Duration::from_secs(5))
}

/// The empty params object (`{}`) — the common case for tests.
fn no_params() -> Params {
    Params::new()
}

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn success_returns_result_and_observes_version() {
    let fake = FakeAddon::ok("get_scene_info", json!({"objects": []}), "0.2.5").await;
    let c = client(fake.host(), fake.port());

    let result = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap();
    assert_eq!(result, json!({"objects": []}));
    assert_eq!(c.addon_version().as_deref(), Some("0.2.5"));
    // Matching versions → no warning.
    assert!(c.pop_version_warning().is_none());
    // One command, one connection (connect-per-command).
    assert_eq!(fake.connection_count(), 1);
}

#[tokio::test]
async fn version_mismatch_warns_once() {
    let fake = FakeAddon::ok("get_scene_info", json!({}), "0.2.4").await;
    let c = client(fake.host(), fake.port());
    c.send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap();
    let w = c.pop_version_warning().expect("mismatch should warn");
    assert!(w.contains("0.2.4") && w.contains("0.2.5"));
    assert!(c.pop_version_warning().is_none(), "one-shot");
}

#[tokio::test]
async fn addon_error_surfaces_and_is_not_retried() {
    const SECRET_PATH: &str = "/workspace/customer-secret.blend";
    const SECRET_TOKEN: &str = "token=shh";
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::Error {
        message: "boom".into(),
        traceback: Some(format!("Traceback {SECRET_PATH} {SECRET_TOKEN}")),
        addon_version: Some("0.2.5".into()),
    })
    .await;
    let c = client(fake.host(), fake.port());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_writer(move || SharedWriter(writer.clone()))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    let err = c
        .send_value("boolean", no_params(), deadline())
        .await
        .unwrap_err();
    drop(guard);
    assert_eq!(err.code(), "addon");
    let display = err.to_string();
    let debug = format!("{err:?}");
    assert_eq!(display, "Blender error: boom");
    let logs = String::from_utf8(captured.lock().expect("log buffer lock").clone())
        .expect("logs are UTF-8");
    assert!(logs.contains("traceback suppressed"), "logs: {logs}");
    for marker in [SECRET_PATH, SECRET_TOKEN] {
        assert!(
            !display.contains(marker),
            "display leaked {marker}: {display}"
        );
        assert!(!debug.contains(marker), "debug leaked {marker}: {debug}");
        assert!(!logs.contains(marker), "logs leaked {marker}: {logs}");
    }
    // Critical safety property: a command that reached the addon is NEVER
    // retried (it may have mutated state). Exactly one connection.
    assert_eq!(fake.connection_count(), 1);
}

#[tokio::test]
async fn recv_timeout_fails_within_budget() {
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::Hang).await;
    let c = client(fake.host(), fake.port());
    let short = Deadline::new(Duration::from_millis(200));

    let start = std::time::Instant::now();
    let err = c
        .send_value("get_scene_info", no_params(), short)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "timeout");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "must fail near the budget"
    );
}

#[tokio::test]
async fn oversized_response_frame_is_capped() {
    // The fake returns a large result; the client's small frame cap rejects it.
    let big: String = "x".repeat(4096);
    let fake = FakeAddon::ok("get_scene_info", json!({ "blob": big }), "0.2.5").await;
    let c = BlenderClient::new(
        fake.host(),
        fake.port(),
        ClientOptions {
            max_frame_length: 256,
            ..Default::default()
        },
    );
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "frame_too_large");
}

#[tokio::test]
async fn connect_failure_surfaces_as_connect_error() {
    // Reserve then release a port so nothing is listening.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let c = client("127.0.0.1".into(), port);
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "connect");
}

#[tokio::test]
async fn connect_failure_is_retried_exactly_once() {
    // Script the connect step: the first attempt fails at connect (before any
    // bytes are written, so it is retryable), the second does a real connect to
    // the fake. This proves the single retry actually fires — deleting the retry
    // call would leave the command failing with a Connect error and fail here.
    let fake = FakeAddon::ok("get_scene_info", json!({ "ok": true }), "0.2.5").await;
    let (host, port) = (fake.host(), fake.port());
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let connect: Connector = Arc::new(move |h: String, p: u16| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "refused",
                ))
            } else {
                TcpStream::connect((h.as_str(), p)).await
            }
        })
    });
    let c = BlenderClient::with_connector(host, port, opts(), connect);

    let result = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap();
    assert_eq!(result, json!({ "ok": true }));
    // One failed connect + exactly one retry that succeeded.
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    // Only the successful attempt reached the addon.
    assert_eq!(fake.connection_count(), 1);
}

#[tokio::test]
async fn transaction_holds_across_two_exchanges() {
    let fake = FakeAddon::spawn(|cmd, _params| ResponseSpec::Success {
        result: json!({ "cmd": cmd }),
        addon_version: Some("0.2.5".into()),
    })
    .await;
    let c = client(fake.host(), fake.port());

    // `send_value` borrows `&mut tx`, so the two exchanges are forced to be
    // polled sequentially — the held lock can never be bypassed by concurrent
    // polling (that would be a compile error).
    let mut tx = c.transaction(deadline()).await.unwrap();
    let a = tx.send_value("get_scene_info", no_params()).await.unwrap();
    let b = tx.send_value("clear_scene", no_params()).await.unwrap();
    assert_eq!(a, json!({"cmd": "get_scene_info"}));
    assert_eq!(b, json!({"cmd": "clear_scene"}));
    drop(tx);
    // Connect-per-command holds inside a transaction too.
    assert_eq!(fake.connection_count(), 2);
}

#[tokio::test]
async fn transaction_requires_and_preserves_one_bridge_process_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let fake = FakeAddon::spawn(move |_cmd, _params| {
        let bridge_instance_id = if handler_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            "11111111-1111-4111-8111-111111111111"
        } else {
            "22222222-2222-4222-8222-222222222222"
        };
        ResponseSpec::RawWithId(json!({
            "status": "success",
            "result": {},
            "addon_version": "0.2.5",
            "bridge_instance_id": bridge_instance_id,
        }))
    })
    .await;
    let c = client(fake.host(), fake.port());
    let mut tx = c.transaction(deadline()).await.unwrap();

    tx.send_value("first", no_params()).await.unwrap();
    let error = tx.send_value("second", no_params()).await.unwrap_err();

    assert_eq!(error.code(), "protocol");
    assert!(error.to_string().contains("process changed"));
}

#[tokio::test]
async fn transaction_pins_identity_before_returning_an_addon_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let fake = FakeAddon::spawn(move |_cmd, _params| {
        if handler_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseSpec::RawWithId(json!({
                "status": "error",
                "error": "first command failed",
                "bridge_instance_id": "11111111-1111-4111-8111-111111111111",
            }))
        } else {
            ResponseSpec::RawWithId(json!({
                "status": "success",
                "result": {},
                "bridge_instance_id": "22222222-2222-4222-8222-222222222222",
            }))
        }
    })
    .await;
    let c = client(fake.host(), fake.port());
    let mut tx = c.transaction(deadline()).await.unwrap();

    let first = tx.send_value("first", no_params()).await.unwrap_err();
    assert_eq!(first.code(), "addon");
    let second = tx.send_value("second", no_params()).await.unwrap_err();

    assert_eq!(second.code(), "protocol");
    assert!(second.to_string().contains("process changed"));
}

#[tokio::test]
async fn transaction_rejects_a_missing_bridge_process_identity() {
    let fake = FakeAddon::spawn(|_cmd, _params| {
        ResponseSpec::RawWithId(json!({
            "status": "success",
            "result": {},
            "addon_version": "0.1.0",
        }))
    })
    .await;
    let c = client(fake.host(), fake.port());
    let mut tx = c.transaction(deadline()).await.unwrap();

    let error = tx.send_value("first", no_params()).await.unwrap_err();

    assert_eq!(error.code(), "protocol");
    assert!(error.to_string().contains("missing field"));
    let warning = c
        .pop_version_warning()
        .expect("a legacy response must still expose its compatibility mismatch");
    assert!(warning.contains("0.1.0") && warning.contains("0.2.5"));
}

#[tokio::test]
async fn every_exchange_rejects_an_invalid_bridge_process_identity() {
    let fake = FakeAddon::spawn(|_cmd, _params| {
        ResponseSpec::RawWithId(json!({
            "status": "success",
            "result": {},
            "addon_version": "0.2.5",
            "bridge_instance_id": "not-a-uuid",
        }))
    })
    .await;
    let c = client(fake.host(), fake.port());

    let error = c
        .send_value("one_shot", no_params(), deadline())
        .await
        .unwrap_err();

    assert_eq!(error.code(), "protocol");
    assert!(error.to_string().contains("invalid process identity"));
}

#[tokio::test]
async fn transaction_blocks_a_competing_client_between_exchanges() {
    // Cross-exchange atomicity: a competing client on the same endpoint must
    // not slip a command between the transaction's two exchanges. The fake
    // records arrival order; if the transaction stopped holding its lock guard
    // across exchanges, the waiting competitor would interleave and reorder it.
    let fake = FakeAddon::spawn(|cmd, _params| ResponseSpec::SuccessAfter {
        result: json!({ "cmd": cmd }),
        addon_version: Some("0.2.5".into()),
        delay: Duration::from_millis(60),
    })
    .await;
    let c = client(fake.host(), fake.port());
    let competitor = client(fake.host(), fake.port());

    let mut tx = c.transaction(deadline()).await.unwrap();
    // The competitor blocks on the endpoint lock the transaction holds.
    let comp = tokio::spawn(async move {
        competitor
            .send_value("competitor", no_params(), deadline())
            .await
    });
    // Give the competitor time to reach the lock and queue behind the tx.
    tokio::time::sleep(Duration::from_millis(20)).await;

    tx.send_value("tx_first", no_params()).await.unwrap();
    tx.send_value("tx_second", no_params()).await.unwrap();
    drop(tx); // release the lock so the competitor can proceed
    comp.await.unwrap().unwrap();

    assert_eq!(
        fake.commands(),
        vec![
            "tx_first".to_string(),
            "tx_second".to_string(),
            "competitor".to_string()
        ],
        "the competitor must not interleave between the transaction's exchanges"
    );
}

#[tokio::test]
async fn work_budget_starts_after_serialized_lane_admission() {
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::SuccessAfter {
        result: json!({"completed": true}),
        addon_version: Some("0.2.5".into()),
        delay: Duration::from_millis(400),
    })
    .await;
    let holder = client(fake.host(), fake.port());
    let worker = BlenderClient::new(
        fake.host(),
        fake.port(),
        ClientOptions {
            default_budget: Duration::from_millis(200),
            ..opts()
        },
    );
    let held = holder.transaction(deadline()).await.unwrap();
    let call = tokio::spawn(async move {
        worker
            .send_value_with_work_budget("execute_code", no_params(), Duration::from_millis(300))
            .await
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(held);

    assert_eq!(call.await.unwrap().unwrap(), json!({"completed": true}));
}

#[tokio::test]
async fn busy_lane_fails_before_sending_caller_work() {
    let fake = FakeAddon::ok("execute_code", json!({}), "0.2.5").await;
    let holder = client(fake.host(), fake.port());
    let worker = BlenderClient::new(
        fake.host(),
        fake.port(),
        ClientOptions {
            default_budget: Duration::from_millis(50),
            ..opts()
        },
    );
    let held = holder.transaction(deadline()).await.unwrap();

    let error = worker
        .send_value_with_work_budget("execute_code", no_params(), Duration::from_secs(600))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        BlenderError::Timeout { phase: Phase::Lock }
    ));
    assert_eq!(fake.connection_count(), 0);
    drop(held);
}

#[tokio::test]
async fn concurrent_sends_are_serialized_by_the_process_lock() {
    // Two callers hit the same client at once; each addon handler stays in
    // flight for 80ms. Without the process-wide lock both would overlap and
    // drive max_in_flight to 2 — the lock must hold it at 1.
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::SuccessAfter {
        result: json!({}),
        addon_version: Some("0.2.5".into()),
        delay: Duration::from_millis(80),
    })
    .await;
    let c = client(fake.host(), fake.port());

    let (r1, r2) = tokio::join!(
        c.send_value("get_scene_info", no_params(), deadline()),
        c.send_value("get_scene_info", no_params(), deadline()),
    );
    r1.unwrap();
    r2.unwrap();
    assert_eq!(
        fake.max_in_flight(),
        1,
        "the process-wide lock must serialize concurrent exchanges"
    );
    assert_eq!(fake.connection_count(), 2);
}

#[tokio::test]
async fn separate_clients_to_the_same_addon_still_serialize() {
    // The serialization lock is process-wide per addon endpoint, not per client
    // instance: two independently-constructed clients pointed at the same addon
    // must still not overlap, or transaction atomicity and Blender's single-
    // thread invariant could be bypassed by simply building a second client.
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::SuccessAfter {
        result: json!({}),
        addon_version: Some("0.2.5".into()),
        delay: Duration::from_millis(80),
    })
    .await;
    let c1 = client(fake.host(), fake.port());
    let c2 = client(fake.host(), fake.port());

    let (r1, r2) = tokio::join!(
        c1.send_value("get_scene_info", no_params(), deadline()),
        c2.send_value("get_scene_info", no_params(), deadline()),
    );
    r1.unwrap();
    r2.unwrap();
    assert_eq!(
        fake.max_in_flight(),
        1,
        "two clients on one addon must share the per-endpoint lock"
    );
}

#[tokio::test]
async fn error_without_message_field_is_a_protocol_error() {
    // Symmetric with the success/result requirement: an error envelope that
    // omits the required `error` message is malformed, not a nameless failure.
    let fake = FakeAddon::spawn(|_cmd, _params| {
        ResponseSpec::RawWithId(json!({ "status": "error", "addon_version": "0.2.5" }))
    })
    .await;
    let c = client(fake.host(), fake.port());
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "protocol");
}

#[tokio::test]
async fn unknown_command_returns_registry_error() {
    let fake = FakeAddon::registry(vec!["get_scene_info", "boolean", "validate"], "0.2.5").await;
    let c = client(fake.host(), fake.port());
    let err = c
        .send_value("no_such_command", no_params(), deadline())
        .await
        .unwrap_err();
    // The add-on's unknown-command error enumerates its authoritative registry.
    let msg = err.to_string();
    assert!(msg.contains("Unknown command: 'no_such_command'"), "{msg}");
    assert!(
        msg.contains("'get_scene_info'") && msg.contains("'validate'"),
        "{msg}"
    );
}

#[tokio::test]
async fn success_with_null_result_is_accepted() {
    // A present `"result": null` is a valid result value (any JSON is legal),
    // not a malformed envelope — it must round-trip as Value::Null.
    let fake = FakeAddon::ok("get_scene_info", Value::Null, "0.2.5").await;
    let c = client(fake.host(), fake.port());
    let result = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap();
    assert_eq!(result, Value::Null);
}

#[tokio::test]
async fn success_without_result_field_is_a_protocol_error() {
    // A success envelope that omits the `result` key entirely is malformed;
    // the id is echoed so this fails on the missing result, not the id check.
    let fake = FakeAddon::spawn(|_cmd, _params| {
        ResponseSpec::RawWithId(json!({ "status": "success", "addon_version": "0.2.5" }))
    })
    .await;
    let c = client(fake.host(), fake.port());
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "protocol");
}

#[tokio::test]
async fn malformed_envelope_is_a_protocol_error() {
    // A response missing `status` cannot be decoded.
    let fake =
        FakeAddon::spawn(|_cmd, _params| ResponseSpec::Raw(json!({ "unexpected": true }))).await;
    let c = client(fake.host(), fake.port());
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "protocol");
}

#[tokio::test]
async fn close_without_response_is_a_protocol_error() {
    let fake = FakeAddon::spawn(|_cmd, _params| ResponseSpec::CloseWithout).await;
    let c = client(fake.host(), fake.port());
    let err = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "protocol");
}

#[tokio::test]
async fn passthrough_result_preserves_json_key_order() {
    // preserve_order keeps structured backend output readable and predictable
    // when it is rendered as text.
    let ordered = json!({ "z": 1, "a": 2, "m": 3 });
    let fake = FakeAddon::ok("get_scene_info", ordered.clone(), "0.2.5").await;
    let c = client(fake.host(), fake.port());
    let result = c
        .send_value("get_scene_info", no_params(), deadline())
        .await
        .unwrap();
    // serde_json with preserve_order keeps insertion order; assert the
    // serialized form matches exactly.
    assert_eq!(
        serde_json::to_string(&result).unwrap(),
        r#"{"z":1,"a":2,"m":3}"#
    );
    let _ = ordered;
}

#[tokio::test]
async fn expired_deadline_times_out_before_connecting() {
    // A zero budget is already spent — fail at the lock/connect phase without a
    // socket attempt.
    let c = client("127.0.0.1".into(), 9);
    let err = c
        .send_value("get_scene_info", no_params(), Deadline::new(Duration::ZERO))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "timeout");
    let _ = Value::Null;
}
