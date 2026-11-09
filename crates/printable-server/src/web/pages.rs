//! HTML for the library web page. maud escapes every interpolated value, so
//! names, descriptions, tags, site text and file names stay text. Links are
//! relative, and the only external addresses are download pages shown as
//! plain anchors.

use maud::{DOCTYPE, Markup, html};

use super::{FileEntry, LibraryForm, MAX_PAGE, PAGE_SIZE, Paths, ProjectView, TEXT_FILTERS};
use crate::projects::{
    card::{CompactCard, Dates, PictureKind, civil_date, utc_date},
    index::{MatchKind, SearchResponse, SearchSort},
};

const SORTS: [(&str, &str); 4] = [
    ("", "Best match"),
    ("last_printed", "Last printed"),
    ("newest", "Newest"),
    ("name", "Name"),
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MAX_CARD_FORMATS: usize = 4;
const MAX_CARD_TAGS: usize = 6;

fn layout(paths: &Paths, title: &str, form: &LibraryForm, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href=(paths.link("static/app.css"));
            }
            body {
                header { (search_form(paths, form)) }
                (content)
            }
        }
    }
}

fn search_icon() -> Markup {
    html! {
        svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor"
            stroke-width="2" aria-hidden="true" {
            circle cx="11" cy="11" r="7" {}
            path d="m20 20-3.5-3.5" {}
        }
    }
}

fn choice(name: &str, label: &str, value: Option<bool>) -> Markup {
    html! {
        label.f {
            (label) " "
            select name=(name) {
                option value="" selected[value.is_none()] { "Any" }
                option value="yes" selected[value == Some(true)] { "Yes" }
                option value="no" selected[value == Some(false)] { "No" }
            }
        }
    }
}

fn sort_value(sort: Option<SearchSort>) -> &'static str {
    match sort {
        None | Some(SearchSort::BestMatch) => "",
        Some(SearchSort::LastPrinted) => "last_printed",
        Some(SearchSort::Newest) => "newest",
        Some(SearchSort::Name) => "name",
    }
}

fn search_form(paths: &Paths, form: &LibraryForm) -> Markup {
    let sort = sort_value(form.sort);
    html! {
        form method="get" action=(paths.root()) {
            div.bar {
                a.home href=(paths.root()) { "Library" }
                label.search {
                    (search_icon())
                    input type="search" name="q" value=(form.q) maxlength="256"
                        placeholder="Search names, tags, descriptions and files"
                        aria-label="Search words";
                }
                label.f {
                    "Sort "
                    select name="sort" {
                        @for (value, label) in SORTS {
                            option value=(value) selected[value == sort] { (label) }
                        }
                    }
                }
                button type="submit" { "Search" }
            }
            div.filters {
                (choice("printed", "Printed", form.printed))
                (choice("has_3mf", "Has 3MF", form.has_3mf))
                @for ((name, label), value) in TEXT_FILTERS.iter().zip(&form.text) {
                    label.f {
                        (label) " "
                        input type="text" name=(name) value=(value) maxlength="128" size="9";
                    }
                }
            }
        }
    }
}

/// A message page, such as for a missing project or a refused file.
pub(super) fn message(paths: &Paths, title: &str, message: &str) -> Markup {
    let content = html! {
        main.notice {
            h1 { (title) }
            p { (message) }
            p { a href=(paths.root()) { "Back to the library" } }
        }
    };
    layout(
        paths,
        &format!("{title} · Printable library"),
        &LibraryForm::default(),
        content,
    )
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

fn sort_label(sort: SearchSort) -> &'static str {
    match sort {
        SearchSort::BestMatch => "best match",
        SearchSort::LastPrinted => "last printed",
        SearchSort::Newest => "newest",
        SearchSort::Name => "name",
    }
}

fn summary(form: &LibraryForm, response: &SearchResponse) -> String {
    let coverage = response.coverage;
    let mut text = plural(response.total, "matching project", "matching projects");
    if response.total > 0 {
        let pages = response.total.div_ceil(PAGE_SIZE).min(MAX_PAGE);
        text.push_str(&format!(" · page {} of {pages}", form.page));
        text.push_str(&format!(
            " · sorted by {}",
            sort_label(form.effective_sort())
        ));
    }
    text.push_str(&format!(
        " · {} of {} in the library indexed",
        coverage.indexed, coverage.total
    ));
    if coverage.failed > 0 {
        text.push_str(&format!(", {} could not be indexed", coverage.failed));
    }
    text
}

