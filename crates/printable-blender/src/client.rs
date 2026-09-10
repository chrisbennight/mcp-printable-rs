use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use crate::BRIDGE_PROTOCOL_VERSION;
use crate::deadline::{Deadline, Phase};
use crate::error::BlenderError;
use crate::protocol::{Params, Request, Response};
use crate::version::VersionState;

/// 64 MiB default inbound frame cap — headroom above the addon's 50 MiB limit
/// (large base64 renders are the biggest legitimate payloads) while still
/// bounding allocation against a malformed length.
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024;

/// Default per-command budget.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(120);

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub max_frame_length: usize,
    pub default_budget: Duration,
    /// The version the client compares the addon against for the one-shot
    /// mismatch warning. `None` disables the warning (nothing to compare).
    pub client_version: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            default_budget: DEFAULT_BUDGET,
            client_version: Some(BRIDGE_PROTOCOL_VERSION.to_string()),
        }
    }
}

/// The per-addon serialization lock, shared process-wide.
///
/// The Blender addon is a process-global resource — it runs every command on
/// Blender's single main thread — so serialization must hold across *all*
/// clients targeting a given addon, not merely within one `BlenderClient`. The
/// lock therefore lives in a process-wide registry keyed by `(host, port)`:
/// every client built for the same endpoint shares one async mutex, while
/// distinct endpoints (including separate test fakes on distinct ports) get
/// independent locks and never contend.
struct EndpointState {
    lock: tokio::sync::Mutex<()>,
    recovery_fenced: AtomicBool,
    readiness_probe_active: AtomicBool,
}

fn endpoint_state(host: &str, port: u16) -> Arc<EndpointState> {
    #[allow(clippy::type_complexity)]
    static REGISTRY: OnceLock<std::sync::Mutex<HashMap<(String, u16), Arc<EndpointState>>>> =
        OnceLock::new();
    let registry = REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = registry.lock().expect("endpoint lock registry");
    Arc::clone(map.entry((host.to_string(), port)).or_insert_with(|| {
        Arc::new(EndpointState {
            lock: tokio::sync::Mutex::new(()),
            recovery_fenced: AtomicBool::new(false),
            readiness_probe_active: AtomicBool::new(false),
        })
    }))
}

struct ReadinessProbeGuard<'a> {
    active: &'a AtomicBool,
}

impl<'a> ReadinessProbeGuard<'a> {
    fn acquire(active: &'a AtomicBool) -> Result<Self, BlenderError> {
        active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| BlenderError::ReadinessProbeInProgress)?;
        Ok(Self { active })
    }
}

impl Drop for ReadinessProbeGuard<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

/// How the client opens a TCP connection to the addon. Production uses
/// `default_connector` (real `TcpStream::connect`); tests inject a connector
/// that scripts a connect-phase failure to exercise the one-retry boundary
/// deterministically (a refused connect is unobservable to a listener, so it
/// cannot be provoked hermetically without this seam).
pub type Connector = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = std::io::Result<TcpStream>> + Send>>
        + Send
        + Sync,
>;

/// The production connector: a plain `TcpStream::connect`.
fn default_connector() -> Connector {
    Arc::new(|host: String, port: u16| {
        Box::pin(async move { TcpStream::connect((host.as_str(), port)).await })
    })
}

/// Client for the Blender addon bridge. The `send` path serializes every
/// exchange through a **per-endpoint process-wide** lock (see `endpoint_state`)
/// because the addon runs commands on Blender's one main thread — so even
/// separately-constructed clients pointed at the same addon cannot overlap.
/// Each exchange opens a fresh TCP connection and closes it (connect-per-
/// command), so multiple MCP clients interleave at request granularity without
/// any holding the socket.
pub struct BlenderClient {
    host: String,
    port: u16,
    max_frame: usize,
    default_budget: Duration,
    client_version: Option<String>,
    endpoint: Arc<EndpointState>,
    version: std::sync::Mutex<VersionState>,
    connect: Connector,
}

struct ExchangeResult {
    outcome: Result<Value, BlenderError>,
    bridge_instance_id: String,
}

