use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FileDuplicate {
    pub id: u64,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct TagSummary {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct LibraryFile {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_external: Option<bool>,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub file_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_printed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<Vec<FileDuplicate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_time_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliced_for_model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FileListResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_external: Option<bool>,
    pub filename: String,
    pub file_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_time_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliced_for_model: Option<String>,
    #[serde(default)]
    pub tags: Vec<TagSummary>,
}

/// A calibration that can be skipped (`off`), forced on every print (`on`), or
/// left to the printer, which skips it when it ran recently (`auto`).
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Calibration {
    Off,
    On,
    Auto,
}
impl From<bool> for Calibration {
    fn from(on: bool) -> Self {
        if on { Self::On } else { Self::Off }
    }
}
impl<'de> Deserialize<'de> for Calibration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Reported;
        impl serde::de::Visitor<'_> for Reported {
            type Value = Calibration;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"off\", \"on\", \"auto\" or a boolean")
            }
            // Bambuddy releases without the auto state report a boolean.
            fn visit_bool<E>(self, on: bool) -> Result<Calibration, E> {
                Ok(on.into())
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Calibration, E> {
                match value {
                    "off" => Ok(Calibration::Off),
                    "on" => Ok(Calibration::On),
                    "auto" => Ok(Calibration::Auto),
                    _ => Err(E::unknown_variant(value, &["off", "on", "auto"])),
                }
            }
        }
        deserializer.deserialize_any(Reported)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct QueueItem {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_filament_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_overrides: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_previous_success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_off_after: Option<bool>,
    pub manual_start: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_short: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_filament_check: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_mapping: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plate_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_levelling: Option<Calibration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_cali: Option<Calibration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vibration_cali: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_inspect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timelapse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_ams: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_offset_cali: Option<Calibration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preheat_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preheat_chamber_target_override: Option<i64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file_thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_time_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliced_for_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub been_jumped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcode_injection: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_library_after_dispatch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_mapping: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrintBatch {
    pub id: u64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printing_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct ArchiveDuplicate {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrintArchive {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timelapse_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_3mf_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f3d_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<Vec<ArchiveDuplicate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_sequence: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_archive_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_time_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_time_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_accuracy: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_layers: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_temperature: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temperature: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliced_for_model: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub makerworld_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub designer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub photos: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_kwh: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_filament_actual_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successful_run_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_run_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrintRun {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<u64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_grams: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_kwh: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrintRuns {
    #[serde(default)]
    pub items: Vec<PrintRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FileMetadata {
    pub sliced_for_model: Option<String>,
    pub print_name: Option<String>,
    pub print_time_seconds: Option<f64>,
    pub filament_used_grams: Option<f64>,
    pub filament_used_mm: Option<f64>,
    pub filament_type: Option<String>,
    pub filament_color: Option<String>,
    pub layer_height: Option<f64>,
    pub total_layers: Option<u64>,
    pub nozzle_diameter: Option<f64>,
    pub bed_temperature: Option<f64>,
    pub bed_type: Option<String>,
    pub nozzle_temperature: Option<f64>,
    pub makerworld_url: Option<String>,
    pub designer: Option<String>,
    pub used_embedded_settings: Option<bool>,
    pub extra_data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FilamentRequirement {
    pub slot_id: u32,
    #[serde(rename = "type")]
    pub material_type: Option<String>,
    pub color: Option<String>,
    pub used_grams: Option<f64>,
    pub used_meters: Option<f64>,
    pub tray_info_idx: Option<String>,
    pub used_in_plate: Option<bool>,
    pub nozzle_id: Option<i32>,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FilamentRequirements {
    pub file_id: Option<u64>,
    pub archive_id: Option<u64>,
    pub filename: Option<String>,
    pub plate_id: Option<u16>,
    #[serde(default)]
    pub filaments: Vec<FilamentRequirement>,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Plate {
    pub index: u16,
    pub name: Option<String>,
    #[serde(default)]
    pub objects: Vec<String>,
    pub object_count: Option<u64>,
    pub has_thumbnail: Option<bool>,
    pub thumbnail_url: Option<String>,
    pub print_time_seconds: Option<f64>,
    pub filament_used_grams: Option<f64>,
    #[serde(default)]
    pub filaments: Vec<FilamentRequirement>,
    pub bed_type: Option<String>,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Plates {
    pub file_id: Option<u64>,
    pub archive_id: Option<u64>,
    pub filename: Option<String>,
    #[serde(default)]
    pub plates: Vec<Plate>,
    pub is_multi_plate: Option<bool>,
    pub embedded_printer: Option<serde_json::Value>,
    pub embedded_process: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn queue_records_read_current_and_earlier_calibration_forms() {
        let current: QueueItem = serde_json::from_value(json!({
            "id": 12, "printer_id": 1, "target_model": null, "target_location": null,
            "required_filament_types": null, "filament_overrides": null, "waiting_reason": null,
            "archive_id": null, "library_file_id": 9, "cost_center_id": null,
            "estimated_cost": null, "position": 1, "scheduled_time": null,
            "require_previous_success": false, "auto_off_after": false, "manual_start": true,
            "filament_short": false, "skip_filament_check": false, "ams_mapping": [0],
            "plate_id": 1, "bed_levelling": "on", "flow_cali": "off", "vibration_cali": true,
            "layer_inspect": false, "timelapse": false, "use_ams": true,
            "nozzle_offset_cali": "auto", "confirm_outcome": false, "preheat_override": "inherit",
            "preheat_chamber_target_override": null, "status": "pending", "started_at": null,
            "completed_at": null, "error_message": null, "created_at": "2026-10-12T14:30:00Z",
            "archive_has_slicer_ams_mapping": false, "variants": [], "been_jumped": false,
            "nozzle_mapping": null, "nozzle_rack_choice": {"1": 2}
        }))
        .unwrap();
        assert_eq!(current.bed_levelling, Some(Calibration::On));
        assert_eq!(current.flow_cali, Some(Calibration::Off));
        assert_eq!(current.nozzle_offset_cali, Some(Calibration::Auto));
        let reported = serde_json::to_value(&current).unwrap();
        assert_eq!(reported["nozzle_offset_cali"], "auto");
        assert_eq!(reported["bed_levelling"], "on");

        let earlier: QueueItem = serde_json::from_value(json!({
            "id": 3, "status": "completed", "manual_start": false,
            "bed_levelling": true, "flow_cali": false
        }))
        .unwrap();
        assert_eq!(earlier.bed_levelling, Some(Calibration::On));
        assert_eq!(earlier.flow_cali, Some(Calibration::Off));
        assert_eq!(earlier.nozzle_offset_cali, None);

        assert!(
            serde_json::from_value::<QueueItem>(json!({
                "id": 4, "status": "pending", "manual_start": true, "flow_cali": "sometimes"
            }))
            .is_err()
        );
    }
}
