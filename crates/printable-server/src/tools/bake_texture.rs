use std::{
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{ColorType, ImageFormat, ImageReader};
use printable_geom::{TextureRegion, TextureReport, bake_texture_stl};
use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config::Settings, error::ToolError, projects::revisions::hash_file};

use super::{
    geometry_blocking, geometry_worker_path, run_geometry_worker_process, validate_blender_artifact,
};

pub const FLAG: &str = "--bake-texture";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BakeTextureParams {
    /// Workspace path to a watertight STL in millimetres, Z up.
    source_path: String,
    /// Workspace path to an 8-bit or 16-bit grayscale PNG height map.
    height_map_path: String,
    /// A new workspace STL path for the textured solid.
    output_path: String,
    /// The rectangular region on an upward-facing planar surface.
    region: Region,
    /// Height at white in millimetres, positive to raise or negative to recess.
    depth_mm: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Region {
    /// The left and right X boundaries in millimetres.
    x_mm: [f64; 2],
    /// The front and back Y boundaries in millimetres.
    y_mm: [f64; 2],
    /// Height of the original planar surface in millimetres.
    surface_z_mm: f64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    source: PathBuf,
    height_map: PathBuf,
    region: TextureRegion,
    depth_mm: f64,
}

pub(super) async fn bake(
    workspace: Arc<Workspace>,
    settings: &Settings,
    params: BakeTextureParams,
) -> Result<Value, ToolError> {
    validate_blender_artifact(&workspace, &params.source_path, ".stl", true)?;
    validate_blender_artifact(&workspace, &params.height_map_path, ".png", true)?;
    validate_blender_artifact(&workspace, &params.output_path, ".stl", false)?;
    let worker_bin = geometry_worker_path(settings)?;
    let memory_bytes = settings.geometry_worker_memory_bytes;
    geometry_blocking(move || -> Result<Value, ToolError> {
        let source = workspace.snapshot_artifact(&params.source_path)?;
        let height_map = workspace.snapshot_artifact(&params.height_map_path)?;
        let source_sha256 = hash_file(source.path())?;
        let height_map_sha256 = hash_file(height_map.path())?;
        let staging = tempfile::Builder::new()
            .prefix("printable-bake-texture-")
            .tempdir()?;
        let request = staging.path().join("request.json");
        let output = staging.path().join("textured.stl");
        fs::write(
            &request,
            serde_json::to_vec(&WorkerRequest {
                source: source.path().into(),
                height_map: height_map.path().into(),
                region: TextureRegion {
                    x_mm: params.region.x_mm,
                    y_mm: params.region.y_mm,
                    surface_z_mm: params.region.surface_z_mm,
                },
                depth_mm: params.depth_mm,
            })?,
        )?;
        let report = run_geometry_worker_process(
            &worker_bin,
            memory_bytes,
            &[FLAG.as_ref(), request.as_os_str(), output.as_os_str()],
        )?;
        let sha256 = hash_file(&output)?;
        printable_geom::limit::check()?;
        let artifact = workspace.commit_generated_artifact(&params.output_path, &output, false)?;
        Ok(json!({
            "artifact": artifact, "sha256": sha256,
            "source_artifact": source.meta(), "source_sha256": source_sha256,
            "height_map_artifact": height_map.meta(), "height_map_sha256": height_map_sha256,
            "units": "millimetres", "report": report,
        }))
    })
    .await
}

/// Run in the disposable geometry worker over server-owned snapshot paths.
pub fn worker_run(request: &Path, output: &Path) -> Result<TextureReport, ToolError> {
    let request: WorkerRequest = serde_json::from_slice(&fs::read(request)?)?;
    let invalid_png =
        || ToolError::Validation("height map must be an 8-bit or 16-bit grayscale PNG".into());
    let file = BufReader::new(fs::File::open(&request.height_map)?);
    let image = ImageReader::with_format(file, ImageFormat::Png)
        .decode()
        .map_err(|_| invalid_png())?;
    if !matches!(image.color(), ColorType::L8 | ColorType::L16) {
        return Err(invalid_png());
    }
    let map = image.into_luma16();
    let (stl, report) = bake_texture_stl(
        &fs::read(request.source)?,
        request.region,
        request.depth_mm,
        [map.width() as usize, map.height() as usize],
        map.as_raw(),
    )?;
    fs::write(output, stl)?;
    Ok(report)
}
