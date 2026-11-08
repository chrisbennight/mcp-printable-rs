//! The derived search index over project library cards.
//!
//! The index holds nothing that cannot be rebuilt from workspace records.
//! Each pass lists the project records, hashes each project's inputs from
//! metadata, rebuilds the cards whose inputs changed, and removes entries for
//! projects that no longer exist. Deleting the index file loses nothing: the
//! next start creates an empty index and the first pass fills it again.
//!
//! Search text never reaches SQLite as query syntax. It is split into words,
//! each word becomes a quoted FTS5 string, and every value is a bound
//! parameter.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use printable_workspace::{Workspace, WsError};
use rusqlite::{Connection, TransactionBehavior, params, params_from_iter, types::Value as Sql};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::card::{self, CompactCard, Inputs};
use crate::{config::SettingsError, error::ToolError, slicing::summary};

const DIR_KEY: &str = "PRINTABLE_PROJECT_INDEX_DIR";
const INTERVAL_KEY: &str = "PRINTABLE_PROJECT_INDEX_INTERVAL_SECONDS";
const DEFAULT_INTERVAL_SECONDS: u64 = 30;
const MAX_INTERVAL_SECONDS: u64 = 86_400;
const FILE_NAME: &str = "index.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Stored as SQLite's `user_version`; a different value recreates the tables.
const SCHEMA_VERSION: i64 = 1;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
pub(crate) const MAX_OFFSET: usize = 1000;
const MAX_TEXT_CHARS: usize = 256;
const MAX_WORDS: usize = 16;
const MAX_FILTER_CHARS: usize = 128;
/// Three-character pieces compared when words match nothing.
const MAX_PIECES: usize = 64;
const MAX_LISTED_FAILURES: usize = 16;
const MAX_ERROR_CHARS: usize = 512;
/// bm25 weights for the name, tags, description, source and files columns.
const WEIGHTS: &str = "10.0, 5.0, 3.0, 2.0, 1.0";

const SCHEMA: &str = "
CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    project_id TEXT NOT NULL UNIQUE,
    input_hash TEXT NOT NULL,
    card TEXT,
    name_folded TEXT NOT NULL DEFAULT '',
    created_unix_ms INTEGER NOT NULL DEFAULT 0,
    last_printed_unix_ms INTEGER,
    printed INTEGER NOT NULL DEFAULT 0,
    has_3mf INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE TABLE facets (
    project INTEGER NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (kind, value, project)
) WITHOUT ROWID;
CREATE INDEX facets_by_project ON facets (project);
CREATE VIRTUAL TABLE project_text USING fts5(
    name, tags, description, source, files,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE project_names USING fts5(name, tokenize = 'trigram');
";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Storage {
    Memory,
    Directory(PathBuf),
}

/// The search index shared by every session and the background indexer.
pub struct ProjectIndex {
    storage: Storage,
    /// Time between background passes; `None` turns them off.
    interval: Option<Duration>,
    /// One connection, opened on first use and held only for short statements.
    connection: Mutex<Option<Connection>>,
    /// Serialises passes so two never interleave their writes.
    passes: Mutex<()>,
    /// Requested passes wait here rather than on the blocking pool.
    turns: Arc<Semaphore>,
    last_pass: Mutex<Option<PassReport>>,
    /// Stops a running pass between projects at shutdown.
    stop: CancellationToken,
}

impl std::fmt::Debug for ProjectIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectIndex")
            .field("storage", &self.storage_kind())
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl Default for ProjectIndex {
    /// An in-memory index with background passes at the default interval.
    fn default() -> Self {
        Self::new(
            Storage::Memory,
            Some(Duration::from_secs(DEFAULT_INTERVAL_SECONDS)),
        )
    }
}

