use crate::{ApiError, Client, ControlReceipt};
use reqwest::Method;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrinterCommand {
    /// Request HMS/print-error clearing; backend acceptance does not prove device resolution.
    ClearErrors,
    RefreshFilament {
        ams_id: u32,
        slot_id: u32,
    },
    LoadFilament {
        mapping_id: u32,
    },
    UnloadFilament,
    /// Use the canonical fault identifier and an action returned by printer status.
    FaultAction {
        full_code: String,
        action: String,
        job_id: Option<String>,
    },
}

impl Client {
    pub async fn refresh_status(&self, id: u64) -> Result<(), ApiError> {
        #[derive(Deserialize)]
        enum Status {
            #[serde(rename = "refresh_requested")]
            Requested,
        }
        #[derive(Deserialize)]
        struct Receipt {
            status: Status,
        }
        let receipt: Receipt = self
            .request_once(
                Method::POST,
                self.endpoint(&[&id.to_string(), "refresh-status"])?,
                true,
            )
            .await?;
        let Status::Requested = receipt.status;
        Ok(())
    }

    pub async fn printer_command(
        &self,
        id: u64,
        command: &PrinterCommand,
    ) -> Result<ControlReceipt, ApiError> {
        let id = id.to_string();
        let url = match command {
            PrinterCommand::ClearErrors => self.endpoint(&[&id, "hms", "clear"])?,
            PrinterCommand::RefreshFilament { ams_id, slot_id } => self.endpoint(&[
                &id,
                "ams",
                &ams_id.to_string(),
                "slot",
                &slot_id.to_string(),
                "refresh",
            ])?,
            PrinterCommand::LoadFilament { mapping_id } => {
                let mut url = self.endpoint(&[&id, "ams", "load"])?;
                url.query_pairs_mut()
                    .append_pair("tray_id", &mapping_id.to_string());
                url
            }
            PrinterCommand::UnloadFilament => self.endpoint(&[&id, "ams", "unload"])?,
            PrinterCommand::FaultAction {
                full_code,
                action,
                job_id,
            } => {
                let response=self.http.post(self.endpoint(&[&id,"hms","execute-action"])?).header("X-API-Key",self.api_key.0.as_str()).json(&serde_json::json!({"print_error":full_code,"action":action,"job_id":job_id})).send().await.map_err(|_|ApiError::AmbiguousOutcome)?;
                return self.decode_response(response, true).await;
            }
        };
        self.request_once(Method::POST, url, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiKey;
    use serde_json::json;
    use std::time::Duration;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn refresh_and_recovery_report_rejection_or_uncertainty_without_retry() {
        for (status, body, expected) in [
            (500, json!({}), ApiError::AmbiguousOutcome),
            (
                200,
                json!({"unexpected":"receipt"}),
                ApiError::AmbiguousOutcome,
            ),
            (403, json!({}), ApiError::Forbidden),
            (404, json!({}), ApiError::NotFound),
            (
                400,
                json!({"detail":"Printer not connected"}),
                ApiError::Rejected(crate::rejection::Rejection::decode(
                    400,
                    br#"{"detail":"Printer not connected"}"#,
                )),
            ),
        ] {
            let server = MockServer::start().await;
            for endpoint in ["refresh-status", "hms/clear"] {
                Mock::given(method("POST"))
                    .and(path(format!("/api/v1/printers/1/{endpoint}")))
                    .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            let client = Client::new(
                server.uri().parse().unwrap(),
                ApiKey::new("fixture".into()).unwrap(),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(client.refresh_status(1).await.unwrap_err(), expected);
            assert_eq!(
                client
                    .printer_command(1, &PrinterCommand::ClearErrors)
                    .await
                    .unwrap_err(),
                expected
            );
            server.verify().await;
        }
    }
}
