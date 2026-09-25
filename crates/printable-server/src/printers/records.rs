use super::{PrinterService, materials::page, print_jobs::job_result};
use crate::error::ToolError;
use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    #[default]
    Queue,
    Library,
    Batches,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Queue { id: u64 },
    Library { id: u64 },
    Archive { id: u64 },
    Batch { id: u64 },
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {
    pub print_id: Option<u64>,
    pub target: Option<Target>,
    #[serde(default)]
    pub detail: bool,
    /// Include the source's plate list and material requirements for this plate.
    pub plate: Option<u16>,
}
pub(super) fn compact(mut value: Value, keys: &[&str], detail: bool) -> Value {
    if !detail && let Some(map) = value.as_object_mut() {
        map.retain(|k, _| keys.contains(&k.as_str()));
    }
    value
}
pub(super) fn queue(value: Value, detail: bool) -> Value {
    compact(
        value,
        &[
            "id",
            "printer_id",
            "printer_name",
            "archive_id",
            "library_file_id",
            "status",
            "manual_start",
            "plate_id",
            "ams_mapping",
            "use_ams",
            "waiting_reason",
            "error_message",
            "filament_short",
            "skip_filament_check",
            "project_source",
            "library_file_name",
            "archive_name",
            "position",
            "batch_id",
            "scheduled_time",
        ],
        detail,
    )
}
pub(super) fn library(value: Value, detail: bool) -> Value {
    compact(
        value,
        &[
            "id",
            "filename",
            "file_type",
            "file_size",
            "sliced_for_model",
            "print_name",
            "print_time_seconds",
            "filament_used_grams",
        ],
        detail,
    )
}
pub(super) fn archive(value: Value, detail: bool) -> Value {
    compact(
        value,
        &[
            "id",
            "printer_id",
            "filename",
            "print_name",
            "status",
            "started_at",
            "completed_at",
            "actual_time_seconds",
            "filament_used_grams",
            "failure_reason",
            "run_count",
        ],
        detail,
    )
}

impl PrinterService {
    pub(super) async fn record_status(
        &self,
        p: StatusParams,
        workspace: &Workspace,
    ) -> Result<Value, ToolError> {
        let target = match (p.print_id, p.target) {
            (Some(id), None) => Target::Queue { id },
            (None, Some(t)) => t,
            _ => {
                return Err(ToolError::Validation(
                    "select print_id or one target".into(),
                ));
            }
        };
        let id = match target {
            Target::Queue { id }
            | Target::Library { id }
            | Target::Archive { id }
            | Target::Batch { id } => id,
        };
        super::validate_id(id)?;
        if p.plate == Some(0) {
            return Err(ToolError::Validation("plate must be positive".into()));
        }
        let (mut result, source) = match target {
            Target::Queue { id } => {
                let job = self.read.print_job(id).await?;
                let evidence = super::delivery::observe(workspace, &job)?;
                let source = job
                    .library_file_id
                    .map(|id| (id, false))
                    .or(job.archive_id.map(|id| (id, true)));
                (
                    json!({"print":queue(job_result(workspace,job)?,p.detail),"delivery_evidence":evidence}),
                    source,
                )
            }
            Target::Library { id } => (
                json!({"library_file":library(serde_json::to_value(self.read.library_file(id).await?)?,p.detail)}),
                Some((id, false)),
            ),
            Target::Archive { id } => (
                json!({"archive":archive(serde_json::to_value(self.read.archive(id).await?)?,p.detail)}),
                Some((id, true)),
            ),
            Target::Batch { id } => (json!({"batch":self.read.batch(id).await?}), None),
        };
        if let (Some(plate), Some((source, archive))) = (p.plate, source) {
            let (plates, requirements) = tokio::join!(
                self.read.plates(source, archive),
                self.read.filament_requirements(source, archive, plate)
            );
            result["plates"] =
                serde_json::to_value(super::materials::Observation::from_result(plates))?;
            result["requirements"] =
                serde_json::to_value(super::materials::Observation::from_result(requirements))?;
        }
        result["fetched_at_unix_ms"] = json!(super::observations::now_ms());
        Ok(result)
    }
    pub(super) async fn record_list(
        &self,
        p: super::print_jobs::ListJobsParams,
        workspace: &Workspace,
    ) -> Result<Value, ToolError> {
        if !(1..=100).contains(&p.limit) {
            return Err(ToolError::Validation("limit must be 1–100".into()));
        }
        if let Some(id) = p.printer_id {
            super::validate_id(id)?;
        }
        let mut result = match p.collection {
            Collection::Queue => {
                let status = p.status.map(serde_json::to_value).transpose()?;
                let items = self
                    .read
                    .print_jobs(p.printer_id, status.as_ref().and_then(Value::as_str))
                    .await?;
                let items = items
                    .into_iter()
                    .filter(|i| p.batch_id.is_none_or(|id| i.batch_id == Some(id as i64)))
                    .map(|i| job_result(workspace, i).map(|i| queue(i, p.detail)))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut result = page(items, p.offset, p.limit)?;
                result["prints"] = result["items"].take();
                result.as_object_mut().unwrap().remove("items");
                result
            }
            Collection::Library => page(
                self.read
                    .library_files(p.folder_id)
                    .await?
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(|v| library(v, p.detail))
                    .collect(),
                p.offset,
                p.limit,
            )?,
            Collection::Batches => page(self.read.batches().await?, p.offset, p.limit)?,
        };
        result["collection"] = serde_json::to_value(p.collection)?;
        Ok(result)
    }
}
