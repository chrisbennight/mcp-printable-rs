use super::{
    materials::{Material, Observation, Unavailable},
    observations::PrinterObservation,
    review::PrintReview,
};
use bambuddy_api::{job_types::*, material_types::*, materials::*};
use schemars::{JsonSchema, SchemaGenerator};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize, JsonSchema)]
struct Page<T> {
    items: Vec<T>,
    total: Option<usize>,
    next_offset: Option<usize>,
}
#[derive(Serialize, JsonSchema)]
struct PrinterList {
    printers: Vec<PrinterSummary>,
    total: usize,
    next_offset: Option<usize>,
}
#[derive(Serialize, JsonSchema)]
pub(super) struct PrinterSummary {
    pub id: u64,
    pub name: String,
    pub model: Option<String>,
    pub location: Option<String>,
    pub enabled: bool,
}
#[derive(Serialize, JsonSchema)]
struct Artifact {
    path: String,
    size_bytes: u64,
    media_type: String,
    modified_ns: i64,
}
#[derive(Serialize, JsonSchema)]
struct Snapshot {
    printer_id: u64,
    project_id: String,
    image: Artifact,
}
#[derive(Serialize, JsonSchema)]
struct Materials<T> {
    printer_id: Option<u64>,
    connected: Option<bool>,
    last_ams_update: Option<f64>,
    #[serde(flatten)]
    page: Page<T>,
    scope: String,
    sources: Option<BTreeMap<String, Option<Unavailable>>>,
    fetched_at_unix_ms: u64,
    cloud_status: Option<String>,
    orca_cloud_status: Option<String>,
}
#[derive(Serialize, JsonSchema)]
struct SensorHistory {
    #[serde(flatten)]
    summary: AMSHistoryResponse,
    page: Page<AMSHistoryPoint>,
    fetched_at_unix_ms: u64,
}
#[derive(Serialize, JsonSchema)]
struct QueueResult {
    #[serde(flatten)]
    print: QueueItem,
    project_source: Option<ProjectSource>,
}
#[derive(Serialize, JsonSchema)]
struct ProjectSource {
    project_id: String,
    source: String,
}
#[derive(Serialize, JsonSchema)]
struct PrintStatus {
    print: QueueResult,
    setup: Option<super::print_jobs::StageParams>,
    compatibility: Option<Compatibility>,
    starts_printing: Option<bool>,
    may_start_printing: Option<bool>,
    plates: Option<Observation<Plates>>,
    requirements: Option<Observation<FilamentRequirements>>,
    fetched_at_unix_ms: Option<u64>,
}
#[derive(Serialize, JsonSchema)]
struct Compatibility {
    status: CompatibilityStatus,
    sliced_for_model: Option<String>,
    printer_model: Option<String>,
}
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompatibilityStatus {
    Matched,
    Unknown,
}
#[derive(Serialize, JsonSchema)]
struct LibraryStatus {
    library_file: LibraryFile,
    project_id: Option<String>,
    source: Option<String>,
    starts_printing: Option<bool>,
    plates: Option<Observation<Plates>>,
    requirements: Option<Observation<FilamentRequirements>>,
}
#[derive(Serialize, JsonSchema)]
struct ArchiveStatus {
    archive: PrintArchive,
    plates: Option<Observation<Plates>>,
    requirements: Option<Observation<FilamentRequirements>>,
}
#[derive(Serialize, JsonSchema)]
struct BatchStatus {
    batch: PrintBatch,
}
#[derive(Serialize, JsonSchema)]
struct QueueList {
    prints: Vec<QueueResult>,
    total: usize,
    next_offset: Option<usize>,
}
#[derive(Serialize, JsonSchema)]
struct ArchiveList {
    archives: Vec<PrintArchive>,
    next_offset: Option<usize>,
}
#[derive(Serialize, JsonSchema)]
struct Control {
    printer_id: u64,
    operation: Option<bambuddy_api::control::PrinterCommand>,
    outcome: Option<OperationOutcome>,
    action: String,
    accepted: bool,
    message: String,
    effect_note: Option<String>,
}
#[derive(Serialize, JsonSchema)]
pub(super) struct RefreshStatus {
    pub printer_id: u64,
    pub refresh_requested: bool,
    pub fresh_status_observed: bool,
    pub message: String,
}
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum OperationOutcome {
    Accepted,
    Rejected,
}
#[derive(Serialize, JsonSchema)]
struct Cancel {
    print_id: Option<u64>,
    batch_id: Option<u64>,
    result: bambuddy_api::jobs::QueueReceipt,
}
#[derive(Serialize, JsonSchema)]
struct Update {
    print_ids: Vec<u64>,
    update: bambuddy_api::job_options::UpdateReceipt,
    may_start_printing: bool,
}

pub(super) fn output(name: &str) -> Value {
    let mut generator = SchemaGenerator::default();
    let variants = if name == "printer" {
        vec![
            generator.subschema_for::<PrinterObservation>(),
            generator.subschema_for::<RefreshStatus>(),
            generator.subschema_for::<PrinterList>(),
            generator.subschema_for::<Snapshot>(),
            generator.subschema_for::<Materials<Material>>(),
            generator.subschema_for::<Materials<Spool>>(),
            generator.subschema_for::<Materials<Preset>>(),
            generator.subschema_for::<Materials<FilamentName>>(),
            generator.subschema_for::<SensorHistory>(),
            generator.subschema_for::<Page<MaintenanceHistoryResponse>>(),
            generator.subschema_for::<Page<SpoolUsageHistoryResponse>>(),
        ]
    } else {
        vec![
            generator.subschema_for::<PrintStatus>(),
            generator.subschema_for::<LibraryStatus>(),
            generator.subschema_for::<ArchiveStatus>(),
            generator.subschema_for::<BatchStatus>(),
            generator.subschema_for::<QueueList>(),
            generator.subschema_for::<ArchiveList>(),
            generator.subschema_for::<Page<FileListResponse>>(),
            generator.subschema_for::<Page<PrintBatch>>(),
            generator.subschema_for::<Page<PrintRun>>(),
            generator.subschema_for::<PrintReview>(),
            generator.subschema_for::<Control>(),
            generator.subschema_for::<Cancel>(),
            generator.subschema_for::<Update>(),
        ]
    };
    serde_json::json!({"type":"object","anyOf":variants,"$defs":generator.take_definitions(true)})
}
