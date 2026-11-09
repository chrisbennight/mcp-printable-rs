//! The read-only project library web page.
//!
//! The page runs on its own listener, separate from the MCP endpoint, and only
//! when `PRINTABLE_WEB_LISTEN` is set. Signing in belongs to an operator's
//! reverse proxy: a request is served only when its TCP peer is a configured
//! proxy address and the proxy's user header names exactly one configured
//! user. Every route reads the workspace; none changes it. Text from project
//! records, model sites and file names is untrusted and reaches pages only
//! through templates that escape it.

mod pages;

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Path, Request, State, rejection::PathRejection},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use maud::Markup;
use printable_workspace::{Workspace, WsError};
use tokio::sync::Semaphore;

use crate::{
    config::SettingsError,
    error::ToolError,
    file_transfer,
    projects::{
        self,
        card::{self, Card},
        cover,
        index::{MAX_OFFSET, ProjectIndex, SearchFilters, SearchParams, SearchSort},
        pictures,
    },
};

const LISTEN_KEY: &str = "PRINTABLE_WEB_LISTEN";
const PROXIES_KEY: &str = "PRINTABLE_WEB_TRUSTED_PROXIES";
const USER_HEADER_KEY: &str = "PRINTABLE_WEB_USER_HEADER";
const USERS_KEY: &str = "PRINTABLE_WEB_USERS";

/// Projects on one library page.
const PAGE_SIZE: usize = 24;
/// The last page whose offset the search index accepts.
const MAX_PAGE: usize = MAX_OFFSET / PAGE_SIZE + 1;
/// Longest accepted query string. Search words and filters are far shorter;
/// the cap bounds parsing before search validation runs.
const MAX_QUERY_BYTES: usize = 16 * 1024;
/// Files listed on a project page.
const MAX_LISTED_FILES: usize = 500;
/// Workspace reads that web requests run at once, apart from MCP tool calls.
const MAX_CONCURRENT_READS: usize = 4;
/// Downloads in progress at once. Each keeps a copy of its file until it ends.
const MAX_CONCURRENT_DOWNLOADS: usize = 4;
/// Text filters in the order the search form shows them.
const TEXT_FILTERS: [(&str, &str); 7] = [
    ("printer", "Printer"),
    ("material", "Material"),
    ("colour", "Colour"),
    ("tag", "Tag"),
    ("source", "Source"),
    ("license", "License"),
    ("format", "Format"),
];
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'none'; img-src 'self'; style-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";
const STYLESHEET: &str = include_str!("web/app.css");

/// Who may read the web page and where it listens.
#[derive(Debug, Clone)]
pub struct WebSettings {
    /// The socket address of the web page's own listener.
    pub listen: SocketAddr,
    trusted_proxies: Vec<AddressRange>,
    user_header: HeaderName,
    users: Vec<String>,
}

impl WebSettings {
    fn trusts(&self, peer: IpAddr) -> bool {
        self.trusted_proxies
            .iter()
            .any(|range| range.contains(peer))
    }

    /// The configured user named by a header value, compared exactly.
    fn user(&self, value: &[u8]) -> Option<&str> {
        self.users
            .iter()
            .find(|user| user.as_bytes() == value)
            .map(String::as_str)
    }
}

/// One IP address or a CIDR range of addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddressRange {
    network: IpAddr,
    prefix: u32,
}

impl AddressRange {
    /// Parse `address` or `address/prefix`. A range must not cover every
    /// address, must not set bits beyond its prefix, and uses the IPv4 form
    /// for IPv4 addresses.
    fn parse(raw: &str) -> Option<Self> {
        let (address, prefix) = match raw.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (raw, None),
        };
        let network: IpAddr = address.parse().ok()?;
        if let IpAddr::V6(v6) = network
            && v6.to_ipv4_mapped().is_some()
        {
            return None;
        }
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                digits
                    .parse::<u32>()
                    .ok()
                    .filter(|p| (1..=bits).contains(p))?
            }
            Some(_) => return None,
        };
        let host_bits_clear = match network {
            IpAddr::V4(v4) => u32::from(v4) & !mask32(prefix) == 0,
            IpAddr::V6(v6) => u128::from(v6) & !mask128(prefix) == 0,
        };
        host_bits_clear.then_some(Self { network, prefix })
    }

    fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                u32::from(network) & mask32(self.prefix) == u32::from(address) & mask32(self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                u128::from(network) & mask128(self.prefix)
                    == u128::from(address) & mask128(self.prefix)
            }
            _ => false,
        }
    }
}

