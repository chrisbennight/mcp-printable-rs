//! Photo reconstruction of FLAME heads, delegated to the reconstruction worker.
//!
//! The worker runs one Python reconstruction at a time with a deadline. Input
//! photographs are copied out of the workspace into private snapshots first,
//! and the displacement map is committed back through the workspace, so the
//! Python process never touches workspace paths.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
    /// Workspace photographs of one person, JPEG, PNG or WebP. All of them
    /// set the identity; the primary one, the first unless `primary_image`
    /// says otherwise, sets the expression and surface detail. One front
    /// view and two three-quarter views, evenly lit, with the hair off the
    /// ears and a relaxed expression, work best.
    pub images: Vec<String>,
    /// The photograph that sets the expression and surface detail: an index
    /// into `images`, or `most_frontal` for the one whose head is turned
    /// least from the camera. The first by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_image: Option<PrimaryImage>,
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
    /// Side of the colour map in texels: 512, 1024 (the default) or 2048.
    /// Needs `texture_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture_size: Option<u32>,
    /// How the colour map colours the eyeballs (default `photo`): `photo`
    /// takes the photographs' colour where they see the eyeball itself, and
    /// `painted` paints a clean iris and pupil around each eye's gaze and
    /// white elsewhere, at life size. Needs `texture_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eyes: Option<EyeChoice>,
    /// sRGB iris colour, each component 0 to 255, for painted eyes. When
    /// omitted it is measured from what the photographs show of the iris.
    /// Needs `eyes` `painted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iris_colour: Option<[u8; 3]>,
    /// sRGB colour of the white of painted eyes (default 240, 236, 230).
    /// Needs `eyes` `painted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sclera_colour: Option<[u8; 3]>,
    /// Workspace path for an RGB PNG picture of the fit: for each
    /// photograph, the fitted head's outline and the edges of its parts drawn
    /// over the photograph, beside the head alone, shaded grey, as the
    /// photograph's camera sees it. Not made when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_path: Option<String>,
    /// Share, 0 to 1 (default 0), of the head's tilt in the primary
    /// photograph, its pitch and roll, to keep in a suggested `pose.neck`,
    /// so a bust can keep the subject's natural tilt. The server applies
    /// it; the worker never sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neck_tilt: Option<f64>,
    /// Share, 0 to 1 (default 0), of the head's turn in the primary
    /// photograph, its yaw, to keep in a suggested `pose.neck`. A turn
    /// reflects where the camera stood as much as the subject. The server
    /// applies it; the worker never sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neck_turn: Option<f64>,
}

/// Which photograph sets the expression and surface detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum PrimaryImage {
    Index(usize),
    Rule(PrimaryRule),
}

/// A rule for choosing the primary photograph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryRule {
    MostFrontal,
}

/// What a colour map does with the photographs' lighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LightingChoice {
    #[default]
    Remove,
    Keep,
}

/// How a colour map colours the eyeballs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EyeChoice {
    #[default]
    Photo,
    Painted,
}

