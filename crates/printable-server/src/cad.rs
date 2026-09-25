//! Project CAD builds delegated to a dedicated native worker.

pub mod qualification;

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{Mutex, Semaphore, oneshot},
};
use tokio_util::sync::CancellationToken;

use crate::{error::ToolError, projects};

const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildParams {
    pub project_id: String,
    /// Project-relative Python script or STEP source, retained with the build.
    pub source: String,
    /// New project-relative directory for this build's inputs, outputs, and report.
    pub output_dir: String,
    /// JSON values available to a modeling script as the parameters dictionary.
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
    /// Additional project-relative input files, copied with their relative paths.
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default = "linear_tolerance")]
    pub linear_tolerance_mm: f64,
    #[serde(default = "angular_tolerance")]
    pub angular_tolerance_rad: f64,
    #[serde(default = "timeout_seconds")]
    pub timeout_seconds: u64,
    /// Build the exact retained source and parameters of this design revision.
    pub revision: Option<projects::revisions::Identity>,
    /// Return retained admission state immediately, then poll status by output_dir.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub qualification: qualification::Requirements,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportParams {
    pub project_id: String,
    /// Project-relative STEP source; every assembly instance is retained.
    pub source: String,
    pub output_dir: String,
    #[serde(default = "linear_tolerance")]
    pub linear_tolerance_mm: f64,
    #[serde(default = "angular_tolerance")]
    pub angular_tolerance_rad: f64,
    #[serde(default = "timeout_seconds")]
    pub timeout_seconds: u64,
    pub revision: Option<projects::revisions::Identity>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub qualification: qualification::Requirements,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildHandle {
    pub project_id: String,
    pub output_dir: String,
}

