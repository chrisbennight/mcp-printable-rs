//! Optional embeddings and re-ranking services behind meaning and picture
//! search.
//!
//! The embeddings service speaks OpenAI's `POST <base>/embeddings` API, so a
//! base URL such as `https://host/v1` works with any compatible server.
//! A picture travels as an `image_url` content part in place of a string in
//! the `input` list, an extension that some multimodal servers accept; they
//! are sent only when turned on. The
//! re-ranking service takes `POST <base>/rerank` with a query and documents and
//! answers with `results[].index` and `relevance_score`.
//!
//! Card text and pictures go to the configured services and nowhere else.
//! Answers are read under size limits and checked before use. API keys travel
//! only in the `Authorization` header and never appear in errors, logs or
//! status.

use std::{
    fmt,
    io::{Cursor, Read as _},
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use reqwest::{
    Client as HttpClient, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::{config::SettingsError, error::ToolError};

const URL_KEY: &str = "PRINTABLE_EMBEDDING_URL";
const MODEL_KEY: &str = "PRINTABLE_EMBEDDING_MODEL";
const API_KEY: &str = "PRINTABLE_EMBEDDING_API_KEY";
const API_KEY_FILE: &str = "PRINTABLE_EMBEDDING_API_KEY_FILE";
const DIMENSIONS_KEY: &str = "PRINTABLE_EMBEDDING_DIMENSIONS";
const QUERY_PREFIX_KEY: &str = "PRINTABLE_EMBEDDING_QUERY_PREFIX";
const DOCUMENT_PREFIX_KEY: &str = "PRINTABLE_EMBEDDING_DOCUMENT_PREFIX";
const REVISION_KEY: &str = "PRINTABLE_EMBEDDING_REVISION";
const IMAGES_KEY: &str = "PRINTABLE_EMBEDDING_IMAGES";
const RERANK_URL_KEY: &str = "PRINTABLE_RERANK_URL";
const RERANK_MODEL_KEY: &str = "PRINTABLE_RERANK_MODEL";
const RERANK_API_KEY: &str = "PRINTABLE_RERANK_API_KEY";
const RERANK_API_KEY_FILE: &str = "PRINTABLE_RERANK_API_KEY_FILE";

/// Longest vector accepted from a service.
pub(crate) const MAX_DIMENSIONS: usize = 8192;
/// Most inputs sent in one embeddings request.
pub(crate) const MAX_BATCH: usize = 32;
/// Largest embeddings answer read; 32 vectors of 8192 numbers in JSON fit.
const MAX_EMBEDDINGS_BYTES: usize = 8 * 1024 * 1024;
const MAX_RERANK_BYTES: usize = 1024 * 1024;
/// Longest model name or revision label accepted from settings, and longest
/// model name kept from an answer.
const MAX_LABEL_CHARS: usize = 256;
const MAX_PREFIX_CHARS: usize = 1024;
const MAX_API_KEY_BYTES: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The wait after a busy answer that names none.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// The longest wait a service can ask for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// Pictures are reduced to at most this many pixels on their long side.
pub(crate) const PICTURE_SIDE: u32 = 1024;
const USER_AGENT: &str = concat!("printable/", env!("CARGO_PKG_VERSION"));

/// The configured services; each is absent when its URL is unset.
pub(crate) struct Services {
    pub embeddings: Option<Embeddings>,
    pub reranker: Option<Reranker>,
}

/// The embeddings service and the settings that shape its vectors.
pub(crate) struct Embeddings {
    http: HttpClient,
    endpoint: Url,
    authorization: Option<HeaderValue>,
    model: String,
    dimensions: Option<usize>,
    revision: Option<String>,
    query_prefix: String,
    document_prefix: String,
    pictures: bool,
}

impl fmt::Debug for Embeddings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Embeddings")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("revision", &self.revision)
            .field("pictures", &self.pictures)
            .field("api_key", &self.authorization.is_some())
            .finish_non_exhaustive()
    }
}

/// The re-ranking service.
pub(crate) struct Reranker {
    http: HttpClient,
    endpoint: Url,
    authorization: Option<HeaderValue>,
    model: String,
}

impl fmt::Debug for Reranker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reranker")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.model)
            .field("api_key", &self.authorization.is_some())
            .finish_non_exhaustive()
    }
}

