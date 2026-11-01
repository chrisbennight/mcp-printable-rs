//! MakerWorld through Bambu Lab's API host. Search and model details need no
//! account; downloading a print profile's 3MF needs a Bambu Lab access token.

use serde::Deserialize;
use url::Url;

use crate::{
    FileKind, LibraryError, License, ModelDetail, ModelSummary, RemoteFile, SearchPage,
    SearchQuery, SearchSort, Site, client::Client, reference, text, wire,
};

const SITE: Site = Site::MakerWorld;
const MODEL_PAGE: &str = "https://makerworld.com/en/models/";

/// MakerWorld serves files from its own file hosts or from path-style Amazon
/// S3 links such as `s3.us-west-2.amazonaws.com/<bucket>/…`. Bucket-named,
/// access-point and object-lambda S3 hosts are refused.
pub(crate) fn file_host_allowed(host: &str) -> bool {
    matches!(
        host,
        "makerworld.bblmw.com" | "public-cdn.bblmw.com" | "s3.amazonaws.com"
    ) || host
        .strip_prefix("s3.")
        .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
        .is_some_and(|region| {
            !region.is_empty()
                && region
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(default, deserialize_with = "wire::count")]
    total: Option<u64>,
    #[serde(default)]
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hit {
    id: wire::Id,
    slug: Option<String>,
    title: String,
    cover: Option<String>,
    license: Option<String>,
    #[serde(default, deserialize_with = "wire::count")]
    like_count: Option<u64>,
    #[serde(default, deserialize_with = "wire::count")]
    download_count: Option<u64>,
    design_creator: Option<Creator>,
}

#[derive(Deserialize)]
struct Creator {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Design {
    id: wire::Id,
    model_id: Option<String>,
    slug: Option<String>,
    title: String,
    summary: Option<String>,
    license: Option<String>,
    cover_url: Option<String>,
    design_creator: Option<Creator>,
    #[serde(default)]
    instances: Vec<Instance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instance {
    profile_id: wire::Id,
    title: Option<String>,
    // The upstream field name is spelled this way.
    extention: Option<Extension>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Extension {
    model_info: Option<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    compatibility: Option<Compatibility>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Compatibility {
    dev_product_name: Option<String>,
}

#[derive(Deserialize)]
struct ProfileDownload {
    url: String,
}

fn api(client: &Client, segments: &[&str]) -> Url {
    let mut url = client.endpoints.api(SITE).clone();
    url.path_segments_mut()
        .expect("fixed API origin has a path")
        .pop_if_empty()
        .extend(segments);
    url
}

pub(crate) async fn search(
    client: &Client,
    query: &SearchQuery,
) -> Result<SearchPage, LibraryError> {
    let order = match query.sort {
        SearchSort::Relevance => "score",
        SearchSort::Popular => "hotScore",
        SearchSort::Newest => {
            return Err(LibraryError::InvalidRequest(
                "MakerWorld search supports relevance and popular ordering".into(),
            ));
        }
    };
    let offset = (query.page - 1) * query.limit;
    let mut url = api(client, &["v1", "search-service", "select", "design2"]);
    url.query_pairs_mut()
        .append_pair("keyword", &query.text)
        .append_pair("limit", &query.limit.to_string())
        .append_pair("offset", &offset.to_string())
        .append_pair("orderBy", order);
    let result: SearchResult = client.json(SITE, client.api_get(url), false).await?;
    let models: Vec<_> = result
        .hits
        .into_iter()
        .map(|hit| ModelSummary {
            site: SITE,
            url: page_url(&hit.id.0, hit.slug.as_deref()),
            title: text::bounded(&hit.title, text::SHORT_TEXT),
            designer: hit
                .design_creator
                .and_then(|creator| text::optional(creator.name.as_deref(), text::SHORT_TEXT)),
            license: text::optional(hit.license.as_deref(), text::SHORT_TEXT),
            image_url: text::web_url(hit.cover.as_deref()),
            likes: hit.like_count,
            downloads: hit.download_count,
            paid: None,
            model_id: hit.id.0,
        })
        .collect();
    let seen = u64::from(offset) + models.len() as u64;
    let more = result
        .total
        .map_or(models.len() as u32 == query.limit, |total| seen < total);
    Ok(SearchPage {
        site: SITE,
        query: query.text.clone(),
        page: query.page,
        limit: query.limit,
        total: result.total,
        models,
        next_page: more.then_some(query.page + 1),
    })
}

pub(crate) async fn model(client: &Client, id: &str) -> Result<ModelDetail, LibraryError> {
    let url = api(client, &["v1", "design-service", "design", id]);
    let design: Design = client.json(SITE, client.api_get(url), false).await?;
    let files = design
        .instances
        .iter()
        .map(|instance| {
            let title = text::optional(instance.title.as_deref(), text::SHORT_TEXT)
                .unwrap_or_else(|| format!("profile-{}", instance.profile_id.0));
            RemoteFile {
                file_id: instance.profile_id.0.clone(),
                name: format!("{title}.3mf"),
                kind: FileKind::PrintProfile,
                size_bytes: None,
                note: instance
                    .extention
                    .as_ref()
                    .and_then(|extension| extension.model_info.as_ref())
                    .and_then(|info| info.compatibility.as_ref())
                    .and_then(|compatibility| {
                        text::optional(compatibility.dev_product_name.as_deref(), text::SHORT_TEXT)
                    })
                    .map(|printer| format!("print profile for {printer}")),
                location: None,
            }
        })
        .collect();
    Ok(ModelDetail {
        site: SITE,
        url: page_url(&design.id.0, design.slug.as_deref()),
        title: text::bounded(&design.title, text::SHORT_TEXT),
        designer: design
            .design_creator
            .and_then(|creator| text::optional(creator.name.as_deref(), text::SHORT_TEXT)),
        license: text::optional(design.license.as_deref(), text::SHORT_TEXT).map(|name| License {
            name,
            remixing_allowed: None,
        }),
        summary: design
            .summary
            .as_deref()
            .map(|summary| text::from_html(summary, text::SUMMARY_TEXT))
            .filter(|summary| !summary.is_empty()),
        image_url: text::web_url(design.cover_url.as_deref()),
        paid: None,
        files,
        download_key: design
            .model_id
            .filter(|key| !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_alphanumeric())),
        model_id: design.id.0,
    })
}

pub(crate) async fn file_url(
    client: &Client,
    model: &ModelDetail,
    file: &RemoteFile,
) -> Result<Url, LibraryError> {
    let token = client.required_token(SITE)?;
    let key = model
        .download_key
        .as_deref()
        .ok_or(LibraryError::IncompatibleResponse { site: SITE })?;
    if !reference::numeric(&file.file_id) {
        return Err(LibraryError::IncompatibleResponse { site: SITE });
    }
    let mut url = api(
        client,
        &["v1", "iot-service", "api", "user", "profile", &file.file_id],
    );
    url.query_pairs_mut().append_pair("model_id", key);
    let request = client.api_get(url).bearer_auth(token.expose());
    let download: ProfileDownload = client.json(SITE, request, true).await?;
    Url::parse(&download.url).map_err(|_| LibraryError::IncompatibleResponse { site: SITE })
}

fn page_url(id: &str, slug: Option<&str>) -> String {
    match text::slug(slug) {
        Some(slug) => format!("{MODEL_PAGE}{id}-{slug}"),
        None => format!("{MODEL_PAGE}{id}"),
    }
}
