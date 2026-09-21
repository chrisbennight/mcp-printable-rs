pub use crate::printer_types::{MaterialTray, MaterialUnit};
use crate::{ApiError, Client, ControlReceipt};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use zeroize::Zeroizing;

impl Client {
    pub async fn clear_plate(&self, printer_id: u64) -> Result<ControlReceipt, ApiError> {
        self.request_once(
            Method::POST,
            self.endpoint(&[&printer_id.to_string(), "clear-plate"])?,
            true,
        )
        .await
    }

    pub async fn camera_snapshot(&self, printer_id: u64) -> Result<Vec<u8>, ApiError> {
        #[derive(Deserialize)]
        struct StreamToken {
            token: String,
        }
        let response: StreamToken = self
            .request_once(
                Method::POST,
                self.endpoint(&["camera", "stream-token"])?,
                false,
            )
            .await?;
        let token = Zeroizing::new(response.token);
        let mut endpoint = self.endpoint(&[&printer_id.to_string(), "camera", "snapshot"])?;
        endpoint.query_pairs_mut().append_pair("token", &token);
        let mut response = self
            .http
            .get(endpoint)
            .send()
            .await
            .map_err(|_| ApiError::Unavailable)?;
        match response.status() {
            status if status.is_success() => {}
            StatusCode::UNAUTHORIZED => return Err(ApiError::Authentication),
            StatusCode::FORBIDDEN => return Err(ApiError::Forbidden),
            StatusCode::NOT_FOUND => return Err(ApiError::NotFound),
            _ => return Err(ApiError::Unavailable),
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.split(';').next() != Some("image/jpeg"))
        {
            return Err(ApiError::IncompatibleResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ApiError::Unavailable)? {
            if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                return Err(ApiError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err(ApiError::IncompatibleResponse);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiKey;
    use std::time::Duration;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    #[tokio::test]
    async fn snapshot_uses_an_internal_camera_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/printers/camera/stream-token"))
            .and(header("X-API-Key", "test-read"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"token":"test-camera"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/1/camera/snapshot"))
            .and(query_param("token", "test-camera"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes([255, 216, 255, 217])
                    .insert_header("Content-Type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(
            server.uri().parse().unwrap(),
            ApiKey::new("test-read".into()).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!client.camera_snapshot(1).await.unwrap().is_empty());
    }
}