fn linear_tolerance() -> f64 {
    0.05
}
fn angular_tolerance() -> f64 {
    0.1
}
fn timeout_seconds() -> u64 {
    600
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CadRequest {
    Model(BuildParams),
    ImportStep(ImportParams),
    Status(BuildHandle),
    Cancel(BuildHandle),
}

impl CadRequest {
    fn parts(&self) -> Result<(&str, std::borrow::Cow<'_, BuildParams>), ToolError> {
        Ok(match self {
            Self::Model(params) => ("model", std::borrow::Cow::Borrowed(params)),
            Self::ImportStep(params) => (
                "import_step",
                std::borrow::Cow::Owned(BuildParams {
                    project_id: params.project_id.clone(),
                    source: params.source.clone(),
                    output_dir: params.output_dir.clone(),
                    parameters: BTreeMap::new(),
                    inputs: Vec::new(),
                    linear_tolerance_mm: params.linear_tolerance_mm,
                    angular_tolerance_rad: params.angular_tolerance_rad,
                    timeout_seconds: params.timeout_seconds,
                    revision: params.revision.clone(),
                    background: params.background,
                    qualification: params.qualification.clone(),
                }),
            ),
            Self::Status(_) | Self::Cancel(_) => {
                return Err(ToolError::Validation(
                    "CAD status and cancellation do not contain build parameters".into(),
                ));
            }
        })
    }

    fn validate(&self, workspace: &Workspace) -> Result<String, ToolError> {
        if let Self::Status(handle) | Self::Cancel(handle) = self {
            return build_directory(workspace, handle);
        }
        let (action, p) = self.parts()?;
        p.qualification.validate()?;
        if let Some(identity) = &p.revision {
            let revision = projects::revisions::get(workspace, &p.project_id, identity)?;
            let mut inputs = p.inputs.clone();
            inputs.push(p.source.clone());
            inputs.sort();
            inputs.dedup();
            let parameters_match = p.parameters.len() == revision.manifest.parameters.len()
                && revision.manifest.parameters.iter().all(|(key, parameter)| {
                    p.parameters.get(key).and_then(Value::as_f64) == Some(parameter.value)
                });
            if p.source != revision.source
                || !parameters_match
                || inputs != revision.files.keys().cloned().collect::<Vec<_>>()
            {
                return Err(ToolError::Validation(
                    "CAD source, inputs, and parameters must match the selected immutable revision"
                        .into(),
                ));
            }
        }
        let extension = Path::new(&p.source)
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if (action == "model" && extension != "py")
            || (action == "import_step" && !matches!(extension.as_str(), "step" | "stp"))
            || !(0.001..=10.0).contains(&p.linear_tolerance_mm)
            || !(0.01..=1.0).contains(&p.angular_tolerance_rad)
            || !(1..=1800).contains(&p.timeout_seconds)
            || p.inputs.len() > 128
        {
            return Err(ToolError::Validation("CAD requires a Python/STEP source, linear tolerance 0.001–10 mm, angular tolerance 0.01–1 rad, timeout 1–1800 seconds, and at most 128 input files".into()));
        }
        projects::resolve(workspace, &p.project_id, &p.source)?;
        for input in &p.inputs {
            projects::resolve(workspace, &p.project_id, input)?;
        }
        let report = projects::resolve(
            workspace,
            &p.project_id,
            &format!("{}/report.json", p.output_dir),
        )?;
        Ok(report
            .strip_suffix("/report.json")
            .expect("known suffix")
            .into())
    }
}

pub async fn forward(
    workspace: &Workspace,
    endpoint: Option<&str>,
    request: CadRequest,
) -> Result<Value, ToolError> {
    request.validate(workspace)?;
    let endpoint = endpoint.ok_or_else(|| ToolError::Cad("CAD worker is not configured".into()))?;
    let timeout = match &request {
        CadRequest::Status(_) | CadRequest::Cancel(_) => 0,
        _ => {
            let (_, p) = request.parts()?;
            if p.background { 0 } else { p.timeout_seconds }
        }
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout + 30))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| ToolError::Cad("cannot initialize CAD connection".into()))?;
    let mut response = client
        .post(format!("{}/build", endpoint.trim_end_matches('/')))
        .json(&request)
        .send()
        .await
        .map_err(|_| {
            ToolError::Cad(
                "CAD connection failed; query cad_build.status with the same project_id and output_dir before deciding whether to submit again".into(),
            )
        })?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ToolError::Cad(
            "CAD response interrupted; query cad_build.status with the same build handle".into(),
        )
    })? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(ToolError::Cad("CAD response exceeds metadata limit".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if !status.is_success() {
        return Err(ToolError::Cad(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("CAD worker rejected build")
                .into(),
        ));
    }
    Ok(value)
}

pub struct CadWorker {
    pub workspace: Arc<Workspace>,
    pub python: std::path::PathBuf,
    pub script: std::path::PathBuf,
    pub admission: Arc<Semaphore>,
    active: Mutex<Option<Active>>,
}

struct Active {
    output: String,
    state: Value,
    cancel: CancellationToken,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

struct ExecutionGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

fn build_directory(workspace: &Workspace, handle: &BuildHandle) -> Result<String, ToolError> {
    let path = projects::resolve(
        workspace,
        &handle.project_id,
        &format!("{}/state.json", handle.output_dir),
    )?;
    Ok(path
        .strip_suffix("/state.json")
        .expect("known suffix")
        .to_owned())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl CadWorker {
    pub fn new(
        workspace: Arc<Workspace>,
        python: std::path::PathBuf,
        script: std::path::PathBuf,
    ) -> Self {
        Self {
            workspace,
            python,
            script,
            admission: Arc::new(Semaphore::new(1)),
            active: Mutex::new(None),
        }
    }

    pub async fn build(self: &Arc<Self>, request: CadRequest) -> Result<Value, ToolError> {
        match request {
            CadRequest::Status(handle) => return self.status(handle, false).await,
            CadRequest::Cancel(handle) => return self.status(handle, true).await,
            _ => {}
        }
        let output = request.validate(&self.workspace)?;
        let (_, params) = request.parts()?;
        let background = params.background;
        let permit = Arc::clone(&self.admission).try_acquire_owned().map_err(|_| {
            ToolError::Cad("CAD worker is busy; no build was admitted. Inspect your active build with cad_build.status before submitting another revision".into())
        })?;
        let handle = BuildHandle {
            project_id: params.project_id.clone(),
            output_dir: params.output_dir.clone(),
        };
        let state = json!({"build":handle,"status":"admitted","phase":"input_snapshot","admitted_at_unix_ms":now_ms(),
            "execution_timeout_seconds":params.timeout_seconds,"cancel_requested":false,"progress":null});
        let mut active = self.active.lock().await;
        if !self.workspace.create_public_directory(&output)? {
            return Err(ToolError::Cad("build directory already exists; query cad_build.status, then use a new output_dir for a revision".into()));
        }
        self.workspace.write_artifact(
            &format!("{output}/request.json"),
            &serde_json::to_vec(&json!({"request":request,"sources":null}))?,
            false,
        )?;
        self.persist_state(&output, &state, false)?;
        let cancel = CancellationToken::new();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let execution_guard = ExecutionGuard(Arc::clone(&alive));
        *active = Some(Active {
            output: output.clone(),
            state: state.clone(),
            cancel: cancel.clone(),
            alive,
        });
        drop(active);
        let worker = Arc::clone(self);
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _execution_guard = execution_guard;
            let _permit = permit;
            let result = worker.run_build(request, &output, &cancel).await;
            let mut active = worker.active.lock().await;
            let job = active
                .as_ref()
                .expect("admitted build owns worker capacity");
            let mut terminal = job.state.clone();
            terminal["phase"] = json!("terminal");
            terminal["finished_at_unix_ms"] = json!(now_ms());
            terminal["cancel_requested"] = json!(cancel.is_cancelled());
            match &result {
                Ok(value) => {
                    terminal["status"] = json!("completed");
                    terminal["result"] = value.clone();
                }
                Err(error) => {
                    terminal["status"] = json!(if cancel.is_cancelled() {
                        "cancelled"
                    } else {
                        "failed"
                    });
                    terminal["error"] = json!(error.to_string());
                }
            }
            let response = match worker.persist_state(&output, &terminal, true) {
                Ok(()) => result,
                Err(error) => {
                    tracing::error!(code = error.code(), "could not persist terminal CAD state");
                    Err(ToolError::Cad("CAD completion could not be recorded; inspect status and retained outputs before retrying".into()))
                }
            };
            *active = None;
            let _ = sender.send(response);
        });
        if background {
            Ok(state)
        } else {
            receiver.await.map_err(|_| {
                ToolError::Cad(
                    "CAD result delivery ended; query retained status before retrying".into(),
                )
            })?
        }
    }

    fn persist_state(&self, output: &str, state: &Value, overwrite: bool) -> Result<(), ToolError> {
        self.workspace.write_artifact(
            &format!("{output}/state.json"),
            &serde_json::to_vec(state)?,
            overwrite,
        )?;
        Ok(())
    }

    async fn status(&self, handle: BuildHandle, cancel: bool) -> Result<Value, ToolError> {
        let output = build_directory(&self.workspace, &handle)?;
        let active = self.active.lock().await;
        if let Some(job) = active.as_ref().filter(|job| {
            job.output == output && job.alive.load(std::sync::atomic::Ordering::Acquire)
        }) {
            if cancel {
                job.cancel.cancel();
            }
            let mut state = job.state.clone();
            state["cancel_requested"] = json!(job.cancel.is_cancelled());
            return Ok(state);
        }
        match self
            .workspace
            .read_artifact(&format!("{output}/state.json"))
        {
            Ok((_, bytes)) => {
                let mut state: Value = serde_json::from_slice(&bytes)?;
                if matches!(state["status"].as_str(), Some("admitted" | "running")) {
                    state["status"] = json!("interrupted");
                    state["error"] = json!(
                        "no active worker owns this build; inspect retained report and artifacts before submitting a new revision"
                    );
                }
                Ok(state)
            }
            Err(printable_workspace::WsError::NotFound(_)) => {
                // Older completed builds remain retrievable without inventing progress.
                match self.workspace.read_artifact(&format!("{output}/report.json")) {
                    Ok((_, bytes)) => Ok(json!({"build":handle,"status":"completed","phase":"terminal","result":serde_json::from_slice::<Value>(&bytes)?,"history":"legacy_report"})),
                    Err(printable_workspace::WsError::NotFound(_)) => Err(ToolError::Cad("no retained CAD state or completed report was found; inspect the output directory before submitting again".into())),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn run_build(
        &self,
        request: CadRequest,
        output: &str,
        cancel: &CancellationToken,
    ) -> Result<Value, ToolError> {
        let workspace = Arc::clone(&self.workspace);
        let (action, params) = request.parts()?;
        let staging = tempfile::tempdir()?;
        let input_root = staging.path().join("inputs");
        std::fs::create_dir(&input_root)?;
        let mut names = params.inputs.clone();
        names.push(params.source.clone());
        names.sort();
        names.dedup();
        let id = params.project_id.clone();
        let revision = params
            .revision
            .as_ref()
            .map(|identity| projects::revisions::get(&workspace, &id, identity))
            .transpose()?;
        let staging_inputs = input_root.clone();
        let sources = tokio::task::spawn_blocking(move || {
            let mut sources = Vec::new();
            let mut total = 0;
            for name in names {
                let path = projects::resolve(&workspace, &id, &name)?;
                let retained = revision.as_ref().map(|r| &r.files[&name]);
                let snapshot = workspace.snapshot_artifact_bounded(retained.map_or(path.as_str(), |s| s.snapshot.as_str()), MAX_FILE_BYTES)?;
                total += snapshot.meta().size_bytes;
                if total > MAX_FILE_BYTES { return Err(ToolError::Cad("CAD input set exceeds 1 GiB".into())); }
                let destination = staging_inputs.join(&name);
                std::fs::create_dir_all(destination.parent().expect("input parent"))?;
                std::fs::copy(snapshot.path(), &destination)?;
                let sha256 = hash_file(&destination)?;
                if retained.is_some_and(|s| s.sha256 != sha256 || s.size_bytes != snapshot.meta().size_bytes) {
                    return Err(ToolError::Validation("retained revision source identity changed; restore verified source bytes before building".into()));
                }
                sources.push(json!({"path":path,"snapshot":format!("inputs/{name}"),"sha256":sha256}));
            }
            Ok::<_,ToolError>(sources)
        }).await.map_err(|_| ToolError::Cad("CAD input staging task failed".into()))??;
        let native = json!({"action":action,"source":format!("inputs/{}",params.source),"parameters":params.parameters,"linear_tolerance_mm":params.linear_tolerance_mm,"angular_tolerance_rad":params.angular_tolerance_rad});
        std::fs::write(
            staging.path().join("request.json"),
            serde_json::to_vec(&native)?,
        )?;
        let result = async {
            self.workspace.write_artifact(
                &format!("{output}/request.json"),
                &serde_json::to_vec(&json!({"request":request,"sources":sources}))?,
                true,
            )?;
            let mut retained_inputs = params.inputs.clone();
            retained_inputs.push(params.source.clone());
            retained_inputs.sort();
            retained_inputs.dedup();
            for name in &retained_inputs {
                let destination = format!("{output}/inputs/{name}");
                self.workspace.commit_generated_artifact_bounded(
                    &destination,
                    &input_root.join(name),
                    false,
                    MAX_FILE_BYTES,
                )?;
            }
            if cancel.is_cancelled() {
                return Err(ToolError::Cad(
                    "CAD build cancelled before native execution".into(),
                ));
            }
            {
                let mut active = self.active.lock().await;
                let state = &mut active.as_mut().expect("admitted build owns capacity").state;
                state["status"] = json!("running");
                state["phase"] = json!("native_execution");
                state["native_started_at_unix_ms"] = json!(now_ms());
                self.persist_state(output, state, true)?;
            }
            let result = self
                .execute(staging.path(), params.timeout_seconds, cancel)
                .await;
            let log = staging.path().join("build-log.json");
            if log.try_exists()? {
                self.workspace.commit_generated_artifact_bounded(
                    &format!("{output}/build-log.json"),
                    &log,
                    false,
                    MAX_LOG_BYTES * 16,
                )?;
            }
            match result {
                Ok(()) => self.commit(
                    staging.path(),
                    output,
                    &params.project_id,
                    params.revision.as_ref(),
                    &params.qualification,
                ),
                Err(error) => Err(error),
            }
        }
        .await;
        if let Err(ref error) = result {
            self.workspace.write_artifact(
                &format!("{output}/failure.json"),
                &serde_json::to_vec(&json!({"error":error.to_string()}))?,
                false,
            )?;
        }
        result
    }

    async fn execute(
        &self,
        directory: &Path,
        timeout: u64,
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        let mut command = Command::new(&self.python);
        command
            .arg("-I")
            .arg(&self.script)
            .arg(directory)
            .current_dir(directory.join("inputs"))
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", directory)
            .env("OMP_NUM_THREADS", "2")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn()?;
        let _group = ProcessGroup(child.id().expect("spawned process"));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let run = async {
            let (status, out, err) =
                tokio::join!(child.wait(), bounded_log(stdout), bounded_log(stderr));
            let log = json!({"stdout":out?,"stderr":err?});
            std::fs::write(directory.join("build-log.json"), serde_json::to_vec(&log)?)?;
            if !status?.success() {
                return Err(ToolError::Cad(
                    "native CAD build failed; inspect build-log.json".into(),
                ));
            }
            Ok(())
        };
        tokio::select! {
            biased;
            result = tokio::time::timeout(Duration::from_secs(timeout), run) => result
            .map_err(|_| ToolError::Cad("CAD build exceeded its native execution deadline".into()))?,
            _ = cancel.cancelled() => Err(ToolError::Cad("CAD build cancelled during native execution".into())),
        }
    }

    fn commit(
        &self,
        staging: &Path,
        output: &str,
        project: &str,
        identity: Option<&projects::revisions::Identity>,
        requirements: &qualification::Requirements,
    ) -> Result<Value, ToolError> {
        let report_file = open_native_file(&staging.join("output/report.json"))?;
        let mut report_bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(report_file, 1024 * 1024 + 1),
            &mut report_bytes,
        )?;
        if report_bytes.len() > 1024 * 1024 {
            return Err(ToolError::Cad("CAD report exceeds metadata limit".into()));
        }
        let report: Value = serde_json::from_slice(&report_bytes)?;
        let mut artifacts = Vec::new();
        for name in ["model.step", "model.stl", "model.glb", "components.json"] {
            let path = staging.join("output").join(name);
            let meta = self.workspace.commit_generated_artifact_bounded(
                &format!("{output}/{name}"),
                &path,
                false,
                MAX_FILE_BYTES,
            )?;
            artifacts.push(json!({"artifact":meta,"sha256":hash_file(&path)?}));
        }
        let qualification = qualification::assess(&report, requirements);
        let mut result = json!({"completion":"completed","qualification":qualification,"report":report,"artifacts":artifacts,"build_directory":output});
        if let Some(identity) = identity {
            let revision = projects::revisions::get(&self.workspace, project, identity)?;
            result["revision"] = json!(identity);
            result["requirements"] = revision.manifest.assess(&report);
            let retained = format!(
                ".printable/revisions/{project}/{}/measurements/{}.json",
                identity.id,
                crate::upload::random_hex_id()?
            );
            let bytes = serde_json::to_vec(&result)?;
            self.workspace
                .write_reserved_artifact(&retained, &bytes, false)?;
            result["measurement"] =
                json!({"path":retained,"sha256":projects::revisions::digest(&bytes)});
        }
        self.workspace.write_artifact(
            &format!("{output}/report.json"),
            &serde_json::to_vec(&result)?,
            false,
        )?;
        Ok(result)
    }
}

fn open_native_file(path: &Path) -> Result<std::fs::File, ToolError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(ToolError::Cad(
            "native CAD output must be a regular file".into(),
        ));
    }
    Ok(file)
}

fn hash_file(path: &Path) -> Result<String, ToolError> {
    let mut file = open_native_file(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

async fn bounded_log(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
) -> Result<String, std::io::Error> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min((MAX_LOG_BYTES as usize).saturating_sub(result.len()));
        result.extend_from_slice(&buffer[..keep]);
    }
    Ok(String::from_utf8_lossy(&result).into_owned())
}

struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The child creates its own process group before any user code runs.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

use std::os::unix::process::CommandExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::ProjectRequest;
    use std::os::unix::fs::PermissionsExt;

    fn workspace(root: &Path) -> Arc<Workspace> {
        let workspace = Arc::new(Workspace::open(Some(root), None).unwrap());
        projects::dispatch(
            &workspace,
            serde_json::from_value::<ProjectRequest>(
                json!({"action":"create","params":{"project_id":"cad","name":"CAD"}}),
            )
            .unwrap(),
        )
        .unwrap();
        workspace
            .write_artifact("projects/cad/source.py", b"original source", false)
            .unwrap();
        workspace
    }

    fn request() -> CadRequest {
        serde_json::from_value(json!({"action":"model","params":{"project_id":"cad","source":"source.py","output_dir":"builds/one"}})).unwrap()
    }

    fn lifecycle_worker(root: &Path) -> Arc<CadWorker> {
        let workspace = workspace(root);
        let python = root.join("fake-lifecycle-cad");
        std::fs::write(&python, b"#!/usr/bin/python3\nimport pathlib,sys,json,time\nmarker=pathlib.Path(sys.argv[-2])\nwith marker.open('a') as out: out.write('called\\n')\nwhile not marker.with_suffix('.release').exists(): time.sleep(0.02)\np=pathlib.Path(sys.argv[-1])/'output'\np.mkdir()\nfor name in ['model.step','model.stl','model.glb','components.json']:\n (p/name).write_text('fake artifact')\n(p/'report.json').write_text(json.dumps({'valid':True}))\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        Arc::new(CadWorker::new(workspace, python, root.join("native.calls")))
    }

    fn handle() -> BuildHandle {
        BuildHandle {
            project_id: "cad".into(),
            output_dir: "builds/one".into(),
        }
    }

    async fn wait_started(worker: &CadWorker) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !worker.script.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn terminal(worker: &CadWorker) -> Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = worker.status(handle(), false).await.unwrap();
                if !matches!(state["status"].as_str(), Some("admitted" | "running")) {
                    break state;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dropped_waiter_does_not_replay_or_cancel_admitted_work() {
        let root = tempfile::tempdir().unwrap();
        let worker = lifecycle_worker(root.path());
        let waiter = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.build(request()).await }
        });
        wait_started(&worker).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let running = worker.status(handle(), false).await.unwrap();
        assert_eq!(running["status"], "running");
        assert_eq!(running["phase"], "native_execution");
        assert!(
            running["native_started_at_unix_ms"].as_u64().unwrap()
                >= running["admitted_at_unix_ms"].as_u64().unwrap()
        );
        std::fs::write(worker.script.with_extension("release"), b"").unwrap();
        let done = terminal(&worker).await;
        assert_eq!(done["status"], "completed");
        assert_eq!(done["result"]["artifacts"].as_array().unwrap().len(), 4);
        assert_eq!(std::fs::read_to_string(&worker.script).unwrap(), "called\n");
        assert_eq!(
            worker.status(handle(), true).await.unwrap()["status"],
            "completed"
        );
        let reopened = CadWorker::new(
            Arc::clone(&worker.workspace),
            worker.python.clone(),
            worker.script.clone(),
        );
        assert_eq!(reopened.status(handle(), false).await.unwrap(), done);
        let schema =
            crate::resources::contracts::read("printable://contracts/cad_build/status").unwrap();
        jsonschema::validator_for(&schema["outputSchema"])
            .unwrap()
            .validate(&done)
            .unwrap();
    }

    #[tokio::test]
    async fn background_cancel_is_terminal_and_busy_admission_has_no_output() {
        let root = tempfile::tempdir().unwrap();
        let worker = lifecycle_worker(root.path());
        let mut input = serde_json::to_value(request()).unwrap();
        input["params"]["background"] = json!(true);
        let admitted = worker
            .build(serde_json::from_value(input.clone()).unwrap())
            .await
            .unwrap();
        assert_eq!(admitted["status"], "admitted");
        input["params"]["output_dir"] = json!("builds/busy");
        assert!(
            worker
                .build(serde_json::from_value(input).unwrap())
                .await
                .is_err()
        );
        assert!(!root.path().join("projects/cad/builds/busy").exists());
        wait_started(&worker).await;
        let requested = worker.status(handle(), true).await.unwrap();
        assert_eq!(requested["cancel_requested"], true);
        let cancelled = terminal(&worker).await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(
            worker
                .workspace
                .read_artifact("projects/cad/builds/one/inputs/source.py")
                .unwrap()
                .1,
            b"original source"
        );
        assert_eq!(
            worker.status(handle(), true).await.unwrap()["status"],
            "cancelled"
        );
    }

    #[tokio::test]
    async fn cancellation_and_completion_have_one_retained_terminal_outcome() {
        let root = tempfile::tempdir().unwrap();
        let worker = lifecycle_worker(root.path());
        let mut input = serde_json::to_value(request()).unwrap();
        input["params"]["background"] = json!(true);
        worker
            .build(serde_json::from_value(input).unwrap())
            .await
            .unwrap();
        wait_started(&worker).await;
        std::fs::write(worker.script.with_extension("release"), b"").unwrap();
        worker.status(handle(), true).await.unwrap();
        let done = terminal(&worker).await;
        assert!(matches!(
            done["status"].as_str(),
            Some("completed" | "cancelled")
        ));
        if done["status"] == "completed" {
            for artifact in done["result"]["artifacts"].as_array().unwrap() {
                assert!(
                    worker
                        .workspace
                        .stat_artifact(artifact["artifact"]["path"].as_str().unwrap())
                        .is_ok()
                );
            }
        }
        assert_eq!(worker.status(handle(), false).await.unwrap(), done);
    }

    #[tokio::test]
    async fn native_deadline_and_restart_are_distinct_from_active_progress() {
        let root = tempfile::tempdir().unwrap();
        let worker = lifecycle_worker(root.path());
        let mut input = serde_json::to_value(request()).unwrap();
        input["params"]["background"] = json!(true);
        input["params"]["timeout_seconds"] = json!(1);
        worker
            .build(serde_json::from_value(input).unwrap())
            .await
            .unwrap();
        let failed = terminal(&worker).await;
        assert_eq!(failed["status"], "failed");
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("execution deadline")
        );
        worker
            .workspace
            .write_artifact(
                "projects/cad/builds/one/state.json",
                &serde_json::to_vec(
                    &json!({"build":handle(),"status":"running","phase":"native_execution"}),
                )
                .unwrap(),
                true,
            )
            .unwrap();
        let restarted = CadWorker::new(
            Arc::clone(&worker.workspace),
            worker.python.clone(),
            worker.script.clone(),
        );
        assert_eq!(
            restarted.status(handle(), false).await.unwrap()["status"],
            "interrupted"
        );
    }

    #[test]
    fn native_output_rejects_fifo_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(open_native_file(&path), Err(ToolError::Cad(_))));
    }

    #[test]
    fn rejects_cross_project_paths_and_invalid_budgets_before_work() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = workspace(directory.path());
        for (field, value) in [
            ("source", json!("../other/source.py")),
            ("output_dir", json!("../other")),
            ("linear_tolerance_mm", json!(-1)),
            ("timeout_seconds", json!(0)),
            ("qualification", json!({"dimensions_mm":[0,20,30]})),
            ("qualification", json!({"dimension_tolerance_mm":-0.1})),
            ("qualification", json!({"solid_count":0})),
        ] {
            let mut input = serde_json::to_value(request()).unwrap();
            input["params"][field] = value;
            let request: CadRequest = serde_json::from_value(input).unwrap();
            assert!(request.validate(&workspace).is_err());
        }
        assert!(!directory.path().join("projects/cad/builds").exists());
    }

    #[tokio::test]
    async fn legacy_status_remains_readable_without_invented_qualification() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = workspace(directory.path());
        let report = json!({"build_directory":"projects/cad/builds/one","report":{"valid":true},"artifacts":[]});
        workspace
            .write_artifact(
                "projects/cad/builds/one/report.json",
                &serde_json::to_vec(&report).unwrap(),
                false,
            )
            .unwrap();
        let worker = CadWorker::new(workspace, "/bin/false".into(), "unused".into());
        let state = worker.status(handle(), false).await.unwrap();
        assert_eq!(state["history"], "legacy_report");
        assert_eq!(state["result"], report);
        let contract =
            crate::resources::contracts::read("printable://contracts/cad_build/status").unwrap();
        let validator = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
        validator.validate(&state).unwrap();
        let mut malformed = state;
        malformed["result"]["qualification"] = json!({"status":"invented"});
        assert!(!validator.is_valid(&malformed));
    }

    #[tokio::test]
    async fn retains_sources_and_publishes_terminal_report_without_overwriting_a_build() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = workspace(directory.path());
        let fake = directory.path().join("fake-cad");
        std::fs::write(&fake, b"#!/usr/bin/python3\nimport pathlib,sys,json\np=pathlib.Path(sys.argv[-1])/'output'\np.mkdir()\nfor name in ['model.step','model.stl','model.glb','components.json']:\n (p/name).write_text('fake artifact')\n(p/'report.json').write_text(json.dumps({'valid':True,'units':'mm','solid_count':1,'solid_volumes_mm3':[6000],'bounds_mm':{'size':[10,20,30]}}))\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = Arc::new(CadWorker::new(
            Arc::clone(&workspace),
            fake,
            "unused".into(),
        ));
        let output = worker.build(request()).await.unwrap();
        assert_eq!(output["completion"], "completed");
        assert_eq!(output["qualification"]["status"], "passed");
        let contract =
            crate::resources::contracts::read("printable://contracts/cad_build/model").unwrap();
        jsonschema::validator_for(&contract["outputSchema"])
            .unwrap()
            .validate(&output)
            .unwrap();
        assert_eq!(output["artifacts"].as_array().unwrap().len(), 4);
        let (_, retained) = workspace
            .read_artifact("projects/cad/builds/one/inputs/source.py")
            .unwrap();
        assert_eq!(retained, b"original source");
        let (_, report) = workspace
            .read_artifact("projects/cad/builds/one/report.json")
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&report).unwrap(), output);
        assert!(worker.build(request()).await.is_err());
        let (_, unchanged) = workspace
            .read_artifact("projects/cad/builds/one/report.json")
            .unwrap();
        assert_eq!(report, unchanged);

        let revision = projects::revisions::revise(&workspace, serde_json::from_value(json!({
            "project_id":"cad","expected_parent":null,"source":"source.py",
            "expected_source_sha256":projects::revisions::digest(b"original source"),
            "manifest":{"format_version":1,"units":"mm","requirements":{"fit":{"kind":"physical_test","description":"Physical fit needs a prototype"}}}
        })).unwrap()).unwrap();
        workspace
            .write_artifact("projects/cad/source.py", b"replacement source", true)
            .unwrap();
        let mut revised = serde_json::to_value(request()).unwrap();
        revised["params"]["output_dir"] = json!("builds/revision");
        revised["params"]["revision"] = revision["identity"].clone();
        let revised = worker
            .build(serde_json::from_value(revised).unwrap())
            .await
            .unwrap();
        assert_eq!(revised["revision"], revision["identity"]);
        assert_eq!(
            revised["requirements"]["criteria"]["fit"]["status"],
            "physical_test_required"
        );
        assert_eq!(
            workspace
                .read_artifact("projects/cad/builds/revision/inputs/source.py")
                .unwrap()
                .1,
            b"original source"
        );
        let measured = workspace
            .read_artifact(revised["measurement"]["path"].as_str().unwrap())
            .unwrap()
            .1;
        assert_eq!(
            projects::revisions::digest(&measured),
            revised["measurement"]["sha256"]
        );
        let measured: Value = serde_json::from_slice(&measured).unwrap();
        assert_eq!(measured["completion"], "completed");
        assert_eq!(measured["qualification"], revised["qualification"]);
        assert_eq!(measured["requirements"], revised["requirements"]);
        let state = worker
            .status(
                BuildHandle {
                    project_id: "cad".into(),
                    output_dir: "builds/revision".into(),
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(state["status"], "completed");
        assert_eq!(state["result"], revised);
        let status_contract =
            crate::resources::contracts::read("printable://contracts/cad_build/status").unwrap();
        jsonschema::validator_for(&status_contract["outputSchema"])
            .unwrap()
            .validate(&state)
            .unwrap();
        let schema =
            crate::resources::contracts::read("printable://contracts/cad_build/model").unwrap();
        jsonschema::validator_for(&schema["outputSchema"])
            .unwrap()
            .validate(&revised)
            .unwrap();
        let mut rejected = serde_json::to_value(request()).unwrap();
        rejected["params"]["output_dir"] = json!("builds/rejected");
        rejected["params"]["qualification"] =
            json!({"policy":"printable_part","dimensions_mm":[42,20,30]});
        let failed = worker
            .build(serde_json::from_value(rejected).unwrap())
            .await
            .unwrap();
        assert_eq!(failed["completion"], "completed");
        assert_eq!(failed["qualification"]["status"], "failed");
        assert_eq!(
            failed["qualification"]["criteria"]["dimensions"]["status"],
            "failed"
        );
        assert_eq!(failed["artifacts"].as_array().unwrap().len(), 4);
        assert!(
            workspace
                .read_artifact("projects/cad/builds/rejected/inputs/source.py")
                .is_ok()
        );
    }
}