impl std::fmt::Debug for BlenderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The connector is an opaque closure; report the addressable identity
        // and the tunable knobs instead.
        f.debug_struct("BlenderClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("max_frame", &self.max_frame)
            .field("default_budget", &self.default_budget)
            .field("client_version", &self.client_version)
            .finish_non_exhaustive()
    }
}

impl BlenderClient {
    pub fn new(host: impl Into<String>, port: u16, opts: ClientOptions) -> Self {
        Self::build(host, port, opts, default_connector())
    }

    /// Construct a client with a custom connector. Test-only: it exists to
    /// script connect-phase failures for the retry-boundary test.
    #[cfg(feature = "fake-addon")]
    pub fn with_connector(
        host: impl Into<String>,
        port: u16,
        opts: ClientOptions,
        connect: Connector,
    ) -> Self {
        Self::build(host, port, opts, connect)
    }

    fn build(host: impl Into<String>, port: u16, opts: ClientOptions, connect: Connector) -> Self {
        let host = host.into();
        let endpoint = endpoint_state(&host, port);
        BlenderClient {
            host,
            port,
            max_frame: opts.max_frame_length,
            default_budget: opts.default_budget,
            client_version: opts.client_version,
            endpoint,
            version: std::sync::Mutex::new(VersionState::default()),
            connect,
        }
    }

    /// A deadline of the configured default budget.
    pub fn default_deadline(&self) -> Deadline {
        Deadline::new(self.default_budget)
    }

    /// The addon version seen on first contact, if any.
    pub fn addon_version(&self) -> Option<String> {
        self.version.lock().expect("version lock").addon_version()
    }

    /// The one-shot version-mismatch warning, returned at most once.
    pub fn pop_version_warning(&self) -> Option<String> {
        self.version.lock().expect("version lock").pop_warning()
    }

    /// Fence ordinary Blender commands while durable recovery owns an
    /// uncertain pre-job session restoration. The fence is shared by every
    /// client targeting this endpoint and is checked after lane admission.
    pub fn set_recovery_fenced(&self, fenced: bool) {
        self.endpoint
            .recovery_fenced
            .store(fenced, Ordering::SeqCst);
    }

    /// Whether ordinary commands are fenced for durable session recovery.
    pub fn recovery_fenced(&self) -> bool {
        self.endpoint.recovery_fenced.load(Ordering::SeqCst)
    }

