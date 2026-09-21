use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct FilamentDeficit {
    pub slot_id: u32,
    pub ams_id: Option<u32>,
    pub tray_id: Option<u32>,
    pub filament_type: String,
    pub required_grams: f64,
    pub remaining_grams: Option<f64>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Rejection {
    pub status: u16,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub deficit: Vec<FilamentDeficit>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fields: Vec<String>,
}
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl Rejection {
    pub(crate) fn decode(status: u16, bytes: &[u8]) -> Self {
        let mut result = Self {
            status,
            code: "upstream_rejected".into(),
            message: "Bambuddy rejected the request; inspect the selected job and options".into(),
            deficit: vec![],
            fields: vec![],
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return result;
        };
        let detail = &value["detail"];
        if status == 409 && detail["code"] == "insufficient_filament" {
            if let Ok(mut deficit) =
                serde_json::from_value::<Vec<FilamentDeficit>>(detail["deficit"].clone())
                && deficit.len() <= 64
                && deficit.iter().all(|d| {
                    d.required_grams.is_finite()
                        && d.required_grams >= 0.0
                        && d.remaining_grams.is_none_or(|g| g.is_finite() && g >= 0.0)
                })
            {
                for d in &mut deficit {
                    d.filament_type = d.filament_type.chars().take(64).collect();
                }
                result.code = "insufficient_filament".into();
                result.message = "Assigned filament is insufficient for the print".into();
                result.deficit = deficit;
            }
        } else if status == 422 {
            result.code = "invalid_options".into();
            result.message = "Bambuddy rejected one or more option fields".into();
            if let Some(errors) = detail.as_array() {
                result.fields = errors
                    .iter()
                    .take(32)
                    .filter_map(|e| e["loc"].as_array())
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .filter(|s| {
                        s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    })
                    .map(str::to_owned)
                    .collect();
            }
        } else if let Some(message) = detail.as_str() {
            if let Some(state) = message
                .strip_prefix("Can only start pending items, current status: '")
                .and_then(|s| s.strip_suffix('\''))
                .filter(|s| matches!(*s, "printing" | "completed" | "failed" | "cancelled"))
            {
                result.code = "queue_item_not_pending".into();
                result.message =
                    format!("Queue item is {state}; only pending items can be started");
            } else if let Some(state) = message
                .strip_prefix("Printer is not awaiting plate-clear acknowledgment (state=")
                .and_then(|s| s.strip_suffix(')'))
                .filter(|s| {
                    matches!(
                        *s,
                        "IDLE"
                            | "FINISH"
                            | "FAILED"
                            | "RUNNING"
                            | "PAUSE"
                            | "PREPARE"
                            | "SLICING"
                            | "unknown"
                    )
                })
            {
                result.code = "plate_clear_not_pending".into();
                result.message =
                    format!("Printer is not awaiting plate-clear acknowledgement (state={state})");
            }
            match message {
                "Printer not connected" => {
                    result.code = "printer_not_connected".into();
                    result.message = message.into();
                }
                "Can only update pending items"
                | "Cannot specify both printer_id and target_model"
                | "Printer not found"
                | "Queue item not found" => result.message = message.into(),
                _ => {}
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_rejections_preserve_only_supported_reasons() {
        for (message, expected) in [
            ("Printer not connected", "printer_not_connected"),
            (
                "Can only start pending items, current status: 'printing'",
                "queue_item_not_pending",
            ),
            (
                "Printer is not awaiting plate-clear acknowledgment (state=RUNNING)",
                "plate_clear_not_pending",
            ),
            (
                "Can only start pending items, current status: 'private-value'",
                "upstream_rejected",
            ),
            ("private-value", "upstream_rejected"),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({"detail":message})).unwrap();
            let rejection = Rejection::decode(400, &bytes);
            assert_eq!(rejection.code, expected);
            assert!(!rejection.message.contains("private-value"));
        }
    }
    #[test]
    fn deficit_is_actionable_and_validation_does_not_echo_inputs() {
        let rejection=Rejection::decode(409,br#"{"detail":{"code":"insufficient_filament","deficit":[{"slot_id":1,"ams_id":0,"tray_id":2,"filament_type":"PLA","required_grams":25,"remaining_grams":10}],"unrelated":"discard"}}"#);
        assert_eq!(rejection.code, "insufficient_filament");
        assert_eq!(rejection.deficit[0].required_grams, 25.0);
        let unknown = Rejection::decode(409, br#"{"detail":{"code":"insufficient_filament","deficit":[{"slot_id":1,"filament_type":"PLA","required_grams":25,"remaining_grams":null}]}}"#);
        assert_eq!(unknown.code, "insufficient_filament");
        assert_eq!(unknown.deficit[0].remaining_grams, None);
        assert_eq!(
            Rejection::decode(409, b"not JSON").code,
            "upstream_rejected"
        );
        let validation=Rejection::decode(422,br#"{"detail":[{"loc":["body","ams_mapping"],"input":"private-input","msg":"private-message"}]}"#);
        assert_eq!(validation.fields, vec!["body", "ams_mapping"]);
        assert!(
            !serde_json::to_string(&validation)
                .unwrap()
                .contains("private")
        );
    }
}