/// The network mask for a prefix of 1 to 32 bits.
fn mask32(prefix: u32) -> u32 {
    u32::MAX << (32 - prefix)
}

/// The network mask for a prefix of 1 to 128 bits.
fn mask128(prefix: u32) -> u128 {
    u128::MAX << (128 - prefix)
}

/// Read the web page settings. Without `PRINTABLE_WEB_LISTEN` the page is
/// off and the other settings are not read; with it, all three are required.
pub fn configure(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<WebSettings>, SettingsError> {
    let value = |key: &str| {
        get(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let Some(listen) = value(LISTEN_KEY) else {
        return Ok(None);
    };
    let listen = listen
        .parse::<SocketAddr>()
        .ok()
        .filter(|address| address.port() != 0)
        .ok_or(SettingsError::Web(LISTEN_KEY))?;
    let list = |key: &'static str| -> Result<Vec<String>, SettingsError> {
        let items: Vec<String> = value(key)
            .ok_or(SettingsError::Web(key))?
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect();
        if items.is_empty() {
            return Err(SettingsError::Web(key));
        }
        Ok(items)
    };
    let trusted_proxies = list(PROXIES_KEY)?
        .iter()
        .map(|item| AddressRange::parse(item).ok_or(SettingsError::Web(PROXIES_KEY)))
        .collect::<Result<Vec<_>, _>>()?;
    let user_header = value(USER_HEADER_KEY)
        .and_then(|name| HeaderName::from_bytes(name.as_bytes()).ok())
        .ok_or(SettingsError::Web(USER_HEADER_KEY))?;
    let mut users = list(USERS_KEY)?;
    if users.iter().any(|user| user.chars().any(char::is_control)) {
        return Err(SettingsError::Web(USERS_KEY));
    }
    let mut seen = HashSet::new();
    users.retain(|user| seen.insert(user.clone()));
    Ok(Some(WebSettings {
        listen,
        trusted_proxies,
        user_header,
        users,
    }))
}

#[derive(Clone)]
struct Web {
    workspace: Arc<Workspace>,
    index: Arc<ProjectIndex>,
    settings: Arc<WebSettings>,
    reads: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
}

impl Web {
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
    ) -> Result<T, ToolError> {
        crate::tools::blocking_with(&self.reads, work).await
    }
}

/// The web page's router. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`: a request without
/// its peer address is refused.
pub fn router(
    settings: WebSettings,
    workspace: Arc<Workspace>,
    index: Arc<ProjectIndex>,
) -> Router {
    let web = Web {
        workspace,
        index,
        settings: Arc::new(settings),
        reads: Arc::new(Semaphore::new(MAX_CONCURRENT_READS)),
        downloads: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
    };
    Router::new()
        .route("/", get(library))
        .route("/projects/{project_id}", get(project))
        .route("/projects/{project_id}/cover", get(cover_picture))
        .route("/projects/{project_id}/pictures/plate", get(plate_picture))
        .route(
            "/projects/{project_id}/pictures/render",
            get(render_picture),
        )
        .route("/projects/{project_id}/files/{*path}", get(file))
        .route("/static/app.css", get(stylesheet))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(web.clone(), admit))
        .layer(middleware::map_response(secure))
        .with_state(web)
}

/// Refuse a request unless it comes from a trusted proxy for a listed user
/// and only reads. The peer is checked before anything else.
async fn admit(State(web): State<Web>, request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    if !peer.is_some_and(|peer| web.settings.trusts(peer)) {
        tracing::debug!(
            peer = ?peer,
            "refused a web page request from an address that is not a trusted proxy"
        );
        return refusal(
            StatusCode::FORBIDDEN,
            "This page is available only through its sign-in proxy.",
        );
    }
    let mut values = request.headers().get_all(&web.settings.user_header).iter();
    let user = match (values.next(), values.next()) {
        (Some(value), None) => web.settings.user(value.as_bytes()),
        _ => None,
    };
    if user.is_none() {
        tracing::info!("refused a web page request without exactly one listed user");
        return refusal(
            StatusCode::FORBIDDEN,
            "Your account is not allowed to read this library.",
        );
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let mut response = refusal(StatusCode::METHOD_NOT_ALLOWED, "This page is read-only.");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    next.run(request).await
}

/// Security headers on every response. Responses that may be cached say so
/// themselves; everything else is never stored.
async fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn refusal(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message,
    )
        .into_response()
}

