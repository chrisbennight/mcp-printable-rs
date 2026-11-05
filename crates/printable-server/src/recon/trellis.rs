//! The optional disk-streamed TRELLIS.2 shape pipeline.

use super::*;

#[derive(Debug, Clone)]
pub struct TrellisConfig {
    pub script: PathBuf,
    pub cache: PathBuf,
    pub scratch: PathBuf,
    pub vram_budget_bytes: u64,
    pub timeout: Duration,
    pub preparation_timeout: Duration,
}

impl TrellisConfig {
    pub fn configure(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(cache) =
            get("PRINTABLE_RECON_TRELLIS_CACHE").filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let number = |name: &str, default: u64, bounds: std::ops::RangeInclusive<u64>| {
            let value = match get(name) {
                Some(value) => value.parse::<u64>().ok(),
                None => Some(default),
            };
            value.filter(|value| bounds.contains(value)).ok_or_else(|| {
                format!(
                    "{name} must be between {} and {}",
                    bounds.start(),
                    bounds.end()
                )
            })
        };
        Ok(Some(Self {
            script: get("PRINTABLE_RECON_TRELLIS_SCRIPT")
                .unwrap_or_else(|| "/opt/printable/recon/generate_trellis.py".into())
                .into(),
            cache: cache.into(),
            scratch: get("PRINTABLE_RECON_TRELLIS_SCRATCH")
                .unwrap_or_else(|| "/scratch/trellis2".into())
                .into(),
            vram_budget_bytes: number("PRINTABLE_RECON_TRELLIS_VRAM_MIB", 4096, 256..=65536)?
                * 1024
                * 1024,
            timeout: Duration::from_secs(number(
                "PRINTABLE_RECON_TRELLIS_TIMEOUT_SECONDS",
                21600,
                1..=86400,
            )?),
            preparation_timeout: Duration::from_secs(number(
                "PRINTABLE_RECON_TRELLIS_PREPARATION_TIMEOUT_SECONDS",
                86400,
                1..=172800,
            )?),
        }))
    }

    pub fn available(&self) -> bool {
        self.script.is_file() && self.cache.is_dir() && self.scratch.is_dir()
    }
}

