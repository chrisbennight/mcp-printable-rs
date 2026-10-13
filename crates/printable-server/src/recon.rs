//! Photo reconstruction of FLAME heads, delegated to the reconstruction worker.
//!
//! The worker runs one Python reconstruction at a time with a deadline. Input
//! photographs are copied out of the workspace into private snapshots first,
//! and the displacement map is committed back through the workspace, so the
//! Python process never touches workspace paths.

use std::{os::unix::process::CommandExt, path::PathBuf, sync::Arc, time::Duration};

use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};

use crate::error::ToolError;

/// Photographs accepted in one request.
pub const MAX_IMAGES: usize = 8;
/// Largest photograph copied out of the workspace.
pub(crate) const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest JSON response read from the Python process. Each photograph's
/// pixel labels travel in it as a small PNG.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Largest displacement map committed to the workspace.
const MAX_MAP_BYTES: u64 = 16 * 1024 * 1024;
const IMAGE_SUFFIXES: &[&str] = &[".jpg", ".jpeg", ".png", ".webp"];
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconRequest {
    /// Workspace photographs of one person, JPEG, PNG or WebP. The first one
    /// sets the expression and surface detail; all of them set the identity.
    pub images: Vec<String>,
    /// Workspace path for the 16-bit PNG displacement map.
    pub displacement_path: String,
    /// Replace existing files at `displacement_path` and `texture_path`.
    #[serde(default)]
    pub overwrite: bool,
    /// Workspace path for an RGB PNG colour map in FLAME's texture layout,
    /// projected from the photographs. Needs the FLAME 2020 generic model
    /// converted with its texture layout. Not made when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture_path: Option<String>,
    /// How the colour map colours the scalp (default `hair`): `hair` paints
    /// it the hair colour wherever the photographs do not show bare scalp,
    /// `skin` colours it as skin, and `photo` takes whatever the
    /// photographs show, hair included. `hair` and `skin` need a model
    /// converted with FLAME's masks to know where the scalp is. Needs
    /// `texture_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scalp: Option<ScalpChoice>,
    /// sRGB colour, each component 0 to 255, to paint the scalp with instead
    /// of the hair colour found in the photographs. Needs `scalp` `hair`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hair_colour: Option<[u8; 3]>,
    /// What becomes of the photographs' light and shade in the colour map
    /// (default `remove`): `remove` estimates each photograph's smooth
    /// shading over the skin and divides it out, keeping the typical skin
    /// colour and limiting highlights; `keep` leaves it as photographed.
    /// Needs `texture_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighting: Option<LightingChoice>,
}

/// What a colour map does with the photographs' lighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LightingChoice {
    #[default]
    Remove,
    Keep,
}

/// How a colour map colours the scalp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScalpChoice {
    #[default]
    Hair,
    Skin,
    Photo,
}

impl ReconRequest {
    /// Checks that need no file access.
    pub fn validate(&self) -> Result<(), ToolError> {
        if self.images.is_empty() || self.images.len() > MAX_IMAGES {
            return Err(ToolError::Validation(format!(
                "images must name between 1 and {MAX_IMAGES} photographs"
            )));
        }
        if let Some(image) = self
            .images
            .iter()
            .find(|image| !IMAGE_SUFFIXES.iter().any(|suffix| image.ends_with(suffix)))
        {
            return Err(ToolError::Validation(format!(
                "{image} is not a .jpg, .jpeg, .png or .webp photograph"
            )));
        }
        if !self.displacement_path.ends_with(".png") {
            return Err(ToolError::Validation(
                "displacement_path must end in .png".into(),
            ));
        }
        if let Some(texture) = &self.texture_path {
            if !texture.ends_with(".png") {
                return Err(ToolError::Validation(
                    "texture_path must end in .png".into(),
                ));
            }
            if *texture == self.displacement_path {
                return Err(ToolError::Validation(
                    "texture_path and displacement_path must differ".into(),
                ));
            }
        } else if self.scalp.is_some() || self.hair_colour.is_some() || self.lighting.is_some() {
            return Err(ToolError::Validation(
                "scalp, hair_colour and lighting need texture_path".into(),
            ));
        }
        if self.hair_colour.is_some() && self.scalp.unwrap_or_default() != ScalpChoice::Hair {
            return Err(ToolError::Validation("hair_colour needs scalp hair".into()));
        }
        Ok(())
    }
}