/// Read the service settings. The other embedding settings take effect only
/// with `PRINTABLE_EMBEDDING_URL`, and the re-ranking ones only with
/// `PRINTABLE_RERANK_URL`. Errors name the setting, never its value.
pub(crate) fn configure(get: &impl Fn(&str) -> Option<String>) -> Result<Services, SettingsError> {
    let set = |key: &str| get(key).filter(|value| !value.trim().is_empty());
    let embeddings = match set(URL_KEY) {
        None => None,
        Some(raw) => Some(Embeddings {
            http: client(URL_KEY)?,
            endpoint: endpoint(&raw, URL_KEY, "embeddings")?,
            authorization: authorization(secret(&set, API_KEY, API_KEY_FILE)?, API_KEY)?,
            model: label(set(MODEL_KEY), MODEL_KEY)?
                .ok_or(SettingsError::ProjectSearch(MODEL_KEY))?,
            dimensions: set(DIMENSIONS_KEY)
                .map(|raw| {
                    raw.trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|dimensions| (1..=MAX_DIMENSIONS).contains(dimensions))
                        .ok_or(SettingsError::ProjectSearch(DIMENSIONS_KEY))
                })
                .transpose()?,
            revision: label(set(REVISION_KEY), REVISION_KEY)?,
            query_prefix: prefix(get(QUERY_PREFIX_KEY), QUERY_PREFIX_KEY)?,
            document_prefix: prefix(get(DOCUMENT_PREFIX_KEY), DOCUMENT_PREFIX_KEY)?,
            pictures: match set(IMAGES_KEY)
                .map(|raw| raw.trim().to_ascii_lowercase())
                .as_deref()
            {
                None | Some("off") => false,
                Some("on") => true,
                Some(_) => return Err(SettingsError::ProjectSearch(IMAGES_KEY)),
            },
        }),
    };
    let reranker = match set(RERANK_URL_KEY) {
        None => None,
        Some(raw) => Some(Reranker {
            http: client(RERANK_URL_KEY)?,
            endpoint: endpoint(&raw, RERANK_URL_KEY, "rerank")?,
            authorization: authorization(
                secret(&set, RERANK_API_KEY, RERANK_API_KEY_FILE)?,
                RERANK_API_KEY,
            )?,
            model: label(set(RERANK_MODEL_KEY), RERANK_MODEL_KEY)?
                .ok_or(SettingsError::ProjectSearch(RERANK_MODEL_KEY))?,
        }),
    };
    Ok(Services {
        embeddings,
        reranker,
    })
}

/// A redirect-free, proxy-free client with connection and request deadlines.
fn client(key: &'static str) -> Result<HttpClient, SettingsError> {
    HttpClient::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| SettingsError::ProjectSearch(key))
}

/// `<base>/<path>` for an HTTPS base, or a plain-HTTP base on a private or
/// loopback address or a single-label host name, without credentials, query
/// or fragment.
fn endpoint(raw: &str, key: &'static str, path: &str) -> Result<Url, SettingsError> {
    let invalid = || SettingsError::ProjectSearch(key);
    let mut url = Url::parse(raw.trim()).map_err(|_| invalid())?;
    let private = match url.host() {
        Some(Host::Ipv4(address)) => address.is_private() || address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback() || address.is_unique_local(),
        Some(Host::Domain(domain)) => domain == "localhost" || !domain.contains('.'),
        None => false,
    };
    if url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && private))
    {
        return Err(invalid());
    }
    url.path_segments_mut()
        .map_err(|()| invalid())?
        .pop_if_empty()
        .push(path);
    Ok(url)
}

/// A trimmed one-line label of at most 256 characters.
fn label(raw: Option<String>, key: &'static str) -> Result<Option<String>, SettingsError> {
    raw.map(|raw| {
        let value = raw.trim();
        if value.chars().count() > MAX_LABEL_CHARS || value.chars().any(char::is_control) {
            Err(SettingsError::ProjectSearch(key))
        } else {
            Ok(value.to_owned())
        }
    })
    .transpose()
}

/// Prefixes are kept exactly as set: models such as E5 expect a trailing
/// space after `query:`.
fn prefix(raw: Option<String>, key: &'static str) -> Result<String, SettingsError> {
    let value = raw.unwrap_or_default();
    if value.chars().count() > MAX_PREFIX_CHARS || value.contains('\0') {
        return Err(SettingsError::ProjectSearch(key));
    }
    Ok(value)
}