fn pager(form: &LibraryForm, response: &SearchResponse) -> Markup {
    // `total` counts every match, including those past the last page shown.
    let more = response.total > form.page * PAGE_SIZE;
    let previous = (form.page > 1).then(|| form.query(form.page - 1));
    let next = (more && form.page < MAX_PAGE).then(|| form.query(form.page + 1));
    let capped = more && form.page >= MAX_PAGE;
    html! {
        @if previous.is_some() || next.is_some() || capped {
            nav.pager aria-label="Pages" {
                @if let Some(href) = previous { a href=(href) rel="prev" { "Previous page" } }
                @if let Some(href) = next { a href=(href) rel="next" { "Next page" } }
                @if capped {
                    span.note {
                        "Only the first " (MAX_PAGE * PAGE_SIZE)
                        " results can be shown. Add words or filters to narrow the search."
                    }
                }
            }
        }
    }
}

/// The library grid, or the form with the reason a search was refused.
pub(super) fn library(
    paths: &Paths,
    form: &LibraryForm,
    outcome: Result<&SearchResponse, &str>,
) -> Markup {
    let content = html! {
        @match outcome {
            Err(reason) => {
                main.notice { p.error { (reason) } }
            }
            Ok(response) => {
                div.status {
                    p.meta { (summary(form, response)) }
                    @if !response.degraded.is_empty() {
                        ul.notes {
                            @for note in &response.degraded { li { (note.message) } }
                        }
                    }
                }
                @if response.results.is_empty() {
                    main.notice {
                        p {
                            @if response.total > 0 {
                                "This page is past the last page of results. "
                                a href=(form.query(1)) { "Go to the first page" } "."
                            } @else if form.is_blank() {
                                "No projects are indexed yet."
                            } @else {
                                "No projects match this search."
                            }
                        }
                    }
                } @else {
                    main.grid {
                        @for result in &response.results {
                            (compact(paths, &result.card, result.matched))
                        }
                    }
                }
                (pager(form, response))
            }
        }
    };
    layout(paths, "Printable library", form, content)
}

/// The badge class and text for a project's print state.
fn print_state(printed: usize, last_outcome: Option<&str>) -> (&'static str, String) {
    match (printed, last_outcome) {
        (_, Some("failed")) => ("failed", "Last print failed".to_owned()),
        (0, _) => ("notprinted", "Not printed".to_owned()),
        (count, _) => ("printed", format!("Printed ×{count}")),
    }
}

fn format_label(format: &str) -> String {
    match format {
        "gcode.3mf" => "Sliced 3MF".to_owned(),
        "gcode" => "G-code".to_owned(),
        "blend" => "Blender".to_owned(),
        other => other.to_ascii_uppercase(),
    }
}

/// A colour as `#RRGGBB` when it is one; cards store hexadecimal colours in
/// that form.
fn hex_colour(colour: &str) -> Option<&str> {
    let digits = colour.strip_prefix('#')?;
    (digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(colour)
}

/// Small colour squares drawn as SVG shapes, which the content security
/// policy allows without inline styles.
fn swatches(colours: &[String]) -> Markup {
    html! {
        @for colour in colours {
            @if let Some(hex) = hex_colour(colour) {
                svg.swatch width="14" height="14" viewBox="0 0 14 14" role="img" aria-label=(hex) {
                    title { (hex) }
                    rect x="0.5" y="0.5" width="13" height="13" rx="3" fill=(hex) {}
                }
            }
        }
    }
}

fn joined<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let text = values.into_iter().collect::<Vec<_>>().join(" · ");
    if text.is_empty() {
        "—".to_owned()
    } else {
        text
    }
}

/// Materials followed by colours that have names rather than digits.
fn materials_and_named_colours(materials: &[String], colours: &[String]) -> String {
    joined(
        materials.iter().map(String::as_str).chain(
            colours
                .iter()
                .map(String::as_str)
                .filter(|colour| hex_colour(colour).is_none()),
        ),
    )
}

/// A date such as `3 Jun 2026`, in UTC.
fn date(unix_ms: u64) -> Markup {
    let (year, month, day) = civil_date(unix_ms);
    html! {
        time datetime=(utc_date(unix_ms)) {
            (day) " " (MONTHS[month - 1]) " " (year)
        }
    }
}

