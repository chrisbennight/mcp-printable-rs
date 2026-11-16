//! Facts extracted from the packaged engine, without guessing job applicability.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    pub status: String,
    pub applicability: String,
    pub native_type: Option<String>,
    #[serde(rename = "type")]
    pub value_type: Option<String>,
    pub vector: Option<bool>,
    pub nullable: Option<bool>,
    pub source: Option<String>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub unit: Option<String>,
    pub gui_type: Option<String>,
    pub ratio_over: Option<String>,
    pub choices: Option<Vec<String>>,
    pub suggestions: Option<Vec<String>>,
    pub default: Option<NativeDefault>,
    pub reason: Option<String>,
    #[serde(default)]
    pub unknown_fields: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeDefault {
    pub value: Value,
    pub source: String,
}

impl Definition {
    /// Only enforce literal constraints whose meaning is independent of the job.
    /// Ratio-based values and serialized compound values remain native validation.
    pub fn validate(&self, key: &str, value: &Value) -> Result<(), ToolError> {
        if self.status != "source_literal" {
            return Ok(());
        }
        let kind = self.value_type.as_deref();
        let values: Vec<&str> = if let Some(value) = value.as_str() {
            vec![value]
        } else {
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        };
        for value in values {
            let value = value.trim();
            if value == "nil" && self.nullable != Some(false) {
                continue;
            }
            if self.vector == Some(true) && value.contains([',', ';']) {
                continue;
            }
            let valid = match kind {
                Some("enum") => self
                    .choices
                    .as_ref()
                    .is_none_or(|choices| choices.iter().any(|choice| choice == value)),
                Some("number" | "integer" | "percent" | "number_or_percent") => {
                    if kind == Some("number_or_percent") && value.ends_with('%') {
                        continue;
                    }
                    let numeric = if kind == Some("percent") {
                        value.strip_suffix('%').unwrap_or(value).trim()
                    } else {
                        value
                    };
                    numeric.parse::<f64>().is_ok_and(|number| {
                        number.is_finite()
                            && (kind != Some("integer") || number.fract() == 0.)
                            && self.minimum.is_none_or(|min| number >= min)
                            && self.maximum.is_none_or(|max| number <= max)
                    })
                }
                _ => true,
            };
            if !valid {
                return Err(ToolError::Validation(format!(
                    "{key} violates the packaged Orca setting definition; inspect slice.settings"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_constraints_do_not_turn_open_suggestions_into_closed_choices() {
        let definition: Definition = serde_json::from_value(json!({
            "status":"source_literal","applicability":"unknown","type":"number",
            "minimum":-1,"suggestions":["-1"]
        }))
        .unwrap();
        for value in ["-1", "0", "8.5"] {
            definition.validate("brim", &json!(value)).unwrap();
        }
        for value in ["-2", "NaN", "Infinity", "unknown"] {
            assert!(definition.validate("brim", &json!(value)).is_err());
        }
    }

    #[test]
    fn nullable_vectors_and_unknown_definitions_remain_distinct() {
        let mut definition: Definition = serde_json::from_value(json!({
            "status":"source_literal","applicability":"unknown","type":"integer",
            "vector":true,"nullable":true,"minimum":0,"maximum":10
        }))
        .unwrap();
        definition
            .validate("override", &json!(["nil", "3"]))
            .unwrap();
        assert!(definition.validate("override", &json!(["11"])).is_err());
        definition.status = "unknown".into();
        definition.validate("override", &json!(["11"])).unwrap();
    }

    #[test]
    fn absolute_number_or_percent_values_use_literal_bounds() {
        let definition: Definition = serde_json::from_value(json!({
            "status":"source_literal","applicability":"unknown","type":"number_or_percent",
            "minimum":0,"maximum":10
        }))
        .unwrap();
        for value in ["0", "0.4", "10"] {
            definition.validate("width", &json!(value)).unwrap();
        }
        for value in ["-1", "11", "NaN"] {
            assert!(definition.validate("width", &json!(value)).is_err());
        }
        definition.validate("width", &json!("200%")).unwrap();
    }
}
