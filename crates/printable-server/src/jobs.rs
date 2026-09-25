//! Durable, restart-recoverable still and animation render jobs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::f64::consts::TAU;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs::File, io::Read as _, io::Seek as _, io::SeekFrom};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, OnceCell, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use printable_blender::{BlenderClient, Deadline, Params, Transaction};
use printable_geom::{
    AssemblyOptions, AssemblyPartSummary, AssemblyRelation, AssemblyReport, AssemblyStaticReport,
    MotionBlockReason, RotationalMotion, RotationalMotionReport,
};
use printable_workspace::{ArtifactMeta, Snapshot, Workspace, WsError};

use crate::error::ToolError;
use crate::tools::{
    MAX_PRODUCT_RENDER_BYTES, MAX_PRODUCT_RENDER_PIXELS, ProductPresentation, geometry_blocking,
    geometry_worker_path_from_override, product_controls_match,
    product_materials_and_shading_match, run_geometry_worker_files, validate_product_presentation,
    verify_product_png_artifact,
};
use crate::upload::random_hex_id;

const JOB_ROOT: &str = ".printable/jobs";
const INDEX_PATH: &str = ".printable/jobs/index.json";
const ISOLATION_PATH: &str = ".printable/render-isolation.json";
const JOB_SCHEMA_VERSION: u8 = 3;
const MAX_JOB_HISTORY: usize = 1000;
const MAX_JOB_LIST_LIMIT: usize = 1000;
const MAX_ENCODER_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_ENCODER_PROGRESS_BYTES: usize = 64 * 1024;
const MAX_BLENDER_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(test)]
const DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_REVIEW_SOURCE_PIXELS: u64 = 8 * 1024 * 1024;
const SESSION_RESTORE_RETRY_INITIAL_SECONDS: u64 = 1;
const SESSION_RESTORE_RETRY_MAX_SECONDS: u64 = 30;

fn default_render_dimension() -> u16 {
    512
}

fn default_render_timeout_seconds() -> f64 {
    3600.0
}

fn default_turntable_frames() -> u32 {
    120
}

fn default_turntable_elevation() -> f64 {
    20.0
}

fn default_frame_start() -> i32 {
    1
}

fn default_frame_end() -> i32 {
    250
}

fn default_frame_step() -> u32 {
    1
}

fn default_frames_per_second() -> u16 {
    30
}

fn default_max_frame_sequence_bytes() -> u64 {
    10 * 1024 * 1024 * 1024
}

fn default_max_source_bytes() -> u64 {
    MAX_BLENDER_ARTIFACT_BYTES
}

fn default_max_video_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_job_list_limit() -> usize {
    100
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RenderJobKind {
    Still,
    Turntable,
    Animation,
    MechanicalRotation,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MechanicalRotationSpec {
    /// Every fixed mesh object in the scene. The set must be non-empty.
    #[schemars(length(min = 1, max = 1000))]
    fixed_objects: Vec<String>,
    /// Every moving mesh object in the scene. The set must be non-empty and disjoint from fixed_objects.
    #[schemars(length(min = 1, max = 1000))]
    moving_objects: Vec<String>,
    /// Rotation pivot in Blender world coordinates and exported STL millimetres.
    pivot_mm: [f64; 3],
    /// Right-hand-rule rotation axis. Magnitude is normalized.
    axis: [f64; 3],
    /// Positive rotation angle authored across frame_start through frame_end.
    angle_degrees: f64,
    /// Non-negative clearance that must be certified over the complete rotation.
    target_clearance_mm: f64,
    /// Positive byte budget applied separately to each analysis STL (maximum 1 GiB).
    #[serde(default = "default_max_source_bytes")]
    #[schemars(range(min = 1, max = 1073741824))]
    max_analysis_mesh_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum RenderJobEngine {
    #[default]
    Eevee,
    Cycles,
}

impl RenderJobEngine {
    fn addon_name(self) -> &'static str {
        match self {
            Self::Eevee => "EEVEE",
            Self::Cycles => "CYCLES",
        }
    }
}

/// `printable_render_job_submit` parameters.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderJobSubmitParams {
    /// Immutable confined `.blend` checkpoint used throughout the job.
    source_blend: String,
    /// Positive source-checkpoint snapshot budget (default and maximum 1 GiB, matching Blender staging).
    #[serde(default = "default_max_source_bytes")]
    #[schemars(range(min = 1, max = 1073741824))]
    max_source_bytes: u64,
    /// Still image, orbiting turntable video, scene-timeline animation, or certified rigid rotation.
    kind: RenderJobKind,
    /// Exact scene partition and motion contract required only for mechanical_rotation jobs.
    mechanical_rotation: Option<MechanicalRotationSpec>,
    /// Frame width in pixels (default 512, maximum 8192). Turntable width × height must not exceed 8,388,608 pixels.
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    width: u16,
    /// Frame height in pixels (default 512, maximum 8192). Turntable width × height must not exceed 8,388,608 pixels.
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    height: u16,
    /// EEVEE for responsive review output or CYCLES for final quality.
    #[serde(default)]
    engine: RenderJobEngine,
    /// CYCLES samples per frame (default 128, maximum 4096); invalid for EEVEE.
    #[schemars(range(min = 1, max = 4096))]
    samples: Option<u16>,
    /// Positive caller-selected budget for each rendered frame. Defaults to one hour; no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    frame_timeout_seconds: f64,
    /// Positive turntable frame count (default 120); valid only for turntable jobs.
    #[serde(default = "default_turntable_frames")]
    #[schemars(range(min = 1))]
    turntable_frames: u32,
    /// Turntable camera elevation in degrees from -89 through 89 (default 20).
    #[serde(default = "default_turntable_elevation")]
    turntable_elevation_degrees: f64,
    /// Reverse the turntable orbit; valid only for turntable jobs.
    #[serde(default)]
    turntable_clockwise: bool,
    /// First Blender timeline frame, inclusive (default 1); valid for animation and mechanical_rotation jobs.
    #[serde(default = "default_frame_start")]
    #[schemars(range(min = -1048574, max = 1048574))]
    frame_start: i32,
    /// Last Blender timeline frame, inclusive (default 250); valid for animation and mechanical_rotation jobs.
    #[serde(default = "default_frame_end")]
    #[schemars(range(min = -1048574, max = 1048574))]
    frame_end: i32,
    /// Positive timeline step (default 1); valid for animation and mechanical_rotation jobs.
    #[serde(default = "default_frame_step")]
    #[schemars(range(min = 1))]
    frame_step: u32,
    /// Encoded video frame rate (default 30, maximum 240); ignored for still jobs.
    #[serde(default = "default_frames_per_second")]
    #[schemars(range(min = 1, max = 240))]
    frames_per_second: u16,
    /// Positive aggregate byte budget for committed PNG frames. Defaults to 10 GiB; no configured maximum.
    #[serde(default = "default_max_frame_sequence_bytes")]
    #[schemars(range(min = 1))]
    max_frame_sequence_bytes: u64,
    /// Positive encoded MP4 byte budget. Defaults to 1 GiB; no configured maximum. Ignored for still jobs.
    #[serde(default = "default_max_video_bytes")]
    #[schemars(range(min = 1, max = 9223372036854775807i64))]
    max_video_bytes: u64,
    /// Positive caller-selected FFmpeg budget. Defaults to one hour; no configured maximum. Ignored for still jobs.
    #[serde(default = "default_render_timeout_seconds")]
    encode_timeout_seconds: f64,
    /// Optional deterministic product presentation persisted with the job.
    presentation: Option<ProductPresentation>,
    /// For general timeline animation, frame the union of evaluated bounds
    /// across the complete requested sequence instead of preserving the
    /// authored camera.
    #[serde(default)]
    auto_frame_sequence: bool,
    /// Separate positive budget for evaluating sequence-wide bounds.
    #[serde(default = "default_render_timeout_seconds")]
    auto_frame_sequence_timeout_seconds: f64,
}

/// `printable_render_job_status` parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderJobStatusParams {
    job_id: String,
}

/// `printable_render_job_list` parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderJobListParams {
    /// Zero-based offset into newest-first retained job history.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: usize,
    /// Maximum jobs returned (default 100, maximum 1000).
    #[serde(default = "default_job_list_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
    /// Optional exact state filter.
    state: Option<JobState>,
}

/// `printable_render_job_artifacts` parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderJobArtifactsParams {
    job_id: String,
    /// Zero-based frame offset.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: u32,
    /// Maximum frame artifacts returned (default 100, maximum 1000).
    #[serde(default = "default_job_list_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
}