/// Read the index settings. Without a directory the index lives in memory
/// and is rebuilt after each start; an interval of zero turns background
/// passes off.
pub fn configure(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Arc<ProjectIndex>, SettingsError> {
    let value = |key| get(key).filter(|value| !value.trim().is_empty());
    let storage = match value(DIR_KEY) {
        None => Storage::Memory,
        Some(raw) => {
            let path = PathBuf::from(raw.trim());
            if !path.is_absolute() {
                return Err(SettingsError::ProjectIndex(DIR_KEY));
            }
            Storage::Directory(path)
        }
    };
    let seconds = match value(INTERVAL_KEY) {
        None => DEFAULT_INTERVAL_SECONDS,
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= MAX_INTERVAL_SECONDS)
            .ok_or(SettingsError::ProjectIndex(INTERVAL_KEY))?,
    };
    Ok(Arc::new(ProjectIndex::new(
        storage,
        (seconds > 0).then(|| Duration::from_secs(seconds)),
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PassKind {
    /// Rebuild cards whose inputs changed.
    Incremental,
    /// Clear the index and rebuild every card.
    Rebuild,
    /// Rebuild changed cards and try failed projects again.
    Retry,
}

/// What one indexing pass did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PassReport {
    pub kind: PassKind,
    /// Project records found.
    pub projects: usize,
    /// Cards built in this pass.
    pub rebuilt: usize,
    pub unchanged: usize,
    /// Projects whose card could not be built in this pass.
    pub failed: usize,
    /// Entries removed because their project no longer exists.
    pub removed: usize,
    /// True when shutdown stopped the pass before every project was visited.
    pub stopped: bool,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
}

/// Request body for `index_status`, `index_rebuild` and `index_retry`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Best word match first; requires `text`.
    BestMatch,
    /// Most recently printed first; never-printed projects last.
    LastPrinted,
    /// Most recently created first.
    Newest,
    /// By name.
    Name,
}

/// Every filter must match. Text filters compare whole values without regard
/// to letter case; colours compare as `#RRGGBB`.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchFilters {
    /// True for projects with at least one completed print, false for the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printed: Option<bool>,
    /// A printer name or model shown on the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// True for projects with a 3MF file, sliced or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_3mf: Option<bool>,
    /// A filament type such as PLA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    /// A filament colour such as #FF8800.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// A model site: makerworld, printables or thingiverse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A license name from a download record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// A file format shown on the card, such as stl, step or gcode.3mf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    /// Words to find in names, tags, descriptions, download sources, file
    /// names, materials and printers: at most 256 characters and 16 words.
    /// Every word must match the start of a word in the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub filters: SearchFilters,
    /// Defaults to best_match with text and newest without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SearchSort>,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: usize,
    #[serde(default)]
    #[schemars(range(max = 1000))]
    pub offset: usize,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

/// How a result matched the search text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Every word matched.
    Words,
    /// No project matched every word; the name shares at least half of the
    /// words' three-character pieces.
    Near,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    #[serde(flatten)]
    pub card: CompactCard,
    /// Null without search text.
    #[serde(rename = "match")]
    pub matched: Option<MatchKind>,
    /// Higher is better: a bm25 relevance for word matches, the share of
    /// matching pieces for near matches, null without search text.
    pub score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Degradation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Coverage {
    /// Projects with a current card in the index.
    pub indexed: usize,
    /// Project records in the workspace.
    pub total: usize,
    /// Projects whose card could not be built.
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    /// Matching projects in the index.
    pub total: usize,
    pub next_offset: Option<usize>,
    /// Search capabilities that are missing or limited for this response.
    pub degraded: Vec<Degradation>,
    pub coverage: Coverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexFailure {
    pub project_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// `index.sqlite3` in the configured directory, kept across restarts.
    File,
    /// Rebuilt after each start.
    Memory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexStatus {
    pub storage: StorageKind,
    /// Project records in the workspace.
    pub projects: usize,
    pub indexed: usize,
    /// Projects not yet indexed.
    pub pending: usize,
    pub failed: usize,
    /// Up to 16 failed projects, by project ID.
    pub failures: Vec<IndexFailure>,
    /// Seconds between background passes; null when they are off.
    pub interval_seconds: Option<u64>,
    /// The latest pass since this server started.
    pub last_pass: Option<PassReport>,
}

fn sql_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Validation(message.into())
}

/// Project IDs with a record, in ID order.
fn project_ids(workspace: &Workspace) -> Result<Vec<String>, ToolError> {
    let files = match workspace.list_artifacts(".printable/projects", usize::MAX) {
        Ok(files) => files,
        Err(WsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(files
        .into_iter()
        .filter_map(|file| {
            file.path
                .strip_prefix(".printable/projects/")
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|id| super::validate_id(id).is_ok())
                .map(str::to_owned)
        })
        .collect())
}

/// Everything stored for one indexed project.
struct Entry {
    compact: String,
    name_folded: String,
    created_unix_ms: u64,
    last_printed_unix_ms: Option<u64>,
    printed: bool,
    has_3mf: bool,
    facets: Vec<(&'static str, String)>,
    text: card::SearchText,
    /// Why the newest slice's summary is missing from the card, so the
    /// project is counted as failed and `index_retry` tries it again.
    summary_problem: Option<String>,
}

impl Entry {
    fn new(workspace: &Workspace, inputs: &Inputs) -> Result<Self, ToolError> {
        let card = card::build(workspace, inputs, card::Summaries::RecordMissing);
        Ok(Self {
            compact: serde_json::to_string(&card.compact())?,
            name_folded: card.name.to_lowercase(),
            created_unix_ms: card.dates.created_unix_ms,
            last_printed_unix_ms: card.dates.last_printed_unix_ms,
            printed: card.prints.outcomes.completed > 0,
            has_3mf: card.has_3mf,
            facets: card.facets().into_iter().collect(),
            text: card.search_text(inputs),
            summary_problem: card
                .unreadable
                .iter()
                .find(|note| note.path.ends_with(&format!("/{}", summary::FILE)))
                .map(|note| {
                    format!(
                        "{}: the newest slice's summary could not be read or recorded",
                        note.code
                    )
                }),
        })
    }
}

fn prepare_schema(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        transaction.execute_batch(
            "DROP TABLE IF EXISTS facets;
             DROP TABLE IF EXISTS projects;
             DROP TABLE IF EXISTS project_text;
             DROP TABLE IF EXISTS project_names;",
        )?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    transaction.commit()
}

fn remove(connection: &Connection, row: i64) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM facets WHERE project = ?1", [row])?;
    connection.execute("DELETE FROM project_text WHERE rowid = ?1", [row])?;
    connection.execute("DELETE FROM project_names WHERE rowid = ?1", [row])?;
    connection.execute("DELETE FROM projects WHERE id = ?1", [row])?;
    Ok(())
}

fn store(
    connection: &mut Connection,
    project_id: &str,
    hash: &str,
    entry: &Entry,
) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: i64 = transaction.query_row(
        "INSERT INTO projects (project_id, input_hash, card, name_folded, created_unix_ms,
             last_printed_unix_ms, printed, has_3mf, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
         ON CONFLICT (project_id) DO UPDATE SET input_hash = excluded.input_hash,
             card = excluded.card, name_folded = excluded.name_folded,
             created_unix_ms = excluded.created_unix_ms,
             last_printed_unix_ms = excluded.last_printed_unix_ms,
             printed = excluded.printed, has_3mf = excluded.has_3mf, error = NULL
         RETURNING id",
        params![
            project_id,
            hash,
            entry.compact,
            entry.name_folded,
            sql_int(entry.created_unix_ms),
            entry.last_printed_unix_ms.map(sql_int),
            entry.printed,
            entry.has_3mf,
        ],
        |row| row.get(0),
    )?;
    transaction.execute("DELETE FROM facets WHERE project = ?1", [row])?;
    {
        let mut insert = transaction
            .prepare("INSERT OR IGNORE INTO facets (project, kind, value) VALUES (?1, ?2, ?3)")?;
        for (kind, value) in &entry.facets {
            insert.execute(params![row, kind, value])?;
        }
    }
    let text = &entry.text;
    transaction.execute("DELETE FROM project_text WHERE rowid = ?1", [row])?;
    transaction.execute(
        "INSERT INTO project_text (rowid, name, tags, description, source, files)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            row,
            text.name,
            text.tags,
            text.description,
            text.source,
            text.files
        ],
    )?;
    transaction.execute("DELETE FROM project_names WHERE rowid = ?1", [row])?;
    transaction.execute(
        "INSERT INTO project_names (rowid, name) VALUES (?1, ?2)",
        params![row, text.name],
    )?;
    transaction.commit()
}

/// Record a failure. A card from an earlier pass stays searchable.
fn store_failure(
    connection: &Connection,
    project_id: &str,
    hash: &str,
    error: &ToolError,
) -> rusqlite::Result<()> {
    let message = format!("{}: {error}", error.code());
    let message = match message.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => &message[..end],
        None => &message,
    };
    connection.execute(
        "INSERT INTO projects (project_id, input_hash, error) VALUES (?1, ?2, ?3)
         ON CONFLICT (project_id) DO UPDATE SET input_hash = excluded.input_hash,
             error = excluded.error",
        params![project_id, hash, message],
    )?;
    Ok(())
}

/// Indexed, failed and total counts for the projects that exist now.
fn coverage(connection: &Connection, ids: &[String]) -> rusqlite::Result<Coverage> {
    let mut statement = connection
        .prepare("SELECT project_id, card IS NOT NULL, error IS NOT NULL FROM projects")?;
    let rows: HashMap<String, (bool, bool)> = statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
        .collect::<rusqlite::Result<_>>()?;
    let mut coverage = Coverage {
        total: ids.len(),
        ..Coverage::default()
    };
    for id in ids {
        match rows.get(id) {
            Some((_, true)) => coverage.failed += 1,
            Some((true, false)) => coverage.indexed += 1,
            _ => {}
        }
    }
    Ok(coverage)
}

fn degraded(coverage: Coverage, background: bool) -> Vec<Degradation> {
    let mut degraded = vec![
        Degradation {
            code: "meaning_search_unavailable".into(),
            message:
                "Search matches words in project records; searching by meaning is not available."
                    .into(),
        },
        Degradation {
            code: "picture_search_unavailable".into(),
            message: "Searching by picture is not available.".into(),
        },
    ];
    if coverage.indexed < coverage.total {
        let pending = coverage.total - coverage.indexed - coverage.failed;
        let next = match (coverage.failed > 0, background) {
            (true, _) => "Read index_status for the failures, then run index_retry.",
            (false, true) => "Background indexing continues.",
            (false, false) => "Background indexing is off; run index_rebuild or index_retry.",
        };
        degraded.push(Degradation {
            code: "index_incomplete".into(),
            message: format!(
                "{} of {} projects are indexed; {pending} are waiting and {} could not be indexed. Results cover indexed projects only. {next}",
                coverage.indexed, coverage.total, coverage.failed
            ),
        });
    }
    degraded
}

/// A quoted FTS5 string. Doubling quotes keeps any text a literal.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matching {
    All,
    Words,
    Near,
}

