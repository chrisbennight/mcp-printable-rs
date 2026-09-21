use crate::{ApiError, Client, material_types::*};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SlotPreset {
    pub ams_id: u32,
    pub tray_id: u32,
    pub preset_id: String,
    pub preset_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FilamentName {
    pub filament_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct InventoryRemaining {
    pub inventory_remain_g: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub source: String,
    pub filament_type: Option<String>,
    pub filament_colour: Option<String>,
    pub compatible_printers: Option<Vec<String>>,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct PresetSlots {
    #[serde(default)]
    pub filament: Vec<Preset>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Presets {
    #[serde(default)]
    pub local: PresetSlots,
    #[serde(default)]
    pub cloud: PresetSlots,
    #[serde(default)]
    pub orca_cloud: PresetSlots,
    #[serde(default)]
    pub standard: PresetSlots,
    pub cloud_status: Option<String>,
    pub orca_cloud_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Storage {
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

impl Client {
    pub async fn slot_presets(&self, id: u64) -> Result<BTreeMap<String, SlotPreset>, ApiError> {
        self.read(self.endpoint(&[&id.to_string(), "slot-presets"])?)
            .await
    }
    pub async fn ams_labels(&self, id: u64) -> Result<BTreeMap<String, String>, ApiError> {
        self.read(self.endpoint(&[&id.to_string(), "ams-labels"])?)
            .await
    }
    pub async fn inventory_remaining(&self, id: u64) -> Result<InventoryRemaining, ApiError> {
        self.read(self.endpoint(&[&id.to_string(), "inventory-remain"])?)
            .await
    }
    pub async fn filament_names(&self) -> Result<Vec<FilamentName>, ApiError> {
        self.read(self.resource(&["cloud", "builtin-filaments"])?)
            .await
    }
    pub async fn filament_presets(&self) -> Result<Presets, ApiError> {
        self.read(self.resource(&["slicer", "presets"])?).await
    }
    pub async fn spools(&self, archived: bool) -> Result<Vec<Spool>, ApiError> {
        let mut url = self.resource(&["inventory", "spools"])?;
        url.query_pairs_mut()
            .append_pair("include_archived", if archived { "true" } else { "false" });
        self.read(url).await
    }
    pub async fn assignments(&self, id: u64) -> Result<Vec<SpoolAssignment>, ApiError> {
        let mut url = self.resource(&["inventory", "assignments"])?;
        url.query_pairs_mut()
            .append_pair("printer_id", &id.to_string());
        self.read(url).await
    }
    pub async fn maintenance(&self, id: u64) -> Result<PrinterMaintenanceOverview, ApiError> {
        self.read(self.resource(&["maintenance", "printers", &id.to_string()])?)
            .await
    }
    pub async fn maintenance_history(
        &self,
        item: u64,
    ) -> Result<Vec<MaintenanceHistoryResponse>, ApiError> {
        self.read(self.resource(&["maintenance", "items", &item.to_string(), "history"])?)
            .await
    }
    pub async fn sensor_history(
        &self,
        printer: u64,
        ams: u32,
        hours: u16,
    ) -> Result<AMSHistoryResponse, ApiError> {
        let mut url = self.resource(&["ams-history", &printer.to_string(), &ams.to_string()])?;
        url.query_pairs_mut()
            .append_pair("hours", &hours.to_string());
        self.read(url).await
    }
    pub async fn material_usage(
        &self,
        printer: Option<u64>,
        spool: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SpoolUsageHistoryResponse>, ApiError> {
        let spool_id = spool.map(|id| id.to_string());
        let mut url = match &spool_id {
            Some(id) => self.resource(&["inventory", "spools", id, "usage"]),
            None => self.resource(&["inventory", "usage"]),
        }?;
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        if let Some(id) = printer {
            url.query_pairs_mut()
                .append_pair("printer_id", &id.to_string());
        }
        self.read(url).await
    }
    pub async fn printer_storage(&self, id: u64) -> Result<Storage, ApiError> {
        self.read(self.endpoint(&[&id.to_string(), "storage"])?)
            .await
    }
}
impl Client {
    pub async fn printer_models(&self) -> Result<BTreeMap<String, String>, ApiError> {
        self.read(self.resource(&["slicer", "printer-models"])?)
            .await
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PrintObject {
    pub id: u64,
    pub name: String,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub skipped: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PrintObjects {
    pub objects: Vec<PrintObject>,
    pub total: u64,
    pub skipped_count: u64,
    pub is_printing: bool,
    pub bbox_all: Option<serde_json::Value>,
}
impl Client {
    pub async fn print_objects(&self, id: u64) -> Result<PrintObjects, ApiError> {
        self.read(self.endpoint(&[&id.to_string(), "print", "objects"])?)
            .await
    }
}
