//! Bounded, typed access to the Bambuddy API.

pub mod control;
pub mod job_options;
pub mod job_types;
pub mod jobs;
pub mod material_types;
pub mod materials;
pub mod observation;
pub mod printer_types;
pub mod rejection;
pub use printer_types::{PrinterFault, PrinterStatus};

use std::{fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use reqwest::{Client as HttpClient, Method, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::{Host, Url};
use zeroize::Zeroizing;

const MAXIMUM_RESPONSE_BYTES: usize = 1024 * 1024;
/// Largest upstream request deadline supported by the client.
pub const MAXIMUM_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One Bambuddy API key.
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// Validate and protect an API key.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or line-bearing value.
    pub fn new(value: String) -> Result<Self, ApiError> {
        Self::from_protected(Zeroizing::new(value))
    }

    /// Validate an API key that is already protected from plaintext drops.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or line-bearing value.
    pub fn from_protected(value: Zeroizing<String>) -> Result<Self, ApiError> {
        if value.is_empty() || value.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
            return Err(ApiError::InvalidConfiguration("API key"));
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

/// Errors expose only typed domain details; raw upstream bodies are discarded.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ApiError {
    #[error("Bambuddy rejected the operation: {0}")]
    Rejected(rejection::Rejection),
    #[error("invalid Bambuddy client configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("Bambuddy authentication failed")]
    Authentication,
    #[error("Bambuddy denied the operation")]
    Forbidden,
    #[error("the requested Bambuddy resource was not found")]
    NotFound,
    #[error("Bambuddy is temporarily unavailable")]
    Unavailable,
    #[error(
        "the mutation outcome is unknown; inspect the affected print or queue before acting again"
    )]
    AmbiguousOutcome,
    #[error("Bambuddy returned an incompatible response")]
    IncompatibleResponse,
    #[error("Bambuddy response exceeded the configured bound")]
    ResponseTooLarge,
}

/// Printer metadata safe for agent discovery. Network and device credentials are discarded.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct Printer {
    pub id: u64,
    pub name: String,
    pub model: Option<String>,
    pub location: Option<String>,
    pub is_active: bool,
}

/// Direct print-state transition supported by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Pause,
    Resume,
    Stop,
}

impl ControlAction {
    fn path(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }
}

/// Minimal acknowledgement from a direct printer-state transition.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ControlReceipt {
    pub success: bool,
    pub message: String,
}

/// Sendable future returned by the object-safe API boundary.
pub type ApiFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

/// Narrow interface consumed by the MCP layer and implemented by test fakes.
pub trait BambuddyApi: Send + Sync {
    fn printers(&self) -> ApiFuture<'_, Vec<Printer>>;
    fn printer_status(&self, printer_id: u64) -> ApiFuture<'_, PrinterStatus>;
    fn control(&self, printer_id: u64, action: ControlAction) -> ApiFuture<'_, ControlReceipt>;
}

/// Redirect-free, proxy-free Bambuddy HTTP client.
#[derive(Clone)]
pub struct Client {
    http: HttpClient,
    base_url: Url,
    api_key: Arc<ApiKey>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Construct a bounded client for one least-privileged Bambuddy key.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe URL, timeout, or HTTP configuration.
    pub fn new(base_url: Url, api_key: ApiKey, timeout: Duration) -> Result<Self, ApiError> {
        validate_base_url(&base_url)?;
        if timeout.is_zero() || timeout > MAXIMUM_REQUEST_TIMEOUT {
            return Err(ApiError::InvalidConfiguration("request timeout"));
        }
        let http = HttpClient::builder()
            .timeout(timeout)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| ApiError::InvalidConfiguration("HTTP client"))?;
        Ok(Self {
            http,
            base_url,
            api_key: Arc::new(api_key),
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let mut endpoint = self.base_url.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| ApiError::InvalidConfiguration("base URL"))?
            .extend(["api", "v1", "printers"])
            .extend(segments);
        Ok(endpoint)
    }

    async fn read<T: DeserializeOwned>(&self, endpoint: Url) -> Result<T, ApiError> {
        match self
            .request_once(Method::GET, endpoint.clone(), false)
            .await
        {
            Err(ApiError::Unavailable) => self.request_once(Method::GET, endpoint, false).await,
            result => result,
        }
    }

