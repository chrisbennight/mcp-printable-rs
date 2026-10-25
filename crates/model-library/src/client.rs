//! The shared HTTP boundary: fixed API origins, bounded responses, status
//! classification and host-checked file downloads.

use std::{
    fmt,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use reqwest::{Client as HttpClient, RequestBuilder, StatusCode, header, redirect::Policy};
use serde::de::DeserializeOwned;
use url::{Host, Url};

use crate::{
    LibraryError, MAXIMUM_SEARCH_LIMIT, ModelDetail, SearchPage, SearchQuery, Site, Token,
    makerworld, printables, reference, text, thingiverse,
};

/// Largest JSON response accepted from a site API.
pub(crate) const MAXIMUM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Redirects a download may follow before it is refused.
pub const MAXIMUM_REDIRECTS: usize = 5;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest time one file transfer may take, so a host that trickles bytes
/// cannot hold a library request indefinitely.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAXIMUM_QUERY_CHARACTERS: usize = 200;
const MAXIMUM_PAGE: u32 = 1000;
const USER_AGENT: &str = concat!("printable/", env!("CARGO_PKG_VERSION"));

/// Optional site access tokens. Each is sent only to its own site's API.
#[derive(Clone, Default)]
pub struct Credentials {
    /// A Bambu Lab account access token, required for MakerWorld downloads.
    pub makerworld: Option<Token>,
    /// A Thingiverse app token, required for every Thingiverse request.
    pub thingiverse: Option<Token>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("makerworld", &self.makerworld.is_some())
            .field("thingiverse", &self.thingiverse.is_some())
            .finish()
    }
}

/// Where requests go. Production uses fixed public origins; tests use one
/// loopback origin for every site.
#[derive(Debug)]
pub(crate) struct Endpoints {
    pub(crate) makerworld: Url,
    pub(crate) printables: Url,
    pub(crate) thingiverse: Url,
    /// Bambu Lab's website host, which answers authenticator sign-in codes.
    pub(crate) bambu_web: Url,
    pub(crate) loopback: bool,
}

impl Endpoints {
    fn public() -> Self {
        let parse = |value| Url::parse(value).expect("fixed site API origin");
        Self {
            makerworld: parse("https://api.bambulab.com/"),
            printables: parse("https://api.printables.com/graphql/"),
            thingiverse: parse("https://api.thingiverse.com/"),
            bambu_web: parse("https://bambulab.com/"),
            loopback: false,
        }
    }

    pub(crate) fn api(&self, site: Site) -> &Url {
        match site {
            Site::MakerWorld => &self.makerworld,
            Site::Printables => &self.printables,
            Site::Thingiverse => &self.thingiverse,
        }
    }
}

/// Bounded client for every supported site.
#[derive(Clone)]
pub struct Client {
    http: HttpClient,
    pub(crate) endpoints: Arc<Endpoints>,
    /// Replaceable so a renewed MakerWorld sign-in takes effect immediately.
    credentials: Arc<RwLock<Credentials>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("endpoints", &self.endpoints)
            .field(
                "credentials",
                &*self
                    .credentials
                    .read()
                    .unwrap_or_else(PoisonError::into_inner),
            )
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Construct a client for the public sites.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(credentials: Credentials) -> Result<Self, LibraryError> {
        Self::build(Endpoints::public(), credentials)
    }

    /// Construct a client that sends every site's requests, and accepts every
    /// download, on one plain-HTTP loopback origin. Intended for tests with
    /// local fake sites: MakerWorld's API under `/makerworld/`, Bambu Lab's
    /// website under `/bambu-web/`, Printables at `/printables/graphql/` and
    /// Thingiverse under `/thingiverse/`.
    ///
    /// # Errors
    ///
    /// Returns an error when `base` is not a loopback HTTP origin.
    pub fn loopback(base: &Url, credentials: Credentials) -> Result<Self, LibraryError> {
        let invalid = LibraryError::InvalidConfiguration("loopback base URL");
        if base.scheme() != "http" || !is_loopback(base) || base.path() != "/" {
            return Err(invalid);
        }
        let join = |path| base.join(path).map_err(|_| invalid.clone());
        Self::build(
            Endpoints {
                makerworld: join("makerworld/")?,
                printables: join("printables/graphql/")?,
                thingiverse: join("thingiverse/")?,
                bambu_web: join("bambu-web/")?,
                loopback: true,
            },
            credentials,
        )
    }