    /// Send one command and return its `result` value. Serialized process-wide;
    /// bounded by `deadline`; retries once if the connection can't be
    /// established (never after any request bytes are written).
    pub async fn send_value(
        &self,
        command: &str,
        params: Params,
        deadline: Deadline,
    ) -> Result<Value, BlenderError> {
        self.ensure_not_recovery_fenced(command)?;
        let remaining = deadline.remaining(Phase::Lock)?;
        let _guard = timeout(remaining, self.endpoint.lock.lock())
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Lock })?;
        self.ensure_not_recovery_fenced(command)?;
        self.exchange_locked(command, params, deadline)
            .await?
            .outcome
    }

    /// Attempt one command only when the endpoint lane is immediately free.
    ///
    /// `Ok(None)` means another healthy in-process request currently owns the
    /// single-threaded Blender lane. Readiness probes use this to report busy
    /// without waiting behind a long render and incorrectly declaring its
    /// backend unavailable.
    pub async fn try_send_value(
        &self,
        command: &str,
        params: Params,
        deadline: Deadline,
    ) -> Result<Option<Value>, BlenderError> {
        self.ensure_not_recovery_fenced(command)?;
        let _probe = ReadinessProbeGuard::acquire(&self.endpoint.readiness_probe_active)?;
        let Ok(_guard) = self.endpoint.lock.try_lock() else {
            return Ok(None);
        };
        self.ensure_not_recovery_fenced(command)?;
        self.exchange_locked(command, params, deadline)
            .await?
            .outcome
            .map(Some)
    }

    /// Send work whose caller-selected execution budget must not be consumed
    /// while waiting for Blender's serialized lane. Admission gets the normal
    /// command budget and fails before sending when Blender stays busy; after
    /// admission, transport gets a fresh normal budget in addition to the work.
    pub async fn send_value_with_work_budget(
        &self,
        command: &str,
        params: Params,
        work_budget: Duration,
    ) -> Result<Value, BlenderError> {
        self.ensure_not_recovery_fenced(command)?;
        let queue_deadline = Deadline::new(self.default_budget);
        let remaining = queue_deadline.remaining(Phase::Lock)?;
        let _guard = timeout(remaining, self.endpoint.lock.lock())
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Lock })?;
        self.ensure_not_recovery_fenced(command)?;
        let execution_deadline = Deadline::new(self.default_budget.saturating_add(work_budget));
        self.exchange_locked(command, params, execution_deadline)
            .await?
            .outcome
    }

    /// Hold the serialization lock across multiple exchanges. Used where a
    /// read-then-mutate sequence must be atomic
    /// against other clients (e.g. `clear_scene` reads the scene, then wipes it).
    pub async fn transaction(&self, deadline: Deadline) -> Result<Transaction<'_>, BlenderError> {
        self.ensure_not_recovery_fenced("transaction")?;
        let remaining = deadline.remaining(Phase::Lock)?;
        let guard = timeout(remaining, self.endpoint.lock.lock())
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Lock })?;
        self.ensure_not_recovery_fenced("transaction")?;
        Ok(Transaction {
            client: self,
            deadline,
            bridge_instance_id: None,
            _guard: guard,
        })
    }

    /// Hold the serialized lane for durable job recovery even while ordinary
    /// commands are fenced. The job worker is the only caller of this path.
    pub async fn recovery_transaction(
        &self,
        deadline: Deadline,
    ) -> Result<Transaction<'_>, BlenderError> {
        let remaining = deadline.remaining(Phase::Lock)?;
        let guard = timeout(remaining, self.endpoint.lock.lock())
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Lock })?;
        Ok(Transaction {
            client: self,
            deadline,
            bridge_instance_id: None,
            _guard: guard,
        })
    }

    fn ensure_not_recovery_fenced(&self, command: &str) -> Result<(), BlenderError> {
        if command != "bridge_status" && self.recovery_fenced() {
            return Err(BlenderError::RecoveryFenced);
        }
        Ok(())
    }

    /// Run one exchange while already holding the lock, with the retry policy
    /// and version observation applied.
    async fn exchange_locked(
        &self,
        command: &str,
        params: Params,
        deadline: Deadline,
    ) -> Result<ExchangeResult, BlenderError> {
        let response = match self.exchange_once(command, &params, deadline).await {
            // Connection establishment failed before any bytes were written —
            // safe to retry exactly once (the command was never delivered).
            Err(e) if is_retryable_connect_failure(&e) => {
                tracing::debug!(error = %e, "blender connect failed; retrying once");
                self.exchange_once(command, &params, deadline).await?
            }
            other => other?,
        };

        validate_bridge_instance_id(response.bridge_instance_id())?;

        // A missing `result` on success was already rejected at decode time (the
        // Success variant requires it); a present `null` is a valid result here.
        match response {
            Response::Success {
                result,
                bridge_instance_id,
                ..
            } => Ok(ExchangeResult {
                outcome: Ok(result),
                bridge_instance_id,
            }),
            Response::Error {
                error,
                error_code,
                scene_state,
                traceback,
                bridge_instance_id,
                ..
            } => {
                if traceback.is_some() {
                    tracing::warn!(command, "blender handler error; traceback suppressed");
                }
                if let Some(state) = &scene_state
                    && (Uuid::parse_str(&state.generation)
                        .ok()
                        .map(|id| id.to_string())
                        .as_deref()
                        != Some(state.generation.as_str())
                        || state.revision > 9007199254740991)
                {
                    return Err(BlenderError::Protocol(
                        "invalid scene state in Blender response".to_string(),
                    ));
                }
                let error = match error_code.as_deref() {
                    Some("stale_scene_state" | "invalid_scene_state") => {
                        let observed = scene_state.ok_or_else(|| {
                            BlenderError::Protocol(
                                "scene precondition failure omitted current state".to_string(),
                            )
                        })?;
                        if error_code.as_deref() == Some("stale_scene_state") {
                            BlenderError::StaleSceneState { observed }
                        } else {
                            BlenderError::InvalidSceneState { observed }
                        }
                    }
                    _ => BlenderError::Addon {
                        message: error,
                        scene_state,
                    },
                };
                Ok(ExchangeResult {
                    outcome: Err(error),
                    bridge_instance_id,
                })
            }
        }
    }

    /// One connect→send→recv exchange with per-phase deadline enforcement.
    async fn exchange_once(
        &self,
        command: &str,
        params: &Params,
        deadline: Deadline,
    ) -> Result<Response, BlenderError> {
        let connect_budget = deadline.remaining(Phase::Connect)?;
        let stream = timeout(connect_budget, (self.connect)(self.host.clone(), self.port))
            .await
            .map_err(|_| BlenderError::Timeout {
                phase: Phase::Connect,
            })?
            .map_err(|source| BlenderError::Connect {
                host: self.host.clone(),
                port: self.port,
                source,
            })?;

        let codec = LengthDelimitedCodec::builder()
            .big_endian()
            .length_field_length(4)
            .max_frame_length(self.max_frame)
            .new_codec();
        let mut framed = Framed::new(stream, codec);

        let request = Request::new(command, params.clone());
        let payload = serde_json::to_vec(&request)
            .map_err(|e| BlenderError::Protocol(format!("failed to encode request: {e}")))?;

        let send_budget = deadline.remaining(Phase::Send)?;
        timeout(send_budget, framed.send(Bytes::from(payload)))
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Send })?
            .map_err(|e| self.map_frame_io(e))?;

        let recv_budget = deadline.remaining(Phase::Recv)?;
        let frame = timeout(recv_budget, framed.next())
            .await
            .map_err(|_| BlenderError::Timeout { phase: Phase::Recv })?
            .ok_or_else(|| {
                BlenderError::Protocol("Blender closed the connection without a response".into())
            })?
            .map_err(|e| self.map_frame_io(e))?;

        let untyped = serde_json::from_slice::<Value>(&frame)
            .map_err(|e| BlenderError::Protocol(format!("failed to decode response: {e}")))?;
        self.version.lock().expect("version lock").observe(
            untyped.get("addon_version").and_then(Value::as_str),
            self.client_version.as_deref(),
        );
        let response = serde_json::from_value::<Response>(untyped)
            .map_err(|e| BlenderError::Protocol(format!("failed to decode response: {e}")))?;

        // The addon echoes the request `id` on every response; a mismatched or
        // missing id means the envelope is not the answer to this request.
        if response.id() != request.id {
            return Err(BlenderError::Protocol(format!(
                "Blender response correlation id {:?} does not match request id {:?}",
                response.id(),
                request.id
            )));
        }
        Ok(response)
    }

    // -- retry predicate lives as a free function below so it is unit-testable.

    /// Map a codec I/O error, distinguishing the frame-cap breach from other
    /// I/O so an oversized declared length surfaces as `FrameTooLarge` rather
    /// than a generic error.
    fn map_frame_io(&self, e: std::io::Error) -> BlenderError {
        if e.kind() == std::io::ErrorKind::InvalidData
            && e.to_string().contains("frame size too big")
        {
            BlenderError::FrameTooLarge {
                max: self.max_frame,
            }
        } else {
            BlenderError::Io(e)
        }
    }
}

