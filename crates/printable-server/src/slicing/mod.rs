//! Durable, explicitly configured Orca preparation in an isolated native worker.

pub mod profiles;
pub mod review;
pub mod setup;
#[cfg(test)]
mod tests;

use crate::{error::ToolError, projects, upload::random_hex_id};
use printable_workspace::Workspace;
use profiles::{Category, ProfileQuery, ProfileSelection, Profiles};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::unix::pipe,
    process::Command,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;

const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LOG: usize = 1024 * 1024;
pub const ENGINE_VERSION: &str = "2.4.2";

pub async fn forward(endpoint: Option<&str>, request: SliceRequest) -> Result<Value, ToolError> {
    let endpoint = endpoint.ok_or_else(|| slice_error("slicer worker is not configured"))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| slice_error("cannot initialize slicer connection"))?;
    let mut response=client.post(format!("{}/slice",endpoint.trim_end_matches('/'))).json(&request).send().await
        .map_err(|_|slice_error("slicer connection failed; inspect the output directory before retrying preparation"))?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| slice_error("slicer response interrupted; inspect the retained state"))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(slice_error("slicer response exceeds metadata limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if !status.is_success() {
        return Err(slice_error(
            value["error"]
                .as_str()
                .unwrap_or("slicer rejected the request"),
        ));
    }
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareParams {
    pub project_id: String,
    /// Project-relative STL or 3MF. STEP must first be converted by cad_build.
    pub source: String,
    /// New project-relative directory retaining source, settings, state and outputs.
    pub output_dir: String,
    /// Physical upward-facing surface, independent of the numbered project plate.
    pub build_plate: setup::BuildPlate,
    pub printer: ProfileSelection,
    pub process: ProfileSelection,
    /// Ordered material slots, matching the source plate's filament indices.
    pub filaments: Vec<ProfileSelection>,
    /// Orca plate index, one-based; zero slices every plate.
    #[serde(default = "default_plate")]
    pub plate: u16,
    #[serde(default)]
    pub auto_orient: bool,
    #[serde(default)]
    pub auto_arrange: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_plate() -> u16 {
    1
}
fn default_timeout() -> u64 {
    1800
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SliceHandle {
    pub project_id: String,
    pub output_dir: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SliceRequest {
    Profiles(ProfileQuery),
    Settings(profiles::SettingsQuery),
    Prepare(PrepareParams),
    Status(SliceHandle),
    Cancel(SliceHandle),
    Review(review::ReviewParams),
}

struct Active {
    output: String,
    state: Value,
    cancel: CancellationToken,
}

#[derive(Deserialize, Serialize)]
struct NativeProgress {
    message: String,
    total_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plate_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plate_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plate_count: Option<u32>,
}

pub struct SliceWorker {
    workspace: Arc<Workspace>,
    profiles: Profiles,
    binary: PathBuf,
    admission: Arc<Semaphore>,
    active: Mutex<Option<Active>>,
}

impl SliceWorker {
    pub fn new(workspace: Arc<Workspace>, profiles: Profiles, binary: PathBuf) -> Self {
        Self {
            workspace,
            profiles,
            binary,
            admission: Arc::new(Semaphore::new(1)),
            active: Mutex::new(None),
        }
    }

    pub async fn dispatch(self: &Arc<Self>, request: SliceRequest) -> Result<Value, ToolError> {
        match request {
            SliceRequest::Profiles(query) => self.profiles.discover(query),
            SliceRequest::Settings(query) => self.profiles.settings(query),
            SliceRequest::Prepare(params) => self.prepare(params).await,
            SliceRequest::Status(handle) => self.status(handle, false).await,
            SliceRequest::Cancel(handle) => self.status(handle, true).await,
            SliceRequest::Review(params) => self.review(params).await,
        }
    }

    async fn review(&self, params: review::ReviewParams) -> Result<Value, ToolError> {
        let state = self.status(params.slice.clone(), false).await?;
        if state["status"] != "completed" {
            return Err(slice_error("toolpath review requires a completed slice"));
        }
        if !params.toolpath.ends_with(".gcode") {
            return Err(invalid("review requires a G-code artifact from the slice"));
        }
        let selected = state["artifacts"]
            .get(&params.toolpath)
            .ok_or_else(|| invalid("toolpath is not an artifact of this slice"))?;
        let path = selected["artifact"]["path"]
            .as_str()
            .ok_or_else(|| slice_error("slice artifact has no path"))?
            .to_owned();
        let expected_hash = selected["sha256"]
            .as_str()
            .ok_or_else(|| slice_error("slice artifact has no hash"))?
            .to_owned();
        let output = slice_directory(
            &self.workspace,
            &params.slice.project_id,
            &params.slice.output_dir,
        )?;
        if path != format!("{output}/{}", params.toolpath) || params.toolpath.contains('/') {
            return Err(invalid(
                "toolpath must belong to the selected slice directory",
            ));
        }
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| {
                slice_error("slicer is busy; retry review after the active operation completes")
            })?;
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let snapshot = workspace.snapshot_artifact_bounded(&path, MAX_BYTES)?;
            if hash_file(snapshot.path())? != expected_hash {
                return Err(slice_error(
                    "toolpath changed since slicing; prepare a new revision",
                ));
            }
            let (bytes, mut evidence) = review::render(snapshot.path(), &params)?;
            let review_id = random_hex_id()?;
            let image_path = format!("{output}/review-{review_id}.png");
            let artifact = workspace.write_artifact(&image_path, &bytes, false)?;
            evidence["source_sha256"] = json!(expected_hash);
            evidence["slice"] = json!(params.slice);
            evidence["toolpath"] = json!(path);
            evidence["image"] = json!(artifact);
            let metadata_path = format!("{output}/review-{review_id}.json");
            workspace.write_artifact(&metadata_path, &serde_json::to_vec(&evidence)?, false)?;
            evidence["metadata_path"] = json!(metadata_path);
            Ok(evidence)
        })
        .await
        .map_err(|_| slice_error("toolpath review task failed"))?
    }

    async fn status(&self, handle: SliceHandle, cancel: bool) -> Result<Value, ToolError> {
        let output = slice_directory(&self.workspace, &handle.project_id, &handle.output_dir)?;
        let active = self.active.lock().await;
        if let Some(job) = active.as_ref().filter(|job| job.output == output) {
            if cancel {
                job.cancel.cancel();
            }
            let mut state = job.state.clone();
            state["cancel_requested"] = json!(job.cancel.is_cancelled());
            return Ok(state);
        }
        let (_, bytes) = self
            .workspace
            .read_artifact(&format!("{output}/state.json"))?;
        let mut state: Value = serde_json::from_slice(&bytes)?;
        if state["status"] == "running" {
            state["status"] = json!("interrupted");
            state["error"] = json!(
                "worker restarted or could not persist completion; inspect retained outputs before preparing a new revision"
            );
        }
        Ok(state)
    }

    async fn prepare(self: &Arc<Self>, params: PrepareParams) -> Result<Value, ToolError> {
        if !(1..=7200).contains(&params.timeout_seconds)
            || !(1..=16).contains(&params.filaments.len())
        {
            return Err(invalid(
                "slicing requires 1–16 material profiles and a 1–7200 second budget",
            ));
        }
        let extension = Path::new(&params.source)
            .extension()
            .and_then(|x| x.to_str())
            .filter(|x| ["stl", "3mf"].contains(x))
            .ok_or_else(|| invalid("slicing accepts STL or 3MF"))?;
        let source_path = projects::resolve(&self.workspace, &params.project_id, &params.source)?;
        let output = slice_directory(&self.workspace, &params.project_id, &params.output_dir)?;
        let printer = self.profiles.resolve(Category::Printer, &params.printer)?;
        let process = self.profiles.resolve(Category::Process, &params.process)?;
        let filaments = params
            .filaments
            .iter()
            .map(|x| self.profiles.resolve(Category::Filament, x))
            .collect::<Result<Vec<_>, _>>()?;
        if !profiles::compatible(&process, &params.printer.name)
            || filaments
                .iter()
                .any(|x| !profiles::compatible(x, &params.printer.name))
        {
            return Err(invalid(
                "process and filament profiles must explicitly support the selected printer/nozzle profile",
            ));
        }
        if printer["machine_start_gcode"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(invalid(
                "resolved printer profile lacks its native start G-code",
            ));
        }
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| slice_error("slicer is busy; inspect the active slice before retrying"))?;
        let staging = Arc::new(
            self.workspace
                .scratch(3 * MAX_BYTES + MAX_LOG as u64, "slice")?,
        );
        let source_name = format!("source.{extension}");
        let local_source = staging.path().join(&source_name);
        let workspace = Arc::clone(&self.workspace);
        let copy_to = local_source.clone();
        let staging_owner = Arc::clone(&staging);
        let source_hash = tokio::task::spawn_blocking(move || {
            let _staging_owner = staging_owner;
            let snapshot = workspace.snapshot_artifact_bounded(&source_path, MAX_BYTES)?;
            std::fs::copy(snapshot.path(), &copy_to)?;
            hash_file(&copy_to)
        })
        .await
        .map_err(|_| slice_error("source snapshot task failed"))??;
        let summary = setup::summary(&params, &printer, &process, &filaments);
        let settings = json!({"build_plate":params.build_plate,"printer":printer,"process":process,"filaments":filaments});
        for (name, value) in [
            ("printer.json", &settings["printer"]),
            ("process.json", &settings["process"]),
        ] {
            std::fs::write(staging.path().join(name), serde_json::to_vec(value)?)?;
        }
        for (index, value) in filaments.iter().enumerate() {
            std::fs::write(
                staging.path().join(format!("filament-{index}.json")),
                serde_json::to_vec(value)?,
            )?;
        }
        let native_output = staging.path().join("output");
        std::fs::create_dir(&native_output)?;
        let id = random_hex_id()?;
        let handle = SliceHandle {
            project_id: params.project_id.clone(),
            output_dir: params.output_dir.clone(),
        };
        let state = json!({"id":id,"slice":handle,"status":"running","engine":{"name":"OrcaSlicer","version":ENGINE_VERSION},
            "source_sha256":source_hash,"setup":summary,"cancel_requested":false,"progress":null});
        if !self.workspace.create_public_directory(&output)? {
            return Err(invalid(
                "slice output directory exists; inspect its state and choose a new directory for a revision",
            ));
        }
        self.workspace.commit_generated_artifact_bounded(
            &format!("{output}/{source_name}"),
            &local_source,
            false,
            MAX_BYTES,
        )?;
        self.workspace.write_artifact(
            &format!("{output}/request.json"),
            &serde_json::to_vec(&params)?,
            false,
        )?;
        self.workspace.write_artifact(
            &format!("{output}/settings.json"),
            &serde_json::to_vec(&settings)?,
            false,
        )?;
        self.workspace.write_artifact(
            &format!("{output}/state.json"),
            &serde_json::to_vec(&state)?,
            false,
        )?;
        let cancel = CancellationToken::new();
        *self.active.lock().await = Some(Active {
            output: output.clone(),
            state: state.clone(),
            cancel: cancel.clone(),
        });
        let worker = Arc::clone(self);
        let response = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result = worker
                .execute(staging.path(), &local_source, &params, &cancel)
                .await;
            let mut active = worker.active.lock().await;
            let mut terminal = active
                .as_ref()
                .expect("active slice owns admission")
                .state
                .clone();
            match result {
                Ok(artifacts) => {
                    terminal["status"] = json!("completed");
                    terminal["artifacts"] = artifacts;
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
            let persisted = serde_json::to_vec(&terminal)
                .map_err(ToolError::from)
                .and_then(|bytes| {
                    worker
                        .workspace
                        .write_artifact(&format!("{output}/state.json"), &bytes, true)
                        .map(|_| ())
                        .map_err(ToolError::from)
                });
            if let Err(error) = persisted {
                tracing::error!(
                    code = error.code(),
                    "could not persist terminal slice state"
                );
            }
            *active = None;
        });
        Ok(response)
    }

    async fn execute(
        self: &Arc<Self>,
        staging: &Path,
        source: &Path,
        params: &PrepareParams,
        cancel: &CancellationToken,
    ) -> Result<Value, ToolError> {
        use std::os::unix::process::CommandExt;
        let output = staging.join("output");
        let progress_path = staging.join("progress.fifo");
        if !Command::new("/usr/bin/mkfifo")
            .arg(&progress_path)
            .status()
            .await?
            .success()
        {
            return Err(slice_error("cannot create native progress pipe"));
        }
        let progress_pipe = pipe::OpenOptions::new()
            .read_write(true)
            .open_receiver(&progress_path)?;
        let profile_paths = (0..params.filaments.len())
            .map(|i| {
                staging
                    .join(format!("filament-{i}.json"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join(";");
        let mut command = Command::new(&self.binary);
        command
            .args([
                "--curr-bed-type",
                params.build_plate.native_name(),
                "--slice",
                &params.plate.to_string(),
                "--export-3mf",
                "model.gcode.3mf",
                "--outputdir",
            ])
            .arg(&output)
            .arg("--pipe")
            .arg(progress_path)
            .arg("--load-settings")
            .arg(format!(
                "{};{}",
                staging.join("printer.json").display(),
                staging.join("process.json").display()
            ))
            .args([
                "--load-filaments",
                &profile_paths,
                "--arrange",
                if params.auto_arrange { "1" } else { "0" },
                "--orient",
                if params.auto_orient { "1" } else { "0" },
            ])
            .arg(source)
            .current_dir(staging)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", staging)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn()?;
        let group = ProcessGroup(child.id().expect("spawned child"));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_task = tokio::spawn(read_log(stdout));
        let err_task = tokio::spawn(read_log(stderr));
        let worker = Arc::clone(self);
        let progress_task = tokio::spawn(async move {
            if let Err(error) = worker.read_progress(progress_pipe).await {
                tracing::warn!(kind = ?error.kind(), "native slicer progress stream failed");
            }
        });
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(slice_error("slice cancelled")),
            _ = tokio::time::sleep(Duration::from_secs(params.timeout_seconds)) => Err(slice_error("slice exceeded its deadline")),
            result = child.wait() => result.map_err(ToolError::from).and_then(|status| if status.success() {Ok(())} else {Err(slice_error("native slicing failed; inspect build-log.json"))})
        };
        drop(group);
        let _ = child.wait().await;
        progress_task.abort();
        let _ = progress_task.await;
        let stdout = out_task
            .await
            .map_err(|_| slice_error("stdout collection failed"))??;
        let stderr = err_task
            .await
            .map_err(|_| slice_error("stderr collection failed"))??;
        let destination = slice_directory(&self.workspace, &params.project_id, &params.output_dir)?;
        self.workspace.write_artifact(
            &format!("{destination}/build-log.json"),
            &serde_json::to_vec(&json!({"stdout":stdout,"stderr":stderr}))?,
            false,
        )?;
        result?;
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || {
            let mut artifacts = BTreeMap::new();
            for entry in std::fs::read_dir(&output)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("invalid native output name"))?;
                if ![Some("gcode"), Some("3mf"), Some("json")]
                    .contains(&entry.path().extension().and_then(|x| x.to_str()))
                {
                    continue;
                }
                if artifacts.len() >= 128 {
                    return Err(slice_error(
                        "native output set exceeds supported plate count",
                    ));
                }
                let hash = hash_file(&entry.path())?;
                let artifact = workspace.commit_generated_artifact_bounded(
                    &format!("{destination}/{name}"),
                    &entry.path(),
                    false,
                    MAX_BYTES,
                )?;
                artifacts.insert(name, json!({"artifact":artifact,"sha256":hash}));
            }
            if !artifacts.contains_key("model.gcode.3mf")
                || !artifacts.keys().any(|x| x.ends_with(".gcode"))
            {
                return Err(slice_error(
                    "native slice omitted its 3MF or toolpath; retained outputs require inspection",
                ));
            }
            Ok(json!(artifacts))
        })
        .await
        .map_err(|_| slice_error("slice publication task failed"))?
    }

    async fn read_progress(&self, pipe: pipe::Receiver) -> Result<(), std::io::Error> {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        let mut oversized = false;
        loop {
            line.clear();
            if (&mut reader)
                .take(4097)
                .read_until(b'\n', &mut line)
                .await?
                == 0
            {
                return Ok(());
            }
            oversized |= line.len() > 4096;
            if line.last() != Some(&b'\n') {
                oversized = true;
                continue;
            }
            if std::mem::take(&mut oversized) {
                continue;
            }
            let Ok(mut progress) = serde_json::from_slice::<NativeProgress>(&line) else {
                continue;
            };
            if !(0.0..=100.0).contains(&progress.total_percent)
                || progress
                    .plate_percent
                    .is_some_and(|percent| !(0.0..=100.0).contains(&percent))
            {
                continue;
            }
            progress.message = progress.message.chars().take(256).collect();
            let value = serde_json::to_value(progress).map_err(std::io::Error::other)?;
            self.active
                .lock()
                .await
                .as_mut()
                .expect("progress reader belongs to active slice")
                .state["progress"] = value;
        }
    }
}

fn slice_directory(
    workspace: &Workspace,
    project_id: &str,
    directory: &str,
) -> Result<String, ToolError> {
    let path = projects::resolve(workspace, project_id, &format!("{directory}/state.json"))?;
    Ok(path
        .strip_suffix("/state.json")
        .expect("resolved state path")
        .to_owned())
}

pub fn hash_file(path: &Path) -> Result<String, ToolError> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(slice_error(
            "slice artifact requires a regular file below 1 GiB",
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    let mut count = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > MAX_BYTES {
            return Err(slice_error("slice artifact grew beyond its byte limit"));
        }
        digest.update(&buffer[..n]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

async fn read_log(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Result<String, std::io::Error> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let n = pipe.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(MAX_LOG.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}
fn slice_error(message: &str) -> ToolError {
    ToolError::Slice(message.into())
}
