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
    // Meshes are rendered on the grid they are compared on; in photo mode,
    // through the analysis's focal length when it has one.
    let (size, focal_px) = {
        let (workspace, mode) = (Arc::clone(&workspace), request.mode);
        let (analysis, first_map) = (
            request.analysis.clone().unwrap_or_default(),
            request.inputs.iter().find(|input| !is_mesh(input)).cloned(),
        );
        blocking(move || -> Result<((u32, u32), Option<f64>), ToolError> {
            match mode {
                CompareMode::Photo => {
                    let (_, manifest) = relief::analysis_manifest(&workspace, &analysis)?;
                    let (width, height) = relief::analysis_size(&manifest)?;
                    let side =
                        |value: usize| u32::try_from(value).map_err(|_| relief::changed_analysis());
                    let focal = manifest["focal_length"]["px"]
                        .as_f64()
                        .filter(|px| px.is_finite() && *px > 0.0);
                    Ok(((side(width)?, side(height)?), focal))
                }
                CompareMode::Synthetic => Ok((SCENE_SIZE, None)),
                CompareMode::Outputs => match first_map {
                    Some(map) => {
                        let snapshot = workspace.snapshot_artifact_bounded(&map, MAX_MAP_BYTES)?;
                        let size = map_size(snapshot.path()).ok_or_else(|| {
                            ToolError::Validation(format!(
                                "{map} could not be read as a PNG or TIFF map"
                            ))
                        })?;
                        Ok((size, None))
                    }
                    None => Ok((MESH_SIZE, None)),
                },
            }
        })
        .await?
    };

    // The scene goes first, so the inputs can be seen through its camera.
    let meshes: Vec<&String> = request
        .scene
        .iter()
        .chain(&request.inputs)
        .filter(|path| is_mesh(path))
        .collect();
    let work = format!("{WORK_ROOT}/{}", crate::upload::random_hex_id()?);
    let mut staged = Vec::new();
    let outcome = async {
        let mut renders = serde_json::Map::new();
        let mut views = std::collections::HashMap::new();
        let mut scene_camera: Option<Value> = None;
        for (index, mesh) in meshes.iter().enumerate() {
            let registry = job_registry(jobs)?;
            let prefix = format!("{work}/{index}");
            let scene = request.scene.as_ref() == Some(*mesh);
            let photograph = scene && companions.scene_photograph.is_some();
            staged.push(format!("{prefix}.zip"));
            if photograph {
                staged.push(format!("{prefix}.png"));
            }
            // A synthetic scene is a perspective view, and a mesh scored
            // against it is seen through exactly its camera; a mesh in photo
            // mode is seen through the photograph's focal length; other
            // meshes are seen from above.
            let view = match (scene, &scene_camera, focal_px) {
                (true, _, _) => json!({"projection": "perspective"}),
                (false, Some(camera), _) => camera.clone(),
                (false, None, Some(focal)) => {
                    json!({"projection": "perspective", "focal_px": focal})
                }
                (false, None, None) => json!({"projection": "orthographic"}),
            };
            let rendered = registry
                .render_geometry(mesh, &prefix, size, view, photograph)
                .await?;
            if scene {
                scene_camera = Some(json!({"projection": "perspective",
                    "focal_px": rendered["focal_px"], "position": rendered["position"]}));
            }
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
            // A benchmark truth archive is read as it is; a mesh by its view.
            truth: request
                .scene
                .as_ref()
                .map(|scene| views.get(scene).cloned().unwrap_or_else(|| scene.clone())),
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
        // The scene's photograph is published before the report, so a
        // failure leaves no report behind.
        let mut photograph = None;
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
            photograph = Some(workspace.write_generated_bytes(path, &bytes, request.overwrite)?);
        }
        let mut report = client.compare(&job).await?;
        if let Some(artifact) = photograph {
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
