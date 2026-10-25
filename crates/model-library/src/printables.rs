//! Printables through its public GraphQL API. Search, model details and
//! download links for free models need no account.

use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    FileKind, LibraryError, License, ModelDetail, ModelSummary, RemoteFile, SearchPage,
    SearchQuery, SearchSort, Site, client::Client, text, wire,
};

const SITE: Site = Site::Printables;
/// Hosts that serve Printables model files.
pub(crate) const FILE_HOSTS: &[&str] = &["files.printables.com"];
const MEDIA_ORIGIN: &str = "https://media.printables.com/";
const MODEL_PAGE: &str = "https://www.printables.com/model/";

const SEARCH: &str = "query SearchModels($query: String!, $limit: Int, $offset: Int, $ordering: SearchChoicesEnum) { result: searchPrints2(query: $query, limit: $limit, offset: $offset, ordering: $ordering, printType: print) { totalCount items { id name slug likesCount downloadCount user { handle publicUsername } image { filePath } license { name } } } }";
const MODEL: &str = "query ModelFiles($id: ID!) { print(id: $id) { id name slug summary user { handle publicUsername } image { filePath } license { name abbreviation disallowRemixing } stls { id name fileSize folder note } gcodes { id name fileSize folder note } slas { id name fileSize folder note } otherFiles { id name fileSize folder note } } }";
const DOWNLOAD: &str = "mutation GetDownloadLink($printId: ID!, $fileType: DownloadFileTypeEnum!, $ids: [ID!]!, $source: DownloadSourceEnum!) { getDownloadLink(printId: $printId, files: [{fileType: $fileType, ids: $ids}], source: $source) { ok errors { field messages } output { link } } }";
const NO_LINK: &str =
    "Printables issued no download link; the model may be paid, Club-only or require signing in";

#[derive(Deserialize)]
struct Response<T> {
    data: Option<T>,
    #[serde(default, deserialize_with = "wire::list")]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: Option<String>,
}

/// A GraphQL answer whose requested field may be null, with the first error
/// message the site gave for it.
struct Answer<T> {
    data: T,
    error: Option<String>,
}

impl<T> Answer<T> {
    /// A null field with an error message is a refusal; without one, `absent`.
    fn refusal(&self, absent: LibraryError) -> LibraryError {
        match &self.error {
            Some(reason) => LibraryError::Forbidden {
                site: SITE,
                reason: reason.clone(),
            },
            None => absent,
        }
    }
}

#[derive(Deserialize)]
struct SearchData {
    result: Option<SearchResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    #[serde(default, deserialize_with = "wire::count")]
    total_count: Option<u64>,
    #[serde(default, deserialize_with = "wire::list")]
    items: Vec<Print>,
}