    fn build(endpoints: Endpoints, credentials: Credentials) -> Result<Self, LibraryError> {
        let http = HttpClient::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(Policy::none())
            .no_proxy()
            .https_only(!endpoints.loopback)
            .build()
            .map_err(|_| LibraryError::InvalidConfiguration("HTTP client"))?;
        Ok(Self {
            http,
            endpoints: Arc::new(endpoints),
            credentials: Arc::new(RwLock::new(credentials)),
        })
    }

    /// Whether a site's access token is configured.
    pub fn has_credentials(&self, site: Site) -> bool {
        self.token(site).is_some()
    }

    pub(crate) fn token(&self, site: Site) -> Option<Token> {
        let credentials = self
            .credentials
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match site {
            Site::MakerWorld => credentials.makerworld.clone(),
            Site::Thingiverse => credentials.thingiverse.clone(),
            Site::Printables => None,
        }
    }

    /// Replace a site's access token, for example after a new sign-in. Sites
    /// without tokens ignore it.
    pub fn set_token(&self, site: Site, token: Option<Token>) {
        let mut credentials = self
            .credentials
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match site {
            Site::MakerWorld => credentials.makerworld = token,
            Site::Thingiverse => credentials.thingiverse = token,
            Site::Printables => {}
        }
    }

    pub(crate) fn required_token(&self, site: Site) -> Result<Token, LibraryError> {
        self.token(site)
            .ok_or(LibraryError::CredentialsRequired { site })
    }

    /// Search one site by keyword.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid query or a site failure.
    pub async fn search(
        &self,
        site: Site,
        query: &SearchQuery,
    ) -> Result<SearchPage, LibraryError> {
        let text = query.text.trim();
        if text.is_empty() || text.chars().count() > MAXIMUM_QUERY_CHARACTERS {
            return Err(LibraryError::InvalidRequest(format!(
                "query must contain 1 to {MAXIMUM_QUERY_CHARACTERS} characters"
            )));
        }
        if !(1..=MAXIMUM_PAGE).contains(&query.page) {
            return Err(LibraryError::InvalidRequest(format!(
                "page must be between 1 and {MAXIMUM_PAGE}"
            )));
        }
        if !(1..=MAXIMUM_SEARCH_LIMIT).contains(&query.limit) {
            return Err(LibraryError::InvalidRequest(format!(
                "limit must be between 1 and {MAXIMUM_SEARCH_LIMIT}"
            )));
        }
        let query = SearchQuery {
            text: text.to_owned(),
            ..query.clone()
        };
        let mut page = match site {
            Site::MakerWorld => makerworld::search(self, &query).await,
            Site::Printables => printables::search(self, &query).await,
            Site::Thingiverse => thingiverse::search(self, &query).await,
        }?;
        page.next_page = page.next_page.filter(|next| *next <= MAXIMUM_PAGE);
        Ok(page)
    }

    /// Describe one model and list its downloadable files.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reference or a site failure.
    pub async fn model(&self, site: Site, reference: &str) -> Result<ModelDetail, LibraryError> {
        let id = reference::model_id(site, reference)?;
        match site {
            Site::MakerWorld => makerworld::model(self, &id).await,
            Site::Printables => printables::model(self, &id).await,
            Site::Thingiverse => thingiverse::model(self, &id).await,
        }
    }

    /// Open one listed file for streaming, refusing bodies over `limit` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is not listed, the site refuses it, a
    /// redirect leaves the allowed hosts or the declared size exceeds `limit`.
    pub async fn open_file(
        &self,
        model: &ModelDetail,
        file_id: &str,
        limit: u64,
    ) -> Result<FileStream, LibraryError> {
        let file = model
            .files
            .iter()
            .find(|file| file.file_id == file_id)
            .ok_or_else(|| {
                LibraryError::InvalidRequest(format!(
                    "{} model {} lists no file {file_id}",
                    model.site, model.model_id
                ))
            })?;
        let site = model.site;
        let url = match site {
            Site::MakerWorld => makerworld::file_url(self, model, file).await?,
            Site::Printables => printables::file_url(self, model, file).await?,
            Site::Thingiverse => thingiverse::file_url(file)?,
        };
        self.download(site, url, limit).await
    }

