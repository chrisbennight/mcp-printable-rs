//! Printer observation and physical print control share the Printable endpoint.

mod delivery;
mod operations;
mod print_jobs;
mod records;
mod review;
mod schema;
pub(crate) fn rejection_payload(
    tool: &str,
    message: String,
    details: bambuddy_api::rejection::Rejection,
) -> Value {
    schema::rejection_payload(tool, message, details)
}

pub(crate) fn output_schema(name: &str) -> Value {
    schema::output(name)
}
pub(crate) fn action_output_schema(name: &str, action: &str) -> Option<Value> {
    schema::selected_output(name, Some(action))
}
mod materials;
mod observations;

use std::{sync::Arc, time::Duration};

use bambuddy_api::{ApiKey, BambuddyApi, Client, ControlAction};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config::SettingsError, error::ToolError};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrinterId {
    pub printer_id: u64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub query: Option<String>,
    pub model: Option<String>,
    pub location: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "materials::default_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotParams {
    pub printer_id: u64,
    pub project_id: String,
    /// New project-relative JPEG artifact path.
    pub output: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PrinterRequest {
    List(ListParams),
    Status(observations::StatusParams),
    /// Request full telemetry; does not prove fresh telemetry or reset a retained FAILED state.
    RefreshStatus(PrinterId),
    Materials(materials::MaterialsParams),
    History(observations::HistoryParams),
    Snapshot(SnapshotParams),
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PrintRequest {
    Import(print_jobs::ImportParams),
    Review(review::ReviewParams),
    Stage(print_jobs::StageParams),
    List(print_jobs::ListJobsParams),
    History(print_jobs::HistoryParams),
    Status(records::StatusParams),
    Start(operations::StartParams),
    Update(operations::UpdateParams),
    Control(operations::ControlParams),
    Cancel(operations::CancelParams),
    Pause(PrinterId),
    Resume(PrinterId),
    Stop(PrinterId),
    /// Acknowledge a physically cleared plate; queued automatic prints may start.
    ClearPlate(PrinterId),
}

pub struct PrinterService {
    read: Arc<Client>,
    control: Arc<Client>,
}

impl std::fmt::Debug for PrinterService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrinterService")
            .finish_non_exhaustive()
    }
}

