//! Photograph analysis: one cached set of depth, normals, subject mask, focal
//! length and confidence for a photograph, made by the geometry models.
//!
//! An analysis is keyed by the photograph's SHA-256, its settings with every
//! default filled in, the worker's budgets, and the registry and analysis
//! code. Its maps and manifest live under `.printable/photo-analysis/<key>/`,
//! which only the server writes. The manifest is written last and records
//! each map's SHA-256, so a directory is a cache hit only when its manifest
//! exists and every map still matches it. A hit runs no Python at all.

use std::io::{Read, Seek, SeekFrom};

use super::*;
use photo_jobs::JobInput;

const CACHE_ROOT: &str = ".printable/photo-analysis";
/// Changes whenever the key's inputs or the stored layout change.
const CACHE_FORMAT: &str = "printable-photo-analysis-cache 1";
/// Each map the analyzer writes, by manifest name and file name.
const MAP_FILES: [(&str, &str); 4] = [
    ("depth", "depth.tiff"),
    ("normals", "normals.png"),
    ("mask", "mask.png"),
    ("confidence", "confidence.png"),
];
/// Largest map committed to the cache: a 2048-pixel 32-bit depth map or a
/// 16-bit RGB normal map, with room for the container.
const MAX_ANALYSIS_MAP_BYTES: u64 = 64 * 1024 * 1024;
/// How long planning, which loads no model, may take.
const PLANNING_TIMEOUT: Duration = Duration::from_secs(60);

/// The settings with every default filled in, as the cache key and the
/// analyzer read them.
fn normalized(request: &AnalyzeRequest) -> Value {
    let interpretation = match request.subject {
        Subject::Shallow => Some(request.interpretation.unwrap_or(Interpretation::Sculpture)),
        Subject::Deep => None,
    };
    json!({
        "subject": request.subject,
        "quality": request.quality.unwrap_or_default(),
        "interpretation": interpretation,
        "normals": request.normals,
        "focal_length_px": request.focal_length_px,
    })
}

fn protocol(message: &str) -> ToolError {
    reconstruction_error("protocol", message.to_owned())
}

/// The sample format a stored map declares in its own header: the
/// container, bits per sample and whether samples are floating point.
fn stored_precision(path: &Path) -> std::io::Result<Option<(&'static str, u16, bool)>> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 26];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if header.starts_with(b"\x89PNG\r\n\x1a\n") && &header[12..16] == b"IHDR" {
        return Ok(Some(("png", u16::from(header[24]), false)));
    }
    let little = match &header[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return Ok(None),
    };
    let u16_at = |bytes: &[u8]| {
        let pair = [bytes[0], bytes[1]];
        if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let u32_at = |bytes: &[u8]| {
        let quad = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if little {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        }
    };
    file.seek(SeekFrom::Start(u64::from(u32_at(&header[4..8]))))?;
    let mut count = [0u8; 2];
    file.read_exact(&mut count)?;
    let mut entries = vec![0u8; usize::from(u16_at(&count)) * 12];
    file.read_exact(&mut entries)?;
    // TIFF defaults: one bit per sample, unsigned integers.
    let (mut bits, mut format) = (1u16, 1u16);
    for entry in entries.chunks_exact(12) {
        let tag = u16_at(&entry[..2]);
        if tag != 258 && tag != 339 {
            continue;
        }
        let values = u32_at(&entry[4..8]);
        let value = if values <= 2 {
            u16_at(&entry[8..10])
        } else {
            // Several channels: the shorts are stored elsewhere; the first
            // describes them all for the maps checked here.
            file.seek(SeekFrom::Start(u64::from(u32_at(&entry[8..12]))))?;
            let mut first = [0u8; 2];
            file.read_exact(&mut first)?;
            u16_at(&first)
        };
        if tag == 258 {
            bits = value;
        } else {
            format = value;
        }
    }
    Ok(Some(("tiff", bits, format == 3)))
}

