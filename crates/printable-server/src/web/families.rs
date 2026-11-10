//! Read-only family browsing. Forms navigate explicit existing combinations;
//! all links retain the family revision and exact downloads retain file hashes.
use super::*;
use crate::projects::families::{self, Attribute, ListParams, SelectParams, Selection};
use maud::html;
use std::collections::BTreeMap;

fn query(uri: &Uri) -> Result<BTreeMap<String, String>, ToolError> {
    let query = uri.query().unwrap_or_default();
    if query.len() > MAX_QUERY_BYTES {
        return Err(ToolError::Validation("The selection is too long.".into()));
    }
    let mut values = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(ToolError::Validation(
                "Selection fields cannot be repeated.".into(),
            ));
        }
    }
    Ok(values)
}
fn offset(values: &BTreeMap<String, String>) -> Result<usize, ToolError> {
    values.get("offset").map_or(Ok(0), |s| {
        s.parse()
            .map_err(|_| ToolError::Validation("Invalid page offset.".into()))
    })
}
fn address(project: &str, family: &str, revision: &str, variant: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("revision", revision);
    if let Some(variant) = variant {
        query.append_pair("variant", variant);
    }
    format!("projects/{project}/families/{family}?{}", query.finish())
}
fn file_url(paths: &Paths, project: &str, file: &families::File, preview: bool) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("sha256", &file.sha256);
    if preview {
        query.append_pair("preview", "1");
    }
    paths.link(&format!(
        "projects/{project}/files/{}?{}",
        pages::encode_path(&file.path),
        query.finish()
    ))
}

pub(super) async fn listing(
    State(web): State<Web>,
    uri: Uri,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let paths = Paths::for_request(uri.path());
    let Ok(Path(project)) = path else {
        return refusal(StatusCode::BAD_REQUEST, "Invalid project address.");
    };
    let params = match query(&uri).and_then(|q| {
        Ok(ListParams {
            project_id: project.clone(),
            text: q.get("q").cloned().unwrap_or_default(),
            offset: offset(&q)?,
            limit: PAGE_SIZE,
        })
    }) {
        Ok(params) => params,
        Err(e) => return failure(&paths, &e),
    };
    let text = params.text.clone();
    let workspace = Arc::clone(&web.workspace);
    let listing = match web
        .blocking(move || families::list(&workspace, &params))
        .await
    {
        Ok(v) => v,
        Err(e) => return failure(&paths, &e),
    };
    let next_url = listing.next_offset.map(|next| {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("q", &text)
            .append_pair("offset", &next.to_string());
        format!("?{}", q.finish())
    });
    let content = html! {main.project {
        p {a href=(paths.link(&format!("projects/{project}"))){"Back to project"}}
        h1 {"Part families"}
        p.meta {(listing.counts.families) " families · " (listing.counts.variants) " variants · " (listing.counts.classified_files) " classified files · " (listing.counts.distinct_classified_contents) " distinct file contents"}
        @if listing.counts.conflicting_files>0 {
            p.note {(listing.counts.conflicting_files) " paths have conflicting source identities across current families. Counts retain all declared identities; exact selections verify source bytes."}
        }
        p {"These are library counts. Required print quantities belong to a selected assembly or build."}
        form method="get" {label {"Find a family or variant " input type="search" name="q" value=(text);} button type="submit" {"Find"}}
        div.grid {@for family in &listing.families {article.card {div.body {
            a.title href=(paths.link(&address(&project,&family.id,&family.revision,None))) {(family.name)}
            p {(family.category)} p {(family.description)} p.meta {(family.variants) " supported variants"}
        }}}}
        @if listing.total==0 {p {"No matching families."}}
        @if let Some(next)=next_url {p {a href=(next){"Next page"}}}
        p { (listing.counts.unclassified_files) " workspace files remain outside the part families and are available on the project page." }
    }};
    html(
        StatusCode::OK,
        pages::family_page(&paths, "Part families", content),
    )
}

