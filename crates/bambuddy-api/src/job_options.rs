use crate::{
    ApiError, Client,
    jobs::{QueueItem, QueueReceipt},
};
use reqwest::Method;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOptions {
    /// Omit to retain the schedule; null clears it to next available.
    #[serde(
        default,
        deserialize_with = "schedule_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub scheduled_time: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_previous_success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_inspect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_offset_cali: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preheat_override: Option<Preheat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preheat_chamber_target_override: Option<u8>,
}
fn schedule_patch<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preheat {
    Inherit,
    On,
    Off,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_levelling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_cali: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vibration_cali: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timelapse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_ams: Option<bool>,
    #[serde(flatten)]
    pub options: JobOptions,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UpdateReceipt {
    pub updated_count: u64,
    pub skipped_count: u64,
    pub message: String,
}
impl Client {
    pub async fn update_jobs(
        &self,
        ids: Vec<u64>,
        patch: &PendingPatch,
    ) -> Result<UpdateReceipt, ApiError> {
        let mut body = serde_json::to_value(patch)
            .map_err(|_| ApiError::InvalidConfiguration("print options"))?;
        body["item_ids"] = serde_json::json!(ids);
        body["manual_start"] = serde_json::json!(true);
        let response = self
            .http
            .patch(self.resource(&["queue", "bulk"])?)
            .header("X-API-Key", self.api_key.0.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|_| ApiError::AmbiguousOutcome)?;
        self.decode_response(response, true).await
    }
    pub async fn start_with_options(
        &self,
        id: u64,
        skip_filament_check: bool,
    ) -> Result<QueueItem, ApiError> {
        let mut url = self.resource(&["queue", &id.to_string(), "start"])?;
        if skip_filament_check {
            url.query_pairs_mut()
                .append_pair("skip_filament_check", "true");
        }
        self.request_once(Method::POST, url, true).await
    }
    pub async fn cancel_batch(&self, id: u64) -> Result<QueueReceipt, ApiError> {
        self.request_once(
            Method::DELETE,
            self.resource(&["queue", "batches", &id.to_string()])?,
            true,
        )
        .await
    }
}
