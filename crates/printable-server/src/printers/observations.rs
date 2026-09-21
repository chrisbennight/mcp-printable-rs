use super::{
    PrinterService,
    materials::{self, Material, Observation, Unavailable},
};
use crate::error::ToolError;
use bambuddy_api::{
    PrinterFault, PrinterStatus,
    material_types::*,
    materials::{PrintObjects, Storage},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const DIAGNOSTICS_NOTE: &str = "Reported diagnostics do not establish whether dispatch is blocked; use supported actions and Bambuddy's dispatch response. Diagnostic job_id does not establish fault onset.";

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Idle,
    Active,
    Unknown,
}

pub(super) fn activity(status: &PrinterStatus) -> Activity {
    if !status.connected {
        return Activity::Unknown;
    }
    match status.state.as_deref() {
        Some("IDLE" | "FINISH" | "FAILED") => Activity::Idle,
        Some("RUNNING" | "PAUSE" | "PREPARE" | "SLICING") => Activity::Active,
        _ => Activity::Unknown,
    }
}

pub(super) fn state_note(status: &PrinterStatus) -> &'static str {
    if !status.connected {
        return "Disconnected; retained state does not establish current activity.";
    }
    match status.state.as_deref() {
        Some("FAILED") => {
            "Previous job failed or was cancelled; Bambuddy treats FAILED as idle for scheduling, subject to plate clearance and other dispatch checks. No transition to IDLE is required."
        }
        Some("FINISH") => {
            "Previous job finished; Bambuddy treats FINISH as idle for scheduling, subject to plate clearance and other dispatch checks."
        }
        _ => {
            "Reported activity only; Bambuddy checks dispatch conditions. Idle does not establish physical readiness."
        }
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusSection {
    Summary,
    Materials,
    Hardware,
    Environment,
    Operation,
    Health,
    Maintenance,
    Storage,
    Objects,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {
    pub printer_id: u64,
    #[serde(default)]
    pub sections: Vec<StatusSection>,
}
#[derive(Debug, Serialize, JsonSchema)]
pub struct Fault {
    #[serde(flatten)]
    pub reported: PrinterFault,
    pub severity_name: Option<String>,
    pub meaning_available: bool,
}
#[derive(Debug, Serialize, JsonSchema)]
pub struct PrinterObservation {
    pub id: u64,
    pub name: String,
    pub connected: bool,
    pub state: Option<String>,
    pub activity: Activity,
    pub state_note: String,
    pub current_print: Option<String>,
    pub progress_percent: Option<f64>,
    pub remaining_minutes: Option<u64>,
    pub current_layer: Option<u64>,
    pub total_layers: Option<u64>,
    pub awaiting_plate_clear: bool,
    pub nozzles: Vec<NozzleObservation>,
    pub materials: Vec<Material>,
    pub faults: Vec<Fault>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics_note: Option<String>,
    pub fetched_at_unix_ms: u64,
    pub source: String,
    pub source_observed_at: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable_sources: BTreeMap<String, Unavailable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<PrinterStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Observation<PrinterMaintenanceOverview>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<Observation<Storage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects: Option<Observation<PrintObjects>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NozzleObservation {
    #[serde(rename = "type")]
    pub nozzle_type: Option<String>,
    pub diameter: Option<String>,
}

impl PrinterService {
    pub(super) async fn status(&self, p: StatusParams) -> Result<Value, ToolError> {
        let loaded = self
            .loaded_materials(p.printer_id, p.sections.contains(&StatusSection::Materials))
            .await?;
        let status = loaded.status;
        let fetched_at = now_ms();
        let objects = if p.sections.contains(&StatusSection::Objects) {
            Some(Observation::from_result(
                self.read.print_objects(p.printer_id).await,
            ))
        } else {
            None
        };
        let maintenance = if p.sections.contains(&StatusSection::Maintenance) {
            Some(Observation::from_result(
                self.read.maintenance(p.printer_id).await,
            ))
        } else {
            None
        };
        let storage = if p.sections.contains(&StatusSection::Storage) {
            Some(Observation::from_result(
                self.read.printer_storage(p.printer_id).await,
            ))
        } else {
            None
        };
        let mut result = serde_json::to_value(PrinterObservation {
            id: status.id,
            name: status.name.clone(),
            connected: status.connected,
            state: status.state.clone(),
            activity: activity(&status),
            state_note: state_note(&status).into(),
            current_print: status.current_print.clone(),
            progress_percent: status.progress,
            remaining_minutes: status.remaining_time,
            current_layer: status.layer_num,
            total_layers: status.total_layers,
            awaiting_plate_clear: status.awaiting_plate_clear,
            nozzles: status
                .nozzles
                .iter()
                .map(|n| NozzleObservation {
                    nozzle_type: n.nozzle_type.clone(),
                    diameter: n.nozzle_diameter.clone(),
                })
                .collect(),
            materials: loaded.materials,
            faults: status
                .hms_errors
                .iter()
                .cloned()
                .map(|fault| Fault {
                    severity_name: match fault.severity {
                        1 => Some("fatal"),
                        2 => Some("serious"),
                        3 => Some("common"),
                        4 => Some("info"),
                        _ => None,
                    }
                    .map(str::to_owned),
                    meaning_available: fault
                        .description
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty()),
                    reported: fault,
                })
                .collect(),
            fetched_at_unix_ms: fetched_at,
            diagnostics_note: (!status.hms_errors.is_empty()).then(|| DIAGNOSTICS_NOTE.into()),
            source: "bambuddy".into(),
            source_observed_at: None,
            unavailable_sources: loaded
                .sources
                .into_iter()
                .filter_map(|(k, v)| v.map(|v| (k, v)))
                .collect(),
            details: None,
            maintenance,
            storage,
            objects,
        })?;
        let mut detail = serde_json::to_value(status)?;
        let mut keys = vec!["id", "name", "connected"];
        for section in &p.sections {
            keys.extend(match section {
                StatusSection::Summary => vec![],
                StatusSection::Materials => vec![
                    "ams",
                    "vt_tray",
                    "ams_exists",
                    "active_extruder",
                    "ams_mapping",
                    "ams_extruder_map",
                    "fila_switch",
                    "tray_now",
                    "ams_status_main",
                    "ams_status_sub",
                    "mc_print_sub_stage",
                    "last_ams_update",
                    "ams_filament_backup",
                ],
                StatusSection::Hardware => vec![
                    "nozzles",
                    "nozzle_rack",
                    "firmware_version",
                    "developer_mode",
                    "supports_drying",
                    "supports_drying_while_printing",
                    "supports_chamber_heater",
                    "wifi_signal",
                    "wired_network",
                ],
                StatusSection::Environment => vec![
                    "temperatures",
                    "door_open",
                    "airduct_mode",
                    "cooling_fan_speed",
                    "big_fan1_speed",
                    "big_fan2_speed",
                    "heatbreak_fan_speed",
                ],
                StatusSection::Operation => vec![
                    "subtask_name",
                    "gcode_file",
                    "cover_url",
                    "print_options",
                    "stg_cur",
                    "stg_cur_name",
                    "stg",
                    "speed_level",
                    "chamber_light",
                    "timelapse",
                    "ipcam",
                    "current_archive_id",
                    "current_plate_id",
                ],
                StatusSection::Health => {
                    vec!["hms_errors", "connected", "state", "awaiting_plate_clear"]
                }
                StatusSection::Storage => vec!["sdcard", "store_to_sdcard"],
                StatusSection::Objects => vec!["printable_objects_count"],
                StatusSection::Maintenance => vec![],
            });
        }
        if keys.len() > 3 {
            if let Some(object) = detail.as_object_mut() {
                object.retain(|key, _| keys.contains(&key.as_str()));
            }
            result["details"] = detail;
        }
        Ok(result)
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "subject", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistorySubject {
    Sensors {
        printer_id: u64,
        ams_id: u32,
        #[serde(default = "default_hours")]
        hours: u16,
    },
    Maintenance {
        item_id: u64,
    },
    MaterialUsage {
        printer_id: Option<u64>,
        spool_id: Option<u64>,
    },
}
fn default_hours() -> u16 {
    24
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    pub target: HistorySubject,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "materials::default_limit")]
    pub limit: usize,
}
impl PrinterService {
    pub(super) async fn printer_history(&self, p: HistoryParams) -> Result<Value, ToolError> {
        if !(1..=100).contains(&p.limit) {
            return Err(ToolError::Validation("limit must be 1–100".into()));
        }
        let mut result = match p.target {
            HistorySubject::Sensors {
                printer_id,
                ams_id,
                hours,
            } => {
                super::validate_id(printer_id)?;
                if !(1..=168).contains(&hours) {
                    return Err(ToolError::Validation(
                        "sensor history hours must be 1–168".into(),
                    ));
                }
                let mut history = self.read.sensor_history(printer_id, ams_id, hours).await?;
                let page = materials::page(std::mem::take(&mut history.data), p.offset, p.limit)?;
                let mut result = serde_json::to_value(history)?;
                result["page"] = page;
                result
            }
            HistorySubject::Maintenance { item_id } => {
                super::validate_id(item_id)?;
                materials::page(
                    self.read.maintenance_history(item_id).await?,
                    p.offset,
                    p.limit,
                )?
            }
            HistorySubject::MaterialUsage {
                printer_id,
                spool_id,
            } => {
                if let Some(id) = printer_id {
                    super::validate_id(id)?;
                }
                if let Some(id) = spool_id {
                    super::validate_id(id)?;
                }
                let count = p
                    .offset
                    .checked_add(p.limit + 1)
                    .ok_or_else(|| ToolError::Validation("history offset is too large".into()))?;
                let items = self
                    .read
                    .material_usage(printer_id, spool_id, count)
                    .await?;
                let more = items.len() == count;
                let mut items: Vec<_> = items.into_iter().skip(p.offset).take(p.limit).collect();
                if let Some(id) = printer_id {
                    items.retain(|i| i.printer_id.and_then(|v| u64::try_from(v).ok()) == Some(id));
                }
                json!({"items":items,"total":null,"next_offset":more.then_some(p.offset + p.limit)})
            }
        };
        result["fetched_at_unix_ms"] = json!(now_ms());
        Ok(result)
    }
}