    /// A JSON GET to a site API with a bounded deadline.
    pub(crate) fn api_get(&self, url: Url) -> RequestBuilder {
        self.http
            .get(url)
            .timeout(API_TIMEOUT)
            .header(header::ACCEPT, "application/json")
    }

    /// A JSON POST to a site API with a bounded deadline.
    pub(crate) fn api_post(&self, url: Url, body: &serde_json::Value) -> RequestBuilder {
        self.http
            .post(url)
            .timeout(API_TIMEOUT)
            .header(header::ACCEPT, "application/json")
            .json(body)
    }

    /// Send a request and keep its status, headers and bounded body, whatever
    /// the status, for exchanges that interpret refusals themselves.
    pub(crate) async fn exchange(
        &self,
        site: Site,
        request: RequestBuilder,
    ) -> Result<Exchange, LibraryError> {
        let response = request
            .send()
            .await
            .map_err(|_| LibraryError::Unavailable { site })?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = read_bounded(site, response).await?;
        Ok(Exchange {
            status,
            headers,
            body,
        })
    }

    /// Send an API request and decode its bounded JSON body. `authenticated`
    /// records whether the request carried the site's access token.
    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        site: Site,
        request: RequestBuilder,
        authenticated: bool,
    ) -> Result<T, LibraryError> {
        let response = request
            .send()
            .await
            .map_err(|_| LibraryError::Unavailable { site })?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.refusal(site, response, authenticated).await);
        }
        let body = read_bounded(site, response).await?;
        serde_json::from_slice(&body).map_err(|_| LibraryError::IncompatibleResponse { site })
    }

    async fn download(
        &self,
        site: Site,
        mut url: Url,
        limit: u64,
    ) -> Result<FileStream, LibraryError> {
        // Only Thingiverse file links start on its API origin and need its
        // token. The token goes on that first request alone, never to a
        // redirect target, so a file host cannot steer it to another API path.
        let mut token = self
            .token(site)
            .filter(|_| site == Site::Thingiverse)
            .filter(|_| url.origin() == self.endpoints.api(site).origin());
        for _ in 0..=MAXIMUM_REDIRECTS {
            if !self.file_host_allowed(site, &url) {
                return Err(LibraryError::DisallowedHost { site });
            }
            let mut request = self.http.get(url.clone()).timeout(DOWNLOAD_TIMEOUT);
            let sent = token.take();
            if let Some(token) = &sent {
                request = request.bearer_auth(token.expose());
            }
            let response = request
                .send()
                .await
                .map_err(|_| LibraryError::Unavailable { site })?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(LibraryError::IncompatibleResponse { site })?;
                url = url
                    .join(location)
                    .map_err(|_| LibraryError::IncompatibleResponse { site })?;
                continue;
            }
            if !status.is_success() {
                return Err(self.refusal(site, response, sent.is_some()).await);
            }
            if response.content_length().is_some_and(|size| size > limit) {
                return Err(LibraryError::FileTooLarge { limit });
            }
            return Ok(FileStream {
                site,
                response,
                limit,
                received: 0,
            });
        }
        Err(LibraryError::IncompatibleResponse { site })
    }

    fn file_host_allowed(&self, site: Site, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        if self.endpoints.loopback {
            return url.scheme() == "http" && is_loopback(url);
        }
        if url.scheme() != "https" || url.port().is_some() {
            return false;
        }
        if url.origin() == self.endpoints.api(site).origin() {
            return true;
        }
        let Some(Host::Domain(host)) = url.host() else {
            return false;
        };
        match site {
            Site::Printables => printables::FILE_HOSTS.contains(&host),
            Site::MakerWorld => makerworld::file_host_allowed(host),
            Site::Thingiverse => thingiverse::FILE_HOSTS.contains(&host),
        }
    }

    /// Classify an unsuccessful response, keeping only a bounded site reason.
    /// `sent` records whether this request carried the site's token.
    async fn refusal(&self, site: Site, response: reqwest::Response, sent: bool) -> LibraryError {
        let status = response.status();
        let challenged = response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"challenge"));
        let body = read_bounded(site, response).await.unwrap_or_default();
        let credentials = if sent {
            Credential::Sent
        } else if site == Site::Printables || self.token(site).is_some() {
            Credential::NotApplicable
        } else {
            Credential::Missing
        };
        classify(site, status, credentials, challenged, &body)
    }
}