/// The retry policy, as a pure decision so it is unit-testable: a failed
/// exchange is retried at most once, and only when the TCP connection could not
/// be *established* — i.e. before any request bytes were written. Once a request
/// is on the wire it is never resent, because non-idempotent commands
/// (`execute_code`, `boolean`) could otherwise be applied twice.
fn is_retryable_connect_failure(err: &BlenderError) -> bool {
    matches!(err, BlenderError::Connect { .. })
}

fn validate_bridge_instance_id(observed: &str) -> Result<(), BlenderError> {
    let parsed = Uuid::parse_str(observed).map_err(|_| {
        BlenderError::Protocol(
            "Blender bridge response contained an invalid process identity".to_string(),
        )
    })?;
    if parsed.to_string() != observed.to_lowercase() {
        return Err(BlenderError::Protocol(
            "Blender bridge response contained an invalid process identity".to_string(),
        ));
    }
    Ok(())
}

/// A held-lock transaction: successive `send_value` calls run without
/// re-acquiring the serialization lock, so no other client can interleave, and
/// they share one transaction-wide deadline. `send_value` takes `&mut self`, so
/// the borrow checker forces the exchanges to be polled sequentially — two
/// concurrently-polled sends is a compile error — which is what keeps the held
/// lock from being bypassed by overlapping exchanges. The first successful
/// response pins the bridge-process UUID and later exchanges must retain it.
/// Dropping the transaction releases the lock.
pub struct Transaction<'a> {
    client: &'a BlenderClient,
    deadline: Deadline,
    bridge_instance_id: Option<String>,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl Transaction<'_> {
    fn observe_bridge_instance(&mut self, observed: String) -> Result<(), BlenderError> {
        match &self.bridge_instance_id {
            Some(expected) if expected != &observed => Err(BlenderError::Protocol(
                "Blender bridge process changed during the transaction".to_string(),
            )),
            Some(_) => Ok(()),
            None => {
                self.bridge_instance_id = Some(observed);
                Ok(())
            }
        }
    }

    /// Send one command within the transaction, under the shared
    /// transaction-wide deadline (its budget is consumed across all exchanges,
    /// not reset per command).
    pub async fn send_value(
        &mut self,
        command: &str,
        params: Params,
    ) -> Result<Value, BlenderError> {
        let exchange = self
            .client
            .exchange_locked(command, params, self.deadline)
            .await?;
        self.observe_bridge_instance(exchange.bridge_instance_id)?;
        exchange.outcome
    }

    /// Send caller-budgeted work while retaining the transaction lock. Each
    /// exchange receives a fresh transport allowance plus its own work budget,
    /// so a long frame sequence is not constrained by one arbitrary aggregate
    /// timeout while other Blender clients still cannot interleave.
    pub async fn send_value_with_work_budget(
        &mut self,
        command: &str,
        params: Params,
        work_budget: Duration,
    ) -> Result<Value, BlenderError> {
        let deadline = Deadline::new(self.client.default_budget.saturating_add(work_budget));
        let exchange = self
            .client
            .exchange_locked(command, params, deadline)
            .await?;
        self.observe_bridge_instance(exchange.bridge_instance_id)?;
        exchange.outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_addon::{FakeAddon, ResponseSpec};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn connect_err() -> BlenderError {
        BlenderError::Connect {
            host: "127.0.0.1".into(),
            port: 9876,
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        }
    }

    #[test]
    fn only_connect_establishment_failures_are_retried() {
        // Retry exactly the pre-write connect failure...
        assert!(is_retryable_connect_failure(&connect_err()));
        // ...and nothing that could mean the command already reached the addon.
        assert!(!is_retryable_connect_failure(&BlenderError::Timeout {
            phase: Phase::Send
        }));
        assert!(!is_retryable_connect_failure(&BlenderError::Timeout {
            phase: Phase::Recv
        }));
        assert!(!is_retryable_connect_failure(&BlenderError::Addon {
            scene_state: None,
            message: "boom".into(),
        }));
        assert!(!is_retryable_connect_failure(
            &BlenderError::FrameTooLarge { max: 1 }
        ));
        assert!(!is_retryable_connect_failure(&BlenderError::Protocol(
            "bad".into()
        )));
    }

    #[test]
    fn default_options_enforce_the_current_bridge_protocol() {
        assert_eq!(
            ClientOptions::default().client_version.as_deref(),
            Some(BRIDGE_PROTOCOL_VERSION)
        );
    }

    #[tokio::test]
    async fn recovery_fence_is_endpoint_wide_and_worker_bypass_is_lock_scoped() {
        let first = BlenderClient::new("recovery-fence.invalid", 65534, ClientOptions::default());
        let second = BlenderClient::new("recovery-fence.invalid", 65534, ClientOptions::default());
        first.set_recovery_fenced(true);

        let send_error = second
            .send_value("get_scene_info", Params::new(), second.default_deadline())
            .await
            .expect_err("ordinary command is fenced before connecting");
        assert!(matches!(send_error, BlenderError::RecoveryFenced));
        let transaction_error = match second.transaction(second.default_deadline()).await {
            Ok(_) => panic!("ordinary transaction crossed the recovery fence"),
            Err(error) => error,
        };
        assert!(matches!(transaction_error, BlenderError::RecoveryFenced));

        let recovery = first
            .recovery_transaction(first.default_deadline())
            .await
            .expect("durable recovery owns the fenced lane");
        let fenced_while_recovery_owns_lane = second
            .send_value("get_scene_info", Params::new(), second.default_deadline())
            .await
            .expect_err("ordinary command fails before waiting on the recovery lane");
        assert!(matches!(
            fenced_while_recovery_owns_lane,
            BlenderError::RecoveryFenced
        ));
        drop(recovery);
        first.set_recovery_fenced(false);
        assert!(!second.recovery_fenced());
    }

    #[tokio::test]
    async fn nonblocking_status_probe_bypasses_fence_and_reports_an_owned_lane() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fake = FakeAddon::spawn({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |command, _| {
                if command == "long_render" {
                    ResponseSpec::SuccessWhenReleased {
                        result: json!({"rendered": true}),
                        addon_version: Some(crate::BRIDGE_PROTOCOL_VERSION.to_string()),
                        started: Arc::clone(&started),
                        release: Arc::clone(&release),
                    }
                } else {
                    ResponseSpec::Success {
                        result: json!({"available": true}),
                        addon_version: Some(crate::BRIDGE_PROTOCOL_VERSION.to_string()),
                    }
                }
            }
        })
        .await;
        let client = Arc::new(BlenderClient::new(
            fake.host(),
            fake.port(),
            ClientOptions::default(),
        ));
        let started_wait = started.notified();
        let rendering = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .send_value("long_render", Params::new(), client.default_deadline())
                    .await
            }
        });
        started_wait.await;
        client.set_recovery_fenced(true);

        let probe = client
            .try_send_value("bridge_status", Params::new(), client.default_deadline())
            .await
            .expect("a fenced busy lane is not a backend failure");
        assert!(probe.is_none(), "probe must not queue behind the render");

        release.notify_one();
        rendering
            .await
            .expect("render task")
            .expect("render response");
        let probe = client
            .try_send_value("bridge_status", Params::new(), client.default_deadline())
            .await
            .expect("status probe remains safe while recovery is fenced");
        assert_eq!(probe, Some(json!({"available": true})));
        client.set_recovery_fenced(false);
    }

    #[tokio::test]
    async fn concurrent_status_probe_cannot_report_a_stalled_probe_as_healthy_work() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fake = FakeAddon::spawn({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_, _| ResponseSpec::SuccessWhenReleased {
                result: json!({"available": true}),
                addon_version: Some(crate::BRIDGE_PROTOCOL_VERSION.to_string()),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }
        })
        .await;
        let client = Arc::new(BlenderClient::new(
            fake.host(),
            fake.port(),
            ClientOptions::default(),
        ));
        let started_wait = started.notified();
        let first_probe = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .try_send_value("bridge_status", Params::new(), client.default_deadline())
                    .await
            }
        });
        started_wait.await;

        let error = client
            .try_send_value("bridge_status", Params::new(), client.default_deadline())
            .await
            .expect_err("a second readiness probe must fail closed");
        assert!(matches!(error, BlenderError::ReadinessProbeInProgress));

        release.notify_one();
        assert_eq!(
            first_probe
                .await
                .expect("first probe task")
                .expect("first probe response"),
            Some(json!({"available": true}))
        );
    }
}
