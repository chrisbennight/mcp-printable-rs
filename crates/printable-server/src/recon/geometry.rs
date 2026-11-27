//! Photograph geometry models: preparation, readiness and single runs.
//!
//! The model list in the worker image names each model's pinned files. The
//! caller chooses which model, device and processing size to run; this module
//! prepares the files, reports each model's preparation state and runs one
//! model on one photograph.

use std::collections::BTreeMap;

use super::*;

/// Operator settings for the geometry models.
#[derive(Debug, Clone)]
pub struct GeometryConfig {
    pub script: PathBuf,
    pub prepare_script: PathBuf,
    /// Persistent storage for downloaded and converted model files.
    pub cache: PathBuf,
    pub timeout: Duration,
    pub preparation_timeout: Duration,
}

impl GeometryConfig {
    pub fn configure(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(cache) =
            get("PRINTABLE_RECON_GEOMETRY_CACHE").filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let seconds = |name: &str, default: u64, bounds: std::ops::RangeInclusive<u64>| {
            let Some(value) = get(name) else {
                return Ok(Duration::from_secs(default));
            };
            value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|value| bounds.contains(value))
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    format!(
                        "{name} must be between {} and {}",
                        bounds.start(),
                        bounds.end()
                    )
                })
        };
        Ok(Some(Self {
            script: get("PRINTABLE_RECON_GEOMETRY_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/estimate_geometry.py".into())
                .into(),
            prepare_script: get("PRINTABLE_RECON_GEOMETRY_PREPARE_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/prepare_geometry.py".into())
                .into(),
            cache: cache.into(),
            timeout: seconds("PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS", 900, 1..=86400)?,
            preparation_timeout: seconds(
                "PRINTABLE_RECON_GEOMETRY_PREPARATION_TIMEOUT_SECONDS",
                86400,
                1..=172800,
            )?,
        }))
    }
}

/// How far each geometry model is prepared, by model name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct GeometryReadiness {
    pub models: BTreeMap<String, PreparationStatus>,
}

impl GeometryReadiness {
    /// Each model's preparation state, for the status tool.
    pub fn report(&self) -> Value {
        let models: serde_json::Map<String, Value> = self
            .models
            .iter()
            .map(|(name, status)| (name.clone(), json!(status.state)))
            .collect();
        json!({"available": true, "models": models})
    }
}

/// Files one geometry run writes; outputs the model does not produce are
/// left unwritten.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GeometryOutputs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normals: Option<PathBuf>,
}

/// One geometry model run on one photograph.
#[derive(Debug, Clone, Serialize)]
pub struct GeometryRun {
    pub image: PathBuf,
    /// The model's name in the worker's model list.
    pub entry: String,
    /// The prepared snapshot, or for a converted model its converted form.
    pub snapshot: PathBuf,
    pub device: String,
    pub side: u32,
    pub outputs: GeometryOutputs,
}

impl ReconWorker {
    /// Each model's preparation state, or why it could not be read. Nothing
    /// is downloaded and no model is loaded.
    pub(super) async fn geometry_readiness(&self) -> Option<Result<GeometryReadiness, ToolError>> {
        let config = self.config.geometry.as_ref()?;
        Some(self.read_geometry_readiness(config).await)
    }

    async fn read_geometry_readiness(
        &self,
        config: &GeometryConfig,
    ) -> Result<GeometryReadiness, ToolError> {
        let mut status = self.python_command(&config.prepare_script);
        status.arg("--status").arg(&config.cache);
        let output = tokio::time::timeout(MODEL_CHECK_TIMEOUT, status.output())
            .await
            .map_err(|_| {
                reconstruction_error(
                    "timeout",
                    "the geometry model preparation check did not finish",
                )
            })??;
        serde_json::from_slice(&output.stdout).map_err(|_| {
            reconstruction_error(
                "protocol",
                "the geometry model preparation check returned no answer",
            )
        })
    }