/// What the token situation says about an unauthorized response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Credential {
    /// The request carried the site's token, so the site rejected it.
    Sent,
    /// The site needs a token that is not configured.
    Missing,
    /// No configurable token would change the answer.
    NotApplicable,
}

/// A complete response whose status the caller interprets.
pub(crate) struct Exchange {
    pub(crate) status: StatusCode,
    pub(crate) headers: header::HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Exchange {
    /// Whether an anti-bot layer, rather than the site's application,
    /// answered: a Cloudflare challenge or block, or Bambu Lab's CAPTCHA.
    pub(crate) fn challenged(&self) -> bool {
        let cloudflare = matches!(self.status.as_u16(), 403 | 429 | 503)
            && String::from_utf8_lossy(&self.body)
                .to_ascii_lowercase()
                .contains("cloudflare");
        self.status.as_u16() == 418
            || cloudflare
            || self
                .headers
                .get("cf-mitigated")
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"challenge"))
    }

    /// The value of a cookie the response sets.
    pub(crate) fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookie(name)
            .and_then(|cookie| cookie.split(';').next()?.trim().split_once('='))
            .map(|(_, value)| value.trim_matches('"').to_owned())
            .filter(|value| !value.is_empty())
    }

    /// The `Max-Age` attribute of a cookie the response sets, in seconds.
    pub(crate) fn cookie_max_age(&self, name: &str) -> Option<u64> {
        self.set_cookie(name)?
            .split(';')
            .skip(1)
            .find_map(|attribute| {
                let (key, value) = attribute.trim().split_once('=')?;
                key.eq_ignore_ascii_case("max-age")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
    }

    fn set_cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| {
                value
                    .split(';')
                    .next()
                    .and_then(|pair| pair.trim().split_once('='))
                    .is_some_and(|(cookie, value)| cookie == name && !value.is_empty())
            })
    }

    pub(crate) fn reason(&self) -> Option<String> {
        refusal_reason(&self.body)
    }
}

/// A file body being received, bounded by the caller's byte limit.
pub struct FileStream {
    site: Site,
    response: reqwest::Response,
    limit: u64,
    received: u64,
}

impl fmt::Debug for FileStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileStream")
            .field("site", &self.site)
            .field("limit", &self.limit)
            .field("received", &self.received)
            .finish_non_exhaustive()
    }
}

impl FileStream {
    /// The size the file host declared, when it declared one.
    pub fn declared_size(&self) -> Option<u64> {
        self.response.content_length()
    }

    /// The next part of the body, or `None` once it is complete.
    ///
    /// # Errors
    ///
    /// Returns an error when the transfer fails or exceeds the limit.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, LibraryError> {
        let site = self.site;
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|_| LibraryError::Interrupted { site })?;
        if let Some(chunk) = &chunk {
            self.received = self.received.saturating_add(chunk.len() as u64);
            if self.received > self.limit {
                return Err(LibraryError::FileTooLarge { limit: self.limit });
            }
        }
        Ok(chunk)
    }
}

