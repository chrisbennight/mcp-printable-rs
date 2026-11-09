//! Library cards assembled from the records Printable keeps for a project.
//!
//! A card summarises the project record, its files, the `source.json` records
//! of model-site downloads, its newest completed slice and the recorded
//! outcomes of its prints. Building one reads workspace records only; it never
//! contacts a printer backend or a model site. Text from sites and project
//! files is untrusted data: it is clipped and reported, never interpreted.

use std::collections::{BTreeMap, BTreeSet};

use printable_workspace::{ArtifactMeta, Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Project, get, pictures};
use crate::{error::ToolError, library::MANIFEST, printers::ledger, slicing::summary};

/// Changes whenever the card layout changes, so stored cards are rebuilt.
const LAYOUT_VERSION: u32 = 4;
/// Largest `state.json`, `summary.json` or `source.json` a card reads.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
/// Cap for each list of printers, materials, colours and printer counts.
const MAX_NAMES: usize = 16;
const MAX_DESCRIPTION_CHARS: usize = 4096;
const SHORT_DESCRIPTION_CHARS: usize = 200;
/// Cap for one piece of site or record text, such as a title or a URL.
const MAX_TEXT_CHARS: usize = 512;
const MAX_DOWNLOAD_RECORDS: usize = 16;
const MAX_SLICE_STATES: usize = 256;
const MAX_UNREADABLE: usize = 16;
const MAX_INDEXED_FILE_NAMES: usize = 256;
/// Longest text sent to an embeddings service for one project.
const MAX_MEANING_CHARS: usize = 2048;
const OUTCOMES: [&str; 5] = ["completed", "failed", "cancelled", "skipped", "unknown"];

