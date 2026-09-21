use super::PrepareParams;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Native surface choices in the packaged Orca release. Third-party plates use
/// their manufacturer's corresponding surface and temperature recommendations.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum BuildPlate {
    #[serde(rename = "Cool Plate")]
    Cool,
    #[serde(rename = "Engineering Plate")]
    Engineering,
    #[serde(rename = "High Temp Plate")]
    HighTemp,
    #[serde(rename = "Textured PEI Plate")]
    TexturedPei,
    #[serde(rename = "Textured Cool Plate")]
    TexturedCool,
    #[serde(rename = "Supertack Plate")]
    Supertack,
}

impl BuildPlate {
    pub const ALL: [Self; 6] = [
        Self::Cool,
        Self::Engineering,
        Self::HighTemp,
        Self::TexturedPei,
        Self::TexturedCool,
        Self::Supertack,
    ];

    pub fn native_name(self) -> &'static str {
        match self {
            Self::Cool => "Cool Plate",
            Self::Engineering => "Engineering Plate",
            Self::HighTemp => "High Temp Plate",
            Self::TexturedPei => "Textured PEI Plate",
            Self::TexturedCool => "Textured Cool Plate",
            Self::Supertack => "Supertack Plate",
        }
    }

    pub fn temperature_key(self) -> &'static str {
        match self {
            Self::Cool => "cool_plate_temp",
            Self::Engineering => "eng_plate_temp",
            Self::HighTemp => "hot_plate_temp",
            Self::TexturedPei => "textured_plate_temp",
            Self::TexturedCool => "textured_cool_plate_temp",
            Self::Supertack => "supertack_plate_temp",
        }
    }
}

pub fn select(value: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| value.get(*key).map(|v| ((*key).into(), v.clone())))
            .collect(),
    )
}

pub fn summary(
    params: &PrepareParams,
    printer: &Value,
    process: &Value,
    filaments: &[Value],
) -> Value {
    let temperature = params.build_plate.temperature_key();
    let initial = format!("{temperature}_initial_layer");
    json!({
        "build_plate": params.build_plate,
        "project_plate": params.plate,
        "printer": select(printer, &["name", "printer_model", "nozzle_diameter"]),
        "process": select(process, &["name", "layer_height", "initial_layer_print_height", "wall_loops", "sparse_infill_density", "sparse_infill_pattern", "enable_support", "support_type", "support_on_build_plate_only", "brim_type"]),
        "filaments": filaments.iter().enumerate().map(|(slot, filament)| json!({
            "slice_material_index": slot,
            "profile": select(filament, &["name", "filament_type", "filament_vendor", "nozzle_temperature", "nozzle_temperature_initial_layer", "fan_min_speed", "fan_max_speed", "close_fan_the_first_x_layers"]),
            "bed_temperature": filament.get(temperature),
            "bed_temperature_initial_layer": filament.get(&initial),
            "bed_temperature_setting": temperature,
        })).collect::<Vec<_>>(),
        "auto_arrange": params.auto_arrange,
        "auto_orient": params.auto_orient,
        "settings_source": "resolved_profiles_and_explicit_selection",
    })
}
