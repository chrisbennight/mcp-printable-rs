//! The `photo` tool's `compare` action: measure maps and meshes against each
//! other, a photograph's cached analysis, or a synthetic scene with exact
//! depth and normals.
//!
//! Meshes are rendered first on the isolated render worker, which casts one
//! ray per pixel for exact depth, normals and coverage. The photograph worker
//! then measures everything with the depth benchmark's scoring code and
//! writes the report, the sheet and any difference maps.

use super::*;
use crate::recon::{CompareInput, CompareJob, CompareMode, CompareRequest, MESH_SUFFIXES};

/// Where rendered views wait for the photograph worker.
const WORK_ROOT: &str = ".printable/photo-compare";
/// A synthetic scene's view: the benchmark's 4:3 frame at its default side.
const SCENE_SIZE: (u32, u32) = (1024, 768);
/// A mesh compared only with other inputs, when no map sets the frame.
const MESH_SIZE: (u32, u32) = (1024, 1024);
/// Largest map whose size is read to frame meshes.
const MAX_MAP_BYTES: u64 = 128 * 1024 * 1024;

fn is_mesh(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    MESH_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// The width and height a PNG or TIFF file declares.
fn map_size(path: &std::path::Path) -> Option<(u32, u32)> {
    if let Ok(size) = image::image_dimensions(path) {
        return Some(size);
    }
    let file = std::fs::File::open(path).ok()?;
    tiff::decoder::Decoder::new(std::io::BufReader::new(file))
        .ok()?
        .dimensions()
        .ok()
}

pub(super) async fn compare(
    workspace: Arc<Workspace>,
    settings: &Settings,
    jobs: Option<&Arc<JobRegistry>>,
    request: CompareRequest,
) -> Result<Value, ToolError> {
    request.validate()?;
    let client = settings
        .recon
        .as_ref()
        .ok_or_else(|| ToolError::Validation("the photograph worker is not configured".into()))?;
    let companions = request.companion_paths();
    validate_scad_destination(&workspace, &request.path, ".json", request.overwrite)?;
    validate_scad_destination(&workspace, &companions.sheet, ".png", request.overwrite)?;
    for path in &companions.differences {
        validate_scad_destination(&workspace, path, ".tiff", request.overwrite)?;
    }
    if let Some(path) = &companions.scene_photograph {
        validate_scad_destination(&workspace, path, ".png", request.overwrite)?;
    }
    for path in request
        .inputs
        .iter()
        .chain(&request.image)
        .chain(&request.scene)
    {
        workspace.stat_artifact(path)?;
    }
    // Meshes are rendered on the grid they are compared on.
    let size = {
        let (workspace, mode) = (Arc::clone(&workspace), request.mode);
        let (analysis, first_map) = (
            request.analysis.clone().unwrap_or_default(),
            request.inputs.iter().find(|input| !is_mesh(input)).cloned(),
        );
        blocking(move || -> Result<(u32, u32), ToolError> {
            match mode {
                CompareMode::Photo => {
                    let (_, manifest) = relief::analysis_manifest(&workspace, &analysis)?;
                    let (width, height) = relief::analysis_size(&manifest)?;
                    let side =
                        |value: usize| u32::try_from(value).map_err(|_| relief::changed_analysis());
                    Ok((side(width)?, side(height)?))
                }
                CompareMode::Synthetic => Ok(SCENE_SIZE),
                CompareMode::Outputs => match first_map {
                    Some(map) => {
                        let snapshot = workspace.snapshot_artifact_bounded(&map, MAX_MAP_BYTES)?;
                        map_size(snapshot.path()).ok_or_else(|| {
                            ToolError::Validation(format!(
                                "{map} could not be read as a PNG or TIFF map"
                            ))
                        })
                    }
                    None => Ok(MESH_SIZE),
                },
            }
        })
        .await?
    };

    let meshes: Vec<&String> = request
        .inputs
        .iter()
        .filter(|input| is_mesh(input))
        .chain(&request.scene)
        .collect();
    let work = format!("{WORK_ROOT}/{}", crate::upload::random_hex_id()?);
    let mut staged = Vec::new();
    let outcome = async {
        let mut renders = serde_json::Map::new();
        let mut views = std::collections::HashMap::new();
        for (index, mesh) in meshes.iter().enumerate() {
            let registry = job_registry(jobs)?;
            let prefix = format!("{work}/{index}");
            let scene = request.scene.as_ref() == Some(*mesh);
            let photograph = scene && companions.scene_photograph.is_some();
            staged.push(format!("{prefix}.zip"));
            if photograph {
                staged.push(format!("{prefix}.png"));
            }
            let projection = if scene { "perspective" } else { "orthographic" };
            let rendered = registry
                .render_geometry(mesh, &prefix, size, projection, photograph)
                .await?;
            renders.insert((*mesh).clone(), rendered);
            views.insert((*mesh).clone(), format!("{prefix}.zip"));
        }
        let job = CompareJob {
            mode: request.mode,
            kind: request.kind.unwrap_or_default(),
            inputs: request
                .inputs
                .iter()
                .map(|input| CompareInput {
                    name: input.clone(),
                    file: views.get(input).cloned().unwrap_or_else(|| input.clone()),
                })
                .collect(),
            analysis: request.analysis.clone(),
            image: request.image.clone(),
            truth: request
                .scene
                .as_ref()
                .and_then(|scene| views.get(scene).cloned()),
            report: request.path.clone(),
            sheet: companions.sheet.clone(),
            differences: companions.differences.clone(),
            overwrite: request.overwrite,
            renders: if renders.is_empty() {
                Value::Null
            } else {
                Value::Object(renders.clone())
            },
        };
        let mut report = client.compare(&job).await?;
        if let (Some(path), Some(scene)) = (&companions.scene_photograph, &request.scene) {
            let rendered = &renders[scene.as_str()]["photograph"];
            let source = format!("{}.png", views[scene.as_str()].trim_end_matches(".zip"));
            let bytes = verify_product_png_artifact(
                Arc::clone(&workspace),
                source,
                rendered,
                u64::from(size.0),
                u64::from(size.1),
            )
            .await?;
            let artifact = workspace.write_generated_bytes(path, &bytes, request.overwrite)?;
            report["artifacts"]["scene_photograph"] = json!(artifact);
        }
        Ok::<_, ToolError>(report)
    }
    .await;
    for path in staged {
        if let Err(error) = workspace.remove_reserved_artifact(&path) {
            tracing::warn!(code = error.code(), "could not remove comparison render");
        }
    }
    outcome
}