fn html(status: StatusCode, markup: Markup) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        markup.into_string(),
    )
        .into_response()
}

fn page(paths: &Paths, status: StatusCode, title: &str, message: &str) -> Response {
    html(status, pages::message(paths, title, message))
}

/// An error page for a failed workspace or index read.
fn failure(paths: &Paths, error: &ToolError) -> Response {
    let (status, title, message) = match error {
        ToolError::Workspace(WsError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            "Not found",
            "That project or file does not exist.".to_owned(),
        ),
        ToolError::Workspace(
            WsError::PathEscapes
            | WsError::ReservedPath
            | WsError::SymlinkRefused
            | WsError::UnsupportedArtifactType
            | WsError::NotRegularFile,
        ) => (
            StatusCode::NOT_FOUND,
            "Not found",
            "That is not a file that this project offers for download.".to_owned(),
        ),
        ToolError::Workspace(WsError::ChangedWhileReading) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again",
            "The file changed while it was being read. Try again.".to_owned(),
        ),
        ToolError::Workspace(
            WsError::StorageBudgetExceeded | WsError::StorageAccountingIncomplete,
        ) => (
            StatusCode::INSUFFICIENT_STORAGE,
            "No room to prepare the download",
            "The workspace has no room for the temporary copy a download needs.".to_owned(),
        ),
        ToolError::Validation(message) => {
            (StatusCode::BAD_REQUEST, "Cannot show this", message.clone())
        }
        other => {
            tracing::warn!(code = other.code(), error = %other, "a web page request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "The library could not be read. The server log has the details.".to_owned(),
            )
        }
    };
    page(paths, status, title, &message)
}

/// Relative links from the requested page, so the page also works behind a
/// proxy that serves it under a path prefix.
struct Paths {
    root: String,
}

impl Paths {
    /// Pages deeper than this link from the server root instead.
    const MAX_DEPTH: usize = 32;

    fn for_request(path: &str) -> Self {
        let depth = path.matches('/').count().saturating_sub(1);
        let root = match depth {
            0 => "./".to_owned(),
            depth if depth <= Self::MAX_DEPTH => "../".repeat(depth),
            _ => "/".to_owned(),
        };
        Self { root }
    }

    /// The library page.
    fn root(&self) -> &str {
        &self.root
    }

    fn link(&self, path: &str) -> String {
        format!("{}{path}", self.root)
    }
}

/// The search form's values, as the library page received them.
#[derive(Debug)]
struct LibraryForm {
    q: String,
    /// `None` is the search default: best match with words, newest without.
    sort: Option<SearchSort>,
    printed: Option<bool>,
    has_3mf: Option<bool>,
    text: [String; TEXT_FILTERS.len()],
    /// One-based page number.
    page: usize,
}

impl Default for LibraryForm {
    fn default() -> Self {
        Self {
            q: String::new(),
            sort: None,
            printed: None,
            has_3mf: None,
            text: Default::default(),
            page: 1,
        }
    }
}

fn yes_no(name: &str, value: &str) -> Result<Option<bool>, String> {
    match value {
        "" => Ok(None),
        "yes" => Ok(Some(true)),
        "no" => Ok(Some(false)),
        _ => Err(format!("{name} must be yes, no or empty.")),
    }
}

