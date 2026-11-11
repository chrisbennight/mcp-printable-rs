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

fn category_name(category: &str) -> &str {
    if category.is_empty() {
        "Uncategorized"
    } else {
        category
    }
}

fn browse_url(project: &str, category: Option<&str>, text: &str, offset: usize) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(category) = category {
        query.append_pair("category", category);
    }
    if !text.is_empty() {
        query.append_pair("q", text);
    }
    if offset > 0 {
        query.append_pair("offset", &offset.to_string());
    }
    let query = query.finish();
    format!(
        "projects/{project}/families{}",
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    )
}

pub(super) fn collections(paths: &Paths, project: &str, families: &[families::Summary]) -> Markup {
    let mut groups: BTreeMap<&str, Vec<&families::Summary>> = BTreeMap::new();
    for family in families {
        groups.entry(&family.category).or_default().push(family);
    }
    html! { div.grid.collections {
        @for (category, members) in groups {
            a.card.collection-card href=(paths.link(&browse_url(project, Some(category), "", 0))) {
                @let pictures: Vec<_> = members.iter().filter_map(|f| f.preview.as_ref()).take(4).collect();
                div.collection-pictures.single[pictures.len() < 2] {
                    @for picture in &pictures {
                        img src=(file_url(paths,project,picture,true)) alt="" loading="lazy";
                    }
                    @if pictures.is_empty() { span.none { (category_name(category)) } }
                }
                div.body {
                    span.title { (category_name(category)) }
                    span.meta { (members.len()) " families · " (members.iter().map(|f| f.variants).sum::<usize>()) " variants" }
                }
            }
        }
    }}
}

