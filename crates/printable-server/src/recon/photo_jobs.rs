//! Retained object jobs, independent of rendering and the live Blender scene.

use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

const ROOT: &str = ".printable/photo-jobs";
const INDEX: &str = ".printable/photo-jobs/index.json";

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhotoJobRequest {
    pub job_id: String,
    /// Include the retained request and source identity; false by default.
    #[serde(default)]
    pub detail: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhotoJobListRequest {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

impl JobState {
    fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Queued,
    PreparingModels,
    WaitingForGpu,
    Foreground,
    Generating,
    Delivering,
    Publishing,
    Finished,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Record {
    format_version: u8,
    job_id: String,
    state: JobState,
    phase: Phase,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    request: ObjectRequest,
    image_sha256: String,
    execution: Value,
    cancellation_requested: bool,
    result: Option<Value>,
    failure: Option<Value>,
}

impl Record {
    fn response(&self, detail: bool) -> Value {
        let mut value = json!({
            "job_id":self.job_id, "state":self.state, "phase":self.phase,
            "model":self.request.model, "mesh_path":self.request.mesh_path,
            "created_unix_ms":self.created_unix_ms, "updated_unix_ms":self.updated_unix_ms,
            "cancellation_requested":self.cancellation_requested,
            "cancellable": !self.state.terminal() && self.phase != Phase::Publishing,
            "result":self.result, "failure":self.failure,
        });
        if detail {
            value["request"] = json!(self.request);
            value["image_sha256"] = json!(self.image_sha256);
            value["execution"] = self.execution.clone();
        }
        value
    }
}

#[derive(Default)]
struct Store {
    lease: Option<OwnedFd>,
    order: Vec<String>,
    records: BTreeMap<String, Record>,
    active: HashMap<String, CancellationToken>,
}

#[derive(Deserialize, Serialize)]
struct Index {
    format_version: u8,
    jobs: Vec<String>,
}

pub(super) struct PhotoJobs {
    workspace: Arc<Workspace>,
    store: Mutex<Store>,
}

fn identifier(value: &str) -> Result<(), ToolError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ToolError::Validation(
            "job_id must be a photograph job identifier".into(),
        ));
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn record_path(id: &str) -> String {
    format!("{ROOT}/{id}/job.json")
}

impl PhotoJobs {
    pub(super) fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            store: Mutex::new(Store::default()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Store>, ToolError> {
        self.store.lock().map_err(|_| {
            reconstruction_error("jobs_unavailable", "photograph job history is unavailable")
        })
    }

    fn persist(&self, record: &Record) -> Result<(), ToolError> {
        self.workspace.write_reserved_artifact(
            &record_path(&record.job_id),
            &serde_json::to_vec(record)?,
            true,
        )?;
        Ok(())
    }

    pub(super) fn initialize(&self) -> Result<(), ToolError> {
        let mut store = self.lock()?;
        if store.lease.is_some() {
            return Ok(());
        }
        let lease = self
            .workspace
            .try_lock_reserved(&format!("{ROOT}/owner.lock"))?;
        let index = match self.workspace.read_generated_bytes(INDEX) {
            Ok((_, bytes)) => serde_json::from_slice::<Index>(&bytes)?,
            Err(printable_workspace::WsError::NotFound(_)) => Index {
                format_version: 1,
                jobs: Vec::new(),
            },
            Err(error) => return Err(error.into()),
        };
        if index.format_version != 1 {
            return Err(reconstruction_error(
                "jobs_unavailable",
                "unsupported photograph job history format",
            ));
        }
        let mut records = BTreeMap::new();
        let mut seen = HashSet::new();
        for id in &index.jobs {
            identifier(id)?;
            if !seen.insert(id) {
                return Err(reconstruction_error(
                    "jobs_unavailable",
                    "photograph job history contains duplicate identifiers",
                ));
            }
            let (_, bytes) = self.workspace.read_generated_bytes(&record_path(id))?;
            let mut record: Record = serde_json::from_slice(&bytes)?;
            if record.format_version != 1 || &record.job_id != id {
                return Err(reconstruction_error(
                    "jobs_unavailable",
                    "photograph job metadata does not match its index",
                ));
            }
            record.request.validate()?;
            if !record.state.terminal() {
                record.state = if record.phase == Phase::Publishing {
                    JobState::OutcomeUnknown
                } else {
                    JobState::Interrupted
                };
                record.failure = Some(
                    json!({"code":"interrupted", "message":"the worker restarted; this job was not replayed; inspect mesh_path before submitting again"}),
                );
                record.updated_unix_ms = now();
                self.persist(&record)?;
            }
            records.insert(id.clone(), record);
        }
        store.order = index.jobs;
        store.records = records;
        store.lease = Some(lease);
        Ok(())
    }

    pub(super) fn has_history(&self) -> bool {
        self.initialize().is_ok() && self.lock().is_ok_and(|store| !store.order.is_empty())
    }

    fn status(&self, request: PhotoJobRequest) -> Result<Value, ToolError> {
        identifier(&request.job_id)?;
        self.initialize()?;
        self.lock()?
            .records
            .get(&request.job_id)
            .map(|record| record.response(request.detail))
            .ok_or_else(|| reconstruction_error("job_not_found", "photograph job was not found"))
    }

    fn list(&self, request: PhotoJobListRequest) -> Result<Value, ToolError> {
        if request.limit == 0 {
            return Err(ToolError::Validation("limit must be positive".into()));
        }
        self.initialize()?;
        let store = self.lock()?;
        let records: Vec<_> = store
            .order
            .iter()
            .rev()
            .skip(request.offset)
            .take(request.limit)
            .map(|id| store.records[id].response(false))
            .collect();
        let next = request.offset.saturating_add(records.len());
        Ok(
            json!({"jobs":records,"requested_limit":request.limit,"returned":records.len(),"next_offset":if next<store.order.len() {Some(next)} else {None}}),
        )
    }

    fn cancel(&self, request: PhotoJobRequest) -> Result<Value, ToolError> {
        identifier(&request.job_id)?;
        self.initialize()?;
        let mut store = self.lock()?;
        let mut record =
            store.records.get(&request.job_id).cloned().ok_or_else(|| {
                reconstruction_error("job_not_found", "photograph job was not found")
            })?;
        if !record.state.terminal() && record.phase != Phase::Publishing {
            record.cancellation_requested = true;
            record.updated_unix_ms = now();
            let persisted = self.persist(&record);
            store.records.insert(request.job_id.clone(), record.clone());
            if let Some(cancel) = store.active.get(&request.job_id) {
                cancel.cancel();
            }
            persisted.map_err(|_| reconstruction_error("outcome_unknown", "cancellation was requested but its record could not be saved; inspect the retained job"))?;
        }
        Ok(record.response(request.detail))
    }

    fn finish(&self, id: &str, outcome: Result<Value, ToolError>) -> Result<(), ToolError> {
        let mut store = self.lock()?;
        let mut record =
            store.records.get(id).cloned().ok_or_else(|| {
                reconstruction_error("job_not_found", "photograph job was not found")
            })?;
        match outcome {
            Ok(value) => {
                record.state = JobState::Succeeded;
                record.phase = Phase::Finished;
                record.result = Some(value);
            }
            Err(error) => {
                record.state = if record.phase == Phase::Publishing
                    && matches!(
                        &error,
                        ToolError::Io(_)
                            | ToolError::Workspace(printable_workspace::WsError::Io(_))
                    ) {
                    JobState::OutcomeUnknown
                } else if error.code() == "cancelled" {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                };
                record.failure =
                    Some(json!({"code":error.code(),"message":worker_message(&error)}));
            }
        }
        record.updated_unix_ms = now();
        store.active.remove(id);
        if let Err(error) = self.persist(&record) {
            record.state = JobState::OutcomeUnknown;
            record.failure = Some(
                json!({"code":"persistence","message":"the final job record could not be saved; inspect mesh_path before submitting again"}),
            );
            store.records.insert(id.to_owned(), record);
            return Err(error);
        }
        store.records.insert(id.to_owned(), record);
        Ok(())
    }
}

pub(super) struct JobControl {
    jobs: Arc<PhotoJobs>,
    id: String,
    pub(super) cancel: CancellationToken,
    image_sha256: String,
}

impl JobControl {
    pub(super) fn verify_input(&self, path: &Path) -> Result<(), ToolError> {
        if file_digest(path)? != self.image_sha256 {
            return Err(reconstruction_error(
                "input_changed",
                "the retained photograph no longer matches its submitted snapshot",
            ));
        }
        Ok(())
    }

    pub(super) fn enter(&self, phase: Phase) -> Result<(), ToolError> {
        let mut store = self.jobs.lock()?;
        let mut record =
            store.records.get(&self.id).cloned().ok_or_else(|| {
                reconstruction_error("job_not_found", "photograph job was not found")
            })?;
        if record.cancellation_requested || self.cancel.is_cancelled() {
            return Err(reconstruction_error(
                "cancelled",
                "photograph job was cancelled before publication",
            ));
        }
        record.state = JobState::Running;
        record.phase = phase;
        record.updated_unix_ms = now();
        self.jobs.persist(&record)?;
        store.records.insert(self.id.clone(), record);
        Ok(())
    }
}

impl ReconWorker {
    pub fn initialize_photo_jobs(&self) -> Result<(), ToolError> {
        self.photo_jobs.initialize()
    }

    pub fn photo_job_status(&self, request: PhotoJobRequest) -> Result<Value, ToolError> {
        self.photo_jobs.status(request)
    }

    pub fn list_photo_jobs(&self, request: PhotoJobListRequest) -> Result<Value, ToolError> {
        self.photo_jobs.list(request)
    }

    pub fn cancel_photo_job(&self, request: PhotoJobRequest) -> Result<Value, ToolError> {
        self.photo_jobs.cancel(request)
    }

    pub fn submit_object(self: &Arc<Self>, request: ObjectRequest) -> Result<Value, ToolError> {
        request.validate()?;
        self.check_destination(&request.mesh_path, request.overwrite)?;
        if request.model == ObjectModel::Trellis2
            && (self.config.device == "cpu"
                || !self
                    .config
                    .trellis
                    .as_ref()
                    .is_some_and(|config| config.available()))
        {
            return Err(reconstruction_error(
                "unavailable",
                "TRELLIS.2 needs CUDA and configured persistent model and scratch storage",
            ));
        }
        self.photo_jobs.initialize()?;
        if self.photo_jobs.lock()?.active.len() >= self.config.photo_queue_depth {
            return Err(reconstruction_error(
                "busy",
                "the photograph job queue is full",
            ));
        }
        let snapshot = self
            .workspace
            .snapshot_artifact_bounded(&request.image, MAX_IMAGE_BYTES)?;
        let image_sha256 = file_digest(snapshot.path())?;
        let id = crate::upload::random_hex_id()?;
        let suffix = Path::new(&request.image)
            .extension()
            .and_then(|value| value.to_str())
            .expect("validated photograph extension");
        let input = format!("{ROOT}/{id}/input.{suffix}");
        let mut execution = request.clone();
        execution.image = input.clone();
        let cancel = CancellationToken::new();
        let settings = match self
            .config
            .trellis
            .as_ref()
            .filter(|_| request.model == ObjectModel::Trellis2)
        {
            Some(config) => {
                json!({"generation_timeout_seconds":config.timeout.as_secs(),"preparation_timeout_seconds":config.preparation_timeout.as_secs(),"gpu_allocation_budget_bytes":config.vram_budget_bytes})
            }
            None => json!({"generation_timeout_seconds":self.config.object_timeout.as_secs()}),
        };
        let record = Record {
            format_version: 1,
            job_id: id.clone(),
            state: JobState::Queued,
            phase: Phase::Queued,
            created_unix_ms: now(),
            updated_unix_ms: now(),
            request,
            image_sha256,
            execution: settings,
            cancellation_requested: false,
            result: None,
            failure: None,
        };
        {
            let mut store = self.photo_jobs.lock()?;
            if store.active.len() >= self.config.photo_queue_depth {
                return Err(reconstruction_error(
                    "busy",
                    "the photograph job queue is full",
                ));
            }
            let mut order = store.order.clone();
            order.push(id.clone());
            let admission = (|| -> Result<(), ToolError> {
                self.workspace.commit_reserved_generated_artifact_bounded(
                    &input,
                    snapshot.path(),
                    false,
                    MAX_IMAGE_BYTES,
                )?;
                self.photo_jobs.persist(&record)?;
                self.workspace.write_reserved_artifact(
                    INDEX,
                    &serde_json::to_vec(&Index {
                        format_version: 1,
                        jobs: order.clone(),
                    })?,
                    true,
                )?;
                Ok(())
            })();
            if let Err(error) = admission {
                // A durability error may follow index replacement. Never
                // remove evidence that the on-disk index could reference.
                let unlisted = match self.workspace.read_generated_bytes(INDEX) {
                    Ok((_, bytes)) => serde_json::from_slice::<Index>(&bytes)
                        .is_ok_and(|index| index.format_version == 1 && !index.jobs.contains(&id)),
                    Err(printable_workspace::WsError::NotFound(_)) => true,
                    Err(_) => false,
                };
                if unlisted {
                    let mut cleanup_failed = false;
                    for path in [record_path(&id), input.clone()] {
                        if let Err(cleanup) = self.workspace.remove_reserved_artifact(&path) {
                            cleanup_failed = true;
                            tracing::error!(job_id=%id, code=cleanup.code(), "rejected photograph admission cleanup failed");
                        }
                    }
                    if !cleanup_failed {
                        return Err(error);
                    }
                }
                let mut uncertain = record.clone();
                uncertain.state = JobState::OutcomeUnknown;
                uncertain.failure = Some(
                    json!({"code":"persistence", "message":"admission persistence could not be confirmed; generation was not started; inspect retained job storage before retrying"}),
                );
                if self.photo_jobs.persist(&uncertain).is_err() {
                    tracing::error!(job_id=%id, "uncertain photograph admission could not be persisted");
                }
                store.order = order;
                store.records.insert(id, uncertain.clone());
                return Ok(uncertain.response(false));
            }
            store.order = order;
            store.records.insert(id.clone(), record.clone());
            store.active.insert(id.clone(), cancel.clone());
        }
        let worker = Arc::clone(self);
        let control = JobControl {
            jobs: Arc::clone(&self.photo_jobs),
            id: id.clone(),
            cancel,
            image_sha256: record.image_sha256.clone(),
        };
        // The immutable input and request are durable before work is enqueued.
        tokio::spawn(async move {
            let outcome = worker
                .generate_object_controlled(execution, Some(&control))
                .await;
            if let Err(error) = worker.photo_jobs.finish(&control.id, outcome) {
                tracing::error!(job_id=%control.id, code=error.code(), "photograph job completion could not be persisted");
            }
        });
        Ok(record.response(false))
    }

    pub(super) async fn job_permit<'a>(
        &'a self,
        control: Option<&JobControl>,
    ) -> Result<tokio::sync::SemaphorePermit<'a>, ToolError> {
        let Some(control) = control else {
            return self.permit();
        };
        control.enter(Phase::WaitingForGpu)?;
        tokio::select! {
            biased;
            _ = control.cancel.cancelled() => Err(reconstruction_error("cancelled", "photograph job was cancelled while waiting for the GPU")),
            permit = self.admission.acquire() => permit.map_err(|_| reconstruction_error("unavailable", "the photograph worker is stopping")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_output(value: &Value) {
        static VALIDATOR: std::sync::LazyLock<jsonschema::Validator> =
            std::sync::LazyLock::new(|| {
                let schema = Value::Object(crate::tools::output::schema("photo").as_ref().clone());
                jsonschema::validator_for(&schema).unwrap()
            });
        VALIDATOR.validate(value).unwrap();
    }

    const RUNTIME: &str = r#"
import json, os, sys, time
from pathlib import Path
if sys.argv[1] == '--check':
    print(json.dumps({'missing':[]}))
    sys.exit(0)
request=json.load(open(sys.argv[1]))
image=Path(request['image']).read_bytes()
if image == b'block':
    Path(__file__).with_suffix('.pid').write_text(str(os.getpid()))
    time.sleep(60)
Path(request['mesh']).write_bytes(image)
print(json.dumps({'result':{'seed':request['seed'],'triangles':1}}))
"#;

    fn fixture() -> (tempfile::TempDir, Arc<ReconWorker>, ObjectRequest) {
        let (root, worker) = super::super::tests::worker(Duration::from_secs(60));
        std::fs::write(&worker.config.object_script, RUNTIME).unwrap();
        let mut request = super::super::tests::object_request("side.png");
        request.background = Some(true);
        request.seed = Some(7);
        (root, Arc::new(worker), request)
    }

    fn query(id: &str) -> PhotoJobRequest {
        PhotoJobRequest {
            job_id: id.to_owned(),
            detail: true,
        }
    }

    async fn wait_for(worker: &ReconWorker, id: &str, field: &str, value: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let state = worker.photo_job_status(query(id)).unwrap();
                check_output(&state);
                if state[field] == value {
                    return state;
                }
                if matches!(
                    state["state"].as_str(),
                    Some("failed" | "cancelled" | "outcome_unknown")
                ) && field != "state"
                {
                    panic!("job stopped before expected phase: {state}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("photograph job reached expected state")
    }

    #[tokio::test]
    async fn inputs_and_settings_are_retained_before_enqueue_and_source_edits_do_not_change_the_job()
     {
        let (root, worker, request) = fixture();
        let permit = worker.permit().unwrap();
        let accepted = worker.submit_object(request.clone()).unwrap();
        check_output(&accepted);
        let id = accepted["job_id"].as_str().unwrap();
        let detail = worker.photo_job_status(query(id)).unwrap();
        check_output(&detail);
        assert_eq!(detail["request"]["seed"], 7);
        let input = format!("{ROOT}/{id}/input.png");
        assert_eq!(
            worker.workspace.read_artifact(&input).unwrap().1,
            b"image bytes"
        );
        worker
            .workspace
            .write_artifact("side.png", b"changed source", true)
            .unwrap();
        drop(permit);
        let complete = wait_for(&worker, id, "state", "succeeded").await;
        assert_eq!(complete["result"]["seed"], 7);
        assert_eq!(
            std::fs::read(root.path().join("workspace/objects/figure.stl")).unwrap(),
            b"image bytes"
        );
        assert_eq!(
            complete["image_sha256"],
            file_digest(&worker.workspace.resolve(&input, true).unwrap()).unwrap()
        );
        assert!(
            worker.readiness().await.1["capabilities"]["photo_jobs"]
                .as_bool()
                .unwrap()
        );
        let page = worker
            .list_photo_jobs(PhotoJobListRequest {
                offset: 0,
                limit: 1,
            })
            .unwrap();
        assert_eq!(page["returned"], 1);
        check_output(&page);
        assert_eq!(page["jobs"][0]["job_id"], id);
    }

    #[tokio::test]
    async fn rejected_admission_releases_the_retained_input_storage() {
        let (_root, worker, request) = fixture();
        worker.initialize_photo_jobs().unwrap();
        let initial = worker.workspace.storage_usage().unwrap().charged_bytes;
        worker
            .workspace
            .configure_storage_budget(Some(initial + 4096 + 222))
            .unwrap();
        assert_eq!(
            worker.submit_object(request).unwrap_err().code(),
            "storage_budget_exceeded"
        );
        assert!(worker.photo_jobs.lock().unwrap().order.is_empty());
        assert!(worker.photo_jobs.lock().unwrap().active.is_empty());
        let root = worker.workspace.resolve(ROOT, true).unwrap();
        let directories: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().is_dir())
            .collect();
        assert_eq!(directories.len(), 1);
        for directory in directories {
            assert!(!directory.path().join("input.png").exists());
            assert!(!directory.path().join("job.json").exists());
        }
        assert!(worker.workspace.storage_usage().unwrap().charged_bytes <= initial + 32);
    }

    #[tokio::test]
    async fn uncertain_index_publication_keeps_evidence_without_starting_generation() {
        let (_root, worker, request) = fixture();
        worker.initialize_photo_jobs().unwrap();
        std::fs::create_dir(worker.workspace.resolve(INDEX, false).unwrap()).unwrap();
        let outcome = worker.submit_object(request).unwrap();
        check_output(&outcome);
        assert_eq!(outcome["state"], "outcome_unknown");
        let id = outcome["job_id"].as_str().unwrap();
        assert_eq!(
            worker.photo_job_status(query(id)).unwrap()["state"],
            "outcome_unknown"
        );
        assert!(worker.photo_jobs.lock().unwrap().active.is_empty());
        assert_eq!(worker.admission.available_permits(), 1);
        assert_eq!(
            worker
                .workspace
                .read_artifact(&format!("{ROOT}/{id}/input.png"))
                .unwrap()
                .1,
            b"image bytes"
        );
        assert!(
            worker
                .workspace
                .stat_artifact("objects/figure.stl")
                .is_err()
        );
    }

    #[tokio::test]
    async fn admission_and_waiting_cancellation_do_not_start_extra_work() {
        let (_root, mut worker, request) = fixture();
        Arc::get_mut(&mut worker).unwrap().config.photo_queue_depth = 1;
        let permit = worker.permit().unwrap();
        let job = worker.submit_object(request.clone()).unwrap();
        let id = job["job_id"].as_str().unwrap();
        let mut second = request;
        second.mesh_path = "objects/second.stl".into();
        assert_eq!(worker.submit_object(second).unwrap_err().code(), "busy");
        wait_for(&worker, id, "phase", "waiting_for_gpu").await;
        let requested = worker.cancel_photo_job(query(id)).unwrap();
        check_output(&requested);
        assert_eq!(requested["cancellation_requested"], true);
        wait_for(&worker, id, "state", "cancelled").await;
        drop(permit);
        assert!(
            worker
                .workspace
                .stat_artifact("objects/figure.stl")
                .is_err()
        );
        assert!(worker.photo_jobs.lock().unwrap().active.is_empty());
    }

    #[tokio::test]
    async fn cancellation_reaps_a_running_native_process_before_releasing_the_gpu_lane() {
        let (_root, worker, request) = fixture();
        worker
            .workspace
            .write_artifact("side.png", b"block", true)
            .unwrap();
        let job = worker.submit_object(request).unwrap();
        let id = job["job_id"].as_str().unwrap();
        let marker = worker.config.object_script.with_extension("pid");
        let pid: i32 = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = value.parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker.cancel_photo_job(query(id)).unwrap();
        wait_for(&worker, id, "state", "cancelled").await;
        // SAFETY: signal zero only queries the isolated fixture's child id.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(worker.admission.available_permits(), 1);
        assert!(
            worker
                .workspace
                .stat_artifact("objects/figure.stl")
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_destination_created_while_waiting_is_preserved() {
        let (_root, worker, request) = fixture();
        let permit = worker.permit().unwrap();
        let accepted = worker.submit_object(request).unwrap();
        let id = accepted["job_id"].as_str().unwrap();
        wait_for(&worker, id, "phase", "waiting_for_gpu").await;
        worker
            .workspace
            .write_artifact("objects/figure.stl", b"another writer", false)
            .unwrap();
        drop(permit);
        let failed = wait_for(&worker, id, "state", "failed").await;
        assert_eq!(failed["failure"]["code"], "already_exists");
        assert_eq!(
            worker
                .workspace
                .read_artifact("objects/figure.stl")
                .unwrap()
                .1,
            b"another writer"
        );
    }

    #[test]
    fn restart_reports_interrupted_and_uncertain_publication_without_replaying() {
        let (root, worker, request) = fixture();
        let workspace = Arc::clone(&worker.workspace);
        let mut ids = Vec::new();
        for (index, phase) in [Phase::Queued, Phase::Generating, Phase::Publishing]
            .into_iter()
            .enumerate()
        {
            let id = format!("{index:032x}");
            let record = Record {
                format_version: 1,
                job_id: id.clone(),
                state: JobState::Running,
                phase,
                created_unix_ms: now(),
                updated_unix_ms: now(),
                request: request.clone(),
                image_sha256: "0".repeat(64),
                execution: json!({}),
                cancellation_requested: false,
                result: None,
                failure: None,
            };
            worker.photo_jobs.persist(&record).unwrap();
            ids.push(id);
        }
        workspace
            .write_reserved_artifact(
                INDEX,
                &serde_json::to_vec(&Index {
                    format_version: 1,
                    jobs: ids.clone(),
                })
                .unwrap(),
                false,
            )
            .unwrap();
        drop(worker);
        let jobs = PhotoJobs::new(workspace);
        jobs.initialize().unwrap();
        for (index, id) in ids.iter().enumerate() {
            let state = jobs.status(query(id)).unwrap();
            check_output(&state);
            assert_eq!(
                state["state"],
                if index == 2 {
                    "outcome_unknown"
                } else {
                    "interrupted"
                }
            );
        }
        assert!(jobs.lock().unwrap().active.is_empty());
        assert!(!root.path().join("workspace/objects/figure.stl").exists());
    }
}