impl LibraryForm {
    /// Read the query string. Unknown parameters are ignored; a known one
    /// given twice is refused rather than guessed.
    fn parse(query: Option<&str>) -> Result<Self, String> {
        let query = query.unwrap_or_default();
        if query.len() > MAX_QUERY_BYTES {
            return Err(format!(
                "The search is longer than {MAX_QUERY_BYTES} bytes."
            ));
        }
        let mut form = Self::default();
        let mut seen = HashSet::new();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let value = value.trim();
            let known = match key.as_ref() {
                "q" => {
                    form.q = value.to_owned();
                    "q"
                }
                "sort" => {
                    form.sort = match value {
                        "" | "best_match" => None,
                        "last_printed" => Some(SearchSort::LastPrinted),
                        "newest" => Some(SearchSort::Newest),
                        "name" => Some(SearchSort::Name),
                        _ => return Err("Choose a sort order from the list.".to_owned()),
                    };
                    "sort"
                }
                "printed" => {
                    form.printed = yes_no("printed", value)?;
                    "printed"
                }
                "has_3mf" => {
                    form.has_3mf = yes_no("has_3mf", value)?;
                    "has_3mf"
                }
                "page" => {
                    form.page = match value {
                        "" => 1,
                        value => value
                            .parse::<usize>()
                            .ok()
                            .filter(|page| (1..=MAX_PAGE).contains(page))
                            .ok_or_else(|| {
                                format!("The page must be a number from 1 to {MAX_PAGE}.")
                            })?,
                    };
                    "page"
                }
                other => {
                    let Some(position) = TEXT_FILTERS.iter().position(|(name, _)| *name == other)
                    else {
                        continue;
                    };
                    form.text[position] = value.to_owned();
                    TEXT_FILTERS[position].0
                }
            };
            if !seen.insert(known) {
                return Err(format!("The {known} parameter is given more than once."));
            }
        }
        Ok(form)
    }

    fn search_params(&self) -> SearchParams {
        let text = |value: &String| (!value.is_empty()).then(|| value.clone());
        let [printer, material, colour, tag, source, license, format] =
            self.text.each_ref().map(text);
        SearchParams {
            text: text(&self.q),
            filters: SearchFilters {
                printed: self.printed,
                printer,
                has_3mf: self.has_3mf,
                material,
                colour,
                tag,
                source,
                license,
                format,
            },
            sort: self.sort,
            limit: PAGE_SIZE,
            offset: (self.page - 1) * PAGE_SIZE,
            picture: None,
            combine: Default::default(),
            detail: Default::default(),
        }
    }

    /// The order results are shown in.
    fn effective_sort(&self) -> SearchSort {
        match self.sort {
            Some(sort) => sort,
            None if self.q.is_empty() => SearchSort::Newest,
            None => SearchSort::BestMatch,
        }
    }

    /// A query string for another page of the same search.
    fn query(&self, page: usize) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if !self.q.is_empty() {
            query.append_pair("q", &self.q);
        }
        match self.sort {
            None => {}
            Some(SearchSort::BestMatch) => {
                query.append_pair("sort", "best_match");
            }
            Some(SearchSort::LastPrinted) => {
                query.append_pair("sort", "last_printed");
            }
            Some(SearchSort::Newest) => {
                query.append_pair("sort", "newest");
            }
            Some(SearchSort::Name) => {
                query.append_pair("sort", "name");
            }
        }
        for (name, flag) in [("printed", self.printed), ("has_3mf", self.has_3mf)] {
            if let Some(flag) = flag {
                query.append_pair(name, if flag { "yes" } else { "no" });
            }
        }
        for ((name, _), value) in TEXT_FILTERS.iter().zip(&self.text) {
            if !value.is_empty() {
                query.append_pair(name, value);
            }
        }
        query.append_pair("page", &page.to_string());
        format!("?{}", query.finish())
    }

    fn is_blank(&self) -> bool {
        self.q.is_empty()
            && self.printed.is_none()
            && self.has_3mf.is_none()
            && self.text.iter().all(String::is_empty)
    }
}

async fn library(State(web): State<Web>, uri: Uri) -> Response {
    let paths = Paths::for_request(uri.path());
    let form = match LibraryForm::parse(uri.query()) {
        Ok(form) => form,
        Err(message) => return page(&paths, StatusCode::BAD_REQUEST, "Cannot search", &message),
    };
    let params = form.search_params();
    match web.index.search(&web.workspace, params, &web.reads).await {
        Ok(response) => html(StatusCode::OK, pages::library(&paths, &form, Ok(&response))),
        Err(ToolError::Validation(message)) => html(
            StatusCode::BAD_REQUEST,
            pages::library(&paths, &form, Err(&message)),
        ),
        Err(error) => failure(&paths, &error),
    }
}

/// A file in a project, as the project page lists it.
struct FileEntry {
    /// Relative to the project directory.
    path: String,
    size_bytes: u64,
    modified_unix_ms: u64,
}

/// What the project page shows: the full card and the first files.
struct ProjectView {
    card: Card,
    files: Vec<FileEntry>,
}

