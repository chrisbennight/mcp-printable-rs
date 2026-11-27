//! The worker's half of `photo compare`: measuring maps and meshes against
//! each other, a photograph's cached analysis or a synthetic scene.
//!
//! The server renders any mesh through the render worker first and sends the
//! rendered views as reserved workspace files. The worker copies every input
//! into a private snapshot, checks analysis maps against their manifest and
//! the photograph against the analysis's SHA-256, and runs the comparison
//! script beside the analysis script. That script uses the depth benchmark's
//! scoring code, so synthetic scores are the benchmark's own numbers. The
//! sheet, any difference maps and the report are then committed.

use super::*;

/// Inputs measured in one request.
pub const MAX_COMPARE_INPUTS: usize = 6;
/// Largest input copied out of the workspace: an analysis-sized map, or a
/// rendered view of a mesh with its depth, normals and coverage.
const MAX_COMPARE_INPUT_BYTES: u64 = 128 * 1024 * 1024;
/// Largest sheet or difference map committed.
const MAX_COMPARE_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAP_SUFFIXES: &[&str] = &[".png", ".tif", ".tiff"];
pub const MESH_SUFFIXES: &[&str] = &[".stl", ".obj", ".ply", ".glb"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompareMode {
    Outputs,
    Photo,
    Synthetic,
}

// How a depth map's values relate to distance: as the depth benchmark scores them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DepthKind {
    #[default]
    Relative,
    Metric,
    Logrel,
}

// Optional fields are advertised without `null`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareRequest {
    /// outputs: inputs vs the first; photo: vs an analysis; synthetic: vs a
    /// mesh's exact depth.
    pub mode: CompareMode,
    /// Maps (16-bit PNG heightmap or normals, float TIFF depth) or meshes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// photo: a finished analysis's cache.key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub analysis: Option<String>,
    /// photo: the analysed photograph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub image: Option<String>,
    /// synthetic: a truth mesh, or a benchmark truth.zip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub scene: Option<String>,
    /// Map depth kind; default relative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "DepthKind")]
    pub kind: Option<DepthKind>,
    /// New .json report; NAME.sheet.png goes beside it.
    pub path: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// The depth benchmark's converted truth for one of its scenes.
fn is_truth(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".zip")
}