async fn read_bounded(
    site: Site,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, LibraryError> {
    if response
        .content_length()
        .is_some_and(|size| size > MAXIMUM_RESPONSE_BYTES as u64)
    {
        return Err(LibraryError::ResponseTooLarge { site });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| LibraryError::Unavailable { site })?
    {
        if body.len().saturating_add(chunk.len()) > MAXIMUM_RESPONSE_BYTES {
            return Err(LibraryError::ResponseTooLarge { site });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn classify(
    site: Site,
    status: StatusCode,
    credentials: Credential,
    challenged: bool,
    body: &[u8],
) -> LibraryError {
    if challenged || status.as_u16() == 418 {
        return LibraryError::Challenged { site };
    }
    match status {
        StatusCode::UNAUTHORIZED => match credentials {
            Credential::Sent => LibraryError::Authentication { site },
            Credential::Missing => LibraryError::CredentialsRequired { site },
            Credential::NotApplicable => LibraryError::Forbidden {
                site,
                reason: refusal_reason(body)
                    .unwrap_or_else(|| "the site requires an account for this request".into()),
            },
        },
        StatusCode::FORBIDDEN => LibraryError::Forbidden {
            site,
            reason: refusal_reason(body).unwrap_or_else(|| "no reason was given".into()),
        },
        StatusCode::NOT_FOUND | StatusCode::GONE => LibraryError::NotFound { site },
        StatusCode::TOO_MANY_REQUESTS => LibraryError::RateLimited { site },
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => match refusal_reason(body) {
            Some(reason) => LibraryError::RequestRejected { site, reason },
            None => LibraryError::IncompatibleResponse { site },
        },
        status if status.is_server_error() => LibraryError::Unavailable { site },
        _ => LibraryError::IncompatibleResponse { site },
    }
}

/// The first non-empty message field of a JSON error body, including the
/// first message of a GraphQL `errors` list.
fn refusal_reason(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let graphql = value.pointer("/errors/0/message");
    ["error", "message", "detail", "msg"]
        .iter()
        .filter_map(|key| value.get(key))
        .chain(graphql)
        .filter_map(serde_json::Value::as_str)
        .map(|reason| text::bounded(reason, text::REASON_TEXT))
        .find(|reason| !reason.is_empty())
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain == "localhost",
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public() -> Client {
        Client::new(Credentials::default()).expect("public client")
    }

    #[test]
    fn public_downloads_stay_on_each_sites_https_file_hosts() {
        let client = public();
        for (site, url, allowed) in [
            (
                Site::Printables,
                "https://files.printables.com/media/prints/1/stls/a.stl",
                true,
            ),
            (
                Site::Printables,
                "https://files.printables.com.evil.example/a.stl",
                false,
            ),
            (Site::Printables, "http://files.printables.com/a.stl", false),
            (
                Site::Printables,
                "https://files.printables.com:8443/a.stl",
                false,
            ),
            (
                Site::Printables,
                "https://user:secret@files.printables.com/a.stl",
                false,
            ),
            (Site::Printables, "https://cdn.thingiverse.com/a.stl", false),
            (
                Site::MakerWorld,
                "https://makerworld.bblmw.com/makerworld/model/a.3mf?at=1",
                true,
            ),
            (
                Site::MakerWorld,
                "https://s3.us-west-2.amazonaws.com/bucket/a.3mf",
                true,
            ),
            (
                Site::MakerWorld,
                "https://s3.amazonaws.com/bucket/a.3mf",
                true,
            ),
            (
                Site::MakerWorld,
                "https://bucket.s3-us-west-2.amazonaws.com/a.3mf",
                false,
            ),
            (
                Site::MakerWorld,
                "https://bucket.s3.us-west-2.amazonaws.com/a.3mf",
                false,
            ),
            (
                Site::MakerWorld,
                "https://a-1.s3-object-lambda.us-west-2.amazonaws.com/a.3mf",
                false,
            ),
            (Site::MakerWorld, "https://evil.amazonaws.com/a.3mf", false),
            (
                Site::MakerWorld,
                "https://s3.amazonaws.com.evil.example/a.3mf",
                false,
            ),
            (Site::MakerWorld, "https://127.0.0.1/a.3mf", false),
            (
                Site::Thingiverse,
                "https://api.thingiverse.com/files/1/download",
                true,
            ),
            (
                Site::Thingiverse,
                "https://cdn.thingiverse.com/assets/a.stl",
                true,
            ),
            (
                Site::Thingiverse,
                "https://files.printables.com/a.stl",
                false,
            ),
        ] {
            let url = Url::parse(url).expect("URL");
            assert_eq!(
                client.file_host_allowed(site, &url),
                allowed,
                "{site} {url}"
            );
        }
    }

    #[test]
    fn loopback_clients_refuse_public_origins_and_downloads() {
        let credentials = Credentials::default;
        for base in [
            "https://127.0.0.1:1/",
            "http://example.com/",
            "http://127.0.0.1:1/api/",
        ] {
            assert!(
                Client::loopback(&Url::parse(base).unwrap(), credentials()).is_err(),
                "{base}"
            );
        }
        let client = Client::loopback(&Url::parse("http://127.0.0.1:1/").unwrap(), credentials())
            .expect("loopback client");
        assert!(!client.file_host_allowed(
            Site::Printables,
            &Url::parse("http://files.printables.com/a.stl").unwrap()
        ));
        assert!(client.file_host_allowed(
            Site::Printables,
            &Url::parse("http://127.0.0.1:9/a.stl").unwrap()
        ));
    }

    #[test]
    fn status_classification_keeps_only_a_bounded_reason() {
        let site = Site::MakerWorld;
        let long = "x".repeat(1000);
        let forbidden = classify(
            site,
            StatusCode::FORBIDDEN,
            Credential::Sent,
            false,
            format!(r#"{{"error":"{long}"}}"#).as_bytes(),
        );
        let LibraryError::Forbidden { reason, .. } = forbidden else {
            panic!("expected a refusal");
        };
        assert_eq!(reason.chars().count(), text::REASON_TEXT + 1);
        let classified = |status, credentials, challenged, body: &[u8]| {
            classify(site, status, credentials, challenged, body)
        };
        assert_eq!(
            classified(StatusCode::FORBIDDEN, Credential::Missing, false, b"<html>"),
            LibraryError::Forbidden {
                site,
                reason: "no reason was given".into()
            }
        );
        assert_eq!(
            classified(StatusCode::FORBIDDEN, Credential::Missing, true, b""),
            LibraryError::Challenged { site }
        );
        assert_eq!(
            classified(
                StatusCode::IM_A_TEAPOT,
                Credential::Sent,
                false,
                br#"{"captchaId":"1"}"#
            ),
            LibraryError::Challenged { site }
        );
        assert_eq!(
            classified(StatusCode::UNAUTHORIZED, Credential::Sent, false, b""),
            LibraryError::Authentication { site }
        );
        assert_eq!(
            classified(StatusCode::UNAUTHORIZED, Credential::Missing, false, b""),
            LibraryError::CredentialsRequired { site }
        );
        assert_eq!(
            classified(
                StatusCode::UNAUTHORIZED,
                Credential::NotApplicable,
                false,
                b""
            ),
            LibraryError::Forbidden {
                site,
                reason: "the site requires an account for this request".into()
            }
        );
        assert_eq!(
            classified(
                StatusCode::TOO_MANY_REQUESTS,
                Credential::Missing,
                false,
                b""
            ),
            LibraryError::RateLimited { site }
        );
        assert_eq!(
            classified(StatusCode::BAD_GATEWAY, Credential::Missing, false, b""),
            LibraryError::Unavailable { site }
        );
        assert_eq!(
            classified(
                StatusCode::BAD_REQUEST,
                Credential::NotApplicable,
                false,
                br#"{"errors":[{"message":"Unknown argument \"files\""}]}"#
            ),
            LibraryError::RequestRejected {
                site,
                reason: "Unknown argument \"files\"".into()
            }
        );
        assert_eq!(
            classified(
                StatusCode::BAD_REQUEST,
                Credential::Missing,
                false,
                b"<html>"
            ),
            LibraryError::IncompatibleResponse { site }
        );
    }

    #[test]
    fn credentials_and_tokens_never_appear_in_debug_output() {
        let credentials = Credentials {
            makerworld: Some(Token::new("SENTINEL_MAKERWORLD".into()).unwrap()),
            thingiverse: Some(Token::new("SENTINEL_THINGIVERSE".into()).unwrap()),
        };
        let client = Client::new(credentials.clone()).unwrap();
        for rendered in [format!("{credentials:?}"), format!("{client:?}")] {
            assert!(!rendered.contains("SENTINEL"), "{rendered}");
        }
        assert!(Token::new("has space".into()).is_err());
        assert!(Token::new("line\nbreak".into()).is_err());
        assert!(Token::new(String::new()).is_err());
    }
}