pub(super) async fn selection(
    State(web): State<Web>,
    uri: Uri,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let paths = Paths::for_request(uri.path());
    let Ok(Path((project, family))) = path else {
        return refusal(StatusCode::BAD_REQUEST, "Invalid family address.");
    };
    let params = match query(&uri).and_then(|q| {
        let attributes = q
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("a_")
                    .filter(|_| !v.is_empty())
                    .map(|id| (id, v))
            })
            .map(|(id, v)| {
                serde_json::from_str::<Attribute>(v)
                    .map(|a| (id.to_owned(), a))
                    .map_err(|_| ToolError::Validation("Invalid attribute choice.".into()))
            })
            .collect::<Result<_, _>>()?;
        Ok(SelectParams {
            project_id: project,
            family_id: family,
            revision: q.get("revision").cloned(),
            variant_id: q.get("variant").cloned(),
            attributes,
            offset: offset(&q)?,
            limit: PAGE_SIZE,
        })
    }) {
        Ok(p) => p,
        Err(e) => return failure(&paths, &e),
    };
    let workspace = Arc::clone(&web.workspace);
    let work = params.clone();
    let result = match web
        .blocking(move || families::select(&workspace, &work))
        .await
    {
        Ok(r) => r,
        Err(e) => return failure(&paths, &e),
    };
    html(StatusCode::OK, render(&paths, &params, &result))
}

fn render(paths: &Paths, params: &SelectParams, selection: &Selection) -> Markup {
    let project = &selection.project_id;
    let family = &selection.family_id;
    let chosen = &params.attributes;
    let base = paths.link(&format!("projects/{project}/families/{family}"));
    let next_url = selection.next_offset.map(|next| {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("revision", &selection.revision)
            .append_pair("offset", &next.to_string());
        for (id, value) in &params.attributes {
            q.append_pair(
                &format!("a_{id}"),
                &serde_json::to_string(value).expect("attribute serializes"),
            );
        }
        format!("?{}", q.finish())
    });
    let content = html! {main.project {
        p {a href=(paths.link(&format!("projects/{project}/families"))){"All part families"}}
        h1 {(selection.family_name)}
        form method="get" action=(base) {
            input type="hidden" name="revision" value=(selection.revision);
            div.filters {@for option in &selection.options {label.f {
                (option.axis.label) @if let Some(unit)=&option.axis.unit {" (" (unit) ")"}
                select name={"a_" (option.axis.id)} {
                    option value="" {"Any"}
                    @for value in &option.values {
                        option value=(serde_json::to_string(value).expect("attribute serializes")) selected[chosen.get(&option.axis.id)==Some(value)] {(value.display())}
                    }
                }
            }} button type="submit" {"Show available variants"}}
        }
        p {a href=(paths.link(&address(project,family,&selection.revision,None))){"Clear choices"}}
        @if let Some(variant)=&selection.selected {
            h2 {(variant.name)} p {(variant.notes)}
            dl {@for option in &selection.options {
                dt {(option.axis.label) @if let Some(unit)=&option.axis.unit {" (" (unit) ")"}}
                dd {(variant.attributes[&option.axis.id].display())}
            }}
            @if let Some(image)=&variant.preview {
                img src=(file_url(paths,project,image,true)) alt="Selected variant preview" class="family-preview";
                p.meta {"Preview supplied for this exact variant."}
            } @else if let Some(image)=&selection.family_preview {
                img src=(file_url(paths,project,image,true)) alt="Representative family illustration" class="family-preview";
                p.meta {"Representative family illustration; not an exact variant preview."}
            }
            @for set in &variant.representations {section {
                h3 {(set.name)}
                p.meta {"File set. Choose the applicable export; alternatives are not additional assembly quantities."}
                ul {@for file in &set.files {li {a href=(file_url(paths,project,file,false)) {(file.path)}}}}
            }}
            p {a href=(paths.link(&address(project,family,&selection.revision,Some(&variant.id)))) {"Link to this exact variant"}}
        } @else {p {(selection.total) " supported variants match."}}
        @if selection.total==0 {p.note {"This combination is unavailable. Clear choices to start again."}}
        @if selection.total>1 {
            table {thead {tr {th {"Variant"} th {"Attributes"}}} tbody {
                @for variant in &selection.variants {tr {
                    td {a href=(paths.link(&address(project,family,&selection.revision,Some(&variant.id)))) {(variant.name)}}
                    td {@for (id,value) in &variant.attributes {(id) ": " (value.display()) " "}}
                }}
            }}
        }
        @if let Some(next)=next_url {p {a href=(next){"Next page"}}}
        @if !selection.related.is_empty() {section {h2 {"Related components"}p {"Reference suggestions; compatibility and quantities require assembly checks."}
            ul {@for related in &selection.related {li {a href=(paths.link(&format!("projects/{project}/families/{}",related.family_id))){(related.description)}}}}
        }}
    }};
    pages::family_page(paths, &selection.family_name, content)
}
