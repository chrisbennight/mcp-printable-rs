//! An in-process fake of the Blender addon for tests and server-crate
//! integration tests: a TCP listener speaking the same length-prefixed JSON
//! protocol, scripted per command. Not part of the production path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// What the fake should reply to a request.
#[derive(Clone)]
pub enum ResponseSpec {
    Success {
        result: Value,
        addon_version: Option<String>,
    },
    /// Like `Success`, but hold the connection open for `delay` before
    /// answering — long enough that two concurrently-handled requests overlap,
    /// so a client that failed to serialize would drive `max_in_flight` above 1.
    SuccessAfter {
        result: Value,
        addon_version: Option<String>,
        delay: Duration,
    },
    /// Hold a successful response until the test releases it. `started` is
    /// notified after the request has reached the fake, before waiting on
    /// `release`.
    SuccessWhenReleased {
        result: Value,
        addon_version: Option<String>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    Error {
        message: String,
        traceback: Option<String>,
        addon_version: Option<String>,
    },
    /// Send an arbitrary JSON object as the response (for malformed-envelope
    /// tests). The request `id` is not auto-injected.
    Raw(Value),
    /// Like `Raw`, but inject the request `id` into the object first — so a test
    /// can craft an otherwise-valid envelope (e.g. a success without `result`)
    /// that still passes the client's id-echo check.
    RawWithId(Value),
    /// Close the connection without responding.
    CloseWithout,
    /// Read the request but never respond (drives a recv-timeout).
    Hang,
}

type Handler = Arc<dyn Fn(String, Value) -> ResponseSpec + Send + Sync>;

/// A running fake addon. Dropping it stops the listener.
pub struct FakeAddon {
    addr: SocketAddr,
    conns: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    commands: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

/// Increments an in-flight counter for the lifetime of one handled connection
/// and records the high-water mark, so a test can assert the client never let
/// two exchanges run concurrently.
struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    fn enter(in_flight: Arc<AtomicUsize>, max: &AtomicUsize) -> Self {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
        InFlightGuard(in_flight)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for FakeAddon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeAddon {
    /// Spawn a fake that replies per the handler closure (`command`, `params`).
    pub async fn spawn(
        handler: impl Fn(String, Value) -> ResponseSpec + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake addon");
        let addr = listener.local_addr().expect("fake addon addr");
        let conns = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let bridge_instance_id = uuid::Uuid::new_v4().to_string();
        let handler: Handler = Arc::new(handler);
        let task = tokio::spawn(accept_loop(
            listener,
            handler,
            Arc::clone(&conns),
            in_flight,
            Arc::clone(&max_in_flight),
            Arc::clone(&commands),
            bridge_instance_id,
        ));
        FakeAddon {
            addr,
            conns,
            max_in_flight,
            commands,
            task,
        }
    }

    /// A fake that always answers `command` with a success `result` and stamps
    /// `addon_version` plus one process identity on every response.
    pub async fn ok(command: &'static str, result: Value, addon_version: &'static str) -> Self {
        let av = addon_version.to_string();
        Self::spawn(move |cmd, _params| {
            if cmd == command {
                ResponseSpec::Success {
                    result: result.clone(),
                    addon_version: Some(av.clone()),
                }
            } else {
                ResponseSpec::Error {
                    message: format!("Unknown command: '{cmd}'"),
                    traceback: None,
                    addon_version: Some(av.clone()),
                }
            }
        })
        .await
    }

    /// A fake whose unknown-command error enumerates its handler set. A known
    /// command returns an empty success.
    pub async fn registry(handlers: Vec<&'static str>, addon_version: &'static str) -> Self {
        let known: Vec<String> = handlers.iter().map(|s| s.to_string()).collect();
        let av = addon_version.to_string();
        Self::spawn(move |cmd, _params| {
            if known.iter().any(|h| h == &cmd) {
                ResponseSpec::Success {
                    result: json!({}),
                    addon_version: Some(av.clone()),
                }
            } else {
                let list = known
                    .iter()
                    .map(|h| format!("'{h}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                ResponseSpec::Error {
                    message: format!("Unknown command: '{cmd}'. Available: [{list}]"),
                    traceback: None,
                    addon_version: Some(av.clone()),
                }
            }
        })
        .await
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Number of connections accepted so far — used to assert the client's
    /// connect-per-command and no-retry-after-write behavior.
    pub fn connection_count(&self) -> usize {
        self.conns.load(Ordering::SeqCst)
    }

    /// The peak number of connections handled at the same time. The client's
    /// process-wide lock must keep this at 1 even under concurrent callers.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    /// The command names received so far, in arrival order — used to assert
    /// that a competing client did not interleave between a transaction's
    /// exchanges.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("commands lock").clone()
    }
}

/// Build a success envelope echoing the request `id` and stamping the fake
/// process identity — the shape both immediate and delayed successes share.
fn success_envelope(
    id: &str,
    result: Value,
    addon_version: Option<String>,
    bridge_instance_id: &str,
) -> Value {
    let mut o = json!({
        "id": id,
        "status": "success",
        "result": result,
        "bridge_instance_id": bridge_instance_id,
    });
    if let Some(av) = addon_version {
        o["addon_version"] = json!(av);
    }
    o
}

async fn accept_loop(
    listener: TcpListener,
    handler: Handler,
    conns: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    commands: Arc<std::sync::Mutex<Vec<String>>>,
    bridge_instance_id: String,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        conns.fetch_add(1, Ordering::SeqCst);
        let handler = Arc::clone(&handler);
        let in_flight = Arc::clone(&in_flight);
        let max_in_flight = Arc::clone(&max_in_flight);
        let commands = Arc::clone(&commands);
        let bridge_instance_id = bridge_instance_id.clone();
        tokio::spawn(async move {
            let _in_flight = InFlightGuard::enter(in_flight, &max_in_flight);
            let codec = LengthDelimitedCodec::builder()
                .big_endian()
                .length_field_length(4)
                .max_frame_length(128 * 1024 * 1024)
                .new_codec();
            let mut framed = Framed::new(stream, codec);
            let Some(Ok(frame)) = framed.next().await else {
                return;
            };
            let req: Value = match serde_json::from_slice(&frame) {
                Ok(v) => v,
                Err(_) => return,
            };
            let id = req
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let command = req
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let params = req.get("params").cloned().unwrap_or(Value::Null);

            commands
                .lock()
                .expect("commands lock")
                .push(command.clone());
            let spec = handler(command, params);
            let response = match spec {
                ResponseSpec::Hang => {
                    // Never respond; hold the connection until aborted.
                    std::future::pending::<()>().await;
                    return;
                }
                ResponseSpec::CloseWithout => return,
                ResponseSpec::Raw(v) => v,
                ResponseSpec::RawWithId(mut v) => {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("id".to_string(), json!(id));
                    }
                    v
                }
                ResponseSpec::Success {
                    result,
                    addon_version,
                } => success_envelope(&id, result, addon_version, &bridge_instance_id),
                ResponseSpec::SuccessAfter {
                    result,
                    addon_version,
                    delay,
                } => {
                    tokio::time::sleep(delay).await;
                    success_envelope(&id, result, addon_version, &bridge_instance_id)
                }
                ResponseSpec::SuccessWhenReleased {
                    result,
                    addon_version,
                    started,
                    release,
                } => {
                    started.notify_one();
                    release.notified().await;
                    success_envelope(&id, result, addon_version, &bridge_instance_id)
                }
                ResponseSpec::Error {
                    message,
                    traceback,
                    addon_version,
                } => {
                    let mut o = json!({ "id": id, "status": "error", "error": message });
                    if let Some(tb) = traceback {
                        o["traceback"] = json!(tb);
                    }
                    if let Some(av) = addon_version {
                        o["addon_version"] = json!(av);
                    }
                    o["bridge_instance_id"] = json!(bridge_instance_id);
                    o
                }
            };
            let payload = serde_json::to_vec(&response).expect("encode fake response");
            let _ = framed.send(Bytes::from(payload)).await;
        });
    }
}
