//! Thingiverse through its official REST API, which requires an app token.

use serde::Deserialize;
use url::Url;

use crate::{
    LibraryError, License, ModelDetail, ModelSummary, RemoteFile, SearchPage, SearchQuery,
    SearchSort, Site, client::Client, text, wire,
};

const SITE: Site = Site::Thingiverse;
/// Hosts that serve Thingiverse files besides the API origin.
pub(crate) const FILE_HOSTS: &[&str] = &["cdn.thingiverse.com", "www.thingiverse.com"];
/// The largest page the search endpoint returns.
const MAXIMUM_PAGE_SIZE: u32 = 30;
const THING_PAGE: &str = "https://www.thingiverse.com/thing:";

#[derive(Deserialize)]
#[serde(untagged)]
enum SearchResult {
    Hits {
        #[serde(default, deserialize_with = "wire::count")]
        total: Option<u64>,
        #[serde(default)]
        hits: Vec<Thing>,
    },
    List(Vec<Thing>),
}

#[derive(Deserialize)]
struct Thing {
    id: wire::Id,
    name: String,
    public_url: Option<String>,
    thumbnail: Option<String>,
    creator: Option<Creator>,
    #[serde(default, deserialize_with = "wire::count")]
    like_count: Option<u64>,
    #[serde(default, deserialize_with = "wire::count")]
    download_count: Option<u64>,
    license: Option<String>,
    allows_derivatives: Option<bool>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Creator {
    name: Option<String>,
}

#[derive(Deserialize)]
struct ThingFile {
    id: wire::Id,
    name: String,
    #[serde(default, deserialize_with = "wire::count")]
    size: Option<u64>,
    download_url: Option<String>,
    direct_url: Option<String>,
}

fn api(client: &Client, segments: &[&str]) -> Url {
    let mut url = client.endpoints.api(SITE).clone();
    url.path_segments_mut()
        .expect("fixed API origin has a path")
        .pop_if_empty()
        .extend(segments);
    url
}

async fn read<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: Url,
) -> Result<T, LibraryError> {
    let token = client.required_token(SITE)?;
    let request = client.api_get(url).bearer_auth(token.expose());
    client.json(SITE, request, true).await
}

pub(crate) async fn search(
    client: &Client,
    query: &SearchQuery,
) -> Result<SearchPage, LibraryError> {
    let limit = query.limit.min(MAXIMUM_PAGE_SIZE);
    let mut url = api(client, &["search", &query.text]);
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("type", "things")
            .append_pair("per_page", &limit.to_string())
            .append_pair("page", &query.page.to_string());
        match query.sort {
            SearchSort::Relevance => {}
            SearchSort::Popular => {
                pairs.append_pair("sort", "popular");
            }
            SearchSort::Newest => {
                pairs.append_pair("sort", "newest");
            }
        }
    }
    let (total, things) = match read(client, url).await? {
        SearchResult::Hits { total, hits } => (total, hits),
        SearchResult::List(things) => (None, things),
    };
    let models: Vec<_> = things.into_iter().map(summary).collect();
    let seen = u64::from((query.page - 1) * limit) + models.len() as u64;
    let more = total.map_or(models.len() as u32 == limit, |total| seen < total);
    Ok(SearchPage {
        site: SITE,
        query: query.text.clone(),
        page: query.page,
        limit,
        total,
        models,
        next_page: more.then_some(query.page + 1),
    })
}

fn summary(thing: Thing) -> ModelSummary {
    ModelSummary {
        site: SITE,
        url: page_url(&thing),
        title: text::bounded(&thing.name, text::SHORT_TEXT),
        designer: thing
            .creator
            .and_then(|creator| text::optional(creator.name.as_deref(), text::SHORT_TEXT)),
        license: text::optional(thing.license.as_deref(), text::SHORT_TEXT),
        image_url: text::web_url(thing.thumbnail.as_deref()),
        likes: thing.like_count,
        downloads: thing.download_count,
        paid: None,
        model_id: thing.id.0,
    }
}

pub(crate) async fn model(client: &Client, id: &str) -> Result<ModelDetail, LibraryError> {
    let thing: Thing = read(client, api(client, &["things", id])).await?;
    let listed: Vec<ThingFile> = read(client, api(client, &["things", id, "files"])).await?;
    let files = listed
        .into_iter()
        .map(|file| RemoteFile {
            kind: wire::kind_for_name(&file.name),
            name: text::file_name(&file.name),
            size_bytes: file.size,
            note: None,
            location: file
                .direct_url
                .filter(|url| !url.is_empty())
                .or(file.download_url),
            file_id: file.id.0,
        })
        .collect();
    Ok(ModelDetail {
        site: SITE,
        url: page_url(&thing),
        title: text::bounded(&thing.name, text::SHORT_TEXT),
        designer: thing
            .creator
            .and_then(|creator| text::optional(creator.name.as_deref(), text::SHORT_TEXT)),
        license: text::optional(thing.license.as_deref(), text::SHORT_TEXT).map(|name| License {
            name,
            remixing_allowed: thing.allows_derivatives,
        }),
        summary: thing
            .description
            .as_deref()
            .map(|description| text::from_html(description, text::SUMMARY_TEXT))
            .filter(|description| !description.is_empty()),
        image_url: text::web_url(thing.thumbnail.as_deref()),
        paid: None,
        files,
        download_key: None,
        model_id: thing.id.0,
    })
}

pub(crate) fn file_url(file: &RemoteFile) -> Result<Url, LibraryError> {
    let location = file.location.as_deref().ok_or(LibraryError::Forbidden {
        site: SITE,
        reason: "Thingiverse lists no download location for this file".into(),
    })?;
    Url::parse(location).map_err(|_| LibraryError::IncompatibleResponse { site: SITE })
}

fn page_url(thing: &Thing) -> String {
    text::web_url(thing.public_url.as_deref())
        .filter(|url| url.starts_with(THING_PAGE))
        .unwrap_or_else(|| format!("{THING_PAGE}{}", thing.id.0))
}
