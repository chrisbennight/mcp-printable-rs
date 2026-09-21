use super::{PrinterService, validate_id};
use crate::error::ToolError;
use bambuddy_api::{BambuddyApi, control::PrinterCommand, job_options::PendingPatch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    pub print_id: u64,
    /// Explicitly override Bambuddy's filament deficit warning for this job.
    #[serde(default)]
    pub skip_filament_check: bool,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    pub print_id: Option<u64>,
    pub batch_id: Option<u64>,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpdateTarget {
    Queue { print_ids: Vec<u64> },
    Batch { batch_id: u64 },
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateParams {
    pub target: UpdateTarget,
    pub patch: PendingPatch,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlParams {
    pub printer_id: u64,
    pub operation: PrinterCommand,
}
fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}
impl PrinterService {
    pub(super) async fn update(&self, p: UpdateParams) -> Result<Value, ToolError> {
        if let Some(id) = p.patch.printer_id {
            validate_id(id)?;
        }
        if p.patch
            .options
            .preheat_chamber_target_override
            .is_some_and(|n| n > 60)
        {
            return Err(invalid("chamber preheat target must be 0–60 Celsius"));
        }
        if serde_json::to_value(&p.patch)?
            .as_object()
            .is_none_or(|p| p.is_empty())
        {
            return Err(invalid("select at least one pending-job change"));
        }
        let ids = match p.target {
            UpdateTarget::Queue { print_ids } => print_ids,
            UpdateTarget::Batch { batch_id } => {
                validate_id(batch_id)?;
                self.read
                    .print_jobs(None, Some("pending"))
                    .await?
                    .into_iter()
                    .filter(|p| p.batch_id.and_then(|i| u64::try_from(i).ok()) == Some(batch_id))
                    .map(|p| p.id)
                    .collect()
            }
        };
        if ids.is_empty() {
            return Err(invalid("no pending jobs selected"));
        }
        for id in &ids {
            validate_id(*id)?;
        }
        // Keep edits staged even if a job was previously automatic.
        let result = self.control.update_jobs(ids.clone(), &p.patch).await?;
        Ok(json!({"print_ids":ids,"update":result,"may_start_printing":false}))
    }
    pub(super) async fn cancel(&self, p: CancelParams) -> Result<Value, ToolError> {
        match (p.print_id, p.batch_id) {
            (Some(id), None) => {
                validate_id(id)?;
                Ok(json!({"print_id":id,"result":self.control.cancel_print_job(id).await?}))
            }
            (None, Some(id)) => {
                validate_id(id)?;
                Ok(json!({"batch_id":id,"result":self.control.cancel_batch(id).await?}))
            }
            _ => Err(invalid("select print_id or batch_id")),
        }
    }
    pub(super) async fn command(&self, p: ControlParams) -> Result<Value, ToolError> {
        validate_id(p.printer_id)?;
        match &p.operation {
            PrinterCommand::FaultAction {
                full_code,
                action,
                job_id,
            } => {
                if !matches!(full_code.len(), 8 | 16)
                    || !full_code.bytes().all(|c| c.is_ascii_hexdigit())
                    || action.is_empty()
                    || action.len() > 64
                    || job_id.as_ref().is_some_and(|s| s.len() > 64)
                {
                    return Err(invalid(
                        "use a canonical fault code and action from printer status",
                    ));
                }
                let status = self.read.printer_status(p.printer_id).await?;
                if !status.hms_errors.iter().any(|f| {
                    f.full_code
                        .as_ref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(full_code))
                        && f.actions.contains(action)
                        && f.job_id == *job_id
                }) {
                    return Err(invalid(
                        "fault action no longer matches the current printer fault",
                    ));
                }
            }
            PrinterCommand::RefreshFilament { ams_id, slot_id } => {
                let status = self.read.printer_status(p.printer_id).await?;
                if !status
                    .ams
                    .iter()
                    .any(|u| u.id == *ams_id && u.tray.iter().any(|t| t.id == *slot_id))
                {
                    return Err(invalid("AMS slot is not reported by this printer"));
                }
            }
            PrinterCommand::LoadFilament { mapping_id } => {
                if *mapping_id > 255 {
                    return Err(invalid(
                        "mapping_id must be a reported printer material location",
                    ));
                }
                let loaded = self.loaded_materials(p.printer_id, false).await?;
                if !loaded
                    .materials
                    .iter()
                    .any(|m| m.mapping_id == Some(*mapping_id))
                {
                    return Err(invalid("material location is not reported by this printer"));
                }
            }
            PrinterCommand::UnloadFilament | PrinterCommand::ClearErrors => {}
        }
        let result = self
            .control
            .printer_command(p.printer_id, &p.operation)
            .await?;
        let mut response = json!({"printer_id":p.printer_id,"action":"control","operation":p.operation,"accepted":result.success,"message":super::bounded(result.message),"outcome":if result.success{super::schema::OperationOutcome::Accepted}else{super::schema::OperationOutcome::Rejected}});
        if matches!(p.operation, PrinterCommand::ClearErrors) {
            response["effect_note"] = json!(
                "Requests clean_print_error and clears Bambuddy's local diagnostic list on acceptance; device resolution is unconfirmed. This is not plate acknowledgement or a prerequisite for replacement printing."
            );
        }
        Ok(response)
    }
}