/// An API key from its setting or from the file another setting names, but
/// not both. Errors name the setting, never the key or the path.
fn secret(
    set: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    file_key: &'static str,
) -> Result<Option<String>, SettingsError> {
    match (set(key), set(file_key)) {
        (Some(_), Some(_)) => Err(SettingsError::ProjectSearch(file_key)),
        (value, None) => Ok(value),
        (None, Some(path)) => {
            let mut text = Zeroizing::new(String::new());
            // One byte more than a key may be, plus a line ending, so an
            // oversized file is refused by the length check.
            std::fs::File::open(path.trim())
                .and_then(|file| {
                    file.take(MAX_API_KEY_BYTES as u64 + 3)
                        .read_to_string(&mut text)
                })
                .map_err(|_| SettingsError::ProjectSearch(file_key))?;
            Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned()))
        }
    }
}

/// The `Authorization` header for an API key, marked sensitive so it is
/// never shown in debug output.
fn authorization(
    raw: Option<String>,
    key: &'static str,
) -> Result<Option<HeaderValue>, SettingsError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = Zeroizing::new(raw);
    let secret = raw.trim();
    if secret.len() > MAX_API_KEY_BYTES || secret.chars().any(char::is_control) {
        return Err(SettingsError::ProjectSearch(key));
    }
    let header = Zeroizing::new(format!("Bearer {secret}"));
    let mut value =
        HeaderValue::from_str(&header).map_err(|_| SettingsError::ProjectSearch(key))?;
    value.set_sensitive(true);
    Ok(Some(value))
}

/// One input to embed.
#[derive(Clone, Copy)]
pub(crate) enum Input<'a> {
    Text(&'a str),
    /// PNG bytes, as [`prepare_picture`] makes them.
    Picture(&'a [u8]),
}

/// An embeddings answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Embedded {
    /// The model the service named, if it named one.
    pub model: Option<String>,
    /// One unit-length vector per input, in input order.
    pub vectors: Vec<Vec<f32>>,
}

/// Why a request gave no usable answer. Each message completes a sentence
/// that names the service, and mentions only HTTP statuses and waits, never
/// request contents or keys.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum ServiceError {
    #[error("could not be reached")]
    Unreachable,
    #[error("failed with HTTP status {0}")]
    Failed(u16),
    #[error("is busy and asked to wait {} seconds", .0.as_secs())]
    Busy(Duration),
    #[error("refused the request with HTTP status {0}")]
    Refused(u16),
    #[error("returned an answer that cannot be used")]
    Incompatible,
    #[error("returned an answer larger than {0} bytes")]
    TooLarge(usize),
}

impl ServiceError {
    /// Whether the failure concerns the service as a whole rather than the
    /// inputs sent: it is down or busy, or its address, model or key is wrong.
    pub(crate) fn is_service_wide(&self) -> bool {
        match self {
            Self::Unreachable | Self::Failed(_) | Self::Busy(_) => true,
            Self::Refused(status) => *status < 400 || matches!(status, 401 | 403 | 404 | 405),
            Self::Incompatible | Self::TooLarge(_) => false,
        }
    }

    /// Whether a refusal of pictures means the model does not accept them.
    /// `413` says only that the request was too large, which a smaller or
    /// later request can avoid.
    pub(crate) fn refuses_pictures(&self) -> bool {
        matches!(self, Self::Refused(status) if *status != 413) && !self.is_service_wide()
    }
}

impl Embeddings {
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    /// Whether pictures are sent for picture search.
    pub(crate) fn pictures(&self) -> bool {
        self.pictures
    }

    /// Identifies the settings that shape stored vectors: the service
    /// address, model name, requested dimensions, revision label and document
    /// prefix. A different service may treat the same model differently,
    /// including whether it accepts pictures.
    pub(crate) fn fingerprint(&self) -> String {
        let dimensions = self.dimensions.map_or_else(String::new, |d| d.to_string());
        let mut bytes = Vec::new();
        for part in [
            self.endpoint.as_str(),
            self.model.as_str(),
            dimensions.as_str(),
            self.revision.as_deref().unwrap_or_default(),
            self.document_prefix.as_str(),
        ] {
            bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
        crate::provenance::digest(&bytes)
    }

    /// Search text with the query prefix.
    pub(crate) fn query(&self, text: &str) -> String {
        format!("{}{text}", self.query_prefix)
    }

    /// Project text with the document prefix.
    pub(crate) fn document(&self, text: &str) -> String {
        format!("{}{text}", self.document_prefix)
    }

    /// Embed 1 to [`MAX_BATCH`] inputs in one request.
    pub(crate) async fn embed(&self, inputs: &[Input<'_>]) -> Result<Embedded, ServiceError> {
        assert!(
            (1..=MAX_BATCH).contains(&inputs.len()),
            "an embeddings request holds 1 to {MAX_BATCH} inputs"
        );
        let input: Vec<Value> = inputs
            .iter()
            .map(|input| match input {
                Input::Text(text) => Value::String((*text).to_owned()),
                Input::Picture(png) => json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{}", STANDARD.encode(png))}
                }),
            })
            .collect();
        let mut body = json!({"model": self.model, "input": input, "encoding_format": "float"});
        if let Some(dimensions) = self.dimensions {
            body["dimensions"] = json!(dimensions);
        }
        let bytes = post(
            &self.http,
            &self.endpoint,
            self.authorization.as_ref(),
            &body,
            MAX_EMBEDDINGS_BYTES,
        )
        .await?;
        parse_embeddings(&bytes, inputs.len(), self.dimensions)
    }
}

