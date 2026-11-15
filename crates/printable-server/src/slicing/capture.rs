//! Native Orca layout observations over immutable project or prepared inputs.

use super::{
    ProcessGroup, SliceHandle, SliceWorker, hash_file, invalid, read_log, slice_directory,
    slice_error,
};
use crate::{error::ToolError, projects, provenance, upload::random_hex_id};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureSource {
    /// An existing project-relative 3MF, captured without arranging or slicing.
    Project { path: String },
    /// The hashed output package of a completed preparation, including its final layout.
    Prepared { output_dir: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    #[default]
    Isometric,
    Top,
    Front,
    Rear,
    Left,
    Right,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    #[default]
    Orthographic,
    Perspective,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Camera {
    pub preset: Preset,
    pub projection: Projection,
    /// Optional native camera quaternion in w/x/y/z order, overriding the preset.
    pub orientation: Option<[f64; 4]>,
    /// Optional target in Orca workspace millimeters, overriding automatic centering.
    pub target: Option<[f64; 3]>,
    /// Optional native zoom, overriding automatic framing. Close-ups may crop.
    pub zoom: Option<f64>,
    /// Automatic framing margin, at least one. Defaults to 1.1.
    pub margin: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            preset: Preset::Isometric,
            projection: Projection::Orthographic,
            orientation: None,
            target: None,
            zoom: None,
            margin: 1.1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Visibility {
    pub beds: bool,
    pub plate_identifiers: bool,
    pub prime_towers: bool,
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            beds: true,
            plate_identifiers: true,
            prime_towers: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureParams {
    pub project_id: String,
    pub source: CaptureSource,
    /// One-based source plate indices; empty selects all, preserving native positions.
    #[serde(default)]
    pub plates: Vec<u32>,
    /// One-based source model object indices; empty selects all on the selected plates.
    #[serde(default)]
    pub objects: Vec<u32>,
    #[serde(default)]
    pub camera: Camera,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    /// Positive processing budget, including native startup. Defaults to five minutes.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_width() -> u32 {
    1600
}
fn default_height() -> u32 {
    1000
}
fn default_timeout() -> u64 {
    300
}

impl CaptureParams {
    fn validate(&self) -> Result<Duration, ToolError> {
        let budget = Duration::from_secs(self.timeout_seconds);
        if self.timeout_seconds == 0
            || Instant::now().checked_add(budget).is_none()
            || self.width == 0
            || self.height == 0
            || i32::try_from(self.width).is_err()
            || i32::try_from(self.height).is_err()
        {
            return Err(invalid(
                "capture requires positive runtime-representable dimensions and processing budget",
            ));
        }
        for values in [&self.plates, &self.objects] {
            let unique: BTreeSet<_> = values.iter().collect();
            if unique.len() != values.len()
                || values
                    .iter()
                    .any(|&value| value == 0 || i32::try_from(value).is_err())
            {
                return Err(invalid(
                    "capture selections require distinct positive one-based indices",
                ));
            }
        }
        if !self.camera.margin.is_finite()
            || self.camera.margin < 1.
            || self
                .camera
                .zoom
                .is_some_and(|zoom| !zoom.is_finite() || zoom <= 0.)
            || self
                .camera
                .target
                .is_some_and(|target| target.iter().any(|n| !n.is_finite()))
        {
            return Err(invalid(
                "capture camera requires finite coordinates, positive zoom and a margin of at least one",
            ));
        }
        if let Some(orientation) = self.camera.orientation {
            let norm: f64 = orientation.iter().map(|n| n * n).sum();
            if !norm.is_finite() || norm == 0. {
                return Err(invalid(
                    "capture orientation requires a finite nonzero w/x/y/z quaternion",
                ));
            }
        }
        Ok(budget)
    }

    fn native_request(&self) -> Value {
        json!({"format_version":1,"plates":self.plates,"objects":self.objects,
            "camera":self.camera,"visibility":self.visibility,"width":self.width,"height":self.height})
    }
}

impl SliceWorker {
    pub(super) async fn capture(&self, params: CaptureParams) -> Result<Value, ToolError> {
        let budget = params.validate()?;
        tokio::time::timeout(budget, self.capture_inner(params))
            .await
            .map_err(|_| slice_error("native Orca capture exceeded its processing deadline"))?
    }

    async fn capture_inner(&self, params: CaptureParams) -> Result<Value, ToolError> {
        let (source, expected_hash, slice_provenance) = match &params.source {
            CaptureSource::Project { path } => {
                if !path.ends_with(".3mf") {
                    return Err(invalid("native layout capture requires a project 3MF"));
                }
                (
                    projects::resolve(&self.workspace, &params.project_id, path)?,
                    None,
                    None,
                )
            }
            CaptureSource::Prepared { output_dir } => {
                let handle = SliceHandle {
                    project_id: params.project_id.clone(),
                    output_dir: output_dir.clone(),
                };
                let state = self.status(handle.clone(), false).await?;
                if state["status"] != "completed" {
                    return Err(invalid(
                        "prepared layout capture requires a completed slice",
                    ));
                }
                let selected = &state["artifacts"]["model.gcode.3mf"];
                let path = selected["artifact"]["path"]
                    .as_str()
                    .ok_or_else(|| invalid("completed slice has no prepared package"))?
                    .to_owned();
                let expected = format!(
                    "{}/model.gcode.3mf",
                    slice_directory(&self.workspace, &params.project_id, output_dir)?
                );
                if path != expected {
                    return Err(invalid(
                        "prepared package must belong to the selected slice",
                    ));
                }
                let hash = selected["sha256"]
                    .as_str()
                    .ok_or_else(|| invalid("prepared package has no recorded digest"))?
                    .to_owned();
                let reference = state
                    .get("provenance")
                    .filter(|value| !value.is_null())
                    .cloned();
                if let Some(reference) = &reference {
                    let reference: provenance::RecordRef =
                        serde_json::from_value(reference.clone())?;
                    let retained = provenance::read(
                        &self.workspace,
                        &reference,
                        provenance::Kind::Slice,
                        &params.project_id,
                    )?;
                    if retained.data["slice"] != json!(handle)
                        || retained.data["artifacts"]["model.gcode.3mf"]["sha256"] != hash
                        || retained.data["artifacts"]["model.gcode.3mf"]["artifact"]["path"] != path
                    {
                        return Err(invalid(
                            "slice state does not match its retained package provenance",
                        ));
                    }
                }
                (path, Some(hash), reference)
            }
        };
        if self.probe_engine().await.native_capture != Some(true) {
            return Err(ToolError::NativeCaptureUnavailable(
                "the installed engine does not provide native workspace capture".into(),
            ));
        }
        let admission = Arc::new(
            self.admit("slicer is busy; retry capture after active work completes")
                .await?,
        );
        let staging = Arc::new(self.workspace.scratch(
            self.workspace.stat_artifact(&source)?.size_bytes,
            "orca-capture",
        )?);
        let workspace = Arc::clone(&self.workspace);
        let copy_from = source.clone();
        let owners = (Arc::clone(&admission), Arc::clone(&staging));
        let source_hash = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let (_admission, staging) = owners;
            let snapshot = workspace.snapshot_artifact(&copy_from)?;
            let hash = hash_file(snapshot.path())?;
            if expected_hash.is_some_and(|expected| expected != hash) {
                return Err(slice_error(
                    "prepared package changed since slicing; prepare a new revision",
                ));
            }
            std::fs::copy(snapshot.path(), staging.path().join("source.3mf"))?;
            Ok(hash)
        })
        .await
        .map_err(|_| slice_error("capture input snapshot task failed"))??;
        let native_request = params.native_request();
        let request_path = staging.path().join("view-request.json");
        std::fs::write(&request_path, serde_json::to_vec(&native_request)?)?;
        let mut command = Command::new(&self.binary);
        command
            .arg("--capture-workspace")
            .arg(&request_path)
            .arg(staging.path().join("source.3mf"))
            .current_dir(staging.path())
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", staging.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        staging.retain_for_command(command.as_std_mut())?;
        let mut child = command.spawn()?;
        let group = ProcessGroup(child.id().expect("spawned capture process"));
        let stdout = child.stdout.take().expect("piped capture stdout");
        let stderr = child.stderr.take().expect("piped capture stderr");
        let out_task = tokio::spawn(read_log(stdout));
        let err_task = tokio::spawn(read_log(stderr));
        let status = child.wait().await?;
        drop(group);
        let stdout = out_task
            .await
            .map_err(|_| slice_error("capture stdout collection failed"))??;
        let stderr = err_task
            .await
            .map_err(|_| slice_error("capture stderr collection failed"))??;
        if !status.success() {
            let diagnostics_path = projects::resolve(
                &self.workspace,
                &params.project_id,
                &format!("observations/orca-failed-{}.json", random_hex_id()?),
            )?;
            self.workspace.write_generated_bytes(
                &diagnostics_path,
                &serde_json::to_vec(&json!({"stdout":stdout,"stderr":stderr}))?,
                false,
            )?;
            if status.code() == Some(3) {
                return Err(ToolError::NativeCaptureUnavailable(format!(
                    "the graphics runtime cannot capture native layouts; inspect {diagnostics_path}"
                )));
            }
            return Err(slice_error(&format!(
                "native Orca capture failed ({status}); inspect {diagnostics_path}"
            )));
        }
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || -> Result<Value, ToolError> {
            let _admission = admission;
            if hash_file(&staging.path().join("source.3mf"))? != source_hash {
                return Err(slice_error("native capture changed its input snapshot"));
            }
            let bytes = read_regular(&staging.path().join("capture.png"))?;
            crate::tools::validate_png(&bytes, u64::from(params.width), u64::from(params.height))?;
            let native: Value = serde_json::from_slice(&read_regular(&staging.path().join("capture.json"))?)?;
            validate_native(&native, &params)?;
            let capture_id = random_hex_id()?;
            let image_path = projects::resolve(&workspace, &params.project_id,
                &format!("observations/orca-{capture_id}.png"))?;
            let stem = image_path.strip_suffix(".png").expect("generated capture PNG suffix");
            let image = workspace.write_generated_bytes(&image_path, &bytes, false)?;
            let image_sha256 = provenance::digest(&bytes);
            let native_metadata_path = format!("{stem}-native.json");
            let native_bytes = serde_json::to_vec(&native)?;
            workspace.write_generated_bytes(&native_metadata_path, &native_bytes, false)?;
            let retained_path = format!(".printable/evidence/observation_media/orca-{capture_id}.png");
            let retained_image = workspace.write_reserved_artifact(&retained_path, &bytes, false)?;
            let evidence = json!({"kind":"native_orca_layout","representation":native["representation"],
                "prime_towers":native["prime_towers"],"source":params.source,"source_path":source,"source_sha256":source_hash,
                "engine":native["engine"],"plates":native["plates"],"objects":native["objects"],
                "camera":native["camera"],"visibility":native["visibility"],"width":params.width,"height":params.height,
                "graphics_renderer":native["graphics_renderer"],"slice_provenance":slice_provenance,
                "image":{"artifact":retained_image,"sha256":image_sha256},
                "native_metadata":{"path":native_metadata_path,"sha256":provenance::digest(&native_bytes)}});
            let reference = provenance::store(&workspace, provenance::Kind::Observation, &params.project_id, evidence.clone())?;
            let mut response = evidence;
            response["image"] = json!(image);
            response["image_sha256"] = json!(image_sha256);
            response["provenance"] = json!(reference);
            let metadata_path = format!("{stem}.json");
            workspace.write_generated_bytes(&metadata_path, &serde_json::to_vec(&response)?, false)?;
            response["metadata_path"] = json!(metadata_path);
            Ok(response)
        }).await.map_err(|_| slice_error("native capture artifact task failed"))?
    }
}

fn read_regular(path: &Path) -> Result<Vec<u8>, ToolError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(slice_error("native capture output must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn validate_native(native: &Value, params: &CaptureParams) -> Result<(), ToolError> {
    if native["format_version"] != 1
        || native["kind"] != "native_orca_layout"
        || native["representation"] != "model_placement"
        || native["prime_towers"] != "native_estimated"
        || native["engine"]["name"] != "OrcaSlicer"
        || native["engine"]["version"] != super::ENGINE_VERSION
        || native["engine"]["source_revision"]
            .as_str()
            .is_none_or(str::is_empty)
        || native["width"] != params.width
        || native["height"] != params.height
        || native["visibility"] != json!(params.visibility)
        || native["objects"] != json!(params.objects)
        || native["camera"]["projection"] != json!(params.camera.projection)
        || native["camera"]["preset"] != json!(params.camera.preset)
        || native["camera"]["margin"] != params.camera.margin
    {
        return Err(slice_error(
            "native capture metadata does not match the requested layout observation",
        ));
    }
    let plates: Vec<u32> = serde_json::from_value(native["plates"].clone())?;
    if plates.is_empty()
        || plates.contains(&0)
        || plates.iter().collect::<BTreeSet<_>>().len() != plates.len()
        || (!params.plates.is_empty()
            && plates.iter().collect::<BTreeSet<_>>()
                != params.plates.iter().collect::<BTreeSet<_>>())
    {
        return Err(slice_error(
            "native capture returned an invalid plate selection",
        ));
    }
    let camera: Camera = serde_json::from_value(native["camera"].clone())?;
    if camera.orientation.is_none() || camera.target.is_none() || camera.zoom.is_none() {
        return Err(slice_error(
            "native capture omitted its resolved camera settings",
        ));
    }
    let mut resolved = params.clone();
    resolved.camera = camera;
    resolved.validate()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;

    fn request() -> CaptureParams {
        serde_json::from_value(
            json!({"project_id":"part","source":{"kind":"project","path":"layout.3mf"},
            "width":16,"height":12}),
        )
        .unwrap()
    }

    fn fixture(root: &Path, behavior: &str) -> Arc<SliceWorker> {
        let worker = super::super::tests::fixture(root, false);
        worker
            .workspace
            .write_artifact(
                "projects/part/layout.3mf",
                b"original project layout",
                false,
            )
            .unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .encode(super::super::package::fixture::png(16, 12));
        let script = r#"#!/usr/bin/python3
import base64,json,pathlib,sys,time
if '--help' in sys.argv:
 print('OrcaSlicer-2.4.2: --slice --export-3mf --capture-workspace')
 sys.exit(0)
root=pathlib.Path(__ROOT__)
behavior=__BEHAVIOR__
root.joinpath('capture-argv.json').write_text(json.dumps(sys.argv))
request=json.loads(pathlib.Path(sys.argv[2]).read_text())
source=pathlib.Path(sys.argv[3])
root.joinpath('captured-input').write_bytes(source.read_bytes())
if behavior == 'slow':
 root.joinpath('capture-started').write_text(str(__import__('os').getpid()))
 time.sleep(60)
if behavior == 'graphics-unavailable': sys.exit(3)
if behavior == 'tamper': source.write_bytes(b'changed source')
camera=request['camera']
camera['orientation']=camera.get('orientation') or [1,0,0,0]
camera['target']=camera.get('target') or [50,50,0]
camera['zoom']=camera.get('zoom') or 2.5
metadata={'format_version':1,'kind':'native_orca_layout','representation':'model_placement',
 'prime_towers':'native_estimated','engine':{'name':'OrcaSlicer','version':'2.4.2','source_revision':'fixture'},
 'width':request['width'],'height':request['height'],'plates':request['plates'] or [1,2,3],
 'objects':request['objects'],'camera':camera,'visibility':request['visibility'],'volumes':[]}
if behavior == 'wrong-plate': metadata['plates']=[2]
if behavior == 'wrong-camera': metadata['camera']['projection']='perspective'
pathlib.Path('capture.json').write_text(json.dumps(metadata))
pathlib.Path('capture.png').write_bytes(b'incomplete PNG' if behavior == 'bad-png' else base64.b64decode(__PNG__))
"#.replace("__ROOT__", &serde_json::to_string(root.to_str().unwrap()).unwrap())
            .replace("__BEHAVIOR__", &serde_json::to_string(behavior).unwrap())
            .replace("__PNG__", &serde_json::to_string(&png).unwrap());
        std::fs::write(&worker.binary, script).unwrap();
        std::fs::set_permissions(&worker.binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        worker
    }

    #[test]
    fn invalid_controls_are_rejected_before_native_work() {
        for patch in [
            json!({"width":0}),
            json!({"timeout_seconds":0}),
            json!({"plates":[1,1]}),
            json!({"objects":[0]}),
            json!({"camera":{"orientation":[0,0,0,0]}}),
            json!({"camera":{"zoom":0}}),
            json!({"camera":{"margin":0.5}}),
        ] {
            let mut value = serde_json::to_value(request()).unwrap();
            for (key, value_patch) in patch.as_object().unwrap() {
                value[key] = value_patch.clone();
            }
            assert!(
                serde_json::from_value::<CaptureParams>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut params = request();
        params.camera.target = Some([0., f64::NAN, 0.]);
        assert!(params.validate().is_err());
        params.camera.target = None;
        params.camera.orientation = Some([f64::INFINITY, 0., 0., 0.]);
        assert!(params.validate().is_err());
        let unknown = json!({"project_id":"part","source":{"kind":"project","path":"layout.3mf"},"shell":"command"});
        assert!(serde_json::from_value::<CaptureParams>(unknown).is_err());
    }

    #[tokio::test]
    async fn capture_retains_exact_source_image_and_reusable_view_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), "normal");
        let mut params = request();
        params.plates = vec![1, 3];
        params.camera.orientation = Some([0.9238795, 0.3826834, 0., 0.]);
        let first = worker.capture(params.clone()).await.unwrap();
        assert_eq!(first["plates"], json!([1, 3]));
        assert_eq!(
            first["source_sha256"],
            provenance::digest(b"original project layout")
        );
        assert_eq!(
            std::fs::read(root.path().join("projects/part/layout.3mf")).unwrap(),
            b"original project layout"
        );
        let argv: Vec<String> =
            serde_json::from_slice(&std::fs::read(root.path().join("capture-argv.json")).unwrap())
                .unwrap();
        assert_eq!(argv.len(), 4);
        assert_eq!(argv[1], "--capture-workspace");
        assert!(argv[3].ends_with("source.3mf"));
        let (image, bytes) = worker
            .workspace
            .read_artifact(first["image"]["path"].as_str().unwrap())
            .unwrap();
        assert_eq!(first["image_sha256"], provenance::digest(&bytes));
        assert!(image.size_bytes > 0);
        let reference: provenance::RecordRef =
            serde_json::from_value(first["provenance"].clone()).unwrap();
        let retained = provenance::read(
            &worker.workspace,
            &reference,
            provenance::Kind::Observation,
            "part",
        )
        .unwrap();
        assert_eq!(retained.data["image"]["sha256"], first["image_sha256"]);
        let (_, retained_bytes) = worker
            .workspace
            .read_artifact(retained.data["image"]["artifact"]["path"].as_str().unwrap())
            .unwrap();
        assert_eq!(retained_bytes, bytes);
        params.camera = serde_json::from_value(first["camera"].clone()).unwrap();
        let replay = worker.capture(params).await.unwrap();
        assert_eq!(
            serde_json::from_value::<Camera>(replay["camera"].clone()).unwrap(),
            serde_json::from_value::<Camera>(first["camera"].clone()).unwrap()
        );
        assert_ne!(replay["image"]["path"], first["image"]["path"]);
    }

    #[tokio::test]
    async fn capture_rejects_missing_native_support_and_invalid_native_evidence() {
        let root = tempfile::tempdir().unwrap();
        let worker = super::super::tests::fixture(root.path(), false);
        let error = worker.capture(request()).await.unwrap_err();
        assert_eq!(error.code(), "native_capture_unavailable");
        let graphics_root = tempfile::tempdir().unwrap();
        let graphics_worker = fixture(graphics_root.path(), "graphics-unavailable");
        assert_eq!(
            graphics_worker.capture(request()).await.unwrap_err().code(),
            "native_capture_unavailable"
        );
        for behavior in ["tamper", "bad-png", "wrong-plate", "wrong-camera"] {
            let root = tempfile::tempdir().unwrap();
            let worker = fixture(root.path(), behavior);
            let mut params = request();
            params.plates = vec![1];
            assert!(worker.capture(params).await.is_err(), "{behavior}");
            assert!(!root.path().join("projects/part/observations").exists());
        }
    }

    #[tokio::test]
    async fn prepared_capture_uses_the_verified_output_package_and_rejects_tampering() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), "normal");
        let package_path = "projects/part/prepared/model.gcode.3mf";
        let package = b"prepared and reoriented output layout";
        worker
            .workspace
            .write_generated_bytes(package_path, package, false)
            .unwrap();
        let slice = SliceHandle {
            project_id: "part".into(),
            output_dir: "prepared".into(),
        };
        let artifacts = json!({"model.gcode.3mf":{"artifact":{"path":package_path},"sha256":provenance::digest(package)}});
        let reference = provenance::store(
            &worker.workspace,
            provenance::Kind::Slice,
            "part",
            json!({"slice":slice,"artifacts":artifacts}),
        )
        .unwrap();
        worker.workspace.write_generated_bytes("projects/part/prepared/state.json", &serde_json::to_vec(&json!({"status":"completed","slice":slice,"artifacts":artifacts,"provenance":reference})).unwrap(), false).unwrap();
        let mut params = request();
        params.source = CaptureSource::Prepared {
            output_dir: "prepared".into(),
        };
        let captured = worker.capture(params.clone()).await.unwrap();
        assert_eq!(captured["source_sha256"], provenance::digest(package));
        assert_eq!(
            std::fs::read(root.path().join("captured-input")).unwrap(),
            package
        );
        worker
            .workspace
            .write_artifact(package_path, b"changed prepared package", true)
            .unwrap();
        let error = worker.capture(params).await.unwrap_err();
        assert!(error.to_string().contains("changed since slicing"));
    }

    #[tokio::test]
    async fn disconnect_kills_the_native_process_and_releases_admission() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture(root.path(), "slow");
        let running_worker = Arc::clone(&worker);
        let task = tokio::spawn(async move { running_worker.capture(request()).await });
        let started = root.path().join("capture-started");
        tokio::time::timeout(Duration::from_secs(20), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fake native capture must start");
        let pid: i32 = std::fs::read_to_string(started).unwrap().parse().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the native process must be killed and reaped");
        assert_eq!(worker.admission.available_permits(), 1);
        assert!(!root.path().join("projects/part/observations").exists());
    }

    #[tokio::test]
    async fn forwarding_preserves_the_native_capability_error_code() {
        use axum::{Router, http::StatusCode, routing::post};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/slice",
            post(|| async {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(
                        json!({"code":"native_capture_unavailable","error":"unsupported engine"}),
                    ),
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = super::super::forward(
            Some(&endpoint),
            super::super::SliceRequest::Capture(request()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "native_capture_unavailable");
        server.abort();
    }
}
