//! The derived search index over project library cards.
//!
//! The index holds nothing that cannot be rebuilt from workspace records.
//! Each pass lists the project records, hashes each project's inputs from
//! metadata, rebuilds the cards whose inputs changed, and removes entries for
//! projects that no longer exist. Deleting the index file loses nothing: the
//! next start creates an empty index and the first pass fills it again.
//!
//! With an embeddings service configured, each pass then asks it for vectors
//! of card text and pictures that lack current ones, a bounded share per pass.
//! Vectors are kept with the identity of the model that made them: the
//! configured settings, the model name the service reports, the vector length
//! and the vector of a fixed probe text. When any of these changes, every
//! vector is discarded and made again, so a search never compares vectors from
//! two models.
//!
//! Search text never reaches SQLite as query syntax. It is split into words,
//! each word becomes a quoted FTS5 string, and every value is a bound
//! parameter. The SQLite connection is never held across a network request.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use printable_workspace::{Workspace, WsError};
use rusqlite::{
    Connection, OptionalExtension, TransactionBehavior, params, params_from_iter,
    types::Value as Sql,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{
    card::{self, CompactCard, Inputs, PictureSource},
    cover,
    embeddings::{self, Embedded, Embeddings, Input, Reranker, ServiceError},
};
use crate::{
    config::SettingsError,
    error::ToolError,
    tools::{blocking, blocking_with},
};

const DIR_KEY: &str = "PRINTABLE_PROJECT_INDEX_DIR";
const INTERVAL_KEY: &str = "PRINTABLE_PROJECT_INDEX_INTERVAL_SECONDS";
const DEFAULT_INTERVAL_SECONDS: u64 = 30;
const MAX_INTERVAL_SECONDS: u64 = 86_400;
const FILE_NAME: &str = "index.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Stored as SQLite's `user_version`; a different value recreates the tables.
const SCHEMA_VERSION: i64 = 2;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
pub(crate) const MAX_OFFSET: usize = 1000;
const MAX_TEXT_CHARS: usize = 256;
const MAX_WORDS: usize = 16;
const MAX_FILTER_CHARS: usize = 128;
const MAX_PICTURE_PATH_CHARS: usize = 1024;
/// Three-character pieces compared when words match nothing.
const MAX_PIECES: usize = 64;
const MAX_LISTED_FAILURES: usize = 16;
const MAX_ERROR_CHARS: usize = 512;
/// bm25 weights for the name, tags, description, source and files columns.
const WEIGHTS: &str = "10.0, 5.0, 3.0, 2.0, 1.0";
/// Project texts one pass embeds.
const MAX_TEXTS_PER_PASS: usize = 256;
/// Project pictures one pass embeds.
const MAX_PICTURES_PER_PASS: usize = 32;
/// Pictures sent in one request.
const PICTURE_BATCH: usize = 4;
/// A pass starts no embedding request after this long.
const EMBEDDING_BUDGET: Duration = Duration::from_secs(120);
/// The wait before a failed vector is tried again; it doubles with each
/// further failure, up to a day.
const FIRST_RETRY_MS: u64 = 5 * 60 * 1000;
const MAX_RETRY_MS: u64 = 24 * 60 * 60 * 1000;
/// Embedded at the start of each pass to notice a different model behind an
/// unchanged name.
const PROBE_TEXT: &str = "A small 3D printed part: a clip that holds cables under a desk.";
/// The largest drop in cosine similarity between two probe vectors that still
/// counts as the same model.
const PROBE_TOLERANCE: f64 = 1e-3;
/// Projects that meaning and picture search each add to the word matches.
const MAX_SIMILAR: usize = 50;
/// Word matches that take part in combining.
const MAX_WORD_CANDIDATES: usize = 2000;
/// Leading results sent to the re-ranker.
const RERANK_DEPTH: usize = 40;
/// Reciprocal rank fusion's constant.
const RANK_CONSTANT: f64 = 60.0;
const PICTURE_LENGTH_DIFFERS: &str =
    "the embeddings service returned picture vectors of another length than text vectors";

fn pictures_refused_with(status: u16) -> String {
    format!("the embeddings service refused pictures with HTTP status {status}")
}

const MODEL_CHANGED: &str = "the embeddings service now reports a different model; the next background pass embeds every project again";

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
    error TEXT,
    meaning_text TEXT,
    text_key TEXT,
    picture TEXT,
    picture_key TEXT
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
CREATE TABLE vectors (
    project INTEGER NOT NULL,
    kind TEXT NOT NULL,
    source_key TEXT NOT NULL,
    vector BLOB,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_unix_ms INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    PRIMARY KEY (project, kind)
) WITHOUT ROWID;
CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    settings TEXT NOT NULL,
    service_model TEXT,
    dimensions INTEGER NOT NULL,
    probe BLOB NOT NULL,
    pictures_refused TEXT
);
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
    passes: Arc<tokio::sync::Mutex<()>>,
    last_pass: Mutex<Option<PassReport>>,
    /// Stops a running pass between projects at shutdown.
    stop: CancellationToken,
    /// The embeddings service behind meaning and picture search.
    embeddings: Option<Embeddings>,
    /// The re-ranking service.
    reranker: Option<Reranker>,
    /// What the latest contacts with those services found.
    services: Mutex<ServiceState>,
}

impl std::fmt::Debug for ProjectIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectIndex")
            .field("storage", &self.storage_kind())
            .field("interval", &self.interval)
            .field("embeddings", &self.embeddings)
            .field("reranker", &self.reranker)
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
    let services = embeddings::configure(get)?;
    let mut index = ProjectIndex::new(storage, (seconds > 0).then(|| Duration::from_secs(seconds)));
    index.embeddings = services.embeddings;
    index.reranker = services.reranker;
    Ok(Arc::new(index))
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
    /// What the pass embedded; null without an embeddings service.
    pub embedding: Option<EmbeddingReport>,
}

/// What one pass embedded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddingReport {
    /// Project texts embedded.
    pub texts: usize,
    /// Project pictures embedded.
    pub pictures: usize,
    /// Project texts and pictures that could not be embedded; each is tried
    /// again later.
    pub failed: usize,
    /// True when the model identity changed, so every vector is made again.
    pub model_changed: bool,
    /// True when no request was made because the service asked to wait.
    pub deferred: bool,
    /// True when the pass's time ran out or the server stopped; later passes
    /// continue.
    pub stopped: bool,
    /// Why the service could not be used in this pass.
    pub problem: Option<String>,
}

/// Request body for `index_status`, `index_rebuild` and `index_retry`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Best match first, combining every available search method; requires
    /// `text` or `picture`.
    BestMatch,
    /// Most recently printed first; never-printed projects last.
    LastPrinted,
    /// Most recently created first.
    Newest,
    /// By name.
    Name,
}

/// How `best_match` combines the scores of words, meaning and picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    /// Standardise each method's scores across the results and add them.
    #[default]
    Standardised,
    /// Add 1 / (60 + rank) for each method that found the result.
    ReciprocalRank,
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
    /// Every word must match the start of a word in the project. With an
    /// embeddings service, best_match also finds projects by what the text
    /// means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Workspace path of a PNG, JPEG or WebP picture: best_match finds
    /// projects whose cover or slice thumbnail looks like it. Needs picture
    /// search; index_status reports whether it is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(default)]
    pub filters: SearchFilters,
    /// Defaults to best_match with text or a picture, and newest without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SearchSort>,
    /// How best_match combines several methods' scores.
    #[serde(default)]
    pub combine: Combine,
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

/// How a result was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Every word matched.
    Words,
    /// No project matched every word; the name shares at least half of the
    /// words' three-character pieces.
    Near,
    /// Among the projects closest in meaning to the text.
    Meaning,
    /// Among the projects whose picture is closest to the query picture.
    Picture,
}

/// Each search method's own score for a result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MethodScores {
    /// bm25 word relevance, or the share of matching pieces for a near match.
    pub words: Option<f64>,
    /// Cosine similarity of the text's and the project's meaning vectors.
    pub meaning: Option<f64>,
    /// Cosine similarity of the query picture's and the project picture's
    /// vectors.
    pub picture: Option<f64>,
    /// The re-ranker's relevance score.
    pub rerank: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    #[serde(flatten)]
    pub card: CompactCard,
    /// Null without search text or a picture.
    #[serde(rename = "match")]
    pub matched: Option<MatchKind>,
    /// Higher is better: a bm25 relevance for word matches, the share of
    /// matching pieces for near matches, or the combined score when
    /// best_match used several methods; null without search text or a
    /// picture.
    pub score: Option<f64>,
    /// Each method's score, present when best_match used meaning, picture or
    /// re-ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scores: Option<MethodScores>,
}

/// Why a search capability is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Its settings are not set.
    NotConfigured,
    /// `PRINTABLE_EMBEDDING_IMAGES` is off.
    TurnedOff,
    /// The embeddings model refused pictures.
    PicturesRejected,
    /// No vectors from the configured model exist yet.
    NotReady,
    /// The service could not be reached or failed.
    Unreachable,
    /// The service asked to wait.
    Busy,
    /// The service refused the request; a setting is probably wrong.
    Refused,
    /// The service's answer could not be used.
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Degradation {
    pub code: String,
    pub message: String,
    /// Why a search method is unavailable; absent for other notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
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

/// Which optional search methods are available now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchMethods {
    /// Searching by what search text means.
    pub meaning: bool,
    /// Searching by a picture.
    pub picture: bool,
    /// Re-ranking the best results.
    pub rerank: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VectorCounts {
    /// Projects with a vector of their current text or picture.
    pub embedded: usize,
    /// Projects waiting for one.
    pub pending: usize,
    /// Projects whose latest attempt failed; they are tried again later.
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VectorFailure {
    pub project_id: String,
    /// `text` or `picture`.
    pub kind: String,
    pub error: String,
    /// When a background pass tries again; `index_retry` tries now.
    pub retry_unix_ms: u64,
}

/// The embeddings model and its vectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddingStatus {
    /// The configured model.
    pub model: String,
    /// The model name the service reported; null until it has answered.
    pub service_model: Option<String>,
    /// The vector length; null until the service has answered.
    pub dimensions: Option<usize>,
    /// The operator's revision label.
    pub revision: Option<String>,
    /// Vectors of project text.
    pub text: VectorCounts,
    /// Vectors of project pictures, counting projects with a cover or a
    /// slice thumbnail; null when pictures are not embedded.
    pub pictures: Option<VectorCounts>,
    /// Up to 16 failed vectors, by project ID.
    pub failures: Vec<VectorFailure>,
    /// The service asked for no background requests before this time.
    pub retry_after_unix_ms: Option<u64>,
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
    /// Optional search methods available now.
    pub methods: SearchMethods,
    /// Why each unavailable optional method is unavailable.
    pub unavailable: Vec<Degradation>,
    /// Meaning and picture vectors; null without an embeddings service.
    pub embeddings: Option<EmbeddingStatus>,
}

fn sql_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Validation(message.into())
}

fn clipped(message: &str) -> &str {
    match message.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => &message[..end],
        None => message,
    }
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
    /// The text embedded for meaning search, and its digest.
    meaning: (String, String),
    /// The picture source as JSON, and a key that changes with the picture.
    picture: Option<(String, String)>,
}