impl ReconWorker {
    pub(super) async fn generate_trellis(
        &self,
        request: ObjectRequest,
    ) -> Result<Value, ToolError> {
        let config = self
            .config
            .trellis
            .as_ref()
            .filter(|config| config.available())
            .ok_or_else(|| {
                reconstruction_error(
                    "unavailable",
                    "TRELLIS.2 needs configured persistent model and scratch directories",
                )
            })?;
        if self.config.device == "cpu" {
            return Err(reconstruction_error(
                "unavailable",
                "TRELLIS.2 requires a CUDA GPU",
            ));
        }
        let snapshot = self
            .workspace
            .snapshot_artifact_bounded(&request.image, MAX_IMAGE_BYTES)?;
        let staging = tempfile::Builder::new()
            .prefix("printable-trellis-")
            .tempdir_in(&config.scratch)?;
        let mesh = staging.path().join("object.stl");
        let raw = staging.path().join("raw");
        let cutout = staging.path().join("foreground.png");
        let mut job = json!({
            "image": snapshot.path(), "cutout": cutout, "raw": raw, "mesh": mesh,
            "cache": config.cache, "scratch": staging.path(),
            "shared_models": self.config.object_model,
            "resolution": request.resolution.unwrap_or(OBJECT_DEFAULT_RESOLUTION),
            "steps": request.steps, "seed": request.seed.unwrap_or(42),
            "size_mm": request.size_mm.unwrap_or(100.0),
            "target_triangles": if request.simplify.unwrap_or(true) {Some(request.target_triangles.unwrap_or(1_000_000))} else {None},
            "vram_budget_bytes": config.vram_budget_bytes,
        });
        let started = std::time::Instant::now();
        let mut preparation = self.python_command(&config.script);
        preparation
            .arg("prepare")
            .env("HF_HUB_OFFLINE", "0")
            .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1");
        for name in [
            "HF_ENDPOINT",
            "HF_TOKEN",
            "HF_HUB_DISABLE_XET",
            "HF_HUB_DOWNLOAD_TIMEOUT",
            "HF_HUB_ETAG_TIMEOUT",
        ] {
            if let Some(value) = std::env::var_os(name) {
                preparation.env(name, value);
            }
        }
        let prepared = self
            .run_command(
                preparation,
                &job,
                staging.path(),
                config.preparation_timeout,
                "TRELLIS.2 model preparation",
            )
            .await?;
        job["snapshot"] = prepared
            .get("snapshot")
            .filter(|value| value.is_string())
            .cloned()
            .ok_or_else(|| {
                reconstruction_error("protocol", "TRELLIS.2 preparation returned no snapshot")
            })?;
        // Preparation uses no GPU. Acquire the shared lane only once the model
        // is ready, so a cold download does not block other photograph work.
        let _permit = self.permit()?;
        let generation_started = std::time::Instant::now();
        let mut stages = vec![
            json!({"stage":"preparing_models", "seconds":prepared["seconds"], "peak_ram_bytes":prepared["peak_ram_bytes"]}),
        ];
        let mut result =
            json!({"model":"trellis2", "manifest":prepared["manifest"], "device":"cuda"});
        for stage in ["foreground", "generate", "deliver"] {
            let remaining = config
                .timeout
                .checked_sub(generation_started.elapsed())
                .filter(|value| !value.is_zero())
                .ok_or_else(|| {
                    reconstruction_error("timeout", "TRELLIS.2 generation exceeded its deadline")
                })?;
            let mut command = self.python_command(&config.script);
            command.arg(stage).env("OMP_NUM_THREADS", "8");
            let answer = self
                .run_command(
                    command,
                    &job,
                    staging.path(),
                    remaining,
                    "TRELLIS.2 generation",
                )
                .await?;
            stages.push(json!({"stage":stage,"seconds":answer["seconds"],"peak_ram_bytes":answer["peak_ram_bytes"],"peak_gpu_memory_bytes":answer["peak_gpu_memory_bytes"]}));
            match stage {
                "foreground" => result["background"] = answer["background"].clone(),
                "generate" => {
                    job["resolution"] = answer["resolution"].clone();
                    result
                        .as_object_mut()
                        .expect("result object")
                        .extend(answer.as_object().expect("stage object").clone());
                }
                "deliver" => result
                    .as_object_mut()
                    .expect("result object")
                    .extend(answer.as_object().expect("stage object").clone()),
                _ => unreachable!(),
            }
        }
        // The workspace admits storage and rechecks overwrite atomically when
        // publishing. Generated files stream independently of inline MCP limits.
        result["artifact"] = json!(self.workspace.commit_generated_artifact(
            &request.mesh_path,
            &mesh,
            request.overwrite
        )?);
        result["image_sha256"] = json!(file_digest(snapshot.path())?);
        result["seconds"] = json!(started.elapsed().as_secs_f64());
        result["peak_ram_bytes"] = json!(
            stages
                .iter()
                .filter_map(|stage| stage["peak_ram_bytes"].as_u64())
                .max()
                .unwrap_or(0)
        );
        result["peak_gpu_memory_bytes"] = json!(
            stages
                .iter()
                .filter_map(|stage| stage["peak_gpu_memory_bytes"].as_u64())
                .max()
                .unwrap_or(0)
        );
        result["process_stages"] = json!(stages);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNTIME: &str = r#"
import json, os, sys
stage = sys.argv[1]
request = json.load(open(sys.argv[2]))
assert os.environ['HF_HUB_OFFLINE'] == ('0' if stage == 'prepare' else '1')
if stage == 'prepare':
    assert os.environ['HF_HUB_DISABLE_IMPLICIT_TOKEN'] == '1'
    result = {'snapshot':request['cache'],'manifest':'verified-model-id'}
else:
    assert 'HF_TOKEN' not in os.environ
    if stage == 'foreground':
        result = {'background':'alpha','peak_gpu_memory_bytes':200}
    elif stage == 'generate':
        result = {'resolution':512,'seed':request['seed'],'peak_gpu_memory_bytes':400,'stages':[{'stage':'shape'}]}
    else:
        assert request['resolution'] == 512
        with open(request['mesh'], 'wb') as mesh:
            mesh.write(b'generated shape')
            if request['target_triangles'] is None:
                mesh.truncate(256*1024*1024+1)
        result = {'triangles':1,'generated_triangles':2,'simplified':request['target_triangles'] is not None}
result.update(seconds=0.01,peak_ram_bytes=1000)
print(json.dumps({'result':result}))
"#;

    #[test]
    fn configuration_is_optional_and_bounds_resource_settings() {
        assert!(TrellisConfig::configure(&|_| None).unwrap().is_none());
        let get = |name: &str| match name {
            "PRINTABLE_RECON_TRELLIS_CACHE" => Some("/models/trellis2".into()),
            _ => None,
        };
        let config = TrellisConfig::configure(&get).unwrap().unwrap();
        assert_eq!(config.vram_budget_bytes, 4 * 1024u64.pow(3));
        for value in ["0", "-1", "65537", "no"] {
            assert!(
                TrellisConfig::configure(&|name| if name == "PRINTABLE_RECON_TRELLIS_VRAM_MIB" {
                    Some(value.into())
                } else {
                    get(name)
                })
                .is_err()
            );
        }
    }

    #[test]
    fn default_model_and_backend_specific_options_are_explicit() {
        let mut request: ObjectRequest =
            serde_json::from_value(json!({"image":"photo.png","mesh_path":"shape.stl"})).unwrap();
        assert_eq!(request.model, ObjectModel::Pixal3d);
        request.simplify = Some(false);
        assert!(request.validate().is_err());
        request.model = ObjectModel::Trellis2;
        assert!(request.validate().is_ok());
        request.target_triangles = Some(10000);
        assert!(request.validate().is_err());
        request.target_triangles = None;
        request.fov_degrees = Some(20.0);
        assert!(request.validate().is_err());
    }

    #[tokio::test]
    async fn trellis_downloads_outside_the_gpu_lane_and_streams_full_detail() {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        let script = root.path().join("runtime/trellis.py");
        std::fs::write(&script, RUNTIME).unwrap();
        let cache = root.path().join("models");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&cache).unwrap();
        std::fs::create_dir(&scratch).unwrap();
        worker.config.device = "cuda".into();
        worker.config.trellis = Some(TrellisConfig {
            script,
            cache,
            scratch,
            vram_budget_bytes: 4 * 1024u64.pow(3),
            timeout: Duration::from_secs(60),
            preparation_timeout: Duration::from_secs(60),
        });
        let mut request = super::super::tests::object_request("side.png");
        request.model = ObjectModel::Trellis2;
        let permit = worker.permit().unwrap();
        let error = worker.generate_object(request.clone()).await.unwrap_err();
        assert_eq!(error.code(), "busy");
        drop(permit);
        let (ready, answer) = worker.readiness().await;
        assert!(ready);
        assert_eq!(answer["capabilities"]["trellis2"], true);
        let result = worker.generate_object(request.clone()).await.unwrap();
        assert_eq!(result["model"], "trellis2");
        assert_eq!(result["simplified"], true);
        assert_eq!(result["peak_ram_bytes"], 1000);
        assert_eq!(result["peak_gpu_memory_bytes"], 400);
        assert_eq!(result["process_stages"].as_array().unwrap().len(), 4);
        request.mesh_path = "objects/full.stl".into();
        request.simplify = Some(false);
        let result = worker.generate_object(request).await.unwrap();
        assert_eq!(result["simplified"], false);
        assert!(result["artifact"]["size_bytes"].as_u64().unwrap() > MAX_OBJECT_MESH_BYTES);
        assert!(root.path().join("workspace/objects/full.stl").is_file());
        assert_eq!(
            std::fs::read_dir(root.path().join("scratch"))
                .unwrap()
                .count(),
            0
        );
    }
}