/// Colour map sides a caller may ask for, in texels.
pub const TEXTURE_SIZES: [u32; 3] = [512, 1024, 2048];

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
    /// The request without the options the server applies itself.
    fn for_worker(&self) -> Self {
        Self {
            neck_tilt: None,
            neck_turn: None,
            texture_size: None,
            eyes: None,
            iris_colour: None,
            sclera_colour: None,
            fit_path: None,
            ..self.clone()
        }
    }

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
        if let Some(PrimaryImage::Index(index)) = self.primary_image
            && index >= self.images.len()
        {
            return Err(ToolError::Validation(format!(
                "primary_image {index} is not one of the {} images",
                self.images.len()
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
        } else if self.scalp.is_some()
            || self.hair_colour.is_some()
            || self.lighting.is_some()
            || self.texture_size.is_some()
            || self.eyes.is_some()
        {
            return Err(ToolError::Validation(
                "scalp, hair_colour, lighting, texture_size and eyes need texture_path".into(),
            ));
        }
        if self.hair_colour.is_some() && self.scalp.unwrap_or_default() != ScalpChoice::Hair {
            return Err(ToolError::Validation("hair_colour needs scalp hair".into()));
        }
        if let Some(fit) = &self.fit_path {
            if !fit.ends_with(".png") {
                return Err(ToolError::Validation("fit_path must end in .png".into()));
            }
            if *fit == self.displacement_path || self.texture_path.as_ref() == Some(fit) {
                return Err(ToolError::Validation(
                    "fit_path must differ from displacement_path and texture_path".into(),
                ));
            }
        }
        if self
            .texture_size
            .is_some_and(|size| !TEXTURE_SIZES.contains(&size))
        {
            return Err(ToolError::Validation(
                "texture_size must be 512, 1024 or 2048".into(),
            ));
        }
        if (self.iris_colour.is_some() || self.sclera_colour.is_some())
            && self.eyes.unwrap_or_default() != EyeChoice::Painted
        {
            return Err(ToolError::Validation(
                "iris_colour and sclera_colour need eyes painted".into(),
            ));
        }
        for (name, share) in [("neck_tilt", self.neck_tilt), ("neck_turn", self.neck_turn)] {
            if share.is_some_and(|share| !(0.0..=1.0).contains(&share)) {
                return Err(ToolError::Validation(format!(
                    "{name} must be between 0 and 1"
                )));
            }
        }
        Ok(())
    }
}

/// Longest side, in pixels, that depth is estimated at unless a request
/// chooses otherwise.
pub const DEPTH_DEFAULT_MAX_SIDE: u32 = 1024;
/// Longest sides a depth request may choose.
pub const DEPTH_MAX_SIDES: std::ops::RangeInclusive<u32> = 256..=2048;
/// Largest depth map committed to the workspace: a 2048-pixel 16-bit map.
const MAX_DEPTH_MAP_BYTES: u64 = 32 * 1024 * 1024;
/// Largest normal map committed to the workspace: a 2048-pixel 16-bit RGB map.
const MAX_NORMAL_MAP_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DepthRequest {
    /// Workspace photograph, JPEG, PNG or WebP. Any transparency is flattened
    /// onto black.
    pub image: String,
    /// Workspace path for the 16-bit greyscale PNG depth map, nearest
    /// brightest: a pixel value p means a relative log depth of
    /// far - (far - near) * p / 65535, with near and far reported. The scale
    /// and offset of the true distances are unknown.
    pub depth_path: String,
    /// Workspace path for a 16-bit RGB PNG of unit surface normals in the
    /// camera's frame, x right, y up and z towards the camera, each component
    /// c stored as (c + 1) / 2 * 65535. Not made when omitted; making it runs
    /// the model a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normals_path: Option<String>,
    /// Longest side, in pixels, to estimate at: 256 to 2048 (default 1024).
    /// Larger photographs are scaled down, never up, and both sides are
    /// rounded to multiples of 16. More pixels resolve finer detail and take
    /// longer and more GPU memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_side: Option<u32>,
    /// Replace existing files at `depth_path` and `normals_path`.
    #[serde(default)]
    pub overwrite: bool,
}

impl DepthRequest {
    /// Checks that need no file access.
    pub fn validate(&self) -> Result<(), ToolError> {
        if !IMAGE_SUFFIXES
            .iter()
            .any(|suffix| self.image.ends_with(suffix))
        {
            return Err(ToolError::Validation(format!(
                "{} is not a .jpg, .jpeg, .png or .webp photograph",
                self.image
            )));
        }
        if !self.depth_path.ends_with(".png") {
            return Err(ToolError::Validation("depth_path must end in .png".into()));
        }
        if let Some(normals) = &self.normals_path {
            if !normals.ends_with(".png") {
                return Err(ToolError::Validation(
                    "normals_path must end in .png".into(),
                ));
            }
            if *normals == self.depth_path {
                return Err(ToolError::Validation(
                    "normals_path and depth_path must differ".into(),
                ));
            }
        }
        if self
            .max_side
            .is_some_and(|side| !DEPTH_MAX_SIDES.contains(&side))
        {
            return Err(ToolError::Validation(format!(
                "max_side must be between {} and {}",
                DEPTH_MAX_SIDES.start(),
                DEPTH_MAX_SIDES.end()
            )));
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
    /// The depth estimation runtime and its converted Marigold V2 model.
    pub depth_script: PathBuf,
    pub depth_model: PathBuf,
    pub depth_timeout: Duration,
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
        let mut missing = self
            .check(&self.config.script, &self.config.checkpoints)
            .await?;
        if !self.config.landmarker.is_file() {
            missing.push("face landmarker model".into());
        }
        if !self.config.segmenter.is_file() {
            missing.push("person segmenter model".into());
        }
        Ok(missing)
    }

    /// Files of the converted Marigold V2 model the operator has not yet
    /// supplied.
    pub async fn missing_depth_files(&self) -> Result<Vec<String>, ToolError> {
        self.check(&self.config.depth_script, &self.config.depth_model)
            .await
    }

    /// Whether the worker can run any job, and its readiness answer:
    /// `capabilities` says which jobs can run and `missing` lists each job's
    /// absent files. Each job's check stands alone, so a check that fails
    /// withholds only its own job.
    pub async fn readiness(&self) -> (bool, Value) {
        let (reconstruct, depth) =
            tokio::join!(self.missing_checkpoints(), self.missing_depth_files());
        let present = |check: &Result<Vec<String>, ToolError>| matches!(check, Ok(missing) if missing.is_empty());
        let describe = |check: Result<Vec<String>, ToolError>| match check {
            Ok(missing) => json!(missing),
            Err(error) => json!({"error": worker_message(&error)}),
        };
        let capabilities = Readiness {
            reconstruct: present(&reconstruct),
            depth: present(&depth),
        };
        let missing = json!({"reconstruct": describe(reconstruct), "depth": describe(depth)});
        let workspace_ready = self.workspace.ready();
        if workspace_ready && (capabilities.reconstruct || capabilities.depth) {
            (
                true,
                json!({
                    "ready": true,
                    "busy": self.admission.available_permits() == 0,
                    "capabilities": capabilities,
                    "missing": missing,
                }),
            )
        } else {
            (
                false,
                json!({"ready": false, "missing": missing, "workspace_ready": workspace_ready}),
            )
        }
    }

    /// Ask a runtime script which of its files `directory` lacks.
    async fn check(&self, script: &Path, directory: &Path) -> Result<Vec<String>, ToolError> {
        let mut command = self.python_command(script);
        command.arg("--check").arg(directory);
        let output = tokio::time::timeout(CHECK_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                reconstruction_error("timeout", "the model file check did not finish")
            })??;
        #[derive(Deserialize)]
        struct Check {
            missing: Vec<String>,
        }
        let check: Check = serde_json::from_slice(&output.stdout).map_err(|_| {
            reconstruction_error("protocol", "the model file check returned no answer")
        })?;
        Ok(check.missing)
    }

    /// Refuse an output path that is unsafe or, unless `overwrite`, taken.
    fn check_destination(&self, path: &str, overwrite: bool) -> Result<(), ToolError> {
        self.workspace.validate_public_mutation_path(path)?;
        if !overwrite && self.workspace.stat_artifact(path).is_ok() {
            return Err(printable_workspace::WsError::AlreadyExists(path.to_owned()).into());
        }
        Ok(())
    }

    /// One photograph job runs at a time, so they share the GPU in turn.
    fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, ToolError> {
        self.admission.try_acquire().map_err(|_| {
            reconstruction_error("busy", "another photograph job is running; retry shortly")
        })
    }

    pub async fn reconstruct(&self, request: ReconRequest) -> Result<Value, ToolError> {
        request.validate()?;
        self.check_destination(&request.displacement_path, request.overwrite)?;
        let _permit = self.permit()?;

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
        let mut job = json!({
            "images": snapshots.iter().map(|snapshot| snapshot.path()).collect::<Vec<_>>(),
            "checkpoints": self.config.checkpoints,
            "landmarker": self.config.landmarker,
            "segmenter": self.config.segmenter,
            "displacement": map,
            "device": self.config.device,
            "seed": 0,
        });
        if let Some(primary) = request.primary_image {
            job["primary"] = json!(primary);
        }
        let mut result = self
            .run(
                &self.config.script,
                &job,
                staging.path(),
                self.config.timeout,
                "reconstruction",
            )
            .await?;
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

    /// Estimate relative depth, and optionally surface normals, for one
    /// photograph with Marigold V2.
    pub async fn estimate_depth(&self, request: DepthRequest) -> Result<Value, ToolError> {
        request.validate()?;
        self.check_destination(&request.depth_path, request.overwrite)?;
        if let Some(normals) = &request.normals_path {
            self.check_destination(normals, request.overwrite)?;
        }
        let _permit = self.permit()?;

        let snapshot = self
            .workspace
            .snapshot_artifact_bounded(&request.image, MAX_IMAGE_BYTES)?;
        let staging = tempfile::Builder::new()
            .prefix("printable-depth-")
            .tempdir()?;
        let depth = staging.path().join("depth.png");
        let normals = staging.path().join("normals.png");
        let mut job = json!({
            "image": snapshot.path(),
            "model": self.config.depth_model,
            "depth": depth,
            "max_side": request.max_side.unwrap_or(DEPTH_DEFAULT_MAX_SIDE),
            "device": self.config.device,
            "seed": 0,
        });
        if request.normals_path.is_some() {
            job["normals"] = json!(normals);
        }
        let mut result = self
            .run(
                &self.config.depth_script,
                &job,
                staging.path(),
                self.config.depth_timeout,
                "depth estimation",
            )
            .await?;
        let artifact = self.workspace.commit_generated_artifact_bounded(
            &request.depth_path,
            &depth,
            request.overwrite,
            MAX_DEPTH_MAP_BYTES,
        )?;
        result["depth"]["artifact"] = json!(artifact);
        if let Some(path) = &request.normals_path {
            let artifact = self.workspace.commit_generated_artifact_bounded(
                path,
                &normals,
                request.overwrite,
                MAX_NORMAL_MAP_BYTES,
            )?;
            result["normals"]["artifact"] = json!(artifact);
        }
        result["image_sha256"] = json!(file_digest(snapshot.path())?);
        Ok(result)
    }

    /// Run one job in a fresh Python process with a deadline and return the
    /// result object it prints. The process and any helpers it starts are
    /// killed if the deadline passes, and its memory returns when it exits.
    async fn run(
        &self,
        script: &Path,
        job: &Value,
        staging: &Path,
        timeout: Duration,
        activity: &str,
    ) -> Result<Value, ToolError> {
        let request_path = staging.join("request.json");
        std::fs::write(&request_path, serde_json::to_vec(job)?)?;

        let mut command = self.python_command(script);
        command
            .arg(&request_path)
            .current_dir(staging)
            .env("HOME", staging);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().map_err(|_| {
            reconstruction_error(
                "unavailable",
                format!("the {activity} runtime could not start"),
            )
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
        let (status, response) = match tokio::time::timeout(timeout, run).await {
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
                        "{activity} did not finish within {} seconds",
                        timeout.as_secs()
                    ),
                ));
            }
        };
        let envelope: Value = serde_json::from_slice(&response).map_err(|_| {
            if status.success() {
                reconstruction_error(
                    "protocol",
                    format!("the {activity} runtime returned no answer"),
                )
            } else {
                reconstruction_error(
                    "resource_limit",
                    format!("{activity} ran out of memory or stopped unexpectedly"),
                )
            }
        })?;
        if let Some(error) = envelope.get("error") {
            let code = match error.get("code").and_then(Value::as_str) {
                Some("no_face") => "no_face",
                Some("checkpoints") => "checkpoints",
                Some("validation") => "validation",
                Some("unavailable") => "unavailable",
                Some("resource_limit") => "resource_limit",
                _ => "failed",
            };
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{activity} failed"), str::to_owned);
            return Err(reconstruction_error(code, message));
        }
        envelope
            .get("result")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| {
                reconstruction_error("protocol", format!("the {activity} answer has no result"))
            })
    }

    fn python_command(&self, script: &Path) -> Command {
        let mut command = Command::new(&self.config.python);
        command
            .arg("-I")
            .arg("-B")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("OMP_NUM_THREADS", "4")
            // The runtimes read only mounted files; nothing is fetched.
            .env("HF_HUB_OFFLINE", "1")
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
    "images": len(request["images"]), "device": request["device"], "primary": request.get("primary", "absent"),
    "displacement": {"offset_mm": -1.0, "scale_mm": 2.0, "width": 256, "height": 256}}}))
