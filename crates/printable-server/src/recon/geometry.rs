//! Photograph geometry models: route selection, preparation and single runs.
//!
//! The model registry in the worker image lists, for every analysis role, the
//! options best first. The worker's GPU and RAM budgets decide which option
//! each role takes; the Python scripts apply that policy, and this module
//! supplies the budgets and reports the answer.

use std::collections::BTreeMap;

use super::*;

/// Operator settings for the geometry models.
#[derive(Debug, Clone)]
pub struct GeometryConfig {
    pub script: PathBuf,
    pub prepare_script: PathBuf,
    /// Runs a whole photograph analysis; beside it are the registry and the
    /// analysis code the cache key covers.
    pub analyze_script: PathBuf,
    /// Persistent storage for downloaded and converted model files.
    pub cache: PathBuf,
    /// GPU memory one model run may allocate; enforced inside the process.
    pub gpu_budget_mib: u64,
    /// Host memory a route may need, or `None` for no limit.
    pub ram_budget_mib: Option<u64>,
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
        let parse = |name: &str, value: String, bounds: std::ops::RangeInclusive<u64>| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|value| bounds.contains(value))
                .ok_or_else(|| {
                    format!(
                        "{name} must be between {} and {}",
                        bounds.start(),
                        bounds.end()
                    )
                })
        };
        let number =
            |name: &str, default: u64, bounds: std::ops::RangeInclusive<u64>| match get(name) {
                Some(value) => parse(name, value, bounds),
                None => Ok(default),
            };
        let ram_budget_mib =
            match get("PRINTABLE_RECON_RAM_BUDGET_MIB").filter(|value| !value.trim().is_empty()) {
                Some(value) => Some(parse(
                    "PRINTABLE_RECON_RAM_BUDGET_MIB",
                    value,
                    512..=1048576,
                )?),
                None => None,
            };
        Ok(Some(Self {
            script: get("PRINTABLE_RECON_GEOMETRY_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/estimate_geometry.py".into())
                .into(),
            prepare_script: get("PRINTABLE_RECON_GEOMETRY_PREPARE_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/prepare_geometry.py".into())
                .into(),
            analyze_script: get("PRINTABLE_RECON_GEOMETRY_ANALYZE_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/analyze_photo.py".into())
                .into(),
            cache: cache.into(),
            gpu_budget_mib: number("PRINTABLE_RECON_GPU_BUDGET_MIB", 3072, 256..=262144)?,
            ram_budget_mib,
            timeout: Duration::from_secs(number(
                "PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS",
                900,
                1..=86400,
            )?),
            preparation_timeout: Duration::from_secs(number(
                "PRINTABLE_RECON_GEOMETRY_PREPARATION_TIMEOUT_SECONDS",
                86400,
                1..=172800,
            )?),
        }))
    }
}

/// The model one analysis role would run on this worker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SelectedRoute {
    /// Registry entry name.
    pub entry: String,
    pub device: String,
    /// Longest side, in pixels, the photograph is processed at.
    pub side: u32,
    /// A better option did not fit this worker's budgets.
    pub fallback: bool,
    /// Why each better option was passed over.
    #[serde(default)]
    pub rejected: Vec<String>,
}

/// A role's route, or why no option fits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RoleRoute {
    Selected(SelectedRoute),
    Unavailable { error: String },
}

/// What the geometry models would run and how far each entry is prepared.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct GeometryReadiness {
    pub roles: BTreeMap<String, RoleRoute>,
    pub preparation: BTreeMap<String, PreparationStatus>,
}

impl GeometryReadiness {
    /// Whether a photograph analysis can run: a subject mask route and at
    /// least one depth route. A request whose other roles have no route is
    /// refused when it is planned.
    pub fn can_analyze(&self) -> bool {
        let selected = |role: &str| matches!(self.roles.get(role), Some(RoleRoute::Selected(_)));
        selected("mask") && (selected("depth.shallow") || selected("depth.deep"))
    }