    async fn request_once<T: DeserializeOwned>(
        &self,
        method: Method,
        endpoint: Url,
        mutation: bool,
    ) -> Result<T, ApiError> {
        let response = self
            .http
            .request(method, endpoint)
            .header("X-API-Key", self.api_key.0.as_str())
            .send()
            .await
            .map_err(|_| {
                if mutation {
                    ApiError::AmbiguousOutcome
                } else {
                    ApiError::Unavailable
                }
            })?;

        self.decode_response(response, mutation).await
    }

    async fn decode_response<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        mutation: bool,
    ) -> Result<T, ApiError> {
        let status = response.status();
        let rejected = matches!(status.as_u16(), 400 | 409 | 422);
        match status {
            status if status.is_success() || rejected => {}
            StatusCode::UNAUTHORIZED => return Err(ApiError::Authentication),
            StatusCode::FORBIDDEN => return Err(ApiError::Forbidden),
            StatusCode::NOT_FOUND => return Err(ApiError::NotFound),
            status
                if mutation
                    && (status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT) =>
            {
                return Err(ApiError::AmbiguousOutcome);
            }
            status if status.is_server_error() => return Err(ApiError::Unavailable),
            _ => return Err(ApiError::IncompatibleResponse),
        }

        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            if mutation {
                ApiError::AmbiguousOutcome
            } else {
                ApiError::Unavailable
            }
        })? {
            if bytes.len().saturating_add(chunk.len()) > MAXIMUM_RESPONSE_BYTES {
                return Err(if mutation {
                    ApiError::AmbiguousOutcome
                } else {
                    ApiError::ResponseTooLarge
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if rejected {
            return Err(ApiError::Rejected(rejection::Rejection::decode(
                status.as_u16(),
                &bytes,
            )));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            if mutation {
                ApiError::AmbiguousOutcome
            } else {
                ApiError::IncompatibleResponse
            }
        })
    }
}

impl BambuddyApi for Client {
    fn printers(&self) -> ApiFuture<'_, Vec<Printer>> {
        Box::pin(async move { self.read(self.endpoint(&[""])?).await })
    }

    fn printer_status(&self, printer_id: u64) -> ApiFuture<'_, PrinterStatus> {
        Box::pin(async move {
            self.read(self.endpoint(&[&printer_id.to_string(), "status"])?)
                .await
        })
    }

    fn control(&self, printer_id: u64, action: ControlAction) -> ApiFuture<'_, ControlReceipt> {
        Box::pin(async move {
            let endpoint = self.endpoint(&[&printer_id.to_string(), "print", action.path()])?;
            self.request_once(Method::POST, endpoint, true).await
        })
    }
}