/// SHA-256 of a file read in pieces, as lowercase hexadecimal.
pub(crate) fn file_digest(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hex(&digest.finalize()));
        }
        digest.update(&buffer[..read]);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Where the reconstruction runtime and its operator-supplied files live.
#[derive(Debug, Clone)]
pub struct ReconConfig {
    pub python: PathBuf,
    pub script: PathBuf,
    pub checkpoints: PathBuf,
    pub landmarker: PathBuf,
    /// MediaPipe's multiclass selfie segmenter, which labels hair, skin and
    /// clothing for the colour map.
    pub segmenter: PathBuf,
    pub device: String,
    pub timeout: Duration,
}

pub struct ReconWorker {
    pub workspace: Arc<Workspace>,
    pub config: ReconConfig,
    pub admission: Semaphore,
}

impl ReconWorker {
    pub fn new(workspace: Arc<Workspace>, config: ReconConfig) -> Self {
        Self {
            workspace,
            config,
            admission: Semaphore::new(1),
        }
    }

    /// Checkpoint files the operator has not yet supplied.
    pub async fn missing_checkpoints(&self) -> Result<Vec<String>, ToolError> {
        let mut command = self.python_command();
        command.arg("--check").arg(&self.config.checkpoints);
        let output = tokio::time::timeout(CHECK_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                reconstruction_error("timeout", "the checkpoint check did not finish")
            })??;
        #[derive(Deserialize)]
        struct Check {
            missing: Vec<String>,
        }
        let check: Check = serde_json::from_slice(&output.stdout).map_err(|_| {
            reconstruction_error("protocol", "the checkpoint check returned no answer")
        })?;
        let mut missing = check.missing;
        if !self.config.landmarker.is_file() {
            missing.push("face landmarker model".into());
        }
        if !self.config.segmenter.is_file() {
            missing.push("person segmenter model".into());
        }
        Ok(missing)
    }

    pub async fn reconstruct(&self, request: ReconRequest) -> Result<Value, ToolError> {
        request.validate()?;
        self.workspace
            .validate_public_mutation_path(&request.displacement_path)?;
        if !request.overwrite
            && self
                .workspace
                .stat_artifact(&request.displacement_path)
                .is_ok()
        {
            return Err(
                printable_workspace::WsError::AlreadyExists(request.displacement_path).into(),
            );
        }
        let _permit = self.admission.try_acquire().map_err(|_| {
            reconstruction_error("busy", "another reconstruction is running; retry shortly")
        })?;

        let mut snapshots = Vec::with_capacity(request.images.len());
        for image in &request.images {
            snapshots.push(
                self.workspace
                    .snapshot_artifact_bounded(image, MAX_IMAGE_BYTES)?,
            );
        }
        let staging = tempfile::Builder::new()
            .prefix("printable-recon-")
            .tempdir()?;
        let map = staging.path().join("displacement.png");
        let job = json!({
            "images": snapshots.iter().map(|snapshot| snapshot.path()).collect::<Vec<_>>(),
            "checkpoints": self.config.checkpoints,
            "landmarker": self.config.landmarker,
            "segmenter": self.config.segmenter,
            "displacement": map,
            "device": self.config.device,
            "seed": 0,
        });
        let request_path = staging.path().join("request.json");
        std::fs::write(&request_path, serde_json::to_vec(&job)?)?;

        let mut command = self.python_command();
        command
            .arg(&request_path)
            .current_dir(staging.path())
            .env("HOME", staging.path());
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().map_err(|_| {
            reconstruction_error("unavailable", "the reconstruction runtime could not start")
        })?;
        let mut group = ProcessGroup(child.id());
        let stdout = child.stdout.take().expect("piped stdout");
        let run = async {
            let mut response = Vec::new();
            let mut bounded = stdout.take(MAX_RESPONSE_BYTES as u64 + 1);
            let (status, read) = tokio::join!(child.wait(), bounded.read_to_end(&mut response));
            read?;
            Ok::<_, std::io::Error>((status?, response))
        };
        let (status, response) = match tokio::time::timeout(self.config.timeout, run).await {
            Ok(result) => {
                // The child has been reaped, so its group id may be reused.
                group.disarm();
                result?
            }
            Err(_) => {
                drop(group);
                return Err(reconstruction_error(
                    "timeout",
                    format!(
                        "reconstruction did not finish within {} seconds",
                        self.config.timeout.as_secs()
                    ),
                ));
            }
        };
        let envelope: Value = serde_json::from_slice(&response).map_err(|_| {
            if status.success() {
                reconstruction_error("protocol", "the reconstruction runtime returned no answer")
            } else {
                reconstruction_error(
                    "resource_limit",
                    "reconstruction ran out of memory or stopped unexpectedly",
                )
            }
        })?;
        if let Some(error) = envelope.get("error") {
            let code = match error.get("code").and_then(Value::as_str) {
                Some("no_face") => "no_face",
                Some("checkpoints") => "checkpoints",
                Some("validation") => "validation",
                _ => "failed",
            };
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("reconstruction failed");
            return Err(reconstruction_error(code, message));
        }
        let mut result = envelope
            .get("result")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| {
                reconstruction_error("protocol", "the reconstruction answer has no result")
            })?;
        let artifact = self.workspace.commit_generated_artifact_bounded(
            &request.displacement_path,
            &map,
            request.overwrite,
            MAX_MAP_BYTES,
        )?;
        result["displacement"]["artifact"] = json!(artifact);
        // Callers that read the photographs themselves can confirm they saw
        // the same bytes.
        let digests = snapshots
            .iter()
            .map(|snapshot| file_digest(snapshot.path()))
            .collect::<Result<Vec<_>, _>>()?;
        result["image_sha256"] = json!(digests);
        Ok(result)
    }

    fn python_command(&self) -> Command {
        let mut command = Command::new(&self.config.python);
        command
            .arg("-I")
            .arg("-B")
            .arg(&self.config.script)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("OMP_NUM_THREADS", "4")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // The NVIDIA container runtime selects devices through these.
        for name in ["CUDA_VISIBLE_DEVICES", "NVIDIA_VISIBLE_DEVICES"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }
}