fn created(dates: &Dates) -> Markup {
    html! {
        "Created "
        @if dates.created_estimated { "about " }
        (date(dates.created_unix_ms))
    }
}

fn last_printed(dates: &Dates) -> Markup {
    html! {
        @match dates.last_printed_unix_ms {
            Some(at) => { "Printed " (date(at)) }
            None => { "Never printed" }
        }
    }
}

fn compact(paths: &Paths, card: &CompactCard, matched: Option<MatchKind>) -> Markup {
    let project = paths.link(&format!("projects/{}", card.project_id));
    let (state, state_text) = print_state(card.printed, card.last_outcome.as_deref());
    html! {
        article.card {
            a.pic href=(project) tabindex="-1" aria-hidden="true" {
                @match card.picture.or(card.cover_path.as_ref().map(|_| PictureKind::Cover)) {
                    Some(PictureKind::Cover) => {
                        img src={ (project) "/cover" } alt="" loading="lazy";
                    }
                    Some(PictureKind::Plate) => {
                        img src={ (project) "/pictures/plate" } alt="" loading="lazy";
                    }
                    None => {
                        span.none { "No picture yet" }
                    }
                }
                div.over {
                    span.badge.(state) { (state_text) }
                    span.r {
                        @for format in card.formats.iter().take(MAX_CARD_FORMATS) {
                            span.badge.fmt { (format_label(format)) }
                        }
                    }
                }
            }
            div.body {
                a.title href=(project) title=(card.name) { (card.name) }
                @if matched == Some(MatchKind::Near) { span.score { "Similar name" } }
                p.desc { (card.description) }
                div.row {
                    span.k { "Colours" }
                    (swatches(&card.colours))
                    span.v { (materials_and_named_colours(&card.materials, &card.colours)) }
                }
                div.row {
                    span.k { "Printers" }
                    span.v { (joined(card.printers.iter().map(String::as_str))) }
                }
                @if !card.tags.is_empty() {
                    div.tags {
                        @for tag in card.tags.iter().take(MAX_CARD_TAGS) { span.tag { (tag) } }
                        @if card.tags.len() > MAX_CARD_TAGS {
                            span.tag { "+" (card.tags.len() - MAX_CARD_TAGS) }
                        }
                    }
                }
                div.dates {
                    span { (created(&card.dates)) }
                    span { (last_printed(&card.dates)) }
                }
            }
        }
    }
}