fn validate_base_url(url: &Url) -> Result<(), ApiError> {
    if url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(ApiError::InvalidConfiguration("base URL"));
    }
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() != "http" {
        return Err(ApiError::InvalidConfiguration("base URL scheme"));
    }
    let private = match url.host() {
        Some(Host::Ipv4(address)) => address.is_private() || address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback() || address.is_unique_local(),
        Some(Host::Domain(domain)) => domain == "localhost" || !domain.contains('.'),
        None => false,
    };
    private
        .then_some(())
        .ok_or(ApiError::InvalidConfiguration("plaintext Bambuddy address"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use url::Url;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::{ApiError, ApiKey, BambuddyApi, Client, ControlAction, MAXIMUM_RESPONSE_BYTES};

    fn client(server: &MockServer) -> Client {
        Client::new(
            Url::parse(&format!("{}/", server.uri())).expect("wiremock URL"),
            ApiKey::new("bb_test_key".into()).expect("API key"),
            Duration::from_secs(2),
        )
        .expect("client")
    }

    #[tokio::test]
    async fn printer_list_projects_out_network_and_device_secrets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/"))
            .and(header("X-API-Key", "bb_test_key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": 7,
                "name": "P1S",
                "model": "P1S",
                "location": "workshop",
                "is_active": true,
                "serial_number": "SENTINEL_SERIAL",
                "ip_address": "SENTINEL_ADDRESS"
            }])))
            .expect(1)
            .mount(&server)
            .await;

        let printers = client(&server).printers().await.expect("printers");
        assert_eq!(printers[0].name, "P1S");
        assert!(!format!("{printers:?}").contains("SENTINEL"));
    }

    #[tokio::test]
    async fn mutation_is_sent_once_and_an_uncertain_failure_stays_ambiguous() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/printers/7/print/stop"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            client(&server)
                .control(7, ControlAction::Stop)
                .await
                .expect_err("ambiguous mutation"),
            ApiError::AmbiguousOutcome
        );
    }

    #[tokio::test]
    async fn read_retries_one_transient_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/7/status"))
            .respond_with(ResponseTemplate::new(503))
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/7/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 7,
                "name": "P1S",
                "connected": true,
                "state": "IDLE",
                "current_print": null,
                "progress": null,
                "remaining_time": null,
                "layer_num": null,
                "total_layers": null,
                "hms_errors": [],
                "awaiting_plate_clear": false
            })))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        assert!(client(&server).printer_status(7).await.is_ok());
    }

    #[tokio::test]
    async fn response_body_limit_preserves_normal_capacity_and_rejects_oversize() {
        let normal_server = MockServer::start().await;
        let normal_name = "P".repeat(4096);
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": 7,
                "name": normal_name,
                "model": null,
                "location": null,
                "is_active": true
            }])))
            .expect(1)
            .mount(&normal_server)
            .await;

        assert_eq!(
            client(&normal_server)
                .printers()
                .await
                .expect("printers")
                .len(),
            1
        );

        let oversized_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAXIMUM_RESPONSE_BYTES + 1]),
            )
            .expect(1)
            .mount(&oversized_server)
            .await;

        assert_eq!(
            client(&oversized_server)
                .printers()
                .await
                .expect_err("oversized response"),
            ApiError::ResponseTooLarge
        );
    }

    #[tokio::test]
    async fn oversized_mutation_response_has_an_ambiguous_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/printers/7/print/pause"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAXIMUM_RESPONSE_BYTES + 1]),
            )
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            client(&server)
                .control(7, ControlAction::Pause)
                .await
                .expect_err("ambiguous mutation"),
            ApiError::AmbiguousOutcome
        );
    }

    #[tokio::test]
    async fn maximum_size_mutation_response_is_accepted() {
        let server = MockServer::start().await;
        let prefix = br#"{"success":true,"message":""#;
        let suffix = br#""}"#;
        let message_length = MAXIMUM_RESPONSE_BYTES - prefix.len() - suffix.len();
        let mut body = Vec::with_capacity(MAXIMUM_RESPONSE_BYTES);
        body.extend_from_slice(prefix);
        body.resize(prefix.len() + message_length, b'x');
        body.extend_from_slice(suffix);
        assert_eq!(body.len(), MAXIMUM_RESPONSE_BYTES);

        Mock::given(method("POST"))
            .and(path("/api/v1/printers/7/print/resume"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(1)
            .mount(&server)
            .await;

        let receipt = client(&server)
            .control(7, ControlAction::Resume)
            .await
            .expect("maximum-size mutation response");
        assert!(receipt.success);
        assert_eq!(receipt.message.len(), message_length);
    }

    #[test]
    fn client_policy_rejects_public_plaintext_and_redacts_the_key() {
        let key = ApiKey::new("SENTINEL_KEY".into()).expect("API key");
        assert_eq!(format!("{key:?}"), "ApiKey([REDACTED])");
        let result = Client::new(
            Url::parse("http://bambuddy.example.com/").expect("URL"),
            key,
            Duration::from_secs(2),
        );
        assert_eq!(
            result.expect_err("public plaintext rejected"),
            ApiError::InvalidConfiguration("plaintext Bambuddy address")
        );
        assert!(matches!(
            Client::new(
                Url::parse("https://bambuddy.test/").expect("URL"),
                ApiKey::new("bb_test_key".into()).expect("API key"),
                Duration::from_secs(31),
            ),
            Err(ApiError::InvalidConfiguration("request timeout"))
        ));
    }
}