impl ProjectView {
    /// Build the card from recorded summaries only, so showing a page never
    /// copies a slice package.
    fn gather(workspace: &Workspace, project_id: &str) -> Result<Self, ToolError> {
        let inputs = card::Inputs::gather(workspace, project_id)?;
        let card = card::build(workspace, &inputs, card::Summaries::Recorded);
        let files = inputs
            .project_files()
            .take(MAX_LISTED_FILES)
            .map(|(path, meta)| FileEntry {
                path: path.to_owned(),
                size_bytes: meta.size_bytes,
                modified_unix_ms: u64::try_from(meta.modified_ns / 1_000_000).unwrap_or(0),
            })
            .collect();
        Ok(Self { card, files })
    }
}

async fn project(
    State(web): State<Web>,
    uri: Uri,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let paths = Paths::for_request(uri.path());
    let Ok(Path(project_id)) = path else {
        return page(
            &paths,
            StatusCode::BAD_REQUEST,
            "Cannot show this",
            "The address is not a project address.",
        );
    };
    let workspace = Arc::clone(&web.workspace);
    match web
        .blocking(move || ProjectView::gather(&workspace, &project_id))
        .await
    {
        Ok(view) => html(StatusCode::OK, pages::project(&paths, &view)),
        Err(error) => failure(&paths, &error),
    }
}

/// Whether an `If-None-Match` header names `tag`.
fn etag_matches(condition: &[u8], tag: &str) -> bool {
    condition.split(|byte| *byte == b',').any(|candidate| {
        let candidate = candidate.trim_ascii();
        candidate == b"*"
            || candidate == tag.as_bytes()
            || candidate.strip_prefix(b"W/") == Some(tag.as_bytes())
    })
}

/// A stored picture to send: its media type, digest and workspace path.
struct StoredPicture {
    media_type: String,
    sha256: String,
    path: String,
}

/// The stored cover picture, as stored.
async fn cover_picture(
    State(web): State<Web>,
    uri: Uri,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    stored_picture(
        web,
        uri,
        headers,
        path,
        |workspace, project_id| {
            Ok(projects::get(workspace, project_id)?
                .cover
                .map(|cover| StoredPicture {
                    media_type: cover.media_type,
                    sha256: cover.sha256,
                    path: cover.path,
                }))
        },
        "This project has no cover picture.",
    )
    .await
}

/// The plate picture generated from the project's model.
async fn plate_picture(
    State(web): State<Web>,
    uri: Uri,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    stored_picture(
        web,
        uri,
        headers,
        path,
        |workspace, project_id| {
            projects::get(workspace, project_id)?;
            Ok(pictures::read(workspace, project_id)
                .and_then(|record| record.plate(project_id).cloned())
                .map(|plate| StoredPicture {
                    media_type: "image/png".into(),
                    sha256: plate.sha256,
                    path: plate.path,
                }))
        },
        "This project has no generated plate picture.",
    )
    .await
}

async fn render_picture(
    State(web): State<Web>,
    uri: Uri,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    stored_picture(
        web,
        uri,
        headers,
        path,
        |workspace, project_id| {
            projects::get(workspace, project_id)?;
            Ok(pictures::read(workspace, project_id)
                .and_then(|record| record.render(project_id).cloned())
                .map(|picture| StoredPicture {
                    media_type: "image/png".into(),
                    sha256: picture.sha256,
                    path: picture.path,
                }))
        },
        "This project has no generated Blender picture.",
    )
    .await
}