pub(super) fn cards(paths: &Paths, project: &str, families: &[families::Summary]) -> Markup {
    html! { div.grid.family-grid {
        @for family in families { article.card.family-card {
            @let url = paths.link(&address(project,&family.id,&family.revision,None));
            a.pic href=(url) tabindex="-1" aria-hidden="true" {
                @if let Some(image) = &family.preview {
                    img src=(file_url(paths,project,image,true)) alt="" loading="lazy" width="512" height="512";
                } @else { span.none { "Preview not supplied" } }
            }
            div.body {
                a.title href=(url) {(family.name)}
                @if !family.description.is_empty() {p.desc {(family.description)}}
                p.meta {(category_name(&family.category)) " · " (family.variants) " variants"}
            }
        }}
    }}
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
    let values = match query(&uri) {
        Ok(q) => q,
        Err(e) => return failure(&paths, &e),
    };
    let category = values.get("category").cloned();
    let params = match (|| {
        Ok::<_, ToolError>(ListParams {
            project_id: project.clone(),
            text: values.get("q").cloned().unwrap_or_default(),
            offset: offset(&values)?,
            limit: PAGE_SIZE,
        })
    })() {
        Ok(params) => params,
        Err(e) => return failure(&paths, &e),
    };
    let text = params.text.clone();
    let page_offset = params.offset;
    let filter_category = category.clone();
    let workspace = Arc::clone(&web.workspace);
    let listing = match web
        .blocking(move || families::browse(&workspace, &params, filter_category.as_deref()))
        .await
    {
        Ok(v) => v,
        Err(e) => return failure(&paths, &e),
    };
    let all_families = listing.all_families;
    let listing = listing.listing;
    let categories: std::collections::BTreeSet<_> =
        all_families.iter().map(|f| f.category.as_str()).collect();
    let content = html! {main.project {
        nav.breadcrumbs aria-label="Breadcrumb" { a href=(paths.root()) {"Library"} span {"/"} a href=(paths.link(&format!("projects/{project}"))) {"Project"} span {"/"} span {"Part families"} }
        h1 {(category.as_deref().map(category_name).unwrap_or("Part families"))}
        @if category.is_none() && text.is_empty() && page_offset==0 {
            section {h2 {"Collections"} (collections(&paths,&project,&all_families))}
        }
        nav.collection-nav aria-label="Collections" {
            a href=(paths.link(&browse_url(&project,None,&text,0))) aria-current=[category.is_none().then_some("page")] {"All families"}
            @for name in categories {
                a href=(paths.link(&browse_url(&project,Some(name),&text,0))) aria-current=[(category.as_deref()==Some(name)).then_some("page")] {(category_name(name))}
            }
        }
        form.family-search method="get" {
            @if let Some(category)=&category {input type="hidden" name="category" value=(category);}
            input type="search" name="q" value=(text) placeholder="Find a family, size or style" aria-label="Find a family or variant";
            button type="submit" {"Search families"}
        }
        p.meta {(listing.total) " families in this view · " (listing.counts.variants) " variants across the project"}
        @if listing.counts.conflicting_files>0 {
            p.note {(listing.counts.conflicting_files) " paths have conflicting source identities across current families. Counts retain all declared identities; exact selections verify source bytes."}
        }
        (cards(&paths,&project,&listing.families))
        @if listing.total==0 {p {"No matching families."}}
        nav.collection-nav aria-label="Family pages" {
            @if page_offset>0 {a href=(paths.link(&browse_url(&project,category.as_deref(),&text,page_offset.saturating_sub(PAGE_SIZE)))) {"Previous page"}}
            @if let Some(next)=listing.next_offset {a href=(paths.link(&browse_url(&project,category.as_deref(),&text,next))) {"Next page"}}
        }
        details {summary {"Library file counts"}p.meta {(listing.counts.classified_files) " classified files · " (listing.counts.distinct_classified_contents) " distinct file contents · " (listing.counts.unclassified_files) " supporting files on the project page. Print quantities come from the assembly guide."}}
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
        nav.breadcrumbs aria-label="Breadcrumb" {a href=(paths.root()) {"Library"} span {"/"} a href=(paths.link(&format!("projects/{project}"))) {"Project"} span {"/"} a href=(paths.link(&format!("projects/{project}/families"))) {"All part families"}}
        h1 {(selection.family_name)}
        div.hero.family-hero {
        div {
            @if let Some(image)=selection.selected.as_ref().and_then(|v|v.preview.as_ref()) {
                div.pic {img src=(file_url(paths,project,image,true)) alt="Selected variant preview" class="family-preview";}
                p.meta {"Preview supplied for this exact variant."}
            } @else if let Some(image)=&selection.family_preview {
                div.pic {img src=(file_url(paths,project,image,true)) alt="Representative family illustration" class="family-preview";}
                p.meta {"Representative family illustration"}
            } @else {div.pic {span.none {"Preview not supplied"}}}
        }
        div {
        h2 {"Choose specifications"}
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
        p.meta {(selection.total) " supported variants match"}
        }
        }
        @if let Some(variant)=&selection.selected {
            h2 {(variant.name)} p {(variant.notes)}
            dl {@for option in &selection.options {
                dt {(option.axis.label) @if let Some(unit)=&option.axis.unit {" (" (unit) ")"}}
                dd {(variant.attributes[&option.axis.id].display())}
            }}
            @for set in &variant.representations {section {
                h3 {(set.name)}
                p.meta {"File set. Choose the applicable export; alternatives are not additional assembly quantities."}
                ul {@for file in &set.files {li {a href=(file_url(paths,project,file,false)) {(file.path)}}}}
            }}
            p {a href=(paths.link(&address(project,family,&selection.revision,Some(&variant.id)))) {"Link to this exact variant"}}
        }
        @if selection.total==0 {p.note {"This combination is unavailable. Clear choices to start again."}}
        @if selection.total>1 {
            div.variant-table {table {thead {tr {th {"Variant"} @for option in &selection.options {th {(option.axis.label) @if let Some(unit)=&option.axis.unit {" (" (unit) ")"}}}}} tbody {
                @for variant in &selection.variants {tr {
                    td {a href=(paths.link(&address(project,family,&selection.revision,Some(&variant.id)))) {(variant.name)}}
                    @for option in &selection.options {td {(variant.attributes[&option.axis.id].display())}}
                }}
            }}}
        }
        @if let Some(next)=next_url {p {a href=(next){"Next page"}}}
        @if !selection.related.is_empty() {section {h2 {"Related components"}p {"Reference suggestions; compatibility and quantities require assembly checks."}
            ul {@for related in &selection.related {li {a href=(paths.link(&format!("projects/{project}/families/{}",related.family_id))){(related.description)}}}}
        }}
    }};
    pages::family_page(paths, &selection.family_name, content)
}