impl Reranker {
    /// Score each document's relevance to the query. Returns
    /// `(document index, relevance)` pairs in the service's order.
    pub(crate) async fn rerank(
        &self,
        query: &str,
        documents: &[String],
    ) -> Result<Vec<(usize, f64)>, ServiceError> {
        let body = json!({
            "model": self.model, "query": query, "documents": documents, "top_n": documents.len()
        });
        let bytes = post(
            &self.http,
            &self.endpoint,
            self.authorization.as_ref(),
            &body,
            MAX_RERANK_BYTES,
        )
        .await?;
        parse_rerank(&bytes, documents.len())
    }
}

/// Send one JSON request and read a successful answer of at most `limit`
/// bytes. Error bodies are never read.
async fn post(
    http: &HttpClient,
    endpoint: &Url,
    authorization: Option<&HeaderValue>,
    body: &Value,
    limit: usize,
) -> Result<Vec<u8>, ServiceError> {
    let mut request = http.post(endpoint.clone()).json(body);
    if let Some(value) = authorization {
        request = request.header(AUTHORIZATION, value.clone());
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| ServiceError::Unreachable)?;
    let status = response.status();
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return Err(ServiceError::Busy(retry_after(response.headers())));
    }
    if status.is_server_error() {
        return Err(ServiceError::Failed(status.as_u16()));
    }
    if !status.is_success() {
        return Err(ServiceError::Refused(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ServiceError::TooLarge(limit));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ServiceError::Unreachable)?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ServiceError::TooLarge(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The wait a busy answer asks for, in seconds or until a date, at most an
/// hour. A missing or unreadable value waits a minute.
fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| {
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .ok()
                .or_else(|| {
                    let at = httpdate::parse_http_date(value).ok()?;
                    Some(at.duration_since(SystemTime::now()).unwrap_or_default())
                })
        })
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(MAX_RETRY_AFTER)
}