/// A project's library card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Card {
    pub project_id: String,
    pub name: String,
    /// At most 4096 characters; `description_truncated` reports a longer one.
    pub description: String,
    pub description_truncated: bool,
    pub tags: Vec<String>,
    pub record_version: u64,
    pub dates: Dates,
    pub cover: Option<CoverSummary>,
    /// Pictures generated from the project's files, shown when it has no
    /// cover.
    pub pictures: Pictures,
    /// Workspace files in the project directory.
    pub file_count: usize,
    pub formats: Vec<FormatCount>,
    /// True when the project has any 3MF file, sliced or not.
    pub has_3mf: bool,
    /// Printer names and models from print records and the newest slice.
    pub printers: Vec<String>,
    /// Filament types from print records and the newest slice.
    pub materials: Vec<String>,
    /// Filament colours, as `#RRGGBB` when they are hexadecimal.
    pub colours: Vec<String>,
    /// True when printers, materials or colours had more than 16 values.
    pub names_truncated: bool,
    pub downloads: Downloads,
    pub slices: Slices,
    pub prints: Prints,
    /// Records that could not be read; the card leaves out their details.
    pub unreadable: Vec<Unreadable>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Dates {
    pub created_unix_ms: u64,
    /// True when the creation time comes from a record file's modification time.
    pub created_estimated: bool,
    /// The newest of the record's last change and the project files'
    /// modification times.
    pub updated_unix_ms: u64,
    /// The earliest recorded model-site download.
    pub downloaded_unix_ms: Option<u64>,
    /// When the newest completed print was recorded.
    pub last_printed_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CoverSummary {
    /// Reserved workspace path of the stored picture.
    pub path: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// Pictures generated from a project's files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Pictures {
    /// Blender's product presentation of the project's model.
    pub render: Option<pictures::Picture>,
    /// OrcaSlicer's view of the project's model on the build plate.
    pub plate: Option<pictures::Picture>,
}

/// Which picture shows a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PictureKind {
    /// The cover set for the project.
    Cover,
    /// The generated Blender product render.
    Render,
    /// The generated plate picture.
    Plate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FormatCount {
    pub format: String,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Downloads {
    /// `source.json` records in the project.
    pub count: usize,
    pub items: Vec<Download>,
    /// True when more than 16 records exist; the first 16 by path are read.
    pub truncated: bool,
}

/// One model-site download, as its `source.json` records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Download {
    /// Workspace path of the `source.json` record.
    pub record: String,
    pub site: Option<String>,
    pub model_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub designer: Option<String>,
    pub license: Option<String>,
    pub retrieved_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Slices {
    /// Completed slices among the slice states read.
    pub count: usize,
    pub newest: Option<SliceSummary>,
    /// True when more than 256 slice states exist; the 256 most recently
    /// modified are read.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SliceSummary {
    /// Workspace directory of the slice.
    pub output_dir: String,
    pub finished_unix_ms: Option<u64>,
    pub finished_estimated: bool,
    pub printer: Option<String>,
    pub printer_model: Option<String>,
    pub process: Option<String>,
    pub plate_count: Option<u64>,
    /// Plates with a thumbnail picture in the slice's package, at most 16.
    pub thumbnails: Vec<u32>,
    pub filaments: Vec<SliceFilament>,
    pub filaments_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SliceFilament {
    pub slot: Option<u64>,
    pub filament_type: Option<String>,
    pub colour: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Prints {
    /// Recorded print outcomes.
    pub count: usize,
    pub outcomes: Outcomes,
    /// Completed prints per printer, most first.
    pub completed_by_printer: Vec<PrinterCount>,
    pub printers_truncated: bool,
    /// Outcome of the most recently recorded print.
    pub last_outcome: Option<String>,
    pub last_printed_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Outcomes {
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: usize,
    pub unknown: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrinterCount {
    pub printer: String,
    pub completed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Unreadable {
    pub path: String,
    pub code: String,
}

/// The short card returned by search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompactCard {
    pub project_id: String,
    pub name: String,
    /// The first 200 characters of the description.
    pub description: String,
    pub cover_path: Option<String>,
    /// The picture that shows the project: its cover, or else a generated
    /// picture; null when it has neither.
    #[serde(default)]
    pub picture: Option<PictureKind>,
    pub dates: Dates,
    /// Completed prints.
    pub printed: usize,
    pub last_outcome: Option<String>,
    pub formats: Vec<String>,
    pub tags: Vec<String>,
    pub printers: Vec<String>,
    pub materials: Vec<String>,
    pub colours: Vec<String>,
}

/// Text indexed for word search, one value per weighted column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchText {
    pub name: String,
    pub tags: String,
    pub description: String,
    pub source: String,
    pub files: String,
}

impl SearchText {
    /// The text sent to an embeddings service for meaning search: the name,
    /// tags, description, download sources and file names, one per line, at
    /// most 2048 characters.
    pub(crate) fn meaning(&self) -> String {
        let text = [
            &self.name,
            &self.tags,
            &self.description,
            &self.source,
            &self.files,
        ]
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        clip(&text, MAX_MEANING_CHARS).0
    }
}

/// The picture that shows a project in picture search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PictureSource {
    /// The stored cover picture.
    Cover { path: String },
    /// A plate thumbnail in the newest slice's package.
    SliceThumbnail { package: String, plate: u32 },
    /// A stored generated picture.
    Generated { path: String },
}

/// What a card is built from: the project record, two directory listings and
/// the record of generated pictures.
pub(crate) struct Inputs {
    project: Project,
    files: Vec<ArtifactMeta>,
    prints: Vec<ArtifactMeta>,
    pictures: Option<pictures::Record>,
}

fn listing(workspace: &Workspace, directory: &str) -> Result<Vec<ArtifactMeta>, ToolError> {
    match workspace.list_artifacts(directory, usize::MAX) {
        Ok(files) => Ok(files),
        Err(WsError::NotFound(_)) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

impl Inputs {
    pub(crate) fn gather(workspace: &Workspace, project_id: &str) -> Result<Self, ToolError> {
        let project = get(workspace, project_id)?;
        let files = listing(workspace, &project.root)?;
        let prints = listing(workspace, &ledger::project_records(project_id))?;
        let pictures = pictures::read(workspace, project_id);
        Ok(Self {
            project,
            files,
            prints,
            pictures,
        })
    }

    /// A digest of the records and of every listed file's path, size and
    /// modification time. A card built from equal inputs is equal.
    pub(crate) fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(LAYOUT_VERSION.to_le_bytes());
        let record = serde_json::to_vec(&self.project).expect("a project record serializes");
        hasher.update((record.len() as u64).to_le_bytes());
        hasher.update(&record);
        for files in [&self.files, &self.prints] {
            hasher.update((files.len() as u64).to_le_bytes());
            for file in files {
                hasher.update((file.path.len() as u64).to_le_bytes());
                hasher.update(file.path.as_bytes());
                hasher.update(file.size_bytes.to_le_bytes());
                hasher.update(file.modified_ns.to_le_bytes());
            }
        }
        // Picture changes rebuild the card; recording a failed attempt leaves
        // its displayed pictures as they were.
        let generated = self
            .pictures
            .as_ref()
            .map(|record| {
                (
                    record.render(&self.project.project_id),
                    record.plate(&self.project.project_id),
                )
            })
            .unwrap_or_default();
        let pictures = serde_json::to_vec(&generated).expect("pictures serialize");
        hasher.update((pictures.len() as u64).to_le_bytes());
        hasher.update(&pictures);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// What the project's plate picture needs.
    pub(crate) fn picture_need(&self, previews: bool, now_unix_ms: u64) -> pictures::Need {
        let files: Vec<_> = self.project_files().collect();
        pictures::plan(
            &self.project.project_id,
            &files,
            self.project.cover.is_some(),
            self.pictures.as_ref(),
            previews,
            now_unix_ms,
        )
    }

    pub(crate) fn render_need(&self, now_unix_ms: u64) -> pictures::Need {
        let files: Vec<_> = self.project_files().collect();
        pictures::render_plan(
            &self.project.project_id,
            &files,
            self.project.cover.is_some(),
            self.pictures.as_ref(),
            now_unix_ms,
        )
    }

    /// The project's files, sorted by path, with paths relative to the
    /// project directory.
    pub(crate) fn project_files(&self) -> impl Iterator<Item = (&str, &ArtifactMeta)> {
        let prefix = format!("{}/", self.project.root);
        self.files
            .iter()
            .filter_map(move |file| Some((file.path.strip_prefix(&prefix)?, file)))
    }
}

/// Build a project's card from its current records.
/// Build a project's card from its current records. A slice without a
/// recorded summary is noted rather than summarised here.
pub fn card(workspace: &Workspace, project_id: &str) -> Result<Card, ToolError> {
    Ok(build(
        workspace,
        &Inputs::gather(workspace, project_id)?,
        Summaries::Recorded,
    ))
}

fn unix_ms(modified_ns: i64) -> u64 {
    u64::try_from(modified_ns / 1_000_000).unwrap_or(0)
}

/// `text` cut to at most `max` characters, and whether it was cut.
fn clip(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((end, _)) => (text[..end].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// A nonempty string, or the first element of a one-element string array as
/// resolved slicer profiles store some values, clipped to a bounded length.
fn text(value: &Value) -> Option<String> {
    let raw = value
        .as_str()
        .or_else(|| value.get(0).and_then(Value::as_str))?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| clip(trimmed, MAX_TEXT_CHARS).0)
}

/// A hexadecimal colour as `#RRGGBB`, ignoring a trailing alpha byte; other
/// text is returned trimmed.
pub(crate) fn colour(raw: &str) -> String {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if matches!(hex.len(), 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        format!("#{}", hex[..6].to_ascii_uppercase())
    } else {
        trimmed.to_owned()
    }
}

/// The value stored for a search filter: colours as `#rrggbb`, everything
/// trimmed and in lower case.
pub(crate) fn facet_value(kind: &str, value: &str) -> String {
    if kind == "colour" {
        colour(value).to_lowercase()
    } else {
        value.trim().to_lowercase()
    }
}

/// The model or print format a file name shows, if any.
fn format_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next()?;
    if name.ends_with(".gcode.3mf") {
        return Some("gcode.3mf");
    }
    Some(match name.rsplit_once('.')?.1 {
        "3mf" => "3mf",
        "stl" => "stl",
        "step" | "stp" => "step",
        "scad" => "scad",
        "blend" => "blend",
        "obj" => "obj",
        "glb" => "glb",
        "gltf" => "gltf",
        "ply" => "ply",
        "off" => "off",
        "gcode" => "gcode",
        "dxf" => "dxf",
        "svg" => "svg",
        "py" => "py",
        "cadmodel" => "cadmodel",
        _ => return None,
    })
}

/// Read a small JSON record that agents or sites may have written. The size
/// is checked before reading, and the read itself is capped.
fn read_json(workspace: &Workspace, path: &str) -> Result<Value, ToolError> {
    let too_large =
        || ToolError::Validation(format!("{path} is larger than {MAX_RECORD_BYTES} bytes"));
    if workspace.stat_artifact(path)?.size_bytes > MAX_RECORD_BYTES {
        return Err(too_large());
    }
    let (_, bytes) = workspace.read_artifact(path)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(too_large());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Default)]
struct Notes(Vec<Unreadable>);

impl Notes {
    fn add(&mut self, path: &str, error: &ToolError) {
        tracing::debug!(path, code = error.code(), "card record unreadable");
        if self.0.len() < MAX_UNREADABLE {
            self.0.push(Unreadable {
                path: path.to_owned(),
                code: error.code().to_owned(),
            });
        }
    }
}

/// Names gathered from records, merged without regard to letter case.
#[derive(Default)]
struct Names {
    printers: BTreeMap<String, String>,
    materials: BTreeMap<String, String>,
    colours: BTreeMap<String, String>,
}

impl Names {
    fn insert(map: &mut BTreeMap<String, String>, value: Option<String>) {
        if let Some(value) = value {
            map.entry(value.to_lowercase()).or_insert(value);
        }
    }

    /// Record values may hold several comma-separated entries.
    fn insert_each(map: &mut BTreeMap<String, String>, value: &Value, convert: fn(&str) -> String) {
        if let Some(value) = text(value) {
            for part in value.split(',') {
                let part = convert(part);
                Self::insert(map, (!part.is_empty()).then_some(part));
            }
        }
    }

    fn list(map: &BTreeMap<String, String>) -> Vec<String> {
        map.values().take(MAX_NAMES).cloned().collect()
    }
}

fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}

fn downloads(workspace: &Workspace, files: &[ArtifactMeta], notes: &mut Notes) -> Downloads {
    let records: Vec<&ArtifactMeta> = files
        .iter()
        .filter(|file| file.path.rsplit('/').next() == Some(MANIFEST))
        .collect();
    let mut items = Vec::new();
    for record in records.iter().take(MAX_DOWNLOAD_RECORDS) {
        match read_json(workspace, &record.path) {
            Ok(value) => items.push(Download {
                record: record.path.clone(),
                site: text(&value["site"]),
                model_id: text(&value["model_id"]),
                url: text(&value["url"]),
                title: text(&value["title"]),
                designer: text(&value["designer"]),
                license: text(&value["license"]["name"]),
                retrieved_unix_ms: value["retrieved_unix_ms"].as_u64(),
            }),
            Err(error) => notes.add(&record.path, &error),
        }
    }
    Downloads {
        count: records.len(),
        items,
        truncated: records.len() > MAX_DOWNLOAD_RECORDS,
    }
}

fn slice_summary(output_dir: &str, summary: &Value, names: &mut Names) -> SliceSummary {
    let printer = text(&summary["printer"]["name"]);
    let printer_model = text(&summary["printer"]["printer_model"]);
    Names::insert(&mut names.printers, printer.clone());
    Names::insert(&mut names.printers, printer_model.clone());
    let all = summary["filaments"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let filaments = all
        .iter()
        .take(MAX_NAMES)
        .map(|filament| {
            let filament_type = text(&filament["filament_type"]);
            let colour = text(&filament["colour"]).map(|value| self::colour(&value));
            Names::insert(&mut names.materials, filament_type.clone());
            Names::insert(&mut names.colours, colour.clone());
            SliceFilament {
                slot: filament["slot"].as_u64(),
                filament_type,
                colour,
            }
        })
        .collect();
    SliceSummary {
        output_dir: output_dir.to_owned(),
        finished_unix_ms: summary["finished_unix_ms"].as_u64(),
        finished_estimated: summary["finished_estimated"].as_bool().unwrap_or(false),
        printer,
        printer_model,
        process: text(&summary["process"]),
        plate_count: summary["plate_count"].as_u64(),
        thumbnails: summary["thumbnails"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|plate| plate.as_u64().and_then(|plate| u32::try_from(plate).ok()))
            .filter(|plate| *plate > 0)
            .take(MAX_NAMES)
            .collect(),
        filaments,
        filaments_truncated: all.len() > MAX_NAMES,
    }
}

/// How a card treats a slice that predates recorded summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Summaries {
    /// Use only recorded summaries; requests never copy a package.
    Recorded,
    /// Summarise the package and record the result beside the slice, so the
    /// work happens once, in the background.
    RecordMissing,
}

/// Count completed slices and summarise the newest from its recorded summary.
fn slices(
    workspace: &Workspace,
    files: &[ArtifactMeta],
    names: &mut Names,
    notes: &mut Notes,
    summaries: Summaries,
) -> Slices {
    let mut states: Vec<&ArtifactMeta> = files
        .iter()
        .filter(|file| file.path.ends_with("/state.json"))
        .collect();
    let truncated = states.len() > MAX_SLICE_STATES;
    states.sort_by(|a, b| {
        b.modified_ns
            .cmp(&a.modified_ns)
            .then_with(|| a.path.cmp(&b.path))
    });
    states.truncate(MAX_SLICE_STATES);
    let mut count = 0;
    // The newest completed slice: finish time, directory, state, state time.
    let mut newest: Option<(u64, &str, Value, u64)> = None;
    for meta in states {
        let state = match read_json(workspace, &meta.path) {
            Ok(state) => state,
            Err(error) => {
                notes.add(&meta.path, &error);
                continue;
            }
        };
        // CAD builds also keep a `state.json`; only slice states name their
        // slice.
        if state["status"] != "completed" || !state["slice"].is_object() {
            continue;
        }
        count += 1;
        let state_ms = unix_ms(meta.modified_ns);
        let finished = state["finished_unix_ms"].as_u64().unwrap_or(state_ms);
        let directory = meta
            .path
            .strip_suffix("/state.json")
            .expect("filtered by suffix");
        let newer = newest.as_ref().is_none_or(|(best, best_directory, ..)| {
            finished > *best || (finished == *best && directory < *best_directory)
        });
        if newer {
            newest = Some((finished, directory, state, state_ms));
        }
    }
    let newest = newest.and_then(|(_, directory, state, state_ms)| {
        let path = format!("{directory}/{}", summary::FILE);
        let recorded = match read_json(workspace, &path) {
            Err(ToolError::Workspace(WsError::NotFound(_)))
                if summaries == Summaries::RecordMissing =>
            {
                summary::summarize(workspace, directory, &state, state_ms).inspect(|value| {
                    let written =
                        serde_json::to_vec(value)
                            .map_err(ToolError::from)
                            .and_then(|bytes| {
                                workspace
                                    .write_generated_bytes(&path, &bytes, false)
                                    .map_err(ToolError::from)
                            });
                    match written {
                        Ok(_) | Err(ToolError::Workspace(WsError::AlreadyExists(_))) => {}
                        // The card keeps the summary; the note has the index
                        // try to record it again.
                        Err(error) => {
                            tracing::warn!(
                                code = error.code(),
                                "could not record an older slice's summary"
                            );
                            notes.add(&path, &error);
                        }
                    }
                })
            }
            other => other,
        };
        match recorded {
            Ok(value) => Some(slice_summary(directory, &value, names)),
            Err(error) => {
                notes.add(&path, &error);
                None
            }
        }
    });
    Slices {
        count,
        newest,
        truncated,
    }
}

fn prints(
    workspace: &Workspace,
    files: &[ArtifactMeta],
    names: &mut Names,
    notes: &mut Notes,
) -> Prints {
    // A damaged record is noted and the others still count.
    let mut records = Vec::with_capacity(files.len());
    for file in files {
        match read_json(workspace, &file.path) {
            Ok(record) => records.push(record),
            Err(error) => notes.add(&file.path, &error),
        }
    }
    let mut outcomes = Outcomes::default();
    let mut by_printer: BTreeMap<String, (String, usize)> = BTreeMap::new();
    // The most recently recorded print: record time, print id, outcome.
    let mut last: Option<(u64, u64, &'static str)> = None;
    let mut last_printed = None;
    for record in &records {
        let outcome = OUTCOMES
            .iter()
            .copied()
            .find(|known| record["outcome"] == *known)
            .unwrap_or("unknown");
        match outcome {
            "completed" => outcomes.completed += 1,
            "failed" => outcomes.failed += 1,
            "cancelled" => outcomes.cancelled += 1,
            "skipped" => outcomes.skipped += 1,
            _ => outcomes.unknown += 1,
        }
        let printer = text(&record["printer"]["name"]);
        Names::insert(&mut names.printers, printer.clone());
        Names::insert(&mut names.printers, text(&record["printer"]["model"]));
        Names::insert_each(&mut names.materials, &record["filament_type"], trimmed);
        Names::insert_each(&mut names.colours, &record["filament_color"], colour);
        let recorded = record["recorded_unix_ms"].as_u64().unwrap_or(0);
        let print_id = record["print_id"].as_u64().unwrap_or(0);
        if outcome == "completed" {
            last_printed = Some(last_printed.map_or(recorded, |best: u64| best.max(recorded)));
            if let Some(printer) = printer {
                by_printer
                    .entry(printer.to_lowercase())
                    .or_insert((printer, 0))
                    .1 += 1;
            }
        }
        if last.is_none_or(|(at, id, _)| (recorded, print_id) > (at, id)) {
            last = Some((recorded, print_id, outcome));
        }
    }
    let mut completed_by_printer: Vec<PrinterCount> = by_printer
        .into_values()
        .map(|(printer, completed)| PrinterCount { printer, completed })
        .collect();
    completed_by_printer.sort_by(|a, b| {
        b.completed
            .cmp(&a.completed)
            .then_with(|| a.printer.to_lowercase().cmp(&b.printer.to_lowercase()))
    });
    let printers_truncated = completed_by_printer.len() > MAX_NAMES;
    completed_by_printer.truncate(MAX_NAMES);
    Prints {
        count: records.len(),
        outcomes,
        completed_by_printer,
        printers_truncated,
        last_outcome: last.map(|(_, _, outcome)| outcome.to_owned()),
        last_printed_unix_ms: last_printed,
    }
}

/// Build a card. Records that cannot be read are listed in `unreadable`
/// rather than failing the card.
pub(crate) fn build(workspace: &Workspace, inputs: &Inputs, summaries: Summaries) -> Card {
    let project = &inputs.project;
    let mut notes = Notes::default();
    let mut names = Names::default();
    let mut formats: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut newest_file_ms = 0;
    for file in &inputs.files {
        if let Some(format) = format_of(&file.path) {
            *formats.entry(format).or_default() += 1;
        }
        newest_file_ms = newest_file_ms.max(unix_ms(file.modified_ns));
    }
    let downloads = downloads(workspace, &inputs.files, &mut notes);
    let slices = slices(workspace, &inputs.files, &mut names, &mut notes, summaries);
    let prints = prints(workspace, &inputs.prints, &mut names, &mut notes);
    let (description, description_truncated) = clip(&project.description, MAX_DESCRIPTION_CHARS);
    let names_truncated = [&names.printers, &names.materials, &names.colours]
        .iter()
        .any(|map| map.len() > MAX_NAMES);
    Card {
        project_id: project.project_id.clone(),
        name: project.name.clone(),
        description,
        description_truncated,
        tags: project.tags.clone(),
        record_version: project.record_version,
        dates: Dates {
            created_unix_ms: project.created_unix_ms.unwrap_or_default(),
            created_estimated: project.created_estimated,
            updated_unix_ms: project
                .updated_unix_ms
                .unwrap_or_default()
                .max(newest_file_ms),
            downloaded_unix_ms: downloads
                .items
                .iter()
                .filter_map(|item| item.retrieved_unix_ms)
                .min(),
            last_printed_unix_ms: prints.last_printed_unix_ms,
        },
        cover: project.cover.as_ref().map(|cover| CoverSummary {
            path: cover.path.clone(),
            media_type: cover.media_type.clone(),
            width: cover.width,
            height: cover.height,
        }),
        pictures: Pictures {
            render: inputs
                .pictures
                .as_ref()
                .and_then(|record| record.render(&project.project_id))
                .cloned(),
            plate: inputs
                .pictures
                .as_ref()
                .and_then(|record| record.plate(&project.project_id))
                .cloned(),
        },
        file_count: inputs.files.len(),
        has_3mf: formats.contains_key("3mf") || formats.contains_key("gcode.3mf"),
        formats: formats
            .into_iter()
            .map(|(format, files)| FormatCount {
                format: format.to_owned(),
                files,
            })
            .collect(),
        printers: Names::list(&names.printers),
        materials: Names::list(&names.materials),
        colours: Names::list(&names.colours),
        names_truncated,
        downloads,
        slices,
        prints,
        unreadable: notes.0,
    }
}

/// Year, month (1 to 12) and day of a Unix time in milliseconds, in UTC.
pub(crate) fn civil_date(unix_ms: u64) -> (i64, usize, u64) {
    let days = i64::try_from(unix_ms / 86_400_000).unwrap_or(i64::MAX / 2);
    // Days since 1 March of year 0, then 400-year eras of 146097 days.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        usize::try_from(month).unwrap_or(1),
        u64::try_from(day).unwrap_or(1),
    )
}

/// A Unix time in milliseconds as a UTC date, `YYYY-MM-DD`.
pub(crate) fn utc_date(unix_ms: u64) -> String {
    let (year, month, day) = civil_date(unix_ms);
    format!("{year:04}-{month:02}-{day:02}")
}

impl Card {
    pub fn compact(&self) -> CompactCard {
        CompactCard {
            project_id: self.project_id.clone(),
            name: self.name.clone(),
            description: clip(&self.description, SHORT_DESCRIPTION_CHARS).0,
            cover_path: self.cover.as_ref().map(|cover| cover.path.clone()),
            picture: self.picture_kind(),
            dates: self.dates.clone(),
            printed: self.prints.outcomes.completed,
            last_outcome: self.prints.last_outcome.clone(),
            formats: self
                .formats
                .iter()
                .map(|format| format.format.clone())
                .collect(),
            tags: self.tags.clone(),
            printers: self.printers.clone(),
            materials: self.materials.clone(),
            colours: self.colours.clone(),
        }
    }

    /// Filter values, normalised with [`facet_value`].
    pub(crate) fn facets(&self) -> BTreeSet<(&'static str, String)> {
        let downloads = &self.downloads.items;
        let groups: [(&'static str, Vec<&str>); 7] = [
            (
                "printer",
                self.printers.iter().map(String::as_str).collect(),
            ),
            (
                "material",
                self.materials.iter().map(String::as_str).collect(),
            ),
            ("colour", self.colours.iter().map(String::as_str).collect()),
            ("tag", self.tags.iter().map(String::as_str).collect()),
            (
                "source",
                downloads.iter().filter_map(|d| d.site.as_deref()).collect(),
            ),
            (
                "license",
                downloads
                    .iter()
                    .filter_map(|d| d.license.as_deref())
                    .collect(),
            ),
            (
                "format",
                self.formats.iter().map(|f| f.format.as_str()).collect(),
            ),
        ];
        groups
            .into_iter()
            .flat_map(|(kind, values)| {
                values
                    .into_iter()
                    .map(move |value| (kind, facet_value(kind, value)))
            })
            .filter(|(_, value)| !value.is_empty())
            .collect()
    }

    /// Which picture shows the project.
    pub fn picture_kind(&self) -> Option<PictureKind> {
        if self.cover.is_some() {
            Some(PictureKind::Cover)
        } else if self.pictures.render.is_some() {
            Some(PictureKind::Render)
        } else if self.pictures.plate.is_some() {
            Some(PictureKind::Plate)
        } else {
            None
        }
    }

    /// The cover, else the Blender render, else the plate picture or first plate
    /// thumbnail of the newest slice's package, with a key that changes
    /// whenever that picture may change. Stored covers and generated pictures
    /// are named by their digest; a thumbnail's key follows its package's
    /// size and modification time.
    pub(crate) fn picture(&self, inputs: &Inputs) -> Option<(PictureSource, String)> {
        if let Some(cover) = &self.cover {
            return Some((
                PictureSource::Cover {
                    path: cover.path.clone(),
                },
                format!("cover:{}", cover.path),
            ));
        }
        if let Some(plate) = self
            .pictures
            .render
            .as_ref()
            .or(self.pictures.plate.as_ref())
        {
            return Some((
                PictureSource::Generated {
                    path: plate.path.clone(),
                },
                format!("generated:{}", plate.path),
            ));
        }
        let newest = self.slices.newest.as_ref()?;
        let plate = *newest.thumbnails.first()?;
        let package = format!("{}/{}", newest.output_dir, summary::PACKAGE);
        let meta = inputs.files.iter().find(|file| file.path == package)?;
        let key = format!(
            "thumbnail:{package}:{plate}:{}:{}",
            meta.size_bytes, meta.modified_ns
        );
        Some((PictureSource::SliceThumbnail { package, plate }, key))
    }

    pub(crate) fn search_text(&self, inputs: &Inputs) -> SearchText {
        let source = self
            .downloads
            .items
            .iter()
            .flat_map(|item| [&item.site, &item.title, &item.designer])
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let files = inputs
            .files
            .iter()
            .filter(|file| format_of(&file.path).is_some())
            .filter_map(|file| file.path.rsplit('/').next())
            .take(MAX_INDEXED_FILE_NAMES)
            .chain(self.materials.iter().map(String::as_str))
            .chain(self.printers.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        SearchText {
            name: self.name.clone(),
            tags: self.tags.join(" "),
            description: self.description.clone(),
            source,
            files,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::projects::{CreateParams, ProjectRequest, UpdateParams, dispatch};
    use crate::slicing::package::fixture;
    use serde_json::json;

    pub(crate) fn create(workspace: &Workspace, id: &str, name: &str, description: &str) {
        dispatch(
            workspace,
            ProjectRequest::Create(CreateParams {
                project_id: id.into(),
                name: name.into(),
                description: description.into(),
                adopt_existing: false,
            }),
        )
        .unwrap();
    }

    pub(crate) fn tag(workspace: &Workspace, id: &str, tags: &[&str]) {
        let version = get(workspace, id).unwrap().record_version;
        dispatch(
            workspace,
            ProjectRequest::Update(UpdateParams {
                project_id: id.into(),
                expected_record_version: version,
                description: None,
                tags: Some(tags.iter().map(|tag| (*tag).to_owned()).collect()),
            }),
        )
        .unwrap();
    }

    pub(crate) fn print(workspace: &Workspace, id: &str, print_id: u64, outcome: &str, at: u64) {
        workspace
            .write_reserved_artifact(
                &ledger::record_path(id, print_id),
                &serde_json::to_vec(&json!({
                    "format_version": 1, "print_id": print_id, "project_id": id,
                    "outcome": outcome,
                    "printer": {"id": 1, "name": "Workshop X1C", "model": "X1 Carbon"},
                    "filament_type": "PLA", "filament_color": "FF8800FF",
                    "recorded_unix_ms": at, "submitted_unix_ms": at - 1,
                }))
                .unwrap(),
                false,
            )
            .unwrap();
    }

    pub(crate) fn download(workspace: &Workspace, id: &str, site: &str, title: &str, at: u64) {
        workspace
            .write_artifact(
                &format!("projects/{id}/{site}/source.json"),
                &serde_json::to_vec(&json!({
                    "format_version": 1, "site": site, "model_id": "553277",
                    "url": format!("https://{site}.example/model/553277"),
                    "title": title, "designer": "Markus",
                    "license": {"name": "CC BY 4.0"}, "retrieved_unix_ms": at, "files": [],
                }))
                .unwrap(),
                true,
            )
            .unwrap();
    }

    pub(crate) fn slice(workspace: &Workspace, id: &str, directory: &str, finished: u64) {
        let output = format!("projects/{id}/{directory}");
        workspace
            .write_artifact(
                &format!("{output}/state.json"),
                &serde_json::to_vec(&json!({"status": "completed", "finished_unix_ms": finished,
                    "slice": {"project_id": id, "output_dir": output},
                    "setup": {"printer": {"name": "Bambu Lab P1S 0.4 nozzle"}}}))
                .unwrap(),
                true,
            )
            .unwrap();
        workspace
            .write_artifact(
                &format!("{output}/summary.json"),
                &serde_json::to_vec(&json!({
                    "format_version": 1, "finished_unix_ms": finished, "finished_estimated": false,
                    "package": format!("{output}/model.gcode.3mf"),
                    "printer": {"name": "Bambu Lab P1S 0.4 nozzle", "printer_model": ["Bambu Lab P1S"]},
                    "process": "0.20mm Standard @BBL P1S", "plate_count": 2,
                    "filaments": [
                        {"slot": 0, "profile": "Bambu PETG HF", "filament_type": "PETG", "colour": "#00aa00"},
                        {"slot": 1, "profile": null, "filament_type": null, "colour": null}
                    ],
                }))
                .unwrap(),
                true,
            )
            .unwrap();
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        (root, workspace)
    }

    #[test]
    fn cad_builds_are_not_slices_and_a_damaged_print_record_hides_only_itself() {
        let (_root, workspace) = workspace();
        create(&workspace, "desk", "Desk organizer", "");
        slice(&workspace, "desk", "slice-1", 5_000);
        workspace
            .write_artifact(
                "projects/desk/cad/build-1/state.json",
                &serde_json::to_vec(&json!({"status": "completed", "phase": "terminal",
                    "build": {"project_id": "desk", "output_dir": "projects/desk/cad/build-1"}}))
                .unwrap(),
                false,
            )
            .unwrap();
        print(&workspace, "desk", 7, "completed", 10_000);
        workspace
            .write_reserved_artifact(&ledger::record_path("desk", 8), b"{not json", false)
            .unwrap();
        let card = card(&workspace, "desk").unwrap();
        assert_eq!(card.slices.count, 1);
        let newest = card.slices.newest.as_ref().unwrap();
        assert_eq!(newest.output_dir, "projects/desk/slice-1");
        assert_eq!(card.prints.count, 1);
        assert_eq!(card.prints.outcomes.completed, 1);
        assert_eq!(card.dates.last_printed_unix_ms, Some(10_000));
        assert_eq!(card.unreadable.len(), 1);
        assert_eq!(card.unreadable[0].path, ledger::record_path("desk", 8));
    }

    #[test]
    fn cards_gather_records_downloads_slices_and_prints() {
        let (_root, workspace) = workspace();
        create(&workspace, "desk", "Desk organizer", "Three drawers");
        tag(&workspace, "desk", &["storage", "Desk"]);
        let picture = fixture::png(6, 4);
        workspace
            .write_artifact("projects/desk/photos/finished.png", &picture, false)
            .unwrap();
        let version = get(&workspace, "desk").unwrap().record_version;
        let params = super::super::cover::CoverParams {
            project_id: "desk".into(),
            expected_record_version: version,
            source: super::super::cover::CoverSource::Image {
                path: "photos/finished.png".into(),
            },
            timeout_seconds: 60.0,
        };
        let super::super::cover::Plan::Store(stored) =
            super::super::cover::plan(&workspace, &params).unwrap()
        else {
            panic!("an image cover is stored directly")
        };
        super::super::cover::commit(&workspace, &params, Some(stored)).unwrap();
        for path in ["parts/drawer.stl", "parts/frame.step", "desk.3mf"] {
            workspace
                .write_artifact(&format!("projects/desk/{path}"), b"model", false)
                .unwrap();
        }
        download(&workspace, "desk", "printables", "Drawer set", 2_000);
        download(&workspace, "desk", "makerworld", "Drawer remix", 1_000);
        slice(&workspace, "desk", "slice-1", 5_000);
        slice(&workspace, "desk", "slice-2", 7_000);
        print(&workspace, "desk", 7, "completed", 10_000);
        print(&workspace, "desk", 8, "failed", 12_000);
        print(&workspace, "desk", 9, "completed", 11_000);

        let card = card(&workspace, "desk").unwrap();
        assert_eq!(card.name, "Desk organizer");
        assert_eq!(card.tags, ["storage", "Desk"]);
        assert_eq!(card.record_version, 3);
        let cover = card.cover.as_ref().unwrap();
        assert_eq!((cover.width, cover.height), (6, 4));
        assert!(cover.path.starts_with(".printable/covers/desk/"));
        assert_eq!(card.dates.downloaded_unix_ms, Some(1_000));
        assert_eq!(card.dates.last_printed_unix_ms, Some(11_000));
        assert!(!card.dates.created_estimated);
        assert!(card.dates.updated_unix_ms >= card.dates.created_unix_ms);
        let formats: Vec<_> = card
            .formats
            .iter()
            .map(|f| (f.format.as_str(), f.files))
            .collect();
        assert_eq!(formats, [("3mf", 1), ("step", 1), ("stl", 1)]);
        assert!(card.has_3mf);
        assert_eq!(card.downloads.count, 2);
        assert_eq!(card.downloads.items[0].site.as_deref(), Some("makerworld"));
        assert_eq!(
            card.downloads.items[1].license.as_deref(),
            Some("CC BY 4.0")
        );
        assert_eq!(card.slices.count, 2);
        let newest = card.slices.newest.as_ref().unwrap();
        assert_eq!(newest.output_dir, "projects/desk/slice-2");
        assert_eq!(newest.printer_model.as_deref(), Some("Bambu Lab P1S"));
        assert_eq!(newest.plate_count, Some(2));
        assert_eq!(newest.filaments[0].colour.as_deref(), Some("#00AA00"));
        assert_eq!(card.prints.count, 3);
        assert_eq!(card.prints.outcomes.completed, 2);
        assert_eq!(card.prints.outcomes.failed, 1);
        assert_eq!(card.prints.last_outcome.as_deref(), Some("failed"));
        assert_eq!(
            card.prints.completed_by_printer,
            [PrinterCount {
                printer: "Workshop X1C".into(),
                completed: 2
            }]
        );
        assert_eq!(
            card.printers,
            [
                "Bambu Lab P1S",
                "Bambu Lab P1S 0.4 nozzle",
                "Workshop X1C",
                "X1 Carbon"
            ]
        );
        assert_eq!(card.materials, ["PETG", "PLA"]);
        assert_eq!(card.colours, ["#00AA00", "#FF8800"]);
        assert!(card.unreadable.is_empty());

        let compact = card.compact();
        assert_eq!(compact.printed, 2);
        assert_eq!(compact.cover_path, Some(cover.path.clone()));
        assert_eq!(compact.formats, ["3mf", "step", "stl"]);
        let facets = card.facets();
        for facet in [
            ("colour", "#ff8800"),
            ("material", "pla"),
            ("printer", "workshop x1c"),
            ("source", "printables"),
            ("license", "cc by 4.0"),
            ("tag", "desk"),
            ("format", "step"),
        ] {
            assert!(facets.contains(&(facet.0, facet.1.to_owned())), "{facet:?}");
        }
        let text = card.search_text(&Inputs::gather(&workspace, "desk").unwrap());
        assert!(text.files.contains("drawer.stl"), "{}", text.files);
        assert!(text.source.contains("Drawer remix"), "{}", text.source);
    }

    #[test]
    fn version_1_records_and_older_slices_are_summarised_honestly() {
        let (_root, workspace) = workspace();
        workspace
            .write_reserved_artifact(
                ".printable/projects/bracket.json",
                br#"{"format_version":1,"project_id":"bracket","name":"Bracket","description":"Old","root":"projects/bracket"}"#,
                false,
            )
            .unwrap();
        let settings =
            json!({"filament_colour": ["#112233"], "filament_type": ["ASA"]}).to_string();
        let package = fixture::package(&[
            ("Metadata/plate_1.gcode", b"G28"),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let output = "projects/bracket/slice-1";
        workspace
            .write_artifact(&format!("{output}/model.gcode.3mf"), &package, false)
            .unwrap();
        workspace
            .write_artifact(
                &format!("{output}/state.json"),
                &serde_json::to_vec(&json!({"status": "completed",
                    "slice": {"project_id": "bracket", "output_dir": output},
                    "setup": {"printer": {"name": "Voron"}, "filaments": []}}))
                .unwrap(),
                false,
            )
            .unwrap();
        workspace
            .write_artifact("projects/bracket/notes/source.json", b"{not json", false)
            .unwrap();
        // A request never copies the package: the missing summary is noted.
        let requested = card(&workspace, "bracket").unwrap();
        assert!(requested.dates.created_estimated);
        assert_eq!(requested.record_version, 0);
        assert!(requested.slices.newest.is_none());
        assert!(
            requested
                .unreadable
                .iter()
                .any(|entry| entry.path == format!("{output}/summary.json")),
            "{:?}",
            requested.unreadable
        );
        assert!(
            workspace
                .stat_artifact(&format!("{output}/summary.json"))
                .is_err(),
            "a card request writes nothing"
        );
        // The background index summarises the older slice once and records it.
        let inputs = Inputs::gather(&workspace, "bracket").unwrap();
        let indexed = build(&workspace, &inputs, Summaries::RecordMissing);
        for card in [indexed, card(&workspace, "bracket").unwrap()] {
            let newest = card.slices.newest.as_ref().unwrap();
            assert!(newest.finished_estimated);
            assert_eq!(newest.printer.as_deref(), Some("Voron"));
            assert_eq!(newest.filaments[0].filament_type.as_deref(), Some("ASA"));
            assert_eq!(card.colours, ["#112233"]);
            assert!(card.has_3mf);
            assert_eq!(card.downloads.count, 1);
            assert!(card.downloads.items.is_empty());
            assert_eq!(
                card.unreadable,
                [Unreadable {
                    path: "projects/bracket/notes/source.json".into(),
                    code: "serde".into()
                }]
            );
        }
        assert!(
            workspace
                .stat_artifact(&format!("{output}/summary.json"))
                .is_ok()
        );
    }

    #[test]
    fn oversized_and_hostile_site_text_stays_bounded() {
        let (_root, workspace) = workspace();
        create(&workspace, "desk", "Desk", &"d".repeat(5000));
        workspace
            .write_artifact(
                "projects/desk/big/source.json",
                &vec![b' '; (MAX_RECORD_BYTES + 1) as usize],
                false,
            )
            .unwrap();
        workspace
            .write_artifact(
                "projects/desk/site/source.json",
                &serde_json::to_vec(&json!({"title": "t".repeat(2000), "site": ["printables"]}))
                    .unwrap(),
                false,
            )
            .unwrap();
        let card = card(&workspace, "desk").unwrap();
        assert_eq!(card.description.chars().count(), MAX_DESCRIPTION_CHARS);
        assert!(card.description_truncated);
        assert_eq!(card.compact().description.chars().count(), 200);
        assert_eq!(card.unreadable[0].path, "projects/desk/big/source.json");
        let item = &card.downloads.items[0];
        assert_eq!(item.title.as_ref().unwrap().chars().count(), MAX_TEXT_CHARS);
        assert_eq!(item.site.as_deref(), Some("printables"));
    }

    #[test]
    fn input_hashes_follow_records_and_file_metadata() {
        let (_root, workspace) = workspace();
        create(&workspace, "desk", "Desk", "");
        let first = Inputs::gather(&workspace, "desk").unwrap().hash();
        assert_eq!(Inputs::gather(&workspace, "desk").unwrap().hash(), first);
        workspace
            .write_artifact("projects/desk/part.stl", b"solid", false)
            .unwrap();
        let second = Inputs::gather(&workspace, "desk").unwrap().hash();
        assert_ne!(second, first);
        print(&workspace, "desk", 1, "completed", 5);
        assert_ne!(Inputs::gather(&workspace, "desk").unwrap().hash(), second);
        assert_eq!(colour(" #ff8800 "), "#FF8800");
        assert_eq!(colour("ff8800cc"), "#FF8800");
        assert_eq!(colour("Galaxy Black"), "Galaxy Black");
        assert_eq!(facet_value("colour", "FF8800FF"), "#ff8800");
    }
}