/// A file size in bytes or binary units.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return plural(
            usize::try_from(bytes).unwrap_or(usize::MAX),
            "byte",
            "bytes",
        );
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Percent-encode each segment of a project-relative path for a link.
fn encode_path(path: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(path.len());
    for (position, segment) in path.split('/').enumerate() {
        if position > 0 {
            encoded.push('/');
        }
        for byte in segment.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(char::from(byte));
            } else {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// An address to link to when it is an HTTP or HTTPS page.
fn web_page(address: &str) -> Option<String> {
    let parsed = url::Url::parse(address).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

fn slices(view: &ProjectView, root: &str) -> Markup {
    let slices = &view.card.slices;
    html! {
        section {
            h2 { "Slices" }
            @if slices.count == 0 {
                p.muted { "Not sliced yet." }
            } @else {
                p {
                    (plural(slices.count, "completed slice", "completed slices"))
                    @if slices.truncated { " among the 256 most recently changed slice records" }
                }
            }
            @if let Some(newest) = &slices.newest {
                dl.facts {
                    dt { "Newest" }
                    dd { (newest.output_dir.strip_prefix(root).unwrap_or(&newest.output_dir)) }
                    dt { "Printer" }
                    dd {
                        (joined(newest.printer.as_deref().into_iter()
                            .chain(newest.printer_model.as_deref())))
                    }
                    dt { "Process" }
                    dd { (newest.process.as_deref().unwrap_or("—")) }
                    dt { "Plates" }
                    dd {
                        @match newest.plate_count {
                            Some(plates) => { (plates) }
                            None => { "—" }
                        }
                    }
                    dt { "Finished" }
                    dd {
                        @match newest.finished_unix_ms {
                            Some(at) => {
                                @if newest.finished_estimated { "about " }
                                (date(at))
                            }
                            None => { "—" }
                        }
                    }
                    dt { "Filaments" }
                    dd {
                        @if newest.filaments.is_empty() {
                            "—"
                        } @else {
                            ul.plain {
                                @for filament in &newest.filaments {
                                    li {
                                        @if let Some(colour) = &filament.colour {
                                            (swatches(std::slice::from_ref(colour)))
                                        }
                                        @if let Some(slot) = filament.slot { "Slot " (slot) ": " }
                                        (joined(filament.filament_type.as_deref().into_iter()
                                            .chain(filament.colour.as_deref())))
                                    }
                                }
                            }
                        }
                        @if newest.filaments_truncated {
                            p.note { "Only the first 16 filaments are shown." }
                        }
                    }
                }
            }
        }
    }
}

fn prints(view: &ProjectView) -> Markup {
    let prints = &view.card.prints;
    let outcomes = &prints.outcomes;
    let counts = [
        (outcomes.completed, "completed"),
        (outcomes.failed, "failed"),
        (outcomes.cancelled, "cancelled"),
        (outcomes.skipped, "skipped"),
        (outcomes.unknown, "with an unknown outcome"),
    ];
    let counted = counts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, outcome)| format!("{count} {outcome}"))
        .collect::<Vec<_>>()
        .join(", ");
    html! {
        section {
            h2 { "Prints" }
            @if prints.count == 0 {
                p.muted { "No prints recorded. Only prints that Printable sent to a printer are counted." }
            } @else {
                p { (plural(prints.count, "recorded print", "recorded prints")) ": " (counted) }
                @if let Some(outcome) = &prints.last_outcome {
                    p { "The latest print " (outcome_phrase(outcome)) "." }
                }
                @if !prints.completed_by_printer.is_empty() {
                    table {
                        thead { tr { th { "Printer" } th.number { "Completed prints" } } }
                        tbody {
                            @for printer in &prints.completed_by_printer {
                                tr { td { (printer.printer) } td.number { (printer.completed) } }
                            }
                        }
                    }
                }
                @if prints.printers_truncated {
                    p.note { "Only the 16 printers with the most completed prints are shown." }
                }
            }
        }
    }
}

fn outcome_phrase(outcome: &str) -> &'static str {
    match outcome {
        "completed" => "completed",
        "failed" => "failed",
        "cancelled" => "was cancelled",
        "skipped" => "was skipped",
        _ => "has an unknown outcome",
    }
}

fn downloads(view: &ProjectView) -> Markup {
    let downloads = &view.card.downloads;
    html! {
        section {
            h2 { "Downloads" }
            @if downloads.count == 0 {
                p.muted { "Not downloaded from a model site." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "Site" } th { "Title" } th { "Designer" } th { "License" }
                            th { "Downloaded" } th { "Page" }
                        }
                    }
                    tbody {
                        @for item in &downloads.items {
                            tr {
                                td { (item.site.as_deref().unwrap_or("—")) }
                                td { (item.title.as_deref().unwrap_or("—")) }
                                td { (item.designer.as_deref().unwrap_or("—")) }
                                td { (item.license.as_deref().unwrap_or("—")) }
                                td {
                                    @match item.retrieved_unix_ms {
                                        Some(at) => { (date(at)) }
                                        None => { "—" }
                                    }
                                }
                                td.address {
                                    @match item.url.as_deref() {
                                        Some(address) => {
                                            @if let Some(href) = web_page(address) {
                                                a href=(href) rel="noopener noreferrer" { (address) }
                                            } @else {
                                                (address)
                                            }
                                        }
                                        None => { "—" }
                                    }
                                }
                            }
                        }
                    }
                }
                @if downloads.truncated {
                    p.note { "Only the first 16 download records are shown." }
                }
            }
        }
    }
}

fn files(view: &ProjectView, project: &str) -> Markup {
    let total = view.card.file_count;
    html! {
        section {
            h2 { "Files" }
            @if view.files.is_empty() {
                p.muted { "No files." }
            } @else {
                table.files {
                    thead { tr { th { "File" } th.number { "Size" } th { "Changed" } } }
                    tbody {
                        @for file in &view.files { (file_row(file, project)) }
                    }
                }
                @if total > view.files.len() {
                    p.note {
                        "Only the first " (view.files.len()) " of " (total) " files are listed."
                    }
                }
            }
        }
    }
}

fn file_row(file: &FileEntry, project: &str) -> Markup {
    html! {
        tr {
            td { a href={ (project) "/files/" (encode_path(&file.path)) } { (file.path) } }
            td.number { (size(file.size_bytes)) }
            td { (date(file.modified_unix_ms)) }
        }
    }
}