#[derive(Debug)]
struct Query {
    words: Vec<String>,
    /// Filter SQL fragments and their bound values, in order.
    filters: Vec<(&'static str, Vec<Sql>)>,
    sort: SearchSort,
    limit: usize,
    offset: usize,
}

struct Page {
    results: Vec<SearchResult>,
    total: usize,
}

impl Query {
    fn new(params: SearchParams) -> Result<Self, ToolError> {
        if !(1..=MAX_LIMIT).contains(&params.limit) {
            return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")));
        }
        if params.offset > MAX_OFFSET {
            return Err(invalid(format!("offset must be at most {MAX_OFFSET}")));
        }
        let words: Vec<String> = match &params.text {
            None => Vec::new(),
            Some(text) => {
                if text.chars().count() > MAX_TEXT_CHARS {
                    return Err(invalid(format!(
                        "search text must be at most {MAX_TEXT_CHARS} characters"
                    )));
                }
                let words: Vec<String> = text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .map(str::to_lowercase)
                    .collect();
                if words.is_empty() {
                    return Err(invalid("search text needs at least one letter or digit"));
                }
                if words.len() > MAX_WORDS {
                    return Err(invalid(format!(
                        "search text can have at most {MAX_WORDS} words"
                    )));
                }
                words
            }
        };
        let sort = match params.sort {
            Some(SearchSort::BestMatch) if words.is_empty() => {
                return Err(invalid("best_match sorting needs search text"));
            }
            Some(sort) => sort,
            None if words.is_empty() => SearchSort::Newest,
            None => SearchSort::BestMatch,
        };
        let SearchFilters {
            printed,
            printer,
            has_3mf,
            material,
            colour,
            tag,
            source,
            license,
            format,
        } = params.filters;
        let mut filters: Vec<(&'static str, Vec<Sql>)> = Vec::new();
        for (column, flag) in [("p.printed = ?", printed), ("p.has_3mf = ?", has_3mf)] {
            if let Some(flag) = flag {
                filters.push((column, vec![Sql::Integer(i64::from(flag))]));
            }
        }
        for (kind, value) in [
            ("printer", printer),
            ("material", material),
            ("colour", colour),
            ("tag", tag),
            ("source", source),
            ("license", license),
            ("format", format),
        ] {
            let Some(value) = value else { continue };
            let value = card::facet_value(kind, &value);
            if value.is_empty() || value.chars().count() > MAX_FILTER_CHARS {
                return Err(invalid(format!(
                    "the {kind} filter must be 1 to {MAX_FILTER_CHARS} characters"
                )));
            }
            filters.push((
                "EXISTS (SELECT 1 FROM facets f WHERE f.kind = ? AND f.value = ? AND f.project = p.id)",
                vec![Sql::Text(kind.into()), Sql::Text(value)],
            ));
        }
        Ok(Self {
            words,
            filters,
            sort,
            limit: params.limit,
            offset: params.offset,
        })
    }

    /// Distinct three-character pieces of the words that have at least three
    /// characters, in order.
    fn pieces(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut pieces = Vec::new();
        for word in &self.words {
            let chars: Vec<char> = word.chars().collect();
            for window in chars.windows(3) {
                let piece: String = window.iter().collect();
                if seen.insert(piece.clone()) {
                    pieces.push(piece);
                }
            }
        }
        pieces.truncate(MAX_PIECES);
        pieces
    }

    fn run(&self, connection: &Connection, matching: Matching) -> rusqlite::Result<Page> {
        let mut values: Vec<Sql> = Vec::new();
        let pieces = self.pieces();
        let (score, from) = match matching {
            Matching::All => ("NULL".to_owned(), "projects p WHERE p.card IS NOT NULL"),
            Matching::Words => {
                values.push(Sql::Text(
                    self.words
                        .iter()
                        .map(|word| format!("{}*", quoted(word)))
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
                (
                    format!("-bm25(project_text, {WEIGHTS})"),
                    "project_text JOIN projects p ON p.id = project_text.rowid
                     WHERE project_text MATCH ? AND p.card IS NOT NULL",
                )
            }
            Matching::Near => {
                values.extend(pieces.iter().map(|piece| Sql::Text(piece.clone())));
                values.push(Sql::Text(
                    pieces
                        .iter()
                        .map(|piece| quoted(piece))
                        .collect::<Vec<_>>()
                        .join(" OR "),
                ));
                (
                    vec!["(instr(p.name_folded, ?) > 0)"; pieces.len()].join(" + "),
                    "project_names JOIN projects p ON p.id = project_names.rowid
                     WHERE project_names MATCH ? AND p.card IS NOT NULL",
                )
            }
        };
        let mut inner = format!(
            "SELECT p.card AS card, {score} AS score, p.name_folded AS name_folded,
                 p.project_id AS project_id, p.created_unix_ms AS created_unix_ms,
                 p.last_printed_unix_ms AS last_printed_unix_ms
             FROM {from}"
        );
        for (condition, bound) in &self.filters {
            inner.push_str(" AND ");
            inner.push_str(condition);
            values.extend(bound.iter().cloned());
        }
        let outer = if matching == Matching::Near {
            values.push(Sql::Integer(pieces.len() as i64));
            "WHERE score * 2 >= ?"
        } else {
            ""
        };
        let total: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM ({inner}) {outer}"),
            params_from_iter(values.iter()),
            |row| row.get(0),
        )?;
        let order = match self.sort {
            SearchSort::BestMatch => "score DESC, name_folded, project_id",
            SearchSort::LastPrinted => {
                "last_printed_unix_ms IS NULL, last_printed_unix_ms DESC, name_folded, project_id"
            }
            SearchSort::Newest => "created_unix_ms DESC, name_folded, project_id",
            SearchSort::Name => "name_folded, project_id",
        };
        values.push(Sql::Integer(self.limit as i64));
        values.push(Sql::Integer(self.offset as i64));
        let mut statement = connection.prepare(&format!(
            "SELECT card, score FROM ({inner}) {outer} ORDER BY {order} LIMIT ? OFFSET ?"
        ))?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut results = Vec::with_capacity(rows.len());
        for (card, score) in rows {
            let card: CompactCard = serde_json::from_str(&card).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let (matched, score) = match matching {
                Matching::All => (None, None),
                Matching::Words => (Some(MatchKind::Words), score),
                Matching::Near => (
                    Some(MatchKind::Near),
                    score.map(|shared| shared / pieces.len() as f64),
                ),
            };
            results.push(SearchResult {
                card,
                matched,
                score,
            });
        }
        Ok(Page {
            results,
            total: usize::try_from(total).unwrap_or(0),
        })
    }
}

impl ProjectIndex {
    fn new(storage: Storage, interval: Option<Duration>) -> Self {
        Self {
            storage,
            interval,
            connection: Mutex::new(None),
            passes: Mutex::new(()),
            turns: Arc::new(Semaphore::new(1)),
            last_pass: Mutex::new(None),
            stop: CancellationToken::new(),
        }
    }

    fn storage_kind(&self) -> StorageKind {
        match self.storage {
            Storage::Memory => StorageKind::Memory,
            Storage::Directory(_) => StorageKind::File,
        }
    }

    fn connect(&self) -> Result<Connection, ToolError> {
        let mut connection = match &self.storage {
            Storage::Memory => Connection::open_in_memory()?,
            Storage::Directory(directory) => {
                if !std::fs::metadata(directory).is_ok_and(|meta| meta.is_dir()) {
                    return Err(ToolError::ProjectIndex(format!(
                        "{DIR_KEY} must name an existing directory"
                    )));
                }
                let connection = Connection::open(directory.join(FILE_NAME))?;
                connection.busy_timeout(BUSY_TIMEOUT)?;
                let mode: String =
                    connection
                        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
                if !mode.eq_ignore_ascii_case("wal") {
                    return Err(ToolError::ProjectIndex(format!(
                        "{DIR_KEY} must be on a local filesystem that supports SQLite's write-ahead log"
                    )));
                }
                connection.pragma_update(None, "synchronous", "NORMAL")?;
                connection
            }
        };
        prepare_schema(&mut connection)?;
        Ok(connection)
    }

    /// Run `work` with the connection, opening it first if needed. The lock
    /// covers only index statements, never workspace reads.
    fn with_connection<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, ToolError>,
    ) -> Result<T, ToolError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if connection.is_none() {
            *connection = Some(self.connect()?);
        }
        work(connection.as_mut().expect("opened above"))
    }