/// Refuse a map stored with fewer than 16 bits per sample, or other than the
/// manifest says: PNG maps hold 16-bit samples and the depth TIFF 32-bit
/// floating point.
fn check_precision(path: &Path, file: &str, description: &Value) -> Result<(), ToolError> {
    let found = stored_precision(path)?;
    let expected = match description["format"].as_str() {
        Some("png") => Some(("png", 16, false)),
        Some("tiff") => Some(("tiff", 32, true)),
        _ => None,
    };
    if let Some((_, bits, _)) = found
        && bits < 16
    {
        return Err(reconstruction_error(
            "failed",
            format!(
                "the analysis wrote {file} with {bits}-bit samples; every map needs 16 or more"
            ),
        ));
    }
    if found.is_none()
        || found != expected
        || description["bit_depth"].as_u64() != expected.map(|(_, bits, _)| u64::from(bits))
    {
        return Err(reconstruction_error(
            "failed",
            format!("the analysis wrote {file} in another format than its manifest describes"),
        ));
    }
    Ok(())
}

impl ReconWorker {
    /// The GPU budget, absent when this worker may not use a GPU, and the
    /// RAM budget the analysis plans with.
    fn analysis_budgets(&self, config: &GeometryConfig) -> (Option<u64>, Option<u64>) {
        (
            (self.config.device != "cpu").then_some(config.gpu_budget_mib),
            config.ram_budget_mib,
        )
    }

