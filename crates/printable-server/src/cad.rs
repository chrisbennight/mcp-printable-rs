//! Project CAD builds delegated to a dedicated native worker.

pub mod qualification;

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};

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
    #[serde(default)]
    pub qualification: qualification::Requirements,
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
}

impl CadRequest {
    fn parts(&self) -> (&str, std::borrow::Cow<'_, BuildParams>) {
        match self {
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
                    qualification: params.qualification.clone(),
                }),
            ),
        }
    }

    fn validate(&self, workspace: &Workspace) -> Result<String, ToolError> {
        let (action, p) = self.parts();
        p.qualification.validate()?;
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
    let timeout = request.parts().1.timeout_seconds;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout + 30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ToolError::Cad("cannot initialize CAD connection".into()))?;
    let mut response = client
        .post(format!("{}/build", endpoint.trim_end_matches('/')))
        .json(&request)
        .send()
        .await
        .map_err(|_| {
            ToolError::Cad(
                "CAD connection failed; inspect the build directory before retrying".into(),
            )
        })?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ToolError::Cad("CAD response interrupted; inspect the build directory".into())
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
    pub admission: Semaphore,
}

impl CadWorker {
    pub async fn build(&self, request: CadRequest) -> Result<Value, ToolError> {
        let output = request.validate(&self.workspace)?;
        let _permit = self.admission.try_acquire().map_err(|_| {
            ToolError::Cad("CAD worker is busy; retry after the active build finishes".into())
        })?;
        let workspace = Arc::clone(&self.workspace);
        let (action, params) = request.parts();
        let staging = tempfile::tempdir()?;
        let input_root = staging.path().join("inputs");
        std::fs::create_dir(&input_root)?;
        let mut names = params.inputs.clone();
        names.push(params.source.clone());
        names.sort();
        names.dedup();
        let id = params.project_id.clone();
        let staging_inputs = input_root.clone();
        let sources = tokio::task::spawn_blocking(move || {
            let mut sources = Vec::new();
            let mut total = 0;
            for name in names {
                let path = projects::resolve(&workspace, &id, &name)?;
                let snapshot = workspace.snapshot_artifact_bounded(&path, MAX_FILE_BYTES)?;
                total += snapshot.meta().size_bytes;
                if total > MAX_FILE_BYTES { return Err(ToolError::Cad("CAD input set exceeds 1 GiB".into())); }
                let destination = staging_inputs.join(&name);
                std::fs::create_dir_all(destination.parent().expect("input parent"))?;
                std::fs::copy(snapshot.path(), &destination)?;
                sources.push(json!({"path":path,"snapshot":format!("inputs/{name}"),"sha256":hash_file(&destination)?}));
            }
            Ok::<_,ToolError>(sources)
        }).await.map_err(|_| ToolError::Cad("CAD input staging task failed".into()))??;
        let native = json!({"action":action,"source":format!("inputs/{}",params.source),"parameters":params.parameters,"linear_tolerance_mm":params.linear_tolerance_mm,"angular_tolerance_rad":params.angular_tolerance_rad});
        std::fs::write(
            staging.path().join("request.json"),
            serde_json::to_vec(&native)?,
        )?;
        if !self.workspace.create_public_directory(&output)? {
            return Err(ToolError::Cad("build directory already exists; inspect report.json or failure.json, then choose a new output_dir for a revision".into()));
        }
        let result = async {
            self.workspace.write_artifact(
                &format!("{output}/request.json"),
                &serde_json::to_vec(&json!({"request":request,"sources":sources}))?,
                false,
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
            let result = self.execute(staging.path(), params.timeout_seconds).await;
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
                Ok(()) => self.commit(staging.path(), &output, &params.qualification),
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

    async fn execute(&self, directory: &Path, timeout: u64) -> Result<(), ToolError> {
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
        tokio::time::timeout(Duration::from_secs(timeout), run)
            .await
            .map_err(|_| ToolError::Cad("CAD build exceeded its deadline".into()))?
    }

    fn commit(
        &self,
        staging: &Path,
        output: &str,
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
        let result = json!({"completion":"completed","qualification":qualification,"report":report,"artifacts":artifacts,"build_directory":output});
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
    async fn retains_sources_and_publishes_terminal_report_without_overwriting_a_build() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = workspace(directory.path());
        let fake = directory.path().join("fake-cad");
        std::fs::write(&fake, b"#!/usr/bin/python3\nimport pathlib,sys,json\np=pathlib.Path(sys.argv[-1])/'output'\np.mkdir()\nfor name in ['model.step','model.stl','model.glb','components.json']:\n (p/name).write_text('fake artifact')\n(p/'report.json').write_text(json.dumps({'valid':True,'units':'mm','solid_count':1,'solid_volumes_mm3':[6000],'bounds_mm':{'size':[10,20,30]}}))\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = CadWorker {
            workspace: Arc::clone(&workspace),
            python: fake,
            script: "unused".into(),
            admission: Semaphore::new(1),
        };
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
