//! What the `catalog` tool returns for a search: concise or full results, and
//! only the notes about search methods that the request would use.
//!
//! The index and the web page keep the complete [`SearchResponse`]; this view
//! is built from it when a tool call answers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    card::utc_date,
    index::{
        Coverage, Degradation, Detail, MEANING_INCOMPLETE, MEANING_UNAVAILABLE, MatchKind,
        PICTURE_INCOMPLETE, PICTURE_UNAVAILABLE, RERANK_UNAVAILABLE, SearchParams, SearchResponse,
        SearchResult, SearchSort,
    },
};

/// A search result in the concise form. Empty lists and absent values are
/// left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConciseResult {
    pub project_id: String,
    pub name: String,
    /// The first 200 characters of the description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Completed prints.
    pub printed: usize,
    /// UTC date of the newest completed print, as `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_printed: Option<String>,
    /// UTC creation date, as `YYYY-MM-DD`.
    pub created: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// How the result was found, with search text or a picture.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<MatchKind>,
    /// Higher is better, with search text or a picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

impl From<SearchResult> for ConciseResult {
    fn from(result: SearchResult) -> Self {
        let SearchResult {
            card,
            matched,
            score,
            scores: _,
        } = result;
        Self {
            project_id: card.project_id,
            name: card.name,
            description: card.description,
            printed: card.printed,
            last_printed: card.dates.last_printed_unix_ms.map(utc_date),
            created: utc_date(card.dates.created_unix_ms),
            formats: card.formats,
            tags: card.tags,
            matched,
            score,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ResultView {
    Concise(ConciseResult),
    Full(SearchResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchView {
    /// Concise results unless the request asked for `full`.
    pub results: Vec<ResultView>,
    /// Matching projects in the index.
    pub total: usize,
    pub next_offset: Option<usize>,
    /// Limits on the search methods this request would use, and
    /// `index_incomplete` while projects are missing from the index.
    pub degraded: Vec<Degradation>,
    pub coverage: Coverage,
}

/// What a search request asks of the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    detail: Detail,
    meaning: bool,
    picture: bool,
    rerank: bool,
}

impl View {
    pub fn of(params: &SearchParams) -> Self {
        Self {
            detail: params.detail,
            meaning: params.text.is_some() && params.effective_sort() == SearchSort::BestMatch,
            picture: params.picture.is_some(),
            rerank: params.would_rerank(),
        }
    }

    /// Whether a note concerns a method this request would use. Notes about
    /// the index itself, and any other note, are always kept.
    fn concerns(&self, note: &Degradation) -> bool {
        match note.code.as_str() {
            MEANING_UNAVAILABLE | MEANING_INCOMPLETE => self.meaning,
            PICTURE_UNAVAILABLE | PICTURE_INCOMPLETE => self.picture,
            RERANK_UNAVAILABLE => self.rerank,
            _ => true,
        }
    }

    pub fn render(&self, response: SearchResponse) -> SearchView {
        let SearchResponse {
            results,
            total,
            next_offset,
            mut degraded,
            coverage,
        } = response;
        degraded.retain(|note| self.concerns(note));
        let results = results
            .into_iter()
            .map(|result| match self.detail {
                Detail::Concise => ResultView::Concise(result.into()),
                Detail::Full => ResultView::Full(result),
            })
            .collect();
        SearchView {
            results,
            total,
            next_offset,
            degraded,
            coverage,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::projects::{
        card::{CompactCard, Dates, PictureKind},
        index::MethodScores,
    };

    fn result(scored: bool) -> SearchResult {
        SearchResult {
            card: CompactCard {
                project_id: "vase".into(),
                name: "Spiral vase".into(),
                description: String::new(),
                cover_path: Some(".printable/covers/vase/a.png".into()),
                picture: Some(PictureKind::Cover),
                dates: Dates {
                    created_unix_ms: 1_780_488_000_000,
                    created_estimated: false,
                    updated_unix_ms: 1_780_488_000_000,
                    downloaded_unix_ms: None,
                    last_printed_unix_ms: None,
                },
                printed: 0,
                last_outcome: None,
                formats: vec!["stl".into()],
                tags: Vec::new(),
                printers: vec!["P1S".into()],
                materials: vec!["PLA".into()],
                colours: vec!["#FF8800".into()],
            },
            matched: scored.then_some(MatchKind::Words),
            score: scored.then_some(2.5),
            scores: scored.then(MethodScores::default),
        }
    }

    fn note(code: &str) -> Degradation {
        Degradation {
            code: code.into(),
            message: "unavailable".into(),
            reason: None,
        }
    }

    fn response(scored: bool) -> SearchResponse {
        SearchResponse {
            results: vec![result(scored)],
            total: 1,
            next_offset: None,
            degraded: [
                MEANING_UNAVAILABLE,
                PICTURE_UNAVAILABLE,
                RERANK_UNAVAILABLE,
                MEANING_INCOMPLETE,
                PICTURE_INCOMPLETE,
                "index_incomplete",
            ]
            .into_iter()
            .map(note)
            .collect(),
            coverage: Coverage::default(),
        }
    }

    fn params(request: Value) -> SearchParams {
        serde_json::from_value(request).unwrap()
    }

    fn codes(view: &SearchView) -> Vec<&str> {
        view.degraded
            .iter()
            .map(|note| note.code.as_str())
            .collect()
    }

    #[test]
    fn concise_results_leave_out_empty_values_and_use_utc_dates() {
        let view = View::of(&params(json!({}))).render(response(false));
        assert_eq!(
            serde_json::to_value(&view.results).unwrap(),
            json!([{"project_id": "vase", "name": "Spiral vase", "printed": 0,
                "created": "2026-06-03", "formats": ["stl"]}])
        );
        let scored = View::of(&params(json!({"text": "spiral"}))).render(response(true));
        let first = serde_json::to_value(&scored.results[0]).unwrap();
        assert_eq!(
            (&first["match"], &first["score"]),
            (&json!("words"), &json!(2.5))
        );
        assert!(first.get("scores").is_none());
    }

    #[test]
    fn full_results_keep_the_complete_compact_card() {
        let view = View::of(&params(json!({"text": "spiral", "detail": "full"})));
        let rendered = view.render(response(true));
        assert_eq!(rendered.results, [ResultView::Full(result(true))]);
    }

    #[test]
    fn degraded_lists_only_the_methods_a_request_would_use() {
        for (request, expected) in [
            (json!({}), vec!["index_incomplete"]),
            (
                json!({"text": "spiral"}),
                vec![
                    MEANING_UNAVAILABLE,
                    RERANK_UNAVAILABLE,
                    MEANING_INCOMPLETE,
                    "index_incomplete",
                ],
            ),
            (
                json!({"text": "spiral", "sort": "name"}),
                vec!["index_incomplete"],
            ),
            (
                json!({"text": "spiral", "offset": 40}),
                vec![MEANING_UNAVAILABLE, MEANING_INCOMPLETE, "index_incomplete"],
            ),
            (
                json!({"picture": "photos/part.png"}),
                vec![PICTURE_UNAVAILABLE, PICTURE_INCOMPLETE, "index_incomplete"],
            ),
        ] {
            let view = View::of(&params(request.clone())).render(response(false));
            assert_eq!(codes(&view), expected, "{request}");
        }
    }
}