/// `printable_render_job_cancel` parameters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderJobCancelParams {
    job_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobSpec {
    source_blend: String,
    max_source_bytes: u64,
    width: u16,
    height: u16,
    engine: RenderJobEngine,
    samples: Option<u16>,
    frame_timeout_seconds: f64,
    turntable_frames: u32,
    turntable_elevation_degrees: f64,
    turntable_clockwise: bool,
    frame_start: i32,
    frame_end: i32,
    frame_step: u32,
    frames_per_second: u16,
    max_frame_sequence_bytes: u64,
    max_video_bytes: u64,
    encode_timeout_seconds: f64,
    #[serde(default)]
    mechanical_rotation: Option<MechanicalRotationSpec>,
    #[serde(default)]
    presentation: Option<ProductPresentation>,
    #[serde(default)]
    auto_frame_sequence: bool,
    #[serde(default = "default_render_timeout_seconds")]
    auto_frame_sequence_timeout_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobProgress {
    phase: String,
    completed_frames: u32,
    total_frames: u32,
    current_frame: Option<i64>,
    frame_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobFailure {
    code: String,
    message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingBlenderOutcome {
    Completed,
    Cancelled,
    Failed(JobFailure),
}

impl PendingBlenderOutcome {
    fn from_result(result: &Result<BlenderRunOutcome, RunFailure>) -> Self {
        match result {
            Ok(BlenderRunOutcome::Completed) => Self::Completed,
            Ok(BlenderRunOutcome::Cancelled) => Self::Cancelled,
            Err(failure) => Self::Failed(JobFailure {
                code: failure.code.clone(),
                message: failure.message.clone(),
            }),
        }
    }

    fn failure(&self) -> Option<JobFailure> {
        match self {
            Self::Failed(failure) => Some(failure.clone()),
            Self::Completed | Self::Cancelled => None,
        }
    }

    fn into_result(self) -> Result<BlenderRunOutcome, RunFailure> {
        match self {
            Self::Completed => Ok(BlenderRunOutcome::Completed),
            Self::Cancelled => Ok(BlenderRunOutcome::Cancelled),
            Self::Failed(failure) => Err(RunFailure::new(failure.code, failure.message)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobArtifact {
    path: String,
    size_bytes: u64,
    media_type: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MechanicalAnalysis {
    #[serde(default)]
    generation: u32,
    fixed_artifact: JobArtifact,
    moving_artifact: JobArtifact,
    units: String,
    certified: bool,
    report: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct PresentationBounds {
    minimum: [f64; 3],
    maximum: [f64; 3],
    dimensions: [f64; 3],
    center: [f64; 3],
    diagonal: f64,
    coordinate_space: String,
    unit: String,
}

#[derive(Debug, Deserialize)]
struct MechanicalPrepareResponse {
    fixed_path: String,
    moving_path: String,
    motion: MechanicalAuthoredMotion,
}

#[derive(Debug, Deserialize)]
struct MechanicalAuthoredMotion {
    controller: String,
    objects: Vec<String>,
    pivot: [f64; 3],
    axis: [f64; 3],
    angle_degrees: f64,
    frame_start: i32,
    frame_end: i32,
    interpolation: String,
}

impl From<ArtifactMeta> for JobArtifact {
    fn from(value: ArtifactMeta) -> Self {
        Self {
            path: value.path,
            size_bytes: value.size_bytes,
            media_type: value.media_type.to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum JobExecution {
    #[default]
    Unspecified,
    LegacySession,
    IsolatedWorker {
        blender_finished: bool,
    },
}

impl JobExecution {
    fn isolated(&self) -> bool {
        matches!(self, Self::IsolatedWorker { .. })
    }

    fn finished(&self) -> bool {
        matches!(
            self,
            Self::IsolatedWorker {
                blender_finished: true
            }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobSourceSnapshot {
    sha256: String,
    size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JobRecord {
    schema_version: u8,
    #[serde(default)]
    execution: JobExecution,
    job_id: String,
    kind: RenderJobKind,
    state: JobState,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    source_artifact: String,
    #[serde(default)]
    source_snapshot: Option<JobSourceSnapshot>,
    session_checkpoint_artifact: String,
    session_checkpoint_captured: bool,
    session_checkpoint_restored: bool,
    #[serde(default)]
    pending_blender_outcome: Option<PendingBlenderOutcome>,
    spec: JobSpec,
    progress: JobProgress,
    cancellation_requested: bool,
    recovery_count: u32,
    video_artifact: Option<JobArtifact>,
    #[serde(default)]
    mechanical_analysis: Option<MechanicalAnalysis>,
    #[serde(default)]
    presentation_bounds: Option<PresentationBounds>,
    failure: Option<JobFailure>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct JobIndex {
    schema_version: u8,
    job_ids: Vec<String>,
}

struct RegistryState {
    jobs: HashMap<String, JobRecord>,
    order: VecDeque<String>,
}

#[derive(Clone, Debug, Default)]
struct RecoveryIntegrity {
    issues: Vec<String>,
}

impl RecoveryIntegrity {
    fn blocked(&self) -> bool {
        !self.issues.is_empty()
    }

    fn report(&self) -> Value {
        json!({
            "status": if self.blocked() { "blocked" } else { "ok" },
            "issues": self.issues,
        })
    }
}

struct JobQueue {
    items: std::sync::Mutex<VecDeque<String>>,
    closed: AtomicBool,
    notify: Notify,
}

impl JobQueue {
    fn new(items: impl IntoIterator<Item = String>) -> Self {
        Self {
            items: std::sync::Mutex::new(items.into_iter().collect()),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn push(&self, job_id: String, capacity: usize) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("render job queue is closed".to_string());
        }
        let mut items = self.items.lock().expect("job queue lock");
        if items.len() >= capacity {
            return Err(format!(
                "physical render job queue exceeded its configured capacity ({capacity})"
            ));
        }
        items.push_back(job_id);
        drop(items);
        self.notify.notify_one();
        Ok(())
    }

    fn remove(&self, job_id: &str) {
        self.items
            .lock()
            .expect("job queue lock")
            .retain(|candidate| candidate != job_id);
    }

    async fn receive(&self) -> Option<String> {
        loop {
            let notified = self.notify.notified();
            if let Some(job_id) = self.items.lock().expect("job queue lock").pop_front() {
                return Some(job_id);
            }
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            notified.await;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[derive(Clone, Copy)]
enum RenderIsolation {
    Legacy,
    Established,
    Blocked,
}

impl RenderIsolation {
    fn established(self) -> bool {
        matches!(self, Self::Established)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy_session",
            Self::Established => "isolated_worker",
            Self::Blocked => "migration_blocked",
        }
    }
}

fn establish_render_isolation(
    workspace: &Workspace,
    configured: bool,
    legacy_active: bool,
    integrity: &mut RecoveryIntegrity,
) -> RenderIsolation {
    let marker = json!({"version": 1, "mode": "isolated_worker"});
    let established = match workspace.read_artifact(ISOLATION_PATH) {
        Ok((_, bytes))
            if serde_json::from_slice::<Value>(&bytes).is_ok_and(|value| value == marker) =>
        {
            true
        }
        Err(WsError::NotFound(_)) => false,
        Err(WsError::Unconfined) if !configured => return RenderIsolation::Legacy,
        _ => {
            integrity
                .issues
                .push("render isolation migration record is unreadable or invalid".into());
            return RenderIsolation::Blocked;
        }
    };
    if established && legacy_active {
        integrity
            .issues
            .push("isolated rendering history contains unfinished legacy work".into());
        return RenderIsolation::Blocked;
    }
    if established {
        return RenderIsolation::Established;
    }
    if !configured {
        return RenderIsolation::Legacy;
    }
    if integrity.blocked() || legacy_active {
        integrity
            .issues
            .push("legacy render work must be verifiably drained before worker migration".into());
        return RenderIsolation::Blocked;
    }
    if workspace
        .write_reserved_artifact(
            ISOLATION_PATH,
            &serde_json::to_vec(&marker).expect("fixed migration marker serializes"),
            false,
        )
        .is_err()
    {
        integrity
            .issues
            .push("render isolation migration record could not be persisted".into());
        return RenderIsolation::Blocked;
    }
    RenderIsolation::Established
}

struct JobRegistryInner {
    workspace: Arc<Workspace>,
    blender: Arc<BlenderClient>,
    render_worker: Option<Arc<BlenderClient>>,
    isolation: RenderIsolation,
    ffmpeg_bin: PathBuf,
    geometry_worker_bin: Option<PathBuf>,
    geometry_worker_memory_bytes: u64,
    queue_depth: usize,
    state: Mutex<RegistryState>,
    admission: Arc<Mutex<()>>,
    persistence: Arc<Mutex<()>>,
    cancellations: Mutex<HashMap<String, CancellationToken>>,
    queue: Arc<JobQueue>,
    recovery_integrity: RecoveryIntegrity,
    encoder_health: OnceCell<Value>,
    worker_verified: AtomicBool,
}

impl Drop for JobRegistryInner {
    fn drop(&mut self) {
        self.queue.close();
    }
}

#[derive(Clone)]
pub(crate) struct JobRegistry {
    inner: Arc<JobRegistryInner>,
}

impl JobRegistry {
    #[cfg(test)]
    pub(crate) fn new(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        ffmpeg_bin: PathBuf,
        queue_depth: usize,
    ) -> Self {
        Self::new_with_geometry_worker(
            workspace,
            blender,
            ffmpeg_bin,
            queue_depth,
            None,
            DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_geometry_worker(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        ffmpeg_bin: PathBuf,
        queue_depth: usize,
        geometry_worker_bin: Option<PathBuf>,
        geometry_worker_memory_bytes: u64,
    ) -> Self {
        Self::new_with_render_worker(
            workspace,
            blender,
            ffmpeg_bin,
            queue_depth,
            geometry_worker_bin,
            geometry_worker_memory_bytes,
            None,
        )
    }

    pub(crate) fn new_with_render_worker(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        ffmpeg_bin: PathBuf,
        queue_depth: usize,
        geometry_worker_bin: Option<PathBuf>,
        geometry_worker_memory_bytes: u64,
        render_worker: Option<Arc<BlenderClient>>,
    ) -> Self {
        let (state, recovered, mut recovery_integrity) = recover_state(&workspace, queue_depth);
        let legacy_active = state.jobs.values().any(|job| {
            !job.execution.isolated()
                && (!job.state.terminal()
                    || (job.session_checkpoint_captured && !job.session_checkpoint_restored))
        });
        let isolation = establish_render_isolation(
            &workspace,
            render_worker.is_some(),
            legacy_active,
            &mut recovery_integrity,
        );
        let recovery_blocked = recovery_integrity.blocked();
        blender.set_recovery_fenced(
            (recovery_blocked && !isolation.established())
                || state
                    .jobs
                    .values()
                    .any(|job| job.session_checkpoint_captured && !job.session_checkpoint_restored),
        );
        let cancellations = recovered
            .iter()
            .map(|job_id| {
                let token = CancellationToken::new();
                if state
                    .jobs
                    .get(job_id)
                    .is_some_and(|job| job.cancellation_requested)
                {
                    token.cancel();
                }
                (job_id.clone(), token)
            })
            .collect();
        let recovered = if recovery_blocked {
            Vec::new()
        } else {
            recovered
        };
        let queue = Arc::new(JobQueue::new(recovered));
        let inner = Arc::new(JobRegistryInner {
            workspace,
            blender,
            render_worker,
            isolation,
            ffmpeg_bin,
            geometry_worker_bin,
            geometry_worker_memory_bytes,
            queue_depth,
            state: Mutex::new(state),
            admission: Arc::new(Mutex::new(())),
            persistence: Arc::new(Mutex::new(())),
            cancellations: Mutex::new(cancellations),
            queue: Arc::clone(&queue),
            recovery_integrity,
            encoder_health: OnceCell::new(),
            worker_verified: AtomicBool::new(false),
        });
        tokio::spawn(job_worker(Arc::downgrade(&inner), queue));
        Self { inner }
    }

    pub(crate) async fn submit(&self, params: RenderJobSubmitParams) -> Result<Value, ToolError> {
        if self.inner.render_worker.is_none() {
            return Err(ToolError::Job(
                "a separate render worker is required for new jobs".into(),
            ));
        }
        if !self.inner.isolation.established() {
            return Err(ToolError::Job(
                "legacy render recovery must finish before isolated jobs can be admitted".into(),
            ));
        }
        self.stage_submission(params).await
    }

    async fn stage_submission(&self, params: RenderJobSubmitParams) -> Result<Value, ToolError> {
        if self.inner.recovery_integrity.blocked() {
            return Err(ToolError::Job(
                "durable render recovery metadata is blocked; repair it and restart before submitting work"
                    .to_string(),
            ));
        }
        if self.inner.isolation.established() && self.inner.render_worker.is_none() {
            return Err(ToolError::Job(
                "render worker configuration is required after isolated rendering migration".into(),
            ));
        }
        let (spec, kind, total_frames) = validate_submit(params)?;
        let admission = Arc::clone(&self.inner.admission).lock_owned().await;
        {
            let state = self.inner.state.lock().await;
            let active = state
                .jobs
                .values()
                .filter(|job| !job.state.terminal())
                .count();
            if active >= self.inner.queue_depth {
                return Err(ToolError::Job(format!(
                    "render job queue is full (max {})",
                    self.inner.queue_depth
                )));
            }
        }
        let job_id = random_hex_id()?;
        let source_artifact = format!("{JOB_ROOT}/{job_id}/source.blend");
        let session_checkpoint_artifact = format!("{JOB_ROOT}/{job_id}/pre-job-session.blend");
        let now = unix_ms();
        let record = JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: if self.inner.isolation.established() {
                JobExecution::IsolatedWorker {
                    blender_finished: false,
                }
            } else {
                JobExecution::LegacySession
            },
            job_id: job_id.clone(),
            kind,
            state: JobState::Queued,
            created_unix_ms: now,
            updated_unix_ms: now,
            source_artifact,
            source_snapshot: None,
            session_checkpoint_artifact,
            session_checkpoint_captured: false,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "staging_source".to_string(),
                completed_frames: 0,
                total_frames,
                current_frame: None,
                frame_bytes: 0,
            },
            cancellation_requested: false,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: None,
            failure: None,
        };
        let staging =
            tokio::spawn(Arc::clone(&self.inner).stage_admitted_submission(admission, record));

        match staging.await {
            Ok(result) => result?,
            Err(error) => {
                self.inner
                    .fail_job(
                        &job_id,
                        "source_staging_task",
                        format!("source staging task failed: {error}"),
                    )
                    .await;
            }
        }
        self.status_value(&job_id).await
    }

    pub(crate) async fn status(&self, params: RenderJobStatusParams) -> Result<Value, ToolError> {
        self.status_value(&params.job_id).await
    }

    async fn status_value(&self, job_id: &str) -> Result<Value, ToolError> {
        let state = self.inner.state.lock().await;
        let record = state
            .jobs
            .get(job_id)
            .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
        serde_json::to_value(record).map_err(Into::into)
    }

    pub(crate) async fn list(&self, params: RenderJobListParams) -> Result<Value, ToolError> {
        if !(1..=MAX_JOB_LIST_LIMIT).contains(&params.limit) {
            return Err(ToolError::Validation(format!(
                "limit must be between 1 and {MAX_JOB_LIST_LIMIT}"
            )));
        }
        let state = self.inner.state.lock().await;
        let matching = state
            .order
            .iter()
            .rev()
            .filter_map(|job_id| state.jobs.get(job_id))
            .filter(|job| params.state.is_none_or(|filter| job.state == filter))
            .collect::<Vec<_>>();
        let jobs = matching
            .iter()
            .skip(params.offset)
            .take(params.limit)
            .map(|job| {
                json!({
                    "job_id": job.job_id,
                    "kind": job.kind,
                    "state": job.state,
                    "created_unix_ms": job.created_unix_ms,
                    "updated_unix_ms": job.updated_unix_ms,
                    "progress": job.progress,
                    "cancellation_requested": job.cancellation_requested,
                    "failure": job.failure,
                })
            })
            .collect::<Vec<_>>();
        let next_offset =
            (params.offset + jobs.len() < matching.len()).then_some(params.offset + jobs.len());
        Ok(json!({
            "jobs": jobs,
            "next_offset": next_offset,
            "retained_jobs": state.jobs.len(),
            "queue_depth": self.inner.queue_depth,
            "execution_mode": self.inner.isolation.name(),
            "worker_configured": self.inner.render_worker.is_some(),
            "worker_concurrency": 1,
        }))
    }

    pub(crate) async fn artifacts(
        &self,
        params: RenderJobArtifactsParams,
    ) -> Result<Value, ToolError> {
        if !(1..=MAX_JOB_LIST_LIMIT).contains(&params.limit) {
            return Err(ToolError::Validation(format!(
                "limit must be between 1 and {MAX_JOB_LIST_LIMIT}"
            )));
        }
        let state = self.inner.state.lock().await;
        let job = state
            .jobs
            .get(&params.job_id)
            .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
        let available = job.progress.completed_frames;
        let end = params
            .offset
            .saturating_add(params.limit as u32)
            .min(available);
        let frames = (params.offset..end)
            .map(|index| {
                json!({
                    "sequence_index": index + 1,
                    "source_frame": source_frame(job, index),
                    "path": frame_path(&job.job_id, index),
                    "media_type": "image/png",
                })
            })
            .collect::<Vec<_>>();
        let next_offset = (end < available).then_some(end);
        Ok(json!({
            "job_id": job.job_id,
            "state": job.state,
            "frames": frames,
            "next_offset": next_offset,
            "available_frames": available,
            "total_frames": job.progress.total_frames,
            "video": job.video_artifact,
            "mechanical_analysis": job.mechanical_analysis,
        }))
    }

    pub(crate) async fn cancel(&self, params: RenderJobCancelParams) -> Result<Value, ToolError> {
        let job_id = params.job_id;
        {
            let mut state = self.inner.state.lock().await;
            let job = state
                .jobs
                .get_mut(&job_id)
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            if !job.state.terminal() {
                job.cancellation_requested = true;
                job.updated_unix_ms = unix_ms();
                if job.state == JobState::Queued
                    && (!job.session_checkpoint_captured || job.session_checkpoint_restored)
                {
                    job.state = JobState::Cancelled;
                    job.progress.phase = "cancelled".to_string();
                    self.inner.queue.remove(&job_id);
                }
            }
        }
        let token = self.inner.cancellations.lock().await.get(&job_id).cloned();
        if let Some(token) = token {
            token.cancel();
        }
        self.inner.persist_job(&job_id).await?;
        self.status_value(&job_id).await
    }

    async fn worker_health(&self) -> Value {
        let Some(worker) = &self.inner.render_worker else {
            return json!({"configured": false, "available": false, "status": "not_configured"});
        };
        match worker
            .try_send_value(
                "bridge_status",
                Params::new(),
                Deadline::new(Duration::from_secs(5)),
            )
            .await
        {
            Ok(Some(identity)) => {
                let valid = identity["role"] == "render_worker" && identity["background"] == true;
                self.inner.worker_verified.store(valid, Ordering::SeqCst);
                json!({"configured": true, "available": valid, "status": if valid { "ready" } else { "role_mismatch" }})
            }
            Ok(None) => {
                json!({"configured": true, "available": self.inner.worker_verified.load(Ordering::SeqCst), "status": "busy"})
            }
            Err(_) => {
                self.inner.worker_verified.store(false, Ordering::SeqCst);
                json!({"configured": true, "available": false, "status": "unavailable"})
            }
        }
    }

    pub(crate) async fn health(&self) -> Value {
        let (queued, running) = {
            let state = self.inner.state.lock().await;
            (
                state
                    .jobs
                    .values()
                    .filter(|job| job.state == JobState::Queued)
                    .count(),
                state
                    .jobs
                    .values()
                    .filter(|job| job.state == JobState::Running)
                    .count(),
            )
        };
        let worker = self.worker_health().await;
        let encoder = self
            .inner
            .encoder_health
            .get_or_init(|| probe_encoder(self.inner.ffmpeg_bin.clone()))
            .await
            .clone();
        json!({
            "queue_depth": self.inner.queue_depth,
            "queued": queued,
            "running": running,
            "recovery_fenced": self.inner.blender.recovery_fenced(),
            "recovery_integrity": self.inner.recovery_integrity.report(),
            "encoder": encoder,
            "worker": worker,
            "execution_mode": self.inner.isolation.name(),
        })
    }
}

async fn probe_encoder(binary: PathBuf) -> Value {
    let mut command = Command::new(&binary);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("color=c=black:s=2x2:r=1")
        .arg("-frames:v")
        .arg("1")
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match command.spawn() {
        Ok(mut child) => match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(status)) if status.success() => json!({
                "available": true,
                "binary": binary,
                "encoder": "libx264",
            }),
            Ok(Ok(status)) => json!({
                "available": false,
                "binary": binary,
                "encoder": "libx264",
                "error": format!("FFmpeg libx264 encode probe exited with {status}"),
            }),
            Ok(Err(error)) => json!({
                "available": false,
                "binary": binary,
                "encoder": "libx264",
                "error": error.to_string(),
            }),
            Err(_) => {
                let cleanup = terminate_encoder_probe(&mut child).await;
                let error = match cleanup {
                    Ok(()) => "FFmpeg libx264 encode probe timed out".to_string(),
                    Err(cleanup) => format!(
                        "FFmpeg libx264 encode probe timed out and cleanup failed: {cleanup}"
                    ),
                };
                json!({
                    "available": false,
                    "binary": binary,
                    "encoder": "libx264",
                    "error": error,
                })
            }
        },
        Err(error) => json!({
            "available": false,
            "binary": binary,
            "encoder": "libx264",
            "error": error.to_string(),
        }),
    }
}

async fn terminate_encoder_probe(child: &mut tokio::process::Child) -> Result<(), String> {
    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return Err(format!("failed to terminate FFmpeg: {error}"));
    }
    child
        .wait()
        .await
        .map(|_| ())
        .map_err(|error| format!("failed to reap FFmpeg: {error}"))
}

impl JobRegistryInner {
    async fn stage_admitted_submission(
        self: Arc<Self>,
        admission: OwnedMutexGuard<()>,
        mut record: JobRecord,
    ) -> Result<(), ToolError> {
        let job_id = record.job_id.clone();
        let source_artifact = record.source_artifact.clone();
        let source_path = record.spec.source_blend.clone();
        let max_source_bytes = record.spec.max_source_bytes;
        let workspace = Arc::clone(&self.workspace);
        let (admission, snapshot, identity) = tokio::task::spawn_blocking(move || {
            let snapshot = workspace.snapshot_artifact_bounded(&source_path, max_source_bytes)?;
            let mut file = File::open(snapshot.path())?;
            let mut digest = Sha256::new();
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                digest.update(&chunk[..read]);
            }
            let identity = JobSourceSnapshot {
                sha256: digest
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                size_bytes: snapshot.meta().size_bytes,
            };
            Ok::<_, WsError>((admission, snapshot, identity))
        })
        .await
        .map_err(|error| ToolError::Job(format!("source snapshot task failed: {error}")))??;
        record.source_snapshot = Some(identity);

        {
            let mut state = self.state.lock().await;
            let mut cancellations = self.cancellations.lock().await;
            let evicted = evict_oldest_terminal(&mut state);
            state.order.push_back(job_id.clone());
            state.jobs.insert(job_id.clone(), record);
            for evicted_job_id in evicted {
                cancellations.remove(&evicted_job_id);
            }
            cancellations.insert(job_id.clone(), CancellationToken::new());
        }

        self.stage_admitted_job(
            admission,
            job_id,
            source_artifact,
            snapshot,
            max_source_bytes,
        )
        .await
    }

    async fn stage_admitted_job(
        self: Arc<Self>,
        _admission: OwnedMutexGuard<()>,
        job_id: String,
        source_artifact: String,
        snapshot: Snapshot,
        max_source_bytes: u64,
    ) -> Result<(), ToolError> {
        if let Err(error) = self.persist_job_and_index(&job_id).await {
            self.remove_job(&job_id).await;
            tracing::error!(job_id, %error, "failed to persist admitted render job");
            return Err(error);
        }

        let workspace = Arc::clone(&self.workspace);
        let commit = tokio::task::spawn_blocking(move || {
            workspace.commit_reserved_generated_artifact_bounded(
                &source_artifact,
                snapshot.path(),
                false,
                max_source_bytes,
            )
        })
        .await;
        match commit {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.fail_job(&job_id, error.code(), error.to_string())
                    .await;
                return Ok(());
            }
            Err(error) => {
                self.fail_job(
                    &job_id,
                    "source_staging_task",
                    format!("source commit task failed: {error}"),
                )
                .await;
                return Ok(());
            }
        }

        let should_enqueue = match self.mark_waiting_for_enqueue(&job_id).await {
            Ok(should_enqueue) => should_enqueue,
            Err(error) => {
                self.fail_job(
                    &job_id,
                    "job_metadata_io",
                    format!("could not persist staged render job: {error}"),
                )
                .await;
                return Ok(());
            }
        };
        if should_enqueue && let Err(error) = self.queue.push(job_id.clone(), self.queue_depth) {
            self.fail_job(&job_id, "queue_unavailable", error.to_string())
                .await;
        }
        Ok(())
    }

    async fn refresh_recovery_fence(&self) {
        let fenced = (self.recovery_integrity.blocked() && !self.isolation.established())
            || self
                .state
                .lock()
                .await
                .jobs
                .values()
                .any(|job| job.session_checkpoint_captured && !job.session_checkpoint_restored);
        self.blender.set_recovery_fenced(fenced);
    }

    async fn remove_job(&self, job_id: &str) {
        let mut state = self.state.lock().await;
        state.jobs.remove(job_id);
        state.order.retain(|candidate| candidate != job_id);
        self.cancellations.lock().await.remove(job_id);
    }

    async fn persist_job_and_index(&self, job_id: &str) -> Result<(), ToolError> {
        let persistence = Arc::clone(&self.persistence).lock_owned().await;
        let (record, index) = {
            let state = self.state.lock().await;
            let record = state
                .jobs
                .get(job_id)
                .cloned()
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            let index = JobIndex {
                schema_version: JOB_SCHEMA_VERSION,
                job_ids: state.order.iter().cloned().collect(),
            };
            (record, index)
        };
        let record_bytes = serde_json::to_vec_pretty(&record)?;
        let record_path = job_metadata_path(&record.job_id);
        let index_bytes = serde_json::to_vec_pretty(&index)?;
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || {
            let _persistence = persistence;
            workspace.write_reserved_artifact(&record_path, &record_bytes, true)?;
            workspace.write_reserved_artifact(INDEX_PATH, &index_bytes, true)
        })
        .await
        .map_err(|error| ToolError::Job(format!("job persistence task failed: {error}")))??;
        Ok(())
    }

    async fn persist_job(&self, job_id: &str) -> Result<(), ToolError> {
        let persistence = Arc::clone(&self.persistence).lock_owned().await;
        let record = {
            let state = self.state.lock().await;
            state
                .jobs
                .get(job_id)
                .cloned()
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?
        };
        self.persist_record_with_guard(persistence, record).await
    }

    async fn persist_record_with_guard(
        &self,
        persistence: OwnedMutexGuard<()>,
        record: JobRecord,
    ) -> Result<(), ToolError> {
        let bytes = serde_json::to_vec_pretty(&record)?;
        let path = job_metadata_path(&record.job_id);
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || {
            let _persistence = persistence;
            workspace.write_reserved_artifact(&path, &bytes, true)
        })
        .await
        .map_err(|error| ToolError::Job(format!("job metadata task failed: {error}")))??;
        Ok(())
    }

    async fn update_job(
        &self,
        job_id: &str,
        update: impl FnOnce(&mut JobRecord),
    ) -> Result<(), ToolError> {
        {
            let mut state = self.state.lock().await;
            let job = state
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            update(job);
            job.updated_unix_ms = unix_ms();
        }
        self.persist_job(job_id).await
    }

    async fn record_session_checkpoint_captured(&self, job_id: &str) -> Result<(), ToolError> {
        let persistence = self
            .update_job(job_id, |job| {
                job.session_checkpoint_captured = true;
                job.session_checkpoint_restored = false;
            })
            .await;
        self.refresh_recovery_fence().await;
        persistence
    }

    async fn persist_restored_marker_once(
        &self,
        job_id: &str,
        pending_failure: Option<JobFailure>,
    ) -> Result<(), ToolError> {
        let persistence = Arc::clone(&self.persistence).lock_owned().await;
        let candidate = {
            let state = self.state.lock().await;
            let mut candidate = state
                .jobs
                .get(job_id)
                .cloned()
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            if !candidate.session_checkpoint_captured {
                return Err(ToolError::Job(
                    "cannot persist session restoration before checkpoint capture".to_string(),
                ));
            }
            candidate.session_checkpoint_restored = true;
            candidate.pending_blender_outcome = None;
            candidate.failure = pending_failure.clone();
            candidate.updated_unix_ms = unix_ms();
            candidate
        };
        self.persist_record_with_guard(persistence, candidate.clone())
            .await?;
        {
            let mut state = self.state.lock().await;
            let job = state
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            job.session_checkpoint_restored = true;
            job.pending_blender_outcome = None;
            job.failure = pending_failure;
            job.updated_unix_ms = candidate.updated_unix_ms;
        }
        self.refresh_recovery_fence().await;
        Ok(())
    }

    async fn persist_restored_marker_until_durable(
        &self,
        job_id: &str,
        pending_failure: Option<JobFailure>,
    ) {
        let mut failures = 0_u32;
        loop {
            match self
                .persist_restored_marker_once(job_id, pending_failure.clone())
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    {
                        let mut state = self.state.lock().await;
                        if let Some(job) = state.jobs.get_mut(job_id) {
                            job.progress.phase =
                                "waiting_for_restoration_metadata_commit".to_string();
                            job.progress.current_frame = None;
                            job.updated_unix_ms = unix_ms();
                        }
                    }
                    self.refresh_recovery_fence().await;
                    let retry_delay = session_restore_retry_delay(failures);
                    tracing::error!(
                        job_id,
                        failures,
                        retry_seconds = retry_delay.as_secs(),
                        %error,
                        "Blender session was restored but its durable marker could not be committed; ordinary Blender access remains fenced"
                    );
                    tokio::time::sleep(retry_delay).await;
                }
            }
        }
    }

    async fn complete_job(
        &self,
        job_id: &str,
        video_artifact: Option<JobArtifact>,
    ) -> Result<JobState, ToolError> {
        let final_state = {
            let mut state = self.state.lock().await;
            let job = state
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            if job.state != JobState::Running {
                job.state
            } else if job.cancellation_requested {
                job.state = JobState::Cancelled;
                job.progress.phase = "cancelled".to_string();
                job.progress.current_frame = None;
                job.video_artifact = video_artifact;
                job.updated_unix_ms = unix_ms();
                JobState::Cancelled
            } else {
                job.state = JobState::Succeeded;
                job.progress.phase = "succeeded".to_string();
                job.progress.current_frame = None;
                job.video_artifact = video_artifact;
                job.updated_unix_ms = unix_ms();
                JobState::Succeeded
            }
        };
        self.persist_job(job_id).await?;
        Ok(final_state)
    }

    async fn mark_cancelled(&self, job_id: &str) -> Result<(), ToolError> {
        self.update_job(job_id, |job| {
            if !job.state.terminal() {
                job.state = JobState::Cancelled;
                job.progress.phase = "cancelled".to_string();
                job.progress.current_frame = None;
                job.cancellation_requested = true;
            }
        })
        .await
    }

    async fn record_pending_session_recovery_failure(
        &self,
        job_id: &str,
        failure: &RunFailure,
    ) -> Result<(), ToolError> {
        let failure = JobFailure {
            code: failure.code.clone(),
            message: failure.message.clone(),
        };
        self.update_job(job_id, move |job| {
            job.progress.phase = "waiting_for_session_recovery".to_string();
            job.progress.current_frame = None;
            job.failure = Some(failure);
        })
        .await
    }

    async fn record_pending_blender_outcome(
        &self,
        job_id: &str,
        outcome: PendingBlenderOutcome,
    ) -> Result<(), ToolError> {
        self.update_job(job_id, move |job| {
            job.pending_blender_outcome = Some(outcome);
        })
        .await
    }

    async fn retry_pending_blender_outcome_until_durable(
        &self,
        job_id: &str,
        outcome: PendingBlenderOutcome,
        mut error: ToolError,
    ) {
        let mut failures = 1_u32;
        loop {
            {
                let mut state = self.state.lock().await;
                if let Some(job) = state.jobs.get_mut(job_id) {
                    job.progress.phase = "waiting_for_blender_outcome_metadata_commit".to_string();
                    job.progress.current_frame = None;
                    job.updated_unix_ms = unix_ms();
                }
            }
            self.refresh_recovery_fence().await;
            let retry_delay = session_restore_retry_delay(failures);
            tracing::error!(
                job_id,
                failures,
                retry_seconds = retry_delay.as_secs(),
                %error,
                "completed Blender work could not be committed; session restoration is deferred behind the fence"
            );
            tokio::time::sleep(retry_delay).await;
            match self
                .record_pending_blender_outcome(job_id, outcome.clone())
                .await
            {
                Ok(()) => return,
                Err(next_error) => {
                    failures = failures.saturating_add(1);
                    error = next_error;
                }
            }
        }
    }

    async fn session_recovery_required(&self, job_id: &str) -> bool {
        self.state
            .lock()
            .await
            .jobs
            .get(job_id)
            .is_some_and(|job| job.session_checkpoint_captured && !job.session_checkpoint_restored)
    }

    async fn fail_job(&self, job_id: &str, code: impl Into<String>, message: impl Into<String>) {
        let code = code.into();
        let message = message.into();
        let changed = {
            let mut state = self.state.lock().await;
            match state.jobs.get_mut(job_id) {
                None => {
                    tracing::error!(
                        job_id,
                        "failed render job disappeared before failure persistence"
                    );
                    false
                }
                Some(job) if job.state.terminal() => false,
                Some(job) => {
                    job.state = JobState::Failed;
                    job.progress.phase = "failed".to_string();
                    job.progress.current_frame = None;
                    job.failure = Some(JobFailure { code, message });
                    job.updated_unix_ms = unix_ms();
                    true
                }
            }
        };
        if changed && let Err(error) = self.persist_job(job_id).await {
            tracing::error!(job_id, %error, "failed to persist render job failure");
        }
        self.refresh_recovery_fence().await;
        self.cancellations.lock().await.remove(job_id);
    }

    async fn mark_waiting_for_enqueue(&self, job_id: &str) -> Result<bool, ToolError> {
        let changed = {
            let mut state = self.state.lock().await;
            let job = state
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| ToolError::Job("render job not found".to_string()))?;
            if job.state != JobState::Queued || job.cancellation_requested {
                false
            } else {
                job.progress.phase = "waiting".to_string();
                job.updated_unix_ms = unix_ms();
                true
            }
        };
        if changed {
            self.persist_job(job_id).await?;
        }
        Ok(changed)
    }

    async fn start_job(&self, job_id: &str) -> Result<bool, ToolError> {
        {
            let mut state = self.state.lock().await;
            let Some(job) = state.jobs.get_mut(job_id) else {
                return Ok(false);
            };
            if job.state != JobState::Queued {
                return Ok(false);
            }
            job.state = JobState::Running;
            job.progress.phase = if job.session_checkpoint_restored || job.execution.finished() {
                "recovering_post_blender"
            } else if job.pending_blender_outcome.is_some() {
                "recovering_session"
            } else {
                "restoring_source"
            }
            .to_string();
            job.updated_unix_ms = unix_ms();
        }
        self.persist_job(job_id).await?;
        Ok(true)
    }
}

async fn job_worker(inner: Weak<JobRegistryInner>, queue: Arc<JobQueue>) {
    while let Some(job_id) = queue.receive().await {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        match inner.start_job(&job_id).await {
            Ok(true) => {}
            Ok(false) => {
                inner.cancellations.lock().await.remove(&job_id);
                continue;
            }
            Err(error) => match resolve_start_persistence_failure(&inner, &job_id, error).await {
                StartPersistenceResolution::Proceed => {}
                StartPersistenceResolution::Stop => continue,
            },
        }
        let token = inner
            .cancellations
            .lock()
            .await
            .get(&job_id)
            .cloned()
            .unwrap_or_else(CancellationToken::new);
        let mut restore_failures = 0_u32;
        loop {
            match run_job(&inner, &job_id, &token).await {
                Ok(RunOutcome::Completed(video)) => {
                    let result = inner.complete_job(&job_id, video).await;
                    if let Err(error) = result {
                        tracing::error!(job_id, %error, "failed to persist render job completion");
                    }
                }
                Ok(RunOutcome::Cancelled) => {
                    if let Err(error) = inner.mark_cancelled(&job_id).await {
                        tracing::error!(job_id, %error, "failed to persist render job cancellation");
                    }
                }
                Err(failure) => {
                    if inner.session_recovery_required(&job_id).await {
                        restore_failures = restore_failures.saturating_add(1);
                        if let Err(error) = inner
                            .record_pending_session_recovery_failure(&job_id, &failure)
                            .await
                        {
                            tracing::error!(job_id, %error, "failed to persist pending Blender session recovery");
                        }
                        let retry_delay = session_restore_retry_delay(restore_failures);
                        tracing::warn!(
                            job_id,
                            failures = restore_failures,
                            retry_seconds = retry_delay.as_secs(),
                            error = %failure.message,
                            "Blender session recovery remains fenced and will retry"
                        );
                        tokio::time::sleep(retry_delay).await;
                        continue;
                    }
                    if is_clean_cancellation(&failure) {
                        if let Err(error) = inner.mark_cancelled(&job_id).await {
                            tracing::error!(job_id, %error, "failed to persist render job cancellation");
                        }
                    } else {
                        inner.fail_job(&job_id, failure.code, failure.message).await;
                    }
                }
            }
            break;
        }
        inner.cancellations.lock().await.remove(&job_id);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartPersistenceResolution {
    Proceed,
    Stop,
}

async fn resolve_start_persistence_failure(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    mut error: ToolError,
) -> StartPersistenceResolution {
    if !inner.session_recovery_required(job_id).await {
        tracing::error!(job_id, %error, "failed to start render job");
        inner
            .fail_job(
                job_id,
                "job_metadata_io",
                format!("could not persist running state: {error}"),
            )
            .await;
        return StartPersistenceResolution::Stop;
    }

    let mut failures = 1_u32;
    loop {
        inner.refresh_recovery_fence().await;
        let retry_delay = session_restore_retry_delay(failures);
        tracing::error!(
            job_id,
            failures,
            retry_seconds = retry_delay.as_secs(),
            %error,
            "recovered render job running state could not be committed; recovery remains fenced and will retry"
        );
        tokio::time::sleep(retry_delay).await;
        match inner.persist_job(job_id).await {
            Ok(()) => return StartPersistenceResolution::Proceed,
            Err(next_error) => {
                failures = failures.saturating_add(1);
                error = next_error;
            }
        }
    }
}

#[derive(Debug)]
struct RunFailure {
    code: String,
    message: String,
}

enum RunOutcome {
    Completed(Option<JobArtifact>),
    Cancelled,
}

enum BlenderRunOutcome {
    Completed,
    Cancelled,
}

impl RunFailure {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

fn is_clean_cancellation(failure: &RunFailure) -> bool {
    failure.code == "cancelled"
}

fn session_restore_retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    Duration::from_secs(
        SESSION_RESTORE_RETRY_INITIAL_SECONDS
            .saturating_mul(1_u64 << exponent)
            .min(SESSION_RESTORE_RETRY_MAX_SECONDS),
    )
}

async fn run_job(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    cancel: &CancellationToken,
) -> Result<RunOutcome, RunFailure> {
    let record = current_job(inner, job_id).await?;
    if record.execution.isolated() {
        return run_isolated_job(inner, &record, cancel).await;
    }
    if record.session_checkpoint_restored {
        return finish_after_blender(inner, &record, cancel).await;
    }
    let frame_budget = runtime_duration(record.spec.frame_timeout_seconds, "frame timeout")?;
    let mut transaction = inner
        .blender
        .recovery_transaction(inner.blender.default_deadline())
        .await
        .map_err(blender_failure)?;

    if !record.session_checkpoint_captured {
        inner
            .update_job(job_id, |job| {
                job.progress.phase = "capturing_session".to_string();
            })
            .await
            .map_err(tool_failure)?;
        transaction
            .send_value_with_work_budget(
                "job_save_checkpoint",
                object_params(json!({ "path": record.session_checkpoint_artifact }))?,
                frame_budget,
            )
            .await
            .map_err(blender_failure)?;
        inner
            .record_session_checkpoint_captured(job_id)
            .await
            .map_err(tool_failure)?;
    }

    let pending_outcome = if let Some(pending) = record.pending_blender_outcome.clone() {
        pending
    } else {
        let blender_result = if cancel.is_cancelled() {
            Ok(BlenderRunOutcome::Cancelled)
        } else {
            run_blender_frames(
                inner,
                job_id,
                &record,
                cancel,
                &mut transaction,
                frame_budget,
            )
            .await
        };
        let pending = PendingBlenderOutcome::from_result(&blender_result);
        if let Err(error) = inner
            .record_pending_blender_outcome(job_id, pending.clone())
            .await
        {
            drop(transaction);
            inner
                .retry_pending_blender_outcome_until_durable(job_id, pending.clone(), error)
                .await;
            transaction = inner
                .blender
                .recovery_transaction(inner.blender.default_deadline())
                .await
                .map_err(blender_failure)?;
        }
        pending
    };
    let operation_summary = match &pending_outcome {
        PendingBlenderOutcome::Completed => "rendering completed".to_string(),
        PendingBlenderOutcome::Cancelled => "rendering was cancelled".to_string(),
        PendingBlenderOutcome::Failed(failure) => format!(
            "rendering failed with {}: {}",
            failure.code, failure.message
        ),
    };

    let restore_result = transaction
        .send_value_with_work_budget(
            "job_restore_checkpoint",
            object_params(json!({ "path": record.session_checkpoint_artifact }))?,
            frame_budget,
        )
        .await;
    if let Err(error) = restore_result {
        return Err(RunFailure::new(
            "session_restore_failed",
            format!(
                "failed to restore the pre-job Blender session from {} after {operation_summary}: {error}",
                record.session_checkpoint_artifact
            ),
        ));
    }
    let pending_failure = pending_outcome.failure();
    drop(transaction);
    inner
        .persist_restored_marker_until_durable(job_id, pending_failure)
        .await;

    match pending_outcome.into_result()? {
        BlenderRunOutcome::Cancelled => return Ok(RunOutcome::Cancelled),
        BlenderRunOutcome::Completed => {}
    }

    let completed_record = current_job(inner, job_id).await?;
    finish_after_blender(inner, &completed_record, cancel).await
}

async fn run_isolated_job(
    inner: &Arc<JobRegistryInner>,
    record: &JobRecord,
    cancel: &CancellationToken,
) -> Result<RunOutcome, RunFailure> {
    if record.source_snapshot.is_none() {
        return Err(RunFailure::new(
            "job_source_unbound",
            "isolated job has no checkpoint identity",
        ));
    }
    if record.execution.finished() {
        return finish_after_blender(inner, record, cancel).await;
    }
    if cancel.is_cancelled() {
        return Ok(RunOutcome::Cancelled);
    }
    let worker = inner.render_worker.as_ref().ok_or_else(|| {
        RunFailure::new(
            "render_worker_unavailable",
            "this job requires its separate background render worker",
        )
    })?;
    let mut transaction = worker
        .transaction(worker.default_deadline())
        .await
        .map_err(blender_failure)?;
    let identity = transaction
        .send_value("bridge_status", Params::new())
        .await
        .map_err(blender_failure)?;
    inner.worker_verified.store(false, Ordering::SeqCst);
    if identity["role"] != "render_worker" || identity["background"] != true {
        return Err(RunFailure::new(
            "render_worker_role_mismatch",
            "configured render endpoint is not a background render worker; no checkpoint was loaded",
        ));
    }
    inner.worker_verified.store(true, Ordering::SeqCst);
    let frame_budget = runtime_duration(record.spec.frame_timeout_seconds, "frame timeout")?;
    let outcome = run_blender_frames(
        inner,
        &record.job_id,
        record,
        cancel,
        &mut transaction,
        frame_budget,
    )
    .await;
    drop(transaction);
    let pending = PendingBlenderOutcome::from_result(&outcome);
    inner
        .update_job(&record.job_id, |job| {
            job.execution = JobExecution::IsolatedWorker {
                blender_finished: true,
            };
            job.failure = pending.failure();
        })
        .await
        .map_err(tool_failure)?;
    match pending.into_result()? {
        BlenderRunOutcome::Cancelled => Ok(RunOutcome::Cancelled),
        BlenderRunOutcome::Completed => {
            let record = current_job(inner, &record.job_id).await?;
            finish_after_blender(inner, &record, cancel).await
        }
    }
}

async fn finish_after_blender(
    inner: &JobRegistryInner,
    record: &JobRecord,
    cancel: &CancellationToken,
) -> Result<RunOutcome, RunFailure> {
    if let Some(failure) = &record.failure {
        return Err(RunFailure::new(&failure.code, &failure.message));
    }
    if cancel.is_cancelled() {
        return Ok(RunOutcome::Cancelled);
    }
    if record.progress.completed_frames != record.progress.total_frames {
        return Err(RunFailure::new(
            "job_recovery_invariant",
            "restored render job has no failure but its Blender frame work is incomplete",
        ));
    }
    if record.kind == RenderJobKind::Still {
        return Ok(RunOutcome::Completed(None));
    }
    inner
        .update_job(&record.job_id, |job| {
            job.progress.phase = "encoding".to_string();
        })
        .await
        .map_err(tool_failure)?;
    encode_video(inner, record, cancel)
        .await
        .map(|artifact| RunOutcome::Completed(Some(artifact)))
}

async fn run_blender_frames(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    record: &JobRecord,
    cancel: &CancellationToken,
    transaction: &mut Transaction<'_>,
    frame_budget: Duration,
) -> Result<BlenderRunOutcome, RunFailure> {
    inner
        .update_job(job_id, |job| {
            job.progress.phase = "restoring_source".to_string();
        })
        .await
        .map_err(tool_failure)?;
    let mut restore_params = json!({ "path": record.source_artifact });
    if let Some(source) = &record.source_snapshot {
        restore_params["expected_sha256"] = json!(source.sha256);
    }
    let restored = transaction
        .send_value_with_work_budget(
            "job_restore_checkpoint",
            object_params(restore_params)?,
            frame_budget,
        )
        .await
        .map_err(blender_failure)?;

    if cancel.is_cancelled() {
        return Ok(BlenderRunOutcome::Cancelled);
    }
    if record.kind == RenderJobKind::MechanicalRotation {
        prepare_and_certify_mechanical_rotation(inner, job_id, record, transaction, frame_budget)
            .await?;
    } else if record.spec.presentation.is_some() && record.presentation_bounds.is_none() {
        match record.kind {
            RenderJobKind::Still | RenderJobKind::Turntable => {
                let frame = restored
                    .get("frame_current")
                    .and_then(Value::as_i64)
                    .and_then(|frame| i32::try_from(frame).ok())
                    .filter(|frame| (-1_048_574..=1_048_574).contains(frame))
                    .ok_or_else(|| {
                        RunFailure::new(
                            "blender_protocol",
                            "Blender checkpoint restore omitted its current frame",
                        )
                    })?;
                prepare_static_presentation_bounds(inner, job_id, record, transaction, frame)
                    .await?;
            }
            RenderJobKind::Animation => {
                prepare_sequence_presentation_bounds(inner, job_id, record, transaction).await?;
            }
            RenderJobKind::MechanicalRotation => unreachable!("handled above"),
        }
    }

    let render_record = current_job(inner, job_id).await?;
    let start = render_record.progress.completed_frames;
    for index in start..render_record.progress.total_frames {
        if cancel.is_cancelled() {
            return Ok(BlenderRunOutcome::Cancelled);
        }
        let source = source_frame(record, index);
        inner
            .update_job(job_id, |job| {
                job.progress.phase = "rendering".to_string();
                job.progress.current_frame = Some(source);
            })
            .await
            .map_err(tool_failure)?;
        let remaining = record
            .spec
            .max_frame_sequence_bytes
            .checked_sub(current_job(inner, job_id).await?.progress.frame_bytes)
            .ok_or_else(|| {
                RunFailure::new(
                    "frame_storage_limit",
                    "rendered frames reached the caller-selected aggregate byte budget",
                )
            })?;
        if remaining == 0 {
            return Err(RunFailure::new(
                "frame_storage_limit",
                "rendered frames reached the caller-selected aggregate byte budget",
            ));
        }
        let params = render_params(&render_record, index, remaining)?;
        let command = if render_record.spec.presentation.is_some() {
            "job_render_product"
        } else if render_record.kind == RenderJobKind::Turntable {
            "job_render_views"
        } else if matches!(
            render_record.kind,
            RenderJobKind::Animation | RenderJobKind::MechanicalRotation
        ) {
            "job_render_frame"
        } else {
            "job_render_still"
        };
        let response = transaction
            .send_value_with_work_budget(command, params, frame_budget)
            .await
            .map_err(blender_failure)?;
        let presented = render_record.spec.presentation.is_some();
        if presented {
            validate_job_product_response(&response, &render_record, index)?;
        }
        let size_bytes = if presented {
            let bytes = verify_product_png_artifact(
                Arc::clone(&inner.workspace),
                frame_path(&render_record.job_id, index),
                &response,
                u64::from(render_record.spec.width),
                u64::from(render_record.spec.height),
            )
            .await
            .map_err(tool_failure)?;
            u64::try_from(bytes.len()).map_err(|_| {
                RunFailure::new(
                    "blender_protocol",
                    "verified product frame size could not be represented",
                )
            })?
        } else {
            rendered_size(&response, render_record.kind)?
        };
        inner
            .update_job(job_id, |job| {
                job.progress.completed_frames = index + 1;
                job.progress.current_frame = None;
                job.progress.frame_bytes = job.progress.frame_bytes.saturating_add(size_bytes);
            })
            .await
            .map_err(tool_failure)?;
    }
    Ok(if cancel.is_cancelled() {
        BlenderRunOutcome::Cancelled
    } else {
        BlenderRunOutcome::Completed
    })
}

async fn prepare_and_certify_mechanical_rotation(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    record: &JobRecord,
    transaction: &mut Transaction<'_>,
    frame_budget: Duration,
) -> Result<(), RunFailure> {
    let spec = record.spec.mechanical_rotation.as_ref().ok_or_else(|| {
        RunFailure::new(
            "invalid_job_metadata",
            "mechanical rotation job omitted its motion contract",
        )
    })?;
    let generation = record.recovery_count;
    let fixed_path = mechanical_fixed_path(job_id, generation);
    let moving_path = mechanical_moving_path(job_id, generation);
    let controller_name = format!("PrintableMechanical-{job_id}");
    inner
        .update_job(job_id, |job| {
            job.progress.phase = "preparing_mechanical_motion".to_string();
        })
        .await
        .map_err(tool_failure)?;
    let response = transaction
        .send_value_with_work_budget(
            "job_prepare_mechanical_rotation",
            object_params(json!({
                "fixed_objects": spec.fixed_objects,
                "moving_objects": spec.moving_objects,
                "controller_name": controller_name,
                "pivot": spec.pivot_mm,
                "axis": spec.axis,
                "angle_degrees": spec.angle_degrees,
                "frame_start": record.spec.frame_start,
                "frame_end": record.spec.frame_end,
                "fixed_path": fixed_path,
                "moving_path": moving_path,
                "max_output_bytes": spec.max_analysis_mesh_bytes,
                "timeout_seconds": record.spec.frame_timeout_seconds,
            }))?,
            frame_budget,
        )
        .await
        .map_err(blender_failure)?;
    let authored_rotation = validate_mechanical_prepare_response(
        &response,
        &fixed_path,
        &moving_path,
        &controller_name,
        spec,
        record.spec.frame_start,
        record.spec.frame_end,
    )?;
    inner
        .update_job(job_id, |job| {
            job.progress.phase = "analyzing_mechanical_clearance".to_string();
        })
        .await
        .map_err(tool_failure)?;
    let workspace = Arc::clone(&inner.workspace);
    let fixed_snapshot_path = fixed_path.clone();
    let moving_snapshot_path = moving_path.clone();
    let max_analysis_mesh_bytes = spec.max_analysis_mesh_bytes;
    let worker_override = inner.geometry_worker_bin.clone();
    let worker_memory_bytes = inner.geometry_worker_memory_bytes;
    let options = AssemblyOptions {
        required_clearance_mm: Some(spec.target_clearance_mm),
        motion: None,
        rotation: Some(authored_rotation),
    };
    let analysis = geometry_blocking(move || {
        let fixed =
            workspace.snapshot_artifact_bounded(&fixed_snapshot_path, max_analysis_mesh_bytes)?;
        let moving =
            workspace.snapshot_artifact_bounded(&moving_snapshot_path, max_analysis_mesh_bytes)?;
        let fixed_size_bytes = std::fs::metadata(fixed.path())?.len();
        let moving_size_bytes = std::fs::metadata(moving.path())?.len();
        let worker_bin = geometry_worker_path_from_override(worker_override.as_deref())?;
        let report = run_geometry_worker_files(
            &worker_bin,
            worker_memory_bytes,
            fixed.path(),
            moving.path(),
            options,
        )?;
        let certified = validate_mechanical_analysis_report(
            &report,
            options.rotation.expect("mechanical rotation options"),
        )?;
        Ok::<_, ToolError>(MechanicalAnalysis {
            generation,
            fixed_artifact: JobArtifact {
                path: fixed_snapshot_path,
                size_bytes: fixed_size_bytes,
                media_type: "model/stl".to_string(),
            },
            moving_artifact: JobArtifact {
                path: moving_snapshot_path,
                size_bytes: moving_size_bytes,
                media_type: "model/stl".to_string(),
            },
            units: "millimetres".to_string(),
            certified,
            report,
        })
    })
    .await
    .map_err(tool_failure)?;
    let certified = analysis.certified;
    let block_reason = analysis
        .report
        .get("rotation")
        .and_then(|rotation| rotation.get("block_reason"))
        .and_then(Value::as_str)
        .unwrap_or("clearance_not_certified")
        .to_string();
    let presentation_report =
        (certified && record.spec.presentation.is_some()).then(|| analysis.report.clone());
    inner
        .update_job(job_id, |job| {
            job.mechanical_analysis = Some(analysis);
        })
        .await
        .map_err(tool_failure)?;
    if !certified {
        return Err(RunFailure::new(
            "mechanical_clearance_not_certified",
            format!(
                "mechanical rotation is not certified for the requested clearance: {block_reason}"
            ),
        ));
    }
    if let Some(report) = presentation_report {
        let presentation_bounds = mechanical_presentation_bounds(&report, spec.pivot_mm)?;
        inner
            .update_job(job_id, move |job| {
                job.presentation_bounds = Some(presentation_bounds);
            })
            .await
            .map_err(tool_failure)?;
    }
    Ok(())
}

async fn prepare_sequence_presentation_bounds(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    record: &JobRecord,
    transaction: &mut Transaction<'_>,
) -> Result<(), RunFailure> {
    let (frame_end, frame_step, expected_frames, timeout_seconds) =
        if record.spec.auto_frame_sequence {
            (
                record.spec.frame_end,
                record.spec.frame_step,
                record.progress.total_frames,
                record.spec.auto_frame_sequence_timeout_seconds,
            )
        } else {
            (
                record.spec.frame_start,
                1,
                1,
                record.spec.frame_timeout_seconds,
            )
        };
    measure_and_persist_presentation_bounds(
        inner,
        job_id,
        transaction,
        record.spec.frame_start..=frame_end,
        frame_step,
        expected_frames,
        timeout_seconds,
    )
    .await
}

async fn prepare_static_presentation_bounds(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    record: &JobRecord,
    transaction: &mut Transaction<'_>,
    frame: i32,
) -> Result<(), RunFailure> {
    measure_and_persist_presentation_bounds(
        inner,
        job_id,
        transaction,
        frame..=frame,
        1,
        1,
        record.spec.frame_timeout_seconds,
    )
    .await
}

async fn measure_and_persist_presentation_bounds(
    inner: &Arc<JobRegistryInner>,
    job_id: &str,
    transaction: &mut Transaction<'_>,
    frames: std::ops::RangeInclusive<i32>,
    frame_step: u32,
    expected_frames: u32,
    timeout_seconds: f64,
) -> Result<(), RunFailure> {
    let frame_start = *frames.start();
    let frame_end = *frames.end();
    let budget = runtime_duration(timeout_seconds, "presentation framing timeout")?;
    inner
        .update_job(job_id, |job| {
            job.progress.phase = "measuring_presentation_bounds".to_string();
        })
        .await
        .map_err(tool_failure)?;
    let response = transaction
        .send_value_with_work_budget(
            "job_measure_sequence_bounds",
            object_params(json!({
                "frame_start": frame_start,
                "frame_end": frame_end,
                "frame_step": frame_step,
                "timeout_seconds": timeout_seconds,
            }))?,
            budget,
        )
        .await
        .map_err(blender_failure)?;
    let bounds = validate_presentation_bounds_response(
        &response,
        frame_start,
        frame_end,
        frame_step,
        expected_frames,
    )?;
    inner
        .update_job(job_id, move |job| {
            job.presentation_bounds = Some(bounds);
        })
        .await
        .map_err(tool_failure)
}

fn validate_presentation_bounds_response(
    response: &Value,
    frame_start: i32,
    frame_end: i32,
    frame_step: u32,
    expected_frames: u32,
) -> Result<PresentationBounds, RunFailure> {
    if response.get("frames_evaluated").and_then(Value::as_u64) != Some(u64::from(expected_frames))
        || response.get("frame_start").and_then(Value::as_i64) != Some(i64::from(frame_start))
        || response.get("frame_end").and_then(Value::as_i64) != Some(i64::from(frame_end))
        || response.get("frame_step").and_then(Value::as_u64) != Some(u64::from(frame_step))
    {
        return Err(RunFailure::new(
            "blender_protocol",
            "Blender sequence framing response did not match the requested timeline",
        ));
    }
    validate_presentation_bounds(response.get("bounds"))
}

fn validate_mechanical_analysis_report(
    report: &Value,
    expected: RotationalMotion,
) -> Result<bool, ToolError> {
    let invalid = || ToolError::GeometryWorker {
        code: "geometry_worker_protocol",
        message: "assembly geometry worker returned a mismatched rotational certificate"
            .to_string(),
    };
    let complete =
        serde_json::from_value::<AssemblyReport>(report.clone()).map_err(|_| invalid())?;
    if !mechanical_report_is_internally_consistent(&complete) {
        return Err(invalid());
    }
    let rotation = complete.rotation.as_ref().ok_or_else(invalid)?;
    let expected_axis = normalized_mechanical_axis(expected.axis).ok_or_else(invalid)?;
    let target_clearance_mm = expected.target_clearance_mm.unwrap_or(0.0);
    let axis_matches = rotation
        .axis
        .iter()
        .zip(expected_axis)
        .all(|(actual, expected)| (actual - expected).abs() <= 1e-12);
    let expected_static_clearance = Some(target_clearance_mm);
    let static_meets = complete.static_analysis.relation
        != printable_geom::AssemblyRelation::Interfering
        && complete.static_analysis.clearance_mm >= target_clearance_mm;
    if complete.motion.is_some()
        || complete.static_analysis.required_clearance_mm != expected_static_clearance
        || complete.static_analysis.meets_required_clearance != Some(static_meets)
        || rotation.pivot_mm != expected.pivot_mm
        || !axis_matches
        || rotation.angle_degrees != expected.angle_degrees
        || rotation.target_clearance_mm != target_clearance_mm
    {
        return Err(invalid());
    }
    if rotation.can_rotate_full_angle {
        if complete.static_analysis.relation != printable_geom::AssemblyRelation::Separated
            || complete.static_analysis.clearance_mm <= target_clearance_mm
        {
            return Err(invalid());
        }
        let minimum = rotation
            .minimum_certified_clearance_mm
            .ok_or_else(invalid)?;
        if minimum <= target_clearance_mm || rotation.block_reason.is_some() {
            return Err(invalid());
        }
    } else if rotation.block_reason.is_none() {
        return Err(invalid());
    }
    Ok(rotation.can_rotate_full_angle)
}

fn mechanical_report_is_internally_consistent(report: &AssemblyReport) -> bool {
    valid_assembly_part(&report.fixed)
        && valid_assembly_part(&report.moving)
        && valid_static_analysis(&report.static_analysis, &report.fixed, &report.moving)
        && report.motion.is_none()
        && report
            .rotation
            .as_ref()
            .is_some_and(|rotation| valid_rotation_report(rotation, &report.static_analysis))
}

fn valid_assembly_part(part: &AssemblyPartSummary) -> bool {
    if part.vertices < 4
        || part.triangles < 4
        || !part.volume_mm3.is_finite()
        || part.volume_mm3 <= 0.0
    {
        return false;
    }
    part.bounds
        .minimum_mm
        .iter()
        .zip(part.bounds.maximum_mm)
        .zip(part.bounds.dimensions_mm)
        .all(|((minimum, maximum), dimension)| {
            minimum.is_finite()
                && maximum.is_finite()
                && dimension.is_finite()
                && maximum > *minimum
                && dimension == maximum - minimum
        })
}

fn valid_static_analysis(
    static_analysis: &AssemblyStaticReport,
    fixed: &AssemblyPartSummary,
    moving: &AssemblyPartSummary,
) -> bool {
    if static_analysis.fixed_interference_fraction > 1.0 + 1e-9
        || static_analysis.moving_interference_fraction > 1.0 + 1e-9
        || !report_numbers_match(
            static_analysis.fixed_interference_fraction,
            static_analysis.interference_volume_mm3 / fixed.volume_mm3,
        )
        || !report_numbers_match(
            static_analysis.moving_interference_fraction,
            static_analysis.interference_volume_mm3 / moving.volume_mm3,
        )
    {
        return false;
    }
    let relation_matches = match static_analysis.relation {
        AssemblyRelation::Separated => {
            static_analysis.interference_volume_mm3 == 0.0
                && static_analysis.surface_gap_mm > 0.0
                && report_numbers_match(
                    static_analysis.clearance_mm,
                    static_analysis.surface_gap_mm,
                )
        }
        AssemblyRelation::Contact => {
            static_analysis.interference_volume_mm3 == 0.0
                && static_analysis.surface_gap_mm == 0.0
                && static_analysis.clearance_mm == 0.0
        }
        AssemblyRelation::Interfering => {
            static_analysis.interference_volume_mm3 > 0.0 && static_analysis.clearance_mm == 0.0
        }
    };
    if !relation_matches {
        return false;
    }
    let witnesses_match = match &static_analysis.closest_surface_points {
        Some(witnesses) => report_numbers_match(
            witnesses
                .fixed_mm
                .iter()
                .zip(witnesses.moving_mm)
                .map(|(fixed, moving)| (moving - fixed).powi(2))
                .sum::<f64>()
                .sqrt(),
            static_analysis.surface_gap_mm,
        ),
        None => static_analysis.surface_gap_mm == 0.0,
    };
    if !witnesses_match {
        return false;
    }
    match static_analysis.required_clearance_mm {
        Some(required) if required.is_finite() && required >= 0.0 => {
            static_analysis.meets_required_clearance
                == Some(
                    static_analysis.relation != AssemblyRelation::Interfering
                        && static_analysis.clearance_mm >= required,
                )
        }
        None => static_analysis.meets_required_clearance.is_none(),
        Some(_) => false,
    }
}

fn valid_rotation_report(
    rotation: &RotationalMotionReport,
    static_analysis: &AssemblyStaticReport,
) -> bool {
    if !rotation.pivot_mm.iter().all(|value| value.is_finite())
        || !rotation.axis.iter().all(|value| value.is_finite())
        || !rotation.angle_degrees.is_finite()
        || rotation.angle_degrees <= 0.0
        || !rotation.target_clearance_mm.is_finite()
        || rotation.target_clearance_mm < 0.0
    {
        return false;
    }
    if rotation.can_rotate_full_angle {
        let Some(clearance_at_end_mm) = rotation.clearance_at_end_mm else {
            return false;
        };
        let Some(minimum_certified_clearance_mm) = rotation.minimum_certified_clearance_mm else {
            return false;
        };
        return !rotation.retained
            && static_analysis.relation == AssemblyRelation::Separated
            && static_analysis.clearance_mm > rotation.target_clearance_mm
            && rotation.first_limit_interval_degrees.is_none()
            && rotation.block_reason.is_none()
            && clearance_at_end_mm.is_finite()
            && minimum_certified_clearance_mm.is_finite()
            && minimum_certified_clearance_mm > rotation.target_clearance_mm
            && minimum_certified_clearance_mm <= static_analysis.clearance_mm
            && minimum_certified_clearance_mm <= clearance_at_end_mm;
    }
    let Some(interval) = rotation.first_limit_interval_degrees else {
        return false;
    };
    let Some(reason) = rotation.block_reason else {
        return false;
    };
    let retained = matches!(
        reason,
        MotionBlockReason::InitialInterference | MotionBlockReason::Contact
    );
    let initial_reason = if static_analysis.relation == AssemblyRelation::Interfering {
        Some(MotionBlockReason::InitialInterference)
    } else if static_analysis.clearance_mm < rotation.target_clearance_mm {
        Some(MotionBlockReason::InsufficientInitialClearance)
    } else if static_analysis.clearance_mm == rotation.target_clearance_mm {
        Some(if rotation.target_clearance_mm > 0.0 {
            MotionBlockReason::ClearanceThreshold
        } else {
            MotionBlockReason::Contact
        })
    } else {
        None
    };
    let reason_matches_start = initial_reason.map_or(
        !matches!(
            reason,
            MotionBlockReason::InitialInterference
                | MotionBlockReason::InsufficientInitialClearance
        ),
        |initial| reason == initial && interval == [0.0, 0.0],
    );
    interval.into_iter().all(|value| value.is_finite())
        && interval[0] >= 0.0
        && interval[0] <= interval[1]
        && interval[1] <= rotation.angle_degrees
        && reason_matches_start
        && rotation.retained == retained
        && rotation.clearance_at_end_mm.is_none()
        && rotation.minimum_certified_clearance_mm.is_none()
}

fn report_numbers_match(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= 1e-9 * left.abs().max(right.abs()).max(1.0)
}

fn validate_mechanical_prepare_response(
    response: &Value,
    fixed_path: &str,
    moving_path: &str,
    controller_name: &str,
    spec: &MechanicalRotationSpec,
    frame_start: i32,
    frame_end: i32,
) -> Result<RotationalMotion, RunFailure> {
    let response =
        serde_json::from_value::<MechanicalPrepareResponse>(response.clone()).map_err(|_| {
            RunFailure::new(
                "blender_protocol",
                "Blender mechanical preparation response was malformed",
            )
        })?;
    let authored = RotationalMotion {
        pivot_mm: response.motion.pivot,
        axis: response.motion.axis,
        angle_degrees: response.motion.angle_degrees,
        target_clearance_mm: Some(spec.target_clearance_mm),
    };
    if response.fixed_path != fixed_path
        || response.moving_path != moving_path
        || response.motion.controller != controller_name
        || response.motion.objects != spec.moving_objects
        || !mechanical_authored_motion_matches_spec(&authored, spec)
        || response.motion.frame_start != frame_start
        || response.motion.frame_end != frame_end
        || response.motion.interpolation != "LINEAR"
    {
        return Err(RunFailure::new(
            "blender_protocol",
            "Blender mechanical preparation response did not match the requested rigid motion",
        ));
    }
    Ok(authored)
}

fn mechanical_authored_motion_matches_spec(
    authored: &RotationalMotion,
    spec: &MechanicalRotationSpec,
) -> bool {
    let Some(authored_axis) = normalized_mechanical_axis(authored.axis) else {
        return false;
    };
    let Some(expected_axis) = normalized_mechanical_axis(spec.axis) else {
        return false;
    };
    authored.target_clearance_mm == Some(spec.target_clearance_mm)
        && authored
            .pivot_mm
            .iter()
            .zip(spec.pivot_mm)
            .all(|(actual, expected)| blender_motion_numbers_match(*actual, expected))
        && authored_axis
            .iter()
            .zip(expected_axis)
            .all(|(actual, expected)| blender_motion_numbers_match(*actual, expected))
        && authored.angle_degrees.is_finite()
        && authored.angle_degrees > 0.0
        && blender_motion_numbers_match(authored.angle_degrees, spec.angle_degrees)
}

fn blender_motion_numbers_match(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= 1e-6 * left.abs().max(right.abs()).max(1.0)
}

fn recovered_mechanical_rotation(
    report: &Value,
    spec: &MechanicalRotationSpec,
) -> Option<RotationalMotion> {
    let complete = serde_json::from_value::<AssemblyReport>(report.clone()).ok()?;
    let rotation = complete.rotation?;
    let authored = RotationalMotion {
        pivot_mm: rotation.pivot_mm,
        axis: rotation.axis,
        angle_degrees: rotation.angle_degrees,
        target_clearance_mm: Some(rotation.target_clearance_mm),
    };
    mechanical_authored_motion_matches_spec(&authored, spec).then_some(authored)
}

fn normalized_mechanical_axis(axis: [f64; 3]) -> Option<[f64; 3]> {
    let scale = axis
        .iter()
        .map(|component| component.abs())
        .fold(0.0_f64, f64::max);
    let scaled = axis.map(|component| component / scale);
    let magnitude = scaled[0].hypot(scaled[1]).hypot(scaled[2]);
    magnitude
        .is_finite()
        .then(|| scaled.map(|component| component / magnitude))
}

async fn current_job(inner: &JobRegistryInner, job_id: &str) -> Result<JobRecord, RunFailure> {
    inner
        .state
        .lock()
        .await
        .jobs
        .get(job_id)
        .cloned()
        .ok_or_else(|| RunFailure::new("job_not_found", "render job disappeared"))
}

fn render_params(
    record: &JobRecord,
    index: u32,
    remaining_bytes: u64,
) -> Result<Params, RunFailure> {
    let output_budget = if record.spec.presentation.is_some() {
        remaining_bytes.min(MAX_PRODUCT_RENDER_BYTES)
    } else {
        remaining_bytes
    };
    let mut value = json!({
        "width": record.spec.width,
        "height": record.spec.height,
        "engine": record.spec.engine.addon_name(),
        "timeout_seconds": record.spec.frame_timeout_seconds,
        "max_output_bytes": output_budget,
    });
    if let Some(samples) = record.spec.samples {
        value["samples"] = json!(samples);
    }
    if let Some(presentation) = &record.spec.presentation {
        value["presentation"] = serde_json::to_value(presentation).map_err(|_| {
            RunFailure::new(
                "invalid_job_metadata",
                "persisted product presentation could not be serialized",
            )
        })?;
        if matches!(record.kind, RenderJobKind::Still | RenderJobKind::Turntable) {
            value["camera_behavior"] = json!("bounds");
            let bounds = record.presentation_bounds.as_ref().ok_or_else(|| {
                RunFailure::new(
                    "invalid_job_metadata",
                    "static product presentation omitted its stable framing bounds",
                )
            })?;
            value["framing_bounds"] = serde_json::to_value(bounds).map_err(|_| {
                RunFailure::new(
                    "invalid_job_metadata",
                    "persisted presentation bounds could not be serialized",
                )
            })?;
        }
        match record.kind {
            RenderJobKind::Still => {
                value["path"] = json!(frame_path(&record.job_id, index));
            }
            RenderJobKind::Turntable => {
                let direction = turntable_direction(record, index);
                value["path"] = json!(frame_path(&record.job_id, index));
                value["presentation"]["view"] = json!(product_view_from_direction(direction));
            }
            RenderJobKind::Animation => {
                value["path"] = json!(frame_path(&record.job_id, index));
                value["frame"] = json!(source_frame(record, index));
                value["camera_behavior"] = json!(if record.spec.auto_frame_sequence {
                    "bounds"
                } else {
                    "preserve"
                });
                let bounds = record.presentation_bounds.as_ref().ok_or_else(|| {
                    RunFailure::new(
                        "invalid_job_metadata",
                        "animation presentation omitted its stable framing bounds",
                    )
                })?;
                value["framing_bounds"] = serde_json::to_value(bounds).map_err(|_| {
                    RunFailure::new(
                        "invalid_job_metadata",
                        "persisted presentation bounds could not be serialized",
                    )
                })?;
            }
            RenderJobKind::MechanicalRotation => {
                value["path"] = json!(frame_path(&record.job_id, index));
                value["frame"] = json!(source_frame(record, index));
                value["camera_behavior"] = json!("bounds");
                value["allow_ground"] = json!(false);
                let bounds = record.presentation_bounds.as_ref().ok_or_else(|| {
                    RunFailure::new(
                        "invalid_job_metadata",
                        "mechanical presentation omitted its certified rotation envelope",
                    )
                })?;
                value["framing_bounds"] = serde_json::to_value(bounds).map_err(|_| {
                    RunFailure::new(
                        "invalid_job_metadata",
                        "persisted presentation bounds could not be serialized",
                    )
                })?;
            }
        }
        return object_params(value);
    }
    match record.kind {
        RenderJobKind::Still => {
            value["path"] = json!(frame_path(&record.job_id, index));
        }
        RenderJobKind::Animation | RenderJobKind::MechanicalRotation => {
            value["path"] = json!(frame_path(&record.job_id, index));
            value["frame"] = json!(source_frame(record, index));
        }
        RenderJobKind::Turntable => {
            let direction = turntable_direction(record, index);
            value["views"] = json!([{
                "path": frame_path(&record.job_id, index),
                "label": format!("Frame {}", index + 1),
                "direction": direction,
            }]);
            value
                .as_object_mut()
                .expect("object")
                .remove("max_output_bytes");
            value["max_output_bytes"] = json!(remaining_bytes);
        }
    }
    object_params(value)
}

fn validate_job_product_response(
    response: &Value,
    record: &JobRecord,
    index: u32,
) -> Result<(), RunFailure> {
    let invalid = || {
        RunFailure::new(
            "blender_protocol",
            "Blender product job response did not match the requested frame presentation",
        )
    };
    let presentation = record.spec.presentation.as_ref().ok_or_else(invalid)?;
    let expected_path = frame_path(&record.job_id, index);
    if response.get("path").and_then(Value::as_str) != Some(expected_path.as_str())
        || response.get("media_type").and_then(Value::as_str) != Some("image/png")
        || response.get("width").and_then(Value::as_u64) != Some(u64::from(record.spec.width))
        || response.get("height").and_then(Value::as_u64) != Some(u64::from(record.spec.height))
        || response
            .get("source_state_verified")
            .and_then(Value::as_bool)
            != Some(true)
        || response.get("cleanup_verified").and_then(Value::as_bool) != Some(true)
        || response
            .get("objects")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(invalid());
    }
    let expected_frame = matches!(
        record.kind,
        RenderJobKind::Animation | RenderJobKind::MechanicalRotation
    )
    .then(|| source_frame(record, index));
    match expected_frame {
        Some(frame) if response.get("frame").and_then(Value::as_i64) != Some(frame) => {
            return Err(invalid());
        }
        None if response.get("frame").is_some() => return Err(invalid()),
        _ => {}
    }
    let actual = response
        .get("presentation")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if actual.get("profile").and_then(Value::as_str) != Some(presentation.profile.name()) {
        return Err(invalid());
    }
    if !product_controls_match(actual, presentation) {
        return Err(invalid());
    }
    let expected_shading = presentation
        .surface_shading
        .unwrap_or_else(|| presentation.profile.default_shading());
    if !product_materials_and_shading_match(actual, &presentation.materials, expected_shading) {
        return Err(invalid());
    }
    let expected_camera_behavior = match record.kind {
        RenderJobKind::Animation if !record.spec.auto_frame_sequence => "preserve",
        RenderJobKind::Animation | RenderJobKind::MechanicalRotation => "bounds",
        RenderJobKind::Still | RenderJobKind::Turntable => "bounds",
    };
    let camera = actual
        .get("camera")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if camera.get("behavior").and_then(Value::as_str) != Some(expected_camera_behavior) {
        return Err(invalid());
    }
    if expected_camera_behavior == "preserve" {
        let position = camera
            .get("position")
            .and_then(Value::as_array)
            .filter(|values| values.len() == 3)
            .ok_or_else(invalid)?;
        let camera_z = position
            .get(2)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .ok_or_else(invalid)?;
        if !position
            .iter()
            .all(|value| value.as_f64().is_some_and(f64::is_finite))
        {
            return Err(invalid());
        }
        let ground = actual
            .get("ground")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        let ground_valid = match presentation.profile {
            crate::tools::ProductPresentationProfile::Engineering => {
                ground.len() == 1 && ground.get("enabled").and_then(Value::as_bool) == Some(false)
            }
            crate::tools::ProductPresentationProfile::StudioNeutral
            | crate::tools::ProductPresentationProfile::StudioDark => {
                ground.get("enabled").and_then(Value::as_bool) == Some(true)
                    && ground
                        .get("z")
                        .and_then(Value::as_f64)
                        .is_some_and(|ground_z| ground_z.is_finite() && camera_z > ground_z)
            }
        };
        if !ground_valid {
            return Err(invalid());
        }
    }
    if expected_camera_behavior != "preserve" {
        let expected_view = match record.kind {
            RenderJobKind::Turntable => {
                product_view_from_direction(turntable_direction(record, index))
            }
            _ => serde_json::to_value(&presentation.view).map_err(|_| invalid())?,
        };
        for name in ["azimuth_degrees", "elevation_degrees"] {
            let expected = expected_view
                .get(name)
                .and_then(Value::as_f64)
                .ok_or_else(invalid)?;
            let observed = camera
                .get(name)
                .and_then(Value::as_f64)
                .ok_or_else(invalid)?;
            if !approximately_equal(expected, observed) {
                return Err(invalid());
            }
        }
    }
    let framing = actual
        .get("framing")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let framing_bounds = validate_presentation_bounds(framing.get("bounds"))?;
    let source_bounds = validate_presentation_bounds(response.get("bounds"))?;
    if framing_bounds != source_bounds {
        return Err(invalid());
    }
    if let Some(expected_bounds) = &record.presentation_bounds
        && &framing_bounds != expected_bounds
    {
        return Err(invalid());
    }
    if record.kind == RenderJobKind::MechanicalRotation
        && actual
            .get("ground")
            .and_then(|ground| ground.get("enabled"))
            .and_then(Value::as_bool)
            != Some(false)
    {
        return Err(invalid());
    }
    Ok(())
}

fn rendered_size(response: &Value, kind: RenderJobKind) -> Result<u64, RunFailure> {
    let size = if kind == RenderJobKind::Turntable {
        response
            .get("views")
            .and_then(Value::as_array)
            .and_then(|views| views.first())
            .and_then(|view| view.get("size_bytes"))
    } else {
        response.get("size_bytes")
    };
    size.and_then(Value::as_u64)
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            RunFailure::new(
                "blender_protocol",
                "Blender render response omitted a positive artifact byte count",
            )
        })
}

fn product_view_from_direction(direction: [f64; 3]) -> Value {
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    let normalized = direction.map(|component| component / magnitude);
    json!({
        "azimuth_degrees": normalized[1].atan2(normalized[0]).to_degrees(),
        "elevation_degrees": normalized[2].clamp(-1.0, 1.0).asin().to_degrees(),
    })
}

async fn encode_video(
    inner: &JobRegistryInner,
    record: &JobRecord,
    cancel: &CancellationToken,
) -> Result<JobArtifact, RunFailure> {
    let encode_budget = runtime_duration(record.spec.encode_timeout_seconds, "encoding timeout")?;
    let deadline = tokio::time::Instant::now()
        .checked_add(encode_budget)
        .ok_or_else(|| {
            RunFailure::new(
                "encoder_timeout",
                "caller-selected encoding budget cannot be represented as a runtime deadline",
            )
        })?;
    let frames_dir = inner
        .workspace
        .resolve(&format!("{JOB_ROOT}/{}/frames", record.job_id), true)
        .map_err(workspace_failure)?;
    let output_dir = inner
        .workspace
        .scratch(record.spec.max_video_bytes, "video_encoding")
        .map_err(workspace_failure)?;
    let staged_video = output_dir.path().join("video.mp4");
    let input_pattern = frames_dir.join("frame-%06d.png");
    let mut command = Command::new(&inner.ffmpeg_bin);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-framerate")
        .arg(record.spec.frames_per_second.to_string())
        .arg("-start_number")
        .arg("1")
        .arg("-i")
        .arg(input_pattern)
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-vf")
        .arg("pad=ceil(iw/2)*2:ceil(ih/2)*2")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-fs")
        .arg(record.spec.max_video_bytes.to_string())
        .arg("-y")
        .arg(&staged_video)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    output_dir
        .retain_for_command(command.as_std_mut())
        .map_err(|error| {
            RunFailure::new(
                "encoder_io",
                format!("could not retain encoder storage: {error}"),
            )
        })?;
    let mut child = command.spawn().map_err(|error| {
        RunFailure::new(
            "encoder_unavailable",
            format!("failed to start FFmpeg: {error}"),
        )
    })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunFailure::new("encoder_io", "FFmpeg diagnostic pipe was unavailable"))?;
    let diagnostics = tokio::spawn(read_bounded(stderr, MAX_ENCODER_DIAGNOSTIC_BYTES));
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let outcome = tokio::select! {
        status = child.wait() => status.map_err(io_failure),
        () = cancel.cancelled() => {
            terminate_encoder(&mut child, diagnostics, "cancellation").await?;
            return Err(RunFailure::new("cancelled", "video encoding was cancelled"));
        }
        () = &mut timeout => {
            terminate_encoder(&mut child, diagnostics, "timeout").await?;
            return Err(RunFailure::new("encoder_timeout", "FFmpeg exceeded the caller-selected encoding budget"));
        }
    }?;
    let diagnostic_bytes = diagnostics
        .await
        .map_err(|error| RunFailure::new("encoder_io", error.to_string()))?
        .map_err(io_failure)?;
    let diagnostics = String::from_utf8_lossy(&diagnostic_bytes)
        .trim()
        .to_string();
    if !outcome.success() {
        return Err(RunFailure::new(
            classify_encoder_failure(&diagnostics),
            if diagnostics.is_empty() {
                format!("FFmpeg exited with {outcome}")
            } else {
                format!("FFmpeg failed: {diagnostics}")
            },
        ));
    }
    if cancel.is_cancelled() {
        return Err(RunFailure::new("cancelled", "video encoding was cancelled"));
    }
    let validation_path = staged_video.clone();
    let max_video_bytes = record.spec.max_video_bytes;
    tokio::task::spawn_blocking(move || validate_mp4(&validation_path, max_video_bytes))
        .await
        .map_err(|error| {
            RunFailure::new(
                "encoder_finalization",
                format!("video structure validation task failed: {error}"),
            )
        })??;
    validate_decodable_video(
        &inner.ffmpeg_bin,
        &staged_video,
        record.progress.total_frames,
        record.spec.frames_per_second,
        deadline,
        cancel,
    )
    .await?;
    if cancel.is_cancelled() {
        return Err(RunFailure::new(
            "cancelled",
            "video validation was cancelled",
        ));
    }
    let destination = format!("{JOB_ROOT}/{}/video.mp4", record.job_id);
    let workspace = Arc::clone(&inner.workspace);
    tokio::task::spawn_blocking(move || {
        workspace
            .commit_reserved_generated_artifact_bounded(
                &destination,
                &staged_video,
                true,
                max_video_bytes,
            )
            .map(JobArtifact::from)
            .map_err(workspace_failure)
    })
    .await
    .map_err(|error| {
        RunFailure::new(
            "encoder_finalization",
            format!("video finalization task failed: {error}"),
        )
    })?
}

async fn validate_decodable_video(
    ffmpeg_bin: &std::path::Path,
    path: &std::path::Path,
    expected_frames: u32,
    frames_per_second: u16,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Result<(), RunFailure> {
    if tokio::time::Instant::now() >= deadline {
        return Err(RunFailure::new(
            "encoder_timeout",
            "FFmpeg encoding and video validation exceeded the caller-selected encoding budget",
        ));
    }
    let mut command = Command::new(ffmpeg_bin);
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        RunFailure::new(
            "encoder_unavailable",
            format!("failed to start FFmpeg video validation: {error}"),
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        RunFailure::new(
            "encoder_io",
            "FFmpeg validation progress pipe was unavailable",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        RunFailure::new(
            "encoder_io",
            "FFmpeg validation diagnostic pipe was unavailable",
        )
    })?;
    let progress = tokio::spawn(read_tail_bounded(stdout, MAX_ENCODER_PROGRESS_BYTES));
    let diagnostics = tokio::spawn(read_bounded(stderr, MAX_ENCODER_DIAGNOSTIC_BYTES));
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let outcome = tokio::select! {
        status = child.wait() => status.map_err(io_failure),
        () = cancel.cancelled() => {
            terminate_video_validation(&mut child, progress, diagnostics, "cancellation").await?;
            return Err(RunFailure::new("cancelled", "video validation was cancelled"));
        }
        () = &mut timeout => {
            terminate_video_validation(&mut child, progress, diagnostics, "timeout").await?;
            return Err(RunFailure::new("encoder_timeout", "FFmpeg video validation exceeded the caller-selected encoding budget"));
        }
    }?;
    let progress = progress
        .await
        .map_err(|error| RunFailure::new("encoder_io", error.to_string()))?
        .map_err(io_failure)?;
    let diagnostic_bytes = diagnostics
        .await
        .map_err(|error| RunFailure::new("encoder_io", error.to_string()))?
        .map_err(io_failure)?;
    let diagnostic = String::from_utf8_lossy(&diagnostic_bytes)
        .trim()
        .to_string();
    if !outcome.success() {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            if diagnostic.is_empty() {
                format!("FFmpeg could not decode the generated video: {outcome}")
            } else {
                format!("FFmpeg could not decode the generated video: {diagnostic}")
            },
        ));
    }
    validate_video_progress(&progress, expected_frames, frames_per_second)
}

async fn terminate_video_validation(
    child: &mut tokio::process::Child,
    progress: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    diagnostics: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    reason: &str,
) -> Result<(), RunFailure> {
    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return Err(RunFailure::new(
            "encoder_cleanup",
            format!("failed to terminate FFmpeg validation after {reason}: {error}"),
        ));
    }
    child.wait().await.map_err(|error| {
        RunFailure::new(
            "encoder_cleanup",
            format!("failed to reap FFmpeg validation after {reason}: {error}"),
        )
    })?;
    progress
        .await
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to join FFmpeg validation progress after {reason}: {error}"),
            )
        })?
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to drain FFmpeg validation progress after {reason}: {error}"),
            )
        })?;
    diagnostics
        .await
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to join FFmpeg validation diagnostics after {reason}: {error}"),
            )
        })?
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to drain FFmpeg validation diagnostics after {reason}: {error}"),
            )
        })?;
    Ok(())
}