#[derive(Deserialize)]
struct EmbeddingsAnswer {
    data: Vec<EmbeddingDatum>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

/// Check an embeddings answer: one vector per input, placed by `index` when
/// every entry has one, all of one length of at most [`MAX_DIMENSIONS`] (and
/// the requested length when one was set), finite and not zero.
fn parse_embeddings(
    bytes: &[u8],
    count: usize,
    dimensions: Option<usize>,
) -> Result<Embedded, ServiceError> {
    let answer: EmbeddingsAnswer =
        serde_json::from_slice(bytes).map_err(|_| ServiceError::Incompatible)?;
    let indexed = answer
        .data
        .iter()
        .filter(|datum| datum.index.is_some())
        .count();
    if answer.data.len() != count || !(indexed == 0 || indexed == count) {
        return Err(ServiceError::Incompatible);
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; count];
    for (position, datum) in answer.data.into_iter().enumerate() {
        match slots.get_mut(datum.index.unwrap_or(position)) {
            Some(slot @ None) => *slot = Some(datum.embedding),
            _ => return Err(ServiceError::Incompatible),
        }
    }
    let length = slots.first().and_then(Option::as_ref).map_or(0, Vec::len);
    if length == 0 || length > MAX_DIMENSIONS || dimensions.is_some_and(|wanted| wanted != length) {
        return Err(ServiceError::Incompatible);
    }
    let vectors = slots
        .into_iter()
        .map(|vector| {
            vector
                .filter(|vector| vector.len() == length)
                .and_then(normalized)
                .ok_or(ServiceError::Incompatible)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let model = answer
        .model
        .map(|model| {
            model
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_LABEL_CHARS)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|model| !model.is_empty());
    Ok(Embedded { model, vectors })
}

#[derive(Deserialize)]
struct RerankAnswer {
    results: Vec<RerankDatum>,
}

#[derive(Deserialize)]
struct RerankDatum {
    index: usize,
    relevance_score: f64,
}

/// Check a re-ranking answer: at most one finite score per document.
fn parse_rerank(bytes: &[u8], count: usize) -> Result<Vec<(usize, f64)>, ServiceError> {
    let answer: RerankAnswer =
        serde_json::from_slice(bytes).map_err(|_| ServiceError::Incompatible)?;
    let mut seen = vec![false; count];
    for datum in &answer.results {
        match seen.get_mut(datum.index) {
            Some(slot @ false) if datum.relevance_score.is_finite() => *slot = true,
            _ => return Err(ServiceError::Incompatible),
        }
    }
    Ok(answer
        .results
        .into_iter()
        .map(|datum| (datum.index, datum.relevance_score))
        .collect())
}

/// The vector scaled to unit length; `None` when it has a value that is not
/// finite or has no length.
fn normalized(mut vector: Vec<f32>) -> Option<Vec<f32>> {
    if vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_normal() {
        return None;
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Some(vector)
}

/// A vector as little-endian 32-bit floats.
pub(crate) fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// A vector stored by [`to_blob`].
pub(crate) fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect()
}

/// Cosine similarity of two unit vectors.
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum()
}

/// Cosine similarity of a unit vector and one stored by [`to_blob`]; `None`
/// when their lengths differ.
pub(crate) fn similarity(vector: &[f32], blob: &[u8]) -> Option<f64> {
    (blob.len() == vector.len() * 4).then(|| {
        vector
            .iter()
            .zip(blob.chunks_exact(4))
            .map(|(a, bytes)| {
                f64::from(*a)
                    * f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            })
            .sum()
    })
}

/// Decode a PNG, JPEG or WebP picture under the cover limits and encode it as
/// a PNG of at most [`PICTURE_SIDE`] pixels on its long side.
pub(crate) fn prepare_picture(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let (_, image) = super::cover::decode(bytes)?;
    let image = if image.width().max(image.height()) > PICTURE_SIDE {
        image.resize(PICTURE_SIDE, PICTURE_SIDE, FilterType::Triangle)
    } else {
        image
    };
    let image = if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.into_rgb8())
    };
    let mut png = Vec::new();
    image
        .write_to(Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| {
            ToolError::Validation(format!("the picture cannot be encoded: {error}"))
        })?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base64::Engine as _;

    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn service(server: &MockServer, extra: &[(&str, &str)]) -> Embeddings {
        let base = format!("{}/v1", server.uri());
        let mut pairs = vec![(URL_KEY, base.as_str()), (MODEL_KEY, "fake-embed")];
        pairs.extend_from_slice(extra);
        configure(&lookup(&pairs)).unwrap().embeddings.unwrap()
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([10, 200, 30]))
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn api_keys_can_be_read_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "sk-file-secret\n").unwrap();
        let path = path.to_str().unwrap().to_owned();
        let services = configure(&lookup(&[
            (URL_KEY, "https://embed.example.com/v1"),
            (MODEL_KEY, "nomic-embed-text"),
            (API_KEY_FILE, &path),
        ]))
        .unwrap();
        let header = services.embeddings.unwrap().authorization.unwrap();
        assert_eq!(header.to_str().unwrap(), "Bearer sk-file-secret");
        for (pairs, refused) in [
            (
                vec![(API_KEY, "sk-inline"), (API_KEY_FILE, path.as_str())],
                API_KEY_FILE,
            ),
            (
                vec![(API_KEY_FILE, "/nonexistent/printable-key")],
                API_KEY_FILE,
            ),
        ] {
            let mut pairs = pairs;
            pairs.extend([
                (URL_KEY, "https://embed.example.com/v1"),
                (MODEL_KEY, "nomic-embed-text"),
            ]);
            let Err(error) = configure(&lookup(&pairs)) else {
                panic!("{refused} is refused");
            };
            assert_eq!(
                error.to_string(),
                SettingsError::ProjectSearch(refused).to_string()
            );
            assert!(!error.to_string().contains("printable-key"), "{error}");
        }
        std::fs::write(dir.path().join("long"), "k".repeat(MAX_API_KEY_BYTES + 1)).unwrap();
        let long = dir.path().join("long").to_str().unwrap().to_owned();
        assert!(
            configure(&lookup(&[
                (URL_KEY, "https://embed.example.com/v1"),
                (MODEL_KEY, "nomic-embed-text"),
                (API_KEY_FILE, &long),
            ]))
            .is_err()
        );
    }

    #[test]
    fn settings_build_endpoints_and_reject_unsafe_values_without_echoing_them() {
        let services = configure(&lookup(&[
            (URL_KEY, "https://embed.example.com/v1"),
            (MODEL_KEY, " nomic-embed-vision "),
            (API_KEY, "sk-settings-secret"),
            (DIMENSIONS_KEY, "768"),
            (QUERY_PREFIX_KEY, "search_query: "),
            (DOCUMENT_PREFIX_KEY, "search_document: "),
            (REVISION_KEY, "2026-11"),
            (IMAGES_KEY, "On"),
            (RERANK_URL_KEY, "http://reranker:8080"),
            (RERANK_MODEL_KEY, "bge-reranker"),
            (RERANK_API_KEY, "sk-rerank-secret"),
        ]))
        .unwrap();
        let embeddings = services.embeddings.unwrap();
        assert_eq!(
            embeddings.endpoint.as_str(),
            "https://embed.example.com/v1/embeddings"
        );
        assert_eq!(embeddings.model(), "nomic-embed-vision");
        assert_eq!(embeddings.query("hook"), "search_query: hook");
        assert_eq!(embeddings.document("hook"), "search_document: hook");
        assert_eq!(embeddings.revision(), Some("2026-11"));
        assert!(embeddings.pictures());
        let reranker = services.reranker.unwrap();
        assert_eq!(reranker.endpoint.as_str(), "http://reranker:8080/rerank");
        let shown = format!("{embeddings:?} {reranker:?}");
        assert!(!shown.contains("secret"), "{shown}");

        for (base, expected) in [
            (
                "https://embed.example.com/v1/",
                "https://embed.example.com/v1/embeddings",
            ),
            (
                "http://192.168.1.20:8080",
                "http://192.168.1.20:8080/embeddings",
            ),
            (
                "http://localhost:11434/v1",
                "http://localhost:11434/v1/embeddings",
            ),
            ("http://[::1]:8080", "http://[::1]:8080/embeddings"),
            ("http://embedder/v1", "http://embedder/v1/embeddings"),
        ] {
            let services = configure(&lookup(&[(URL_KEY, base), (MODEL_KEY, "m")])).unwrap();
            assert_eq!(services.embeddings.unwrap().endpoint.as_str(), expected);
        }
        let none = configure(&lookup(&[(MODEL_KEY, "m"), (IMAGES_KEY, "off")])).unwrap();
        assert!(none.embeddings.is_none() && none.reranker.is_none());

        let base = [(URL_KEY, "https://embed.example.com/v1"), (MODEL_KEY, "m")];
        for (setting, value) in [
            (URL_KEY, "http://embed.example.com/v1"),
            (URL_KEY, "https://user:hunter2@embed.example.com/v1"),
            (URL_KEY, "https://embed.example.com/v1?key=hunter2"),
            (URL_KEY, "https://embed.example.com/v1#hunter2"),
            (URL_KEY, "ftp://embed.example.com"),
            (URL_KEY, "hunter2"),
            (MODEL_KEY, " "),
            (MODEL_KEY, "model\nhunter2"),
            (API_KEY, "sk-hunter2\r\nX-Other: 1"),
            (DIMENSIONS_KEY, "0"),
            (DIMENSIONS_KEY, "8193"),
            (DIMENSIONS_KEY, "hunter2"),
            (IMAGES_KEY, "hunter2"),
            (REVISION_KEY, "rev\u{7}hunter2"),
            (RERANK_URL_KEY, "https://rerank.example.com"),
        ] {
            let mut pairs = base.to_vec();
            pairs.retain(|(key, _)| *key != setting);
            pairs.push((setting, value));
            let expected = match (setting, value) {
                (MODEL_KEY, " ") => MODEL_KEY,
                (RERANK_URL_KEY, _) => RERANK_MODEL_KEY,
                _ => setting,
            };
            let error = configure(&lookup(&pairs)).map(|_| ()).unwrap_err();
            assert_eq!(error, SettingsError::ProjectSearch(expected), "{setting}");
            assert!(!error.to_string().contains("hunter2"), "{error}");
        }
        let error = crate::config::Settings::from_lookup(|key| match key {
            "PRINTABLE_MCP_BEARER" => Some("0".repeat(64)),
            URL_KEY => Some("https://embed.example.com/v1".into()),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(error, SettingsError::ProjectSearch(MODEL_KEY));
    }

    #[tokio::test]
    async fn requests_use_the_openai_shape_and_answers_are_placed_by_index() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_json(json!({
                "model": "fake-embed", "input": ["passage: one", "passage: two"],
                "encoding_format": "float", "dimensions": 3
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "model": "fake-embed-v2",
                "data": [{"index": 1, "embedding": [0.0, 3.0, 4.0]}, {"index": 0, "embedding": [2.0, 0.0, 0.0]}]
            })))
            .mount(&server)
            .await;
        let service = service(
            &server,
            &[
                (API_KEY, "sk-test"),
                (DIMENSIONS_KEY, "3"),
                (DOCUMENT_PREFIX_KEY, "passage: "),
            ],
        );
        let (one, two) = (service.document("one"), service.document("two"));
        let embedded = service
            .embed(&[Input::Text(&one), Input::Text(&two)])
            .await
            .unwrap();
        assert_eq!(embedded.model.as_deref(), Some("fake-embed-v2"));
        let expected = [[1.0_f32, 0.0, 0.0], [0.0, 0.6, 0.8]];
        for (vector, expected) in embedded.vectors.iter().zip(expected) {
            for (value, expected) in vector.iter().zip(expected) {
                assert!((value - expected).abs() < 1e-6, "{vector:?}");
            }
        }
        assert!((dot(&embedded.vectors[1], &embedded.vectors[1]) - 1.0).abs() < 1e-6);
        let blob = to_blob(&embedded.vectors[1]);
        assert_eq!(from_blob(&blob), embedded.vectors[1]);
        assert_eq!(similarity(&embedded.vectors[0], &blob), Some(0.0));
        assert_eq!(similarity(&embedded.vectors[0], &blob[..8]), None);
    }

    #[test]
    fn answers_that_cannot_be_compared_are_refused() {
        let answer = |data: Value| serde_json::to_vec(&json!({"data": data})).unwrap();
        for (data, count, dimensions) in [
            (json!([{"embedding": [1.0]}]), 2, None),
            (
                json!([{"embedding": [1.0], "index": 0}, {"embedding": [1.0]}]),
                2,
                None,
            ),
            (
                json!([{"embedding": [1.0], "index": 0}, {"embedding": [1.0], "index": 0}]),
                2,
                None,
            ),
            (json!([{"embedding": [1.0], "index": 3}]), 1, None),
            (json!([{"embedding": [0.0, 0.0]}]), 1, None),
            (json!([{"embedding": []}]), 1, None),
            (
                json!([{"embedding": [1.0, 2.0]}, {"embedding": [1.0]}]),
                2,
                None,
            ),
            (json!([{"embedding": [1.0, 2.0]}]), 1, Some(3)),
            (
                json!([{"embedding": vec![0.5; MAX_DIMENSIONS + 1]}]),
                1,
                None,
            ),
            (json!([{"embedding": "AAAAAA=="}]), 1, None),
            (json!([{"embedding": [1e39]}]), 1, None),
        ] {
            assert_eq!(
                parse_embeddings(&answer(data.clone()), count, dimensions),
                Err(ServiceError::Incompatible),
                "{data}"
            );
        }
        let positional = parse_embeddings(
            &answer(json!([{"embedding": [2.0, 0.0]}, {"embedding": [0.0, 5.0]}])),
            2,
            None,
        )
        .unwrap();
        assert_eq!(positional.vectors, [[1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(positional.model, None);
        for bad in [
            json!({"results": [{"index": 2, "relevance_score": 0.5}]}),
            json!({"results": [{"index": 0, "relevance_score": 0.5}, {"index": 0, "relevance_score": 0.4}]}),
            json!({"results": [{"index": 0, "score": 0.5}]}),
        ] {
            assert_eq!(
                parse_rerank(&serde_json::to_vec(&bad).unwrap(), 2),
                Err(ServiceError::Incompatible),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_retry_date_waits_until_that_time() {
        let mut headers = HeaderMap::new();
        let later = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(20 * 60));
        headers.insert(RETRY_AFTER, HeaderValue::from_str(&later).unwrap());
        let wait = retry_after(&headers);
        assert!(
            wait > Duration::from_secs(19 * 60) && wait <= Duration::from_secs(20 * 60),
            "{wait:?}"
        );
    }

    #[tokio::test]
    async fn statuses_map_to_waits_refusals_and_failures() {
        let server = MockServer::start().await;
        let service = service(&server, &[]);
        for (response, expected) in [
            (
                ResponseTemplate::new(429).insert_header("retry-after", "7"),
                ServiceError::Busy(Duration::from_secs(7)),
            ),
            (
                ResponseTemplate::new(429).insert_header("retry-after", "999999"),
                ServiceError::Busy(MAX_RETRY_AFTER),
            ),
            (
                ResponseTemplate::new(503),
                ServiceError::Busy(DEFAULT_RETRY_AFTER),
            ),
            (
                ResponseTemplate::new(503).insert_header("retry-after", "soon"),
                ServiceError::Busy(DEFAULT_RETRY_AFTER),
            ),
            (
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT"),
                ServiceError::Busy(Duration::ZERO),
            ),
            (ResponseTemplate::new(401), ServiceError::Refused(401)),
            (ResponseTemplate::new(400), ServiceError::Refused(400)),
            (ResponseTemplate::new(302), ServiceError::Refused(302)),
            (ResponseTemplate::new(500), ServiceError::Failed(500)),
            (
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_EMBEDDINGS_BYTES + 1)),
                ServiceError::TooLarge(MAX_EMBEDDINGS_BYTES),
            ),
            (
                ResponseTemplate::new(200).set_body_string("not json"),
                ServiceError::Incompatible,
            ),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            let error = service.embed(&[Input::Text("hook")]).await.unwrap_err();
            assert_eq!(error, expected);
        }
        assert!(ServiceError::Refused(401).is_service_wide());
        assert!(ServiceError::Refused(302).is_service_wide());
        assert!(!ServiceError::Refused(400).is_service_wide());
        assert!(!ServiceError::Incompatible.is_service_wide());
        assert!(ServiceError::Refused(422).refuses_pictures());
        assert!(!ServiceError::Refused(413).refuses_pictures());
        assert!(!ServiceError::Refused(401).refuses_pictures());
        assert!(!ServiceError::Failed(500).refuses_pictures());
        // Nothing listens on a port whose listener has been dropped.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let base = format!("http://127.0.0.1:{port}/v1");
        let closed = configure(&lookup(&[(URL_KEY, base.as_str()), (MODEL_KEY, "m")]))
            .unwrap()
            .embeddings
            .unwrap();
        assert_eq!(
            closed.embed(&[Input::Text("hook")]).await.unwrap_err(),
            ServiceError::Unreachable
        );
    }

    #[tokio::test]
    async fn pictures_are_reduced_and_sent_as_image_content() {
        let large = png(2048, 1024);
        let prepared = prepare_picture(&large).unwrap();
        let decoded = image::load_from_memory(&prepared).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1024, 512));
        let small = prepare_picture(&png(40, 30)).unwrap();
        let decoded = image::load_from_memory(&small).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 30));
        assert!(prepare_picture(b"not a picture").is_err());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "fake-embed", "data": [{"index": 0, "embedding": [1.0, 1.0]}]
            })))
            .mount(&server)
            .await;
        let service = service(&server, &[(IMAGES_KEY, "on")]);
        service.embed(&[Input::Picture(&small)]).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let part = &body["input"][0];
        assert_eq!(part["type"], "image_url");
        let url = part["image_url"]["url"].as_str().unwrap();
        let encoded = url.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(STANDARD.decode(encoded).unwrap(), small);
    }

    #[tokio::test]
    async fn rerank_requests_name_the_model_query_and_documents() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .and(header("authorization", "Bearer sk-rerank"))
            .and(body_json(json!({
                "model": "fake-rerank", "query": "hook", "documents": ["a", "b"], "top_n": 2
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{"index": 1, "relevance_score": 0.9, "document": {"text": "b"}},
                            {"index": 0, "relevance_score": 0.1}]
            })))
            .mount(&server)
            .await;
        let services = configure(&lookup(&[
            (RERANK_URL_KEY, server.uri().as_str()),
            (RERANK_MODEL_KEY, "fake-rerank"),
            (RERANK_API_KEY, "sk-rerank"),
        ]))
        .unwrap();
        let scores = services
            .reranker
            .unwrap()
            .rerank("hook", &["a".into(), "b".into()])
            .await
            .unwrap();
        assert_eq!(scores, [(1, 0.9), (0, 0.1)]);
    }
}