pub fn configure(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Arc<PrinterService>>, SettingsError> {
    let Some(endpoint) = get("PRINTABLE_BAMBUDDY_URL").filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let required = |name| {
        get(name)
            .filter(|value| !value.is_empty())
            .ok_or(SettingsError::Printers(name))
    };
    let read_key = ApiKey::new(required("PRINTABLE_BAMBUDDY_READ_KEY")?)
        .map_err(|_| SettingsError::Printers("PRINTABLE_BAMBUDDY_READ_KEY"))?;
    let control_key = ApiKey::new(required("PRINTABLE_BAMBUDDY_CONTROL_KEY")?)
        .map_err(|_| SettingsError::Printers("PRINTABLE_BAMBUDDY_CONTROL_KEY"))?;
    let endpoint = endpoint
        .parse::<url::Url>()
        .map_err(|_| SettingsError::Printers("PRINTABLE_BAMBUDDY_URL"))?;
    let read = Client::new(endpoint.clone(), read_key, Duration::from_secs(10))
        .map_err(|_| SettingsError::Printers("PRINTABLE_BAMBUDDY_URL"))?;
    let control = Client::new(endpoint, control_key, Duration::from_secs(10))
        .map_err(|_| SettingsError::Printers("PRINTABLE_BAMBUDDY_URL"))?;
    Ok(Some(Arc::new(PrinterService {
        read: Arc::new(read),
        control: Arc::new(control),
    })))
}

impl PrinterService {
    pub async fn observe(
        &self,
        request: PrinterRequest,
        workspace: &printable_workspace::Workspace,
    ) -> Result<Value, ToolError> {
        match &request {
            PrinterRequest::RefreshStatus(params) => validate_id(params.printer_id)?,
            PrinterRequest::Status(params) => validate_id(params.printer_id)?,
            PrinterRequest::Snapshot(params) => validate_id(params.printer_id)?,
            PrinterRequest::List(_) | PrinterRequest::Materials(_) | PrinterRequest::History(_) => {
            }
        }
        match request {
            PrinterRequest::List(params) => {
                if !(1..=100).contains(&params.limit) {
                    return Err(ToolError::Validation("limit must be 1–100".into()));
                }
                let mut printers = self.read.printers().await?;
                printers.retain(|p| {
                    params
                        .model
                        .as_ref()
                        .is_none_or(|v| p.model.as_ref().is_some_and(|m| m.eq_ignore_ascii_case(v)))
                        && params.location.as_ref().is_none_or(|v| {
                            p.location
                                .as_ref()
                                .is_some_and(|m| m.eq_ignore_ascii_case(v))
                        })
                        && params
                            .query
                            .as_ref()
                            .is_none_or(|v| p.name.to_lowercase().contains(&v.to_lowercase()))
                });
                printers.sort_by_key(|printer| (printer.name.to_lowercase(), printer.id));
                let total = printers.len();
                let printers = printers
                    .into_iter()
                    .skip(params.offset)
                    .take(params.limit)
                    .map(|p| schema::PrinterSummary {
                        id: p.id,
                        name: bounded(p.name),
                        model: p.model,
                        location: p.location,
                        enabled: p.is_active,
                    })
                    .collect::<Vec<_>>();
                let next = params.offset.saturating_add(printers.len());
                Ok(
                    json!({"printers":printers,"total":total,"next_offset":(next<total).then_some(next)}),
                )
            }
            PrinterRequest::Status(params) => self.status(params).await,
            PrinterRequest::RefreshStatus(params) => {
                self.read.refresh_status(params.printer_id).await?;
                Ok(serde_json::to_value(schema::RefreshStatus {
                    printer_id: params.printer_id,
                    refresh_requested: true,
                    fresh_status_observed: false,
                    message: "Full telemetry requested; read status separately. Retrieval does not prove new telemetry, and FAILED may remain valid.".into(),
                })?)
            }
            PrinterRequest::Materials(params) => self.materials(params).await,
            PrinterRequest::History(params) => self.printer_history(params).await,
            PrinterRequest::Snapshot(params) => {
                if !params.output.ends_with(".jpg") && !params.output.ends_with(".jpeg") {
                    return Err(ToolError::Validation(
                        "camera output must be a JPEG path".into(),
                    ));
                }
                let output =
                    crate::projects::resolve(workspace, &params.project_id, &params.output)?;
                let bytes = self.read.camera_snapshot(params.printer_id).await?;
                let image = workspace.write_artifact(&output, &bytes, false)?;
                Ok(
                    json!({"printer_id":params.printer_id,"project_id":params.project_id,"image":image}),
                )
            }
        }
    }

    pub async fn control(
        &self,
        request: PrintRequest,
        workspace: &Arc<printable_workspace::Workspace>,
    ) -> Result<Value, ToolError> {
        let (params, action, name) = match request {
            PrintRequest::ClearPlate(params) => {
                validate_id(params.printer_id)?;
                let result = self.control.clear_plate(params.printer_id).await?;
                return Ok(
                    json!({"printer_id":params.printer_id,"action":"clear_plate","accepted":result.success,"message":bounded(result.message)}),
                );
            }
            PrintRequest::Pause(params) => (params, ControlAction::Pause, "pause"),
            PrintRequest::Resume(params) => (params, ControlAction::Resume, "resume"),
            PrintRequest::Stop(params) => (params, ControlAction::Stop, "stop"),
            request => return self.dispatch_job(request, workspace).await,
        };
        validate_id(params.printer_id)?;
        let result = self.control.control(params.printer_id, action).await?;
        Ok(
            json!({"printer_id":params.printer_id,"action":name,"accepted":result.success,"message":bounded(result.message)}),
        )
    }
}

fn validate_id(id: u64) -> Result<(), ToolError> {
    if id == 0 {
        return Err(ToolError::Validation("printer_id must be positive".into()));
    }
    Ok(())
}

fn bounded(mut value: String) -> String {
    if let Some((boundary, _)) = value.char_indices().nth(255)
        && value.chars().count() > 256
    {
        value.truncate(boundary);
        value.push('…');
    }
    value
}

#[cfg(test)]
mod tests;