/// The text the worker reports for an error, without the server's prefix.
pub fn worker_message(error: &ToolError) -> String {
    match error {
        ToolError::Reconstruction { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

fn reconstruction_error(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError::Reconstruction {
        code,
        message: message.into(),
    }
}

/// Kills the child's whole process group, including any helpers it started,
/// unless the child has already been reaped.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(id) = self.0.and_then(|id| i32::try_from(id).ok()) {
            // SAFETY: kill has no memory-safety preconditions; the group was
            // created for this child alone.
            unsafe {
                libc::kill(-id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in reconstruction runtime: it echoes the request through the
    /// worker protocol, choosing its behaviour from the first image's name.
    const FAKE_RUNTIME: &str = r#"
import json, sys, time
if sys.argv[1:] == ["--check", sys.argv[-1]] and len(sys.argv) == 3:
    print(json.dumps({"missing": []}))
    sys.exit(0)
request = json.load(open(sys.argv[1]))
name = request["images"][0]
if "noface" in name:
    print(json.dumps({"error": {"code": "no_face", "message": "no face was found"}}))
    sys.exit(2)
if "slow" in name:
    time.sleep(30)
if "crash" in name:
    sys.exit(1)
open(request["displacement"], "wb").write(b"\x89PNG\r\n\x1a\nfake")
print(json.dumps({"result": {"identity": [0.5] * 300, "expression": [0.0] * 50, "jaw": [0.1, 0.0, 0.0],
    "images": len(request["images"]), "device": request["device"],
    "displacement": {"offset_mm": -1.0, "scale_mm": 2.0, "width": 256, "height": 256}}}))
"#;

    fn worker(timeout: Duration) -> (tempfile::TempDir, ReconWorker) {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("reconstruct.py"), FAKE_RUNTIME).unwrap();
        std::fs::write(runtime.join("face_landmarker.task"), b"model").unwrap();
        std::fs::write(runtime.join("segmenter.tflite"), b"model").unwrap();
        let workspace_root = root.path().join("workspace");
        std::fs::create_dir(&workspace_root).unwrap();
        for name in [
            "face.jpg",
            "side.png",
            "noface.jpg",
            "slow.jpg",
            "crash.jpg",
        ] {
            std::fs::write(workspace_root.join(name), b"image bytes").unwrap();
        }
        let workspace = Arc::new(Workspace::open(Some(&workspace_root), None).unwrap());
        let worker = ReconWorker::new(
            workspace,
            ReconConfig {
                python: "python3".into(),
                script: runtime.join("reconstruct.py"),
                checkpoints: runtime.clone(),
                landmarker: runtime.join("face_landmarker.task"),
                segmenter: runtime.join("segmenter.tflite"),
                device: "cpu".into(),
                timeout,
            },
        );
        (root, worker)
    }

    #[tokio::test]
    async fn photographs_are_snapshotted_and_the_map_is_committed() {
        let (root, worker) = worker(Duration::from_secs(60));
        assert!(worker.missing_checkpoints().await.unwrap().is_empty());
        let result = worker
            .reconstruct(request(&["face.jpg", "side.png"], "detail/map.png"))
            .await
            .unwrap();
        assert_eq!(result["images"], 2);
        assert_eq!(result["device"], "cpu");
        assert_eq!(result["displacement"]["artifact"]["path"], "detail/map.png");
        assert!(root.path().join("workspace/detail/map.png").is_file());
        // The digests identify the bytes the runtime was given.
        let digests: Vec<String> = ["face.jpg", "side.png"]
            .iter()
            .map(|name| file_digest(&root.path().join("workspace").join(name)).unwrap())
            .collect();
        assert_eq!(result["image_sha256"], json!(digests));
        // An existing map is kept unless overwrite is requested.
        let repeat = worker
            .reconstruct(request(&["face.jpg"], "detail/map.png"))
            .await;
        assert_eq!(repeat.unwrap_err().code(), "already_exists");
        let mut overwrite = request(&["face.jpg"], "detail/map.png");
        overwrite.overwrite = true;
        assert!(worker.reconstruct(overwrite).await.is_ok());
    }

    #[tokio::test]
    async fn failures_are_reported_with_their_cause() {
        let (_root, worker) = worker(Duration::from_secs(2));
        let code = |result: Result<Value, ToolError>| result.unwrap_err().code();
        assert_eq!(
            code(worker.reconstruct(request(&["noface.jpg"], "a.png")).await),
            "no_face"
        );
        assert_eq!(
            code(worker.reconstruct(request(&["crash.jpg"], "b.png")).await),
            "resource_limit"
        );
        assert_eq!(
            code(worker.reconstruct(request(&["absent.jpg"], "c.png")).await),
            "not_found"
        );
        let started = std::time::Instant::now();
        assert_eq!(
            code(worker.reconstruct(request(&["slow.jpg"], "d.png")).await),
            "timeout"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn one_reconstruction_runs_at_a_time() {
        let (_root, worker) = worker(Duration::from_secs(60));
        let _permit = worker.admission.try_acquire().unwrap();
        let busy = worker.reconstruct(request(&["face.jpg"], "e.png")).await;
        assert_eq!(busy.unwrap_err().code(), "busy");
    }

    fn request(images: &[&str], displacement: &str) -> ReconRequest {
        ReconRequest {
            images: images.iter().map(|image| (*image).to_owned()).collect(),
            displacement_path: displacement.into(),
            overwrite: false,
            texture_path: None,
            scalp: None,
            hair_colour: None,
            lighting: None,
        }
    }

    #[test]
    fn requests_name_photographs_and_a_png_map() {
        assert!(request(&["a.jpg", "b.png"], "map.png").validate().is_ok());
        assert!(request(&[], "map.png").validate().is_err());
        assert!(request(&["a.gif"], "map.png").validate().is_err());
        assert!(request(&["a.jpg"], "map.tif").validate().is_err());
        let many: Vec<String> = (0..=MAX_IMAGES)
            .map(|index| format!("{index}.jpg"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(request(&many, "map.png").validate().is_err());
        let with_texture = |texture: &str| ReconRequest {
            texture_path: Some(texture.into()),
            ..request(&["a.jpg"], "map.png")
        };
        assert!(with_texture("colour.png").validate().is_ok());
        assert!(with_texture("colour.jpg").validate().is_err());
        assert!(with_texture("map.png").validate().is_err());
        let coloured =
            |scalp: Option<ScalpChoice>, hair: Option<[u8; 3]>, texture: bool| ReconRequest {
                scalp,
                hair_colour: hair,
                texture_path: texture.then(|| "colour.png".into()),
                ..request(&["a.jpg"], "map.png")
            };
        assert!(
            coloured(Some(ScalpChoice::Skin), None, true)
                .validate()
                .is_ok()
        );
        assert!(coloured(None, Some([1, 2, 3]), true).validate().is_ok());
        assert!(
            coloured(Some(ScalpChoice::Hair), Some([1, 2, 3]), true)
                .validate()
                .is_ok()
        );
        for (scalp, hair, texture, message) in [
            (Some(ScalpChoice::Photo), None, false, "need texture_path"),
            (None, Some([1, 2, 3]), false, "need texture_path"),
            (
                Some(ScalpChoice::Skin),
                Some([1, 2, 3]),
                true,
                "needs scalp hair",
            ),
        ] {
            let error = coloured(scalp, hair, texture).validate().unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
        let unlit = ReconRequest {
            lighting: Some(LightingChoice::Keep),
            ..request(&["a.jpg"], "map.png")
        };
        assert!(
            unlit
                .validate()
                .unwrap_err()
                .to_string()
                .contains("need texture_path")
        );
    }
}

/// How often the server asks the worker whether it can reconstruct.
const READINESS_INTERVAL: Duration = Duration::from_secs(30);
/// The worker enforces its own deadline; this only bounds a lost connection.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3660);

/// The server's view of the reconstruction worker.
#[derive(Debug)]
pub struct ReconClient {
    endpoint: String,
    http: reqwest::Client,
    /// The worker's last readiness answer; subscribers hear each change.
    ready: tokio::sync::watch::Sender<bool>,
    /// Building a colour map holds a photograph, its depth buffer and the map
    /// in the server's memory, so the server builds one at a time.
    colour_maps: Arc<Semaphore>,
}

impl ReconClient {
    /// A client for `PRINTABLE_RECON_ENDPOINT`, or `None` when it is unset.
    pub fn configure(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Arc<Self>>, String> {
        let Some(endpoint) =
            get("PRINTABLE_RECON_ENDPOINT").filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let url = reqwest::Url::parse(endpoint.trim())
            .map_err(|_| "PRINTABLE_RECON_ENDPOINT must be an http URL".to_string())?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err("PRINTABLE_RECON_ENDPOINT must be an http URL".into());
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|error| format!("cannot initialize the reconstruction client: {error}"))?;
        Ok(Some(Arc::new(Self {
            endpoint: endpoint.trim().trim_end_matches('/').to_owned(),
            http,
            ready: tokio::sync::watch::Sender::new(false),
            colour_maps: Arc::new(Semaphore::new(1)),
        })))
    }

    /// Whether the worker last reported every checkpoint present.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Permission to build a colour map, or a busy error while another is
    /// being built.
    pub fn colour_map_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ToolError> {
        Arc::clone(&self.colour_maps)
            .try_acquire_owned()
            .map_err(|_| {
                reconstruction_error("busy", "another colour map is being made; retry shortly")
            })
    }

    /// A receiver that wakes whenever readiness changes.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.ready.subscribe()
    }

    /// Ask the worker now and remember the answer.
    pub async fn refresh(&self) -> bool {
        let ready = matches!(
            self.http
                .get(format!("{}/readyz", self.endpoint))
                .timeout(CHECK_TIMEOUT)
                .send()
                .await,
            Ok(response) if response.status().is_success()
        );
        let changed = self.ready.send_if_modified(|current| {
            let changed = *current != ready;
            *current = ready;
            changed
        });
        if changed {
            tracing::info!(ready, "reconstruction worker availability changed");
        }
        ready
    }

    /// Keep the readiness answer current until `cancel` fires.
    pub fn watch(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        tokio::spawn(async move {
            loop {
                self.refresh().await;
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(READINESS_INTERVAL) => {}
                }
            }
        });
    }

    pub async fn reconstruct(&self, request: &ReconRequest) -> Result<Value, ToolError> {
        request.validate()?;
        let response = self
            .http
            .post(format!("{}/reconstruct", self.endpoint))
            .timeout(REQUEST_TIMEOUT)
            .json(request)
            .send()
            .await
            .map_err(|_| {
                reconstruction_error(
                    "outcome_unknown",
                    "the reconstruction worker could not be reached or the connection was lost; \
                     check displacement_path before retrying, because the map may have been written",
                )
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|_| {
            reconstruction_error(
                "outcome_unknown",
                "the reconstruction answer was interrupted; check displacement_path before retrying",
            )
        })?;
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            reconstruction_error("protocol", "the reconstruction worker returned no answer")
        })?;
        if status.is_success() {
            return Ok(value);
        }
        let code = match value.get("code").and_then(Value::as_str) {
            Some("no_face") => "no_face",
            Some("busy") => "busy",
            Some("timeout") => "timeout",
            Some("checkpoints") => "checkpoints",
            Some("resource_limit") => "resource_limit",
            Some("validation") => "validation",
            Some("already_exists") => "already_exists",
            _ => "failed",
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the reconstruction worker refused the request");
        Err(reconstruction_error(code, message))
    }
}