impl Entry {
    fn new(workspace: &Workspace, inputs: &Inputs) -> Result<Self, ToolError> {
        let card = card::build(workspace, inputs, card::Summaries::RecordMissing);
        let text = card.search_text(inputs);
        let meaning = text.meaning();
        let picture = card
            .picture(inputs)
            .map(|(source, key)| serde_json::to_string(&source).map(|source| (source, key)))
            .transpose()?;
        Ok(Self {
            compact: serde_json::to_string(&card.compact())?,
            name_folded: card.name.to_lowercase(),
            created_unix_ms: card.dates.created_unix_ms,
            last_printed_unix_ms: card.dates.last_printed_unix_ms,
            printed: card.prints.outcomes.completed > 0,
            has_3mf: card.has_3mf,
            facets: card.facets().into_iter().collect(),
            text,
            meaning: (crate::provenance::digest(meaning.as_bytes()), meaning),
            picture,
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
             DROP TABLE IF EXISTS project_names;
             DROP TABLE IF EXISTS vectors;
             DROP TABLE IF EXISTS identity;",
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
    connection.execute("DELETE FROM vectors WHERE project = ?1", [row])?;
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
    let (text_key, meaning_text) = &entry.meaning;
    let (picture, picture_key) = match &entry.picture {
        Some((source, key)) => (Some(source), Some(key)),
        None => (None, None),
    };
    let row: i64 = transaction.query_row(
        "INSERT INTO projects (project_id, input_hash, card, name_folded, created_unix_ms,
             last_printed_unix_ms, printed, has_3mf, error, meaning_text, text_key, picture,
             picture_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?11, ?12)
         ON CONFLICT (project_id) DO UPDATE SET input_hash = excluded.input_hash,
             card = excluded.card, name_folded = excluded.name_folded,
             created_unix_ms = excluded.created_unix_ms,
             last_printed_unix_ms = excluded.last_printed_unix_ms,
             printed = excluded.printed, has_3mf = excluded.has_3mf, error = NULL,
             meaning_text = excluded.meaning_text, text_key = excluded.text_key,
             picture = excluded.picture, picture_key = excluded.picture_key
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
            meaning_text,
            text_key,
            picture,
            picture_key,
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
    connection.execute(
        "INSERT INTO projects (project_id, input_hash, error) VALUES (?1, ?2, ?3)
         ON CONFLICT (project_id) DO UPDATE SET input_hash = excluded.input_hash,
             error = excluded.error",
        params![project_id, hash, clipped(&message)],
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

/// The note for projects missing from the index, if any are.
fn incomplete(coverage: Coverage, background: bool) -> Option<Degradation> {
    (coverage.indexed < coverage.total).then(|| {
        let pending = coverage.total - coverage.indexed - coverage.failed;
        let next = match (coverage.failed > 0, background) {
            (true, _) => "Read index_status for the failures, then run index_retry.",
            (false, true) => "Background indexing continues.",
            (false, false) => "Background indexing is off; run index_rebuild or index_retry.",
        };
        Degradation {
            code: "index_incomplete".into(),
            message: format!(
                "{} of {} projects are indexed; {pending} are waiting and {} could not be indexed. Results cover indexed projects only. {next}",
                coverage.indexed, coverage.total, coverage.failed
            ),
            reason: None,
        }
    })
}

/// An optional search method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    Meaning,
    Picture,
    Rerank,
}

/// The note saying why a search method is unavailable. `detail` completes
/// the sentence when the reason alone does not say enough.
fn note(capability: Capability, reason: Reason, detail: Option<String>) -> Degradation {
    let (code, name) = match capability {
        Capability::Meaning => ("meaning_search_unavailable", "Searching by meaning"),
        Capability::Picture => ("picture_search_unavailable", "Searching by picture"),
        Capability::Rerank => ("rerank_unavailable", "Re-ranking"),
    };
    let message = match (detail, reason, capability) {
        (Some(detail), ..) => format!("{name} is unavailable: {detail}."),
        (None, Reason::NotConfigured, Capability::Meaning) => "Searching by meaning is not configured; it needs an embeddings service (PRINTABLE_EMBEDDING_URL and PRINTABLE_EMBEDDING_MODEL).".to_owned(),
        (None, Reason::NotConfigured, Capability::Picture) => "Searching by picture is not configured; it needs an embeddings service that accepts pictures (PRINTABLE_EMBEDDING_IMAGES=on).".to_owned(),
        (None, Reason::NotConfigured, Capability::Rerank) => "Re-ranking is not configured; results keep their combined order.".to_owned(),
        (None, Reason::TurnedOff, _) => format!("{name} is turned off; set PRINTABLE_EMBEDDING_IMAGES=on when the embeddings model accepts pictures."),
        (None, Reason::NotReady, _) => format!("{name} is unavailable until background indexing has embedded projects with the configured model."),
        (None, ..) => format!("{name} is unavailable."),
    };
    Degradation {
        code: code.to_owned(),
        message,
        reason: Some(reason),
    }
}

fn reason(error: &ServiceError) -> Reason {
    match error {
        ServiceError::Unreachable | ServiceError::Failed(_) => Reason::Unreachable,
        ServiceError::Busy(_) => Reason::Busy,
        ServiceError::Refused(_) => Reason::Refused,
        ServiceError::Incompatible | ServiceError::TooLarge(_) => Reason::Incompatible,
    }
}

/// A quoted FTS5 string. Doubling quotes keeps any text a literal.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn parse_card(card: &str) -> rusqlite::Result<CompactCard> {
    serde_json::from_str(card).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
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
    /// The search text, for meaning search and re-ranking.
    text: Option<String>,
    /// Workspace path of the query picture.
    picture: Option<String>,
    /// Filter SQL fragments and their bound values, in order.
    filters: Vec<(&'static str, Vec<Sql>)>,
    sort: SearchSort,
    combine: Combine,
    limit: usize,
    offset: usize,
}

struct Page {
    results: Vec<SearchResult>,
    total: usize,
}

/// A project and one method's score for it.
#[derive(Debug, Clone)]
struct Scored {
    row: i64,
    project_id: String,
    name_folded: String,
    score: f64,
}

impl Query {
    fn new(params: SearchParams) -> Result<Self, ToolError> {
        if !(1..=MAX_LIMIT).contains(&params.limit) {
            return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")));
        }
        if params.offset > MAX_OFFSET {
            return Err(invalid(format!("offset must be at most {MAX_OFFSET}")));
        }
        let text = params.text;
        let words: Vec<String> = match &text {
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
        let picture = match params.picture {
            Some(path)
                if path.trim().is_empty() || path.chars().count() > MAX_PICTURE_PATH_CHARS =>
            {
                return Err(invalid(format!(
                    "picture must be a workspace path of 1 to {MAX_PICTURE_PATH_CHARS} characters"
                )));
            }
            picture => picture,
        };
        let sort = match params.sort {
            Some(SearchSort::BestMatch) if words.is_empty() && picture.is_none() => {
                return Err(invalid("best_match sorting needs search text or a picture"));
            }
            Some(sort) if sort != SearchSort::BestMatch && picture.is_some() => {
                return Err(invalid("a picture search is ordered by best_match"));
            }
            Some(sort) => sort,
            None if words.is_empty() && picture.is_none() => SearchSort::Newest,
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
            text: text.filter(|_| !words.is_empty()),
            words,
            picture,
            filters,
            sort,
            combine: params.combine,
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

    /// SQL selecting the matching projects with their score, the condition
    /// a near match's score must meet, and the values to bind.
    fn selection(&self, matching: Matching) -> (String, &'static str, Vec<Sql>) {
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
            "SELECT p.id AS id, p.card AS card, {score} AS score, p.name_folded AS name_folded,
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
        (inner, outer, values)
    }

    fn count(
        connection: &Connection,
        inner: &str,
        outer: &str,
        values: &[Sql],
    ) -> rusqlite::Result<usize> {
        let total: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM ({inner}) {outer}"),
            params_from_iter(values.iter()),
            |row| row.get(0),
        )?;
        Ok(usize::try_from(total).unwrap_or(0))
    }

    fn run(&self, connection: &Connection, matching: Matching) -> rusqlite::Result<Page> {
        let (inner, outer, mut values) = self.selection(matching);
        let total = Self::count(connection, &inner, outer, &values)?;
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
        let pieces = self.pieces().len();
        let mut results = Vec::with_capacity(rows.len());
        for (card, score) in rows {
            let (matched, score) = match matching {
                Matching::All => (None, None),
                Matching::Words => (Some(MatchKind::Words), score),
                Matching::Near => (
                    Some(MatchKind::Near),
                    score.map(|shared| shared / pieces as f64),
                ),
            };
            results.push(SearchResult {
                card: parse_card(&card)?,
                matched,
                score,
                scores: None,
            });
        }
        Ok(Page { results, total })
    }

    /// The best `limit` matches by score, and how many match in all.
    fn matches(
        &self,
        connection: &Connection,
        matching: Matching,
        limit: usize,
    ) -> rusqlite::Result<(usize, Vec<Scored>)> {
        let (inner, outer, mut values) = self.selection(matching);
        let total = Self::count(connection, &inner, outer, &values)?;
        values.push(Sql::Integer(limit as i64));
        let pieces = self.pieces().len().max(1) as f64;
        let mut statement = connection.prepare(&format!(
            "SELECT id, project_id, name_folded, score FROM ({inner}) {outer}
             ORDER BY score DESC, name_folded, project_id LIMIT ?"
        ))?;
        let found = statement
            .query_map(params_from_iter(values.iter()), |row| {
                let score: f64 = row.get(3)?;
                Ok(Scored {
                    row: row.get(0)?,
                    project_id: row.get(1)?,
                    name_folded: row.get(2)?,
                    score: if matching == Matching::Near {
                        score / pieces
                    } else {
                        score
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((total, found))
    }

    /// The similarity to `vector` of every filtered project's current vector
    /// of `kind`. Vectors are compared one row at a time, never all held.
    fn similarities(
        &self,
        connection: &Connection,
        kind: VectorKind,
        vector: &[f32],
    ) -> rusqlite::Result<Vec<Scored>> {
        let (_, key) = kind.columns();
        let mut sql = format!(
            "SELECT p.id, p.project_id, p.name_folded, v.vector
             FROM projects p JOIN vectors v
                 ON v.project = p.id AND v.kind = ? AND v.source_key = p.{key}
             WHERE p.card IS NOT NULL AND v.vector IS NOT NULL"
        );
        let mut values = vec![Sql::Text(kind.name().into())];
        for (condition, bound) in &self.filters {
            sql.push_str(" AND ");
            sql.push_str(condition);
            values.extend(bound.iter().cloned());
        }
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        let mut scored = Vec::new();
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(3)?;
            if let Some(score) = embeddings::similarity(vector, &blob) {
                scored.push(Scored {
                    row: row.get(0)?,
                    project_id: row.get(1)?,
                    name_folded: row.get(2)?,
                    score,
                });
            }
        }
        Ok(scored)
    }
}

/// Text or picture vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VectorKind {
    Text,
    Picture,
}

impl VectorKind {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Picture => "picture",
        }
    }

    /// The `projects` columns holding the input and its key.
    fn columns(self) -> (&'static str, &'static str) {
        match self {
            Self::Text => ("meaning_text", "text_key"),
            Self::Picture => ("picture", "picture_key"),
        }
    }
}

/// The model that made the stored vectors.
#[derive(Debug, Clone, PartialEq)]
struct Identity {
    /// [`Embeddings::fingerprint`] of the settings in use.
    settings: String,
    /// The model name the service reported.
    service_model: Option<String>,
    dimensions: usize,
    /// The unit vector of [`PROBE_TEXT`].
    probe: Vec<f32>,
    /// Why the model refused pictures; `None` while pictures are accepted.
    pictures_refused: Option<String>,
}

impl Identity {
    /// Whether the stored vectors were made under these settings.
    fn current(&self, service: &Embeddings) -> bool {
        self.settings == service.fingerprint()
    }

    /// Whether both describe the same model, so their vectors compare.
    fn same_model(&self, other: &Identity) -> bool {
        self.settings == other.settings
            && self.service_model == other.service_model
            && self.dimensions == other.dimensions
            && self.probe == other.probe
    }

    /// Whether an answer comes from this model, so its vectors compare with
    /// the stored ones.
    fn matches(&self, embedded: &Embedded) -> bool {
        embedded.model == self.service_model
            && embedded
                .vectors
                .iter()
                .all(|vector| vector.len() == self.dimensions)
    }
}

fn read_identity(connection: &Connection) -> rusqlite::Result<Option<Identity>> {
    connection
        .query_row(
            "SELECT settings, service_model, dimensions, probe, pictures_refused
             FROM identity WHERE id = 1",
            [],
            |row| {
                let probe: Vec<u8> = row.get(3)?;
                Ok(Identity {
                    settings: row.get(0)?,
                    service_model: row.get(1)?,
                    dimensions: usize::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    probe: embeddings::from_blob(&probe),
                    pictures_refused: row.get(4)?,
                })
            },
        )
        .optional()
}

/// Record the service's answer to the probe. When the settings, the model
/// name, the vector length or the probe vector differ from the stored
/// identity, every vector is deleted with it in one transaction. Turning
/// pictures off forgets an earlier refusal. Returns whether an earlier
/// identity was replaced, and the identity now in force.
fn record_probe(
    connection: &mut Connection,
    settings: &str,
    probe: Embedded,
    pictures: bool,
) -> rusqlite::Result<(bool, Identity)> {
    let vector = probe.vectors.into_iter().next().unwrap_or_default();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let prior = read_identity(&transaction)?;
    let same = prior.as_ref().is_some_and(|prior| {
        prior.settings == settings
            && prior.service_model == probe.model
            && prior.dimensions == vector.len()
            && embeddings::dot(&prior.probe, &vector) >= 1.0 - PROBE_TOLERANCE
    });
    let replaced = prior.is_some() && !same;
    let identity = match prior.filter(|_| same) {
        Some(mut identity) => {
            if !pictures && identity.pictures_refused.is_some() {
                transaction.execute(
                    "UPDATE identity SET pictures_refused = NULL WHERE id = 1",
                    [],
                )?;
                identity.pictures_refused = None;
            }
            identity
        }
        None => {
            transaction.execute("DELETE FROM vectors", [])?;
            transaction.execute(
                "INSERT OR REPLACE INTO identity
                     (id, settings, service_model, dimensions, probe, pictures_refused)
                 VALUES (1, ?1, ?2, ?3, ?4, NULL)",
                params![
                    settings,
                    probe.model,
                    vector.len() as i64,
                    embeddings::to_blob(&vector)
                ],
            )?;
            Identity {
                settings: settings.to_owned(),
                service_model: probe.model,
                dimensions: vector.len(),
                probe: vector,
                pictures_refused: None,
            }
        }
    };
    transaction.commit()?;
    Ok((replaced, identity))
}

/// A project whose text or picture needs a vector.
#[derive(Debug, Clone)]
struct Work {
    row: i64,
    /// The text, or the picture source as JSON.
    input: String,
    key: String,
    /// Failed attempts at this same input so far.
    attempts: u32,
}

/// Projects whose current input of `kind` has no vector, new ones first.
/// Failed ones wait for their retry time unless `retry` is set.
fn pending(
    connection: &Connection,
    kind: VectorKind,
    now: u64,
    retry: bool,
    limit: usize,
) -> rusqlite::Result<Vec<Work>> {
    let (input, key) = kind.columns();
    let mut statement = connection.prepare(&format!(
        "SELECT p.id, p.{input}, p.{key},
             CASE WHEN v.source_key = p.{key} THEN v.attempts ELSE 0 END AS attempts
         FROM projects p LEFT JOIN vectors v ON v.project = p.id AND v.kind = ?1
         WHERE p.card IS NOT NULL AND p.{input} IS NOT NULL AND p.{key} IS NOT NULL
           AND (v.project IS NULL OR v.source_key != p.{key}
                OR (v.vector IS NULL AND (?2 OR v.next_attempt_unix_ms <= ?3)))
         ORDER BY attempts > 0, p.project_id
         LIMIT ?4"
    ))?;
    let work = statement
        .query_map(
            params![kind.name(), retry, sql_int(now), limit as i64],
            |row| {
                Ok(Work {
                    row: row.get(0)?,
                    input: row.get(1)?,
                    key: row.get(2)?,
                    attempts: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(u32::MAX),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(work)
}

/// New work in batches of `size`; work that failed before goes alone, so one
/// input the service refuses cannot fail others again.
fn batches(work: &[Work], size: usize) -> Vec<&[Work]> {
    let split = work
        .iter()
        .position(|work| work.attempts > 0)
        .unwrap_or(work.len());
    work[..split]
        .chunks(size)
        .chain(work[split..].chunks(1))
        .collect()
}

fn write_vectors(
    connection: &mut Connection,
    kind: VectorKind,
    done: &[(i64, String, Vec<u8>)],
) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut upsert = transaction.prepare(
            "INSERT INTO vectors (project, kind, source_key, vector, attempts,
                 next_attempt_unix_ms, error)
             SELECT id, ?2, ?3, ?4, 0, 0, NULL FROM projects WHERE id = ?1
             ON CONFLICT (project, kind) DO UPDATE SET source_key = excluded.source_key,
                 vector = excluded.vector, attempts = 0, next_attempt_unix_ms = 0, error = NULL",
        )?;
        for (row, key, blob) in done {
            upsert.execute(params![row, kind.name(), key, blob])?;
        }
    }
    transaction.commit()
}

/// The wait after the given number of failed attempts.
fn backoff(attempts: u32) -> u64 {
    FIRST_RETRY_MS
        .saturating_mul(1 << attempts.saturating_sub(1).min(20))
        .min(MAX_RETRY_MS)
}

fn write_vector_failures(
    connection: &mut Connection,
    kind: VectorKind,
    failed: &[(i64, String, u32)],
    error: &str,
    now: u64,
) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut upsert = transaction.prepare(
            "INSERT INTO vectors (project, kind, source_key, vector, attempts,
                 next_attempt_unix_ms, error)
             SELECT id, ?2, ?3, NULL, ?4, ?5, ?6 FROM projects WHERE id = ?1
             ON CONFLICT (project, kind) DO UPDATE SET source_key = excluded.source_key,
                 vector = NULL, attempts = excluded.attempts,
                 next_attempt_unix_ms = excluded.next_attempt_unix_ms, error = excluded.error",
        )?;
        for (row, key, attempts) in failed {
            let next = sql_int(now.saturating_add(backoff(*attempts)));
            upsert.execute(params![row, kind.name(), key, attempts, next, error])?;
        }
    }
    transaction.commit()
}

/// How many indexed projects have an input of `kind`, and how many of those
/// have a vector of it.
fn vector_coverage(connection: &Connection, kind: VectorKind) -> rusqlite::Result<(usize, usize)> {
    let (input, key) = kind.columns();
    connection.query_row(
        &format!(
            "SELECT COUNT(*), COUNT(v.project) FROM projects p
             LEFT JOIN vectors v ON v.project = p.id AND v.kind = ?1
                 AND v.source_key = p.{key} AND v.vector IS NOT NULL
             WHERE p.card IS NOT NULL AND p.{input} IS NOT NULL"
        ),
        [kind.name()],
        |row| {
            Ok((
                usize::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
            ))
        },
    )
}

fn vectors_incomplete(kind: VectorKind, embedded: usize, indexed: usize) -> Degradation {
    let (code, message) = match kind {
        VectorKind::Text => (
            "meaning_index_incomplete",
            format!(
                "{embedded} of {indexed} indexed projects have a meaning vector; the others are found by words only until background indexing embeds them."
            ),
        ),
        VectorKind::Picture => (
            "picture_index_incomplete",
            format!(
                "{embedded} of {indexed} projects with a picture have a picture vector; the others are not found by picture until background indexing embeds them."
            ),
        ),
    };
    Degradation {
        code: code.into(),
        message,
        reason: None,
    }
}

/// One project's vector state for `index_status`.
struct VectorRow {
    project_id: String,
    text: Slot,
    /// `None` when the project has no picture.
    picture: Option<Slot>,
}

struct Slot {
    embedded: bool,
    error: Option<String>,
    retry_unix_ms: Option<i64>,
}

fn vector_rows(connection: &Connection) -> rusqlite::Result<Vec<VectorRow>> {
    let mut statement = connection.prepare(
        "SELECT p.project_id,
             t.vector IS NOT NULL, t.error, t.next_attempt_unix_ms,
             p.picture_key IS NOT NULL, i.vector IS NOT NULL, i.error, i.next_attempt_unix_ms
         FROM projects p
         LEFT JOIN vectors t ON t.project = p.id AND t.kind = 'text'
             AND t.source_key = p.text_key
         LEFT JOIN vectors i ON i.project = p.id AND i.kind = 'picture'
             AND i.source_key = p.picture_key
         WHERE p.card IS NOT NULL AND p.text_key IS NOT NULL
         ORDER BY p.project_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            let picture = if row.get::<_, bool>(4)? {
                Some(Slot {
                    embedded: row.get(5)?,
                    error: row.get(6)?,
                    retry_unix_ms: row.get(7)?,
                })
            } else {
                None
            };
            Ok(VectorRow {
                project_id: row.get(0)?,
                text: Slot {
                    embedded: row.get(1)?,
                    error: row.get(2)?,
                    retry_unix_ms: row.get(3)?,
                },
                picture,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn tally(
    counts: &mut VectorCounts,
    failures: &mut Vec<VectorFailure>,
    slot: &Slot,
    project_id: &str,
    kind: VectorKind,
) {
    if slot.embedded {
        counts.embedded += 1;
    } else if let Some(error) = &slot.error {
        counts.failed += 1;
        if failures.len() < MAX_LISTED_FAILURES {
            failures.push(VectorFailure {
                project_id: project_id.to_owned(),
                kind: kind.name().to_owned(),
                error: error.clone(),
                retry_unix_ms: slot
                    .retry_unix_ms
                    .and_then(|ms| u64::try_from(ms).ok())
                    .unwrap_or(0),
            });
        }
    } else {
        counts.pending += 1;
    }
}

fn embedding_status(
    service: &Embeddings,
    identity: Option<&Identity>,
    rows: &[VectorRow],
    present: &HashSet<&str>,
    state: &ServiceState,
) -> EmbeddingStatus {
    let mut text = VectorCounts::default();
    let mut pictures = VectorCounts::default();
    let mut failures = Vec::new();
    for row in rows
        .iter()
        .filter(|row| present.contains(row.project_id.as_str()))
    {
        if identity.is_none() {
            // Vectors from other settings are discarded by the next pass.
            text.pending += 1;
            pictures.pending += usize::from(row.picture.is_some());
            continue;
        }
        tally(
            &mut text,
            &mut failures,
            &row.text,
            &row.project_id,
            VectorKind::Text,
        );
        if let Some(slot) = &row.picture {
            tally(
                &mut pictures,
                &mut failures,
                slot,
                &row.project_id,
                VectorKind::Picture,
            );
        }
    }
    let embedding_pictures =
        service.pictures() && identity.is_none_or(|identity| identity.pictures_refused.is_none());
    EmbeddingStatus {
        model: service.model().to_owned(),
        service_model: identity.and_then(|identity| identity.service_model.clone()),
        dimensions: identity.map(|identity| identity.dimensions),
        revision: service.revision().map(str::to_owned),
        text,
        pictures: embedding_pictures.then_some(pictures),
        failures,
        retry_after_unix_ms: state
            .retry_after_unix_ms
            .filter(|until| *until > super::unix_ms()),
    }
}

/// A project's picture, ready to send.
fn picture_input(workspace: &Workspace, source: &str) -> Result<Vec<u8>, ToolError> {
    let bytes = match serde_json::from_str::<PictureSource>(source)? {
        PictureSource::Cover { path } => cover::read_picture(workspace, &path)?,
        PictureSource::SliceThumbnail { package, plate } => {
            cover::read_thumbnail(workspace, &package, plate)?
        }
    };
    embeddings::prepare_picture(&bytes)
}

/// How a method treats a candidate it did not score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Missing {
    /// As no relevance: word search found nothing in it.
    Zero,
    /// As the method's lowest score: the project has no vector to compare.
    Lowest,
}

/// Each candidate's combined score over the methods that scored at least one
/// candidate. Candidates are in tie-breaking order.
fn combine(methods: &[(Vec<Option<f64>>, Missing)], how: Combine) -> Vec<f64> {
    let count = methods.first().map_or(0, |(scores, _)| scores.len());
    let mut combined = vec![0.0; count];
    for (scores, missing) in methods {
        let present: Vec<(usize, f64)> = scores
            .iter()
            .enumerate()
            .filter_map(|(index, score)| score.map(|score| (index, score)))
            .collect();
        if present.is_empty() {
            continue;
        }
        match how {
            Combine::Standardised => {
                let fill = match missing {
                    Missing::Zero => 0.0,
                    Missing::Lowest => present
                        .iter()
                        .map(|(_, score)| *score)
                        .fold(f64::INFINITY, f64::min),
                };
                let values: Vec<f64> = scores.iter().map(|score| score.unwrap_or(fill)).collect();
                let mean = values.iter().sum::<f64>() / count as f64;
                let deviation = (values
                    .iter()
                    .map(|value| (value - mean).powi(2))
                    .sum::<f64>()
                    / count as f64)
                    .sqrt();
                if deviation > 1e-12 {
                    for (total, value) in combined.iter_mut().zip(&values) {
                        *total += (value - mean) / deviation;
                    }
                }
            }
            Combine::ReciprocalRank => {
                let mut ranked = present;
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                for (rank, (index, _)) in ranked.into_iter().enumerate() {
                    combined[index] += 1.0 / (RANK_CONSTANT + (rank + 1) as f64);
                }
            }
        }
    }
    combined
}

/// A project found by one or more methods.
#[derive(Debug, Clone)]
struct Candidate {
    row: i64,
    project_id: String,
    name_folded: String,
    matched: MatchKind,
    scores: MethodScores,
}

/// A candidate in result order.
#[derive(Debug, Clone)]
struct Ranked {
    row: i64,
    matched: MatchKind,
    score: f64,
    scores: MethodScores,
}

/// `best_match` results before re-ranking.
struct Ranking {
    entries: Vec<Ranked>,
    total: usize,
    /// Cards for the entries that can appear on the requested page.
    cards: HashMap<i64, CompactCard>,
    /// Text of the leading entries for the re-ranker; empty without one.
    documents: Vec<String>,
}

impl Ranking {
    fn page(&self, offset: usize, limit: usize) -> Vec<SearchResult> {
        self.entries
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(|entry| {
                self.cards.get(&entry.row).map(|card| SearchResult {
                    card: card.clone(),
                    matched: Some(entry.matched),
                    score: Some(entry.score),
                    scores: Some(entry.scores.clone()),
                })
            })
            .collect()
    }
}

/// Put the first `depth` entries in the re-ranker's order, highest relevance
/// first; any it left out follow in their earlier order.
fn reorder(entries: &mut [Ranked], depth: usize, mut relevance: Vec<(usize, f64)>) {
    relevance.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut placed = vec![false; depth];
    let mut order = Vec::with_capacity(depth);
    for (index, score) in relevance {
        entries[index].scores.rerank = Some(score);
        placed[index] = true;
        order.push(index);
    }
    order.extend((0..depth).filter(|index| !placed[*index]));
    let reordered: Vec<Ranked> = order.iter().map(|index| entries[*index].clone()).collect();
    entries[..depth].clone_from_slice(&reordered);
}

/// Query vectors for one search, and whether it is re-ranked.
struct QueryVectors {
    meaning: Option<Vec<f32>>,
    picture: Option<Vec<f32>>,
    rerank: bool,
    /// The stored model the query vectors were checked against.
    made_by: Option<Identity>,
}

/// Rank word, meaning and picture candidates for `best_match`, and load what
/// the requested page and the re-ranker need.
fn rank(
    connection: &Connection,
    query: &Query,
    vectors: &QueryVectors,
    notes: &mut Vec<Degradation>,
) -> rusqlite::Result<Ranking> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut positions: HashMap<i64, usize> = HashMap::new();
    let mut unloaded = 0;
    if !query.words.is_empty() {
        let mut matched = MatchKind::Words;
        let (mut total, mut found) =
            query.matches(connection, Matching::Words, MAX_WORD_CANDIDATES)?;
        if total == 0 && !query.pieces().is_empty() {
            matched = MatchKind::Near;
            (total, found) = query.matches(connection, Matching::Near, MAX_WORD_CANDIDATES)?;
        }
        unloaded = total.saturating_sub(found.len());
        for scored in found {
            positions.insert(scored.row, candidates.len());
            candidates.push(Candidate {
                row: scored.row,
                project_id: scored.project_id,
                name_folded: scored.name_folded,
                matched,
                scores: MethodScores {
                    words: Some(scored.score),
                    ..MethodScores::default()
                },
            });
        }
    }
    // Each method adds its closest projects as candidates; then every
    // candidate is scored by every method that has a vector for it.
    let mut lists = Vec::new();
    for (kind, vector, matched) in [
        (
            VectorKind::Text,
            vectors.meaning.as_deref(),
            MatchKind::Meaning,
        ),
        (
            VectorKind::Picture,
            vectors.picture.as_deref(),
            MatchKind::Picture,
        ),
    ] {
        let Some(vector) = vector else { continue };
        let (indexed, embedded) = vector_coverage(connection, kind)?;
        if embedded < indexed {
            notes.push(vectors_incomplete(kind, embedded, indexed));
        }
        let mut similar = query.similarities(connection, kind, vector)?;
        similar.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.name_folded.cmp(&b.name_folded))
                .then_with(|| a.project_id.cmp(&b.project_id))
        });
        for scored in similar.iter().take(MAX_SIMILAR) {
            if let Entry::Vacant(slot) = positions.entry(scored.row) {
                slot.insert(candidates.len());
                candidates.push(Candidate {
                    row: scored.row,
                    project_id: scored.project_id.clone(),
                    name_folded: scored.name_folded.clone(),
                    matched,
                    scores: MethodScores::default(),
                });
            }
        }
        lists.push((kind, similar));
    }
    for (kind, similar) in lists {
        for scored in similar {
            let Some(&position) = positions.get(&scored.row) else {
                continue;
            };
            let scores = &mut candidates[position].scores;
            match kind {
                VectorKind::Text => scores.meaning = Some(scored.score),
                VectorKind::Picture => scores.picture = Some(scored.score),
            }
        }
    }
    candidates.sort_by(|a, b| {
        a.name_folded
            .cmp(&b.name_folded)
            .then_with(|| a.project_id.cmp(&b.project_id))
    });
    let methods = [
        (
            candidates
                .iter()
                .map(|c| c.scores.words)
                .collect::<Vec<_>>(),
            Missing::Zero,
        ),
        (
            candidates.iter().map(|c| c.scores.meaning).collect(),
            Missing::Lowest,
        ),
        (
            candidates.iter().map(|c| c.scores.picture).collect(),
            Missing::Lowest,
        ),
    ];
    let active: Vec<&Vec<Option<f64>>> = methods
        .iter()
        .map(|(scores, _)| scores)
        .filter(|scores| scores.iter().any(Option::is_some))
        .collect();
    // One method keeps its own scores, so word search alone ranks and scores
    // exactly as it does without the other methods.
    let combined: Vec<f64> = match active.as_slice() {
        [single] => single.iter().map(|score| score.unwrap_or(0.0)).collect(),
        _ => combine(&methods, query.combine),
    };
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|a, b| combined[*b].total_cmp(&combined[*a]).then(a.cmp(b)));
    let entries: Vec<Ranked> = order
        .iter()
        .map(|index| {
            let candidate = &candidates[*index];
            Ranked {
                row: candidate.row,
                matched: candidate.matched,
                score: combined[*index],
                scores: candidate.scores.clone(),
            }
        })
        .collect();
    let depth = if vectors.rerank {
        entries.len().min(RERANK_DEPTH)
    } else {
        0
    };
    let page = query.offset.min(entries.len())..(query.offset + query.limit).min(entries.len());
    let wanted: Vec<i64> = entries[..depth]
        .iter()
        .chain(&entries[page])
        .map(|entry| entry.row)
        .collect();
    let mut cards = HashMap::new();
    {
        let mut statement =
            connection.prepare("SELECT card FROM projects WHERE id = ?1 AND card IS NOT NULL")?;
        for row in wanted {
            if cards.contains_key(&row) {
                continue;
            }
            let card: Option<String> = statement.query_row([row], |r| r.get(0)).optional()?;
            if let Some(card) = card {
                cards.insert(row, parse_card(&card)?);
            }
        }
    }
    let mut documents = Vec::with_capacity(depth);
    {
        let mut statement = connection
            .prepare("SELECT COALESCE(meaning_text, name_folded) FROM projects WHERE id = ?1")?;
        for entry in &entries[..depth] {
            let text: Option<String> = statement.query_row([entry.row], |r| r.get(0)).optional()?;
            documents.push(text.unwrap_or_default());
        }
    }
    Ok(Ranking {
        total: entries.len() + unloaded,
        entries,
        cards,
        documents,
    })
}

/// Search results from the index, before any re-ranking.
enum Draft {
    Page {
        page: Page,
        coverage: Coverage,
        notes: Vec<Degradation>,
    },
    Ranked {
        ranking: Ranking,
        coverage: Coverage,
        notes: Vec<Degradation>,
    },
}

/// What the latest contacts with the services found. Kept in memory: each
/// start contacts them again.
#[derive(Debug, Default, Clone)]
struct ServiceState {
    /// Why the embeddings service could not be used by the latest pass;
    /// cleared when it answers.
    embeddings: Option<(Reason, String)>,
    /// Background requests wait until then after a busy answer.
    retry_after_unix_ms: Option<u64>,
    /// Why the latest re-ranking failed; cleared when it succeeds.
    reranker: Option<(Reason, String)>,
}

/// What one pass's embedding requests share.
struct Step<'a> {
    service: &'a Embeddings,
    identity: &'a Identity,
    now: u64,
    retry: bool,
    deadline: Instant,
}

impl ProjectIndex {
    fn new(storage: Storage, interval: Option<Duration>) -> Self {
        Self {
            storage,
            interval,
            connection: Mutex::new(None),
            passes: Arc::new(tokio::sync::Mutex::new(())),
            last_pass: Mutex::new(None),
            stop: CancellationToken::new(),
            embeddings: None,
            reranker: None,
            services: Mutex::new(ServiceState::default()),
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
    /// covers only index statements, never workspace reads or requests.
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

    /// Run index statements on the blocking pool.
    async fn db<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, ToolError> {
        let index = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            index.with_connection(|connection| Ok(work(connection)?))
        })
        .await
        .map_err(|error| ToolError::Io(std::io::Error::other(error)))?
    }

    fn state(&self) -> MutexGuard<'_, ServiceState> {
        self.services.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open the index now so a misconfigured directory fails at startup.
    pub fn open(&self) -> Result<(), ToolError> {
        self.with_connection(|_| Ok(()))
    }

    /// Bring the cards up to date with the workspace, one project at a time.
    /// Callers serialise passes through `passes`.
    pub(crate) fn pass(
        &self,
        workspace: &Workspace,
        kind: PassKind,
    ) -> Result<PassReport, ToolError> {
        let started_unix_ms = super::unix_ms();
        let ids = project_ids(workspace)?;
        let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let (known, removed) = self.with_connection(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if kind == PassKind::Rebuild {
                transaction.execute_batch(
                    "DELETE FROM facets; DELETE FROM project_text;
                     DELETE FROM project_names; DELETE FROM vectors; DELETE FROM projects;",
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
            embedding: None,
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
                    self.with_connection(|connection| Ok(store(connection, id, &hash, &entry)?))?;
                    report.rebuilt += 1;
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
        self.remember(&report);
        Ok(report)
    }

    fn remember(&self, report: &PassReport) {
        *self
            .last_pass
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(report.clone());
    }

    /// Run one pass: cards on the blocking pool, then vectors when an
    /// embeddings service is configured.
    pub async fn run(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        kind: PassKind,
    ) -> Result<PassReport, ToolError> {
        // The card pass owns the guard, so a pass whose caller gives up still
        // finishes its cards before another pass starts.
        let guard = Arc::clone(&self.passes).lock_owned().await;
        let index = Arc::clone(self);
        let cards = Arc::clone(workspace);
        let (report, guard) =
            tokio::task::spawn_blocking(move || (index.pass(&cards, kind), guard))
                .await
                .map_err(|error| ToolError::Io(std::io::Error::other(error)))?;
        let mut report = report?;
        if self.embeddings.is_some() && !self.stop.is_cancelled() {
            report.embedding = Some(self.embed(workspace, kind).await?);
            report.finished_unix_ms = super::unix_ms();
            self.remember(&report);
        }
        drop(guard);
        Ok(report)
    }

    /// Check the model identity, then embed the texts and pictures that lack
    /// current vectors, at most a pass's share.
    async fn embed(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        kind: PassKind,
    ) -> Result<EmbeddingReport, ToolError> {
        let mut report = EmbeddingReport::default();
        let Some(service) = &self.embeddings else {
            return Ok(report);
        };
        let now = super::unix_ms();
        if self
            .state()
            .retry_after_unix_ms
            .is_some_and(|until| until > now)
        {
            report.deferred = true;
            return Ok(report);
        }
        let probe = match service.embed(&[Input::Text(PROBE_TEXT)]).await {
            Ok(probe) => probe,
            Err(error) => {
                report.problem = Some(self.embeddings_failed(&error));
                return Ok(report);
            }
        };
        self.embeddings_answered();
        let settings = service.fingerprint();
        let pictures = service.pictures();
        let (replaced, identity) = self
            .db(move |connection| record_probe(connection, &settings, probe, pictures))
            .await?;
        if replaced {
            tracing::info!("the embeddings model changed; every project is embedded again");
        }
        report.model_changed = replaced;
        let step = Step {
            service,
            identity: &identity,
            now,
            retry: kind == PassKind::Retry,
            deadline: Instant::now() + EMBEDDING_BUDGET,
        };
        let more = self.embed_texts(&step, &mut report).await?;
        if more && pictures && identity.pictures_refused.is_none() {
            self.embed_pictures(workspace, &step, &mut report).await?;
        }
        Ok(report)
    }

    fn out_of_time(&self, step: &Step<'_>, report: &mut EmbeddingReport) -> bool {
        let out = self.stop.is_cancelled() || Instant::now() >= step.deadline;
        report.stopped |= out;
        out
    }

    /// Returns false when the pass should make no more requests.
    async fn embed_texts(
        self: &Arc<Self>,
        step: &Step<'_>,
        report: &mut EmbeddingReport,
    ) -> Result<bool, ToolError> {
        let (now, retry) = (step.now, step.retry);
        let work = self
            .db(move |connection| {
                pending(connection, VectorKind::Text, now, retry, MAX_TEXTS_PER_PASS)
            })
            .await?;
        for batch in batches(&work, embeddings::MAX_BATCH) {
            if self.out_of_time(step, report) {
                return Ok(false);
            }
            let texts: Vec<String> = batch
                .iter()
                .map(|work| step.service.document(&work.input))
                .collect();
            let inputs: Vec<Input<'_>> = texts
                .iter()
                .map(|text| Input::Text(text.as_str()))
                .collect();
            match step.service.embed(&inputs).await {
                Ok(embedded) if step.identity.matches(&embedded) => {
                    report.texts += batch.len();
                    self.store_vectors(VectorKind::Text, batch, &embedded)
                        .await?;
                }
                Ok(_) => {
                    report.problem = Some(self.model_changed_during_pass());
                    return Ok(false);
                }
                Err(error) if error.is_service_wide() => {
                    report.problem = Some(self.embeddings_failed(&error));
                    return Ok(false);
                }
                Err(error) => {
                    report.failed += batch.len();
                    let message = format!("the embeddings service {error}");
                    self.vectors_failed(VectorKind::Text, batch, message, now)
                        .await?;
                }
            }
        }
        Ok(true)
    }

    async fn embed_pictures(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        step: &Step<'_>,
        report: &mut EmbeddingReport,
    ) -> Result<(), ToolError> {
        let (now, retry) = (step.now, step.retry);
        let work = self
            .db(move |connection| {
                pending(
                    connection,
                    VectorKind::Picture,
                    now,
                    retry,
                    MAX_PICTURES_PER_PASS,
                )
            })
            .await?;
        for batch in batches(&work, PICTURE_BATCH) {
            if self.out_of_time(step, report) {
                return Ok(());
            }
            let sources: Vec<String> = batch.iter().map(|work| work.input.clone()).collect();
            let pictures_from = Arc::clone(workspace);
            let prepared = crate::tools::visual_blocking(move || {
                Ok::<_, ToolError>(
                    sources
                        .iter()
                        .map(|source| picture_input(&pictures_from, source))
                        .collect::<Vec<_>>(),
                )
            })
            .await?;
            let mut ready = Vec::new();
            let mut pictures = Vec::new();
            for (work, result) in batch.iter().zip(prepared) {
                match result {
                    Ok(png) => {
                        ready.push(work.clone());
                        pictures.push(png);
                    }
                    Err(error) => {
                        report.failed += 1;
                        let message = format!("{}: {error}", error.code());
                        self.vectors_failed(
                            VectorKind::Picture,
                            std::slice::from_ref(work),
                            message,
                            now,
                        )
                        .await?;
                    }
                }
            }
            if ready.is_empty() {
                continue;
            }
            let inputs: Vec<Input<'_>> = pictures
                .iter()
                .map(|png| Input::Picture(png.as_slice()))
                .collect();
            match step.service.embed(&inputs).await {
                Ok(embedded) if step.identity.matches(&embedded) => {
                    report.pictures += ready.len();
                    self.store_vectors(VectorKind::Picture, &ready, &embedded)
                        .await?;
                }
                Ok(embedded) if embedded.model == step.identity.service_model => {
                    self.refuse_pictures(PICTURE_LENGTH_DIFFERS.to_owned())
                        .await?;
                    return Ok(());
                }
                Ok(_) => {
                    report.problem = Some(self.model_changed_during_pass());
                    return Ok(());
                }
                Err(ServiceError::Refused(status))
                    if !ServiceError::Refused(status).is_service_wide() =>
                {
                    self.refuse_pictures(pictures_refused_with(status)).await?;
                    return Ok(());
                }
                Err(error) if error.is_service_wide() => {
                    report.problem = Some(self.embeddings_failed(&error));
                    return Ok(());
                }
                Err(error) => {
                    report.failed += ready.len();
                    let message = format!("the embeddings service {error}");
                    self.vectors_failed(VectorKind::Picture, &ready, message, now)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn store_vectors(
        self: &Arc<Self>,
        kind: VectorKind,
        batch: &[Work],
        embedded: &Embedded,
    ) -> Result<(), ToolError> {
        let done: Vec<(i64, String, Vec<u8>)> = batch
            .iter()
            .zip(&embedded.vectors)
            .map(|(work, vector)| (work.row, work.key.clone(), embeddings::to_blob(vector)))
            .collect();
        self.db(move |connection| write_vectors(connection, kind, &done))
            .await
    }

    async fn vectors_failed(
        self: &Arc<Self>,
        kind: VectorKind,
        batch: &[Work],
        error: String,
        now: u64,
    ) -> Result<(), ToolError> {
        tracing::warn!(kind = kind.name(), projects = batch.len(), problem = %error, "could not embed project search vectors");
        let failed: Vec<(i64, String, u32)> = batch
            .iter()
            .map(|work| (work.row, work.key.clone(), work.attempts.saturating_add(1)))
            .collect();
        let error = clipped(&error).to_owned();
        self.db(move |connection| write_vector_failures(connection, kind, &failed, &error, now))
            .await
    }

    /// Stop sending pictures until the settings or the model identity change.
    async fn refuse_pictures(self: &Arc<Self>, detail: String) -> Result<(), ToolError> {
        let detail = format!(
            "{detail}, so pictures are not sent again until the embedding settings or the model change"
        );
        tracing::warn!(problem = %detail, "project picture search is unavailable");
        self.db(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "UPDATE identity SET pictures_refused = ?1 WHERE id = 1",
                [&detail],
            )?;
            transaction.execute("DELETE FROM vectors WHERE kind = 'picture'", [])?;
            transaction.commit()
        })
        .await
    }

    fn embeddings_failed(&self, error: &ServiceError) -> String {
        let wait = match error {
            ServiceError::Busy(wait) => Some(*wait),
            _ => None,
        };
        self.embeddings_problem(
            reason(error),
            format!("the embeddings service {error}"),
            wait,
        )
    }

    fn model_changed_during_pass(&self) -> String {
        self.embeddings_problem(
            Reason::NotReady,
            "the embeddings service reported a different model during the pass; the next pass checks it again"
                .to_owned(),
            None,
        )
    }

    /// Record why background passes cannot use the embeddings service,
    /// logging only changes.
    fn embeddings_problem(&self, reason: Reason, detail: String, wait: Option<Duration>) -> String {
        let mut state = self.state();
        if let Some(wait) = wait {
            let wait = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            state.retry_after_unix_ms = Some(super::unix_ms().saturating_add(wait));
        }
        if state
            .embeddings
            .as_ref()
            .is_none_or(|(_, known)| *known != detail)
        {
            tracing::warn!(problem = %detail, "project search cannot use the embeddings service");
        }
        state.embeddings = Some((reason, detail.clone()));
        detail
    }

    fn embeddings_answered(&self) {
        let mut state = self.state();
        state.retry_after_unix_ms = None;
        if state.embeddings.take().is_some() {
            tracing::info!("the embeddings service answers again");
        }
    }

    fn reranker_failed(&self, error: &ServiceError) -> Degradation {
        let detail = format!("the re-ranking service {error}; results keep their combined order");
        let mut state = self.state();
        if state
            .reranker
            .as_ref()
            .is_none_or(|(_, known)| *known != detail)
        {
            tracing::warn!(problem = %detail, "project search could not re-rank results");
        }
        state.reranker = Some((reason(error), detail.clone()));
        note(Capability::Rerank, reason(error), Some(detail))
    }

    /// Search the index. `permits` bounds its blocking index reads, so a
    /// caller with its own pool does not use the MCP tools' workspace
    /// operations.
    pub async fn search(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        params: SearchParams,
        permits: &Arc<Semaphore>,
    ) -> Result<SearchResponse, ToolError> {
        let query = Arc::new(Query::new(params)?);
        let index = Arc::clone(self);
        let identity = blocking_with(permits, move || {
            index.with_connection(|connection| Ok(read_identity(connection)?))
        })
        .await?
        .filter(|identity| {
            self.embeddings
                .as_ref()
                .is_some_and(|service| identity.current(service))
        });
        let mut degraded = Vec::new();
        let meaning = self
            .query_meaning(&query, identity.as_ref(), &mut degraded)
            .await;
        let picture = self
            .query_picture(workspace, &query, identity.as_ref(), &mut degraded)
            .await?;
        let rerank = if self.reranker.is_some() {
            query.text.is_some()
                && query.sort == SearchSort::BestMatch
                && query.offset < RERANK_DEPTH
        } else {
            degraded.push(note(Capability::Rerank, Reason::NotConfigured, None));
            false
        };
        let made_by = identity.filter(|_| meaning.is_some() || picture.is_some());
        let vectors = QueryVectors {
            meaning,
            picture,
            rerank,
            made_by,
        };
        let index = Arc::clone(self);
        let indexed = Arc::clone(workspace);
        let request = Arc::clone(&query);
        let draft =
            blocking_with(permits, move || index.draft(&indexed, &request, vectors)).await?;
        let (results, total, coverage) = match draft {
            Draft::Page {
                page,
                coverage,
                notes,
            } => {
                degraded.extend(notes);
                (page.results, page.total, coverage)
            }
            Draft::Ranked {
                mut ranking,
                coverage,
                notes,
            } => {
                if let (Some(reranker), Some(text), false) =
                    (&self.reranker, &query.text, ranking.documents.is_empty())
                {
                    match reranker.rerank(text, &ranking.documents).await {
                        Ok(relevance) => {
                            self.state().reranker = None;
                            let depth = ranking.documents.len();
                            reorder(&mut ranking.entries, depth, relevance);
                        }
                        Err(error) => degraded.push(self.reranker_failed(&error)),
                    }
                }
                degraded.extend(notes);
                (
                    ranking.page(query.offset, query.limit),
                    ranking.total,
                    coverage,
                )
            }
        };
        let next = query.offset + results.len();
        Ok(SearchResponse {
            next_offset: (!results.is_empty() && next < total).then_some(next),
            results,
            total,
            degraded,
            coverage,
        })
    }

    /// The vector of the search text, or the note saying why there is none.
    async fn query_meaning(
        &self,
        query: &Query,
        identity: Option<&Identity>,
        degraded: &mut Vec<Degradation>,
    ) -> Option<Vec<f32>> {
        let Some(service) = &self.embeddings else {
            degraded.push(note(Capability::Meaning, Reason::NotConfigured, None));
            return None;
        };
        let Some(identity) = identity else {
            degraded.push(note(Capability::Meaning, Reason::NotReady, None));
            return None;
        };
        let text = query
            .text
            .as_deref()
            .filter(|_| query.sort == SearchSort::BestMatch)?;
        let text = service.query(text);
        match service.embed(&[Input::Text(&text)]).await {
            Ok(embedded) if identity.matches(&embedded) => embedded.vectors.into_iter().next(),
            Ok(_) => {
                degraded.push(note(
                    Capability::Meaning,
                    Reason::NotReady,
                    Some(MODEL_CHANGED.to_owned()),
                ));
                None
            }
            Err(error) => {
                degraded.push(note(
                    Capability::Meaning,
                    reason(&error),
                    Some(format!("the embeddings service {error}")),
                ));
                None
            }
        }
    }

    /// The vector of the query picture, or the note saying why there is none.
    /// A picture that cannot be read is the caller's error.
    async fn query_picture(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        query: &Query,
        identity: Option<&Identity>,
        degraded: &mut Vec<Degradation>,
    ) -> Result<Option<Vec<f32>>, ToolError> {
        let unavailable = match (&self.embeddings, identity) {
            (None, _) => Some((Reason::NotConfigured, None)),
            (Some(service), _) if !service.pictures() => Some((Reason::TurnedOff, None)),
            (Some(_), None) => Some((Reason::NotReady, None)),
            (
                Some(_),
                Some(Identity {
                    pictures_refused: Some(detail),
                    ..
                }),
            ) => Some((Reason::PicturesRejected, Some(detail.clone()))),
            (Some(_), Some(_)) => None,
        };
        if let Some((reason, detail)) = unavailable {
            degraded.push(note(Capability::Picture, reason, detail));
            return Ok(None);
        }
        let (Some(service), Some(identity), Some(path)) =
            (&self.embeddings, identity, query.picture.clone())
        else {
            return Ok(None);
        };
        let pictures_from = Arc::clone(workspace);
        let png = crate::tools::visual_blocking(move || {
            embeddings::prepare_picture(&cover::read_picture(&pictures_from, &path)?)
        })
        .await?;
        let refused = |detail: &str| {
            format!(
                "{detail}, so pictures are not sent again until the embedding settings or the model change"
            )
        };
        Ok(match service.embed(&[Input::Picture(&png)]).await {
            Ok(embedded) if identity.matches(&embedded) => embedded.vectors.into_iter().next(),
            Ok(embedded) if embedded.model == identity.service_model => {
                let detail = PICTURE_LENGTH_DIFFERS.to_owned();
                self.refuse_pictures(detail.clone()).await?;
                degraded.push(note(
                    Capability::Picture,
                    Reason::PicturesRejected,
                    Some(refused(&detail)),
                ));
                None
            }
            Ok(_) => {
                degraded.push(note(
                    Capability::Picture,
                    Reason::NotReady,
                    Some(MODEL_CHANGED.to_owned()),
                ));
                None
            }
            Err(ServiceError::Refused(status))
                if !ServiceError::Refused(status).is_service_wide() =>
            {
                let detail = pictures_refused_with(status);
                self.refuse_pictures(detail.clone()).await?;
                degraded.push(note(
                    Capability::Picture,
                    Reason::PicturesRejected,
                    Some(refused(&detail)),
                ));
                None
            }
            Err(error) => {
                degraded.push(note(
                    Capability::Picture,
                    reason(&error),
                    Some(format!("the embeddings service {error}")),
                ));
                None
            }
        })
    }

    /// Read the results from the index: one page of word or ordered results,
    /// or the ranked `best_match` candidates when vectors or re-ranking take
    /// part.
    fn draft(
        &self,
        workspace: &Workspace,
        query: &Query,
        mut vectors: QueryVectors,
    ) -> Result<Draft, ToolError> {
        let ids = project_ids(workspace)?;
        let background = self.interval.is_some();
        self.with_connection(|connection| {
            let coverage = coverage(connection, &ids)?;
            let mut notes: Vec<Degradation> =
                incomplete(coverage, background).into_iter().collect();
            // A background pass may have replaced the model while the query
            // was being embedded; its vectors no longer compare.
            if let Some(made_by) = vectors.made_by.take()
                && !read_identity(connection)?.is_some_and(|now| now.same_model(&made_by))
            {
                for (vector, capability) in [
                    (vectors.meaning.take(), Capability::Meaning),
                    (vectors.picture.take(), Capability::Picture),
                ] {
                    if vector.is_some() {
                        notes.push(note(
                            capability,
                            Reason::NotReady,
                            Some(MODEL_CHANGED.to_owned()),
                        ));
                    }
                }
            }
            let vectors = &vectors;
            let ranked = query.sort == SearchSort::BestMatch
                && (vectors.meaning.is_some() || vectors.picture.is_some() || vectors.rerank);
            if ranked {
                let ranking = rank(connection, query, vectors, &mut notes)?;
                return Ok(Draft::Ranked {
                    ranking,
                    coverage,
                    notes,
                });
            }
            let page = if query.words.is_empty() && query.picture.is_some() {
                // A picture alone finds nothing without picture search.
                Page {
                    results: Vec::new(),
                    total: 0,
                }
            } else if query.words.is_empty() {
                query.run(connection, Matching::All)?
            } else {
                let page = query.run(connection, Matching::Words)?;
                if page.total == 0 && !query.pieces().is_empty() {
                    query.run(connection, Matching::Near)?
                } else {
                    page
                }
            };
            Ok(Draft::Page {
                page,
                coverage,
                notes,
            })
        })
    }

    /// Which optional methods are available, judged from the stored identity
    /// and the services' latest answers.
    fn availability(
        &self,
        identity: Option<&Identity>,
        state: &ServiceState,
    ) -> (SearchMethods, Vec<Degradation>) {
        let service = match (&self.embeddings, &state.embeddings, identity) {
            (None, ..) => Some((Reason::NotConfigured, None)),
            (Some(_), Some((reason, detail)), _) => Some((*reason, Some(detail.clone()))),
            (Some(_), None, None) => Some((Reason::NotReady, None)),
            (Some(_), None, Some(_)) => None,
        };
        let meaning = service
            .clone()
            .map(|(reason, detail)| note(Capability::Meaning, reason, detail));
        let picture = match (&self.embeddings, identity) {
            (None, _) => Some(note(Capability::Picture, Reason::NotConfigured, None)),
            (Some(embeddings), _) if !embeddings.pictures() => {
                Some(note(Capability::Picture, Reason::TurnedOff, None))
            }
            (
                Some(_),
                Some(Identity {
                    pictures_refused: Some(detail),
                    ..
                }),
            ) => Some(note(
                Capability::Picture,
                Reason::PicturesRejected,
                Some(detail.clone()),
            )),
            _ => service.map(|(reason, detail)| note(Capability::Picture, reason, detail)),
        };
        let rerank = match (&self.reranker, &state.reranker) {
            (None, _) => Some(note(Capability::Rerank, Reason::NotConfigured, None)),
            (Some(_), Some((reason, detail))) => {
                Some(note(Capability::Rerank, *reason, Some(detail.clone())))
            }
            (Some(_), None) => None,
        };
        let methods = SearchMethods {
            meaning: meaning.is_none(),
            picture: picture.is_none(),
            rerank: rerank.is_none(),
        };
        (
            methods,
            [meaning, picture, rerank].into_iter().flatten().collect(),
        )
    }

    pub fn status(&self, workspace: &Workspace) -> Result<IndexStatus, ToolError> {
        let ids = project_ids(workspace)?;
        let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let configured = self.embeddings.is_some();
        let (coverage, failures, identity, rows) = self.with_connection(|connection| {
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
            let identity = read_identity(connection)?;
            let rows = if configured {
                vector_rows(connection)?
            } else {
                Vec::new()
            };
            Ok((coverage, failures, identity, rows))
        })?;
        let identity = identity.filter(|identity| {
            self.embeddings
                .as_ref()
                .is_some_and(|service| identity.current(service))
        });
        let state = self.state().clone();
        let (methods, unavailable) = self.availability(identity.as_ref(), &state);
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
            methods,
            unavailable,
            embeddings: self.embeddings.as_ref().map(|service| {
                embedding_status(service, identity.as_ref(), &rows, &present, &state)
            }),
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
                        Ok(report) => {
                            let embedded = report.embedding.clone().unwrap_or_default();
                            if report.rebuilt + report.failed + report.removed + embedded.texts
                                + embedded.pictures + embedded.failed > 0
                            {
                                tracing::info!(
                                    rebuilt = report.rebuilt,
                                    failed = report.failed,
                                    removed = report.removed,
                                    texts = embedded.texts,
                                    pictures = embedded.pictures,
                                    vectors_failed = embedded.failed,
                                    "updated the project search index"
                                );
                            }
                        }
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

    fn indexed() -> (tempfile::TempDir, Arc<Workspace>, Arc<ProjectIndex>) {
        let (root, workspace) = workspace();
        library(&workspace);
        let index = Arc::new(ProjectIndex::new(Storage::Memory, None));
        let report = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((report.projects, report.rebuilt, report.failed), (4, 4, 0));
        (root, workspace, index)
    }

    /// Search from a synchronous test.
    fn try_search(
        index: &Arc<ProjectIndex>,
        workspace: &Arc<Workspace>,
        request: Value,
    ) -> Result<SearchResponse, ToolError> {
        let permits = Arc::new(Semaphore::new(4));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(index.search(
                workspace,
                serde_json::from_value(request).unwrap(),
                &permits,
            ))
    }

    fn search(
        index: &Arc<ProjectIndex>,
        workspace: &Arc<Workspace>,
        request: Value,
    ) -> SearchResponse {
        try_search(index, workspace, request).unwrap()
    }

    async fn find(
        index: &Arc<ProjectIndex>,
        workspace: &Arc<Workspace>,
        request: Value,
    ) -> SearchResponse {
        index
            .search(
                workspace,
                serde_json::from_value(request).unwrap(),
                &Arc::new(Semaphore::new(4)),
            )
            .await
            .unwrap()
    }

    fn ids(response: &SearchResponse) -> Vec<&str> {
        response
            .results
            .iter()
            .map(|result| result.card.project_id.as_str())
            .collect()
    }

    fn noted<'a>(degraded: &'a [Degradation], code: &str) -> Option<&'a Degradation> {
        degraded.iter().find(|note| note.code == code)
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
        assert_eq!(clip.degraded.len(), 3);
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
            let error = try_search(&index, &workspace, json!({"filters": filters})).unwrap_err();
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
            json!({"picture": "query.png", "sort": "newest"}),
            json!({"picture": " "}),
            json!({"combine": "average"}),
        ] {
            let error = match serde_json::from_value::<SearchParams>(request.clone()) {
                Ok(_) => try_search(&index, &workspace, request.clone()).unwrap_err(),
                Err(_) => continue,
            };
            assert_eq!(error.code(), "validation", "{request}");
        }
        assert!(serde_json::from_value::<SearchParams>(json!({"combine": "average"})).is_err());
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
        let index = Arc::new(ProjectIndex::new(Storage::Memory, None));
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
        let incomplete = noted(&response.degraded, "index_incomplete").unwrap();
        assert!(incomplete.message.contains("index_retry"));
        let retried = index.pass(&workspace, PassKind::Retry).unwrap();
        assert_eq!((retried.failed, retried.unchanged), (1, 4));
        record(&workspace, "bad", "Mended", "", 5_000);
        let mended = index.pass(&workspace, PassKind::Incremental).unwrap();
        assert_eq!((mended.rebuilt, mended.failed), (1, 0));
        assert_eq!(index.status(&workspace).unwrap().failed, 0);
        assert!(
            noted(
                &search(&index, &workspace, json!({})).degraded,
                "index_incomplete"
            )
            .is_none()
        );
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
        let run = |index: &Arc<ProjectIndex>| -> Vec<SearchResponse> {
            requests
                .iter()
                .map(|request| search(index, &workspace, request.clone()))
                .collect()
        };
        let first = Arc::new(ProjectIndex::new(storage.clone(), None));
        first.pass(&workspace, PassKind::Incremental).unwrap();
        let expected = run(&first);
        drop(first);
        let reopened = Arc::new(ProjectIndex::new(storage.clone(), None));
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
        let rebuilt = Arc::new(ProjectIndex::new(storage, None));
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
        let index = Arc::new(ProjectIndex::default());
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
        assert_eq!(found["degraded"][0]["reason"], "not_configured");
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

    mod services {
        use std::sync::atomic::{AtomicBool, Ordering};

        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use wiremock::{
            Mock, MockServer, Request, Respond, ResponseTemplate,
            matchers::{header, method, path},
        };

        use super::*;

        /// Words the fake model recognises; each text dimension counts the
        /// words of one concept.
        const CONCEPTS: [&[&str]; 4] = [
            &["cable", "cables", "clip", "cord", "wire"],
            &["vase", "flower", "flowers", "bouquet", "tulips"],
            &["hook", "hang", "hangs", "bag", "coat"],
            &["desk", "drawer", "drawers", "organizer", "storage"],
        ];

        /// A deterministic embeddings model. A text's vector counts concept
        /// words; a picture's vector is its average colour.
        #[derive(Clone)]
        struct FakeModel {
            model: &'static str,
            /// Changes every text vector, as a retrained model would.
            shifted: Arc<AtomicBool>,
            refuse_pictures: bool,
            /// Texts containing this word are refused with HTTP 400.
            refuse_word: Option<&'static str>,
        }

        impl FakeModel {
            fn new() -> Self {
                Self {
                    model: "fake-embed-1",
                    shifted: Arc::new(AtomicBool::new(false)),
                    refuse_pictures: false,
                    refuse_word: None,
                }
            }

            fn text(&self, text: &str) -> Vec<f32> {
                let mut vector = vec![0.0_f32; 6];
                for word in text.split(|c: char| !c.is_alphanumeric()) {
                    let word = word.to_lowercase();
                    if let Some(concept) = CONCEPTS
                        .iter()
                        .position(|words| words.contains(&word.as_str()))
                    {
                        vector[concept] += 1.0;
                    }
                }
                vector[4] = 0.25;
                if self.shifted.load(Ordering::SeqCst) {
                    vector[5] = 1.0;
                }
                vector
            }

            fn picture(parts: &Value) -> Vec<f32> {
                let url = parts[0]["image_url"]["url"].as_str().unwrap();
                let png = STANDARD
                    .decode(url.strip_prefix("data:image/png;base64,").unwrap())
                    .unwrap();
                let image = image::load_from_memory(&png).unwrap().to_rgb8();
                let mut sum = [0.0_f64; 3];
                for pixel in image.pixels() {
                    for (total, channel) in sum.iter_mut().zip(pixel.0) {
                        *total += f64::from(channel);
                    }
                }
                let count = f64::from(image.width() * image.height());
                let [r, g, b] = sum.map(|total| (total / count) as f32);
                vec![r, g, b, 0.0, 0.0, 1.0]
            }
        }

        impl Respond for FakeModel {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let mut data = Vec::new();
                for (index, input) in body["input"].as_array().unwrap().iter().enumerate() {
                    let embedding = match input {
                        Value::String(text) => {
                            let refused = self
                                .refuse_word
                                .is_some_and(|word| text.to_lowercase().contains(word));
                            if refused {
                                return ResponseTemplate::new(400);
                            }
                            self.text(text)
                        }
                        _ if self.refuse_pictures => return ResponseTemplate::new(400),
                        parts => Self::picture(parts),
                    };
                    data.push(
                        json!({"object": "embedding", "index": index, "embedding": embedding}),
                    );
                }
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object": "list", "model": self.model, "data": data}))
            }
        }

        /// Re-ranks documents by how often they mention cables.
        struct FakeReranker;

        impl Respond for FakeReranker {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let results: Vec<Value> = body["documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(index, document)| {
                        let text = document.as_str().unwrap().to_lowercase();
                        json!({"index": index, "relevance_score": text.matches("cable").count() as f64})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({"results": results}))
            }
        }

        async fn served(server: &MockServer, model: FakeModel) {
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .respond_with(model)
                .mount(server)
                .await;
        }

        /// An index using the fake service; `extra` settings override the
        /// defaults.
        fn configured(server: &MockServer, extra: &[(&str, &str)]) -> Arc<ProjectIndex> {
            let mut pairs: Vec<(String, String)> = [
                ("PRINTABLE_EMBEDDING_URL", format!("{}/v1", server.uri())),
                ("PRINTABLE_EMBEDDING_MODEL", "fake-embed".to_owned()),
                ("PRINTABLE_EMBEDDING_QUERY_PREFIX", "query: ".to_owned()),
                (
                    "PRINTABLE_EMBEDDING_DOCUMENT_PREFIX",
                    "passage: ".to_owned(),
                ),
                (INTERVAL_KEY, "0".to_owned()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
            pairs.extend(
                extra
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
            );
            configure(&move |key: &str| {
                pairs
                    .iter()
                    .rev()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            })
            .unwrap()
        }

        fn coloured_png(colour: [u8; 3]) -> Vec<u8> {
            let mut bytes = Vec::new();
            image::RgbImage::from_pixel(24, 16, image::Rgb(colour))
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            bytes
        }

        fn set_cover(workspace: &Workspace, id: &str, colour: [u8; 3]) {
            workspace
                .write_artifact(
                    &format!("projects/{id}/cover.png"),
                    &coloured_png(colour),
                    true,
                )
                .unwrap();
            let params = cover::CoverParams {
                project_id: id.into(),
                expected_record_version: crate::projects::get(workspace, id)
                    .unwrap()
                    .record_version,
                source: cover::CoverSource::Image {
                    path: "cover.png".into(),
                },
                timeout_seconds: 60.0,
            };
            let cover::Plan::Store(picture) = cover::plan(workspace, &params).unwrap() else {
                panic!("an image cover is stored directly")
            };
            cover::commit(workspace, &params, Some(picture)).unwrap();
        }

        /// A newer slice whose package has a plate thumbnail.
        fn sliced_thumbnail(workspace: &Workspace, id: &str, colour: [u8; 3]) {
            let output = format!("projects/{id}/slice-9");
            let thumbnail = coloured_png(colour);
            let package = crate::slicing::package::fixture::package(&[
                ("Metadata/plate_1.gcode", b"G28"),
                ("Metadata/plate_1.png", &thumbnail),
            ]);
            workspace
                .write_artifact(&format!("{output}/model.gcode.3mf"), &package, false)
                .unwrap();
            for (name, value) in [
                (
                    "state.json",
                    json!({"status": "completed", "finished_unix_ms": 20_000,
                        "slice": {"project_id": id, "output_dir": output}}),
                ),
                (
                    "summary.json",
                    json!({"format_version": 1, "finished_unix_ms": 20_000,
                        "finished_estimated": false, "plate_count": 1, "thumbnails": [1],
                        "filaments": []}),
                ),
            ] {
                workspace
                    .write_artifact(
                        &format!("{output}/{name}"),
                        &serde_json::to_vec(&value).unwrap(),
                        false,
                    )
                    .unwrap();
            }
        }

        async fn bodies(server: &MockServer) -> Vec<Value> {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|request| serde_json::from_slice(&request.body).unwrap())
                .collect()
        }

        async fn picture_requests(server: &MockServer) -> usize {
            bodies(server)
                .await
                .iter()
                .filter(|body| body["input"][0].is_array())
                .count()
        }

        #[test]
        fn without_services_search_matches_words_and_names_what_is_unavailable() {
            let (_root, workspace, index) = indexed();
            let response = search(&index, &workspace, json!({"text": "clip"}));
            assert_eq!(ids(&response), ["clip", "desk"]);
            assert!(
                response
                    .results
                    .iter()
                    .all(|result| result.scores.is_none())
            );
            let notes: Vec<_> = response
                .degraded
                .iter()
                .map(|note| (note.code.as_str(), note.reason))
                .collect();
            assert_eq!(
                notes,
                [
                    ("meaning_search_unavailable", Some(Reason::NotConfigured)),
                    ("picture_search_unavailable", Some(Reason::NotConfigured)),
                    ("rerank_unavailable", Some(Reason::NotConfigured)),
                ]
            );
            let combined = search(
                &index,
                &workspace,
                json!({"text": "clip", "combine": "reciprocal_rank"}),
            );
            assert_eq!(combined.results, response.results);
            workspace
                .write_artifact("queries/red.png", &coloured_png([250, 0, 0]), false)
                .unwrap();
            let picture = search(&index, &workspace, json!({"picture": "queries/red.png"}));
            assert_eq!((picture.total, picture.results.len()), (0, 0));
            let status = index.status(&workspace).unwrap();
            assert_eq!(status.methods, SearchMethods::default());
            assert_eq!(status.unavailable.len(), 3);
            assert!(status.embeddings.is_none());
            assert!(validator().is_valid(&serde_json::to_value(&status).unwrap()));
        }

        #[tokio::test]
        async fn meaning_search_finds_projects_by_what_the_text_means() {
            let server = MockServer::start().await;
            served(&server, FakeModel::new()).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[]);
            index.pass(&workspace, PassKind::Incremental).unwrap();
            let before = find(&index, &workspace, json!({"text": "bouquet holder"})).await;
            assert_eq!(before.degraded[0].reason, Some(Reason::NotReady));
            assert_eq!(before.total, 0);

            let report = index.run(&workspace, PassKind::Incremental).await.unwrap();
            let embedding = report.embedding.unwrap();
            assert_eq!(
                (embedding.texts, embedding.failed, embedding.model_changed),
                (4, 0, false)
            );
            let found = find(&index, &workspace, json!({"text": "bouquet holder"})).await;
            assert_eq!(found.results[0].card.project_id, "vase");
            assert_eq!(found.results[0].matched, Some(MatchKind::Meaning));
            let scores = found.results[0].scores.clone().unwrap();
            assert_eq!(scores.words, None);
            assert_eq!(found.results[0].score, scores.meaning);
            assert!(noted(&found.degraded, "meaning_search_unavailable").is_none());
            let response = serde_json::to_value(&found).unwrap();
            assert!(validator().is_valid(&response), "{response}");

            // Words and meaning combine; the word match leads.
            let both = find(&index, &workspace, json!({"text": "cables"})).await;
            assert_eq!(both.results[0].card.project_id, "clip");
            assert_eq!(both.results[0].matched, Some(MatchKind::Words));
            let scores = both.results[0].scores.clone().unwrap();
            assert!(scores.words.is_some() && scores.meaning.is_some());
            // Other orders keep word matching.
            let named = find(
                &index,
                &workspace,
                json!({"text": "bouquet holder", "sort": "name"}),
            )
            .await;
            assert_eq!(named.total, 0);

            let sent = bodies(&server).await;
            assert!(
                sent.iter()
                    .any(|body| body["input"][0] == "query: bouquet holder")
            );
            assert!(sent.iter().any(|body| {
                body["input"][0]
                    .as_str()
                    .is_some_and(|text| text.starts_with("passage: Cable clip"))
            }));

            let status = index.status(&workspace).unwrap();
            assert_eq!(
                status.methods,
                SearchMethods {
                    meaning: true,
                    picture: false,
                    rerank: false
                }
            );
            let embeddings = status.embeddings.clone().unwrap();
            assert_eq!(
                embeddings.text,
                VectorCounts {
                    embedded: 4,
                    pending: 0,
                    failed: 0
                }
            );
            assert_eq!(embeddings.service_model.as_deref(), Some("fake-embed-1"));
            assert_eq!(embeddings.dimensions, Some(6));
            assert_eq!(embeddings.pictures, None);
            assert_eq!(
                noted(&status.unavailable, "picture_search_unavailable")
                    .unwrap()
                    .reason,
                Some(Reason::TurnedOff)
            );
            assert!(validator().is_valid(&serde_json::to_value(&status).unwrap()));

            // An edited project is embedded again; the others are not.
            record(&workspace, "hook", "Coat hook", "Hangs a coat", 4_000);
            let edited = index.run(&workspace, PassKind::Incremental).await.unwrap();
            assert_eq!((edited.rebuilt, edited.embedding.unwrap().texts), (1, 1));
            let quiet = index.run(&workspace, PassKind::Incremental).await.unwrap();
            assert_eq!(quiet.embedding.unwrap().texts, 0);
            // A rebuild clears vectors too, and the next pass makes them.
            let rebuilt = index.run(&workspace, PassKind::Rebuild).await.unwrap();
            assert_eq!(rebuilt.embedding.unwrap().texts, 4);
        }

        #[tokio::test]
        async fn a_changed_model_identity_embeds_every_project_again() {
            let server = MockServer::start().await;
            let model = FakeModel::new();
            let shifted = Arc::clone(&model.shifted);
            served(&server, model).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let directory = tempfile::tempdir().unwrap();
            let dir = directory.path().to_str().unwrap().to_owned();
            let open = |extra: &[(&str, &str)]| {
                let mut pairs = vec![(DIR_KEY, dir.as_str())];
                pairs.extend_from_slice(extra);
                configured(&server, &pairs)
            };
            let first = open(&[]);
            let report = first.run(&workspace, PassKind::Incremental).await.unwrap();
            assert_eq!(report.embedding.unwrap().texts, 4);
            drop(first);
            let reopened = open(&[]);
            let same = reopened
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((same.texts, same.model_changed), (0, false));
            drop(reopened);
            for extra in [
                &[("PRINTABLE_EMBEDDING_MODEL", "fake-embed-large")][..],
                &[("PRINTABLE_EMBEDDING_DIMENSIONS", "6")][..],
                &[("PRINTABLE_EMBEDDING_REVISION", "retrained")][..],
            ] {
                let changed = open(extra);
                // Until the service is asked again, the old vectors are not
                // compared.
                let before = find(&changed, &workspace, json!({"text": "bouquet holder"})).await;
                assert_eq!(
                    before.degraded[0].reason,
                    Some(Reason::NotReady),
                    "{extra:?}"
                );
                assert_eq!(before.total, 0, "{extra:?}");
                let report = changed
                    .run(&workspace, PassKind::Incremental)
                    .await
                    .unwrap()
                    .embedding
                    .unwrap();
                assert_eq!((report.texts, report.model_changed), (4, true), "{extra:?}");
                let after = find(&changed, &workspace, json!({"text": "bouquet holder"})).await;
                assert_eq!(after.results[0].card.project_id, "vase", "{extra:?}");
                drop(changed);
            }
            // The same settings with a model that now answers differently.
            let settled = open(&[("PRINTABLE_EMBEDDING_REVISION", "retrained")]);
            shifted.store(true, Ordering::SeqCst);
            let probed = settled
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((probed.texts, probed.model_changed), (4, true));
            let again = settled
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((again.texts, again.model_changed), (0, false));
        }

        #[tokio::test]
        async fn picture_search_finds_projects_whose_pictures_look_alike() {
            let server = MockServer::start().await;
            served(&server, FakeModel::new()).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            set_cover(&workspace, "vase", [220, 30, 30]);
            set_cover(&workspace, "hook", [30, 200, 40]);
            sliced_thumbnail(&workspace, "clip", [20, 40, 230]);
            let index = configured(&server, &[("PRINTABLE_EMBEDDING_IMAGES", "on")]);
            let report = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((report.texts, report.pictures, report.failed), (4, 3, 0));
            for (colour, expected) in [([250, 10, 10], "vase"), ([10, 30, 250], "clip")] {
                workspace
                    .write_artifact("queries/query.png", &coloured_png(colour), true)
                    .unwrap();
                let found = find(&index, &workspace, json!({"picture": "queries/query.png"})).await;
                assert_eq!(found.results[0].card.project_id, expected);
                assert_eq!(found.results[0].matched, Some(MatchKind::Picture));
                assert!(found.results[0].scores.as_ref().unwrap().picture.is_some());
                assert!(noted(&found.degraded, "picture_search_unavailable").is_none());
                assert!(noted(&found.degraded, "picture_index_incomplete").is_none());
            }
            let status = index.status(&workspace).unwrap();
            assert!(status.methods.meaning && status.methods.picture);
            assert_eq!(
                status.embeddings.unwrap().pictures,
                Some(VectorCounts {
                    embedded: 3,
                    pending: 0,
                    failed: 0
                })
            );
            let unreadable = index
                .search(
                    &workspace,
                    serde_json::from_value(json!({"picture": "queries/missing.png"})).unwrap(),
                    &Arc::new(Semaphore::new(4)),
                )
                .await
                .unwrap_err();
            assert_ne!(unreadable.code(), "project_index");
        }

        #[tokio::test]
        async fn a_model_that_refuses_pictures_turns_picture_search_off() {
            let server = MockServer::start().await;
            served(
                &server,
                FakeModel {
                    refuse_pictures: true,
                    ..FakeModel::new()
                },
            )
            .await;
            let (_root, workspace) = workspace();
            library(&workspace);
            set_cover(&workspace, "vase", [220, 30, 30]);
            let index = configured(&server, &[("PRINTABLE_EMBEDDING_IMAGES", "on")]);
            let first = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((first.texts, first.pictures), (4, 0));
            assert_eq!(picture_requests(&server).await, 1);
            index.run(&workspace, PassKind::Retry).await.unwrap();
            assert_eq!(
                picture_requests(&server).await,
                1,
                "pictures are not sent again"
            );
            let status = index.status(&workspace).unwrap();
            assert!(status.methods.meaning && !status.methods.picture);
            let refused = noted(&status.unavailable, "picture_search_unavailable").unwrap();
            assert_eq!(refused.reason, Some(Reason::PicturesRejected));
            assert!(refused.message.contains("HTTP status 400"), "{refused:?}");
            assert_eq!(status.embeddings.unwrap().pictures, None);
            workspace
                .write_artifact("queries/red.png", &coloured_png([250, 0, 0]), false)
                .unwrap();
            let found = find(&index, &workspace, json!({"picture": "queries/red.png"})).await;
            assert_eq!(found.total, 0);
            assert_eq!(
                noted(&found.degraded, "picture_search_unavailable")
                    .unwrap()
                    .reason,
                Some(Reason::PicturesRejected)
            );
            assert_eq!(picture_requests(&server).await, 1);
        }

        #[tokio::test]
        async fn a_refused_query_picture_turns_picture_search_off() {
            let server = MockServer::start().await;
            served(
                &server,
                FakeModel {
                    refuse_pictures: true,
                    ..FakeModel::new()
                },
            )
            .await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[("PRINTABLE_EMBEDDING_IMAGES", "on")]);
            index.run(&workspace, PassKind::Incremental).await.unwrap();
            assert_eq!(
                picture_requests(&server).await,
                0,
                "no project has a picture"
            );
            workspace
                .write_artifact("queries/red.png", &coloured_png([250, 0, 0]), false)
                .unwrap();
            for _ in 0..2 {
                let found = find(&index, &workspace, json!({"picture": "queries/red.png"})).await;
                assert_eq!(
                    noted(&found.degraded, "picture_search_unavailable")
                        .unwrap()
                        .reason,
                    Some(Reason::PicturesRejected)
                );
            }
            assert_eq!(
                picture_requests(&server).await,
                1,
                "pictures are not sent again"
            );
            let status = index.status(&workspace).unwrap();
            assert!(status.methods.meaning && !status.methods.picture);
        }

        #[tokio::test]
        async fn query_vectors_from_a_replaced_model_are_not_compared() {
            let server = MockServer::start().await;
            served(&server, FakeModel::new()).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[]);
            index.run(&workspace, PassKind::Incremental).await.unwrap();
            let mut earlier = index
                .with_connection(|connection| Ok(read_identity(connection)?))
                .unwrap()
                .unwrap();
            earlier.probe.reverse();
            let query =
                Query::new(serde_json::from_value(json!({"text": "vase"})).unwrap()).unwrap();
            let vectors = QueryVectors {
                meaning: Some(vec![0.0; earlier.dimensions]),
                picture: None,
                rerank: false,
                made_by: Some(earlier),
            };
            let Draft::Page { notes, .. } = index.draft(&workspace, &query, vectors).unwrap()
            else {
                panic!("vectors from another model are not ranked");
            };
            let note = noted(&notes, "meaning_search_unavailable").unwrap();
            assert_eq!(note.reason, Some(Reason::NotReady));
        }

        #[tokio::test]
        async fn a_text_the_service_refuses_is_retried_alone_after_a_wait() {
            let server = MockServer::start().await;
            served(
                &server,
                FakeModel {
                    refuse_word: Some("hook"),
                    ..FakeModel::new()
                },
            )
            .await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[]);
            let first = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((first.texts, first.failed), (0, 4));
            let waiting = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((waiting.texts, waiting.failed), (0, 0));
            let retried = index
                .run(&workspace, PassKind::Retry)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((retried.texts, retried.failed), (3, 1));
            let embeddings = index.status(&workspace).unwrap().embeddings.unwrap();
            assert_eq!(
                embeddings.text,
                VectorCounts {
                    embedded: 3,
                    pending: 0,
                    failed: 1
                }
            );
            let failure = &embeddings.failures[0];
            assert_eq!(
                (failure.project_id.as_str(), failure.kind.as_str()),
                ("hook", "text")
            );
            assert!(failure.error.contains("HTTP status 400"), "{failure:?}");
            assert!(failure.retry_unix_ms > super::super::super::unix_ms());
        }

        #[tokio::test]
        async fn a_busy_service_defers_background_work_until_it_may_be_asked_again() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            served(&server, FakeModel::new()).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[]);
            let busy = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!(busy.texts, 0);
            assert!(busy.problem.unwrap().contains("busy"));
            let status = index.status(&workspace).unwrap();
            assert!(status.embeddings.unwrap().retry_after_unix_ms.is_some());
            assert_eq!(
                noted(&status.unavailable, "meaning_search_unavailable")
                    .unwrap()
                    .reason,
                Some(Reason::Busy)
            );
            let deferred = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert!(deferred.deferred);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            let resumed = index
                .run(&workspace, PassKind::Incremental)
                .await
                .unwrap()
                .embedding
                .unwrap();
            assert_eq!((resumed.texts, resumed.deferred), (4, false));
            let status = index.status(&workspace).unwrap();
            assert!(status.methods.meaning);
            assert_eq!(status.embeddings.unwrap().retry_after_unix_ms, None);
        }