"#;

    /// A stand-in depth runtime: it answers the file check by whether the
    /// model directory exists and otherwise echoes the request, choosing its
    /// behaviour from the photograph's name.
    const FAKE_DEPTH_RUNTIME: &str = r#"
import json, os, sys, time
if len(sys.argv) == 3 and sys.argv[1] == "--check":
    print(json.dumps({"missing": [] if os.path.isdir(sys.argv[2]) else ["manifest.json"]}))
    sys.exit(0)
request = json.load(open(sys.argv[1]))
name = request["image"]
if "slow" in name:
    time.sleep(30)
if "oom" in name:
    print(json.dumps({"error": {"code": "resource_limit", "message": "the GPU ran out of memory"}}))
    sys.exit(2)
open(request["depth"], "wb").write(b"\x89PNG\r\n\x1a\ndepth")
result = {"width": 64, "height": 48, "depth": {"near": -1.0, "far": 1.0}, "device": request["device"],
    "max_side": request["max_side"], "model": request["model"]}
if "normals" in request:
    open(request["normals"], "wb").write(b"\x89PNG\r\n\x1a\nnormals")
    result["normals"] = {"encoding": "c = 2 * p / 65535 - 1", "frame": "x right, y up, z towards the camera"}
print(json.dumps({"result": result}))
"#;

    fn worker(timeout: Duration) -> (tempfile::TempDir, ReconWorker) {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("reconstruct.py"), FAKE_RUNTIME).unwrap();
        std::fs::write(runtime.join("estimate_depth.py"), FAKE_DEPTH_RUNTIME).unwrap();
        std::fs::create_dir(runtime.join("marigold")).unwrap();
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
            "oom.jpg",
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
                depth_script: runtime.join("estimate_depth.py"),
                depth_model: runtime.join("marigold"),
                depth_timeout: timeout,
            },
        );
        (root, worker)
    }

    fn depth_request(image: &str, normals: Option<&str>) -> DepthRequest {
        DepthRequest {
            image: image.into(),
            depth_path: "relief/depth.png".into(),
            normals_path: normals.map(Into::into),
            max_side: None,
            overwrite: false,
        }
    }

    #[tokio::test]
    async fn depth_maps_are_committed_with_the_photographs_digest() {
        let (root, worker) = worker(Duration::from_secs(60));
        assert!(worker.missing_depth_files().await.unwrap().is_empty());
        let result = worker
            .estimate_depth(depth_request("face.jpg", Some("relief/normals.png")))
            .await
            .unwrap();
        assert_eq!(result["depth"]["artifact"]["path"], "relief/depth.png");
        assert_eq!(result["normals"]["artifact"]["path"], "relief/normals.png");
        assert_eq!(result["max_side"], DEPTH_DEFAULT_MAX_SIDE);
        assert_eq!(result["device"], "cpu");
        assert!(root.path().join("workspace/relief/depth.png").is_file());
        assert!(root.path().join("workspace/relief/normals.png").is_file());
        let digest = file_digest(&root.path().join("workspace/face.jpg")).unwrap();
        assert_eq!(result["image_sha256"], digest);
        // Without normals_path the runtime is not asked for them.
        let mut plain = depth_request("side.png", None);
        plain.depth_path = "relief/plain.png".into();
        let result = worker.estimate_depth(plain).await.unwrap();
        assert!(result.get("normals").is_none());
        // Existing maps are kept unless overwrite is requested.
        for normals in [None, Some("relief/normals.png")] {
            let mut repeat = depth_request("face.jpg", normals);
            if normals.is_some() {
                repeat.depth_path = "relief/other.png".into();
            }
            let error = worker.estimate_depth(repeat).await.unwrap_err();
            assert_eq!(error.code(), "already_exists");
        }
        let mut overwrite = depth_request("face.jpg", Some("relief/normals.png"));
        overwrite.overwrite = true;
        overwrite.max_side = Some(2048);
        let result = worker.estimate_depth(overwrite).await.unwrap();
        assert_eq!(result["max_side"], 2048);
    }

    #[tokio::test]
    async fn depth_failures_keep_their_cause() {
        let (_root, worker) = worker(Duration::from_secs(60));
        let error = worker
            .estimate_depth(depth_request("oom.jpg", None))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "resource_limit");
        assert_eq!(worker_message(&error), "the GPU ran out of memory");
        drop(worker);
        let (_short_root, short) = super::tests::worker(Duration::from_secs(1));
        let error = short
            .estimate_depth(depth_request("slow.jpg", None))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "timeout");
        assert!(worker_message(&error).starts_with("depth estimation did not finish"));
        std::fs::remove_dir(&short.config.depth_model).unwrap();
        assert_eq!(
            short.missing_depth_files().await.unwrap(),
            ["manifest.json"]
        );
    }

    #[tokio::test]
    async fn photograph_jobs_take_turns() {
        let (_root, worker) = worker(Duration::from_secs(60));
        let worker = Arc::new(worker);
        let slow = {
            let worker = Arc::clone(&worker);
            tokio::spawn(
                async move { worker.estimate_depth(depth_request("slow.jpg", None)).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        let busy = worker
            .reconstruct(request(&["face.jpg"], "detail/map.png"))
            .await
            .unwrap_err();
        assert_eq!(busy.code(), "busy");
        slow.abort();
    }

    #[tokio::test]
    async fn each_job_is_ready_on_its_own_files() {
        let (root, worker) = worker(Duration::from_secs(60));
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        assert_eq!(
            answer["capabilities"],
            json!({"reconstruct": true, "depth": true})
        );
        // A depth check that cannot run withholds depth alone.
        let mut broken = worker.config.clone();
        broken.depth_script = root.path().join("absent.py");
        let worker = ReconWorker::new(Arc::clone(&worker.workspace), broken);
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        assert_eq!(
            answer["capabilities"],
            json!({"reconstruct": true, "depth": false})
        );
        assert!(answer["missing"]["depth"]["error"].is_string());
        // Without either job's files the worker is not ready and says why.
        let mut bare = worker.config.clone();
        bare.depth_script = root.path().join("runtime/estimate_depth.py");
        bare.landmarker = root.path().join("absent.task");
        bare.depth_model = root.path().join("absent");
        let worker = ReconWorker::new(Arc::clone(&worker.workspace), bare);
        let (ready, answer) = worker.readiness().await;
        assert!(!ready);
        assert_eq!(answer["missing"]["depth"], json!(["manifest.json"]));
        assert_eq!(
            answer["missing"]["reconstruct"],
            json!(["face landmarker model"])
        );
    }

    #[test]
    fn depth_requests_are_checked_before_any_work() {
        assert!(depth_request("a.jpg", Some("n.png")).validate().is_ok());
        for (request, message) in [
            (depth_request("a.gif", None), "not a .jpg"),
            (
                DepthRequest {
                    depth_path: "d.tif".into(),
                    ..depth_request("a.jpg", None)
                },
                "depth_path must end in .png",
            ),
            (
                depth_request("a.jpg", Some("n.jpg")),
                "normals_path must end in .png",
            ),
            (
                depth_request("a.jpg", Some("relief/depth.png")),
                "must differ",
            ),
            (
                DepthRequest {
                    max_side: Some(4096),
                    ..depth_request("a.jpg", None)
                },
                "between 256 and 2048",
            ),
        ] {
            let error = request.validate().unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
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
    async fn the_primary_photograph_is_passed_to_the_runtime() {
        let (_root, worker) = worker(Duration::from_secs(60));
        for (primary, expected) in [
            (None, json!("absent")),
            (Some(PrimaryImage::Index(1)), json!(1)),
            (
                Some(PrimaryImage::Rule(PrimaryRule::MostFrontal)),
                json!("most_frontal"),
            ),
        ] {
            let mut asked = request(&["face.jpg", "side.png"], "map.png");
            asked.primary_image = primary;
            asked.overwrite = true;
            let result = worker.reconstruct(asked).await.unwrap();
            assert_eq!(result["primary"], expected);
        }
        let mut outside = request(&["face.jpg", "side.png"], "map.png");
        outside.primary_image = Some(PrimaryImage::Index(2));
        let error = worker.reconstruct(outside).await.unwrap_err();
        assert!(
            error.to_string().contains("not one of the 2 images"),
            "{error}"
        );
        // The wire form is a number or the rule's name.
        let parsed: ReconRequest = serde_json::from_value(json!({
            "images": ["a.jpg"], "displacement_path": "d.png", "primary_image": "most_frontal"
        }))
        .unwrap();
        assert_eq!(
            parsed.primary_image,
            Some(PrimaryImage::Rule(PrimaryRule::MostFrontal))
        );
        assert!(
            serde_json::from_value::<ReconRequest>(json!({
                "images": ["a.jpg"], "displacement_path": "d.png", "primary_image": "first"
            }))
            .is_err()
        );
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
            primary_image: None,
            displacement_path: displacement.into(),
            overwrite: false,
            texture_path: None,
            scalp: None,
            hair_colour: None,
            lighting: None,
            texture_size: None,
            eyes: None,
            iris_colour: None,
            sclera_colour: None,
            fit_path: None,
            neck_tilt: None,
            neck_turn: None,
        }
    }

    #[test]
    fn options_the_server_applies_stay_with_the_server() {
        let request = ReconRequest {
            neck_tilt: Some(0.5),
            neck_turn: Some(1.0),
            texture_path: Some("colour.png".into()),
            texture_size: Some(2048),
            eyes: Some(EyeChoice::Painted),
            iris_colour: Some([1, 2, 3]),
            sclera_colour: Some([4, 5, 6]),
            fit_path: Some("fit.png".into()),
            ..request(&["a.jpg"], "map.png")
        };
        let sent = serde_json::to_value(request.for_worker()).unwrap();
        for name in [
            "fit_path",
            "neck_tilt",
            "neck_turn",
            "texture_size",
            "eyes",
            "iris_colour",
            "sclera_colour",
        ] {
            assert!(sent.get(name).is_none(), "{name}: {sent}");
        }
        assert_eq!(sent["texture_path"], "colour.png");
    }

    #[test]
    fn colour_map_options_are_checked() {
        let textured = ReconRequest {
            texture_path: Some("colour.png".into()),
            ..request(&["a.jpg"], "map.png")
        };
        for size in TEXTURE_SIZES {
            let sized = ReconRequest {
                texture_size: Some(size),
                ..textured.clone()
            };
            assert!(sized.validate().is_ok());
        }
        let painted = ReconRequest {
            eyes: Some(EyeChoice::Painted),
            iris_colour: Some([1, 2, 3]),
            sclera_colour: Some([4, 5, 6]),
            ..textured.clone()
        };
        assert!(painted.validate().is_ok());
        for (refused, message) in [
            (
                ReconRequest {
                    texture_size: Some(4096),
                    ..textured.clone()
                },
                "512, 1024 or 2048",
            ),
            (
                ReconRequest {
                    texture_path: None,
                    texture_size: Some(1024),
                    ..textured.clone()
                },
                "need texture_path",
            ),
            (
                ReconRequest {
                    texture_path: None,
                    eyes: Some(EyeChoice::Photo),
                    ..textured.clone()
                },
                "need texture_path",
            ),
            (
                ReconRequest {
                    iris_colour: Some([1, 2, 3]),
                    ..textured.clone()
                },
                "eyes painted",
            ),
            (
                ReconRequest {
                    eyes: Some(EyeChoice::Photo),
                    sclera_colour: Some([1, 2, 3]),
                    ..textured.clone()
                },
                "eyes painted",
            ),
        ] {
            let error = refused.validate().unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
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

/// Which of the photograph worker's jobs it last reported it can run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Readiness {
    /// Every reconstruction checkpoint is present.
    pub reconstruct: bool,
    /// The converted Marigold V2 model is present.
    pub depth: bool,
}

/// The server's view of the reconstruction worker.
#[derive(Debug)]
pub struct ReconClient {
    endpoint: String,
    http: reqwest::Client,
    /// The worker's last readiness answer; subscribers hear each change.
    ready: tokio::sync::watch::Sender<Readiness>,
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
            ready: tokio::sync::watch::Sender::new(Readiness::default()),
            colour_maps: Arc::new(Semaphore::new(1)),
        })))
    }

    /// Whether the worker last reported every reconstruction checkpoint
    /// present.
    pub fn can_reconstruct(&self) -> bool {
        self.ready.borrow().reconstruct
    }

    /// Whether the worker last reported the depth model present.
    pub fn can_estimate_depth(&self) -> bool {
        self.ready.borrow().depth
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
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Readiness> {
        self.ready.subscribe()
    }

    /// Ask the worker now and remember the answer.
    pub async fn refresh(&self) -> Readiness {
        #[derive(Deserialize)]
        struct Answer {
            capabilities: Readiness,
        }
        let ready = match self
            .http
            .get(format!("{}/readyz", self.endpoint))
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response
                .json::<Answer>()
                .await
                .map(|answer| answer.capabilities)
                .unwrap_or_default(),
            _ => Readiness::default(),
        };
        let changed = self.ready.send_if_modified(|current| {
            let changed = *current != ready;
            *current = ready;
            changed
        });
        if changed {
            tracing::info!(
                reconstruct = ready.reconstruct,
                depth = ready.depth,
                "photograph worker availability changed"
            );
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
        self.post(
            "reconstruct",
            &request.for_worker(),
            "check displacement_path before retrying, because the map may have been written",
        )
        .await
    }

    pub async fn estimate_depth(&self, request: &DepthRequest) -> Result<Value, ToolError> {
        request.validate()?;
        self.post(
            "depth",
            request,
            "check depth_path before retrying, because the map may have been written",
        )
        .await
    }

    /// Send one job to the worker. A request the worker may have received
    /// but whose answer was lost has an unknown outcome; `advice` tells the
    /// caller what to inspect before retrying.
    async fn post(
        &self,
        route: &str,
        body: &impl Serialize,
        advice: &str,
    ) -> Result<Value, ToolError> {
        let response = self
            .http
            .post(format!("{}/{route}", self.endpoint))
            .timeout(REQUEST_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|_| {
                reconstruction_error(
                    "outcome_unknown",
                    format!(
                        "the photograph worker could not be reached or the connection was lost; {advice}"
                    ),
                )
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|_| {
            reconstruction_error(
                "outcome_unknown",
                format!("the photograph worker's answer was interrupted; {advice}"),
            )
        })?;
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            reconstruction_error("protocol", "the photograph worker returned no answer")
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
            Some("unavailable") => "unavailable",
            _ => "failed",
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the photograph worker refused the request");
        Err(reconstruction_error(code, message))
    }
}
