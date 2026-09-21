use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrinterFault {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attr: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<i64>,
    pub severity: u8,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct MaterialTray {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_sub_brands: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_id_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_info_idx: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remain: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cali_idx: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drying_temp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drying_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct MaterialUnit {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Firmware humidity value; may be a percentage or an index. Sensor history distinguishes both.
    pub humidity: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_ams_ht: Option<bool>,
    #[serde(default)]
    pub tray: Vec<MaterialTray>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sw_ver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_sub_status: Option<i64>,
    #[serde(default)]
    pub dry_sf_reason: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_target_temp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_filament: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct NozzleInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct NozzleRackSlot {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wear: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_temp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FilaSwitch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<bool>,
    #[serde(default)]
    pub in_slots: Vec<i64>,
    #[serde(default)]
    pub out_extruders: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrintOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spaghetti_detector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_halt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub halt_print_sensitivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_layer_inspector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printing_monitor: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buildplate_marker_detector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_skip_parts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_clumping_detector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_clumping_sensitivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pileup_detector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pileup_sensitivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airprint_detector: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airprint_sensitivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recovery_step_loss: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_tangle_detect: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrinterStatus {
    pub id: u64,
    pub name: String,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_print: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gcode_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_num: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_layers: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperatures: Option<Temperatures>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub hms_errors: Vec<PrinterFault>,
    #[serde(default)]
    pub ams: Vec<MaterialUnit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_exists: Option<bool>,
    #[serde(default)]
    pub vt_tray: Vec<MaterialTray>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdcard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_to_sdcard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timelapse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipcam: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_signal: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wired_network: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub door_open: Option<bool>,
    #[serde(default)]
    pub nozzles: Vec<NozzleInfo>,
    #[serde(default)]
    pub nozzle_rack: Vec<NozzleRackSlot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_options: Option<PrintOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stg_cur: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stg_cur_name: Option<String>,
    #[serde(default)]
    pub stg: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airduct_mode: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_level: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chamber_light: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_extruder: Option<i64>,
    #[serde(default)]
    pub ams_mapping: Vec<i64>,
    #[serde(default)]
    pub ams_extruder_map: std::collections::BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fila_switch: Option<FilaSwitch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_now: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_status_main: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_status_sub: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mc_print_sub_stage: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ams_update: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printable_objects_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooling_fan_speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub big_fan1_speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub big_fan2_speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heatbreak_fan_speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_filament_backup: Option<bool>,
    #[serde(default)]
    pub awaiting_plate_clear: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_drying: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_drying_while_printing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_chamber_heater: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_archive_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_plate_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Temperatures {
    pub bed: Option<f64>,
    pub bed_target: Option<f64>,
    pub nozzle: Option<f64>,
    pub nozzle_target: Option<f64>,
    pub nozzle_2: Option<f64>,
    pub nozzle_2_target: Option<f64>,
    pub chamber: Option<f64>,
    pub chamber_target: Option<f64>,
    pub chamber_heating: Option<bool>,
    pub nozzle_heating: Option<bool>,
}