fn validate_video_progress(
    progress: &[u8],
    expected_frames: u32,
    frames_per_second: u16,
) -> Result<(), RunFailure> {
    let progress = String::from_utf8_lossy(progress);
    let mut decoded_frames = None;
    let mut duration_us = None;
    let mut completed = false;
    for line in progress.lines() {
        if let Some(value) = line.strip_prefix("frame=") {
            decoded_frames = value.trim().parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("out_time_us=") {
            duration_us = value.trim().parse::<u64>().ok();
        } else if line.trim() == "progress=end" {
            completed = true;
        }
    }
    let decoded_frames = decoded_frames.ok_or_else(|| {
        RunFailure::new(
            "encoder_invalid_output",
            "FFmpeg validation did not report a decoded video frame count",
        )
    })?;
    let duration_us = duration_us.ok_or_else(|| {
        RunFailure::new(
            "encoder_invalid_output",
            "FFmpeg validation did not report a decoded video duration",
        )
    })?;
    if !completed {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            "FFmpeg validation did not reach the end of the video stream",
        ));
    }
    if decoded_frames != expected_frames {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            format!("generated video decoded {decoded_frames} frames; expected {expected_frames}"),
        ));
    }
    let frame_duration_us = 1_000_000_u64.div_ceil(u64::from(frames_per_second));
    let expected_duration_us =
        u64::from(expected_frames).saturating_mul(1_000_000) / u64::from(frames_per_second);
    if duration_us.abs_diff(expected_duration_us) > frame_duration_us {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            format!(
                "generated video duration was {duration_us} microseconds; expected approximately {expected_duration_us} for {expected_frames} frames at {frames_per_second} fps"
            ),
        ));
    }
    Ok(())
}

async fn terminate_encoder(
    child: &mut tokio::process::Child,
    diagnostics: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    reason: &str,
) -> Result<(), RunFailure> {
    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return Err(RunFailure::new(
            "encoder_cleanup",
            format!("failed to terminate FFmpeg after {reason}: {error}"),
        ));
    }
    child.wait().await.map_err(|error| {
        RunFailure::new(
            "encoder_cleanup",
            format!("failed to reap FFmpeg after {reason}: {error}"),
        )
    })?;
    diagnostics
        .await
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to join FFmpeg diagnostics after {reason}: {error}"),
            )
        })?
        .map_err(|error| {
            RunFailure::new(
                "encoder_cleanup",
                format!("failed to drain FFmpeg diagnostics after {reason}: {error}"),
            )
        })?;
    Ok(())
}

fn validate_mp4(path: &std::path::Path, max_bytes: u64) -> Result<(), RunFailure> {
    let mut file = File::open(path).map_err(io_failure)?;
    let length = file.metadata().map_err(io_failure)?.len();
    if length == 0 || length > max_bytes {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            "FFmpeg output is empty or exceeds the caller-selected video byte budget",
        ));
    }
    let mut offset = 0_u64;
    let mut first = true;
    let mut has_ftyp = false;
    let mut has_moov = false;
    let mut has_mdat = false;
    while offset < length {
        if length - offset < 8 {
            return Err(RunFailure::new(
                "encoder_invalid_output",
                "FFmpeg output contains a truncated MP4 atom header",
            ));
        }
        file.seek(SeekFrom::Start(offset)).map_err(io_failure)?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header).map_err(io_failure)?;
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let kind: [u8; 4] = header[4..].try_into().expect("four bytes");
        let (atom_size, header_size) = if size32 == 1 {
            if length - offset < 16 {
                return Err(RunFailure::new(
                    "encoder_invalid_output",
                    "FFmpeg output contains a truncated extended MP4 atom header",
                ));
            }
            let mut extended = [0_u8; 8];
            file.read_exact(&mut extended).map_err(io_failure)?;
            (u64::from_be_bytes(extended), 16_u64)
        } else if size32 == 0 {
            (length - offset, 8_u64)
        } else {
            (u64::from(size32), 8_u64)
        };
        if atom_size < header_size || atom_size > length - offset {
            return Err(RunFailure::new(
                "encoder_invalid_output",
                "FFmpeg output contains an invalid or truncated MP4 atom",
            ));
        }
        if first && kind != *b"ftyp" {
            return Err(RunFailure::new(
                "encoder_invalid_output",
                "FFmpeg output does not begin with an MP4 file-type atom",
            ));
        }
        first = false;
        has_ftyp |= kind == *b"ftyp";
        has_moov |= kind == *b"moov";
        has_mdat |= kind == *b"mdat";
        offset = offset.checked_add(atom_size).ok_or_else(|| {
            RunFailure::new("encoder_invalid_output", "MP4 atom offsets overflowed")
        })?;
    }
    if !has_ftyp || !has_moov || !has_mdat {
        return Err(RunFailure::new(
            "encoder_invalid_output",
            "FFmpeg output is missing required ftyp, moov, or mdat atoms",
        ));
    }
    Ok(())
}