        #[tokio::test]
        async fn a_reranker_reorders_the_best_results_and_its_failure_keeps_their_order() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/rerank"))
                .respond_with(FakeReranker)
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/rerank"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let base = format!("{}/v1", server.uri());
            let index = configure(&|key: &str| match key {
                "PRINTABLE_RERANK_URL" => Some(base.clone()),
                "PRINTABLE_RERANK_MODEL" => Some("fake-rerank".to_owned()),
                INTERVAL_KEY => Some("0".to_owned()),
                _ => None,
            })
            .unwrap();
            index.pass(&workspace, PassKind::Incremental).unwrap();
            let reranked = find(&index, &workspace, json!({"text": "desk"})).await;
            assert_eq!(ids(&reranked), ["clip", "desk"]);
            assert_eq!(
                reranked.results[0].scores.as_ref().unwrap().rerank,
                Some(2.0)
            );
            assert_eq!(reranked.results[1].matched, Some(MatchKind::Words));
            assert!(noted(&reranked.degraded, "rerank_unavailable").is_none());
            let sent = bodies(&server).await;
            assert_eq!(sent[0]["model"], "fake-rerank");
            assert_eq!(sent[0]["query"], "desk");
            assert_eq!(sent[0]["top_n"], 2);
            assert!(
                sent[0]["documents"][0]
                    .as_str()
                    .unwrap()
                    .starts_with("Desk organizer")
            );
            let kept = find(&index, &workspace, json!({"text": "desk"})).await;
            assert_eq!(ids(&kept), ["desk", "clip"]);
            assert!(kept.results[0].scores.as_ref().unwrap().rerank.is_none());
            let failed = noted(&kept.degraded, "rerank_unavailable").unwrap();
            assert_eq!(failed.reason, Some(Reason::Unreachable));
            assert!(failed.message.contains("combined order"), "{failed:?}");
            let status = index.status(&workspace).unwrap();
            assert!(!status.methods.rerank);
            // Pages beyond the re-ranked results do not ask the re-ranker.
            let requests = server.received_requests().await.unwrap().len();
            find(&index, &workspace, json!({"text": "desk", "offset": 40})).await;
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }

        #[test]
        fn combining_standardised_and_reciprocal_rank_scores_is_deterministic() {
            let methods = [
                (vec![Some(3.0), None, Some(1.0), None], Missing::Zero),
                (vec![Some(0.2), Some(0.9), Some(0.5), None], Missing::Lowest),
                (vec![None, None, None, None], Missing::Lowest),
            ];
            let standardised = combine(&methods, Combine::Standardised);
            assert_eq!(standardised, combine(&methods, Combine::Standardised));
            // Words 3, 0, 1, 0 and meaning 0.2, 0.9, 0.5 with the last filled
            // by the lowest, 0.2, each standardised and added.
            let words = [2.0, -1.0, 0.0, -1.0].map(|d: f64| d / 1.5_f64.sqrt());
            let meaning = [-0.25, 0.45, 0.05, -0.25].map(|d: f64| d / 0.0825_f64.sqrt());
            for ((total, words), meaning) in standardised.iter().zip(words).zip(meaning) {
                assert!((total - (words + meaning)).abs() < 1e-9, "{standardised:?}");
            }
            let reciprocal = combine(&methods, Combine::ReciprocalRank);
            assert_eq!(
                reciprocal,
                [
                    1.0 / 61.0 + 1.0 / 63.0,
                    1.0 / 61.0,
                    1.0 / 62.0 + 1.0 / 62.0,
                    0.0
                ]
            );
            // Equal scores rank in candidate order.
            let tied = combine(
                &[(vec![Some(1.0), Some(1.0)], Missing::Zero)],
                Combine::ReciprocalRank,
            );
            assert_eq!(tied, [1.0 / 61.0, 1.0 / 62.0]);
            // A method whose scores are all equal adds nothing.
            let flat = combine(
                &[(vec![Some(0.5), Some(0.5)], Missing::Lowest)],
                Combine::Standardised,
            );
            assert_eq!(flat, [0.0, 0.0]);
        }

        #[tokio::test]
        async fn combined_searches_order_results_the_same_way_every_time() {
            let server = MockServer::start().await;
            served(&server, FakeModel::new()).await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let index = configured(&server, &[]);
            index.run(&workspace, PassKind::Incremental).await.unwrap();
            for combine in ["standardised", "reciprocal_rank"] {
                let request = json!({"text": "desk clip", "combine": combine});
                let first = find(&index, &workspace, request.clone()).await;
                let second = find(&index, &workspace, request).await;
                assert_eq!(first, second, "{combine}");
                // Both projects match the words and lead; the others follow
                // by meaning alone.
                let mut leading = ids(&first)[..2].to_vec();
                leading.sort_unstable();
                assert_eq!(leading, ["clip", "desk"], "{combine}");
                assert_eq!(first.total, 4, "{combine}");
            }
        }

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        #[tokio::test]
        async fn api_keys_never_appear_in_status_errors_or_logs() {
            const EMBEDDING_KEY: &str = "sk-embedding-key-never-shown";
            const RERANK_KEY: &str = "sk-rerank-key-never-shown";
            const WRONG_KEY: &str = "sk-wrong-key-never-shown";
            let logs = Capture::default();
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            // With one registered dispatcher, tracing judges a new callsite by
            // the default of whichever thread reaches it first, so another
            // test's thread could turn this test's events off. A second idle
            // dispatcher makes every callsite consult all of them.
            let _idle = tracing::Dispatch::new(tracing_subscriber::registry());
            let _logging = tracing::subscriber::set_default(subscriber);
            tracing::callsite::rebuild_interest_cache();
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .and(header(
                    "authorization",
                    format!("Bearer {EMBEDDING_KEY}").as_str(),
                ))
                .respond_with(FakeModel::new())
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/rerank"))
                .and(header(
                    "authorization",
                    format!("Bearer {RERANK_KEY}").as_str(),
                ))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            let (_root, workspace) = workspace();
            library(&workspace);
            let base = format!("{}/v1", server.uri());
            let index = configured(
                &server,
                &[
                    ("PRINTABLE_EMBEDDING_API_KEY", EMBEDDING_KEY),
                    ("PRINTABLE_RERANK_URL", base.as_str()),
                    ("PRINTABLE_RERANK_MODEL", "fake-rerank"),
                    ("PRINTABLE_RERANK_API_KEY", RERANK_KEY),
                ],
            );
            let report = index.run(&workspace, PassKind::Incremental).await.unwrap();
            assert_eq!(
                report.embedding.as_ref().unwrap().texts,
                4,
                "the key is sent"
            );
            let found = find(&index, &workspace, json!({"text": "desk"})).await;
            assert_eq!(
                noted(&found.degraded, "rerank_unavailable").unwrap().reason,
                Some(Reason::Refused)
            );
            let status = index.status(&workspace).unwrap();
            let wrong = configured(&server, &[("PRINTABLE_EMBEDDING_API_KEY", WRONG_KEY)]);
            let refused = wrong.run(&workspace, PassKind::Incremental).await.unwrap();
            assert!(
                refused
                    .embedding
                    .as_ref()
                    .unwrap()
                    .problem
                    .as_ref()
                    .unwrap()
                    .contains("refused")
            );
            let wrong_status = wrong.status(&workspace).unwrap();
            assert_eq!(
                noted(&wrong_status.unavailable, "meaning_search_unavailable")
                    .unwrap()
                    .reason,
                Some(Reason::Refused)
            );
            let settings_error = embeddings::configure(&|key: &str| match key {
                "PRINTABLE_EMBEDDING_URL" => Some("http://embed.example.com/v1".to_owned()),
                "PRINTABLE_EMBEDDING_MODEL" => Some("fake-embed".to_owned()),
                "PRINTABLE_EMBEDDING_API_KEY" => Some(EMBEDDING_KEY.to_owned()),
                _ => None,
            })
            .map(|_| ())
            .unwrap_err();
            let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            assert!(logged.contains("embeddings service"), "{logged}");
            let shown = [
                serde_json::to_string(&report).unwrap(),
                serde_json::to_string(&found).unwrap(),
                serde_json::to_string(&status).unwrap(),
                serde_json::to_string(&refused).unwrap(),
                serde_json::to_string(&wrong_status).unwrap(),
                format!("{index:?} {wrong:?}"),
                settings_error.to_string(),
                logged,
            ];
            for text in shown {
                for key in [EMBEDDING_KEY, RERANK_KEY, WRONG_KEY] {
                    assert!(!text.contains(key), "{key} appeared in {text}");
                }
            }
        }
    }
}
