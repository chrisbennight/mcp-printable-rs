use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SpoolKProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extruder: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_diameter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cali_idx: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting_id: Option<String>,
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Spool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rgba: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_colors: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_weight: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_weight: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_weight_catalog_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_used_baseline: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slicer_filament: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slicer_filament_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temp_max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_per_kg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_locked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scale_weight: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_weighed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_stock_threshold_pct: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<i64>,
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_full: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encode_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub k_profiles: Vec<SpoolKProfile>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SpoolAssignment {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool: Option<Spool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_config: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_label: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct MaintenanceStatus {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_type_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_type_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_type_icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_type_wiki_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours_since_maintenance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours_until_due: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_since_maintenance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_until_due: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_due: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_warning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_performed_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct PrinterMaintenanceOverview {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_print_hours: Option<f64>,
    #[serde(default)]
    pub maintenance_items: Vec<MaintenanceStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct MaintenanceHistoryResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_maintenance_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours_at_maintenance: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SpoolUsageHistoryResponse {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct AMSHistoryPoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity_raw: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct AMSHistoryResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ams_id: Option<i64>,
    #[serde(default)]
    pub data: Vec<AMSHistoryPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_humidity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_humidity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_humidity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_temperature: Option<f64>,
}
