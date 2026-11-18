use std::{
    ffi::OsStr,
    io::{self, Cursor, Read, Seek, Write},
    path::Path,
    sync::Arc,
};

use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use printable_geom::{TextureRegion, bake_texture_stl};
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
const SOURCE_FILE: &str = "source.stl";
const HEIGHT_MAP_FILE: &str = "height.png";
const OUTPUT_FILE: &str = "textured.stl";

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
    source: StagedArtifact,
    height_map: StagedArtifact,
    region: TextureRegion,
    depth_mm: f64,
    geometry_memory_bytes: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StagedArtifact {
    size_bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct WorkerResult {
    output: StagedArtifact,
    report: Value,
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
        let (source_artifact, source) = stage_input(
            &workspace,
            &staging,
            &params.source_path,
            SOURCE_FILE,
            memory_bytes,
        )?;
        let (height_map_artifact, height_map) = stage_input(
            &workspace,
            &staging,
            &params.height_map_path,
            HEIGHT_MAP_FILE,
            memory_bytes,
        )?;
        let request = WorkerRequest {
                source,
                height_map,
                region: TextureRegion {
                    x_mm: params.region.x_mm,
                    y_mm: params.region.y_mm,
                    surface_z_mm: params.region.surface_z_mm,
                },
                depth_mm: params.depth_mm,
                geometry_memory_bytes: memory_bytes,
        };
        // Keep the worker instructions outside the shared workspace.
        let mut input = tempfile::tempfile()?;
        serde_json::to_writer(&mut input, &request)?;
        input.rewind()?;
        let result: WorkerResult = serde_json::from_value(run_geometry_worker_process(
            &worker_bin,
            memory_bytes,
            &[FLAG.as_ref(), workspace_root.as_os_str(), scratch_id.as_ref()],
            Some(input),
        )?)?;
        printable_geom::limit::check()?;
        let output = result.output;
        let publication = workspace.commit_verified_scratch_artifact(
            &params.output_path, &staging, OUTPUT_FILE, output.size_bytes,
            |input, destination| {
                let (size, digest) = copy_and_hash(input, destination)?;
                if size != output.size_bytes || digest != output.sha256 {
                    return Err(WsError::ChangedWhileReading);
                }
                Ok(size)
            },
        );
        let artifact = match publication {
            Ok(artifact) => artifact,
            Err(error) => {
                printable_geom::limit::check()?;
                return Err(error.into());
            }
        };
        Ok(json!({
            "artifact": artifact, "sha256": output.sha256,
            "source_artifact": source_artifact, "source_sha256": request.source.sha256,
            "height_map_artifact": height_map_artifact, "height_map_sha256": request.height_map.sha256,
            "units": "millimetres", "report": result.report,
        }))
    })
    .await
}

fn stage_input(
    workspace: &Workspace,
    staging: &ManagedScratch,
    path: &str,
    destination: &str,
    memory_bytes: u64,
) -> Result<(ArtifactMeta, StagedArtifact), ToolError> {
    printable_geom::limit::check()?;
    let size = workspace.stat_artifact(path)?.size_bytes;
    if size >= memory_bytes {
        return Err(ToolError::Validation(
            "texture input exceeds the geometry worker's memory budget".into(),
        ));
    }
    let _storage = staging.admit_growth(size)?;
    let mut output = staging.create_file(destination)?;
    let staged = workspace.with_artifact_reader(path, |source| {
        copy_and_hash(&mut source.take(size), &mut output)
    });
    printable_geom::limit::check()?;
    let (artifact, (size, digest)) = staged?;
    if artifact.size_bytes != size {
        return Err(WsError::ChangedWhileReading.into());
    }
    Ok((
        artifact,
        StagedArtifact {
            size_bytes: size,
            sha256: digest,
        },
    ))
}

fn copy_and_hash(source: &mut dyn Read, destination: &mut dyn Write) -> io::Result<(u64, String)> {
    let mut buffer = [0u8; 65_536];
    let mut hash = Sha256::new();
    let mut copied = 0u64;
    loop {
        printable_geom::limit::check().map_err(io::Error::other)?;
        let len = source.read(&mut buffer)?;
        if len == 0 {
            return Ok((copied, hex_digest(&hash.finalize())));
        }
        let bytes = &buffer[..len];
        destination.write_all(bytes)?;
        hash.update(bytes);
        copied += len as u64;
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_staged_input(
    stage: &ManagedScratch,
    name: &str,
    input: &StagedArtifact,
    memory_bytes: u64,
) -> Result<Vec<u8>, ToolError> {
    if input.size_bytes >= memory_bytes {
        return Err(ToolError::Validation(
            "texture input exceeds the geometry worker's memory budget".into(),
        ));
    }
    let mut bytes = Vec::new();
    stage
        .open_file(name)?
        .take(input.size_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != input.size_bytes || hex_digest(&Sha256::digest(&bytes)) != input.sha256
    {
        return Err(WsError::ChangedWhileReading.into());
    }
    Ok(bytes)
}

/// Run in the disposable geometry worker with instructions from its parent.
pub fn worker_run(workspace_root: &Path, scratch_id: &OsStr) -> Result<Value, ToolError> {
    let request: WorkerRequest = serde_json::from_reader(io::stdin().lock().take(4096))?;
    let workspace = Workspace::open(Some(workspace_root), None)?;
    let scratch_id = scratch_id.to_str().ok_or(WsError::InvalidScratchId)?;
    let staging = workspace.retain_scratch(scratch_id)?;
    let invalid_png =
        || ToolError::Validation("height map must be an 8-bit or 16-bit grayscale PNG".into());
    let height_map = read_staged_input(
        &staging,
        HEIGHT_MAP_FILE,
        &request.height_map,
        request.geometry_memory_bytes,
    )?;
    let decoder = ImageReader::with_format(Cursor::new(height_map), ImageFormat::Png)
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
    let source = read_staged_input(
        &staging,
        SOURCE_FILE,
        &request.source,
        request.geometry_memory_bytes,
    )?;
    let (stl, report) = bake_texture_stl(
        &source,
        request.region,
        request.depth_mm,
        [map.width() as usize, map.height() as usize],
        map.as_raw(),
    )?;
    let output = StagedArtifact {
        size_bytes: stl.len() as u64,
        sha256: hex_digest(&Sha256::digest(&stl)),
    };
    let _storage = staging.admit_growth(output.size_bytes)?;
    staging.create_file(OUTPUT_FILE)?.write_all(&stl)?;
    Ok(json!({"output":output,"report":report}))
}