#[derive(Deserialize)]
struct ModelData {
    print: Option<Print>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Print {
    id: wire::Id,
    name: String,
    slug: Option<String>,
    summary: Option<String>,
    #[serde(default, deserialize_with = "wire::count")]
    likes_count: Option<u64>,
    #[serde(default, deserialize_with = "wire::count")]
    download_count: Option<u64>,
    user: Option<User>,
    image: Option<Image>,
    license: Option<PrintLicense>,
    #[serde(default, deserialize_with = "wire::list")]
    stls: Vec<PrintFile>,
    #[serde(default, deserialize_with = "wire::list")]
    gcodes: Vec<PrintFile>,
    #[serde(default, deserialize_with = "wire::list")]
    slas: Vec<PrintFile>,
    #[serde(default, deserialize_with = "wire::list")]
    other_files: Vec<PrintFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    handle: Option<String>,
    public_username: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Image {
    file_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrintLicense {
    name: Option<String>,
    abbreviation: Option<String>,
    disallow_remixing: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrintFile {
    id: wire::Id,
    name: String,
    #[serde(default, deserialize_with = "wire::count")]
    file_size: Option<u64>,
    folder: Option<String>,
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkData {
    get_download_link: Option<Link>,
}

#[derive(Deserialize)]
struct Link {
    ok: bool,
    #[serde(default, deserialize_with = "wire::list")]
    errors: Vec<LinkError>,
    output: Option<LinkOutput>,
}

#[derive(Deserialize)]
struct LinkError {
    #[serde(default, deserialize_with = "wire::list")]
    messages: Vec<String>,
}

#[derive(Deserialize)]
struct LinkOutput {
    link: Option<String>,
}

/// Send one GraphQL operation. A response without `data` means the site
/// refused the request itself; its first error message says why.
async fn query<T: serde::de::DeserializeOwned>(
    client: &Client,
    operation: &str,
    document: &str,
    variables: serde_json::Value,
) -> Result<Answer<T>, LibraryError> {
    let body = json!({"operationName": operation, "query": document, "variables": variables});
    let request = client.api_post(client.endpoints.api(SITE).clone(), &body);
    let response: Response<T> = client.json(SITE, request, false).await?;
    let error = response
        .errors
        .iter()
        .filter_map(|error| error.message.as_deref())
        .map(|message| text::bounded(message, text::REASON_TEXT))
        .find(|message| !message.is_empty());
    let Some(data) = response.data else {
        return Err(match error {
            Some(reason) => LibraryError::RequestRejected { site: SITE, reason },
            None => LibraryError::IncompatibleResponse { site: SITE },
        });
    };
    Ok(Answer { data, error })
}

pub(crate) async fn search(
    client: &Client,
    query_text: &SearchQuery,
) -> Result<SearchPage, LibraryError> {
    let ordering = match query_text.sort {
        SearchSort::Relevance => "best_match",
        SearchSort::Popular => "popular",
        SearchSort::Newest => "latest",
    };
    let offset = (query_text.page - 1) * query_text.limit;
    let answer: Answer<SearchData> = query(
        client,
        "SearchModels",
        SEARCH,
        json!({"query": query_text.text, "limit": query_text.limit, "offset": offset, "ordering": ordering}),
    )
    .await?;
    let refusal = answer.refusal(LibraryError::IncompatibleResponse { site: SITE });
    let result = answer.data.result.ok_or(refusal)?;
    let models: Vec<_> = result.items.into_iter().map(summary).collect();
    let seen = u64::from(offset) + models.len() as u64;
    let more = result
        .total_count
        .map_or(models.len() as u32 == query_text.limit, |total| {
            seen < total
        });
    Ok(SearchPage {
        site: SITE,
        query: query_text.text.clone(),
        page: query_text.page,
        limit: query_text.limit,
        total: result.total_count,
        models,
        next_page: more.then_some(query_text.page + 1),
    })
}

fn summary(print: Print) -> ModelSummary {
    ModelSummary {
        site: SITE,
        url: page_url(&print),
        title: text::bounded(&print.name, text::SHORT_TEXT),
        designer: designer(print.user.as_ref()),
        license: print
            .license
            .as_ref()
            .and_then(|license| text::optional(license.name.as_deref(), text::SHORT_TEXT)),
        image_url: image_url(print.image.as_ref()),
        likes: print.likes_count,
        downloads: print.download_count,
        paid: None,
        model_id: print.id.0,
    }
}

pub(crate) async fn model(client: &Client, id: &str) -> Result<ModelDetail, LibraryError> {
    let answer: Answer<ModelData> = query(client, "ModelFiles", MODEL, json!({"id": id})).await?;
    let refusal = answer.refusal(LibraryError::NotFound { site: SITE });
    let print = answer.data.print.ok_or(refusal)?;
    let mut files = Vec::new();
    for (kind, list) in [
        ("stl", &print.stls),
        ("gcode", &print.gcodes),
        ("sla", &print.slas),
        ("other", &print.other_files),
    ] {
        files.extend(list.iter().map(|file| RemoteFile {
            file_id: format!("{kind}-{}", file.id.0),
            name: text::file_name(&file.name),
            kind: match kind {
                "stl" => wire::kind_for_name(&file.name),
                "gcode" | "sla" => FileKind::Sliced,
                _ => FileKind::Other,
            },
            size_bytes: file.file_size,
            note: note(file),
            location: None,
        }));
    }
    let license = print.license.as_ref().and_then(|license| {
        let name = text::optional(
            license.name.as_deref().or(license.abbreviation.as_deref()),
            text::SHORT_TEXT,
        )?;
        Some(License {
            name,
            remixing_allowed: license.disallow_remixing.map(|disallowed| !disallowed),
        })
    });
    Ok(ModelDetail {
        site: SITE,
        url: page_url(&print),
        title: text::bounded(&print.name, text::SHORT_TEXT),
        designer: designer(print.user.as_ref()),
        license,
        summary: text::optional(print.summary.as_deref(), text::SUMMARY_TEXT),
        image_url: image_url(print.image.as_ref()),
        paid: None,
        files,
        download_key: None,
        model_id: print.id.0,
    })
}

pub(crate) async fn file_url(
    client: &Client,
    model: &ModelDetail,
    file: &RemoteFile,
) -> Result<Url, LibraryError> {
    let (kind, id) = file
        .file_id
        .split_once('-')
        .ok_or(LibraryError::IncompatibleResponse { site: SITE })?;
    let answer: Answer<LinkData> = query(
        client,
        "GetDownloadLink",
        DOWNLOAD,
        json!({
            "printId": model.model_id,
            "source": "model_detail",
            "fileType": kind,
            "ids": [id],
        }),
    )
    .await?;
    let refusal = answer.refusal(LibraryError::IncompatibleResponse { site: SITE });
    let link = answer.data.get_download_link.ok_or(refusal)?;
    let refused = |errors: &[LinkError]| LibraryError::Forbidden {
        site: SITE,
        reason: errors
            .iter()
            .flat_map(|error| &error.messages)
            .map(|message| text::bounded(message, text::REASON_TEXT))
            .find(|message| !message.is_empty())
            .unwrap_or_else(|| NO_LINK.into()),
    };
    if !link.ok {
        return Err(refused(&link.errors));
    }
    let output = link.output.ok_or_else(|| refused(&link.errors))?;
    let location = output.link.ok_or_else(|| refused(&[]))?;
    Url::parse(&location).map_err(|_| LibraryError::IncompatibleResponse { site: SITE })
}

fn page_url(print: &Print) -> String {
    match text::slug(print.slug.as_deref()) {
        Some(slug) => format!("{MODEL_PAGE}{}-{slug}", print.id.0),
        None => format!("{MODEL_PAGE}{}", print.id.0),
    }
}

fn designer(user: Option<&User>) -> Option<String> {
    let user = user?;
    text::optional(
        user.public_username.as_deref().or(user.handle.as_deref()),
        text::SHORT_TEXT,
    )
}

fn image_url(image: Option<&Image>) -> Option<String> {
    let path = image?.file_path.as_deref()?.trim_start_matches('/');
    text::web_url(Some(&format!("{MEDIA_ORIGIN}{path}"))).filter(|_| !path.is_empty())
}

fn note(file: &PrintFile) -> Option<String> {
    let parts: Vec<_> = [file.folder.as_deref(), file.note.as_deref()]
        .into_iter()
        .flatten()
        .map(|part| text::bounded(part, text::SHORT_TEXT))
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(": "))
}