async fn read_bounded(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

async fn read_tail_bounded(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut retained = VecDeque::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained.into());
        }
        retained.extend(&buffer[..read]);
        if retained.len() > limit {
            retained.drain(..retained.len() - limit);
        }
    }
}

fn validate_submit(
    params: RenderJobSubmitParams,
) -> Result<(JobSpec, RenderJobKind, u32), ToolError> {
    if !params.source_blend.ends_with(".blend") {
        return Err(ToolError::Validation(
            "source_blend must end in .blend".to_string(),
        ));
    }
    if params.width == 0 || params.width > 8192 || params.height == 0 || params.height > 8192 {
        return Err(ToolError::Validation(
            "width and height must be integers between 1 and 8192".to_string(),
        ));
    }
    if !(1..=MAX_BLENDER_ARTIFACT_BYTES).contains(&params.max_source_bytes) {
        return Err(ToolError::Validation(format!(
            "max_source_bytes must be between 1 and {MAX_BLENDER_ARTIFACT_BYTES}"
        )));
    }
    validate_duration(params.frame_timeout_seconds, "frame_timeout_seconds")?;
    validate_duration(params.encode_timeout_seconds, "encode_timeout_seconds")?;
    validate_duration(
        params.auto_frame_sequence_timeout_seconds,
        "auto_frame_sequence_timeout_seconds",
    )?;
    if let Some(presentation) = &params.presentation {
        validate_product_presentation(presentation, None)?;
        if u64::from(params.width) * u64::from(params.height) > MAX_PRODUCT_RENDER_PIXELS {
            return Err(ToolError::Validation(format!(
                "product render exceeds the {MAX_PRODUCT_RENDER_PIXELS}-pixel output limit; reduce width or height"
            )));
        }
    }
    if params.auto_frame_sequence
        && (params.kind != RenderJobKind::Animation || params.presentation.is_none())
    {
        return Err(ToolError::Validation(
            "auto_frame_sequence requires an animation job with presentation".to_string(),
        ));
    }
    if !(-1_048_574..=1_048_574).contains(&params.frame_start)
        || !(-1_048_574..=1_048_574).contains(&params.frame_end)
    {
        return Err(ToolError::Validation(
            "frame_start and frame_end must be between -1048574 and 1048574".to_string(),
        ));
    }
    if params.frame_step == 0 {
        return Err(ToolError::Validation(
            "frame_step must be positive".to_string(),
        ));
    }
    match (params.engine, params.samples) {
        (RenderJobEngine::Eevee, Some(_)) => {
            return Err(ToolError::Validation(
                "samples is only valid for CYCLES jobs".to_string(),
            ));
        }
        (RenderJobEngine::Cycles, Some(samples)) if !(1..=4096).contains(&samples) => {
            return Err(ToolError::Validation(
                "samples must be between 1 and 4096".to_string(),
            ));
        }
        _ => {}
    }
    if params.max_frame_sequence_bytes == 0
        || params.max_video_bytes == 0
        || params.max_video_bytes > i64::MAX as u64
    {
        return Err(ToolError::Validation(
            "frame and video byte budgets must be positive and runtime-representable".to_string(),
        ));
    }
    if !(1..=240).contains(&params.frames_per_second) {
        return Err(ToolError::Validation(
            "frames_per_second must be between 1 and 240".to_string(),
        ));
    }
    let total_frames = match params.kind {
        RenderJobKind::Still => 1,
        RenderJobKind::Turntable => {
            if u64::from(params.width) * u64::from(params.height) > MAX_REVIEW_SOURCE_PIXELS {
                return Err(ToolError::Validation(format!(
                    "turntable frame dimensions must contain at most {MAX_REVIEW_SOURCE_PIXELS} pixels"
                )));
            }
            if params.turntable_frames == 0 {
                return Err(ToolError::Validation(
                    "turntable_frames must be positive".to_string(),
                ));
            }
            if !params.turntable_elevation_degrees.is_finite()
                || !(-89.0..=89.0).contains(&params.turntable_elevation_degrees)
            {
                return Err(ToolError::Validation(
                    "turntable_elevation_degrees must be finite from -89 through 89".to_string(),
                ));
            }
            if params.presentation.as_ref().is_some_and(|presentation| {
                presentation.profile.ground_material().is_some()
                    && params.turntable_elevation_degrees < 0.0
            }) {
                return Err(ToolError::Validation(
                    "grounded studio presentation cannot render a below-ground turntable"
                        .to_string(),
                ));
            }
            params.turntable_frames
        }
        RenderJobKind::Animation => {
            if params.frame_start > params.frame_end {
                return Err(ToolError::Validation(
                    "frame_start must not exceed frame_end".to_string(),
                ));
            }
            let span = i64::from(params.frame_end) - i64::from(params.frame_start);
            u32::try_from(span / i64::from(params.frame_step) + 1).map_err(|_| {
                ToolError::Validation("animation frame range is too large".to_string())
            })?
        }
        RenderJobKind::MechanicalRotation => {
            if params.frame_start >= params.frame_end {
                return Err(ToolError::Validation(
                    "frame_start must be less than frame_end for mechanical_rotation jobs"
                        .to_string(),
                ));
            }
            let span = i64::from(params.frame_end) - i64::from(params.frame_start);
            if span % i64::from(params.frame_step) != 0 {
                return Err(ToolError::Validation(
                    "frame_step must include frame_end for mechanical_rotation jobs".to_string(),
                ));
            }
            u32::try_from(span / i64::from(params.frame_step) + 1).map_err(|_| {
                ToolError::Validation("mechanical rotation frame range is too large".to_string())
            })?
        }
    };
    let mechanical_rotation = match (params.kind, params.mechanical_rotation) {
        (RenderJobKind::MechanicalRotation, Some(spec)) => {
            validate_mechanical_rotation(&spec)?;
            Some(spec)
        }
        (RenderJobKind::MechanicalRotation, None) => {
            return Err(ToolError::Validation(
                "mechanical_rotation is required for mechanical_rotation jobs".to_string(),
            ));
        }
        (_, Some(_)) => {
            return Err(ToolError::Validation(
                "mechanical_rotation is only valid for mechanical_rotation jobs".to_string(),
            ));
        }
        (_, None) => None,
    };
    let kind = params.kind;
    Ok((
        JobSpec {
            source_blend: params.source_blend,
            max_source_bytes: params.max_source_bytes,
            width: params.width,
            height: params.height,
            engine: params.engine,
            samples: params
                .samples
                .or((params.engine == RenderJobEngine::Cycles).then_some(128)),
            frame_timeout_seconds: params.frame_timeout_seconds,
            turntable_frames: params.turntable_frames,
            turntable_elevation_degrees: params.turntable_elevation_degrees,
            turntable_clockwise: params.turntable_clockwise,
            frame_start: params.frame_start,
            frame_end: params.frame_end,
            frame_step: params.frame_step,
            frames_per_second: params.frames_per_second,
            max_frame_sequence_bytes: params.max_frame_sequence_bytes,
            max_video_bytes: params.max_video_bytes,
            encode_timeout_seconds: params.encode_timeout_seconds,
            mechanical_rotation,
            presentation: params.presentation,
            auto_frame_sequence: params.auto_frame_sequence,
            auto_frame_sequence_timeout_seconds: params.auto_frame_sequence_timeout_seconds,
        },
        kind,
        total_frames,
    ))
}

fn validate_mechanical_rotation(spec: &MechanicalRotationSpec) -> Result<(), ToolError> {
    fn validate_names(names: &[String], field: &str) -> Result<HashSet<String>, ToolError> {
        if names.is_empty() || names.len() > 1000 {
            return Err(ToolError::Validation(format!(
                "mechanical_rotation.{field} must contain between 1 and 1000 object names"
            )));
        }
        if names.iter().any(|name| name.is_empty() || name.len() > 255) {
            return Err(ToolError::Validation(format!(
                "mechanical_rotation.{field} must contain non-empty object names of at most 255 UTF-8 bytes"
            )));
        }
        let unique = names.iter().cloned().collect::<HashSet<_>>();
        if unique.len() != names.len() {
            return Err(ToolError::Validation(format!(
                "mechanical_rotation.{field} must not contain duplicate object names"
            )));
        }
        Ok(unique)
    }

    let fixed = validate_names(&spec.fixed_objects, "fixed_objects")?;
    let moving = validate_names(&spec.moving_objects, "moving_objects")?;
    if !fixed.is_disjoint(&moving) {
        return Err(ToolError::Validation(
            "mechanical_rotation fixed_objects and moving_objects must be disjoint".to_string(),
        ));
    }
    if spec
        .pivot_mm
        .iter()
        .chain(spec.axis.iter())
        .any(|component| !component.is_finite())
    {
        return Err(ToolError::Validation(
            "mechanical_rotation pivot_mm and axis must contain finite coordinates".to_string(),
        ));
    }
    let axis_scale = spec
        .axis
        .iter()
        .map(|component| component.abs())
        .fold(0.0_f64, f64::max);
    if axis_scale == 0.0 {
        return Err(ToolError::Validation(
            "mechanical_rotation axis must be non-zero".to_string(),
        ));
    }
    if !spec.angle_degrees.is_finite() || spec.angle_degrees <= 0.0 {
        return Err(ToolError::Validation(
            "mechanical_rotation angle_degrees must be finite and positive".to_string(),
        ));
    }
    if !spec.target_clearance_mm.is_finite() || spec.target_clearance_mm < 0.0 {
        return Err(ToolError::Validation(
            "mechanical_rotation target_clearance_mm must be finite and non-negative".to_string(),
        ));
    }
    if !(1..=MAX_BLENDER_ARTIFACT_BYTES).contains(&spec.max_analysis_mesh_bytes) {
        return Err(ToolError::Validation(format!(
            "mechanical_rotation max_analysis_mesh_bytes must be between 1 and {MAX_BLENDER_ARTIFACT_BYTES}"
        )));
    }
    Ok(())
}

fn validate_duration(value: f64, name: &str) -> Result<(), ToolError> {
    if value <= 0.0 || Duration::try_from_secs_f64(value).is_err() {
        Err(ToolError::Validation(format!(
            "{name} must be a positive runtime-representable number"
        )))
    } else {
        Ok(())
    }
}

fn validate_presentation_bounds(value: Option<&Value>) -> Result<PresentationBounds, RunFailure> {
    let invalid = || {
        RunFailure::new(
            "blender_protocol",
            "Blender presentation bounds were malformed",
        )
    };
    let bounds: PresentationBounds =
        serde_json::from_value(value.cloned().ok_or_else(invalid)?).map_err(|_| invalid())?;
    if !presentation_bounds_are_valid(&bounds) {
        return Err(invalid());
    }
    Ok(bounds)
}

fn presentation_bounds_are_valid(bounds: &PresentationBounds) -> bool {
    if bounds.coordinate_space != "world"
        || bounds.unit != "blender_unit"
        || !bounds.diagonal.is_finite()
        || bounds.diagonal < 0.1
    {
        return false;
    }
    for axis in 0..3 {
        let minimum = bounds.minimum[axis];
        let maximum = bounds.maximum[axis];
        let dimension = bounds.dimensions[axis];
        let center = bounds.center[axis];
        if !minimum.is_finite()
            || !maximum.is_finite()
            || !dimension.is_finite()
            || !center.is_finite()
            || maximum < minimum
            || !approximately_equal(dimension, maximum - minimum)
            || !approximately_equal(center, (minimum + maximum) * 0.5)
        {
            return false;
        }
    }
    let expected_diagonal = bounds
        .dimensions
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt()
        .max(0.1);
    if !approximately_equal(bounds.diagonal, expected_diagonal) {
        return false;
    }
    true
}

fn mechanical_presentation_bounds(
    report: &Value,
    pivot: [f64; 3],
) -> Result<PresentationBounds, RunFailure> {
    let malformed = || {
        RunFailure::new(
            "geometry_protocol",
            "mechanical analysis omitted bounds needed for presentation framing",
        )
    };
    let part_bounds = |name: &str| -> Result<([f64; 3], [f64; 3]), RunFailure> {
        let bounds = report
            .get(name)
            .and_then(|part| part.get("bounds"))
            .ok_or_else(malformed)?;
        let minimum: [f64; 3] =
            serde_json::from_value(bounds.get("minimum_mm").cloned().ok_or_else(malformed)?)
                .map_err(|_| malformed())?;
        let maximum: [f64; 3] =
            serde_json::from_value(bounds.get("maximum_mm").cloned().ok_or_else(malformed)?)
                .map_err(|_| malformed())?;
        if minimum
            .iter()
            .chain(maximum.iter())
            .any(|value| !value.is_finite())
            || (0..3).any(|axis| maximum[axis] < minimum[axis])
        {
            return Err(malformed());
        }
        Ok((minimum, maximum))
    };
    let (fixed_minimum, fixed_maximum) = part_bounds("fixed")?;
    let (moving_minimum, moving_maximum) = part_bounds("moving")?;
    let mut radius = 0.0_f64;
    for x in [moving_minimum[0], moving_maximum[0]] {
        for y in [moving_minimum[1], moving_maximum[1]] {
            for z in [moving_minimum[2], moving_maximum[2]] {
                radius = radius.max((x - pivot[0]).hypot(y - pivot[1]).hypot(z - pivot[2]));
            }
        }
    }
    if !radius.is_finite() {
        return Err(malformed());
    }
    let minimum = std::array::from_fn(|axis| fixed_minimum[axis].min(pivot[axis] - radius));
    let maximum = std::array::from_fn(|axis| fixed_maximum[axis].max(pivot[axis] + radius));
    let dimensions = std::array::from_fn(|axis| maximum[axis] - minimum[axis]);
    let center = std::array::from_fn(|axis| (minimum[axis] + maximum[axis]) * 0.5);
    let diagonal = dimensions[0]
        .hypot(dimensions[1])
        .hypot(dimensions[2])
        .max(0.1);
    Ok(PresentationBounds {
        minimum,
        maximum,
        dimensions,
        center,
        diagonal,
        coordinate_space: "world".to_string(),
        unit: "blender_unit".to_string(),
    })
}

fn approximately_equal(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * left.abs().max(right.abs()).max(1.0)
}

fn runtime_duration(value: f64, name: &str) -> Result<Duration, RunFailure> {
    if value <= 0.0 {
        return Err(RunFailure::new(
            "invalid_job_metadata",
            format!("persisted {name} is not positive and runtime-representable"),
        ));
    }
    Duration::try_from_secs_f64(value).map_err(|_| {
        RunFailure::new(
            "invalid_job_metadata",
            format!("persisted {name} is not positive and runtime-representable"),
        )
    })
}

fn source_frame(job: &JobRecord, index: u32) -> i64 {
    match job.kind {
        RenderJobKind::Animation | RenderJobKind::MechanicalRotation => {
            let offset = i64::from(index) * i64::from(job.spec.frame_step);
            i64::from(job.spec.frame_start) + offset
        }
        _ => i64::from(index) + 1,
    }
}

fn turntable_direction(job: &JobRecord, index: u32) -> [f64; 3] {
    let fraction = f64::from(index) / f64::from(job.progress.total_frames);
    let base_angle = fraction * TAU;
    let angle = if job.spec.turntable_clockwise {
        -base_angle
    } else {
        base_angle
    };
    let elevation = job.spec.turntable_elevation_degrees.to_radians();
    [
        angle.sin() * elevation.cos(),
        -angle.cos() * elevation.cos(),
        elevation.sin(),
    ]
}

fn frame_path(job_id: &str, index: u32) -> String {
    format!("{JOB_ROOT}/{job_id}/frames/frame-{:06}.png", index + 1)
}

fn mechanical_fixed_path(job_id: &str, generation: u32) -> String {
    format!("{JOB_ROOT}/{job_id}/analysis/attempt-{generation}/fixed.stl")
}

fn mechanical_moving_path(job_id: &str, generation: u32) -> String {
    format!("{JOB_ROOT}/{job_id}/analysis/attempt-{generation}/moving.stl")
}

fn job_metadata_path(job_id: &str) -> String {
    format!("{JOB_ROOT}/{job_id}/job.json")
}

fn object_params(value: Value) -> Result<Params, RunFailure> {
    value.as_object().cloned().ok_or_else(|| {
        RunFailure::new(
            "serialization",
            "Blender command parameters were not an object",
        )
    })
}

fn evict_oldest_terminal(state: &mut RegistryState) -> Vec<String> {
    let mut evicted = Vec::new();
    while state.order.len() >= MAX_JOB_HISTORY {
        let Some(position) = state.order.iter().position(|job_id| {
            state
                .jobs
                .get(job_id)
                .is_some_and(|job| job.state.terminal())
        }) else {
            return evicted;
        };
        if let Some(job_id) = state.order.remove(position) {
            state.jobs.remove(&job_id);
            evicted.push(job_id);
        }
    }
    evicted
}

fn recover_state(
    workspace: &Workspace,
    queue_depth: usize,
) -> (RegistryState, Vec<String>, RecoveryIntegrity) {
    let mut state = RegistryState {
        jobs: HashMap::new(),
        order: VecDeque::new(),
    };
    let mut integrity = RecoveryIntegrity::default();
    let index = match workspace.read_artifact(INDEX_PATH) {
        Ok((_meta, bytes)) => serde_json::from_slice::<JobIndex>(&bytes),
        Err(WsError::NotFound(_)) => {
            match workspace.list_artifacts(JOB_ROOT, 1) {
                Ok(artifacts) if !artifacts.is_empty() => integrity
                    .issues
                    .push("durable render job index is missing while job artifacts remain".to_string()),
                Ok(_) | Err(WsError::NotFound(_)) => {}
                Err(error) => integrity.issues.push(format!(
                    "durable render job index is missing and retained jobs cannot be inspected ({})",
                    error.code()
                )),
            }
            return (state, Vec::new(), integrity);
        }
        Err(WsError::Unconfined) => {
            return (state, Vec::new(), integrity);
        }
        Err(error) => {
            tracing::error!(%error, "failed to read durable render job index");
            integrity.issues.push(format!(
                "durable render job index is unreadable ({})",
                error.code()
            ));
            return (state, Vec::new(), integrity);
        }
    };
    let Ok(index) = index else {
        tracing::error!("durable render job index is invalid JSON");
        integrity
            .issues
            .push("durable render job index is invalid JSON".to_string());
        return (state, Vec::new(), integrity);
    };
    if !matches!(index.schema_version, 2 | JOB_SCHEMA_VERSION) {
        tracing::error!(
            schema_version = index.schema_version,
            "durable render job index schema is unsupported"
        );
        integrity
            .issues
            .push("durable render job index schema is unsupported".to_string());
        return (state, Vec::new(), integrity);
    }
    if index.job_ids.len() > MAX_JOB_HISTORY {
        tracing::error!(
            entries = index.job_ids.len(),
            "durable render job index exceeds retained history"
        );
        integrity
            .issues
            .push("durable render job index exceeds retained history".to_string());
    }
    for job_id in index.job_ids.into_iter().take(MAX_JOB_HISTORY) {
        if !valid_job_id(&job_id) || state.jobs.contains_key(&job_id) {
            tracing::error!(
                job_id,
                "durable render job index contains an invalid identity"
            );
            integrity.issues.push(
                "durable render job index contains an invalid or duplicate identity".to_string(),
            );
            continue;
        }
        let bytes = match workspace.read_artifact(&job_metadata_path(&job_id)) {
            Ok((_meta, bytes)) => bytes,
            Err(error) => {
                tracing::error!(job_id, %error, "durable render job metadata is unavailable");
                integrity.issues.push(format!(
                    "durable render job {job_id} metadata is unavailable ({})",
                    error.code()
                ));
                continue;
            }
        };
        let Ok(mut job) = serde_json::from_slice::<JobRecord>(&bytes) else {
            tracing::error!(job_id, "durable render job metadata is invalid JSON");
            integrity.issues.push(format!(
                "durable render job {job_id} metadata is invalid JSON"
            ));
            continue;
        };
        if job.schema_version == 2 && matches!(job.execution, JobExecution::Unspecified) {
            job.execution = JobExecution::LegacySession;
        }
        if !matches!(job.schema_version, 2 | JOB_SCHEMA_VERSION)
            || job.job_id != job_id
            || validate_recovered_job(&job).is_err()
        {
            tracing::error!(job_id, "durable render job metadata identity is invalid");
            integrity.issues.push(format!(
                "durable render job {job_id} metadata invariants are invalid"
            ));
            continue;
        }
        let requires_session_restore =
            job.session_checkpoint_captured && !job.session_checkpoint_restored;
        if job.state == JobState::Running || requires_session_restore {
            job.progress.current_frame = None;
            job.recovery_count = job.recovery_count.saturating_add(1);
            job.updated_unix_ms = unix_ms();
            if job.cancellation_requested
                && (!job.session_checkpoint_captured || job.session_checkpoint_restored)
            {
                job.state = JobState::Cancelled;
                job.progress.phase = "cancelled".to_string();
            } else {
                job.state = JobState::Queued;
                job.progress.phase = if requires_session_restore {
                    "recovered_session_restore"
                } else {
                    "recovered"
                }
                .to_string();
            }
            persist_recovered_job(workspace, &job);
        }
        state.order.push_back(job_id.clone());
        state.jobs.insert(job_id, job);
    }
    let mut recovered = state
        .order
        .iter()
        .filter(|job_id| {
            state
                .jobs
                .get(*job_id)
                .is_some_and(|job| job.state == JobState::Queued)
        })
        .cloned()
        .collect::<Vec<_>>();
    recovered.sort_by_key(|job_id| {
        !state
            .jobs
            .get(job_id)
            .is_some_and(|job| job.session_checkpoint_captured && !job.session_checkpoint_restored)
    });
    let required_restore_count = recovered
        .iter()
        .filter(|job_id| {
            state.jobs.get(*job_id).is_some_and(|job| {
                job.session_checkpoint_captured && !job.session_checkpoint_restored
            })
        })
        .count();
    let recovery_capacity = queue_depth.max(required_restore_count);
    if recovered.len() > recovery_capacity {
        for job_id in recovered.drain(recovery_capacity..) {
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.state = JobState::Failed;
                job.progress.phase = "failed".to_string();
                job.failure = Some(JobFailure {
                    code: "queue_capacity_reduced".to_string(),
                    message:
                        "job could not be recovered because the configured queue depth was reduced"
                            .to_string(),
                });
                job.updated_unix_ms = unix_ms();
                persist_recovered_job(workspace, job);
            }
        }
    }
    (state, recovered, integrity)
}

fn valid_job_id(job_id: &str) -> bool {
    job_id.len() == 32 && job_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn persist_recovered_job(workspace: &Workspace, job: &JobRecord) {
    let bytes = match serde_json::to_vec_pretty(job) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(job_id = job.job_id, %error, "failed to serialize recovered render job");
            return;
        }
    };
    if let Err(error) =
        workspace.write_reserved_artifact(&job_metadata_path(&job.job_id), &bytes, true)
    {
        tracing::error!(job_id = job.job_id, %error, "failed to persist recovered render job");
    }
}

fn validate_recovered_job(job: &JobRecord) -> Result<(), ToolError> {
    if matches!(job.execution, JobExecution::Unspecified) {
        return Err(ToolError::Validation(
            "persisted job execution mode is missing".into(),
        ));
    }
    if let Some(source) = &job.source_snapshot
        && (source.sha256.len() != 64
            || !source
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || source.size_bytes > job.spec.max_source_bytes)
    {
        return Err(ToolError::Validation(
            "persisted checkpoint identity is invalid".into(),
        ));
    }
    if job.execution.isolated()
        && (job.schema_version < JOB_SCHEMA_VERSION
            || job.source_snapshot.is_none()
            || job.session_checkpoint_captured
            || job.session_checkpoint_restored
            || job.pending_blender_outcome.is_some()
            || (job.execution.finished()
                && !job.cancellation_requested
                && job.failure.is_none()
                && job.progress.completed_frames != job.progress.total_frames))
    {
        return Err(ToolError::Validation(
            "persisted isolated worker state is invalid".into(),
        ));
    }
    let params = RenderJobSubmitParams {
        source_blend: job.spec.source_blend.clone(),
        max_source_bytes: job.spec.max_source_bytes,
        kind: job.kind,
        mechanical_rotation: job.spec.mechanical_rotation.clone(),
        width: job.spec.width,
        height: job.spec.height,
        engine: job.spec.engine,
        samples: job.spec.samples,
        frame_timeout_seconds: job.spec.frame_timeout_seconds,
        turntable_frames: job.spec.turntable_frames,
        turntable_elevation_degrees: job.spec.turntable_elevation_degrees,
        turntable_clockwise: job.spec.turntable_clockwise,
        frame_start: job.spec.frame_start,
        frame_end: job.spec.frame_end,
        frame_step: job.spec.frame_step,
        frames_per_second: job.spec.frames_per_second,
        max_frame_sequence_bytes: job.spec.max_frame_sequence_bytes,
        max_video_bytes: job.spec.max_video_bytes,
        encode_timeout_seconds: job.spec.encode_timeout_seconds,
        presentation: job.spec.presentation.clone(),
        auto_frame_sequence: job.spec.auto_frame_sequence,
        auto_frame_sequence_timeout_seconds: job.spec.auto_frame_sequence_timeout_seconds,
    };
    let (_spec, _kind, total_frames) = validate_submit(params)?;
    if job.source_artifact != format!("{JOB_ROOT}/{}/source.blend", job.job_id)
        || job.session_checkpoint_artifact
            != format!("{JOB_ROOT}/{}/pre-job-session.blend", job.job_id)
        || (job.session_checkpoint_restored && !job.session_checkpoint_captured)
        || (job.pending_blender_outcome.is_some()
            && (!job.session_checkpoint_captured || job.session_checkpoint_restored))
        || (matches!(
            job.pending_blender_outcome.as_ref(),
            Some(PendingBlenderOutcome::Completed)
        ) && job.progress.completed_frames != total_frames)
        || (matches!(
            job.pending_blender_outcome.as_ref(),
            Some(PendingBlenderOutcome::Cancelled)
        ) && !job.cancellation_requested)
        || job.progress.total_frames != total_frames
        || job.progress.completed_frames > total_frames
        || job.progress.frame_bytes > job.spec.max_frame_sequence_bytes
        || (job.session_checkpoint_restored
            && !job.cancellation_requested
            && job.failure.is_none()
            && job.progress.completed_frames != total_frames)
    {
        return Err(ToolError::Validation(
            "persisted render job invariants are invalid".to_string(),
        ));
    }
    if let Some(video) = &job.video_artifact
        && (video.path != format!("{JOB_ROOT}/{}/video.mp4", job.job_id)
            || video.size_bytes > job.spec.max_video_bytes
            || video.media_type != "video/mp4")
    {
        return Err(ToolError::Validation(
            "persisted render job video metadata is invalid".to_string(),
        ));
    }
    validate_recovered_mechanical_analysis(job)?;
    validate_recovered_presentation(job)?;
    Ok(())
}

fn validate_recovered_presentation(job: &JobRecord) -> Result<(), ToolError> {
    let invalid = || {
        ToolError::Validation("persisted render job presentation metadata is invalid".to_string())
    };
    if job
        .presentation_bounds
        .as_ref()
        .is_some_and(|bounds| !presentation_bounds_are_valid(bounds))
    {
        return Err(invalid());
    }
    let presentation_requested = job.spec.presentation.is_some();
    if job.progress.completed_frames > 0
        && presentation_requested
        && job.presentation_bounds.is_none()
    {
        return Err(invalid());
    }
    if !presentation_requested && job.presentation_bounds.is_some() {
        return Err(invalid());
    }
    Ok(())
}

