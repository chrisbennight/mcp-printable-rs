//! Library uploads and staged printer jobs backed by Bambuddy's queue.

use super::{ApiError, Client};
use reqwest::{
    Method,
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

pub use crate::job_types::{FileMetadata, LibraryFile, PrintArchive, QueueItem};

#[derive(Debug, Serialize)]
pub struct StageJob {
    pub printer_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_id: Option<u64>,
    pub quantity: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<u64>,
    #[serde(flatten)]
    pub options: crate::job_options::JobOptions,
    pub plate_id: u16,
    pub ams_mapping: Vec<i32>,
    pub use_ams: bool,
    pub bed_levelling: bool,
    pub flow_cali: bool,
    pub vibration_cali: bool,
    pub timelapse: bool,
    pub manual_start: bool,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QueueReceipt {
    pub message: String,
}

impl Client {
    pub async fn print_history(
        &self,
        printer: Option<u64>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PrintArchive>, ApiError> {
        let mut endpoint = self.resource(&["archives", ""])?;
        endpoint
            .query_pairs_mut()
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.to_string());
        if let Some(id) = printer {
            endpoint
                .query_pairs_mut()
                .append_pair("printer_id", &id.to_string());
        }
        self.read(endpoint).await
    }

    pub async fn library_file(&self, id: u64) -> Result<LibraryFile, ApiError> {
        self.read(self.resource(&["library", "files", &id.to_string()])?)
            .await
    }

    pub(crate) fn resource(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| ApiError::InvalidConfiguration("base URL"))?
            .extend(["api", "v1"])
            .extend(segments);
        Ok(url)
    }

    pub async fn upload_print(
        &self,
        body: reqwest::Body,
        size_bytes: u64,
        filename: String,
    ) -> Result<LibraryFile, ApiError> {
        let part = Part::stream_with_length(body, size_bytes).file_name(filename);
        let response = self
            .http
            .post(self.resource(&["library", "files"])?)
            .header("X-API-Key", self.api_key.0.as_str())
            .timeout(Duration::from_secs(300))
            .multipart(Form::new().part("file", part))
            .send()
            .await
            .map_err(|_| ApiError::AmbiguousOutcome)?;
        self.decode_response(response, true).await
    }

    pub async fn stage_print(&self, job: &StageJob) -> Result<QueueItem, ApiError> {
        let response = self
            .http
            .post(self.resource(&["queue", ""])?)
            .header("X-API-Key", self.api_key.0.as_str())
            .json(job)
            .send()
            .await
            .map_err(|_| ApiError::AmbiguousOutcome)?;
        self.decode_response(response, true).await
    }

    pub async fn print_job(&self, id: u64) -> Result<QueueItem, ApiError> {
        self.read(self.resource(&["queue", &id.to_string()])?).await
    }

    pub async fn print_jobs(
        &self,
        printer: Option<u64>,
        status: Option<&str>,
    ) -> Result<Vec<QueueItem>, ApiError> {
        let mut endpoint = self.resource(&["queue", ""])?;
        if let Some(id) = printer {
            endpoint
                .query_pairs_mut()
                .append_pair("printer_id", &id.to_string());
        }
        if let Some(status) = status {
            endpoint.query_pairs_mut().append_pair("status", status);
        }
        self.read(endpoint).await
    }

    pub async fn start_print_job(&self, id: u64) -> Result<QueueItem, ApiError> {
        self.request_once(
            Method::POST,
            self.resource(&["queue", &id.to_string(), "start"])?,
            true,
        )
        .await
    }

    pub async fn cancel_print_job(&self, id: u64) -> Result<QueueReceipt, ApiError> {
        self.request_once(
            Method::POST,
            self.resource(&["queue", &id.to_string(), "cancel"])?,
            true,
        )
        .await
    }
}

impl Client {
    pub async fn library_files(
        &self,
        folder: Option<u64>,
    ) -> Result<Vec<crate::job_types::FileListResponse>, ApiError> {
        let mut url = self.resource(&["library", "files"])?;
        if let Some(id) = folder {
            url.query_pairs_mut()
                .append_pair("folder_id", &id.to_string());
        } else {
            url.query_pairs_mut().append_pair("include_root", "false");
        }
        self.read(url).await
    }
    pub async fn archive(&self, id: u64) -> Result<PrintArchive, ApiError> {
        self.read(self.resource(&["archives", &id.to_string()])?)
            .await
    }
    pub async fn plates(
        &self,
        id: u64,
        archive: bool,
    ) -> Result<crate::job_types::Plates, ApiError> {
        let id = id.to_string();
        self.read(if archive {
            self.resource(&["archives", &id, "plates"])?
        } else {
            self.resource(&["library", "files", &id, "plates"])?
        })
        .await
    }
    pub async fn filament_requirements(
        &self,
        id: u64,
        archive: bool,
        plate: u16,
    ) -> Result<crate::job_types::FilamentRequirements, ApiError> {
        let id = id.to_string();
        let mut url = if archive {
            self.resource(&["archives", &id, "filament-requirements"])?
        } else {
            self.resource(&["library", "files", &id, "filament-requirements"])?
        };
        url.query_pairs_mut()
            .append_pair("plate_id", &plate.to_string());
        self.read(url).await
    }
    pub async fn archive_runs(&self, id: u64) -> Result<crate::job_types::PrintRuns, ApiError> {
        self.read(self.resource(&["archives", &id.to_string(), "runs"])?)
            .await
    }
    pub async fn batches(&self) -> Result<Vec<crate::job_types::PrintBatch>, ApiError> {
        self.read(self.resource(&["queue", "batches"])?).await
    }
    pub async fn batch(&self, id: u64) -> Result<crate::job_types::PrintBatch, ApiError> {
        self.read(self.resource(&["queue", "batches", &id.to_string()])?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiKey;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    #[tokio::test]
    async fn staged_job_is_released_by_id_and_failed_start_is_not_retried() {
        let server = MockServer::start().await;
        let staged = json!({"id":42,"printer_id":1,"library_file_id":9,"status":"pending","manual_start":true});
        Mock::given(method("POST")).and(path("/api/v1/queue/"))
            .and(body_partial_json(json!({"manual_start":true,"printer_id":1,"library_file_id":9,"plate_id":1,"use_ams":false})))
            .respond_with(ResponseTemplate::new(200).set_body_json(&staged)).expect(1).mount(&server).await;
        let mut released = staged.clone();
        released["manual_start"] = json!(false);
        Mock::given(method("POST"))
            .and(path("/api/v1/queue/42/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(released))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/queue/43/start"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(
            server.uri().parse().unwrap(),
            ApiKey::new("fixture".into()).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        let job = client
            .stage_print(&StageJob {
                printer_id: 1,
                library_file_id: Some(9),
                archive_id: None,
                quantity: 1,
                batch_id: None,
                options: Default::default(),
                plate_id: 1,
                ams_mapping: vec![],
                use_ams: false,
                bed_levelling: true,
                flow_cali: true,
                vibration_cali: true,
                timelapse: false,
                manual_start: true,
            })
            .await
            .unwrap();
        assert!(job.manual_start);
        assert!(!client.start_print_job(job.id).await.unwrap().manual_start);
        assert_eq!(
            client.start_print_job(43).await.unwrap_err(),
            ApiError::AmbiguousOutcome
        );
        server.verify().await;
    }
}