/// Send a stored picture as stored. Stored pictures are named by their
/// digest, so the digest is the entity tag and an unchanged picture is not
/// sent again.
async fn stored_picture(
    web: Web,
    uri: Uri,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    find: fn(&Workspace, &str) -> Result<Option<StoredPicture>, ToolError>,
    missing: &'static str,
) -> Response {
    let paths = Paths::for_request(uri.path());
    let Ok(Path(project_id)) = path else {
        return page(
            &paths,
            StatusCode::BAD_REQUEST,
            "Cannot show this",
            "The address is not a project address.",
        );
    };
    let condition = headers
        .get(header::IF_NONE_MATCH)
        .map(|value| value.as_bytes().to_vec());
    let workspace = Arc::clone(&web.workspace);
    let outcome = web
        .blocking(move || {
            let Some(picture) = find(&workspace, &project_id)? else {
                return Ok(None);
            };
            let tag = format!("\"{}\"", picture.sha256);
            if condition
                .as_deref()
                .is_some_and(|condition| etag_matches(condition, &tag))
            {
                return Ok(Some((picture.media_type, tag, None)));
            }
            if workspace.stat_artifact(&picture.path)?.size_bytes > cover::MAX_COVER_BYTES {
                return Err(ToolError::Validation(
                    "the stored picture is larger than a picture may be".into(),
                ));
            }
            let (_, bytes) = workspace.read_artifact(&picture.path)?;
            Ok(Some((picture.media_type, tag, Some(bytes))))
        })
        .await;
    let (media_type, tag, bytes) = match outcome {
        Ok(Some(found)) => found,
        Ok(None) => {
            return page(&paths, StatusCode::NOT_FOUND, "Not found", missing);
        }
        Err(error) => return failure(&paths, &error),
    };
    let mut response = match bytes {
        None => StatusCode::NOT_MODIFIED.into_response(),
        Some(bytes) => ([(header::CONTENT_TYPE, media_type)], bytes).into_response(),
    };
    let headers = response.headers_mut();
    if let Ok(tag) = HeaderValue::from_str(&tag) {
        headers.insert(header::ETAG, tag);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    response
}

/// Whether a raw request path encodes a slash or backslash, which would
/// otherwise turn into a separator after decoding.
fn has_encoded_separator(path: &str) -> bool {
    path.as_bytes().windows(3).any(|window| {
        window[0] == b'%'
            && (window[1..].eq_ignore_ascii_case(b"2f") || window[1..].eq_ignore_ascii_case(b"5c"))
    })
}

/// A `Content-Disposition` value that saves the file under its own name. The
/// quoted name keeps only safe ASCII; the encoded name keeps the original.
fn attachment(name: &str) -> HeaderValue {
    use std::fmt::Write as _;
    let mut fallback = String::with_capacity(name.len());
    let mut encoded = String::with_capacity(name.len());
    for character in name.chars() {
        fallback.push(
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+') {
                character
            } else {
                '_'
            },
        );
    }
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("the value holds only visible ASCII")
}

/// Download one file from a project. The path is confined to the project's
/// directory; symbolic links, reserved paths and encoded separators are
/// refused. The bytes come from an immutable snapshot and are streamed in
/// small pieces, so a large file is never held in memory.
async fn file(
    State(web): State<Web>,
    method: Method,
    uri: Uri,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let paths = Paths::for_request(uri.path());
    if has_encoded_separator(uri.path()) {
        return page(
            &paths,
            StatusCode::BAD_REQUEST,
            "Cannot download this",
            "File addresses cannot contain encoded slashes or backslashes.",
        );
    }
    let Ok(Path((project_id, path))) = path else {
        return page(
            &paths,
            StatusCode::BAD_REQUEST,
            "Cannot download this",
            "The address is not a project file address.",
        );
    };
    let head = method == Method::HEAD;
    // A download keeps its slot until its copy has been sent or abandoned.
    let slot = if head {
        None
    } else {
        match Arc::clone(&web.downloads).try_acquire_owned() {
            Ok(slot) => Some(slot),
            Err(_) => {
                return page(
                    &paths,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too many downloads",
                    "Other downloads are in progress. Try again when one has finished.",
                );
            }
        }
    };
    let workspace = Arc::clone(&web.workspace);
    let outcome = web
        .blocking(move || -> Result<_, ToolError> {
            let resolved = projects::resolve(&workspace, &project_id, &path)?;
            if head {
                return Ok((workspace.stat_artifact(&resolved)?, None));
            }
            let snapshot = workspace.snapshot_artifact(&resolved)?;
            let meta = snapshot.meta().clone();
            Ok((meta, Some(file_transfer::snapshot_stream(snapshot, slot)?)))
        })
        .await;
    let (meta, stream) = match outcome {
        Ok(found) => found,
        Err(error) => return failure(&paths, &error),
    };
    let name = meta.path.rsplit('/').next().unwrap_or(&meta.path);
    let body = stream.map_or_else(Body::empty, Body::from_stream);
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.size_bytes));
    headers.insert(header::CONTENT_DISPOSITION, attachment(name));
    response
}

async fn stylesheet() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLESHEET,
    )
        .into_response()
}

async fn not_found(uri: Uri) -> Response {
    page(
        &Paths::for_request(uri.path()),
        StatusCode::NOT_FOUND,
        "Not found",
        "There is no page at this address.",
    )
}

#[cfg(test)]
mod tests;
