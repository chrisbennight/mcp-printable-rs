use super::{PrintRequest, PrinterService, validate_id};
use crate::{error::ToolError, projects};
use bambuddy_api::{BambuddyApi, jobs::StageJob};
use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportParams {
    pub project_id: String,
    /// Project-relative printer-ready .gcode.3mf artifact.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListJobsParams {
    #[serde(default)]
    pub collection: super::records::Collection,
    pub folder_id: Option<u64>,
    pub batch_id: Option<u64>,
    #[serde(default)]
    pub detail: bool,
    pub printer_id: Option<u64>,
    pub status: Option<JobStatus>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    25
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Printing,
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    /// Select individual runs for one archive instead of archive summaries.
    pub archive_id: Option<u64>,
    #[serde(default)]
    pub detail: bool,
    pub printer_id: Option<u64>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StageParams {
    pub library_file_id: Option<u64>,
    pub archive_id: Option<u64>,
    #[serde(default = "one")]
    pub quantity: u16,
    pub batch_id: Option<u64>,
    #[serde(default)]
    pub options: bambuddy_api::job_options::JobOptions,
    pub printer_id: u64,
    pub plate: u16,
    /// Global AMS tray IDs in the slice's material-slot order; -1 is unused.
    pub ams_mapping: Vec<i32>,
    pub use_ams: bool,
    #[serde(default = "enabled")]
    pub bed_levelling: bool,
    #[serde(default = "enabled")]
    pub flow_calibration: bool,
    #[serde(default = "enabled")]
    pub vibration_calibration: bool,
    #[serde(default)]
    pub timelapse: bool,
}
fn one() -> u16 {
    1
}
fn enabled() -> bool {
    true
}

impl PrinterService {
    async fn check_print_target(
        &self,
        file_id: u64,
        archive: bool,
        printer_id: u64,
    ) -> Result<Value, ToolError> {
        let (file_type, model) = if archive {
            let file = self.read.archive(file_id).await?;
            (None, file.sliced_for_model)
        } else {
            let file = self.read.library_file(file_id).await?;
            (
                Some(file.file_type),
                file.metadata
                    .and_then(|m| m.sliced_for_model)
                    .or(file.sliced_for_model),
            )
        };
        let printers = self.read.printers().await?;
        let printer = printers
            .iter()
            .find(|p| p.id == printer_id && p.is_active)
            .ok_or_else(|| invalid("selected printer is missing or disabled"))?;
        if file_type.as_deref().is_some_and(|kind| kind != "gcode.3mf") {
            return Err(invalid("select a printer-ready .gcode.3mf file"));
        }
        let models = self.read.printer_models().await.unwrap_or_default();
        let sliced_for = model.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let target = printer
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let compatibility = match (sliced_for, target) {
            (Some(source), Some(target))
                if super::review::canon(source, &models)
                    != super::review::canon(target, &models) =>
            {
                return Err(invalid(
                    "slice and selected printer declare different models",
                ));
            }
            (Some(_), Some(_)) => super::schema::CompatibilityStatus::Matched,
            _ => super::schema::CompatibilityStatus::Unknown,
        };
        Ok(json!({"status":compatibility,"sliced_for_model":sliced_for,"printer_model":target}))
    }

    pub(super) async fn dispatch_job(
        &self,
        request: PrintRequest,
        workspace: &Workspace,
    ) -> Result<Value, ToolError> {
        match request {
            PrintRequest::Review(params) => self.review(params).await,
            PrintRequest::Update(params) => self.update(params).await,
            PrintRequest::Control(params) => self.command(params).await,
            PrintRequest::Import(params) => {
                if !params.path.ends_with(".gcode.3mf") {
                    return Err(invalid("import requires a printer-ready .gcode.3mf slice"));
                }
                let path = projects::resolve(workspace, &params.project_id, &params.path)?;
                let source = workspace.snapshot_artifact_bounded(&path, 1024 * 1024 * 1024)?;
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| invalid("invalid print filename"))?
                    .to_owned();
                let file = self.control.upload_print(source.path(), filename).await?;
                workspace.write_reserved_artifact(
                    &format!(".printable/bambuddy-library/{}.json", file.id),
                    &serde_json::to_vec(&json!({"project_id":params.project_id,"source":path}))?, false,
                ).map_err(|_| invalid(&format!("library file {} was uploaded but its project association could not be saved; inspect before retrying", file.id)))?;
                Ok(
                    json!({"library_file":file,"project_id":params.project_id,"source":path,"starts_printing":false}),
                )
            }
            PrintRequest::Stage(params) => {
                validate_id(params.printer_id)?;
                let (source_id, is_archive) = match (params.library_file_id, params.archive_id) {
                    (Some(id), None) => (id, false),
                    (None, Some(id)) => (id, true),
                    _ => return Err(invalid("select library_file_id or archive_id")),
                };
                validate_id(source_id)?;
                if params.quantity == 0 {
                    return Err(invalid("quantity must be positive"));
                }
                if params
                    .options
                    .preheat_chamber_target_override
                    .is_some_and(|n| n > 60)
                {
                    return Err(invalid("chamber preheat target must be 0–60 Celsius"));
                }
                if params.plate == 0
                    || params.ams_mapping.len() > 16
                    || params.ams_mapping.iter().any(|v| *v < -1 || *v > 255)
                    || (params.use_ams && params.ams_mapping.is_empty())
                {
                    return Err(invalid(
                        "select a positive plate and explicit supported AMS mapping",
                    ));
                }
                let compatibility = self
                    .check_print_target(source_id, is_archive, params.printer_id)
                    .await?;
                let setup = serde_json::to_value(&params)?;
                let job = StageJob {
                    printer_id: params.printer_id,
                    library_file_id: params.library_file_id,
                    archive_id: params.archive_id,
                    quantity: params.quantity,
                    batch_id: params.batch_id,
                    options: params.options,
                    plate_id: params.plate,
                    ams_mapping: params.ams_mapping,
                    use_ams: params.use_ams,
                    bed_levelling: params.bed_levelling,
                    flow_cali: params.flow_calibration,
                    vibration_cali: params.vibration_calibration,
                    timelapse: params.timelapse,
                    manual_start: true,
                };
                Ok(
                    json!({"print":self.control.stage_print(&job).await?,"starts_printing":false,"setup":setup,"compatibility":compatibility}),
                )
            }
            PrintRequest::List(params) => self.record_list(params, workspace).await,
            PrintRequest::History(params) => {
                if !(1..=100).contains(&params.limit) {
                    return Err(invalid("limit must be 1–100"));
                }
                if let Some(id) = params.printer_id {
                    validate_id(id)?;
                }
                if let Some(id) = params.archive_id {
                    validate_id(id)?;
                    let runs = self.read.archive_runs(id).await?;
                    let runs = runs
                        .items
                        .into_iter()
                        .filter(|run| {
                            params
                                .printer_id
                                .is_none_or(|id| run.printer_id == Some(id))
                        })
                        .collect();
                    return super::materials::page(runs, params.offset, params.limit);
                }
                let mut archives = self
                    .read
                    .print_history(params.printer_id, params.offset, params.limit + 1)
                    .await?;
                let has_more = archives.len() > params.limit;
                archives.truncate(params.limit);
                Ok(
                    json!({"archives":archives.into_iter().map(|a|serde_json::to_value(a).map(|a|super::records::archive(a,params.detail))).collect::<Result<Vec<_>,_>>()?,"next_offset":has_more.then_some(params.offset.saturating_add(params.limit))}),
                )
            }
            PrintRequest::Status(params) => self.record_status(params, workspace).await,
            PrintRequest::Start(params) => {
                validate_id(params.print_id)?;
                let job = self.read.print_job(params.print_id).await?;
                if job.status != "pending" || !job.manual_start {
                    return Err(invalid("only a staged pending print can be started"));
                }
                let printer_id = job
                    .printer_id
                    .ok_or_else(|| invalid("print has no assigned printer"))?;
                let (file_id, is_archive) = job
                    .library_file_id
                    .map(|id| (id, false))
                    .or(job.archive_id.map(|id| (id, true)))
                    .ok_or_else(|| invalid("staged print has no source"))?;
                let compatibility = self
                    .check_print_target(file_id, is_archive, printer_id)
                    .await?;
                let printer = self.read.printer_status(printer_id).await?;
                if printer.awaiting_plate_clear {
                    return Err(invalid(
                        "printer requires plate-clear acknowledgement in Bambuddy",
                    ));
                }
                if self.read.print_job(params.print_id).await? != job {
                    return Err(invalid(
                        "staged print changed during inspection; inspect it again before starting",
                    ));
                }
                Ok(
                    json!({"print":self.control.start_with_options(params.print_id, params.skip_filament_check).await?,"may_start_printing":true,"compatibility":compatibility}),
                )
            }
            PrintRequest::Cancel(params) => self.cancel(params).await,
            _ => Err(invalid("printer control does not use the queue workflow")),
        }
    }
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}

pub(super) fn job_result(
    workspace: &Workspace,
    job: bambuddy_api::jobs::QueueItem,
) -> Result<Value, ToolError> {
    let source = match job.library_file_id {
        Some(id) => {
            match workspace.read_artifact(&format!(".printable/bambuddy-library/{id}.json")) {
                Ok((_, bytes)) => serde_json::from_slice(&bytes)?,
                Err(printable_workspace::WsError::NotFound(_)) => Value::Null,
                Err(error) => return Err(error.into()),
            }
        }
        None => Value::Null,
    };
    let mut result = serde_json::to_value(job)?;
    result["project_source"] = source;
    Ok(result)
}