    /// Download, verify and convert the named models, returning each
    /// model's snapshot and, where it has one, its converted form.
    /// Preparation uses no GPU, so it runs outside the photograph job lane.
    pub async fn prepare_geometry(&self, models: &[String]) -> Result<Value, ToolError> {
        let config = self.geometry_config()?;
        let staging = tempfile::Builder::new()
            .prefix("printable-geometry-prepare-")
            .tempdir()?;
        let mut command = self.python_command(&config.prepare_script);
        command.arg("prepare");
        allow_downloads(&mut command);
        self.run_command(
            command,
            &json!({"cache": config.cache, "models": models}),
            staging.path(),
            config.preparation_timeout,
            "geometry model preparation",
            None,
        )
        .await
    }

    /// Run one prepared geometry model on one photograph, in the photograph
    /// job lane, with this worker's CPU thread count.
    pub async fn estimate_geometry(&self, run: GeometryRun) -> Result<Value, ToolError> {
        let config = self.geometry_config()?;
        if run.device == "cuda" && self.config.device == "cpu" {
            return Err(reconstruction_error(
                "unavailable",
                "this worker is configured not to use a GPU",
            ));
        }
        let mut job = serde_json::to_value(&run)?;
        job["threads"] = json!(self.config.cpu_threads);
        let _permit = self.permit()?;
        let staging = tempfile::Builder::new()
            .prefix("printable-geometry-run-")
            .tempdir()?;
        self.run(
            &config.script,
            &job,
            staging.path(),
            config.timeout,
            "geometry estimation",
        )
        .await
    }