    /// Open the index now so a misconfigured directory fails at startup.
    pub fn open(&self) -> Result<(), ToolError> {
        self.with_connection(|_| Ok(()))
    }

    /// Bring the index up to date with the workspace, one project at a time.
    pub fn pass(&self, workspace: &Workspace, kind: PassKind) -> Result<PassReport, ToolError> {
        let _one_pass = self.passes.lock().unwrap_or_else(PoisonError::into_inner);
        let started_unix_ms = super::unix_ms();
        let ids = project_ids(workspace)?;
        let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let (known, removed) = self.with_connection(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if kind == PassKind::Rebuild {
                transaction.execute_batch(
                    "DELETE FROM facets; DELETE FROM project_text;
                     DELETE FROM project_names; DELETE FROM projects;",
                )?;
            }
            let rows = transaction
                .prepare("SELECT id, project_id, input_hash, error IS NOT NULL FROM projects")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut known = HashMap::new();
            let mut removed = 0;
            for (row, project_id, hash, failed) in rows {
                if present.contains(project_id.as_str()) {
                    known.insert(project_id, (hash, failed));
                } else {
                    remove(&transaction, row)?;
                    removed += 1;
                }
            }
            transaction.commit()?;
            Ok((known, removed))
        })?;
        let mut report = PassReport {
            kind,
            projects: ids.len(),
            rebuilt: 0,
            unchanged: 0,
            failed: 0,
            removed,
            stopped: false,
            started_unix_ms,
            finished_unix_ms: started_unix_ms,
        };
        for id in &ids {
            if self.stop.is_cancelled() {
                report.stopped = true;
                break;
            }
            let prior = known.get(id);
            let retry = kind == PassKind::Retry && prior.is_some_and(|(_, failed)| *failed);
            let gathered = Inputs::gather(workspace, id);
            // An unreadable record has no input hash; it is retried when it
            // becomes readable or on request.
            let hash = gathered
                .as_ref()
                .map_or_else(|_| String::new(), Inputs::hash);
            if !retry && prior.is_some_and(|(known, _)| *known == hash) {
                report.unchanged += 1;
                continue;
            }
            match gathered.and_then(|inputs| Entry::new(workspace, &inputs)) {
                Ok(entry) => {
                    self.with_connection(|connection| {
                        store(connection, id, &hash, &entry)?;
                        if let Some(problem) = &entry.summary_problem {
                            connection.execute(
                                "UPDATE projects SET error = ?2 WHERE project_id = ?1",
                                params![id, problem],
                            )?;
                        }
                        Ok(())
                    })?;
                    if entry.summary_problem.is_some() {
                        tracing::warn!(project_id = %id, "could not record an older slice's summary for its card");
                        report.failed += 1;
                    } else {
                        report.rebuilt += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(project_id = %id, code = error.code(), "could not index a project card");
                    self.with_connection(|connection| {
                        Ok(store_failure(connection, id, &hash, &error)?)
                    })?;
                    report.failed += 1;
                }
            }
        }
        report.finished_unix_ms = super::unix_ms();
        *self
            .last_pass
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(report.clone());
        Ok(report)
    }