    /// A short per-role summary for the status tool.
    pub fn report(&self) -> Value {
        let roles: serde_json::Map<String, Value> = self
            .roles
            .iter()
            .map(|(role, route)| {
                let value = match route {
                    RoleRoute::Selected(route) => {
                        let mut value = json!({
                            "model": route.entry,
                            "device": route.device,
                            "side_px": route.side,
                            "fallback": route.fallback,
                            "preparation": self.preparation.get(&route.entry).map(|status| status.state),
                        });
                        if route.fallback {
                            value["fallback_reasons"] = json!(route.rejected);
                        }
                        value
                    }
                    RoleRoute::Unavailable { error } => json!({"unavailable": error}),
                };
                (role.clone(), value)
            })
            .collect();
        json!({"available": true, "roles": roles})
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

/// One registry entry run on one photograph.
#[derive(Debug, Clone, Serialize)]
pub struct GeometryRun {
    pub image: PathBuf,
    pub entry: String,
    /// The prepared snapshot, or for a converted entry its converted model.
    pub snapshot: PathBuf,
    pub device: String,
    pub side: u32,
    pub outputs: GeometryOutputs,
}

impl ReconWorker {
    /// The route for every role and each entry's preparation state, or why
    /// they could not be read. Nothing is downloaded.
    pub(super) async fn geometry_readiness(&self) -> Option<Result<GeometryReadiness, ToolError>> {
        let config = self.config.geometry.as_ref()?;
        Some(self.read_geometry_readiness(config).await)
    }

    async fn read_geometry_readiness(
        &self,
        config: &GeometryConfig,
    ) -> Result<GeometryReadiness, ToolError> {
        let staging = tempfile::Builder::new()
            .prefix("printable-geometry-")
            .tempdir()?;
        let request = json!({
            "gpu_budget_mib": (self.config.device != "cpu").then_some(config.gpu_budget_mib),
            "ram_budget_mib": config.ram_budget_mib,
        });
        let mut select = self.python_command(&config.prepare_script);
        select.arg("--select");
        let mut status = self.python_command(&config.prepare_script);
        status.arg("--status").arg(&config.cache);
        let (selected, status) = tokio::join!(
            self.run_command(
                select,
                &request,
                staging.path(),
                MODEL_CHECK_TIMEOUT,
                "geometry model selection",
                None,
            ),
            tokio::time::timeout(MODEL_CHECK_TIMEOUT, status.output())
        );
        #[derive(Deserialize)]
        struct Selected {
            roles: BTreeMap<String, RoleRoute>,
        }
        #[derive(Deserialize)]
        struct Status {
            entries: BTreeMap<String, PreparationStatus>,
        }
        let roles = serde_json::from_value::<Selected>(selected?)
            .map_err(|_| {
                reconstruction_error("protocol", "the geometry model selection was malformed")
            })?
            .roles;
        let output = status.map_err(|_| {
            reconstruction_error(
                "timeout",
                "the geometry model preparation check did not finish",
            )
        })??;
        let preparation = serde_json::from_slice::<Status>(&output.stdout)
            .map_err(|_| {
                reconstruction_error(
                    "protocol",
                    "the geometry model preparation check returned no answer",
                )
            })?
            .entries;
        Ok(GeometryReadiness { roles, preparation })
    }

    /// Download, verify and convert the named registry entries, returning
    /// each entry's snapshot and, where it has one, its converted model.
    /// Preparation uses no GPU, so it runs outside the photograph job lane.
    pub async fn prepare_geometry(&self, entries: &[String]) -> Result<Value, ToolError> {
        self.prepare_geometry_controlled(entries, None).await
    }

    pub(super) async fn prepare_geometry_controlled(
        &self,
        entries: &[String],
        control: Option<&JobControl>,
    ) -> Result<Value, ToolError> {
        let config = self.geometry_config()?;
        let staging = tempfile::Builder::new()
            .prefix("printable-geometry-prepare-")
            .tempdir()?;
        let mut command = self.python_command(&config.prepare_script);
        command.arg("prepare");
        allow_downloads(&mut command);
        self.run_command(
            command,
            &json!({"cache": config.cache, "entries": entries}),
            staging.path(),
            config.preparation_timeout,
            "geometry model preparation",
            control,
        )
        .await
    }

    /// Run one prepared geometry model on one photograph, in the photograph
    /// job lane, with this worker's GPU budget and CPU thread count.
    pub async fn estimate_geometry(&self, run: GeometryRun) -> Result<Value, ToolError> {
        let config = self.geometry_config()?;
        if run.device == "cuda" && self.config.device == "cpu" {
            return Err(reconstruction_error(
                "unavailable",
                "this worker is configured not to use a GPU",
            ));
        }
        let mut job = serde_json::to_value(&run)?;
        job["gpu_budget_mib"] = json!((run.device == "cuda").then_some(config.gpu_budget_mib));
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

    pub(super) fn geometry_config(&self) -> Result<&GeometryConfig, ToolError> {
        self.config.geometry.as_ref().ok_or_else(|| {
            reconstruction_error(
                "unavailable",
                "the geometry models need a configured model cache",
            )
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const PREPARE: &str = r#"
import json, os, sys
if sys.argv[1] == '--status':
    prepared = os.path.isdir(os.path.join(sys.argv[2], 'snapshots'))
    state = {'state': 'prepared', 'last_verified_unix_ms': 5} if prepared else {'state': 'not_prepared', 'last_verified_unix_ms': None}
    print(json.dumps({'entries': {'small': state}}))
    sys.exit(0)
request = json.load(open(sys.argv[2]))
assert 'HF_TOKEN' not in os.environ or sys.argv[1] == 'prepare'
if sys.argv[1] == '--select':
    assert os.environ['HF_HUB_OFFLINE'] == '1'
    if request['gpu_budget_mib'] is None:
        roles = {'depth.deep': {'entry': 'small', 'device': 'cpu', 'side': 1024, 'fallback': True,
            'rejected': ['large on cuda at 2048 px: no GPU is available to this worker']},
            'normals': {'error': 'no normals model fits this worker'}}
    else:
        roles = {'depth.deep': {'entry': 'small', 'device': 'cuda', 'side': 1024, 'fallback': False, 'rejected': []}}
    print(json.dumps({'result': {'roles': roles, 'budget': request}}))
    sys.exit(0)
assert sys.argv[1] == 'prepare'
assert os.environ['HF_HUB_OFFLINE'] == '0'
assert os.environ['HF_HUB_DISABLE_IMPLICIT_TOKEN'] == '1'
os.makedirs(os.path.join(request['cache'], 'snapshots'), exist_ok=True)
print(json.dumps({'result': {'entries': {name: {'snapshot': os.path.join(request['cache'], 'snapshots', name)} for name in request['entries']}}}))
"#;

    const ESTIMATE: &str = r#"
import json, os, sys
request = json.load(open(sys.argv[1]))
assert os.environ['HF_HUB_OFFLINE'] == '1'
assert os.environ['OMP_NUM_THREADS'] == str(request['threads'])
if 'oom' in request['image']:
    print(json.dumps({'error': {'code': 'resource_limit', 'message': 'needs more than the budget'}}))
    sys.exit(2)
open(request['outputs']['depth'], 'wb').write(b'depth')
print(json.dumps({'result': {'entry': request['entry'], 'device': request['device'],
    'gpu_budget_mib': request['gpu_budget_mib'], 'threads': request['threads'], 'outputs': ['depth']}}))
"#;

    pub(in crate::recon) fn configured(root: &Path, worker: &mut ReconWorker) {
        let prepare = root.join("runtime/prepare_geometry.py");
        let estimate = root.join("runtime/estimate_geometry.py");
        std::fs::write(&prepare, PREPARE).unwrap();
        std::fs::write(&estimate, ESTIMATE).unwrap();
        worker.config.geometry = Some(GeometryConfig {
            script: estimate,
            prepare_script: prepare,
            analyze_script: root.join("runtime/analyze_photo.py"),
            cache: root.join("geometry"),
            gpu_budget_mib: 3072,
            ram_budget_mib: None,
            timeout: Duration::from_secs(60),
            preparation_timeout: Duration::from_secs(60),
        });
        worker.config.cpu_threads = 3;
    }

    #[test]
    fn configuration_is_optional_and_bounds_budgets() {
        assert!(GeometryConfig::configure(&|_| None).unwrap().is_none());
        let get = |name: &str| match name {
            "PRINTABLE_RECON_GEOMETRY_CACHE" => Some("/models/geometry".into()),
            _ => None,
        };
        let config = GeometryConfig::configure(&get).unwrap().unwrap();
        assert_eq!(config.gpu_budget_mib, 3072);
        assert_eq!(config.ram_budget_mib, None);
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
        for value in ["0", "255", "262145", "-1", "no"] {
            assert!(with("PRINTABLE_RECON_GPU_BUDGET_MIB", value).is_err());
        }
        for value in ["511", "1048577", "lots"] {
            assert!(with("PRINTABLE_RECON_RAM_BUDGET_MIB", value).is_err());
        }
        assert_eq!(
            with("PRINTABLE_RECON_RAM_BUDGET_MIB", "8192")
                .unwrap()
                .unwrap()
                .ram_budget_mib,
            Some(8192)
        );
        assert!(with("PRINTABLE_RECON_GEOMETRY_TIMEOUT_SECONDS", "0").is_err());
    }

    #[tokio::test]
    async fn readiness_reports_routes_and_preparation_without_downloading() {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        let (_, answer) = worker.readiness().await;
        assert_eq!(answer["capabilities"]["geometry"], Value::Null);
        configured(root.path(), &mut worker);
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        let geometry: GeometryReadiness =
            serde_json::from_value(answer["capabilities"]["geometry"].clone()).unwrap();
        // The worker's device is cpu, so no GPU budget is offered.
        assert_eq!(
            geometry.roles["depth.deep"],
            RoleRoute::Selected(SelectedRoute {
                entry: "small".into(),
                device: "cpu".into(),
                side: 1024,
                fallback: true,
                rejected: vec![
                    "large on cuda at 2048 px: no GPU is available to this worker".into()
                ],
            })
        );
        assert_eq!(
            geometry.roles["normals"],
            RoleRoute::Unavailable {
                error: "no normals model fits this worker".into()
            }
        );
        assert_eq!(
            geometry.preparation["small"].state,
            PreparationState::NotPrepared
        );
        let report = geometry.report();
        assert_eq!(report["roles"]["depth.deep"]["preparation"], "not_prepared");
        assert_eq!(
            report["roles"]["depth.deep"]["fallback_reasons"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(report["roles"]["normals"]["unavailable"].is_string());

        worker.config.device = "cuda".into();
        let (_, answer) = worker.readiness().await;
        assert_eq!(
            answer["capabilities"]["geometry"]["roles"]["depth.deep"]["device"],
            "cuda"
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
            prepared["entries"]["small"]["snapshot"]
                .as_str()
                .unwrap()
                .ends_with("snapshots/small")
        );
        let geometry = worker.geometry_readiness().await.unwrap().unwrap();
        assert_eq!(
            geometry.preparation["small"].state,
            PreparationState::Prepared
        );
    }

    #[tokio::test]
    async fn a_run_gets_the_budget_and_threads_and_takes_its_turn() {
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
        assert_eq!(result["gpu_budget_mib"], Value::Null);
        assert_eq!(result["threads"], 3);
        assert!(output.is_file());
        worker.config.device = "cuda".into();
        let result = worker
            .estimate_geometry(run("photo.png", "cuda"))
            .await
            .unwrap();
        assert_eq!(result["gpu_budget_mib"], 3072);
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
