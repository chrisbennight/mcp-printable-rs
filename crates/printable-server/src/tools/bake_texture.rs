use std::{
    fs,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use printable_geom::{TextureRegion, TextureReport, bake_texture_stl};
use printable_workspace::{ArtifactMeta, ManagedScratch, Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{config::Settings, error::ToolError};

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
    geometry_memory_bytes: u64,
    workspace_root: PathBuf,
    scratch_id: String,
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
        let workspace_root = workspace.root_path().ok_or(WsError::Unconfined)?;
        let staging = workspace.scratch(0, "bake-texture")?;
        let scratch_id = staging
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("workspace scratch IDs contain only ASCII hexadecimal digits");
        let request = staging.path().join("request.json");
        let output = staging.path().join("textured.stl");
        let source = staging.path().join("source.stl");
        let height_map = staging.path().join("height.png");
        let (source_artifact, source_sha256) = stage_input(
            &workspace,
            &staging,
            &params.source_path,
            &source,
            memory_bytes,
        )?;
        let (height_map_artifact, height_map_sha256) = stage_input(
            &workspace,
            &staging,
            &params.height_map_path,
            &height_map,
            memory_bytes,
        )?;
        fs::write(
            &request,
            serde_json::to_vec(&WorkerRequest {
                source,
                height_map,
                region: TextureRegion {
                    x_mm: params.region.x_mm,
                    y_mm: params.region.y_mm,
                    surface_z_mm: params.region.surface_z_mm,
                },
                depth_mm: params.depth_mm,
                geometry_memory_bytes: memory_bytes,
                workspace_root: workspace_root.into(),
                scratch_id: scratch_id.into(),
            })?,
        )?;
        let report = run_geometry_worker_process(
            &worker_bin,
            memory_bytes,
            &[FLAG.as_ref(), request.as_os_str(), output.as_os_str()],
        )?;
        let written = copy_and_hash(&mut fs::File::open(&output)?, &mut io::sink());
        printable_geom::limit::check()?;
        let (_, sha256) = written?;
        let artifact = workspace.commit_generated_artifact(&params.output_path, &output, false)?;
        Ok(json!({
            "artifact": artifact, "sha256": sha256,
            "source_artifact": source_artifact, "source_sha256": source_sha256,
            "height_map_artifact": height_map_artifact, "height_map_sha256": height_map_sha256,
            "units": "millimetres", "report": report,
        }))
    })
    .await
}

fn stage_input(
    workspace: &Workspace,
    staging: &ManagedScratch,
    path: &str,
    destination: &Path,
    memory_bytes: u64,
) -> Result<(ArtifactMeta, String), ToolError> {
    printable_geom::limit::check()?;
    let size = workspace.stat_artifact(path)?.size_bytes;
    if size >= memory_bytes {
        return Err(ToolError::Validation(
            "texture input exceeds the geometry worker's memory budget".into(),
        ));
    }
    let _storage = staging.admit_growth(size)?;
    let staged = workspace.with_artifact_reader(path, |source| {
        copy_and_hash(&mut source.take(size), &mut fs::File::create(destination)?)
    });
    printable_geom::limit::check()?;
    let (artifact, (size, digest)) = staged?;
    if artifact.size_bytes != size {
        return Err(WsError::ChangedWhileReading.into());
    }
    Ok((artifact, digest))
}

fn copy_and_hash(source: &mut dyn Read, destination: &mut dyn Write) -> io::Result<(u64, String)> {
    let mut buffer = [0u8; 65_536];
    let mut hash = Sha256::new();
    let mut copied = 0u64;
    loop {
        printable_geom::limit::check().map_err(io::Error::other)?;
        let len = source.read(&mut buffer)?;
        if len == 0 {
            let digest = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            return Ok((copied, digest));
        }
        let bytes = &buffer[..len];
        destination.write_all(bytes)?;
        hash.update(bytes);
        copied += len as u64;
    }
}

/// Run in the disposable geometry worker over server-owned snapshot paths.
pub fn worker_run(request: &Path, output: &Path) -> Result<TextureReport, ToolError> {
    let request: WorkerRequest = serde_json::from_slice(&fs::read(request)?)?;
    let workspace = Workspace::open(Some(&request.workspace_root), None)?;
    let staging = workspace.retain_scratch(&request.scratch_id)?;
    if output != staging.path().join("textured.stl") {
        return Err(ToolError::Validation(
            "texture worker output must use its workspace scratch directory".into(),
        ));
    }
    let invalid_png =
        || ToolError::Validation("height map must be an 8-bit or 16-bit grayscale PNG".into());
    let file = BufReader::new(fs::File::open(&request.height_map)?);
    let decoder = ImageReader::with_format(file, ImageFormat::Png)
        .into_decoder()
        .map_err(|_| invalid_png())?;
    if !matches!(decoder.color_type(), ColorType::L8 | ColorType::L16) {
        return Err(invalid_png());
    }
    let (width, height) = decoder.dimensions();
    // An interior sample contributes two XYZ vertices and four triangles with
    // 64-bit indices before allocating the kernel's own mesh data.
    let bytes_per_sample = 6 * size_of::<f64>() + 12 * size_of::<u64>();
    if (u64::from(width) * u64::from(height))
        .checked_mul(bytes_per_sample as u64)
        .is_none_or(|bytes| bytes >= request.geometry_memory_bytes)
    {
        return Err(ToolError::Validation(format!(
            "{width} × {height} is too large for the geometry worker's memory budget; resample the height map"
        )));
    }
    let map = DynamicImage::from_decoder(decoder)
        .map_err(|_| invalid_png())?
        .into_luma16();
    let (stl, report) = bake_texture_stl(
        &fs::read(request.source)?,
        request.region,
        request.depth_mm,
        [map.width() as usize, map.height() as usize],
        map.as_raw(),
    )?;
    let _storage = staging.admit_growth(stl.len() as u64)?;
    fs::write(output, stl)?;
    Ok(report)
}