    /// Run one pass on the blocking pool.
    pub async fn run(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        kind: PassKind,
    ) -> Result<PassReport, ToolError> {
        // The turn moves into the pass, so a dropped request still lets its
        // pass finish before the next one starts.
        let turn = Arc::clone(&self.turns)
            .acquire_owned()
            .await
            .map_err(|_| ToolError::Validation("server is shutting down".to_owned()))?;
        let index = Arc::clone(self);
        let workspace = Arc::clone(workspace);
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            index.pass(&workspace, kind)
        })
        .await
        .map_err(|error| ToolError::Io(std::io::Error::other(error)))?
    }

    pub fn search(
        &self,
        workspace: &Workspace,
        params: SearchParams,
    ) -> Result<SearchResponse, ToolError> {
        let query = Query::new(params)?;
        let ids = project_ids(workspace)?;
        let (coverage, page) = self.with_connection(|connection| {
            let coverage = coverage(connection, &ids)?;
            let page = if query.words.is_empty() {
                query.run(connection, Matching::All)?
            } else {
                let page = query.run(connection, Matching::Words)?;
                if page.total == 0 && !query.pieces().is_empty() {
                    query.run(connection, Matching::Near)?
                } else {
                    page
                }
            };
            Ok((coverage, page))
        })?;
        let next = query.offset + page.results.len();
        Ok(SearchResponse {
            next_offset: (!page.results.is_empty() && next < page.total && next <= MAX_OFFSET)
                .then_some(next),
            results: page.results,
            total: page.total,
            degraded: degraded(coverage, self.interval.is_some()),
            coverage,
        })
    }

    pub fn status(&self, workspace: &Workspace) -> Result<IndexStatus, ToolError> {
        let ids = project_ids(workspace)?;
        let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let (coverage, failures) = self.with_connection(|connection| {
            let coverage = coverage(connection, &ids)?;
            let failures = connection
                .prepare(
                    "SELECT project_id, error FROM projects WHERE error IS NOT NULL
                     ORDER BY project_id",
                )?
                .query_map([], |row| {
                    Ok(IndexFailure {
                        project_id: row.get(0)?,
                        error: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((coverage, failures))
        })?;
        Ok(IndexStatus {
            storage: self.storage_kind(),
            projects: coverage.total,
            indexed: coverage.indexed,
            pending: coverage.total - coverage.indexed - coverage.failed,
            failed: coverage.failed,
            failures: failures
                .into_iter()
                .filter(|failure| present.contains(failure.project_id.as_str()))
                .take(MAX_LISTED_FAILURES)
                .collect(),
            interval_seconds: self.interval.map(|interval| interval.as_secs()),
            last_pass: self
                .last_pass
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        })
    }
}

/// Keep the index current until the server shuts down. Without an interval
/// only explicit rebuild and retry requests update it.
pub fn spawn(index: Arc<ProjectIndex>, workspace: Arc<Workspace>, cancel: CancellationToken) {
    tokio::spawn(async move {
        if let Some(period) = index.interval {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticks.tick() => {}
                }
                tokio::select! {
                    () = cancel.cancelled() => break,
                    result = index.run(&workspace, PassKind::Incremental) => match result {
                        Ok(report) if report.rebuilt + report.failed + report.removed > 0 => {
                            tracing::info!(
                                rebuilt = report.rebuilt,
                                failed = report.failed,
                                removed = report.removed,
                                "updated the project search index"
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(code = error.code(), error = %error, "project index pass failed");
                        }
                    },
                }
            }
        } else {
            cancel.cancelled().await;
        }
        index.stop.cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::card::tests::{download, print, slice, tag};
    use serde_json::{Value, json};

    fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        (root, Arc::new(workspace))
    }

    /// A version 2 project record with a chosen creation time.
    fn record(workspace: &Workspace, id: &str, name: &str, description: &str, created: u64) {
        workspace
            .write_reserved_artifact(
                &format!(".printable/projects/{id}.json"),
                &serde_json::to_vec(&json!({
                    "format_version": 2, "project_id": id, "name": name,
                    "description": description, "root": format!("projects/{id}"),
                    "record_version": 1, "created_unix_ms": created, "updated_unix_ms": created,
                }))
                .unwrap(),
                true,
            )
            .unwrap();
    }

    /// Four projects that differ in every searchable way.
    fn library(workspace: &Workspace) {
        record(workspace, "clip", "Cable clip", "Holds desk cables", 1_000);
        record(
            workspace,
            "desk",
            "Desk organizer",
            "Three drawers for a desk clip collection",
            2_000,
        );
        record(workspace, "vase", "Spiral vase", "A tall vase", 3_000);
        record(workspace, "hook", "Wall hook", "Hangs a bag", 4_000);
        tag(workspace, "vase", &["Decor"]);
        download(workspace, "vase", "printables", "Twisted vase", 1_500);
        slice(workspace, "desk", "slice-1", 5_000);
        workspace
            .write_artifact("projects/desk/desk.3mf", b"3mf", false)
            .unwrap();
        print(workspace, "desk", 1, "completed", 9_000);
        print(workspace, "clip", 2, "completed", 8_000);
        print(workspace, "hook", 3, "failed", 7_000);
    }

    fn indexed() -> (tempfile::TempDir, Arc<Workspace>, ProjectIndex) {
        let (root, workspace) = workspace();
        library(&workspace);
        let index = ProjectIndex::new(Storage::Memory, None);
        let report = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((report.projects, report.rebuilt, report.failed), (4, 4, 0));
        (root, workspace, index)
    }

    fn search(index: &ProjectIndex, workspace: &Workspace, request: Value) -> SearchResponse {
        index
            .search(workspace, serde_json::from_value(request).unwrap())
            .unwrap()
    }

    fn ids(response: &SearchResponse) -> Vec<&str> {
        response
            .results
            .iter()
            .map(|result| result.card.project_id.as_str())
            .collect()
    }

    fn validator() -> jsonschema::Validator {
        let schema = Value::Object(crate::tools::output::schema("project").as_ref().clone());
        jsonschema::validator_for(&schema).unwrap()
    }

    #[test]
    fn name_matches_outrank_description_matches() {
        let (_root, workspace, index) = indexed();
        let clip = search(&index, &workspace, json!({"text": "clip"}));
        assert_eq!(ids(&clip), ["clip", "desk"]);
        assert!(clip.results[0].score > clip.results[1].score);
        assert_eq!(clip.results[0].matched, Some(MatchKind::Words));
        assert_eq!(
            ids(&search(&index, &workspace, json!({"text": "DESK"}))),
            ["desk", "clip"]
        );
        assert_eq!(
            ids(&search(&index, &workspace, json!({"text": "desk drawers"}))),
            ["desk"]
        );
        // Words match the start of a word, so a stem finds longer forms.
        assert_eq!(
            ids(&search(&index, &workspace, json!({"text": "organ"}))),
            ["desk"]
        );
        // Download titles, file names and printers are searchable too.
        for (text, expected) in [
            ("twisted", "vase"),
            ("desk.3mf", "desk"),
            ("workshop", "clip"),
        ] {
            assert_eq!(
                ids(&search(&index, &workspace, json!({"text": text})))[0],
                expected,
                "{text}"
            );
        }
        let response = serde_json::to_value(&clip).unwrap();
        assert!(validator().is_valid(&response), "{response}");
        assert_eq!(clip.degraded.len(), 2);
        assert_eq!(
            clip.coverage,
            Coverage {
                indexed: 4,
                total: 4,
                failed: 0
            }
        );
    }

    #[test]
    fn every_filter_narrows_the_results() {
        let (_root, workspace, index) = indexed();
        for (filters, expected) in [
            (json!({"printed": true}), &["desk", "clip"][..]),
            (json!({"printed": false}), &["hook", "vase"][..]),
            (
                json!({"printer": "WORKSHOP x1c"}),
                &["hook", "desk", "clip"][..],
            ),
            (json!({"printer": "Bambu Lab P1S"}), &["desk"][..]),
            (json!({"has_3mf": true}), &["desk"][..]),
            (json!({"has_3mf": false}), &["hook", "vase", "clip"][..]),
            (json!({"material": "petg"}), &["desk"][..]),
            (json!({"material": "PLA"}), &["hook", "desk", "clip"][..]),
            (json!({"colour": "00aa00"}), &["desk"][..]),
            (json!({"colour": "#FF8800"}), &["hook", "desk", "clip"][..]),
            (json!({"tag": "decor"}), &["vase"][..]),
            (json!({"source": "Printables"}), &["vase"][..]),
            (json!({"license": "cc by 4.0"}), &["vase"][..]),
            (json!({"format": "3mf"}), &["desk"][..]),
            (json!({"printed": true, "material": "PETG"}), &["desk"][..]),
            (json!({"tag": "missing"}), &[][..]),
        ] {
            let response = search(&index, &workspace, json!({"filters": filters}));
            assert_eq!(ids(&response), expected, "{filters}");
            assert_eq!(response.total, expected.len());
        }
        let printed = search(
            &index,
            &workspace,
            json!({"text": "desk", "filters": {"printed": true}}),
        );
        assert_eq!(ids(&printed), ["desk", "clip"]);
        for filters in [json!({"tag": " "}), json!({"material": "x".repeat(129)})] {
            let error = index
                .search(
                    &workspace,
                    serde_json::from_value(json!({"filters": filters})).unwrap(),
                )
                .unwrap_err();
            assert_eq!(error.code(), "validation");
        }
    }

    #[test]
    fn sorts_and_pages_are_stable() {
        let (_root, workspace, index) = indexed();
        for (sort, expected) in [
            ("newest", ["hook", "vase", "desk", "clip"]),
            ("name", ["clip", "desk", "vase", "hook"]),
            ("last_printed", ["desk", "clip", "vase", "hook"]),
        ] {
            assert_eq!(
                ids(&search(&index, &workspace, json!({"sort": sort}))),
                expected,
                "{sort}"
            );
        }
        assert_eq!(
            ids(&search(
                &index,
                &workspace,
                json!({"text": "desk", "sort": "name"})
            )),
            ["clip", "desk"]
        );
        let first = search(&index, &workspace, json!({"limit": 3}));
        assert_eq!((first.total, first.next_offset), (4, Some(3)));
        assert_eq!(first.results[0].matched, None);
        assert_eq!(first.results[0].score, None);
        let last = search(&index, &workspace, json!({"limit": 3, "offset": 3}));
        assert_eq!(ids(&last), ["clip"]);
        assert_eq!(last.next_offset, None);
        let beyond = search(&index, &workspace, json!({"offset": 1000}));
        assert!(beyond.results.is_empty());
        assert_eq!((beyond.total, beyond.next_offset), (4, None));
        for request in [
            json!({"limit": 0}),
            json!({"limit": 51}),
            json!({"offset": 1001}),
            json!({"sort": "best_match"}),
            json!({"text": "x".repeat(257)}),
            json!({"text": "a b c d e f g h i j k l m n o p q"}),
            json!({"text": "*** ()"}),
        ] {
            let error = index
                .search(&workspace, serde_json::from_value(request.clone()).unwrap())
                .unwrap_err();
            assert_eq!(error.code(), "validation", "{request}");
        }
    }

    #[test]
    fn misspelt_names_find_near_matches_only_when_words_find_nothing() {
        let (_root, workspace, index) = indexed();
        let near = search(&index, &workspace, json!({"text": "spirl vase"}));
        assert_eq!(ids(&near), ["vase"]);
        assert_eq!(near.results[0].matched, Some(MatchKind::Near));
        assert_eq!(near.results[0].score, Some(0.8));
        let organiser = search(&index, &workspace, json!({"text": "orgnizer"}));
        assert_eq!(ids(&organiser), ["desk"]);
        assert_eq!(organiser.results[0].matched, Some(MatchKind::Near));
        // An exact word match suppresses near matches.
        let exact = search(&index, &workspace, json!({"text": "vase"}));
        assert_eq!(ids(&exact), ["vase"]);
        assert_eq!(exact.results[0].matched, Some(MatchKind::Words));
        // Too few shared pieces is no match, and short words have no pieces.
        assert_eq!(
            search(&index, &workspace, json!({"text": "spoon"})).total,
            0
        );
        assert_eq!(search(&index, &workspace, json!({"text": "zz"})).total, 0);
        let filtered = search(
            &index,
            &workspace,
            json!({"text": "spirl", "filters": {"printed": true}}),
        );
        assert_eq!(filtered.total, 0);
    }

    #[test]
    fn hostile_text_stays_literal() {
        let (_root, workspace, index) = indexed();
        // Read as FTS syntax, OR would add the clip project.
        assert_eq!(
            ids(&search(
                &index,
                &workspace,
                json!({"text": "\"desk\" OR clip"})
            )),
            ["desk"]
        );
        for text in [
            "name:* OR \"x",
            "desk\"",
            "NEAR(desk clip)",
            "-desk ^clip",
            "{name tags}: desk",
            "'; DROP TABLE projects; --",
            "desk AND NOT clip",
            "\u{0}desk",
        ] {
            search(&index, &workspace, json!({"text": text}));
        }
        assert_eq!(search(&index, &workspace, json!({})).total, 4);
    }

    #[test]
    fn passes_rebuild_only_changed_projects_and_remove_deleted_ones() {
        let (_root, workspace, index) = indexed();
        let again = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((again.rebuilt, again.unchanged, again.removed), (0, 4, 0));
        workspace
            .write_artifact("projects/vase/vase.stl", b"solid vase", false)
            .unwrap();
        let changed = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((changed.rebuilt, changed.unchanged), (1, 3));
        assert_eq!(
            ids(&search(
                &index,
                &workspace,
                json!({"filters": {"format": "stl"}})
            )),
            ["vase"]
        );
        assert!(
            workspace
                .remove_reserved_artifact(".printable/projects/hook.json")
                .unwrap()
        );
        let removed = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((removed.projects, removed.removed), (3, 1));
        assert!(!ids(&search(&index, &workspace, json!({}))).contains(&"hook"));
        let rebuilt = index.pass(&workspace, PassKind::Rebuild).unwrap();
        assert_eq!((rebuilt.rebuilt, rebuilt.unchanged), (3, 0));
        let status = index.status(&workspace).unwrap();
        assert_eq!(
            (
                status.projects,
                status.indexed,
                status.pending,
                status.failed
            ),
            (3, 3, 0, 0)
        );
        assert_eq!(status.last_pass, Some(rebuilt));
        assert_eq!(status.storage, StorageKind::Memory);
        assert_eq!(status.interval_seconds, None);
        assert!(validator().is_valid(&serde_json::to_value(&status).unwrap()));
    }

    #[test]
    fn unreadable_projects_are_reported_and_retried_on_request() {
        let (_root, workspace) = workspace();
        library(&workspace);
        workspace
            .write_reserved_artifact(".printable/projects/bad.json", b"{broken", false)
            .unwrap();
        let index = ProjectIndex::new(Storage::Memory, None);
        let before = search(&index, &workspace, json!({}));
        assert_eq!(before.coverage.indexed, 0);
        let incomplete = before.degraded.last().unwrap();
        assert_eq!(incomplete.code, "index_incomplete");
        assert!(
            incomplete.message.contains("run index_rebuild"),
            "{incomplete:?}"
        );
        let first = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((first.rebuilt, first.failed), (4, 1));
        let quiet = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((quiet.failed, quiet.unchanged), (0, 5));
        let status = index.status(&workspace).unwrap();
        assert_eq!((status.failed, status.pending), (1, 0));
        assert_eq!(status.failures[0].project_id, "bad");
        assert!(status.failures[0].error.starts_with("serde:"));
        let response = search(&index, &workspace, json!({}));
        assert_eq!(
            response.coverage,
            Coverage {
                indexed: 4,
                total: 5,
                failed: 1
            }
        );
        assert!(response.degraded[2].message.contains("index_retry"));
        let retried = index.pass(&workspace, PassKind::Retry).unwrap();
        assert_eq!((retried.failed, retried.unchanged), (1, 4));
        record(&workspace, "bad", "Mended", "", 5_000);
        let mended = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((mended.rebuilt, mended.failed), (1, 0));
        assert_eq!(index.status(&workspace).unwrap().failed, 0);
        assert_eq!(search(&index, &workspace, json!({})).degraded.len(), 2);
    }

    #[test]
    fn a_deleted_index_file_is_rebuilt_with_identical_results() {
        let (_root, workspace) = workspace();
        library(&workspace);
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::Directory(directory.path().to_path_buf());
        let requests = [
            json!({"text": "desk"}),
            json!({"text": "spirl vase"}),
            json!({"filters": {"printed": true}, "sort": "last_printed"}),
            json!({"sort": "name", "limit": 2, "offset": 1}),
        ];
        let run = |index: &ProjectIndex| -> Vec<SearchResponse> {
            requests
                .iter()
                .map(|request| search(index, &workspace, request.clone()))
                .collect()
        };
        let first = ProjectIndex::new(storage.clone(), None);
        first.pass(&workspace, PassKind::Incremental).unwrap();
        let expected = run(&first);
        drop(first);
        let reopened = ProjectIndex::new(storage.clone(), None);
        assert_eq!(run(&reopened), expected, "the file keeps the index");
        assert_eq!(
            reopened.status(&workspace).unwrap().storage,
            StorageKind::File
        );
        drop(reopened);
        for suffix in ["", "-wal", "-shm"] {
            let path = directory.path().join(format!("{FILE_NAME}{suffix}"));
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        let rebuilt = ProjectIndex::new(storage, None);
        assert_eq!(search(&rebuilt, &workspace, json!({})).coverage.indexed, 0);
        let report = rebuilt.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!(report.rebuilt, 4);
        assert_eq!(run(&rebuilt), expected);
        let missing = ProjectIndex::new(Storage::Directory(directory.path().join("missing")), None);
        assert_eq!(missing.open().unwrap_err().code(), "project_index");
    }

    #[test]
    fn the_default_index_lives_in_memory() {
        let (_root, workspace) = workspace();
        library(&workspace);
        let index = ProjectIndex::default();
        index.open().unwrap();
        index.pass(&workspace, PassKind::Incremental).unwrap();
        let status = index.status(&workspace).unwrap();
        assert_eq!(status.storage, StorageKind::Memory);
        assert_eq!(status.interval_seconds, Some(30));
        assert_eq!(search(&index, &workspace, json!({})).total, 4);
    }

    #[test]
    fn settings_choose_storage_and_validate_the_interval() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let default = configure(&lookup(&[])).unwrap();
        assert_eq!(default.storage, Storage::Memory);
        assert_eq!(default.interval, Some(Duration::from_secs(30)));
        let file = configure(&lookup(&[
            (DIR_KEY, "/var/lib/printable-index"),
            (INTERVAL_KEY, "0"),
        ]))
        .unwrap();
        assert_eq!(
            file.storage,
            Storage::Directory(PathBuf::from("/var/lib/printable-index"))
        );
        assert_eq!(file.interval, None);
        assert_eq!(
            configure(&lookup(&[(INTERVAL_KEY, "86400")]))
                .unwrap()
                .interval,
            Some(Duration::from_secs(86_400))
        );
        for (key, value) in [
            (DIR_KEY, "relative/index"),
            (INTERVAL_KEY, "86401"),
            (INTERVAL_KEY, "-1"),
            (INTERVAL_KEY, "soon"),
        ] {
            let pairs: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(key, value)].into_boxed_slice());
            assert_eq!(
                configure(&lookup(pairs)).unwrap_err(),
                SettingsError::ProjectIndex(key)
            );
        }
        let error = crate::config::Settings::from_lookup(|key| match key {
            "PRINTABLE_MCP_BEARER" => Some("0".repeat(64)),
            INTERVAL_KEY => Some("never".into()),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(error, SettingsError::ProjectIndex(INTERVAL_KEY));
    }

    /// Call one `project` action through the MCP handler and check its
    /// structured result against the published output schema.
    async fn call(server: &crate::mcp::PrintableServer, action: &str, params: Value) -> Value {
        let Value::Object(arguments) = json!({"action": action, "params": params}) else {
            unreachable!("the request is an object")
        };
        let result = server
            .invoke_tool(
                rmcp::model::CallToolRequestParams::new("project").with_arguments(arguments),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{action}: {result:?}");
        let value = result.structured_content.unwrap();
        assert!(validator().is_valid(&value), "{action}: {value}");
        value
    }

    #[tokio::test]
    async fn the_project_tool_routes_card_search_and_index_actions() {
        let (_root, workspace) = workspace();
        library(&workspace);
        let settings = crate::config::Settings::from_lookup(|key| {
            (key == "PRINTABLE_MCP_BEARER").then(|| "0".repeat(64))
        })
        .unwrap();
        let server = crate::mcp::PrintableServer::new(
            Arc::clone(&workspace),
            Arc::new(printable_blender::BlenderClient::new(
                "127.0.0.1",
                9,
                Default::default(),
            )),
            Arc::new(settings),
        );
        let pending = call(&server, "index_status", json!({})).await;
        assert_eq!(
            (pending["pending"].as_u64(), pending["storage"].as_str()),
            (Some(4), Some("memory"))
        );
        let rebuilt = call(&server, "index_rebuild", json!({})).await;
        assert_eq!(rebuilt["rebuilt"], 4);
        let retried = call(&server, "index_retry", json!({})).await;
        assert_eq!(retried["kind"], "retry");
        let found = call(&server, "search", json!({"text": "spiral", "limit": 5})).await;
        assert_eq!(found["results"][0]["project_id"], "vase");
        assert_eq!(found["results"][0]["match"], "words");
        assert_eq!(found["degraded"][0]["code"], "meaning_search_unavailable");
        let card = call(&server, "card", json!({"project_id": "desk"})).await;
        assert_eq!(
            card["slices"]["newest"]["process"],
            "0.20mm Standard @BBL P1S"
        );
        assert_eq!(card["prints"]["outcomes"]["completed"], 1);
        for (action, operation) in [
            ("search", "printable_project_search"),
            ("index_status", "printable_project_index_status"),
            ("index_rebuild", "printable_project_index_rebuild"),
            ("index_retry", "printable_project_index_retry"),
            ("card", "printable_project"),
        ] {
            let params = if action == "card" {
                json!({"project_id": "desk"})
            } else {
                json!({})
            };
            let resolved = crate::tools::workflows::resolve(
                "project",
                json!({"action": action, "params": params}),
            )
            .unwrap();
            assert_eq!(resolved.name, operation);
        }
    }
}