fn validate_recovered_mechanical_analysis(job: &JobRecord) -> Result<(), ToolError> {
    let invalid = || {
        ToolError::Validation(
            "persisted render job mechanical analysis metadata is invalid".to_string(),
        )
    };
    let Some(spec) = &job.spec.mechanical_rotation else {
        return if job.mechanical_analysis.is_none() {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    let Some(analysis) = &job.mechanical_analysis else {
        return if job.progress.completed_frames == 0
            && !matches!(
                job.pending_blender_outcome.as_ref(),
                Some(PendingBlenderOutcome::Completed)
            )
            && job.state != JobState::Succeeded
        {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    let authored_rotation =
        recovered_mechanical_rotation(&analysis.report, spec).ok_or_else(invalid)?;
    let report_certified = validate_mechanical_analysis_report(&analysis.report, authored_rotation)
        .map_err(|_| invalid())?;
    if analysis.generation > job.recovery_count
        || analysis.fixed_artifact.path != mechanical_fixed_path(&job.job_id, analysis.generation)
        || analysis.moving_artifact.path != mechanical_moving_path(&job.job_id, analysis.generation)
        // Older retained jobs used the certificate-list MIME suffix mapping.
        // Accept that metadata on recovery without changing their evidence.
        || !matches!(analysis.fixed_artifact.media_type.as_str(), "model/stl" | "application/vnd.ms-pki.stl")
        || !matches!(analysis.moving_artifact.media_type.as_str(), "model/stl" | "application/vnd.ms-pki.stl")
        || analysis.fixed_artifact.size_bytes == 0
        || analysis.moving_artifact.size_bytes == 0
        || analysis.fixed_artifact.size_bytes > spec.max_analysis_mesh_bytes
        || analysis.moving_artifact.size_bytes > spec.max_analysis_mesh_bytes
        || analysis.units != "millimetres"
        || report_certified != analysis.certified
        || ((job.progress.completed_frames > 0 || job.state == JobState::Succeeded)
            && !analysis.certified)
    {
        return Err(invalid());
    }
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn blender_failure(error: printable_blender::BlenderError) -> RunFailure {
    let code = match &error {
        printable_blender::BlenderError::Timeout {
            phase: printable_blender::Phase::Lock,
        } => "blender_busy",
        printable_blender::BlenderError::Addon { message, .. }
            if {
                let lower = message.to_ascii_lowercase();
                lower.contains("out of memory")
                    || lower.contains("cuda_error_out_of_memory")
                    || lower.contains("optix out of memory")
            } =>
        {
            "gpu_out_of_memory"
        }
        _ => error.code(),
    };
    RunFailure::new(code, error.to_string())
}

fn workspace_failure(error: WsError) -> RunFailure {
    RunFailure::new(error.code(), error.to_string())
}

fn io_failure(error: std::io::Error) -> RunFailure {
    RunFailure::new("encoder_io", error.to_string())
}

fn tool_failure(error: ToolError) -> RunFailure {
    RunFailure::new(error.code(), error.to_string())
}

fn classify_encoder_failure(diagnostics: &str) -> &'static str {
    let lower = diagnostics.to_ascii_lowercase();
    if lower.contains("no space left") || lower.contains("file too large") {
        "encoder_output_limit"
    } else if lower.contains("unknown encoder") || lower.contains("encoder not found") {
        "encoder_unavailable"
    } else {
        "encoder_failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use printable_blender::{
        ClientOptions,
        fake_addon::{FakeAddon, ResponseSpec},
    };
    use std::future::Future as _;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;
    use std::task::Poll;

    fn submit_params(source_blend: &str, kind: &str) -> RenderJobSubmitParams {
        serde_json::from_value(json!({
            "source_blend": source_blend,
            "kind": kind,
            "width": 32,
            "height": 24,
            "frame_timeout_seconds": 5.0,
            "encode_timeout_seconds": 5.0,
        }))
        .expect("valid job params")
    }

    fn write_product_test_frame(
        root: &std::path::Path,
        path: &str,
        width: u32,
        height: u32,
    ) -> (u64, String) {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([32, 96, 160]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode product test frame");
        let bytes = bytes.into_inner();
        let destination = root.join(path);
        std::fs::create_dir_all(destination.parent().expect("frame parent"))
            .expect("create frame directory");
        std::fs::write(&destination, &bytes).expect("write product test frame");
        (
            u64::try_from(bytes.len()).expect("test PNG size"),
            crate::tools::sha256_hex(&bytes),
        )
    }

    fn mechanical_report(
        pivot_mm: [f64; 3],
        certified: bool,
        block_reason: Option<&str>,
        minimum_certified_clearance_mm: Option<f64>,
    ) -> Value {
        json!({
            "fixed": {
                "vertices": 8,
                "triangles": 12,
                "bounds": {
                    "minimum_mm": [0.0, 0.0, 0.0],
                    "maximum_mm": [1.0, 1.0, 1.0],
                    "dimensions_mm": [1.0, 1.0, 1.0]
                },
                "volume_mm3": 1.0
            },
            "moving": {
                "vertices": 8,
                "triangles": 12,
                "bounds": {
                    "minimum_mm": [2.0, 0.0, 0.0],
                    "maximum_mm": [3.0, 1.0, 1.0],
                    "dimensions_mm": [1.0, 1.0, 1.0]
                },
                "volume_mm3": 1.0
            },
            "static_analysis": {
                "relation": "separated",
                "clearance_mm": 1.0,
                "surface_gap_mm": 1.0,
                "closest_surface_points": {
                    "fixed_mm": [1.0, 0.0, 0.0],
                    "moving_mm": [2.0, 0.0, 0.0]
                },
                "interference_volume_mm3": 0.0,
                "fixed_interference_fraction": 0.0,
                "moving_interference_fraction": 0.0,
                "required_clearance_mm": 0.2,
                "meets_required_clearance": true
            },
            "rotation": {
                "pivot_mm": pivot_mm,
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.2,
                "can_rotate_full_angle": certified,
                "retained": matches!(block_reason, Some("contact" | "initial_interference")),
                "first_limit_interval_degrees": if certified { None } else { Some([45.0, 46.0]) },
                "block_reason": block_reason,
                "clearance_at_end_mm": if certified { Some(0.3) } else { None },
                "minimum_certified_clearance_mm": minimum_certified_clearance_mm
            }
        })
    }

    fn typed_mechanical_report() -> AssemblyReport {
        serde_json::from_value(mechanical_report([1.0, 2.0, 3.0], true, None, Some(0.25)))
            .expect("typed mechanical report")
    }

    fn test_job_record(job_id: &str, state: JobState) -> JobRecord {
        let (spec, kind, total_frames) =
            validate_submit(submit_params("scene.blend", "still")).expect("valid still job");
        JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: JobExecution::LegacySession,
            job_id: job_id.to_string(),
            kind,
            state,
            created_unix_ms: 1,
            updated_unix_ms: 2,
            source_artifact: format!("{JOB_ROOT}/{job_id}/source.blend"),
            source_snapshot: None,
            session_checkpoint_artifact: format!("{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            session_checkpoint_captured: false,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "test".to_string(),
                completed_frames: 0,
                total_frames,
                current_frame: None,
                frame_bytes: 0,
            },
            cancellation_requested: false,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: None,
            failure: None,
        }
    }

    fn job_product_presentation(params: &Value, camera: Value, bounds: &Value) -> Value {
        let presentation = &params["presentation"];
        let profile = presentation["profile"].as_str().expect("profile");
        let shading = presentation
            .get("surface_shading")
            .and_then(Value::as_str)
            .unwrap_or(if profile == "engineering" {
                "preserve"
            } else {
                "smooth_by_angle"
            });
        json!({
            "profile": profile,
            "camera": camera,
            "materials": {
                "overrides": presentation
                    .get("materials")
                    .cloned()
                    .unwrap_or_else(|| json!([]))
            },
            "shading": {
                "mode": shading,
                "angle_degrees": if shading == "smooth_by_angle" {
                    Some(30.0)
                } else {
                    None
                },
                "presentation_only": true
            },
            "framing": {"bounds": bounds}
        })
    }

    fn product_test_bounds() -> Value {
        json!({
            "minimum": [0.0, 0.0, 0.0],
            "maximum": [1.0, 1.0, 1.0],
            "dimensions": [1.0, 1.0, 1.0],
            "center": [0.5, 0.5, 0.5],
            "diagonal": 3.0_f64.sqrt(),
            "coordinate_space": "world",
            "unit": "blender_unit"
        })
    }

    #[test]
    fn presentation_bounds_response_requires_each_requested_timeline_field() {
        let response = json!({
            "bounds": product_test_bounds(),
            "frames_evaluated": 3,
            "frame_start": 2,
            "frame_end": 6,
            "frame_step": 2
        });
        validate_presentation_bounds_response(&response, 2, 6, 2, 3)
            .expect("exact bounds measurement response");
        for field in ["frames_evaluated", "frame_start", "frame_end", "frame_step"] {
            let mut malformed = response.clone();
            malformed[field] = json!(99);
            assert!(
                validate_presentation_bounds_response(&malformed, 2, 6, 2, 3).is_err(),
                "{field} must independently match the requested measurement"
            );
        }
    }

    #[test]
    fn durable_product_frame_rejects_presentation_drift() {
        let mut record = test_job_record("00112233445566778899aabbccddeeff", JobState::Running);
        record.spec.presentation = Some(
            serde_json::from_value(json!({
                "profile": "studio_neutral",
                "materials": [{
                    "objects": ["Body"],
                    "base_color_srgb": [0.7, 0.2, 0.1],
                    "metallic": 0.1,
                    "roughness": 0.4
                }],
                "surface_shading": "preserve"
            }))
            .expect("product presentation"),
        );
        let bounds = product_test_bounds();
        record.presentation_bounds =
            Some(serde_json::from_value(bounds.clone()).expect("presentation bounds"));
        let mut response = json!({
            "path": frame_path(&record.job_id, 0),
            "media_type": "image/png",
            "width": record.spec.width,
            "height": record.spec.height,
            "objects": ["Body"],
            "bounds": bounds,
            "source_state_verified": true,
            "cleanup_verified": true,
            "presentation": {
                "profile": "studio_neutral",
                "camera": {
                    "behavior": "bounds",
                    "azimuth_degrees": 45.0,
                    "elevation_degrees": 25.0
                },
                "materials": {
                    "overrides": [{
                        "objects": ["Body"],
                        "base_color_srgb": [0.7, 0.2, 0.1],
                        "metallic": 0.1,
                        "roughness": 0.4
                    }]
                },
                "shading": {
                    "mode": "preserve",
                    "angle_degrees": null,
                    "presentation_only": true
                },
                "framing": {"bounds": bounds}
            }
        });
        validate_job_product_response(&response, &record, 0)
            .expect("exact persisted presentation accepted");

        record
            .spec
            .presentation
            .as_mut()
            .expect("presentation")
            .exposure_stops = Some(1.25);
        record
            .spec
            .presentation
            .as_mut()
            .expect("presentation")
            .light_intensity_scale = Some(0.5);
        assert!(validate_job_product_response(&response, &record, 0).is_err());
        response["presentation"]["color_management"] = json!({"exposure": 1.25});
        response["presentation"]["light_intensity_scale"] = json!(0.5);
        validate_job_product_response(&response, &record, 0).expect("requested controls verified");
        response["presentation"]["light_intensity_scale"] = json!(1.0);
        assert!(validate_job_product_response(&response, &record, 0).is_err());
        response["presentation"]["light_intensity_scale"] = json!(0.5);
        response["presentation"]["color_management"]["exposure"] = json!(0.0);
        assert!(validate_job_product_response(&response, &record, 0).is_err());
        response["presentation"]["color_management"]["exposure"] = json!(1.25);

        response["presentation"]["materials"]["overrides"][0]["roughness"] = json!(0.6);
        assert!(validate_job_product_response(&response, &record, 0).is_err());
        response["presentation"]["materials"]["overrides"][0]["roughness"] = json!(0.4);
        response["presentation"]["shading"]["mode"] = json!("smooth_by_angle");
        response["presentation"]["shading"]["angle_degrees"] = json!(30.0);
        assert!(validate_job_product_response(&response, &record, 0).is_err());

        response["presentation"]["shading"]["mode"] = json!("preserve");
        response["presentation"]["shading"]["angle_degrees"] = Value::Null;
        record.kind = RenderJobKind::Animation;
        record.presentation_bounds =
            Some(serde_json::from_value(bounds.clone()).expect("presentation bounds"));
        response["frame"] = json!(source_frame(&record, 0));
        response["presentation"]["camera"] = json!({
            "behavior": "preserve",
            "position": [2.0, -3.0, 2.0]
        });
        response["presentation"]["ground"] = json!({
            "enabled": true,
            "z": -0.01
        });
        validate_job_product_response(&response, &record, 0)
            .expect("preserved camera above the studio ground accepted");
        response["presentation"]["camera"]["position"][2] = json!(-0.01);
        assert!(
            validate_job_product_response(&response, &record, 0).is_err(),
            "preserved camera on an opaque studio ground must fail before progress"
        );
        response["presentation"]["camera"]["position"][2] = json!(-0.02);
        assert!(
            validate_job_product_response(&response, &record, 0).is_err(),
            "preserved camera below an opaque studio ground must fail before progress"
        );

        record
            .spec
            .presentation
            .as_mut()
            .expect("presentation")
            .profile = crate::tools::ProductPresentationProfile::Engineering;
        response["presentation"]["profile"] = json!("engineering");
        response["presentation"]["camera"]["position"][2] = json!(2.0);
        response["presentation"]["ground"] = json!({"enabled": false});
        validate_job_product_response(&response, &record, 0)
            .expect("engineering preserve camera requires no presentation ground");
        response["presentation"]["ground"]["unexpected"] = json!(true);
        assert!(
            validate_job_product_response(&response, &record, 0).is_err(),
            "engineering ground attestation must be exact"
        );
    }

    #[test]
    fn rendered_size_requires_positive_bytes_for_each_response_shape() {
        assert_eq!(
            rendered_size(&json!({"size_bytes": 7}), RenderJobKind::Still)
                .expect("positive still size"),
            7
        );
        assert_eq!(
            rendered_size(
                &json!({"views": [{"size_bytes": 9}]}),
                RenderJobKind::Turntable
            )
            .expect("positive turntable size"),
            9
        );
        for (response, kind) in [
            (json!({"size_bytes": 0}), RenderJobKind::Animation),
            (
                json!({"views": [{"size_bytes": 0}]}),
                RenderJobKind::Turntable,
            ),
        ] {
            assert!(rendered_size(&response, kind).is_err());
        }
    }

    fn test_registry_inner(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        job_id: String,
        record: JobRecord,
    ) -> Arc<JobRegistryInner> {
        test_registry_inner_with_encoder(
            workspace,
            blender,
            job_id,
            record,
            PathBuf::from("unused-ffmpeg"),
        )
    }

    fn test_registry_inner_with_encoder(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        job_id: String,
        record: JobRecord,
        ffmpeg_bin: PathBuf,
    ) -> Arc<JobRegistryInner> {
        Arc::new(JobRegistryInner {
            workspace,
            blender,
            render_worker: None,
            isolation: RenderIsolation::Legacy,
            ffmpeg_bin,
            geometry_worker_bin: None,
            geometry_worker_memory_bytes: DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
            queue_depth: 1,
            state: Mutex::new(RegistryState {
                jobs: HashMap::from([(job_id.clone(), record)]),
                order: VecDeque::from([job_id]),
            }),
            admission: Arc::new(Mutex::new(())),
            persistence: Arc::new(Mutex::new(())),
            cancellations: Mutex::new(HashMap::new()),
            queue: Arc::new(JobQueue::new([])),
            recovery_integrity: RecoveryIntegrity::default(),
            encoder_health: OnceCell::new(),
            worker_verified: AtomicBool::new(false),
        })
    }

    async fn occupy_only_blocking_thread()
    -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let started = Arc::new(AtomicBool::new(false));
        let blocker_started = Arc::clone(&started);
        let (release, released) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            blocker_started.store(true, Ordering::SeqCst);
            released.recv().expect("release blocking pool");
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking pool is occupied");
        (release, blocker)
    }

    async fn wait_for_state(registry: &JobRegistry, job_id: &str, expected: JobState) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let value = registry.status_value(job_id).await.expect("job status");
                if value["state"] == serde_json::to_value(expected).expect("state JSON") {
                    return value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("job reaches expected state")
    }

    #[tokio::test]
    async fn isolated_render_and_cancellation_leave_live_editing_available() {
        for cancel_job in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            workspace
                .write_artifact("scene.blend", b"original checkpoint", false)
                .unwrap();
            let live = FakeAddon::spawn(|command, _| {
                assert_eq!(command, "rename_object");
                ResponseSpec::Success {
                    result: json!({"name": "Live edit"}),
                    addon_version: None,
                }
            })
            .await;
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let render_started = Arc::clone(&started);
            let render_release = Arc::clone(&release);
            let worker = FakeAddon::spawn(move |command, params| match command.as_str() {
                "bridge_status" => ResponseSpec::Success {
                    result: json!({"role": "render_worker", "background": true}),
                    addon_version: None,
                },
                "job_restore_checkpoint" => {
                    assert!(params["path"].as_str().unwrap().ends_with("/source.blend"));
                    assert_eq!(
                        params["expected_sha256"],
                        crate::tools::sha256_hex(b"original checkpoint")
                    );
                    ResponseSpec::Success {
                        result: json!({"frame_current": 1}),
                        addon_version: None,
                    }
                }
                "job_render_still" => ResponseSpec::SuccessWhenReleased {
                    result: json!({"size_bytes": 1}),
                    addon_version: None,
                    started: Arc::clone(&render_started),
                    release: Arc::clone(&render_release),
                },
                other => panic!("unexpected worker command {other}"),
            })
            .await;
            let live_client = Arc::new(BlenderClient::new(
                live.host(),
                live.port(),
                ClientOptions::default(),
            ));
            let registry = JobRegistry::new_with_render_worker(
                Arc::clone(&workspace),
                Arc::clone(&live_client),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(Arc::new(BlenderClient::new(
                    worker.host(),
                    worker.port(),
                    ClientOptions::default(),
                ))),
            );
            let submitted = registry
                .submit(submit_params("scene.blend", "still"))
                .await
                .unwrap();
            let job_id = submitted["job_id"].as_str().unwrap().to_owned();
            tokio::time::timeout(Duration::from_secs(2), started.notified())
                .await
                .unwrap();
            let edited = tokio::time::timeout(
                Duration::from_secs(1),
                live_client.send_value(
                    "rename_object",
                    Params::new(),
                    live_client.default_deadline(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(edited["name"], "Live edit");
            workspace
                .write_artifact("scene.blend", b"later live checkpoint", true)
                .unwrap();
            let (_, frozen) = workspace
                .read_artifact(submitted["source_artifact"].as_str().unwrap())
                .unwrap();
            assert_eq!(frozen, b"original checkpoint");
            assert_eq!(
                submitted["source_snapshot"]["sha256"],
                crate::tools::sha256_hex(&frozen)
            );
            assert_eq!(submitted["source_snapshot"]["size_bytes"], frozen.len());
            if cancel_job {
                registry
                    .cancel(RenderJobCancelParams {
                        job_id: job_id.clone(),
                    })
                    .await
                    .unwrap();
            }
            release.notify_one();
            let terminal = wait_for_state(
                &registry,
                &job_id,
                if cancel_job {
                    JobState::Cancelled
                } else {
                    JobState::Succeeded
                },
            )
            .await;
            assert_eq!(terminal["execution"]["mode"], "isolated_worker");
            assert_eq!(terminal["execution"]["blender_finished"], true);
            assert_eq!(terminal["session_checkpoint_captured"], false);
            assert_eq!(live.commands(), vec!["rename_object"]);
            assert_eq!(
                worker.commands(),
                vec![
                    "bridge_status",
                    "job_restore_checkpoint",
                    "job_render_still"
                ]
            );
            assert!(!live_client.recovery_fenced());
        }
    }

    #[tokio::test]
    async fn isolated_worker_rejects_live_endpoint_before_loading_source() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .unwrap();
        let live = FakeAddon::spawn(|command, _| {
            assert_eq!(command, "bridge_status");
            ResponseSpec::Success {
                result: json!({"role": "live", "background": true}),
                addon_version: None,
            }
        })
        .await;
        let client = Arc::new(BlenderClient::new(
            live.host(),
            live.port(),
            ClientOptions::default(),
        ));
        let registry = JobRegistry::new_with_render_worker(
            workspace,
            Arc::clone(&client),
            PathBuf::from("unused-ffmpeg"),
            1,
            None,
            DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
            Some(client),
        );
        let submitted = registry
            .submit(submit_params("scene.blend", "still"))
            .await
            .unwrap();
        let failed = wait_for_state(
            &registry,
            submitted["job_id"].as_str().unwrap(),
            JobState::Failed,
        )
        .await;
        assert_eq!(failed["failure"]["code"], "render_worker_role_mismatch");
        assert_eq!(live.commands(), vec!["bridge_status"]);
    }

    #[tokio::test]
    async fn isolated_recovery_never_restores_or_contacts_the_live_scene() {
        for finished in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            let job_id = random_hex_id().unwrap();
            let mut record = test_job_record(&job_id, JobState::Running);
            record.execution = JobExecution::IsolatedWorker {
                blender_finished: finished,
            };
            record.source_snapshot = Some(JobSourceSnapshot {
                sha256: crate::tools::sha256_hex(b"checkpoint"),
                size_bytes: 10,
            });
            workspace
                .write_reserved_artifact(&record.source_artifact, b"checkpoint", false)
                .unwrap();
            if finished {
                let (size, _) =
                    write_product_test_frame(temp.path(), &frame_path(&job_id, 0), 32, 24);
                record.progress.completed_frames = 1;
                record.progress.frame_bytes = size;
            }
            workspace
                .write_reserved_artifact(
                    &job_metadata_path(&job_id),
                    &serde_json::to_vec(&record).unwrap(),
                    false,
                )
                .unwrap();
            workspace
                .write_reserved_artifact(
                    INDEX_PATH,
                    &serde_json::to_vec(&JobIndex {
                        schema_version: JOB_SCHEMA_VERSION,
                        job_ids: vec![job_id.clone()],
                    })
                    .unwrap(),
                    false,
                )
                .unwrap();
            let live = FakeAddon::spawn(|_, _| panic!("recovery contacted live Blender")).await;
            let worker = FakeAddon::spawn(|command, _| ResponseSpec::Success {
                result: match command.as_str() {
                    "bridge_status" => json!({"role": "render_worker", "background": true}),
                    "job_restore_checkpoint" => json!({"frame_current": 1}),
                    "job_render_still" => json!({"size_bytes": 1}),
                    other => panic!("unexpected recovery command {other}"),
                },
                addon_version: None,
            })
            .await;
            let registry = JobRegistry::new_with_render_worker(
                workspace,
                Arc::new(BlenderClient::new(
                    live.host(),
                    live.port(),
                    ClientOptions::default(),
                )),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(Arc::new(BlenderClient::new(
                    worker.host(),
                    worker.port(),
                    ClientOptions::default(),
                ))),
            );
            let completed = wait_for_state(&registry, &job_id, JobState::Succeeded).await;
            assert_eq!(completed["recovery_count"], 1);
            assert_eq!(completed["execution"]["blender_finished"], true);
            assert!(live.commands().is_empty());
            if finished {
                assert!(worker.commands().is_empty());
            } else {
                assert_eq!(
                    worker.commands(),
                    vec![
                        "bridge_status",
                        "job_restore_checkpoint",
                        "job_render_still"
                    ]
                );
            }
        }
    }

    #[tokio::test]
    async fn worker_migration_retains_history_but_blocks_unrestored_legacy_jobs() {
        for restored in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            let job_id = random_hex_id().unwrap();
            let mut record = test_job_record(
                &job_id,
                if restored {
                    JobState::Succeeded
                } else {
                    JobState::Running
                },
            );
            record.schema_version = 2;
            record.session_checkpoint_captured = true;
            record.session_checkpoint_restored = restored;
            record.progress.completed_frames = 1;
            let mut legacy = serde_json::to_value(record).unwrap();
            legacy.as_object_mut().unwrap().remove("execution");
            workspace
                .write_reserved_artifact(
                    &job_metadata_path(&job_id),
                    &serde_json::to_vec(&legacy).unwrap(),
                    false,
                )
                .unwrap();
            workspace
                .write_reserved_artifact(
                    INDEX_PATH,
                    &serde_json::to_vec(&JobIndex {
                        schema_version: 2,
                        job_ids: vec![job_id.clone()],
                    })
                    .unwrap(),
                    false,
                )
                .unwrap();
            let live = FakeAddon::spawn(|_, _| panic!("migration replayed a legacy job")).await;
            let worker =
                FakeAddon::spawn(|_, _| panic!("migration replayed a legacy job on worker")).await;
            let client = Arc::new(BlenderClient::new(
                live.host(),
                live.port(),
                ClientOptions::default(),
            ));
            let registry = JobRegistry::new_with_render_worker(
                workspace,
                Arc::clone(&client),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(Arc::new(BlenderClient::new(
                    worker.host(),
                    worker.port(),
                    ClientOptions::default(),
                ))),
            );
            assert_eq!(registry.inner.recovery_integrity.blocked(), !restored);
            assert_eq!(client.recovery_fenced(), !restored);
            assert_eq!(
                registry.status_value(&job_id).await.unwrap()["execution"]["mode"],
                "legacy_session"
            );
            assert!(live.commands().is_empty());
            assert!(worker.commands().is_empty());
        }
    }

    #[test]
    fn current_job_metadata_requires_an_explicit_execution_mode() {
        let mut value =
            serde_json::to_value(test_job_record(&random_hex_id().unwrap(), JobState::Queued))
                .unwrap();
        value.as_object_mut().unwrap().remove("execution");
        let record: JobRecord = serde_json::from_value(value).unwrap();
        assert!(validate_recovered_job(&record).is_err());
    }

    #[tokio::test]
    async fn worker_migration_preserves_live_fencing_when_legacy_history_is_unreadable() {
        for corrupt_index in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            let job_id = random_hex_id().unwrap();
            let index = if corrupt_index {
                b"{".to_vec()
            } else {
                workspace
                    .write_reserved_artifact(&job_metadata_path(&job_id), b"{", false)
                    .unwrap();
                serde_json::to_vec(&JobIndex {
                    schema_version: 2,
                    job_ids: vec![job_id],
                })
                .unwrap()
            };
            workspace
                .write_reserved_artifact(INDEX_PATH, &index, false)
                .unwrap();
            let fake =
                FakeAddon::spawn(|_, _| panic!("unreadable migration history reached Blender"))
                    .await;
            let client = Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            ));
            let registry = JobRegistry::new_with_render_worker(
                workspace.clone(),
                client.clone(),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(client.clone()),
            );
            assert!(client.recovery_fenced());
            assert!(registry.inner.recovery_integrity.blocked());
            assert!(!registry.inner.isolation.established());
            registry.inner.refresh_recovery_fence().await;
            assert!(client.recovery_fenced());
            assert!(matches!(
                workspace.read_artifact(ISOLATION_PATH),
                Err(WsError::NotFound(_))
            ));
            assert!(
                registry
                    .submit(submit_params("scene.blend", "still"))
                    .await
                    .is_err()
            );
            assert!(fake.commands().is_empty());
        }
    }

    #[tokio::test]
    async fn new_jobs_without_a_worker_never_contact_live_blender() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .unwrap();
        let fake = FakeAddon::spawn(|_, _| panic!("new job reached live Blender")).await;
        let client = Arc::new(BlenderClient::new(
            fake.host(),
            fake.port(),
            ClientOptions::default(),
        ));
        let registry =
            JobRegistry::new(workspace, client.clone(), PathBuf::from("unused-ffmpeg"), 1);
        assert!(
            registry
                .submit(submit_params("scene.blend", "still"))
                .await
                .is_err()
        );
        assert!(registry.inner.state.lock().await.jobs.is_empty());
        assert!(!client.recovery_fenced());
        assert_eq!(registry.worker_health().await["status"], "not_configured");
        assert!(fake.commands().is_empty());
    }

    #[tokio::test]
    async fn worker_readiness_verifies_role_and_does_not_wait_for_active_work() {
        for (role, background, available) in [
            ("live", false, false),
            ("render_worker", false, false),
            ("render_worker", true, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            let fake = FakeAddon::spawn(move |_, _| ResponseSpec::Success {
                result: json!({"role": role, "background": background}),
                addon_version: Some("test".into()),
            })
            .await;
            let client = Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            ));
            let registry = JobRegistry::new_with_render_worker(
                workspace,
                client.clone(),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(client.clone()),
            );
            assert_eq!(registry.worker_health().await["available"], available);
            let transaction = client.transaction(client.default_deadline()).await.unwrap();
            let busy = tokio::time::timeout(Duration::from_millis(100), registry.worker_health())
                .await
                .unwrap();
            assert_eq!(busy["status"], "busy");
            assert_eq!(busy["available"], available);
            drop(transaction);
        }
    }

    #[tokio::test]
    async fn established_isolation_never_reverts_to_live_rendering() {
        for (configured, corrupt_index) in [(true, true), (false, true), (false, false)] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).unwrap());
            workspace
                .write_artifact("scene.blend", b"checkpoint", false)
                .unwrap();
            let fake =
                FakeAddon::spawn(|_, _| panic!("isolated recovery contacted live Blender")).await;
            let client = Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            ));
            let initial = JobRegistry::new_with_render_worker(
                workspace.clone(),
                client.clone(),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                Some(client.clone()),
            );
            assert!(initial.inner.isolation.established());
            drop(initial);
            if corrupt_index {
                workspace
                    .write_reserved_artifact(INDEX_PATH, b"{", true)
                    .unwrap();
            }
            let registry = JobRegistry::new_with_render_worker(
                workspace.clone(),
                client.clone(),
                PathBuf::from("unused-ffmpeg"),
                1,
                None,
                DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                configured.then(|| client.clone()),
            );
            assert!(registry.inner.isolation.established());
            assert!(!client.recovery_fenced());
            registry.inner.refresh_recovery_fence().await;
            assert!(!client.recovery_fenced());
            assert_eq!(registry.inner.recovery_integrity.blocked(), corrupt_index);
            assert!(
                registry
                    .submit(submit_params("scene.blend", "still"))
                    .await
                    .is_err()
            );
            assert!(fake.commands().is_empty());
        }
    }

    #[tokio::test]
    async fn still_job_persists_progress_and_discoverable_artifact() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let root = temp.path().to_path_buf();
        let commands = Arc::new(StdMutex::new(Vec::new()));
        let observed_commands = Arc::clone(&commands);
        let render_calls = Arc::new(AtomicUsize::new(0));
        let observed_render_calls = Arc::clone(&render_calls);
        let fake = FakeAddon::spawn(move |command, params| {
            observed_commands.lock().unwrap().push(format!(
                "{command}:{}",
                params.get("path").and_then(Value::as_str).unwrap_or("")
            ));
            match command.as_str() {
                "job_save_checkpoint" => ResponseSpec::Success {
                    result: json!({"saved": true}),
                    addon_version: Some("test".to_string()),
                },
                "job_restore_checkpoint" => ResponseSpec::Success {
                    result: json!({"restored": true, "frame_current": 1}),
                    addon_version: Some("test".to_string()),
                },
                "job_measure_sequence_bounds" => ResponseSpec::Success {
                    result: json!({
                        "bounds": product_test_bounds(),
                        "frames_evaluated": 1,
                        "frame_start": params["frame_start"],
                        "frame_end": params["frame_end"],
                        "frame_step": params["frame_step"]
                    }),
                    addon_version: Some("test".to_string()),
                },
                "job_render_product" => {
                    assert_eq!(params["presentation"]["profile"], json!("studio_neutral"));
                    assert_eq!(
                        params["max_output_bytes"],
                        json!(MAX_PRODUCT_RENDER_BYTES),
                        "the aggregate durable-job budget is capped to the product renderer's per-frame contract"
                    );
                    let path = params["path"].as_str().expect("render path");
                    let (size_bytes, sha256) =
                        if observed_render_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            write_product_test_frame(
                                &root,
                                path,
                                u32::try_from(params["width"].as_u64().expect("width"))
                                    .expect("test width"),
                                u32::try_from(params["height"].as_u64().expect("height"))
                                    .expect("test height"),
                            )
                        } else {
                            let bytes = b"not-a-png";
                            let destination = root.join(path);
                            std::fs::create_dir_all(
                                destination.parent().expect("frame parent"),
                            )
                            .expect("create frame directory");
                            std::fs::write(&destination, bytes).expect("write corrupt frame");
                            (
                                u64::try_from(bytes.len()).expect("corrupt frame size"),
                                crate::tools::sha256_hex(bytes),
                            )
                        };
                    let bounds = product_test_bounds();
                    assert_eq!(params["camera_behavior"], json!("bounds"));
                    assert_eq!(params["framing_bounds"], bounds);
                    ResponseSpec::Success {
                        result: json!({
                            "path": path,
                            "size_bytes": size_bytes,
                            "sha256": sha256,
                            "media_type": "image/png",
                            "width": params["width"],
                            "height": params["height"],
                            "objects": ["Body"],
                            "bounds": bounds,
                            "source_state_verified": true,
                            "cleanup_verified": true,
                            "presentation": job_product_presentation(
                                &params,
                                json!({
                                    "behavior": "bounds",
                                    "azimuth_degrees": 45.0,
                                    "elevation_degrees": 25.0
                                }),
                                &bounds
                            )
                        }),
                        addon_version: Some("test".to_string()),
                    }
                }
                _ => ResponseSpec::Error {
                    message: format!("unexpected command: {command}"),
                    traceback: None,
                    addon_version: Some("test".to_string()),
                },
            }
        })
        .await;
        let blender = Arc::new(BlenderClient::new(
            fake.host(),
            fake.port(),
            ClientOptions::default(),
        ));
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            blender,
            PathBuf::from("unused-ffmpeg"),
            1,
        );

        let mut presented = submit_params("scene.blend", "still");
        presented.presentation = Some(
            serde_json::from_value(json!({
                "profile": "studio_neutral"
            }))
            .expect("product presentation"),
        );
        let submitted = registry
            .stage_submission(presented.clone())
            .await
            .expect("submit still job");
        let job_id = submitted["job_id"].as_str().expect("job id").to_string();
        let completed = wait_for_state(&registry, &job_id, JobState::Succeeded).await;

        assert_eq!(completed["progress"]["completed_frames"], json!(1));
        assert!(
            completed["progress"]["frame_bytes"]
                .as_u64()
                .is_some_and(|size| size > 0)
        );
        let artifacts = registry
            .artifacts(RenderJobArtifactsParams {
                job_id: job_id.clone(),
                offset: 0,
                limit: 100,
            })
            .await
            .expect("job artifacts");
        assert_eq!(artifacts["frames"][0]["sequence_index"], json!(1));
        assert_eq!(artifacts["frames"][0]["media_type"], json!("image/png"));
        assert!(
            temp.path().join(frame_path(&job_id, 0)).is_file(),
            "frame is durable in the workspace"
        );
        assert_eq!(
            *commands.lock().unwrap(),
            vec![
                format!("job_save_checkpoint:{JOB_ROOT}/{job_id}/pre-job-session.blend"),
                format!("job_restore_checkpoint:{JOB_ROOT}/{job_id}/source.blend"),
                "job_measure_sequence_bounds:".to_string(),
                format!("job_render_product:{}", frame_path(&job_id, 0)),
                format!("job_restore_checkpoint:{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            ],
            "the live Blender session is checkpointed before source mutation and restored afterward"
        );
        assert_eq!(completed["session_checkpoint_captured"], json!(true));
        assert_eq!(completed["session_checkpoint_restored"], json!(true));
        assert_eq!(
            completed["spec"]["presentation"]["profile"],
            json!("studio_neutral")
        );
        assert_eq!(completed["presentation_bounds"], product_test_bounds());
        assert!(!registry.inner.blender.recovery_fenced());
        let corrupt = registry
            .stage_submission(presented)
            .await
            .expect("terminal job releases the only admission slot");
        let corrupt_job_id = corrupt["job_id"].as_str().expect("job id");
        let failed = wait_for_state(&registry, corrupt_job_id, JobState::Failed).await;
        assert_eq!(failed["failure"]["code"], json!("validation"));
        assert_eq!(failed["progress"]["completed_frames"], json!(0));
        assert_eq!(failed["progress"]["frame_bytes"], json!(0));
    }

    #[tokio::test]
    async fn mechanical_job_persists_failed_certificate_before_rendering() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let geometry_worker = temp.path().join("fake-geometry-worker");
        std::fs::write(
            &geometry_worker,
            r#"#!/bin/sh
printf '%s' '{"report":{"fixed":{"vertices":8,"triangles":12,"bounds":{"minimum_mm":[0.0,0.0,0.0],"maximum_mm":[1.0,1.0,1.0],"dimensions_mm":[1.0,1.0,1.0]},"volume_mm3":1.0},"moving":{"vertices":8,"triangles":12,"bounds":{"minimum_mm":[2.0,0.0,0.0],"maximum_mm":[3.0,1.0,1.0],"dimensions_mm":[1.0,1.0,1.0]},"volume_mm3":1.0},"static_analysis":{"relation":"separated","clearance_mm":1.0,"surface_gap_mm":1.0,"closest_surface_points":{"fixed_mm":[1.0,0.0,0.0],"moving_mm":[2.0,0.0,0.0]},"interference_volume_mm3":0.0,"fixed_interference_fraction":0.0,"moving_interference_fraction":0.0,"required_clearance_mm":0.2,"meets_required_clearance":true},"rotation":{"pivot_mm":[0.0,0.0,0.0],"axis":[0.0,0.0,1.0],"angle_degrees":90.0,"target_clearance_mm":0.2,"can_rotate_full_angle":false,"retained":true,"first_limit_interval_degrees":[45.0,46.0],"block_reason":"contact"}}}'
"#,
        )
        .expect("write fake geometry worker");
        let mut permissions = std::fs::metadata(&geometry_worker)
            .expect("geometry worker metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&geometry_worker, permissions)
            .expect("make fake geometry worker executable");

        let root = temp.path().to_path_buf();
        let render_attempts = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&render_attempts);
        let fake = FakeAddon::spawn(move |command, params| match command.as_str() {
            "job_save_checkpoint" | "job_restore_checkpoint" => ResponseSpec::Success {
                result: json!({"ok": true}),
                addon_version: Some("test".to_string()),
            },
            "job_prepare_mechanical_rotation" => {
                let fixed_path = params["fixed_path"].as_str().expect("fixed path");
                let moving_path = params["moving_path"].as_str().expect("moving path");
                for path in [fixed_path, moving_path] {
                    let destination = root.join(path);
                    std::fs::create_dir_all(destination.parent().expect("analysis parent"))
                        .expect("create analysis directory");
                    std::fs::write(destination, b"solid fake\nendsolid fake\n")
                        .expect("write analysis STL");
                }
                ResponseSpec::Success {
                    result: json!({
                        "fixed_path": fixed_path,
                        "moving_path": moving_path,
                        "motion": {
                            "controller": params["controller_name"],
                            "objects": params["moving_objects"],
                            "pivot": params["pivot"],
                            "axis": [0.0, 0.0, 1.0],
                            "angle_degrees": params["angle_degrees"],
                            "frame_start": params["frame_start"],
                            "frame_end": params["frame_end"],
                            "interpolation": "LINEAR"
                        }
                    }),
                    addon_version: Some("test".to_string()),
                }
            }
            "job_render_frame" | "job_render_product" => {
                observed_renders.fetch_add(1, Ordering::SeqCst);
                ResponseSpec::Success {
                    result: json!({"size_bytes": 1}),
                    addon_version: Some("test".to_string()),
                }
            }
            _ => ResponseSpec::Error {
                message: format!("unexpected command: {command}"),
                traceback: None,
                addon_version: Some("test".to_string()),
            },
        })
        .await;
        let registry = JobRegistry::new_with_geometry_worker(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            PathBuf::from("unused-ffmpeg"),
            1,
            Some(geometry_worker),
            64 * 1024 * 1024,
        );
        let params = serde_json::from_value(json!({
            "source_blend": "scene.blend",
            "kind": "mechanical_rotation",
            "mechanical_rotation": {
                "fixed_objects": ["Base"],
                "moving_objects": ["Leaf"],
                "pivot_mm": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.2,
                "max_analysis_mesh_bytes": 1024
            },
            "frame_start": 1,
            "frame_end": 3,
            "width": 32,
            "height": 24,
            "presentation": {
                "profile": "studio_dark"
            },
            "frame_timeout_seconds": 5.0,
            "encode_timeout_seconds": 5.0
        }))
        .expect("mechanical job params");

        let submitted = registry
            .stage_submission(params)
            .await
            .expect("submit mechanical job");
        let job_id = submitted["job_id"].as_str().expect("job id");
        let failed = wait_for_state(&registry, job_id, JobState::Failed).await;

        assert_eq!(
            failed["failure"]["code"],
            json!("mechanical_clearance_not_certified")
        );
        assert_eq!(failed["progress"]["completed_frames"], json!(0));
        assert_eq!(failed["mechanical_analysis"]["generation"], json!(0));
        assert_eq!(failed["mechanical_analysis"]["certified"], json!(false));
        assert_eq!(
            failed["spec"]["presentation"]["profile"],
            json!("studio_dark")
        );
        assert_eq!(failed["presentation_bounds"], Value::Null);
        assert_eq!(
            failed["mechanical_analysis"]["report"]["rotation"]["block_reason"],
            json!("contact")
        );
        assert_eq!(render_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(failed["session_checkpoint_restored"], json!(true));
        assert!(
            temp.path().join(mechanical_fixed_path(job_id, 0)).is_file(),
            "fixed analysis snapshot is durable"
        );
        assert!(
            temp.path()
                .join(mechanical_moving_path(job_id, 0))
                .is_file(),
            "moving analysis snapshot is durable"
        );
    }

    #[tokio::test]
    async fn render_failure_restores_the_pre_job_blender_session() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let commands = Arc::new(StdMutex::new(Vec::new()));
        let observed_commands = Arc::clone(&commands);
        let render_attempts = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&render_attempts);
        let restore_attempts = Arc::new(AtomicUsize::new(0));
        let observed_restores = Arc::clone(&restore_attempts);
        let fake = FakeAddon::spawn(move |command, params| {
            observed_commands.lock().unwrap().push(format!(
                "{command}:{}",
                params.get("path").and_then(Value::as_str).unwrap_or("")
            ));
            match command.as_str() {
                "job_save_checkpoint" => ResponseSpec::Success {
                    result: json!({"saved": true}),
                    addon_version: Some("test".to_string()),
                },
                "job_restore_checkpoint" => {
                    let path = params["path"].as_str().expect("restore path");
                    if path.ends_with("pre-job-session.blend")
                        && observed_restores.fetch_add(1, Ordering::SeqCst) == 0
                    {
                        ResponseSpec::Error {
                            message: "transient restore failure".to_string(),
                            traceback: None,
                            addon_version: Some("test".to_string()),
                        }
                    } else {
                        ResponseSpec::Success {
                            result: json!({"restored": true}),
                            addon_version: Some("test".to_string()),
                        }
                    }
                }
                "job_render_still" => {
                    observed_renders.fetch_add(1, Ordering::SeqCst);
                    ResponseSpec::Error {
                        message: "render engine failed".to_string(),
                        traceback: None,
                        addon_version: Some("test".to_string()),
                    }
                }
                _ => ResponseSpec::Error {
                    message: format!("unexpected command: {command}"),
                    traceback: None,
                    addon_version: Some("test".to_string()),
                },
            }
        })
        .await;
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            PathBuf::from("unused-ffmpeg"),
            1,
        );

        let submitted = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect("submit still job");
        let job_id = submitted["job_id"].as_str().expect("job id");
        let failed = wait_for_state(&registry, job_id, JobState::Failed).await;

        assert_eq!(failed["failure"]["code"], json!("addon"));
        assert_eq!(failed["session_checkpoint_restored"], json!(true));
        assert_eq!(render_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(restore_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            commands.lock().unwrap().last(),
            Some(&format!(
                "job_restore_checkpoint:{JOB_ROOT}/{job_id}/pre-job-session.blend"
            )),
            "the pre-job session restore runs after a render error"
        );
    }

    #[tokio::test]
    async fn transient_session_restore_failure_stays_visible_and_retries_until_recovered() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let session_restore_attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&session_restore_attempts);
        let render_attempts = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&render_attempts);
        let fake = FakeAddon::spawn(move |command, params| match command.as_str() {
            "job_save_checkpoint" => ResponseSpec::Success {
                result: json!({"saved": true}),
                addon_version: Some("test".to_string()),
            },
            "job_restore_checkpoint" => {
                let path = params["path"].as_str().expect("restore path");
                if path.ends_with("pre-job-session.blend")
                    && observed_attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    ResponseSpec::Error {
                        message: "transient restore failure".to_string(),
                        traceback: None,
                        addon_version: Some("test".to_string()),
                    }
                } else {
                    ResponseSpec::Success {
                        result: json!({"restored": true}),
                        addon_version: Some("test".to_string()),
                    }
                }
            }
            "job_render_still" => {
                observed_renders.fetch_add(1, Ordering::SeqCst);
                ResponseSpec::Success {
                    result: json!({"size_bytes": 9}),
                    addon_version: Some("test".to_string()),
                }
            }
            _ => ResponseSpec::Error {
                message: format!("unexpected command: {command}"),
                traceback: None,
                addon_version: Some("test".to_string()),
            },
        })
        .await;
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            PathBuf::from("unused-ffmpeg"),
            1,
        );

        let submitted = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect("submit still job");
        let job_id = submitted["job_id"].as_str().expect("job id");
        let waiting = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = registry.status_value(job_id).await.expect("job status");
                if status["progress"]["phase"] == json!("waiting_for_session_recovery") {
                    return status;
                }
                assert!(
                    status["state"] == json!("queued") || status["state"] == json!("running"),
                    "session recovery must not become terminal: {status}"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restore failure becomes visible");
        assert_eq!(waiting["failure"]["code"], json!("session_restore_failed"));
        assert!(registry.inner.blender.recovery_fenced());

        let completed = wait_for_state(&registry, job_id, JobState::Succeeded).await;
        assert_eq!(completed["session_checkpoint_restored"], json!(true));
        assert!(completed["failure"].is_null());
        assert_eq!(render_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(session_restore_attempts.load(Ordering::SeqCst), 2);
        assert!(!registry.inner.blender.recovery_fenced());
    }

    #[tokio::test]
    async fn recovered_start_metadata_failure_stays_fenced_until_retry_is_durable() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let blocking_path = temp.path().join(JOB_ROOT);
        std::fs::create_dir_all(blocking_path.parent().expect("job root parent"))
            .expect("create metadata parent");
        std::fs::write(&blocking_path, b"blocks job metadata directory")
            .expect("block metadata directory");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "0123456789abcdef0123456789abcdef".to_string();
        let mut record = test_job_record(&job_id, JobState::Queued);
        record.session_checkpoint_captured = true;
        record.progress.phase = "recovered_session_restore".to_string();
        let blender = Arc::new(BlenderClient::new(
            "start-persistence.invalid",
            65530,
            ClientOptions::default(),
        ));
        blender.set_recovery_fenced(true);
        let inner = test_registry_inner(
            Arc::clone(&workspace),
            Arc::clone(&blender),
            job_id.clone(),
            record,
        );
        let initial_error = inner
            .start_job(&job_id)
            .await
            .expect_err("blocked metadata path fails the recovered start transition");
        assert_eq!(
            inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("recovered job")
                .state,
            JobState::Running
        );

        let retrying_inner = Arc::clone(&inner);
        let retrying_job_id = job_id.clone();
        let retrying = tokio::spawn(async move {
            resolve_start_persistence_failure(&retrying_inner, &retrying_job_id, initial_error)
                .await
        });
        assert!(blender.recovery_fenced());
        assert!(
            !inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("recovered job")
                .state
                .terminal()
        );

        std::fs::remove_file(&blocking_path).expect("remove metadata blocker");
        std::fs::create_dir(&blocking_path).expect("create metadata directory");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), retrying)
                .await
                .expect("metadata retry completes")
                .expect("metadata retry task"),
            StartPersistenceResolution::Proceed
        );

        let (_, bytes) = workspace
            .read_artifact(&job_metadata_path(&job_id))
            .expect("durable recovered running state");
        let durable: JobRecord = serde_json::from_slice(&bytes).expect("durable job metadata");
        assert_eq!(durable.state, JobState::Running);
        assert!(durable.session_checkpoint_captured);
        assert!(!durable.session_checkpoint_restored);
        assert!(blender.recovery_fenced());
    }

    #[test]
    fn cancelled_persistence_keeps_serialization_until_the_blocking_write_finishes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace tempdir");
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
            let job_id = "abcdef0123456789abcdef0123456789".to_string();
            let record = test_job_record(&job_id, JobState::Queued);
            let inner = test_registry_inner(
                Arc::clone(&workspace),
                Arc::new(BlenderClient::new(
                    "cancelled-persistence.invalid",
                    65530,
                    ClientOptions::default(),
                )),
                job_id.clone(),
                record,
            );
            let (release, blocker) = occupy_only_blocking_thread().await;
            let mut persistence = Box::pin(inner.persist_job(&job_id));
            std::future::poll_fn(|context| match persistence.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => {
                    panic!("persistence completed before its blocking write: {result:?}")
                }
            })
            .await;

            drop(persistence);
            assert!(
                inner.persistence.try_lock().is_err(),
                "the blocking write must retain serialization after its caller is cancelled"
            );
            release.send(()).expect("release blocking pool");
            blocker.await.expect("blocking pool task");
            let serialization = tokio::time::timeout(
                Duration::from_secs(2),
                Arc::clone(&inner.persistence).lock_owned(),
            )
            .await
            .expect("blocking metadata write releases serialization");
            drop(serialization);

            let (_, bytes) = workspace
                .read_artifact(&job_metadata_path(&job_id))
                .expect("cancelled caller still completes its metadata write");
            let durable: JobRecord = serde_json::from_slice(&bytes).expect("durable job metadata");
            assert_eq!(durable.job_id, job_id);
        });
    }

    #[tokio::test]
    async fn restored_session_keeps_the_fence_closed_when_metadata_persistence_fails() {
        let job_id = "fedcba9876543210fedcba9876543210".to_string();
        let mut record = test_job_record(&job_id, JobState::Running);
        record.session_checkpoint_captured = true;
        let blender = Arc::new(BlenderClient::new(
            "restore-persistence.invalid",
            65530,
            ClientOptions::default(),
        ));
        blender.set_recovery_fenced(true);
        let inner = test_registry_inner(
            Arc::new(Workspace::open(None, None).expect("unconfined workspace")),
            Arc::clone(&blender),
            job_id.clone(),
            record,
        );

        let error = inner
            .persist_restored_marker_once(&job_id, None)
            .await
            .expect_err("unconfined metadata write fails");

        assert_eq!(error.code(), "unconfined");
        assert!(
            !inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("job state")
                .session_checkpoint_restored
        );
        assert!(blender.recovery_fenced());
        assert!(
            inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("job state")
                .session_checkpoint_captured
        );
    }

    #[tokio::test]
    async fn restored_session_reopens_the_fence_only_after_a_retry_commits_metadata() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let blocking_path = temp.path().join(JOB_ROOT);
        std::fs::create_dir_all(blocking_path.parent().expect("job root parent"))
            .expect("create metadata parent");
        std::fs::write(&blocking_path, b"blocks job metadata directory")
            .expect("block metadata directory");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "abcdef9876543210abcdef9876543210".to_string();
        let mut record = test_job_record(&job_id, JobState::Running);
        record.session_checkpoint_captured = true;
        let blender = Arc::new(BlenderClient::new(
            "restore-persistence.invalid",
            65530,
            ClientOptions::default(),
        ));
        blender.set_recovery_fenced(true);
        let inner = test_registry_inner(
            Arc::clone(&workspace),
            Arc::clone(&blender),
            job_id.clone(),
            record,
        );
        let retrying_inner = Arc::clone(&inner);
        let retrying_job_id = job_id.clone();
        let persistence = tokio::spawn(async move {
            retrying_inner
                .persist_restored_marker_until_durable(&retrying_job_id, None)
                .await;
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let waiting = inner
                    .state
                    .lock()
                    .await
                    .jobs
                    .get(&job_id)
                    .is_some_and(|job| {
                        job.progress.phase == "waiting_for_restoration_metadata_commit"
                    });
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("metadata failure becomes visible");
        assert!(blender.recovery_fenced());
        assert!(
            !inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("job state")
                .session_checkpoint_restored
        );

        std::fs::remove_file(&blocking_path).expect("remove metadata blocker");
        std::fs::create_dir(&blocking_path).expect("create metadata directory");
        tokio::time::timeout(Duration::from_secs(3), persistence)
            .await
            .expect("metadata retry completes")
            .expect("metadata retry task");

        assert!(
            inner
                .state
                .lock()
                .await
                .jobs
                .get(&job_id)
                .expect("job state")
                .session_checkpoint_restored
        );
        assert!(!blender.recovery_fenced());
        let (_, bytes) = workspace
            .read_artifact(&job_metadata_path(&job_id))
            .expect("durable restored marker");
        let durable: JobRecord = serde_json::from_slice(&bytes).expect("durable job metadata");
        assert!(durable.session_checkpoint_restored);
    }

    #[tokio::test]
    async fn completed_blender_outcome_retries_metadata_before_session_restoration() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let blocking_path = temp.path().join(JOB_ROOT);
        std::fs::create_dir_all(blocking_path.parent().expect("job root parent"))
            .expect("create metadata parent");
        std::fs::write(&blocking_path, b"blocks job metadata directory")
            .expect("block metadata directory");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "7890abcdef1234567890abcdef123456".to_string();
        let mut record = test_job_record(&job_id, JobState::Running);
        record.session_checkpoint_captured = true;
        let blender = Arc::new(BlenderClient::new(
            "outcome-persistence.invalid",
            65530,
            ClientOptions::default(),
        ));
        blender.set_recovery_fenced(true);
        let inner = test_registry_inner(
            Arc::clone(&workspace),
            Arc::clone(&blender),
            job_id.clone(),
            record,
        );
        let outcome = PendingBlenderOutcome::Failed(JobFailure {
            code: "addon".to_string(),
            message: "deterministic render failure".to_string(),
        });
        let initial_error = inner
            .record_pending_blender_outcome(&job_id, outcome.clone())
            .await
            .expect_err("blocked metadata path fails");
        let retrying_inner = Arc::clone(&inner);
        let retrying_job_id = job_id.clone();
        let persistence = tokio::spawn(async move {
            retrying_inner
                .retry_pending_blender_outcome_until_durable(
                    &retrying_job_id,
                    outcome,
                    initial_error,
                )
                .await;
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let waiting = inner
                    .state
                    .lock()
                    .await
                    .jobs
                    .get(&job_id)
                    .is_some_and(|job| {
                        job.progress.phase == "waiting_for_blender_outcome_metadata_commit"
                    });
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("outcome metadata failure becomes visible");
        assert!(blender.recovery_fenced());

        std::fs::remove_file(&blocking_path).expect("remove metadata blocker");
        std::fs::create_dir(&blocking_path).expect("create metadata directory");
        tokio::time::timeout(Duration::from_secs(3), persistence)
            .await
            .expect("outcome metadata retry completes")
            .expect("outcome metadata retry task");

        let (_, bytes) = workspace
            .read_artifact(&job_metadata_path(&job_id))
            .expect("durable pending Blender outcome");
        let durable: JobRecord = serde_json::from_slice(&bytes).expect("durable job metadata");
        assert!(matches!(
            durable.pending_blender_outcome,
            Some(PendingBlenderOutcome::Failed(JobFailure { ref code, .. })) if code == "addon"
        ));
        assert!(!durable.session_checkpoint_restored);
        assert!(blender.recovery_fenced());
    }

    #[tokio::test]
    async fn turntable_job_encodes_and_commits_a_structurally_valid_mp4() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let encoder = temp.path().join("fake-ffmpeg");
        std::fs::write(
            &encoder,
            "#!/bin/sh\nvalidation=0\nfor argument in \"$@\"; do\n  [ \"$argument\" = \"-progress\" ] && validation=1\n  output=$argument\ndone\nif [ \"$validation\" = 1 ]; then\n  printf 'frame=2\\nout_time_us=66667\\nprogress=end\\n'\n  exit 0\nfi\nprintf '\\000\\000\\000\\020ftypisom\\000\\000\\000\\000\\000\\000\\000\\010mdat\\000\\000\\000\\010moov' > \"$output\"\n",
        )
        .expect("write fake encoder");
        let mut permissions = std::fs::metadata(&encoder).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&encoder, permissions).expect("make fake encoder executable");
        let root = temp.path().to_path_buf();
        let fake = FakeAddon::spawn(move |command, params| match command.as_str() {
            "job_save_checkpoint" => ResponseSpec::Success {
                result: json!({"saved": true}),
                addon_version: Some("test".to_string()),
            },
            "job_restore_checkpoint" => ResponseSpec::Success {
                result: json!({"restored": true, "frame_current": 1}),
                addon_version: Some("test".to_string()),
            },
            "job_measure_sequence_bounds" => ResponseSpec::Success {
                result: json!({
                    "bounds": product_test_bounds(),
                    "frames_evaluated": 1,
                    "frame_start": params["frame_start"],
                    "frame_end": params["frame_end"],
                    "frame_step": params["frame_step"]
                }),
                addon_version: Some("test".to_string()),
            },
            "job_render_product" => {
                let path = params["path"].as_str().expect("render path");
                let (size_bytes, sha256) = write_product_test_frame(
                    &root,
                    path,
                    u32::try_from(params["width"].as_u64().expect("width"))
                        .expect("test width"),
                    u32::try_from(params["height"].as_u64().expect("height"))
                        .expect("test height"),
                );
                let bounds = product_test_bounds();
                assert_eq!(params["camera_behavior"], json!("bounds"));
                assert_eq!(params["framing_bounds"], bounds);
                ResponseSpec::Success {
                    result: json!({
                        "path": path,
                        "size_bytes": size_bytes,
                        "sha256": sha256,
                        "media_type": "image/png",
                        "width": params["width"],
                        "height": params["height"],
                        "objects": ["Body"],
                        "bounds": bounds,
                        "source_state_verified": true,
                        "cleanup_verified": true,
                        "presentation": job_product_presentation(
                            &params,
                            json!({
                                "behavior": "bounds",
                                "azimuth_degrees": params["presentation"]["view"]["azimuth_degrees"],
                                "elevation_degrees": params["presentation"]["view"]["elevation_degrees"]
                            }),
                            &bounds
                        )
                    }),
                    addon_version: Some("test".to_string()),
                }
            }
            _ => ResponseSpec::Error {
                message: format!("unexpected command: {command}"),
                traceback: None,
                addon_version: Some("test".to_string()),
            },
        })
        .await;
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            encoder,
            2,
        );
        let params = serde_json::from_value(json!({
            "source_blend": "scene.blend",
            "kind": "turntable",
            "turntable_frames": 2,
            "width": 32,
            "height": 24,
            "presentation": {"profile": "engineering"},
            "frame_timeout_seconds": 5.0,
            "encode_timeout_seconds": 5.0,
        }))
        .expect("turntable params");

        let submitted = registry
            .stage_submission(params)
            .await
            .expect("submit turntable");
        let job_id = submitted["job_id"].as_str().expect("job id").to_string();
        let completed = wait_for_state(&registry, &job_id, JobState::Succeeded).await;

        assert_eq!(completed["progress"]["completed_frames"], json!(2));
        assert_eq!(
            completed["spec"]["presentation"]["profile"],
            json!("engineering")
        );
        assert_eq!(completed["presentation_bounds"], product_test_bounds());
        assert_eq!(
            completed["video_artifact"]["media_type"],
            json!("video/mp4")
        );
        assert_eq!(completed["video_artifact"]["size_bytes"], json!(32));
        assert_eq!(
            std::fs::metadata(temp.path().join(format!("{JOB_ROOT}/{job_id}/video.mp4")))
                .expect("committed video")
                .len(),
            32
        );
    }

    #[tokio::test]
    async fn video_encoding_and_decode_validation_share_one_runtime_budget() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "fedcba9876543210fedcba9876543210".to_string();
        for index in 0..2 {
            workspace
                .write_reserved_artifact(&frame_path(&job_id, index), b"frame", false)
                .expect("durable frame");
        }
        let encoder = temp.path().join("slow-fake-ffmpeg");
        std::fs::write(
            &encoder,
            "#!/bin/sh\nvalidation=0\nfor argument in \"$@\"; do\n  [ \"$argument\" = \"-progress\" ] && validation=1\n  output=$argument\ndone\nif [ \"$validation\" = 1 ]; then\n  exec sleep 0.6\nfi\nsleep 0.2\nprintf '\\000\\000\\000\\020ftypisom\\000\\000\\000\\000\\000\\000\\000\\010mdat\\000\\000\\000\\010moov' > \"$output\"\n",
        )
        .expect("write slow fake encoder");
        let mut permissions = std::fs::metadata(&encoder).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&encoder, permissions).expect("make fake encoder executable");
        let mut record = test_job_record(&job_id, JobState::Running);
        record.kind = RenderJobKind::Turntable;
        record.progress.total_frames = 2;
        record.progress.completed_frames = 2;
        record.spec.turntable_frames = 2;
        record.spec.encode_timeout_seconds = 0.5;
        let inner = test_registry_inner_with_encoder(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                "encoder-budget.invalid",
                65530,
                ClientOptions::default(),
            )),
            job_id.clone(),
            record.clone(),
            encoder,
        );

        let failure = encode_video(&inner, &record, &CancellationToken::new())
            .await
            .expect_err("decode validation must receive only the remaining runtime budget");
        assert_eq!(failure.code, "encoder_timeout");
        assert!(
            workspace
                .resolve(&format!("{JOB_ROOT}/{job_id}/video.mp4"), true)
                .is_err(),
            "timed-out validation cannot publish the video"
        );
    }

    #[tokio::test]
    async fn queued_cancel_is_immediate_and_running_cancel_waits_for_the_frame_boundary() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let render_started = Arc::new(Notify::new());
        let release_render = Arc::new(Notify::new());
        let fake_render_started = Arc::clone(&render_started);
        let fake_release_render = Arc::clone(&release_render);
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let fake = FakeAddon::spawn(move |command, _params| match command.as_str() {
            "job_save_checkpoint" => ResponseSpec::Success {
                result: json!({"saved": true}),
                addon_version: Some("test".to_string()),
            },
            "job_restore_checkpoint" => ResponseSpec::Success {
                result: json!({"restored": true}),
                addon_version: Some("test".to_string()),
            },
            "job_render_still" => ResponseSpec::SuccessWhenReleased {
                result: json!({"size_bytes": 9}),
                addon_version: Some("test".to_string()),
                started: Arc::clone(&fake_render_started),
                release: Arc::clone(&fake_release_render),
            },
            _ => ResponseSpec::Error {
                message: format!("unexpected command: {command}"),
                traceback: None,
                addon_version: Some("test".to_string()),
            },
        })
        .await;
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            PathBuf::from("unused-ffmpeg"),
            2,
        );
        let first = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect("first job");
        let first_id = first["job_id"].as_str().expect("first id").to_string();
        wait_for_state(&registry, &first_id, JobState::Running).await;
        render_started.notified().await;
        let second = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect("second job");
        let second_id = second["job_id"].as_str().expect("second id").to_string();
        assert_eq!(
            registry
                .stage_submission(submit_params("scene.blend", "still"))
                .await
                .expect_err("active jobs consume the configured admission capacity")
                .code(),
            "job_queue_full"
        );

        let cancelled = registry
            .cancel(RenderJobCancelParams {
                job_id: second_id.clone(),
            })
            .await
            .expect("cancel queued job");
        assert_eq!(cancelled["state"], json!("cancelled"));
        assert_eq!(cancelled["progress"]["completed_frames"], json!(0));
        let replacement = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect("queued cancellation releases logical admission capacity");
        let replacement_id = replacement["job_id"]
            .as_str()
            .expect("replacement id")
            .to_string();
        registry
            .cancel(RenderJobCancelParams {
                job_id: replacement_id.clone(),
            })
            .await
            .expect("cancel replacement job");

        let running_cancel = registry
            .cancel(RenderJobCancelParams {
                job_id: first_id.clone(),
            })
            .await
            .expect("request running cancellation");
        assert_eq!(running_cancel["state"], json!("running"));
        assert_eq!(running_cancel["cancellation_requested"], json!(true));
        release_render.notify_one();
        let first_cancelled = wait_for_state(&registry, &first_id, JobState::Cancelled).await;
        assert_eq!(first_cancelled["progress"]["completed_frames"], json!(1));
        assert_eq!(
            registry.status_value(&second_id).await.unwrap()["state"],
            json!("cancelled")
        );
        assert_eq!(
            registry.status_value(&replacement_id).await.unwrap()["state"],
            json!("cancelled")
        );
    }

    #[tokio::test]
    async fn queued_cancellation_reclaims_physical_admission_under_churn() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "ffeeddccbbaa99887766554433221100".to_string();
        let mut jobs = HashMap::new();
        jobs.insert(job_id.clone(), test_job_record(&job_id, JobState::Queued));
        let queue = Arc::new(JobQueue::new([job_id.clone()]));
        let registry = JobRegistry {
            inner: Arc::new(JobRegistryInner {
                workspace,
                blender: Arc::new(BlenderClient::new(
                    "queue-churn.invalid",
                    65532,
                    ClientOptions::default(),
                )),
                render_worker: None,
                isolation: RenderIsolation::Legacy,
                ffmpeg_bin: PathBuf::from("unused-ffmpeg"),
                geometry_worker_bin: None,
                geometry_worker_memory_bytes: DEFAULT_GEOMETRY_WORKER_MEMORY_BYTES,
                queue_depth: 1,
                state: Mutex::new(RegistryState {
                    jobs,
                    order: VecDeque::from([job_id.clone()]),
                }),
                admission: Arc::new(Mutex::new(())),
                persistence: Arc::new(Mutex::new(())),
                cancellations: Mutex::new(HashMap::from([(
                    job_id.clone(),
                    CancellationToken::new(),
                )])),
                queue: Arc::clone(&queue),
                recovery_integrity: RecoveryIntegrity::default(),
                encoder_health: OnceCell::new(),
                worker_verified: AtomicBool::new(false),
            }),
        };

        let cancelled = registry
            .cancel(RenderJobCancelParams { job_id })
            .await
            .expect("cancel queued job");
        assert_eq!(cancelled["state"], json!("cancelled"));

        for index in 0..=MAX_JOB_HISTORY {
            let replacement = format!("replacement-{index}");
            queue
                .push(replacement.clone(), 1)
                .expect("cancelled entries never consume physical capacity");
            queue.remove(&replacement);
        }
    }

    #[tokio::test]
    async fn queued_recovery_cancellation_keeps_the_restore_work_scheduled() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "00112233445566778899aabbccddeeff".to_string();
        let mut record = test_job_record(&job_id, JobState::Queued);
        record.progress.phase = "recovered_session_restore".to_string();
        record.session_checkpoint_captured = true;
        record.session_checkpoint_restored = false;
        let blender = Arc::new(BlenderClient::new(
            "queued-recovery-cancel.invalid",
            65530,
            ClientOptions::default(),
        ));
        blender.set_recovery_fenced(true);
        let inner = test_registry_inner(
            Arc::clone(&workspace),
            Arc::clone(&blender),
            job_id.clone(),
            record,
        );
        inner
            .queue
            .push(job_id.clone(), 1)
            .expect("schedule recovered job");
        inner
            .cancellations
            .lock()
            .await
            .insert(job_id.clone(), CancellationToken::new());
        let registry = JobRegistry {
            inner: Arc::clone(&inner),
        };

        let cancelled = registry
            .cancel(RenderJobCancelParams {
                job_id: job_id.clone(),
            })
            .await
            .expect("request recovered job cancellation");

        assert_eq!(cancelled["state"], json!("queued"));
        assert_eq!(cancelled["cancellation_requested"], json!(true));
        assert_eq!(
            cancelled["progress"]["phase"],
            json!("recovered_session_restore")
        );
        assert!(blender.recovery_fenced());
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), inner.queue.receive())
                .await
                .expect("recovery work remains scheduled"),
            Some(job_id.clone())
        );

        let (_, bytes) = workspace
            .read_artifact(&job_metadata_path(&job_id))
            .expect("durable cancellation request");
        let durable: JobRecord = serde_json::from_slice(&bytes).expect("durable job metadata");
        assert_eq!(durable.state, JobState::Queued);
        assert!(durable.cancellation_requested);
        assert!(durable.session_checkpoint_captured);
        assert!(!durable.session_checkpoint_restored);
    }

    #[tokio::test]
    async fn staging_owns_admission_until_cancellation_reaches_a_durable_boundary() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        workspace
            .write_artifact("scene.blend", b"checkpoint", false)
            .expect("source checkpoint");
        let registry = JobRegistry::new(
            Arc::clone(&workspace),
            Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
            PathBuf::from("unused-ffmpeg"),
            2,
        );
        let persistence = registry.inner.persistence.lock().await;
        let first_registry = registry.clone();
        let first = tokio::spawn(async move {
            first_registry
                .stage_submission(submit_params("scene.blend", "still"))
                .await
        });
        let first_job_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(job_id) = registry.inner.state.lock().await.order.front().cloned() {
                    break job_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first submission reaches durable staging");

        assert!(
            registry.inner.admission.try_lock().is_err(),
            "the detached staging continuation must own admission"
        );
        let cancelling_registry = registry.clone();
        let cancelling_job_id = first_job_id.clone();
        let cancelling = tokio::spawn(async move {
            cancelling_registry
                .cancel(RenderJobCancelParams {
                    job_id: cancelling_job_id,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .inner
                    .state
                    .lock()
                    .await
                    .jobs
                    .get(&first_job_id)
                    .is_some_and(|job| job.cancellation_requested)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation becomes visible during staging");
        {
            let state = registry.inner.state.lock().await;
            let job = state.jobs.get(&first_job_id).expect("staging job");
            assert_eq!(job.state, JobState::Cancelled);
            assert_eq!(job.progress.phase, "cancelled");
        }

        let second_registry = registry.clone();
        let second = tokio::spawn(async move {
            second_registry
                .stage_submission(submit_params("scene.blend", "still"))
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            registry.inner.state.lock().await.order.len(),
            1,
            "a later submission cannot enter the durable index snapshot"
        );
        drop(persistence);

        cancelling
            .await
            .expect("cancellation task")
            .expect("cancellation request");
        let first_status = first
            .await
            .expect("first submission task")
            .expect("first submission result");
        assert_eq!(first_status["state"], json!("cancelled"));
        second
            .await
            .expect("second submission task")
            .expect("second submission result");

        let (_, index_bytes) = workspace
            .read_artifact(INDEX_PATH)
            .expect("durable job index");
        let index: JobIndex =
            serde_json::from_slice(&index_bytes).expect("parse durable job index");
        for job_id in index.job_ids {
            workspace
                .read_artifact(&job_metadata_path(&job_id))
                .expect("every indexed job has durable metadata");
        }
    }

    #[test]
    fn admitted_submission_owns_snapshot_after_the_request_is_cancelled() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace tempdir");
            let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
            workspace
                .write_artifact("scene.blend", b"checkpoint", false)
                .expect("source checkpoint");
            let registry = JobRegistry::new(
                Arc::clone(&workspace),
                Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
                PathBuf::from("unused-ffmpeg"),
                1,
            );
            let (release, blocker) = occupy_only_blocking_thread().await;
            let submitting_registry = registry.clone();
            let mut submission = Box::pin(
                submitting_registry.stage_submission(submit_params("scene.blend", "still")),
            );
            std::future::poll_fn(|context| match submission.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => {
                    panic!("submission completed before source snapshotting: {result:?}")
                }
            })
            .await;

            drop(submission);
            assert!(
                registry.inner.admission.try_lock().is_err(),
                "the detached admitted operation must retain snapshot admission"
            );
            assert!(
                registry.inner.state.lock().await.order.is_empty(),
                "the blocked snapshot has not entered active job accounting yet"
            );
            release.send(()).expect("release blocking pool");
            blocker.await.expect("blocking pool task");

            let job_id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(job_id) = registry.inner.state.lock().await.order.front().cloned() {
                        break job_id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("detached submission completes its snapshot");
            let failed = wait_for_state(&registry, &job_id, JobState::Failed).await;
            assert_eq!(failed["failure"]["code"], json!("connect"));
            assert!(
                workspace
                    .read_artifact(&format!("{JOB_ROOT}/{job_id}/source.blend"))
                    .is_ok(),
                "the detached staging continuation commits the immutable source"
            );
            registry
                .stage_submission(submit_params("scene.blend", "still"))
                .await
                .expect("terminal continuation releases the only admission slot");
        });
    }

    #[tokio::test]
    async fn cancellation_during_source_staging_cannot_be_queued_or_reclassified() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let registry = JobRegistry::new(
            workspace,
            Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
            PathBuf::from("unused-ffmpeg"),
            1,
        );
        let (spec, kind, total_frames) =
            validate_submit(submit_params("scene.blend", "still")).expect("valid still job");
        let job_id = "00112233445566778899aabbccddeeff".to_string();
        let record = JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: JobExecution::LegacySession,
            job_id: job_id.clone(),
            kind,
            state: JobState::Cancelled,
            created_unix_ms: 1,
            updated_unix_ms: 2,
            source_artifact: format!("{JOB_ROOT}/{job_id}/source.blend"),
            source_snapshot: None,
            session_checkpoint_artifact: format!("{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            session_checkpoint_captured: false,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "cancelled".to_string(),
                completed_frames: 0,
                total_frames,
                current_frame: None,
                frame_bytes: 0,
            },
            cancellation_requested: true,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: None,
            failure: None,
        };
        registry
            .inner
            .state
            .lock()
            .await
            .jobs
            .insert(job_id.clone(), record);
        registry
            .inner
            .cancellations
            .lock()
            .await
            .insert(job_id.clone(), CancellationToken::new());

        assert!(
            !registry
                .inner
                .mark_waiting_for_enqueue(&job_id)
                .await
                .expect("staging completion recheck")
        );
        registry
            .inner
            .fail_job(&job_id, "source_commit", "source commit failed")
            .await;

        let state = registry.inner.state.lock().await;
        let job = state.jobs.get(&job_id).expect("job remains visible");
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(job.progress.phase, "cancelled");
        assert!(job.failure.is_none());
        drop(state);
        assert!(
            !registry
                .inner
                .cancellations
                .lock()
                .await
                .contains_key(&job_id),
            "terminal failure cleanup releases the cancellation token"
        );
    }

    #[tokio::test]
    async fn job_completion_and_cancellation_share_one_atomic_state_transition() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let registry = JobRegistry::new(
            workspace,
            Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
            PathBuf::from("unused-ffmpeg"),
            2,
        );
        let cancelled_id = "00112233445566778899aabbccddeeff";
        let succeeded_id = "ffeeddccbbaa99887766554433221100";
        let mut cancelled = test_job_record(cancelled_id, JobState::Running);
        cancelled.cancellation_requested = true;
        let succeeded = test_job_record(succeeded_id, JobState::Running);
        {
            let mut state = registry.inner.state.lock().await;
            state.jobs.insert(cancelled_id.to_string(), cancelled);
            state.jobs.insert(succeeded_id.to_string(), succeeded);
        }

        assert_eq!(
            registry
                .inner
                .complete_job(cancelled_id, None)
                .await
                .expect("completion observes prior cancellation"),
            JobState::Cancelled
        );
        assert_eq!(
            registry
                .inner
                .complete_job(succeeded_id, None)
                .await
                .expect("completion wins before later cancellation"),
            JobState::Succeeded
        );
        let late_cancel = registry
            .cancel(RenderJobCancelParams {
                job_id: succeeded_id.to_string(),
            })
            .await
            .expect("terminal cancellation request is a no-op");
        assert_eq!(late_cancel["state"], json!("succeeded"));
        assert_eq!(late_cancel["cancellation_requested"], json!(false));
    }

    #[tokio::test]
    async fn restored_job_recovery_never_reenters_blender() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let completed_id = "00112233445566778899aabbccddeeff";
        let failed_id = "ffeeddccbbaa99887766554433221100";
        let mut completed = test_job_record(completed_id, JobState::Running);
        completed.session_checkpoint_captured = true;
        completed.session_checkpoint_restored = true;
        completed.progress.completed_frames = completed.progress.total_frames;
        let mut failed = test_job_record(failed_id, JobState::Running);
        failed.session_checkpoint_captured = true;
        failed.session_checkpoint_restored = true;
        failed.failure = Some(JobFailure {
            code: "addon".to_string(),
            message: "render failed before session restoration".to_string(),
        });
        for record in [&completed, &failed] {
            workspace
                .write_reserved_artifact(
                    &job_metadata_path(&record.job_id),
                    &serde_json::to_vec(record).expect("record JSON"),
                    false,
                )
                .expect("job metadata");
        }
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![completed_id.to_string(), failed_id.to_string()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");
        let registry = JobRegistry::new(
            workspace,
            Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
            PathBuf::from("unused-ffmpeg"),
            2,
        );

        let completed = wait_for_state(&registry, completed_id, JobState::Succeeded).await;
        let failed = wait_for_state(&registry, failed_id, JobState::Failed).await;
        assert_eq!(completed["session_checkpoint_restored"], json!(true));
        assert_eq!(failed["session_checkpoint_restored"], json!(true));
        assert_eq!(failed["failure"]["code"], json!("addon"));
        assert_eq!(
            failed["failure"]["message"],
            json!("render failed before session restoration")
        );
    }

    #[tokio::test]
    async fn restart_retries_only_session_restoration_and_preserves_the_render_failure() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "abcdef0123456789abcdef0123456789";
        let mut record = test_job_record(job_id, JobState::Failed);
        record.session_checkpoint_captured = true;
        record.pending_blender_outcome = Some(PendingBlenderOutcome::Failed(JobFailure {
            code: "addon".to_string(),
            message: "deterministic render failure".to_string(),
        }));
        record.failure = Some(JobFailure {
            code: "session_restore_failed".to_string(),
            message: "Blender was temporarily unavailable".to_string(),
        });
        workspace
            .write_reserved_artifact(
                &job_metadata_path(job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.to_string()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");
        let render_attempts = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&render_attempts);
        let restore_attempts = Arc::new(AtomicUsize::new(0));
        let observed_restores = Arc::clone(&restore_attempts);
        let fake = FakeAddon::spawn(move |command, _params| match command.as_str() {
            "job_restore_checkpoint" => {
                observed_restores.fetch_add(1, Ordering::SeqCst);
                ResponseSpec::Success {
                    result: json!({"restored": true}),
                    addon_version: Some("test".to_string()),
                }
            }
            "job_render_still" => {
                observed_renders.fetch_add(1, Ordering::SeqCst);
                ResponseSpec::Error {
                    message: "rendering must not repeat during session recovery".to_string(),
                    traceback: None,
                    addon_version: Some("test".to_string()),
                }
            }
            _ => ResponseSpec::Error {
                message: format!("unexpected command: {command}"),
                traceback: None,
                addon_version: Some("test".to_string()),
            },
        })
        .await;
        let registry = JobRegistry::new(
            workspace,
            Arc::new(BlenderClient::new(
                fake.host(),
                fake.port(),
                ClientOptions::default(),
            )),
            PathBuf::from("unused-ffmpeg"),
            1,
        );

        let failed = wait_for_state(&registry, job_id, JobState::Failed).await;
        assert_eq!(failed["session_checkpoint_restored"], json!(true));
        assert!(failed["pending_blender_outcome"].is_null());
        assert_eq!(failed["failure"]["code"], json!("addon"));
        assert_eq!(
            failed["failure"]["message"],
            json!("deterministic render failure")
        );
        assert_eq!(render_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(restore_attempts.load(Ordering::SeqCst), 1);
        assert!(!registry.inner.blender.recovery_fenced());
    }

    #[test]
    fn encoder_cleanup_failures_are_not_reclassified_as_clean_cancellation() {
        assert!(is_clean_cancellation(&RunFailure::new(
            "cancelled",
            "encoding was cancelled"
        )));
        assert!(!is_clean_cancellation(&RunFailure::new(
            "encoder_cleanup",
            "failed to reap FFmpeg"
        )));
    }

    #[test]
    fn pending_blender_failure_is_carried_into_the_restored_boundary() {
        let outcome = PendingBlenderOutcome::Failed(JobFailure {
            code: "addon".to_string(),
            message: "render failed".to_string(),
        });
        let failure = outcome.failure().expect("failed outcome remains failed");
        assert_eq!(failure.code, "addon");
        assert_eq!(failure.message, "render failed");
        assert!(PendingBlenderOutcome::Completed.failure().is_none());
        assert!(PendingBlenderOutcome::Cancelled.failure().is_none());
    }

    #[test]
    fn mp4_validation_rejects_successful_but_truncated_encoder_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let invalid = temp.path().join("invalid.mp4");
        std::fs::write(&invalid, b"\0\0\0\x20ftypshort").expect("write invalid mp4");
        let failure = validate_mp4(&invalid, 1024).expect_err("truncated MP4 rejected");
        assert_eq!(failure.code, "encoder_invalid_output");
    }

    #[test]
    fn video_validation_rejects_empty_or_truncated_frame_sequences() {
        let no_frames = b"frame=0\nout_time_us=0\nprogress=end\n";
        let failure = validate_video_progress(no_frames, 2, 30)
            .expect_err("empty decoded frame sequence rejected");
        assert_eq!(failure.code, "encoder_invalid_output");
        assert!(failure.message.contains("decoded 0 frames; expected 2"));

        let short_duration = b"frame=2\nout_time_us=1\nprogress=end\n";
        let failure = validate_video_progress(short_duration, 2, 30)
            .expect_err("truncated decoded duration rejected");
        assert_eq!(failure.code, "encoder_invalid_output");
        assert!(failure.message.contains("duration"));

        let one_frame_timestamp_tolerance = b"frame=2\nout_time_us=33332\nprogress=end\n";
        validate_video_progress(one_frame_timestamp_tolerance, 2, 30)
            .expect("one frame of timestamp ambiguity is accepted");
    }

    #[tokio::test]
    async fn encoder_readiness_requires_a_successful_libx264_encode_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let capable = temp.path().join("capable-ffmpeg");
        std::fs::write(
            &capable,
            "#!/bin/sh\nencoder=0\nsource=0\nfor argument in \"$@\"; do\n  [ \"$argument\" = \"libx264\" ] && encoder=1\n  [ \"$argument\" = \"color=c=black:s=2x2:r=1\" ] && source=1\ndone\n[ \"$encoder\" = 1 ] && [ \"$source\" = 1 ]\n",
        )
        .expect("write capable fake");
        let version_only = temp.path().join("version-only-ffmpeg");
        std::fs::write(
            &version_only,
            "#!/bin/sh\n[ \"$1\" = \"-version\" ] && exit 0\nexit 9\n",
        )
        .expect("write version-only fake");
        for binary in [&capable, &version_only] {
            let mut permissions = std::fs::metadata(binary).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(binary, permissions).expect("make fake executable");
        }

        let available = probe_encoder(capable).await;
        assert_eq!(available["available"], json!(true));
        assert_eq!(available["encoder"], json!("libx264"));
        let unavailable = probe_encoder(version_only).await;
        assert_eq!(unavailable["available"], json!(false));
        assert!(
            unavailable["error"]
                .as_str()
                .expect("probe error")
                .contains("libx264 encode probe")
        );
    }

    #[test]
    fn turntable_dimensions_observe_the_backend_review_surface_limit() {
        let mut too_large = submit_params("scene.blend", "turntable");
        too_large.width = 4096;
        too_large.height = 4096;
        assert_eq!(
            validate_submit(too_large).unwrap_err().to_string(),
            format!(
                "invalid arguments: turntable frame dimensions must contain at most {MAX_REVIEW_SOURCE_PIXELS} pixels"
            )
        );

        let mut boundary = submit_params("scene.blend", "turntable");
        boundary.width = 4096;
        boundary.height = 2048;
        validate_submit(boundary).expect("backend surface boundary is usable");

        let mut still = submit_params("scene.blend", "still");
        still.width = 4096;
        still.height = 4096;
        validate_submit(still).expect("single stills do not use the review-view backend");
    }

    #[test]
    fn job_failures_distinguish_lane_contention_and_gpu_exhaustion() {
        let busy = blender_failure(printable_blender::BlenderError::Timeout {
            phase: printable_blender::Phase::Lock,
        });
        assert_eq!(busy.code, "blender_busy");
        let oom = blender_failure(printable_blender::BlenderError::Addon {
            scene_state: None,
            message: "CUDA_ERROR_OUT_OF_MEMORY while allocating render buffer".to_string(),
        });
        assert_eq!(oom.code, "gpu_out_of_memory");
    }

    #[test]
    fn running_job_recovers_at_the_next_unfinished_frame() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Workspace::open(Some(temp.path()), None).expect("workspace");
        let mut params = submit_params("scene.blend", "animation");
        params.presentation = Some(
            serde_json::from_value(json!({
                "profile": "studio_dark"
            }))
            .expect("product presentation"),
        );
        params.auto_frame_sequence = true;
        params.auto_frame_sequence_timeout_seconds = 25.0;
        let (spec, kind, total_frames) = validate_submit(params).expect("valid animation");
        let job_id = "00112233445566778899aabbccddeeff".to_string();
        let presentation_bounds = PresentationBounds {
            minimum: [0.0, 0.0, 0.0],
            maximum: [4.0, 2.0, 1.0],
            dimensions: [4.0, 2.0, 1.0],
            center: [2.0, 1.0, 0.5],
            diagonal: 21.0_f64.sqrt(),
            coordinate_space: "world".to_string(),
            unit: "blender_unit".to_string(),
        };
        let record = JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: JobExecution::LegacySession,
            job_id: job_id.clone(),
            kind,
            state: JobState::Running,
            created_unix_ms: 1,
            updated_unix_ms: 2,
            source_artifact: format!("{JOB_ROOT}/{job_id}/source.blend"),
            source_snapshot: None,
            session_checkpoint_artifact: format!("{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            session_checkpoint_captured: true,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "rendering".to_string(),
                completed_frames: 7,
                total_frames,
                current_frame: Some(8),
                frame_bytes: 70,
            },
            cancellation_requested: false,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: Some(presentation_bounds.clone()),
            failure: None,
        };
        workspace
            .write_reserved_artifact(
                &job_metadata_path(&job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.clone()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");

        let (state, recovered, integrity) = recover_state(&workspace, 2);
        let job = state.jobs.get(&job_id).expect("recovered job");
        assert_eq!(recovered, vec![job_id]);
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.progress.completed_frames, 7);
        assert_eq!(job.progress.current_frame, None);
        assert_eq!(job.progress.phase, "recovered_session_restore");
        assert!(!job.cancellation_requested);
        assert_eq!(job.recovery_count, 1);
        assert_eq!(
            serde_json::to_value(
                job.spec
                    .presentation
                    .as_ref()
                    .expect("presentation survives restart")
                    .profile
            )
            .expect("profile JSON"),
            json!("studio_dark")
        );
        assert!(job.spec.auto_frame_sequence);
        assert_eq!(job.spec.auto_frame_sequence_timeout_seconds, 25.0);
        assert_eq!(
            job.presentation_bounds.as_ref().expect("stable bounds"),
            &presentation_bounds
        );
        let replay = render_params(job, 7, 1024).expect("replayed render request");
        assert_eq!(replay["presentation"]["profile"], json!("studio_dark"));
        assert_eq!(replay["camera_behavior"], json!("bounds"));
        assert_eq!(
            replay["framing_bounds"],
            serde_json::to_value(&presentation_bounds).expect("bounds JSON")
        );
        assert!(!integrity.blocked());
    }

    #[test]
    fn recovered_presentation_metadata_fails_closed_without_rejecting_unstarted_work() {
        let mut params = submit_params("scene.blend", "animation");
        params.presentation = Some(
            serde_json::from_value(json!({
                "profile": "studio_dark"
            }))
            .expect("product presentation"),
        );
        params.auto_frame_sequence = true;
        let (spec, kind, total_frames) = validate_submit(params).expect("valid animation");
        let mut job = test_job_record("00112233445566778899aabbccddeeff", JobState::Queued);
        job.kind = kind;
        job.spec = spec;
        job.progress.total_frames = total_frames;

        validate_recovered_presentation(&job).expect("unstarted presentation needs no bounds");

        let mut started_without_bounds = job.clone();
        started_without_bounds.progress.completed_frames = 2;
        assert!(validate_recovered_presentation(&started_without_bounds).is_err());

        let valid_bounds = PresentationBounds {
            minimum: [0.0, 0.0, 0.0],
            maximum: [4.0, 2.0, 1.0],
            dimensions: [4.0, 2.0, 1.0],
            center: [2.0, 1.0, 0.5],
            diagonal: 21.0_f64.sqrt(),
            coordinate_space: "world".to_string(),
            unit: "blender_unit".to_string(),
        };
        let mut started_with_bounds = started_without_bounds.clone();
        started_with_bounds.presentation_bounds = Some(valid_bounds.clone());
        validate_recovered_presentation(&started_with_bounds)
            .expect("started presentation retains its framing contract");

        let mut legacy_with_bounds = job.clone();
        legacy_with_bounds.spec.presentation = None;
        legacy_with_bounds.presentation_bounds = Some(valid_bounds.clone());
        assert!(validate_recovered_presentation(&legacy_with_bounds).is_err());

        let mut still_with_bounds =
            test_job_record("ffeeddccbbaa99887766554433221100", JobState::Queued);
        still_with_bounds.spec.presentation = job.spec.presentation.clone();
        still_with_bounds.presentation_bounds = Some(valid_bounds.clone());
        validate_recovered_presentation(&still_with_bounds)
            .expect("unstarted static presentation may retain measured bounds");

        let mut turntable_with_bounds = still_with_bounds.clone();
        turntable_with_bounds.kind = RenderJobKind::Turntable;
        turntable_with_bounds.spec.turntable_frames = 2;
        turntable_with_bounds.progress.completed_frames = 1;
        turntable_with_bounds.progress.total_frames = 2;
        validate_recovered_presentation(&turntable_with_bounds)
            .expect("partially completed turntable retains stable bounds");
        let replay =
            render_params(&turntable_with_bounds, 1, 1024).expect("turntable replay params");
        assert_eq!(replay["camera_behavior"], json!("bounds"));
        assert_eq!(
            replay["framing_bounds"],
            serde_json::to_value(&valid_bounds).expect("bounds JSON")
        );

        let mut completed_without_bounds = turntable_with_bounds;
        completed_without_bounds.presentation_bounds = None;
        assert!(validate_recovered_presentation(&completed_without_bounds).is_err());

        let mut invalid_bounds = job;
        invalid_bounds.presentation_bounds = Some(PresentationBounds {
            coordinate_space: "local".to_string(),
            ..valid_bounds
        });
        assert!(validate_recovered_presentation(&invalid_bounds).is_err());
    }

    #[test]
    fn restart_preserves_a_running_jobs_persisted_cancellation_request() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Workspace::open(Some(temp.path()), None).expect("workspace");
        let (spec, kind, total_frames) =
            validate_submit(submit_params("scene.blend", "animation")).expect("valid job");
        let job_id = "ffeeddccbbaa99887766554433221100".to_string();
        let record = JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: JobExecution::LegacySession,
            job_id: job_id.clone(),
            kind,
            state: JobState::Running,
            created_unix_ms: 1,
            updated_unix_ms: 2,
            source_artifact: format!("{JOB_ROOT}/{job_id}/source.blend"),
            source_snapshot: None,
            session_checkpoint_artifact: format!("{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            session_checkpoint_captured: false,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "rendering".to_string(),
                completed_frames: 3,
                total_frames,
                current_frame: Some(4),
                frame_bytes: 30,
            },
            cancellation_requested: true,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: None,
            failure: None,
        };
        let mut legacy_record = serde_json::to_value(&record).expect("legacy job JSON");
        let legacy_object = legacy_record.as_object_mut().expect("job object");
        legacy_object.remove("presentation_bounds");
        let legacy_spec = legacy_object["spec"].as_object_mut().expect("job spec");
        legacy_spec.remove("presentation");
        legacy_spec.remove("auto_frame_sequence");
        legacy_spec.remove("auto_frame_sequence_timeout_seconds");
        workspace
            .write_reserved_artifact(
                &job_metadata_path(&job_id),
                &serde_json::to_vec(&legacy_record).unwrap(),
                false,
            )
            .unwrap();
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.clone()],
                })
                .unwrap(),
                false,
            )
            .unwrap();

        let (state, recovered, integrity) = recover_state(&workspace, 2);
        let job = state.jobs.get(&job_id).expect("recovered cancellation");
        assert!(recovered.is_empty());
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(job.progress.phase, "cancelled");
        assert_eq!(job.progress.completed_frames, 3);
        assert!(job.cancellation_requested);
        assert_eq!(job.recovery_count, 1);
        assert!(job.spec.presentation.is_none());
        assert!(!job.spec.auto_frame_sequence);
        assert_eq!(
            job.spec.auto_frame_sequence_timeout_seconds,
            default_render_timeout_seconds()
        );
        assert!(job.presentation_bounds.is_none());
        assert!(!integrity.blocked());
    }

    #[test]
    fn restart_requeues_cancellation_until_the_live_session_is_restored() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Workspace::open(Some(temp.path()), None).expect("workspace");
        let job_id = "ffeeddccbbaa99887766554433221100";
        let mut record = test_job_record(job_id, JobState::Running);
        record.cancellation_requested = true;
        record.session_checkpoint_captured = true;
        record.session_checkpoint_restored = false;
        workspace
            .write_reserved_artifact(
                &job_metadata_path(job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.to_string()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");

        let (state, recovered, integrity) = recover_state(&workspace, 1);
        let recovered_job = state.jobs.get(job_id).expect("recovered job");
        assert_eq!(recovered, vec![job_id.to_string()]);
        assert_eq!(recovered_job.state, JobState::Queued);
        assert_eq!(recovered_job.progress.phase, "recovered_session_restore");
        assert!(recovered_job.cancellation_requested);
        assert!(recovered_job.session_checkpoint_captured);
        assert!(!recovered_job.session_checkpoint_restored);
        assert!(!integrity.blocked());
    }

    #[test]
    fn restart_requeues_a_terminal_job_when_session_restore_is_still_required() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Workspace::open(Some(temp.path()), None).expect("workspace");
        let job_id = "00112233445566778899aabbccddeeff";
        let mut record = test_job_record(job_id, JobState::Failed);
        record.session_checkpoint_captured = true;
        record.session_checkpoint_restored = false;
        record.failure = Some(JobFailure {
            code: "session_restore_failed".to_string(),
            message: "Blender was temporarily unavailable".to_string(),
        });
        workspace
            .write_reserved_artifact(
                &job_metadata_path(job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.to_string()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");

        let (state, recovered, integrity) = recover_state(&workspace, 1);
        let recovered_job = state.jobs.get(job_id).expect("recovered job");
        assert_eq!(recovered, vec![job_id]);
        assert_eq!(recovered_job.state, JobState::Queued);
        assert_eq!(recovered_job.progress.phase, "recovered_session_restore");
        assert_eq!(recovered_job.recovery_count, 1);
        assert!(recovered_job.session_checkpoint_captured);
        assert!(!recovered_job.session_checkpoint_restored);
        assert!(!integrity.blocked());
    }

    #[tokio::test]
    async fn restart_reconstructs_the_fence_before_recovery_is_scheduled() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Arc::new(Workspace::open(Some(temp.path()), None).expect("workspace"));
        let job_id = "ffeeddccbbaa99887766554433221100";
        let mut record = test_job_record(job_id, JobState::Running);
        record.session_checkpoint_captured = true;
        record.session_checkpoint_restored = false;
        workspace
            .write_reserved_artifact(
                &job_metadata_path(job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec![job_id.to_string()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");
        let blender = Arc::new(BlenderClient::new(
            "job-recovery-fence.invalid",
            65533,
            ClientOptions::default(),
        ));

        let _registry = JobRegistry::new(
            workspace,
            Arc::clone(&blender),
            PathBuf::from("unused-ffmpeg"),
            1,
        );

        assert!(blender.recovery_fenced());
        blender.set_recovery_fenced(false);
    }

    #[tokio::test]
    async fn recovery_integrity_block_rejects_submissions_and_schedules_no_jobs() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let workspace = Workspace::open(Some(temp.path()), None).expect("workspace");
        let (spec, kind, total_frames) =
            validate_submit(submit_params("scene.blend", "animation")).expect("valid job");
        let job_id = "0123456789abcdef0123456789abcdef".to_string();
        let record = JobRecord {
            schema_version: JOB_SCHEMA_VERSION,
            execution: JobExecution::LegacySession,
            job_id: job_id.clone(),
            kind,
            state: JobState::Running,
            created_unix_ms: 1,
            updated_unix_ms: 2,
            source_artifact: format!("{JOB_ROOT}/{job_id}/source.blend"),
            source_snapshot: None,
            session_checkpoint_artifact: format!("{JOB_ROOT}/{job_id}/pre-job-session.blend"),
            session_checkpoint_captured: false,
            session_checkpoint_restored: false,
            pending_blender_outcome: None,
            spec,
            progress: JobProgress {
                phase: "rendering".to_string(),
                completed_frames: 0,
                total_frames,
                current_frame: Some(1),
                frame_bytes: 0,
            },
            cancellation_requested: false,
            recovery_count: 0,
            video_artifact: None,
            mechanical_analysis: None,
            presentation_bounds: None,
            failure: None,
        };
        workspace
            .write_reserved_artifact(
                &job_metadata_path(&job_id),
                &serde_json::to_vec(&record).expect("record JSON"),
                false,
            )
            .expect("job metadata");
        workspace
            .write_reserved_artifact(
                INDEX_PATH,
                &serde_json::to_vec(&JobIndex {
                    schema_version: JOB_SCHEMA_VERSION,
                    job_ids: vec!["../../outside".to_string(), job_id.clone()],
                })
                .expect("index JSON"),
                false,
            )
            .expect("job index");

        let (state, recovered, integrity) = recover_state(&workspace, 2);
        assert_eq!(state.order, VecDeque::from([job_id.clone()]));
        assert_eq!(recovered, vec![job_id.clone()]);
        assert!(integrity.blocked());

        let workspace = Arc::new(workspace);
        let blender = Arc::new(BlenderClient::new(
            "corrupt-recovery.invalid",
            65531,
            ClientOptions::default(),
        ));
        let registry = JobRegistry::new(
            workspace,
            Arc::clone(&blender),
            PathBuf::from("unused-ffmpeg"),
            2,
        );
        assert!(blender.recovery_fenced());
        assert_eq!(
            registry.inner.recovery_integrity.report()["status"],
            json!("blocked")
        );
        tokio::task::yield_now().await;
        assert_eq!(
            registry.status_value(&job_id).await.expect("job status")["state"],
            json!("queued"),
            "integrity-blocked recovery work must not reach the Blender bypass"
        );
        let error = registry
            .stage_submission(submit_params("scene.blend", "still"))
            .await
            .expect_err("integrity block rejects new durable jobs");
        assert_eq!(error.code(), "job");
        assert!(error.to_string().contains("recovery metadata is blocked"));
        assert_eq!(registry.inner.state.lock().await.jobs.len(), 1);
        blender.set_recovery_fenced(false);
    }

    #[test]
    fn animation_frame_count_honors_the_complete_blender_range_and_step() {
        let stepped = serde_json::from_value::<RenderJobSubmitParams>(json!({
            "source_blend": "scene.blend",
            "kind": "animation",
            "frame_start": -1048574,
            "frame_end": 1048574,
            "frame_step": 3,
        }))
        .expect("stepped params");
        let (_spec, _kind, total) = validate_submit(stepped).expect("stepped range works");
        assert_eq!(total, 699_050);
    }

    #[test]
    fn mechanical_rotation_submit_requires_a_disjoint_finite_motion_contract() {
        let valid_contract = json!({
            "fixed_objects": ["Base"],
            "moving_objects": ["Leaf"],
            "pivot_mm": [1.0, 2.0, 3.0],
            "axis": [0.0, 0.0, 2.0],
            "angle_degrees": 90.0,
            "target_clearance_mm": 0.2,
            "max_analysis_mesh_bytes": 4096
        });
        let valid = serde_json::from_value::<RenderJobSubmitParams>(json!({
            "source_blend": "scene.blend",
            "kind": "mechanical_rotation",
            "mechanical_rotation": valid_contract.clone(),
            "frame_start": 2,
            "frame_end": 6,
            "frame_step": 2
        }))
        .expect("typed mechanical params");
        let (spec, kind, total) = validate_submit(valid).expect("valid mechanical job");
        assert_eq!(kind, RenderJobKind::MechanicalRotation);
        assert_eq!(total, 3);
        assert_eq!(
            spec.mechanical_rotation
                .expect("mechanical contract")
                .target_clearance_mm,
            0.2
        );

        for arguments in [
            json!({
                "source_blend": "scene.blend",
                "kind": "mechanical_rotation"
            }),
            json!({
                "source_blend": "scene.blend",
                "kind": "animation",
                "mechanical_rotation": valid_contract.clone()
            }),
            json!({
                "source_blend": "scene.blend",
                "kind": "mechanical_rotation",
                "mechanical_rotation": {
                    "fixed_objects": ["Shared"],
                    "moving_objects": ["Shared"],
                    "pivot_mm": [0.0, 0.0, 0.0],
                    "axis": [0.0, 0.0, 1.0],
                    "angle_degrees": 90.0,
                    "target_clearance_mm": 0.0
                }
            }),
        ] {
            let params = serde_json::from_value(arguments).expect("typed invalid params");
            assert_eq!(validate_submit(params).unwrap_err().code(), "validation");
        }
    }

    #[test]
    fn mechanical_rotation_validation_enforces_collection_and_numeric_boundaries() {
        let valid = || MechanicalRotationSpec {
            fixed_objects: vec!["F".repeat(255)],
            moving_objects: vec!["Leaf".to_string()],
            pivot_mm: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
            angle_degrees: 90.0,
            target_clearance_mm: 0.0,
            max_analysis_mesh_bytes: MAX_BLENDER_ARTIFACT_BYTES,
        };
        validate_mechanical_rotation(&valid()).expect("exact boundaries are valid");
        let mut maximum_set = valid();
        maximum_set.fixed_objects = (0..1000).map(|index| format!("Fixed{index}")).collect();
        validate_mechanical_rotation(&maximum_set).expect("maximum object set is valid");

        let mut invalid_cases = Vec::new();
        let mut empty_set = valid();
        empty_set.fixed_objects.clear();
        invalid_cases.push(empty_set);
        let mut too_many = valid();
        too_many.fixed_objects = (0..=1000).map(|index| format!("Fixed{index}")).collect();
        invalid_cases.push(too_many);
        let mut empty_name = valid();
        empty_name.fixed_objects = vec![String::new()];
        invalid_cases.push(empty_name);
        let mut long_name = valid();
        long_name.fixed_objects = vec!["F".repeat(256)];
        invalid_cases.push(long_name);
        let mut duplicates = valid();
        duplicates.fixed_objects = vec!["Base".to_string(), "Base".to_string()];
        invalid_cases.push(duplicates);
        let mut overlap = valid();
        overlap.fixed_objects = vec!["Leaf".to_string()];
        invalid_cases.push(overlap);
        let mut invalid_pivot = valid();
        invalid_pivot.pivot_mm[0] = f64::NAN;
        invalid_cases.push(invalid_pivot);
        let mut invalid_axis = valid();
        invalid_axis.axis[0] = f64::INFINITY;
        invalid_cases.push(invalid_axis);
        let mut zero_axis = valid();
        zero_axis.axis = [0.0; 3];
        invalid_cases.push(zero_axis);
        let mut invalid_angle = valid();
        invalid_angle.angle_degrees = f64::NAN;
        invalid_cases.push(invalid_angle);
        let mut zero_angle = valid();
        zero_angle.angle_degrees = 0.0;
        invalid_cases.push(zero_angle);
        let mut invalid_clearance = valid();
        invalid_clearance.target_clearance_mm = f64::NAN;
        invalid_cases.push(invalid_clearance);
        let mut negative_clearance = valid();
        negative_clearance.target_clearance_mm = -1.0;
        invalid_cases.push(negative_clearance);
        let mut zero_budget = valid();
        zero_budget.max_analysis_mesh_bytes = 0;
        invalid_cases.push(zero_budget);

        for invalid in invalid_cases {
            assert_eq!(
                validate_mechanical_rotation(&invalid)
                    .expect_err("invalid mechanical contract")
                    .code(),
                "validation"
            );
        }
    }

    #[test]
    fn mechanical_part_summary_integrity_checks_every_independent_boundary() {
        let valid = typed_mechanical_report().fixed;
        assert!(valid_assembly_part(&valid));
        assert!(valid_assembly_part(&AssemblyPartSummary {
            vertices: 4,
            triangles: 4,
            ..valid.clone()
        }));

        let mut invalid = vec![
            AssemblyPartSummary {
                vertices: 3,
                ..valid.clone()
            },
            AssemblyPartSummary {
                triangles: 3,
                ..valid.clone()
            },
            AssemblyPartSummary {
                volume_mm3: 0.0,
                ..valid.clone()
            },
            AssemblyPartSummary {
                volume_mm3: f64::NAN,
                ..valid.clone()
            },
        ];
        let mut zero_extent = valid.clone();
        zero_extent.bounds.maximum_mm[0] = zero_extent.bounds.minimum_mm[0];
        zero_extent.bounds.dimensions_mm[0] = 0.0;
        invalid.push(zero_extent);
        let mut mismatched_dimension = valid.clone();
        mismatched_dimension.bounds.dimensions_mm[0] = 2.0;
        invalid.push(mismatched_dimension);

        for part in invalid {
            assert!(!valid_assembly_part(&part));
        }
    }

    #[test]
    fn mechanical_static_report_integrity_checks_each_relation_and_derived_field() {
        let complete = typed_mechanical_report();
        let fixed = complete.fixed;
        let moving = complete.moving;
        let separated = complete.static_analysis;
        assert!(valid_static_analysis(&separated, &fixed, &moving));

        let mut contact = separated.clone();
        contact.relation = AssemblyRelation::Contact;
        contact.clearance_mm = 0.0;
        contact.surface_gap_mm = 0.0;
        contact.closest_surface_points = None;
        contact.meets_required_clearance = Some(false);
        assert!(valid_static_analysis(&contact, &fixed, &moving));

        let mut interfering = contact.clone();
        interfering.relation = AssemblyRelation::Interfering;
        interfering.interference_volume_mm3 = 1.0;
        interfering.fixed_interference_fraction = 1.0;
        interfering.moving_interference_fraction = 1.0;
        assert!(valid_static_analysis(&interfering, &fixed, &moving));

        let mut smaller_fixed = fixed.clone();
        smaller_fixed.volume_mm3 = 0.5;
        let mut fixed_fraction_too_large = interfering.clone();
        fixed_fraction_too_large.interference_volume_mm3 = 0.6;
        fixed_fraction_too_large.fixed_interference_fraction = 1.2;
        fixed_fraction_too_large.moving_interference_fraction = 0.6;
        assert!(!valid_static_analysis(
            &fixed_fraction_too_large,
            &smaller_fixed,
            &moving
        ));
        let mut smaller_moving = moving.clone();
        smaller_moving.volume_mm3 = 0.5;
        let mut moving_fraction_too_large = interfering.clone();
        moving_fraction_too_large.interference_volume_mm3 = 0.6;
        moving_fraction_too_large.fixed_interference_fraction = 0.6;
        moving_fraction_too_large.moving_interference_fraction = 1.2;
        assert!(!valid_static_analysis(
            &moving_fraction_too_large,
            &fixed,
            &smaller_moving
        ));

        let mut larger_fixed = fixed.clone();
        larger_fixed.volume_mm3 = 4.0;
        let mut larger_moving = moving.clone();
        larger_moving.volume_mm3 = 2.0;
        let mut fractional = interfering.clone();
        fractional.fixed_interference_fraction = 0.25;
        fractional.moving_interference_fraction = 0.5;
        assert!(valid_static_analysis(
            &fractional,
            &larger_fixed,
            &larger_moving
        ));

        let fraction_tolerance = 1.0 + 1e-9;
        let mut tolerance_boundary = interfering.clone();
        tolerance_boundary.interference_volume_mm3 = fraction_tolerance;
        tolerance_boundary.fixed_interference_fraction = fraction_tolerance;
        tolerance_boundary.moving_interference_fraction = fraction_tolerance;
        assert!(valid_static_analysis(&tolerance_boundary, &fixed, &moving));
        tolerance_boundary.interference_volume_mm3 = 1.0 + 2e-9;
        tolerance_boundary.fixed_interference_fraction = 1.0 + 2e-9;
        tolerance_boundary.moving_interference_fraction = 1.0 + 2e-9;
        assert!(!valid_static_analysis(&tolerance_boundary, &fixed, &moving));

        let mut invalid = Vec::new();
        let mut value = separated.clone();
        value.interference_volume_mm3 = 0.1;
        value.fixed_interference_fraction = 0.1;
        value.moving_interference_fraction = 0.1;
        invalid.push(value);
        let mut value = separated.clone();
        value.clearance_mm = 0.5;
        invalid.push(value);
        let mut value = separated.clone();
        value.surface_gap_mm = 0.0;
        value.clearance_mm = 0.0;
        value.closest_surface_points = None;
        value.meets_required_clearance = Some(false);
        invalid.push(value);
        let mut value = separated.clone();
        value.closest_surface_points = None;
        invalid.push(value);
        let mut value = separated.clone();
        value.closest_surface_points.as_mut().unwrap().moving_mm[0] = f64::INFINITY;
        invalid.push(value);
        let mut value = separated.clone();
        value.required_clearance_mm = None;
        invalid.push(value);
        let mut value = separated.clone();
        value.required_clearance_mm = Some(f64::NAN);
        value.meets_required_clearance = Some(false);
        invalid.push(value);
        let mut value = separated.clone();
        value.required_clearance_mm = Some(-1.0);
        value.meets_required_clearance = Some(true);
        invalid.push(value);
        let mut value = contact.clone();
        value.interference_volume_mm3 = 0.1;
        value.fixed_interference_fraction = 0.1;
        value.moving_interference_fraction = 0.1;
        invalid.push(value);
        let mut value = contact.clone();
        value.surface_gap_mm = 1.0;
        value.closest_surface_points = separated.closest_surface_points.clone();
        invalid.push(value);
        let mut value = contact.clone();
        value.clearance_mm = 1.0;
        invalid.push(value);
        let mut value = interfering.clone();
        value.interference_volume_mm3 = 0.0;
        value.fixed_interference_fraction = 0.0;
        value.moving_interference_fraction = 0.0;
        invalid.push(value);
        let mut value = interfering.clone();
        value.clearance_mm = 1.0;
        invalid.push(value);

        for static_analysis in invalid {
            assert!(!valid_static_analysis(&static_analysis, &fixed, &moving));
        }

        let mut no_requirement = separated.clone();
        no_requirement.required_clearance_mm = None;
        no_requirement.meets_required_clearance = None;
        assert!(valid_static_analysis(&no_requirement, &fixed, &moving));
        let mut unmet_requirement = separated;
        unmet_requirement.required_clearance_mm = Some(2.0);
        unmet_requirement.meets_required_clearance = Some(false);
        assert!(valid_static_analysis(&unmet_requirement, &fixed, &moving));
    }

    #[test]
    fn mechanical_rotation_report_integrity_checks_pass_and_blocked_shapes() {
        let complete = typed_mechanical_report();
        let separated = complete.static_analysis;
        let passing = complete.rotation.unwrap();
        assert!(valid_rotation_report(&passing, &separated));

        let mut zero_target = passing.clone();
        zero_target.target_clearance_mm = 0.0;
        assert!(valid_rotation_report(&zero_target, &separated));
        let mut minimum_at_end = passing.clone();
        minimum_at_end.minimum_certified_clearance_mm = minimum_at_end.clearance_at_end_mm;
        assert!(valid_rotation_report(&minimum_at_end, &separated));

        let mut invalid_passing = Vec::new();
        let mut value = passing.clone();
        value.pivot_mm[0] = f64::NAN;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.axis[0] = f64::NAN;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.angle_degrees = f64::NAN;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.angle_degrees = 0.0;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.target_clearance_mm = f64::NAN;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.target_clearance_mm = -1.0;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.retained = true;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.first_limit_interval_degrees = Some([1.0, 2.0]);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.block_reason = Some(MotionBlockReason::Contact);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.clearance_at_end_mm = None;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.clearance_at_end_mm = Some(f64::NAN);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.clearance_at_end_mm = Some(value.target_clearance_mm);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.minimum_certified_clearance_mm = None;
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.minimum_certified_clearance_mm = Some(f64::NAN);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.minimum_certified_clearance_mm = Some(value.target_clearance_mm);
        invalid_passing.push(value);
        let mut value = passing.clone();
        value.minimum_certified_clearance_mm = Some(0.4);
        invalid_passing.push(value);
        for rotation in invalid_passing {
            assert!(!valid_rotation_report(&rotation, &separated));
        }
        let mut wrong_static = separated.clone();
        wrong_static.relation = AssemblyRelation::Contact;
        assert!(!valid_rotation_report(&passing, &wrong_static));
        let mut boundary_static = separated.clone();
        boundary_static.clearance_mm = passing.target_clearance_mm;
        assert!(!valid_rotation_report(&passing, &boundary_static));
        let mut start_below_certified_minimum = separated.clone();
        start_below_certified_minimum.clearance_mm =
            passing.minimum_certified_clearance_mm.unwrap() - 0.01;
        assert!(!valid_rotation_report(
            &passing,
            &start_below_certified_minimum
        ));

        let blocked = RotationalMotionReport {
            can_rotate_full_angle: false,
            retained: true,
            first_limit_interval_degrees: Some([45.0, 46.0]),
            block_reason: Some(MotionBlockReason::Contact),
            clearance_at_end_mm: None,
            minimum_certified_clearance_mm: None,
            ..passing.clone()
        };
        assert!(valid_rotation_report(&blocked, &separated));
        let not_certified = RotationalMotionReport {
            retained: false,
            block_reason: Some(MotionBlockReason::ClearanceNotCertified),
            ..blocked.clone()
        };
        assert!(valid_rotation_report(&not_certified, &separated));

        let mut invalid_blocked = Vec::new();
        let mut value = blocked.clone();
        value.first_limit_interval_degrees = None;
        invalid_blocked.push(value);
        let mut value = blocked.clone();
        value.block_reason = None;
        invalid_blocked.push(value);
        for interval in [[f64::NAN, 1.0], [-1.0, 1.0], [47.0, 46.0], [45.0, 91.0]] {
            let mut value = blocked.clone();
            value.first_limit_interval_degrees = Some(interval);
            invalid_blocked.push(value);
        }
        let mut value = blocked.clone();
        value.retained = false;
        invalid_blocked.push(value);
        let mut value = blocked.clone();
        value.clearance_at_end_mm = Some(0.3);
        invalid_blocked.push(value);
        let mut value = blocked.clone();
        value.minimum_certified_clearance_mm = Some(0.25);
        invalid_blocked.push(value);
        let mut value = blocked.clone();
        value.block_reason = Some(MotionBlockReason::InitialInterference);
        invalid_blocked.push(value);
        for rotation in invalid_blocked {
            assert!(!valid_rotation_report(&rotation, &separated));
        }

        let mut interfering_static = separated.clone();
        interfering_static.relation = AssemblyRelation::Interfering;
        let initial_interference = RotationalMotionReport {
            first_limit_interval_degrees: Some([0.0, 0.0]),
            block_reason: Some(MotionBlockReason::InitialInterference),
            ..blocked.clone()
        };
        assert!(valid_rotation_report(
            &initial_interference,
            &interfering_static
        ));
        let mut wrong_initial_reason = initial_interference.clone();
        wrong_initial_reason.block_reason = Some(MotionBlockReason::Contact);
        assert!(!valid_rotation_report(
            &wrong_initial_reason,
            &interfering_static
        ));
        let mut nonzero_initial_interval = initial_interference.clone();
        nonzero_initial_interval.first_limit_interval_degrees = Some([0.0, 1.0]);
        assert!(!valid_rotation_report(
            &nonzero_initial_interval,
            &interfering_static
        ));
        let mut insufficient_static = separated.clone();
        insufficient_static.clearance_mm = 0.1;
        let insufficient = RotationalMotionReport {
            retained: false,
            first_limit_interval_degrees: Some([0.0, 0.0]),
            block_reason: Some(MotionBlockReason::InsufficientInitialClearance),
            ..blocked.clone()
        };
        assert!(valid_rotation_report(&insufficient, &insufficient_static));
        let mut threshold_static = separated.clone();
        threshold_static.clearance_mm = passing.target_clearance_mm;
        let threshold = RotationalMotionReport {
            retained: false,
            first_limit_interval_degrees: Some([0.0, 0.0]),
            block_reason: Some(MotionBlockReason::ClearanceThreshold),
            ..blocked.clone()
        };
        assert!(valid_rotation_report(&threshold, &threshold_static));
        let mut contact_static = separated.clone();
        contact_static.clearance_mm = 0.0;
        let initial_contact = RotationalMotionReport {
            target_clearance_mm: 0.0,
            first_limit_interval_degrees: Some([0.0, 0.0]),
            ..blocked
        };
        assert!(valid_rotation_report(&initial_contact, &contact_static));

        assert!(report_numbers_match(10.0, 10.0 + 5e-9));
        assert!(!report_numbers_match(10.0, 10.0 + 20e-9));
        assert!(!report_numbers_match(f64::INFINITY, f64::INFINITY));
    }

    #[test]
    fn mechanical_certificate_must_match_motion_and_exceed_the_clearance_threshold() {
        let expected = RotationalMotion {
            pivot_mm: [1.0, 2.0, 3.0],
            axis: [0.0, 0.0, 2.0],
            angle_degrees: 90.0,
            target_clearance_mm: Some(0.2),
        };
        let report = mechanical_report([1.0, 2.0, 3.0], true, None, Some(0.25));
        assert!(validate_mechanical_analysis_report(&report, expected).expect("valid certificate"));

        for missing in ["fixed", "moving", "static_analysis"] {
            let mut incomplete = report.clone();
            incomplete
                .as_object_mut()
                .expect("assembly report object")
                .remove(missing);
            assert_eq!(
                validate_mechanical_analysis_report(&incomplete, expected)
                    .expect_err("incomplete assembly report")
                    .code(),
                "geometry_worker_protocol"
            );
        }
        let mut inconsistent_static = report.clone();
        inconsistent_static["static_analysis"]["relation"] = json!("interfering");
        assert_eq!(
            validate_mechanical_analysis_report(&inconsistent_static, expected)
                .expect_err("static clearance fields must be internally consistent")
                .code(),
            "geometry_worker_protocol"
        );
        for (field, mismatched) in [
            ("required_clearance_mm", json!(0.1)),
            ("meets_required_clearance", json!(false)),
        ] {
            let mut mismatch = report.clone();
            mismatch["static_analysis"][field] = mismatched;
            assert_eq!(
                validate_mechanical_analysis_report(&mismatch, expected)
                    .expect_err("static analysis must match the requested clearance")
                    .code(),
                "geometry_worker_protocol"
            );
        }
        for (clearance_mm, meets_required_clearance) in [(0.1, false), (0.2, true)] {
            let mut invalid_start = report.clone();
            invalid_start["static_analysis"]["clearance_mm"] = json!(clearance_mm);
            invalid_start["static_analysis"]["surface_gap_mm"] = json!(clearance_mm);
            invalid_start["static_analysis"]["closest_surface_points"]["moving_mm"] =
                json!([1.0 + clearance_mm, 0.0, 0.0]);
            invalid_start["static_analysis"]["meets_required_clearance"] =
                json!(meets_required_clearance);
            assert_eq!(
                validate_mechanical_analysis_report(&invalid_start, expected)
                    .expect_err("a passing rotation must start strictly above the threshold")
                    .code(),
                "geometry_worker_protocol"
            );
        }

        for (field, mismatched) in [
            ("pivot_mm", json!([0.0, 2.0, 3.0])),
            ("axis", json!([1.0, 0.0, 0.0])),
            ("angle_degrees", json!(45.0)),
            ("target_clearance_mm", json!(0.1)),
        ] {
            let mut mismatch = report.clone();
            mismatch["rotation"][field] = mismatched;
            assert_eq!(
                validate_mechanical_analysis_report(&mismatch, expected)
                    .expect_err("mismatched motion is not the requested certificate")
                    .code(),
                "geometry_worker_protocol"
            );
        }
        for (pointer, corrupted) in [
            ("/fixed/vertices", json!(0)),
            ("/fixed/volume_mm3", json!(0.0)),
            ("/fixed/bounds/dimensions_mm", json!([2.0, 1.0, 1.0])),
            ("/static_analysis/surface_gap_mm", json!(0.75)),
            ("/static_analysis/interference_volume_mm3", json!(0.1)),
            ("/static_analysis/fixed_interference_fraction", json!(0.1)),
            ("/rotation/retained", json!(true)),
            ("/rotation/first_limit_interval_degrees", json!([1.0, 2.0])),
            ("/rotation/clearance_at_end_mm", Value::Null),
            ("/rotation/clearance_at_end_mm", json!(0.2)),
            ("/rotation/clearance_at_end_mm", json!(0.21)),
        ] {
            let mut malformed = report.clone();
            *malformed
                .pointer_mut(pointer)
                .expect("mechanical report field") = corrupted;
            assert_eq!(
                validate_mechanical_analysis_report(&malformed, expected)
                    .expect_err("contradictory typed report must fail closed")
                    .code(),
                "geometry_worker_protocol"
            );
        }
        let mut extra_field = report.clone();
        extra_field["rotation"]["unexpected"] = json!(true);
        assert_eq!(
            validate_mechanical_analysis_report(&extra_field, expected)
                .expect_err("unknown report fields must fail closed")
                .code(),
            "geometry_worker_protocol"
        );
        let mut extra_bounds_field = report.clone();
        extra_bounds_field["fixed"]["bounds"]["unexpected"] = json!(true);
        assert_eq!(
            validate_mechanical_analysis_report(&extra_bounds_field, expected)
                .expect_err("unknown nested bounds fields must fail closed")
                .code(),
            "geometry_worker_protocol"
        );
        let mut inconsistent_blocked = report.clone();
        inconsistent_blocked["static_analysis"]["relation"] = json!("interfering");
        inconsistent_blocked["static_analysis"]["meets_required_clearance"] = json!(true);
        inconsistent_blocked["rotation"]["can_rotate_full_angle"] = json!(false);
        inconsistent_blocked["rotation"]["minimum_certified_clearance_mm"] = Value::Null;
        inconsistent_blocked["rotation"]["block_reason"] = json!("contact");
        assert_eq!(
            validate_mechanical_analysis_report(&inconsistent_blocked, expected)
                .expect_err("blocked certificates still require consistent static fields")
                .code(),
            "geometry_worker_protocol"
        );
        let mut equality = report;
        equality["rotation"]["minimum_certified_clearance_mm"] = json!(0.2);
        assert_eq!(
            validate_mechanical_analysis_report(&equality, expected)
                .expect_err("threshold equality is not a conservative certificate")
                .code(),
            "geometry_worker_protocol"
        );
        let mut contradictory = equality;
        contradictory["rotation"]["minimum_certified_clearance_mm"] = json!(0.25);
        contradictory["rotation"]["block_reason"] = json!("contact");
        assert_eq!(
            validate_mechanical_analysis_report(&contradictory, expected)
                .expect_err("a passing certificate cannot carry a block reason")
                .code(),
            "geometry_worker_protocol"
        );
        contradictory["rotation"]["can_rotate_full_angle"] = json!(false);
        contradictory["rotation"]["block_reason"] = Value::Null;
        assert_eq!(
            validate_mechanical_analysis_report(&contradictory, expected)
                .expect_err("a blocked certificate must identify its reason")
                .code(),
            "geometry_worker_protocol"
        );
    }

    #[test]
    fn blender_mechanical_preparation_must_attest_the_complete_authored_motion() {
        let spec = MechanicalRotationSpec {
            fixed_objects: vec!["Base".to_string()],
            moving_objects: vec!["Leaf".to_string()],
            pivot_mm: [1.0, 2.0, 3.0],
            axis: [0.0, 0.0, 2.0],
            angle_degrees: 90.0,
            target_clearance_mm: 0.2,
            max_analysis_mesh_bytes: 4096,
        };
        let response = json!({
            "fixed_path": "fixed.stl",
            "moving_path": "moving.stl",
            "motion": {
                "controller": "Controller",
                "objects": ["Leaf"],
                "pivot": [1.0, 2.0, 3.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "frame_start": 1,
                "frame_end": 3,
                "interpolation": "LINEAR"
            }
        });
        let authored = validate_mechanical_prepare_response(
            &response,
            "fixed.stl",
            "moving.stl",
            "Controller",
            &spec,
            1,
            3,
        )
        .expect("matching Blender preparation");
        assert_eq!(authored.pivot_mm, [1.0, 2.0, 3.0]);
        assert_eq!(authored.axis, [0.0, 0.0, 1.0]);
        assert_eq!(authored.angle_degrees, 90.0);

        let mut stored_coercion = response.clone();
        stored_coercion["motion"]["pivot"] = json!([1.0000005, 2.0, 3.0]);
        stored_coercion["motion"]["axis"] = json!([0.0000005, 0.0, 1.0]);
        stored_coercion["motion"]["angle_degrees"] = json!(90.00005);
        let authored = validate_mechanical_prepare_response(
            &stored_coercion,
            "fixed.stl",
            "moving.stl",
            "Controller",
            &spec,
            1,
            3,
        )
        .expect("stored Blender values within the comparison tolerance");
        assert_eq!(authored.pivot_mm, [1.0000005, 2.0, 3.0]);
        assert_eq!(authored.axis, [0.0000005, 0.0, 1.0]);
        assert_eq!(authored.angle_degrees, 90.00005);

        let mut tiny_angle_spec = spec.clone();
        tiny_angle_spec.angle_degrees = 0.0000005;
        let mut zero_angle = response.clone();
        zero_angle["motion"]["angle_degrees"] = json!(0.0);
        assert_eq!(
            validate_mechanical_prepare_response(
                &zero_angle,
                "fixed.stl",
                "moving.stl",
                "Controller",
                &tiny_angle_spec,
                1,
                3,
            )
            .expect_err("the authored rotation must remain strictly positive")
            .code,
            "blender_protocol"
        );

        for (pointer, mismatched) in [
            ("/fixed_path", json!("other.stl")),
            ("/moving_path", json!("other.stl")),
            ("/motion/controller", json!("OtherController")),
            ("/motion/objects", json!(["OtherLeaf"])),
            ("/motion/pivot", json!([0.0, 2.0, 3.0])),
            ("/motion/axis", json!([1.0, 0.0, 0.0])),
            ("/motion/angle_degrees", json!(45.0)),
            ("/motion/frame_start", json!(0)),
            ("/motion/frame_end", json!(4)),
            ("/motion/interpolation", json!("BEZIER")),
        ] {
            let mut mismatch = response.clone();
            *mismatch
                .pointer_mut(pointer)
                .expect("mechanical response field") = mismatched;
            assert_eq!(
                validate_mechanical_prepare_response(
                    &mismatch,
                    "fixed.stl",
                    "moving.stl",
                    "Controller",
                    &spec,
                    1,
                    3,
                )
                .expect_err("mismatched Blender preparation")
                .code,
                "blender_protocol"
            );
        }
    }

    #[test]
    fn mechanical_axis_normalization_is_stable_and_unit_length() {
        let diagonal = normalized_mechanical_axis([1.0, 1.0, 0.0]).expect("diagonal axis");
        assert!((diagonal[0] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        assert!((diagonal[1] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        assert_eq!(diagonal[2], 0.0);

        for axis in [
            [f64::MAX, f64::MAX, 0.0],
            [f64::MIN_POSITIVE, f64::MIN_POSITIVE, 0.0],
        ] {
            let normalized = normalized_mechanical_axis(axis).expect("finite extreme axis");
            assert!((normalized[0] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
            assert!((normalized[1] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        }
        assert_eq!(normalized_mechanical_axis([0.0, 0.0, 0.0]), None);
        assert_eq!(normalized_mechanical_axis([f64::INFINITY, 0.0, 0.0]), None);
    }

    #[test]
    fn recovered_mechanical_certificate_metadata_fails_closed() {
        let params = serde_json::from_value(json!({
            "source_blend": "scene.blend",
            "kind": "mechanical_rotation",
            "mechanical_rotation": {
                "fixed_objects": ["Base"],
                "moving_objects": ["Leaf"],
                "pivot_mm": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.2,
                "max_analysis_mesh_bytes": 1024
            },
            "frame_start": 1,
            "frame_end": 3
        }))
        .expect("mechanical params");
        let (spec, kind, total_frames) = validate_submit(params).expect("mechanical job");
        let job_id = "0123456789abcdef0123456789abcdef";
        let mut job = test_job_record(job_id, JobState::Queued);
        job.kind = kind;
        job.spec = spec;
        job.progress.total_frames = total_frames;
        validate_recovered_mechanical_analysis(&job).expect("unstarted job needs no certificate");

        let mut invalid_without_analysis = job.clone();
        invalid_without_analysis.progress.completed_frames = 1;
        assert!(validate_recovered_mechanical_analysis(&invalid_without_analysis).is_err());
        let mut invalid_without_analysis = job.clone();
        invalid_without_analysis.pending_blender_outcome = Some(PendingBlenderOutcome::Completed);
        assert!(validate_recovered_mechanical_analysis(&invalid_without_analysis).is_err());
        let mut invalid_without_analysis = job.clone();
        invalid_without_analysis.state = JobState::Succeeded;
        assert!(validate_recovered_mechanical_analysis(&invalid_without_analysis).is_err());

        job.mechanical_analysis = Some(MechanicalAnalysis {
            generation: 0,
            fixed_artifact: JobArtifact {
                path: mechanical_fixed_path(job_id, 0),
                size_bytes: 10,
                media_type: "model/stl".to_string(),
            },
            moving_artifact: JobArtifact {
                path: mechanical_moving_path(job_id, 0),
                size_bytes: 10,
                media_type: "model/stl".to_string(),
            },
            units: "millimetres".to_string(),
            certified: true,
            report: mechanical_report([0.0, 0.0, 0.0], true, None, Some(0.25)),
        });
        validate_recovered_mechanical_analysis(&job).expect("valid recovered certificate");

        let mut legacy = job.clone();
        let legacy_analysis = legacy.mechanical_analysis.as_mut().unwrap();
        legacy_analysis.fixed_artifact.media_type = "application/vnd.ms-pki.stl".into();
        legacy_analysis.moving_artifact.media_type = "application/vnd.ms-pki.stl".into();
        validate_recovered_mechanical_analysis(&legacy)
            .expect("legacy STL metadata is recoverable");
        legacy
            .mechanical_analysis
            .as_mut()
            .unwrap()
            .fixed_artifact
            .media_type = "text/plain".into();
        assert!(validate_recovered_mechanical_analysis(&legacy).is_err());

        let mut stored_coercion = job.clone();
        let stored_report = &mut stored_coercion
            .mechanical_analysis
            .as_mut()
            .expect("mechanical analysis")
            .report;
        stored_report["rotation"]["pivot_mm"] = json!([0.0000005, 0.0, 0.0]);
        stored_report["rotation"]["axis"] = json!([0.0000005, 0.0, 1.0]);
        stored_report["rotation"]["angle_degrees"] = json!(90.00005);
        validate_recovered_mechanical_analysis(&stored_coercion)
            .expect("recovery must preserve the certified Blender values");

        let mut recovered_after_certification = job.clone();
        recovered_after_certification.recovery_count = 1;
        validate_recovered_mechanical_analysis(&recovered_after_certification)
            .expect("a persisted certificate remains valid after a later render recovery");

        let mut boundary_sized = job.clone();
        let boundary_analysis = boundary_sized.mechanical_analysis.as_mut().unwrap();
        boundary_analysis.fixed_artifact.size_bytes = 1024;
        boundary_analysis.moving_artifact.size_bytes = 1024;
        validate_recovered_mechanical_analysis(&boundary_sized)
            .expect("analysis artifacts may use the complete configured byte budget");

        let mut partially_rendered = job.clone();
        partially_rendered.state = JobState::Running;
        partially_rendered.progress.completed_frames = 1;
        validate_recovered_mechanical_analysis(&partially_rendered)
            .expect("rendered frames remain valid when backed by a passing certificate");

        let mut blocked_before_render = job.clone();
        blocked_before_render.state = JobState::Failed;
        let blocked_analysis = blocked_before_render.mechanical_analysis.as_mut().unwrap();
        blocked_analysis.certified = false;
        blocked_analysis.report["rotation"]["can_rotate_full_angle"] = json!(false);
        blocked_analysis.report["rotation"]["retained"] = json!(true);
        blocked_analysis.report["rotation"]["first_limit_interval_degrees"] = json!([45.0, 46.0]);
        blocked_analysis.report["rotation"]["block_reason"] = json!("contact");
        blocked_analysis.report["rotation"]["clearance_at_end_mm"] = Value::Null;
        blocked_analysis.report["rotation"]["minimum_certified_clearance_mm"] = Value::Null;
        validate_recovered_mechanical_analysis(&blocked_before_render)
            .expect("a failed pre-render clearance certificate is durable job evidence");

        let mut invalid_jobs = Vec::new();
        let mut invalid = job.clone();
        let future_analysis = invalid.mechanical_analysis.as_mut().unwrap();
        future_analysis.generation = 1;
        future_analysis.fixed_artifact.path = mechanical_fixed_path(job_id, 1);
        future_analysis.moving_artifact.path = mechanical_moving_path(job_id, 1);
        invalid_jobs.push(invalid);
        let mut invalid = job.clone();
        invalid
            .mechanical_analysis
            .as_mut()
            .unwrap()
            .fixed_artifact
            .path = "wrong.stl".to_string();
        invalid_jobs.push(invalid);
        let mut invalid = job.clone();
        invalid
            .mechanical_analysis
            .as_mut()
            .unwrap()
            .moving_artifact
            .path = "wrong.stl".to_string();
        invalid_jobs.push(invalid);
        for fixed in [true, false] {
            let mut invalid = job.clone();
            let analysis = invalid.mechanical_analysis.as_mut().unwrap();
            let artifact = if fixed {
                &mut analysis.fixed_artifact
            } else {
                &mut analysis.moving_artifact
            };
            artifact.media_type = "application/octet-stream".to_string();
            invalid_jobs.push(invalid);
            let mut invalid = job.clone();
            let analysis = invalid.mechanical_analysis.as_mut().unwrap();
            let artifact = if fixed {
                &mut analysis.fixed_artifact
            } else {
                &mut analysis.moving_artifact
            };
            artifact.size_bytes = 0;
            invalid_jobs.push(invalid);
            let mut invalid = job.clone();
            let analysis = invalid.mechanical_analysis.as_mut().unwrap();
            let artifact = if fixed {
                &mut analysis.fixed_artifact
            } else {
                &mut analysis.moving_artifact
            };
            artifact.size_bytes = 1025;
            invalid_jobs.push(invalid);
        }
        let mut invalid = job.clone();
        invalid.mechanical_analysis.as_mut().unwrap().units = "metres".to_string();
        invalid_jobs.push(invalid);
        let mut invalid = job.clone();
        invalid.mechanical_analysis.as_mut().unwrap().report["rotation"]["pivot_mm"] =
            json!([1.0, 0.0, 0.0]);
        invalid_jobs.push(invalid);
        let mut invalid = job.clone();
        invalid.mechanical_analysis.as_mut().unwrap().report["rotation"]["can_rotate_full_angle"] =
            json!(false);
        invalid.mechanical_analysis.as_mut().unwrap().report["rotation"]["block_reason"] =
            json!("contact");
        invalid_jobs.push(invalid);
        let mut invalid = job.clone();
        invalid.progress.completed_frames = 1;
        invalid.mechanical_analysis.as_mut().unwrap().certified = false;
        invalid.mechanical_analysis.as_mut().unwrap().report["rotation"]["can_rotate_full_angle"] =
            json!(false);
        invalid.mechanical_analysis.as_mut().unwrap().report["rotation"]["block_reason"] =
            json!("contact");
        invalid_jobs.push(invalid);

        for invalid in invalid_jobs {
            assert_eq!(
                validate_recovered_mechanical_analysis(&invalid)
                    .expect_err("invalid recovered certificate")
                    .code(),
                "validation"
            );
        }

        let mut ordinary = test_job_record(job_id, JobState::Queued);
        ordinary.mechanical_analysis = job.mechanical_analysis;
        assert!(validate_recovered_mechanical_analysis(&ordinary).is_err());
    }

    #[test]
    fn invalid_runtime_budgets_and_zero_frame_step_fail_before_job_creation() {
        for arguments in [
            json!({
                "source_blend": "scene.blend",
                "kind": "animation",
                "frame_step": 0,
            }),
            json!({
                "source_blend": "scene.blend",
                "kind": "still",
                "frame_timeout_seconds": f64::MAX,
            }),
            json!({
                "source_blend": "scene.blend",
                "kind": "still",
                "width": 8192,
                "height": 8192,
                "presentation": {"profile": "studio_neutral"},
            }),
        ] {
            let params = serde_json::from_value(arguments).expect("typed invalid params");
            assert_eq!(validate_submit(params).unwrap_err().code(), "validation");
        }

        let presented_boundary = serde_json::from_value(json!({
            "source_blend": "scene.blend",
            "kind": "still",
            "width": 4096,
            "height": 4096,
            "presentation": {"profile": "studio_neutral"},
        }))
        .expect("typed boundary params");
        validate_submit(presented_boundary)
            .expect("the exact product-render pixel boundary is admitted");
    }
}