fn is_mesh(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    MESH_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

impl CompareRequest {
    /// Checks that need no file access.
    pub fn validate(&self) -> Result<(), ToolError> {
        let refuse = |message: &str| Err(ToolError::Validation(message.into()));
        if !self.path.ends_with(".json") {
            return refuse("path must end in .json");
        }
        if self.inputs.len() > MAX_COMPARE_INPUTS {
            return refuse("give at most 6 inputs");
        }
        for input in &self.inputs {
            let lower = input.to_ascii_lowercase();
            if !MAP_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) && !is_mesh(input) {
                return refuse(
                    "each input must be a .png or .tiff map or an .stl, .obj, .ply or .glb mesh",
                );
            }
        }
        let (photo, synthetic) = (
            self.analysis.is_some() || self.image.is_some(),
            self.scene.is_some(),
        );
        match self.mode {
            CompareMode::Outputs if self.inputs.len() < 2 => {
                refuse("outputs compares two or more inputs")
            }
            CompareMode::Outputs if photo || synthetic => {
                refuse("outputs takes only inputs; analysis, image and scene belong to other modes")
            }
            CompareMode::Photo if self.inputs.is_empty() => {
                refuse("photo needs at least one input")
            }
            CompareMode::Photo if self.analysis.is_none() || self.image.is_none() => refuse(
                "photo needs analysis, the cache.key of an analysis, and image, its photograph",
            ),
            CompareMode::Photo if synthetic => refuse("scene belongs to the synthetic mode"),
            CompareMode::Photo
                if self
                    .analysis
                    .as_deref()
                    .is_some_and(|key| !crate::tools::relief::is_cache_key(key)) =>
            {
                refuse("analysis must be the cache.key of an analysis")
            }
            CompareMode::Synthetic if !synthetic => refuse("synthetic needs scene, a mesh"),
            CompareMode::Synthetic if photo => {
                refuse("analysis and image belong to the photo mode")
            }
            CompareMode::Synthetic
                if !self
                    .scene
                    .as_deref()
                    .is_some_and(|scene| is_mesh(scene) || is_truth(scene)) =>
            {
                refuse("scene must be an .stl, .obj, .ply or .glb mesh or a benchmark truth .zip")
            }
            CompareMode::Synthetic
                if self.inputs.is_empty() && self.scene.as_deref().is_some_and(is_truth) =>
            {
                refuse("a benchmark truth has its own photograph; give the inputs to score")
            }
            CompareMode::Synthetic
                if self.scene.as_deref().is_some_and(is_truth)
                    && self.inputs.iter().any(|input| is_mesh(input)) =>
            {
                refuse("a benchmark truth has no camera to render a mesh with; give maps")
            }
            _ => Ok(()),
        }
    }

    /// The files written beside the report: the sheet, then a difference map
    /// for each input after the first (outputs), or the scene's photograph
    /// when a synthetic request has no inputs.
    pub fn companion_paths(&self) -> CompanionPaths {
        let stem = &self.path[..self.path.len() - ".json".len()];
        CompanionPaths {
            sheet: format!("{stem}.sheet.png"),
            differences: match self.mode {
                CompareMode::Outputs => (1..self.inputs.len())
                    .map(|index| format!("{stem}.diff-{index}.tiff"))
                    .collect(),
                _ => Vec::new(),
            },
            scene_photograph: (self.mode == CompareMode::Synthetic
                && self.inputs.is_empty()
                && self.scene.as_deref().is_some_and(is_mesh))
            .then(|| format!("{stem}.scene.png")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionPaths {
    pub sheet: String,
    pub differences: Vec<String>,
    pub scene_photograph: Option<String>,
}

/// One input as the worker reads it: the path the caller named, and the
/// workspace file holding it, which for a mesh is its rendered view.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompareInput {
    pub name: String,
    pub file: String,
}

/// What the server asks the worker to measure, after rendering any mesh.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompareJob {
    pub mode: CompareMode,
    pub kind: DepthKind,
    pub inputs: Vec<CompareInput>,
    pub analysis: Option<String>,
    pub image: Option<String>,
    /// The scene's rendered view, for synthetic.
    pub truth: Option<String>,
    pub report: String,
    pub sheet: String,
    pub differences: Vec<String>,
    pub overwrite: bool,
    /// How each mesh was rendered, recorded in the report.
    pub renders: Value,
}

impl ReconWorker {
    pub async fn compare(&self, job: CompareJob) -> Result<Value, ToolError> {
        let geometry = self.config.geometry.as_ref().ok_or_else(|| {
            reconstruction_error(
                "unavailable",
                "the photograph geometry models are not configured",
            )
        })?;
        let script = geometry.analyze_script.with_file_name("compare_photo.py");
        for path in std::iter::once(&job.report)
            .chain(std::iter::once(&job.sheet))
            .chain(&job.differences)
        {
            self.check_destination(path, job.overwrite)?;
        }
        // Comparisons take their turn with the other photograph work.
        let _permit = self.permit()?;
        let snapshot = |path: &str| {
            self.workspace
                .snapshot_artifact_bounded(path, MAX_COMPARE_INPUT_BYTES)
                .map_err(ToolError::from)
        };
        let mut snapshots = Vec::new();
        let mut inputs = Vec::new();
        for input in &job.inputs {
            let copy = snapshot(&input.file)?;
            inputs.push(json!({"name": input.name, "file": copy.path()}));
            snapshots.push(copy);
        }
        let mut request = json!({
            "mode": job.mode,
            "kind": job.kind,
            "inputs": inputs,
        });
        let mut analysis_report = Value::Null;
        if let (Some(key), Some(image)) = (&job.analysis, &job.image) {
            let workspace = &self.workspace;
            let (directory, manifest) = crate::tools::relief::analysis_manifest(workspace, key)?;
            let mut maps = serde_json::Map::new();
            for name in ["depth", "normals", "mask"] {
                let copy =
                    crate::tools::relief::snapshot_map(workspace, &manifest, &directory, name)?;
                maps.insert(name.into(), json!(copy.path()));
                snapshots.push(copy);
            }
            let photograph = workspace.snapshot_artifact_bounded(image, MAX_IMAGE_BYTES)?;
            if manifest["image_sha256"].as_str() != Some(file_digest(photograph.path())?.as_str()) {
                return Err(ToolError::Validation(
                    "image is not the photograph this analysis was made from".into(),
                ));
            }
            request["analysis"] = Value::Object(maps);
            request["photograph"] = json!(photograph.path());
            snapshots.push(photograph);
            analysis_report = json!({
                "cache_key": key,
                "image_sha256": manifest["image_sha256"],
                "width": manifest["width"],
                "height": manifest["height"],
                "settings": manifest["settings"],
                "depth": manifest["depth"],
                "focal_length": manifest["focal_length"],
                "scores": manifest["scores"],
                "warnings": manifest["warnings"],
            });
        }
        if let Some(truth) = &job.truth {
            let copy = snapshot(truth)?;
            request["truth"] = json!(copy.path());
            snapshots.push(copy);
        }
        let staging = tempfile::Builder::new()
            .prefix("printable-compare-")
            .tempdir()?;
        let output = staging.path().join("output");
        std::fs::create_dir(&output)?;
        request["output"] = json!(output);
        let result = self
            .run(
                &script,
                &request,
                staging.path(),
                geometry.timeout,
                "comparison",
            )
            .await?;
        drop(snapshots);
        let files = &result["files"];
        let file = |key: &str| {
            files[key]
                .as_str()
                .filter(|name| !name.contains('/') && !name.starts_with('.'))
                .map(|name| output.join(name))
                .ok_or_else(|| {
                    reconstruction_error("protocol", "the comparison did not report its files")
                })
        };
        // Every output is checked before any is published.
        let sheet_file = file("sheet")?;
        let difference_files = (1..=job.differences.len())
            .map(|index| file(&format!("difference_{index}")))
            .collect::<Result<Vec<_>, _>>()?;
        for path in std::iter::once(&sheet_file).chain(&difference_files) {
            if std::fs::metadata(path)?.len() > MAX_COMPARE_OUTPUT_BYTES {
                return Err(reconstruction_error(
                    "resource_limit",
                    "a comparison output is larger than the worker publishes",
                ));
            }
        }
        let mut artifacts = serde_json::Map::new();
        let sheet = self.workspace.commit_generated_artifact_bounded(
            &job.sheet,
            &sheet_file,
            job.overwrite,
            MAX_COMPARE_OUTPUT_BYTES,
        )?;
        artifacts.insert("sheet".into(), json!(sheet));
        let mut differences = Vec::new();
        for (path, source) in job.differences.iter().zip(&difference_files) {
            differences.push(self.workspace.commit_generated_artifact_bounded(
                path,
                source,
                job.overwrite,
                MAX_COMPARE_OUTPUT_BYTES,
            )?);
        }
        if !differences.is_empty() {
            artifacts.insert("differences".into(), json!(differences));
        }
        let mut report = json!({
            "mode": job.mode,
            "kind": job.kind,
            "inputs": job.inputs.iter().map(|input| &input.name).collect::<Vec<_>>(),
            "results": result["results"],
        });
        if !analysis_report.is_null() {
            report["analysis"] = analysis_report;
        }
        if !job.renders.is_null() {
            report["renders"] = job.renders.clone();
        }
        let saved = self.workspace.write_generated_bytes(
            &job.report,
            &serde_json::to_vec_pretty(&report)?,
            job.overwrite,
        )?;
        artifacts.insert("report".into(), json!(saved));
        report["artifacts"] = Value::Object(artifacts);
        Ok(report)
    }
}

impl ReconClient {
    pub async fn compare(&self, job: &CompareJob) -> Result<Value, ToolError> {
        self.post(
            "photo/compare",
            job,
            "inspect the report path before retrying, because the files may have been written",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> CompareRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn each_mode_needs_its_own_fields() {
        for (value, message) in [
            (
                json!({"mode": "outputs", "inputs": ["a.png"], "path": "r.json"}),
                "two or more",
            ),
            (
                json!({"mode": "outputs", "inputs": ["a.png", "b.stl"], "image": "p.jpg", "path": "r.json"}),
                "only inputs",
            ),
            (
                json!({"mode": "photo", "inputs": ["a.png"], "image": "p.jpg", "path": "r.json"}),
                "analysis",
            ),
            (
                json!({"mode": "photo", "inputs": ["a.png"], "image": "p.jpg", "analysis": "ABC", "path": "r.json"}),
                "cache.key",
            ),
            (
                json!({"mode": "photo", "image": "p.jpg", "analysis": "a", "path": "r.json"}),
                "at least one",
            ),
            (json!({"mode": "synthetic", "path": "r.json"}), "scene"),
            (
                json!({"mode": "synthetic", "scene": "s.png", "path": "r.json"}),
                "mesh",
            ),
            (
                json!({"mode": "synthetic", "scene": "truth.zip", "path": "r.json"}),
                "inputs to score",
            ),
            (
                json!({"mode": "synthetic", "scene": "truth.zip", "inputs": ["m.stl"], "path": "r.json"}),
                "give maps",
            ),
            (
                json!({"mode": "synthetic", "scene": "s.stl", "image": "p.jpg", "path": "r.json"}),
                "photo mode",
            ),
            (
                json!({"mode": "synthetic", "scene": "s.stl", "path": "r.stl"}),
                ".json",
            ),
            (
                json!({"mode": "synthetic", "scene": "s.stl", "inputs": ["a.jpg"], "path": "r.json"}),
                "each input",
            ),
            (
                json!({"mode": "outputs", "inputs": ["1.png", "2.png", "3.png", "4.png", "5.png", "6.png", "7.png"], "path": "r.json"}),
                "at most 6",
            ),
        ] {
            let error = request(value.clone()).validate().unwrap_err().to_string();
            assert!(error.contains(message), "{value}: {error}");
        }
        for value in [
            json!({"mode": "outputs", "inputs": ["a.png", "b.STL"], "path": "r.json"}),
            json!({"mode": "photo", "inputs": ["h.png"], "image": "p.jpg", "analysis": "a".repeat(64), "path": "r.json"}),
            json!({"mode": "synthetic", "scene": "s.stl", "path": "r.json"}),
            json!({"mode": "synthetic", "scene": "s.glb", "inputs": ["d.tiff"], "kind": "metric", "path": "r.json"}),
            json!({"mode": "synthetic", "scene": "gt/relief/truth.zip", "inputs": ["d.tiff"], "path": "r.json"}),
        ] {
            request(value.clone())
                .validate()
                .unwrap_or_else(|error| panic!("{value}: {error}"));
        }
        assert!(
            serde_json::from_value::<CompareRequest>(
                json!({"mode": "photo", "path": "r.json", "extra": 1})
            )
            .is_err()
        );
    }

    /// A stand-in comparison script: it checks it was given snapshot copies,
    /// writes a sheet and one difference map per later input, and echoes what
    /// it read.
    const COMPARER: &str = r#"
import json, sys
from pathlib import Path
request = json.load(open(sys.argv[-1]))
out = Path(request['output'])
files = {'sheet': 'sheet.png'}
(out / 'sheet.png').write_bytes(b'sheet')
if request['mode'] == 'outputs':
    for index in range(1, len(request['inputs'])):
        (out / f'diff-{index}.tiff').write_bytes(b'difference')
        files[f'difference_{index}'] = f'diff-{index}.tiff'
results = []
for item in request['inputs']:
    assert Path(item['file']).is_absolute() and item['file'] != item['name'], item
    assert Path(item['file']).read_text() == item['name']
if request['mode'] == 'outputs':
    first = request['inputs'][0]['name']
    for item in request['inputs'][1:]:
        stats = {'pixels': 4, 'rmse': 0.25, 'rmse_percent_of_range': 5.0}
        results.append({'input': item['name'], 'reference': first, 'alignment': {'scale': 1.0, 'offset': 0.0},
                        'correlation': 0.9, 'whole': stats, 'regions': {'centre': stats}})
else:
    results = [{'input': item['name'], 'source': 'heightmap', 'absrel': 0.5} for item in request['inputs']]
if request['mode'] == 'photo':
    assert Path(request['photograph']).read_bytes() == b'image bytes'
    assert sorted(request['analysis']) == ['depth', 'mask', 'normals']
print(json.dumps({'result': {'results': results, 'files': files}}))
"#;

    fn fixture() -> (tempfile::TempDir, ReconWorker) {
        let (root, mut worker) = super::super::tests::worker(Duration::from_secs(60));
        super::super::geometry::tests::configured(root.path(), &mut worker);
        let script = root.path().join("runtime/compare_photo.py");
        std::fs::write(script, COMPARER).unwrap();
        for name in ["a.png", "b.tiff"] {
            worker
                .workspace
                .write_artifact(name, name.as_bytes(), false)
                .unwrap();
        }
        (root, worker)
    }

    fn job(mode: CompareMode, inputs: &[&str]) -> CompareJob {
        CompareJob {
            mode,
            kind: DepthKind::Relative,
            inputs: inputs
                .iter()
                .map(|name| CompareInput {
                    name: (*name).into(),
                    file: (*name).into(),
                })
                .collect(),
            analysis: None,
            image: None,
            truth: None,
            report: "cmp/r.json".into(),
            sheet: "cmp/r.sheet.png".into(),
            differences: (1..inputs.len())
                .map(|i| format!("cmp/r.diff-{i}.tiff"))
                .collect(),
            overwrite: false,
            renders: Value::Null,
        }
    }

    #[tokio::test]
    async fn outputs_are_committed_with_a_report() {
        let (_root, worker) = fixture();
        let report = worker
            .compare(job(CompareMode::Outputs, &["a.png", "b.tiff"]))
            .await
            .unwrap();
        let schema = Value::Object(crate::tools::output::schema("photo").as_ref().clone());
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&report), "{report}");
        // Each row is an outputs-mode row, not merely some row the report allows.
        let [_, outputs, _] = crate::tools::output::comparison_rows();
        let rows = jsonschema::validator_for(&outputs).unwrap();
        assert_eq!(report["results"].as_array().unwrap().len(), 1, "{report}");
        assert!(rows.is_valid(&report["results"][0]), "{report}");
        assert_eq!(report["results"][0]["reference"], "a.png");
        assert_eq!(
            report["artifacts"]["differences"][0]["path"],
            "cmp/r.diff-1.tiff"
        );
        let (_, saved) = worker.workspace.read_artifact("cmp/r.json").unwrap();
        let saved: Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved["results"], report["results"]);
        assert!(saved.get("artifacts").is_none());
        // A comparison waits for no other photograph work: it is busy instead.
        let held = worker.admission.try_acquire().unwrap();
        let mut other = job(CompareMode::Outputs, &["a.png", "b.tiff"]);
        other.report = "cmp/busy.json".into();
        other.sheet = "cmp/busy.sheet.png".into();
        other.differences = vec!["cmp/busy.diff-1.tiff".into()];
        assert_eq!(worker.compare(other).await.unwrap_err().code(), "busy");
        drop(held);
        // The report is never replaced unless asked.
        let error = worker
            .compare(job(CompareMode::Outputs, &["a.png", "b.tiff"]))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "already_exists", "{error}");
    }

    #[tokio::test]
    async fn photo_mode_needs_the_analysed_photograph() {
        let (_root, worker) = fixture();
        let key = "a".repeat(64);
        let directory = format!("{ANALYSIS_CACHE_ROOT}/{key}");
        let mut artifacts = serde_json::Map::new();
        for name in ["depth", "normals", "mask"] {
            let path = format!("{directory}/{name}.png");
            worker
                .workspace
                .write_reserved_artifact(&path, name.as_bytes(), false)
                .unwrap();
            let digest = {
                use sha2::{Digest, Sha256};
                hex(&Sha256::digest(name.as_bytes()))
            };
            artifacts.insert(
                name.into(),
                json!({"artifact": {"path": path}, "sha256": digest}),
            );
        }
        let photograph = {
            use sha2::{Digest, Sha256};
            hex(&Sha256::digest(b"image bytes"))
        };
        let manifest = json!({"cache": {"key": key}, "width": 4, "height": 3, "image_sha256": photograph,
            "scores": {"mean_confidence": 0.8}, "warnings": [], "artifacts": artifacts});
        worker
            .workspace
            .write_reserved_artifact(
                &format!("{directory}/manifest.json"),
                &serde_json::to_vec(&manifest).unwrap(),
                false,
            )
            .unwrap();
        let mut request = job(CompareMode::Photo, &["a.png"]);
        request.analysis = Some(key);
        request.image = Some("blank.png".into());
        let report = worker.compare(request.clone()).await.unwrap();
        assert_eq!(report["analysis"]["scores"]["mean_confidence"], 0.8);
        worker
            .workspace
            .write_artifact("other.png", b"other", false)
            .unwrap();
        request.image = Some("other.png".into());
        request.report = "cmp/other.json".into();
        request.sheet = "cmp/other.sheet.png".into();
        let error = worker.compare(request).await.unwrap_err();
        assert!(error.to_string().contains("not the photograph"), "{error}");
    }

    #[test]
    fn companion_files_sit_beside_the_report() {
        let outputs = request(
            json!({"mode": "outputs", "inputs": ["a.png", "b.png", "c.stl"], "path": "out/cmp.json"}),
        );
        assert_eq!(
            outputs.companion_paths(),
            CompanionPaths {
                sheet: "out/cmp.sheet.png".into(),
                differences: vec!["out/cmp.diff-1.tiff".into(), "out/cmp.diff-2.tiff".into()],
                scene_photograph: None,
            }
        );
        let scene = request(json!({"mode": "synthetic", "scene": "s.stl", "path": "cmp.json"}));
        assert_eq!(
            scene.companion_paths().scene_photograph.as_deref(),
            Some("cmp.scene.png")
        );
        let scored = request(
            json!({"mode": "synthetic", "scene": "s.stl", "inputs": ["d.tiff"], "path": "cmp.json"}),
        );
        assert_eq!(scored.companion_paths().scene_photograph, None);
    }
}