    /// The cache key: SHA-256 over the photograph's digest, the settings,
    /// the budgets that choose routes, and the digests of the registry and
    /// the analysis code beside the analyzer.
    fn analysis_key(
        &self,
        config: &GeometryConfig,
        image_sha256: &str,
        settings: &Value,
    ) -> Result<String, ToolError> {
        use sha2::{Digest, Sha256};
        let directory = config.analyze_script.parent().unwrap_or(Path::new("/"));
        let code = |path: &Path| match file_digest(path) {
            Ok(digest) => Ok(json!(digest)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
            Err(error) => Err(ToolError::from(error)),
        };
        let (gpu_budget_mib, ram_budget_mib) = self.analysis_budgets(config);
        let identity = json!({
            "format": CACHE_FORMAT,
            "image_sha256": image_sha256,
            "settings": settings,
            "gpu_budget_mib": gpu_budget_mib,
            "ram_budget_mib": ram_budget_mib,
            "registry": code(&directory.join("models/geometry.json"))?,
            "analyzer": code(&config.analyze_script)?,
            "photo_analysis": code(&directory.join("printable_recon/photo_analysis.py"))?,
            "geometry_models": code(&directory.join("printable_recon/geometry_models.py"))?,
        });
        Ok(hex(&Sha256::digest(serde_json::to_vec(&identity)?)))
    }

    /// SHA-256 of a cached artifact, read through the workspace.
    fn artifact_digest(&self, path: &str) -> Result<String, printable_workspace::WsError> {
        use sha2::{Digest, Sha256};
        let (_, digest) = self.workspace.with_artifact_reader(path, |reader| {
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; 1 << 16];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    return Ok(hex(&digest.finalize()));
                }
                digest.update(&buffer[..read]);
            }
        })?;
        Ok(digest)
    }

    /// The cached analysis for `key`, when its manifest exists and every map
    /// still matches it.
    fn cached_analysis(&self, key: &str) -> Result<Option<Value>, ToolError> {
        let directory = format!("{CACHE_ROOT}/{key}");
        let (meta, bytes) = match self
            .workspace
            .read_generated_bytes(&format!("{directory}/manifest.json"))
        {
            Ok(found) => found,
            Err(printable_workspace::WsError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Ok(mut manifest) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(None);
        };
        if manifest["cache"]["key"] != key {
            return Ok(None);
        }
        for (name, file) in MAP_FILES {
            let path = format!("{directory}/{file}");
            let description = &manifest["artifacts"][name];
            if description["artifact"]["path"] != path.as_str() {
                return Ok(None);
            }
            match self.artifact_digest(&path) {
                Ok(digest) if description["sha256"] == digest.as_str() => {}
                Ok(_) | Err(printable_workspace::WsError::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        manifest["cache"]["hit"] = json!(true);
        manifest["manifest"] = json!(meta);
        Ok(Some(manifest))
    }

    /// Return the cached analysis of a photograph at once, or retain the
    /// request as a background job that makes and caches it.
    pub fn submit_analysis(self: &Arc<Self>, request: AnalyzeRequest) -> Result<Value, ToolError> {
        request.validate()?;
        let config = self.geometry_config()?;
        let snapshot = self
            .workspace
            .snapshot_artifact_bounded(&request.image, MAX_IMAGE_BYTES)?;
        let image_sha256 = file_digest(snapshot.path())?;
        let settings = normalized(&request);
        let key = self.analysis_key(config, &image_sha256, &settings)?;
        if let Some(cached) = self.cached_analysis(&key)? {
            return Ok(cached);
        }
        self.ensure_queue_space()?;
        let (gpu_budget_mib, ram_budget_mib) = self.analysis_budgets(config);
        let execution = json!({
            "cache_key": key,
            "settings": settings,
            "gpu_budget_mib": gpu_budget_mib,
            "ram_budget_mib": ram_budget_mib,
            "model_run_timeout_seconds": config.timeout.as_secs(),
            "preparation_timeout_seconds": config.preparation_timeout.as_secs(),
        });
        self.admit(
            JobInput::Analysis(request),
            snapshot.path(),
            image_sha256,
            execution,
        )
    }

    /// Plan, prepare, run and publish one retained analysis.
    pub(super) async fn analyze_controlled(
        &self,
        request: AnalyzeRequest,
        control: &JobControl,
    ) -> Result<Value, ToolError> {
        request.validate()?;
        let config = self.geometry_config()?;
        let snapshot = self
            .workspace
            .snapshot_artifact_bounded(&request.image, MAX_IMAGE_BYTES)?;
        control.verify_input(snapshot.path())?;
        let image_sha256 = file_digest(snapshot.path())?;
        let settings = normalized(&request);
        let key = self.analysis_key(config, &image_sha256, &settings)?;
        let (gpu_budget_mib, ram_budget_mib) = self.analysis_budgets(config);
        let budgets = json!({
            "settings": settings,
            "gpu_budget_mib": gpu_budget_mib,
            "ram_budget_mib": ram_budget_mib,
        });

        control.enter(Phase::PreparingModels)?;
        let staging = tempfile::Builder::new()
            .prefix("printable-analysis-")
            .tempdir()?;
        let mut planning = self.python_command(&config.analyze_script);
        planning.arg("--plan");
        let plan = self
            .run_command(
                planning,
                &budgets,
                staging.path(),
                PLANNING_TIMEOUT,
                "photograph analysis planning",
                Some(control),
            )
            .await?;
        let entries: Vec<String> = serde_json::from_value(plan["entries"].clone())
            .map_err(|_| protocol("the analysis plan names no models"))?;
        let runs = plan["runs"].as_array().map_or(1, Vec::len).max(1);
        // Preparation uses no GPU, so it runs before the job takes its turn.
        let prepared = self
            .prepare_geometry_controlled(&entries, Some(control))
            .await?;
        let _permit = self.job_permit(Some(control)).await?;
        // An identical job may have finished while this one waited.
        if let Some(cached) = self.cached_analysis(&key)? {
            return Ok(cached);
        }
        control.enter(Phase::Generating)?;
        let output = staging.path().join("maps");
        std::fs::create_dir(&output)?;
        let mut job = budgets;
        job["image"] = json!(snapshot.path());
        job["threads"] = json!(self.config.cpu_threads);
        job["prepared"] = prepared["entries"].clone();
        job["output"] = json!(output);
        let answer = self
            .run_command(
                self.python_command(&config.analyze_script),
                &job,
                staging.path(),
                config
                    .timeout
                    .saturating_mul(u32::try_from(runs).unwrap_or(u32::MAX)),
                "photograph analysis",
                Some(control),
            )
            .await?;
        control.enter(Phase::Publishing)?;
        self.publish_analysis(&key, &image_sha256, &output, answer)
    }

    /// Commit each map to the cache directory, checking its stored precision,
    /// then the manifest last.
    fn publish_analysis(
        &self,
        key: &str,
        image_sha256: &str,
        output: &Path,
        answer: Value,
    ) -> Result<Value, ToolError> {
        let mut manifest = answer
            .get("manifest")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| protocol("the analysis answer has no manifest"))?;
        let directory = format!("{CACHE_ROOT}/{key}");
        for (name, file) in MAP_FILES {
            let description = manifest["artifacts"]
                .get_mut(name)
                .filter(|value| value.is_object() && value["file"] == file)
                .ok_or_else(|| protocol("the analysis manifest does not list every map"))?;
            let source = output.join(file);
            check_precision(&source, file, description)?;
            let digest = file_digest(&source)?;
            let artifact = self.workspace.commit_reserved_generated_artifact_bounded(
                &format!("{directory}/{file}"),
                &source,
                true,
                MAX_ANALYSIS_MAP_BYTES,
            )?;
            description["sha256"] = json!(digest);
            description["artifact"] = json!(artifact);
        }
        manifest["image_sha256"] = json!(image_sha256);
        manifest["cache"] = json!({"key": key, "directory": directory});
        let meta = self.workspace.write_reserved_artifact(
            &format!("{directory}/manifest.json"),
            &serde_json::to_vec(&manifest)?,
            true,
        )?;
        manifest["cache"]["hit"] = json!(false);
        manifest["manifest"] = json!(meta);
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in analyzer. It plans one model, and a run writes maps whose
    /// stored formats follow the photograph's bytes. Every call is logged.
    const ANALYZER: &str = r#"
import json, os, struct, sys, zlib
from pathlib import Path
here = Path(__file__).resolve().parent
request = json.load(open(sys.argv[-1]))
with open(here / 'calls.log', 'a') as log:
    log.write(('plan' if sys.argv[1] == '--plan' else 'run') + '\n')
plan = {'entries': ['small'], 'runs': [{'entry': 'small', 'roles': ['depth.shallow', 'mask', 'focal']}],
        'depth_role': 'depth.shallow', 'routes': {}}
if sys.argv[1] == '--plan':
    print(json.dumps({'result': plan}))
    sys.exit(0)
assert request['prepared']['small']['snapshot']
image = Path(request['image']).read_bytes()
out = Path(request['output'])
def png(bits, channels, path):
    colour = {1: 0, 3: 2}[channels]
    rows = b''.join(b'\0' + bytes(2 * channels * bits // 8) for _ in range(2))
    chunk = lambda kind, data: struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 2, 2, bits, colour, 0, 0, 0))
                     + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))
def tiff(path):
    entries = [(256, 3, 1, 2), (257, 3, 1, 2), (258, 3, 1, 32), (259, 3, 1, 1), (262, 3, 1, 1),
               (273, 4, 1, 8 + 2 + 12 * 10 + 4), (277, 3, 1, 1), (278, 3, 1, 2), (279, 4, 1, 16), (339, 3, 1, 3)]
    ifd = struct.pack('<H', len(entries)) + b''.join(struct.pack('<HHII', *entry) for entry in entries) + bytes(4)
    path.write_bytes(b'II*\0' + struct.pack('<I', 8) + ifd + struct.pack('<4f', 1, 1, 1, 1))
tiff(out / 'depth.tiff')
png(16, 3, out / 'normals.png')
png(8 if image == b'eight bit' else 16, 1, out / 'mask.png')
png(16, 1, out / 'confidence.png')
described = lambda file, fmt, bits, channels: {'file': file, 'format': fmt, 'bit_depth': bits, 'channels': channels}
print(json.dumps({'result': {'manifest': {'width': 2, 'height': 2, 'settings': request['settings'],
    'focal_length': {'source': 'estimated', 'model': 'small', 'px': 2.0, 'photograph_px': 4.0},
    'depth': {'role': 'depth.shallow', 'model': 'small', 'kind': 'relative'},
    'scores': {'mask_fraction': 1.0, 'mean_confidence': 1.0}, 'warnings': [],
    'artifacts': {'depth': described('depth.tiff', 'tiff', 32, 1), 'normals': described('normals.png', 'png', 16, 3),
                  'mask': described('mask.png', 'png', 16, 1), 'confidence': described('confidence.png', 'png', 16, 1)}}}}))
"#;

    fn check_output(value: &Value) {
        let schema = Value::Object(crate::tools::output::schema("photo").as_ref().clone());
        let validator = jsonschema::validator_for(&schema).unwrap();
        if let Err(error) = validator.validate(value) {
            panic!("{error}: {value}");
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<ReconWorker>) {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        super::super::geometry::tests::configured(root.path(), &mut worker);
        std::fs::write(
            &worker.config.geometry.as_ref().unwrap().analyze_script,
            ANALYZER,
        )
        .unwrap();
        (root, Arc::new(worker))
    }

    fn request(image: &str) -> AnalyzeRequest {
        AnalyzeRequest {
            image: image.into(),
            subject: Subject::Shallow,
            quality: None,
            interpretation: None,
            normals: false,
            focal_length_px: None,
        }
    }

    /// How many times the analyzer ran a model and how many times it was
    /// called at all.
    fn calls(root: &Path) -> (usize, usize) {
        let log = std::fs::read_to_string(root.join("runtime/calls.log")).unwrap_or_default();
        (
            log.lines().filter(|line| *line == "run").count(),
            log.lines().count(),
        )
    }

    async fn finished(worker: &ReconWorker, accepted: &Value) -> Value {
        let id = accepted["job_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no job: {accepted}"));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = worker
                    .photo_job_status(PhotoJobRequest {
                        job_id: id.into(),
                        detail: true,
                    })
                    .unwrap();
                check_output(&state);
                if !matches!(state["state"].as_str(), Some("queued" | "running")) {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the analysis job finished")
    }

    #[tokio::test]
    async fn a_repeated_request_returns_the_cached_analysis_without_running_a_model() {
        let (root, worker) = fixture();
        let accepted = worker.submit_analysis(request("face.jpg")).unwrap();
        check_output(&accepted);
        assert_eq!(accepted["action"], "analyze");
        let done = finished(&worker, &accepted).await;
        assert_eq!(done["state"], "succeeded", "{done}");
        let result = &done["result"];
        assert_eq!(result["cache"]["hit"], false);
        let key = result["cache"]["key"].as_str().unwrap();
        assert_eq!(done["cache_key"], key);
        assert_eq!(
            result["settings"],
            json!({"subject": "shallow", "quality": "best", "interpretation": "sculpture",
                "normals": false, "focal_length_px": null})
        );
        for (name, file) in MAP_FILES {
            let path = format!("{CACHE_ROOT}/{key}/{file}");
            assert_eq!(result["artifacts"][name]["artifact"]["path"], path.as_str());
            assert_eq!(
                result["artifacts"][name]["sha256"],
                worker.artifact_digest(&path).unwrap().as_str()
            );
        }
        assert_eq!(calls(root.path()), (1, 2));

        // The same photograph and settings are served from the cache without
        // calling the analyzer or preparing a model.
        let config = worker.config.geometry.as_ref().unwrap();
        std::fs::write(&config.prepare_script, "import sys; sys.exit(3)").unwrap();
        let cached = worker.submit_analysis(request("face.jpg")).unwrap();
        check_output(&cached);
        assert!(cached.get("job_id").is_none());
        assert_eq!(cached["cache"]["hit"], true);
        assert_eq!(cached["cache"]["key"], key);
        assert_eq!(cached["artifacts"], result["artifacts"]);
        assert_eq!(calls(root.path()), (1, 2));
    }

    #[tokio::test]
    async fn the_key_covers_the_photograph_settings_budgets_and_registry() {
        let (root, mut worker) = fixture();
        let worker = Arc::get_mut(&mut worker).unwrap();
        let config = worker.config.geometry.clone().unwrap();
        let settings = normalized(&request("face.jpg"));
        let key = |worker: &ReconWorker, config: &GeometryConfig, image: &str, settings: &Value| {
            worker.analysis_key(config, image, settings).unwrap()
        };
        let base = key(worker, &config, "a", &settings);
        assert_eq!(base.len(), 64);
        assert_eq!(base, key(worker, &config, "a", &settings));
        // Omitted defaults and their explicit values are the same analysis.
        let mut explicit = request("face.jpg");
        explicit.quality = Some(AnalysisQuality::Best);
        explicit.interpretation = Some(Interpretation::Sculpture);
        assert_eq!(base, key(worker, &config, "a", &normalized(&explicit)));
        assert_ne!(base, key(worker, &config, "b", &settings));
        for change in [
            json!({"quality": "fast"}),
            json!({"interpretation": "physical"}),
            json!({"normals": true}),
            json!({"focal_length_px": 900.0}),
        ] {
            let mut other = settings.clone();
            for (name, value) in change.as_object().unwrap() {
                other[name] = value.clone();
            }
            assert_ne!(base, key(worker, &config, "a", &other), "{change}");
        }
        let mut budget = config.clone();
        budget.ram_budget_mib = Some(4096);
        assert_ne!(base, key(worker, &budget, "a", &settings));
        worker.config.device = "cuda".into();
        assert_ne!(base, key(worker, &config, "a", &settings));
        worker.config.device = "cpu".into();
        let registry = root.path().join("runtime/models/geometry.json");
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(&registry, b"{}").unwrap();
        assert_ne!(base, key(worker, &config, "a", &settings));
    }

    #[tokio::test]
    async fn an_eight_bit_map_fails_the_job_and_caches_nothing() {
        let (_root, worker) = fixture();
        worker
            .workspace
            .write_artifact("eight.png", b"eight bit", false)
            .unwrap();
        let done = finished(
            &worker,
            &worker.submit_analysis(request("eight.png")).unwrap(),
        )
        .await;
        assert_eq!(done["state"], "failed");
        assert_eq!(done["failure"]["code"], "failed");
        assert!(
            done["failure"]["message"]
                .as_str()
                .unwrap()
                .contains("8-bit")
        );
        let key = done["cache_key"].as_str().unwrap();
        assert!(worker.cached_analysis(key).unwrap().is_none());
        assert!(
            worker
                .workspace
                .stat_artifact(&format!("{CACHE_ROOT}/{key}/manifest.json"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_changed_map_is_not_a_cache_hit() {
        let (root, worker) = fixture();
        let done = finished(
            &worker,
            &worker.submit_analysis(request("face.jpg")).unwrap(),
        )
        .await;
        let key = done["result"]["cache"]["key"].as_str().unwrap();
        std::fs::write(
            root.path()
                .join(format!("workspace/{CACHE_ROOT}/{key}/mask.png")),
            b"changed",
        )
        .unwrap();
        let again = worker.submit_analysis(request("face.jpg")).unwrap();
        assert!(again["job_id"].is_string(), "{again}");
        assert_eq!(finished(&worker, &again).await["state"], "succeeded");
        assert_eq!(calls(root.path()).0, 2);
        assert_eq!(
            worker.submit_analysis(request("face.jpg")).unwrap()["cache"]["hit"],
            true
        );
    }

    #[tokio::test]
    async fn analysis_needs_configured_geometry_models_and_valid_settings() {
        let (_root, worker) = super::super::tests::worker(Duration::from_secs(60));
        let worker = Arc::new(worker);
        assert_eq!(
            worker
                .submit_analysis(request("face.jpg"))
                .unwrap_err()
                .code(),
            "unavailable"
        );
        let mut deep = request("face.jpg");
        deep.subject = Subject::Deep;
        deep.interpretation = Some(Interpretation::Physical);
        assert_eq!(
            worker.submit_analysis(deep).unwrap_err().code(),
            "validation"
        );
        let mut focal = request("face.jpg");
        focal.focal_length_px = Some(5.0);
        assert_eq!(
            worker.submit_analysis(focal).unwrap_err().code(),
            "validation"
        );
        assert_eq!(
            worker
                .submit_analysis(request("face.gif"))
                .unwrap_err()
                .code(),
            "validation"
        );
    }

    #[test]
    fn stored_precision_reads_png_and_tiff_headers() {
        let root = tempfile::tempdir().unwrap();
        let png = root.path().join("map.png");
        let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x02\0\0\0\x02".to_vec();
        header.extend([16, 0, 0, 0, 0]);
        std::fs::write(&png, &header).unwrap();
        assert_eq!(stored_precision(&png).unwrap(), Some(("png", 16, false)));
        header[24] = 8;
        std::fs::write(&png, &header).unwrap();
        let error = check_precision(&png, "map.png", &json!({"format": "png", "bit_depth": 16}))
            .unwrap_err();
        assert!(worker_message(&error).contains("8-bit"));
        std::fs::write(&png, b"not an image at all, but long enough").unwrap();
        assert_eq!(stored_precision(&png).unwrap(), None);
    }
}