    fn geometry_config(&self) -> Result<&GeometryConfig, ToolError> {
        self.config.geometry.as_ref().ok_or_else(|| {
            reconstruction_error(
                "unavailable",
                "the geometry models need a configured model cache",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREPARE: &str = r#"
import json, os, sys
if sys.argv[1] == '--status':
    assert os.environ['HF_HUB_OFFLINE'] == '1'
    prepared = os.path.isdir(os.path.join(sys.argv[2], 'snapshots'))
    state = {'state': 'prepared', 'last_verified_unix_ms': 5} if prepared else {'state': 'not_prepared', 'last_verified_unix_ms': None}
    print(json.dumps({'models': {'small': state}}))
    sys.exit(0)
request = json.load(open(sys.argv[2]))
assert sys.argv[1] == 'prepare'
assert os.environ['HF_HUB_OFFLINE'] == '0'
assert os.environ['HF_HUB_DISABLE_IMPLICIT_TOKEN'] == '1'
os.makedirs(os.path.join(request['cache'], 'snapshots'), exist_ok=True)
print(json.dumps({'result': {'models': {name: {'snapshot': os.path.join(request['cache'], 'snapshots', name)} for name in request['models']}}}))
"#;

    const ESTIMATE: &str = r#"
import json, os, sys
request = json.load(open(sys.argv[1]))
assert os.environ['HF_HUB_OFFLINE'] == '1'
assert os.environ['OMP_NUM_THREADS'] == str(request['threads'])
assert 'gpu_budget_mib' not in request
if 'oom' in request['image']:
    print(json.dumps({'error': {'code': 'resource_limit', 'message': 'the GPU ran out of memory'}}))
    sys.exit(2)
open(request['outputs']['depth'], 'wb').write(b'depth')
print(json.dumps({'result': {'entry': request['entry'], 'device': request['device'],
    'threads': request['threads'], 'outputs': ['depth']}}))
"#;

    fn configured(root: &Path, worker: &mut ReconWorker) {
        let prepare = root.join("runtime/prepare_geometry.py");
        let estimate = root.join("runtime/estimate_geometry.py");
        std::fs::write(&prepare, PREPARE).unwrap();
        std::fs::write(&estimate, ESTIMATE).unwrap();
        worker.config.geometry = Some(GeometryConfig {
            script: estimate,
            prepare_script: prepare,
            cache: root.join("geometry"),
            timeout: Duration::from_secs(60),
            preparation_timeout: Duration::from_secs(60),
        });
        worker.config.cpu_threads = 3;
    }

    #[test]
    fn configuration_is_optional_and_bounds_timeouts() {
        assert!(GeometryConfig::configure(&|_| None).unwrap().is_none());
        let get = |name: &str| match name {
            "PRINTABLE_RECON_GEOMETRY_CACHE" => Some("/models/geometry".into()),
            _ => None,
        };
        let config = GeometryConfig::configure(&get).unwrap().unwrap();
        assert_eq!(config.cache, PathBuf::from("/models/geometry"));
        assert_eq!(config.timeout, Duration::from_secs(900));
        assert_eq!(config.preparation_timeout, Duration::from_secs(86400));
        let with = |key: &'static str, value: &'static str| {
            GeometryConfig::configure(&move |name| {
                if name == key {
                    Some(value.into())
                } else {
                    get(name)
                }
            })
        };
        for value in ["0", "86401", "soon"] {
            assert!(with("PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS", value).is_err());
        }
        assert!(
            with(
                "PRINTABLE_RECON_GEOMETRY_PREPARATION_TIMEOUT_SECONDS",
                "172801"
            )
            .is_err()
        );
        assert_eq!(
            with("PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS", "60")
                .unwrap()
                .unwrap()
                .timeout,
            Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn readiness_reports_each_models_preparation_without_downloading() {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        let (_, answer) = worker.readiness().await;
        assert_eq!(answer["capabilities"]["geometry"], Value::Null);
        configured(root.path(), &mut worker);
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        let geometry: GeometryReadiness =
            serde_json::from_value(answer["capabilities"]["geometry"].clone()).unwrap();
        assert_eq!(
            geometry.models["small"].state,
            PreparationState::NotPrepared
        );
        assert_eq!(
            geometry.report(),
            json!({"available": true, "models": {"small": "not_prepared"}})
        );
        // A geometry check that cannot run withholds geometry alone.
        worker.config.geometry.as_mut().unwrap().prepare_script = root.path().join("absent.py");
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        assert!(answer["capabilities"]["depth"].as_bool().unwrap());
        assert_eq!(answer["capabilities"]["geometry"], Value::Null);
        assert!(answer["missing"]["geometry"]["error"].is_string());
    }

    #[tokio::test]
    async fn preparation_downloads_outside_the_gpu_lane() {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        assert_eq!(
            worker
                .prepare_geometry(&["small".into()])
                .await
                .unwrap_err()
                .code(),
            "unavailable"
        );
        configured(root.path(), &mut worker);
        let permit = worker.permit().unwrap();
        let prepared = worker.prepare_geometry(&["small".into()]).await.unwrap();
        drop(permit);
        assert!(
            prepared["models"]["small"]["snapshot"]
                .as_str()
                .unwrap()
                .ends_with("snapshots/small")
        );
        let geometry = worker.geometry_readiness().await.unwrap().unwrap();
        assert_eq!(geometry.models["small"].state, PreparationState::Prepared);
    }

    #[tokio::test]
    async fn a_run_gets_the_threads_and_takes_its_turn() {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        configured(root.path(), &mut worker);
        let output = root.path().join("depth.npy");
        let run = |image: &str, device: &str| GeometryRun {
            image: root.path().join(image),
            entry: "small".into(),
            snapshot: root.path().join("geometry/snapshots/small"),
            device: device.into(),
            side: 1024,
            outputs: GeometryOutputs {
                depth: Some(output.clone()),
                ..GeometryOutputs::default()
            },
        };
        assert_eq!(
            worker
                .estimate_geometry(run("photo.png", "cuda"))
                .await
                .unwrap_err()
                .code(),
            "unavailable"
        );
        let result = worker
            .estimate_geometry(run("photo.png", "cpu"))
            .await
            .unwrap();
        assert_eq!(result["threads"], 3);
        assert!(output.is_file());
        worker.config.device = "cuda".into();
        let result = worker
            .estimate_geometry(run("photo.png", "cuda"))
            .await
            .unwrap();
        assert_eq!(result["device"], "cuda");
        let error = worker
            .estimate_geometry(run("oom.png", "cuda"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "resource_limit");
        let permit = worker.permit().unwrap();
        let error = worker
            .estimate_geometry(run("photo.png", "cuda"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "busy");
        drop(permit);
    }
}