/// A project's page, from its full card.
pub(super) fn project(paths: &Paths, view: &ProjectView) -> Markup {
    let card = &view.card;
    let project = paths.link(&format!("projects/{}", card.project_id));
    let root = format!("projects/{}/", card.project_id);
    let (state, state_text) = print_state(
        card.prints.outcomes.completed,
        card.prints.last_outcome.as_deref(),
    );
    let content = html! {
        main.project {
            p { a href=(paths.root()) { "Back to the library" } }
            div.hero {
                div.pic.large {
                    @if let Some(cover) = &card.cover {
                        img src={ (project) "/cover" } alt="Cover picture"
                            width=(cover.width) height=(cover.height);
                    } @else if let Some(plate) = &card.pictures.plate {
                        img src={ (project) "/pictures/plate" }
                            alt="Plate preview generated from the project's model"
                            title="Generated plate preview"
                            width=(plate.width) height=(plate.height);
                    } @else {
                        span.none { "No picture yet" }
                    }
                }
                div.summary {
                    h1 { (card.name) }
                    p.badges {
                        span.badge.(state) { (state_text) }
                        @for format in &card.formats {
                            span.badge.chip { (format_label(&format.format)) " ×" (format.files) }
                        }
                    }
                    @if !card.description.is_empty() { p.description { (card.description) } }
                    @if card.description_truncated {
                        p.note { "Only the first 4096 characters of the description are shown." }
                    }
                    @if !card.tags.is_empty() {
                        div.tags { @for tag in &card.tags { span.tag { (tag) } } }
                    }
                    dl.facts {
                        dt { "Created" }
                        dd {
                            @if card.dates.created_estimated { "about " }
                            (date(card.dates.created_unix_ms))
                        }
                        dt { "Changed" }
                        dd { (date(card.dates.updated_unix_ms)) }
                        @if let Some(at) = card.dates.downloaded_unix_ms {
                            dt { "Downloaded" }
                            dd { (date(at)) }
                        }
                        dt { "Last printed" }
                        dd {
                            @match card.dates.last_printed_unix_ms {
                                Some(at) => { (date(at)) }
                                None => { "Never" }
                            }
                        }
                        dt { "Printers" }
                        dd { (joined(card.printers.iter().map(String::as_str))) }
                        dt { "Materials" }
                        dd { (joined(card.materials.iter().map(String::as_str))) }
                        dt { "Colours" }
                        dd {
                            (swatches(&card.colours))
                            " " (joined(card.colours.iter().map(String::as_str)))
                        }
                    }
                    @if card.names_truncated {
                        p.note { "Only the first 16 printers, materials and colours are shown." }
                    }
                }
            }
            (slices(view, &root))
            (prints(view))
            (downloads(view))
            (files(view, &project))
            @if !card.unreadable.is_empty() {
                section {
                    h2 { "Unreadable records" }
                    p { "These records could not be read, so this page leaves out their details." }
                    ul {
                        @for entry in &card.unreadable {
                            li { code { (entry.path) } " (" (entry.code) ")" }
                        }
                    }
                }
            }
        }
    };
    layout(
        paths,
        &format!("{} · Printable library", card.name),
        &LibraryForm::default(),
        content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_sizes_and_paths_are_formatted_for_people() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        // 2026-06-03T12:00:00Z and the last millisecond of 2024-02-29.
        assert_eq!(civil_date(1_780_488_000_000), (2026, 6, 3));
        assert_eq!(civil_date(1_709_251_199_999), (2024, 2, 29));
        assert_eq!(civil_date(1_709_251_200_000), (2024, 3, 1));
        assert_eq!(
            date(1_780_488_000_000).into_string(),
            r#"<time datetime="2026-06-03">3 Jun 2026</time>"#
        );
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(1023), "1023 bytes");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(
            encode_path("parts/<b> \"x\"/drawer #1.stl"),
            "parts/%3Cb%3E%20%22x%22/drawer%20%231.stl"
        );
        assert_eq!(hex_colour("#00AA00"), Some("#00AA00"));
        assert_eq!(hex_colour("#00AA00\"><b>"), None);
        assert_eq!(hex_colour("Galaxy Black"), None);
        assert_eq!(web_page("javascript:alert(1)"), None);
        assert_eq!(
            web_page("https://printables.example/model/1").as_deref(),
            Some("https://printables.example/model/1")
        );
    }
}
