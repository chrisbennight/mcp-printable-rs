//! The `photo` tool's `relief` action: a printable relief solid, a 16-bit
//! heightmap and a report from a photograph's cached analysis.
//!
//! The analysis runs as a retained photograph job. While it is unfinished,
//! the action returns that job; repeating the request once it succeeds reads
//! the analysis from its cache. The server snapshots the maps the relief
//! needs and checks each against the SHA-256 its manifest records. The
//! memory-isolated geometry worker decodes them and runs the stages of
//! [`printable_geom::relief`], and the server publishes the files it writes.

use std::{
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{DynamicImage, ImageFormat, ImageReader};
use printable_geom::relief::{
    Analysis, CompressionOptions, Grid, IntegrationMethod, IntegrationOptions, PrintabilityOptions,
    ReliefError, ReliefMap, SolidOptions, check_relief, compress, integrate, relief_solid,
};
use printable_workspace::{Snapshot, Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::Settings,
    error::ToolError,
    recon::{
        ANALYSIS_CACHE_ROOT, AnalysisQuality, AnalyzeRequest, DepthModel, Subject, file_digest,
    },
};

use super::{
    geometry_blocking, geometry_worker_path, run_geometry_worker_process, validate_scad_destination,
};

pub const FLAG: &str = "--relief";
const STL_FILE: &str = "relief.stl";
const HEIGHTMAP_FILE: &str = "height.png";
/// Largest analysis map snapshotted for a relief, as the analysis caches it.
const MAX_MAP_BYTES: u64 = 64 * 1024 * 1024;
/// The layer height the narrow-detail check counts losses in.
const LAYER_MM: f64 = 0.2;
const WIDTH_MM: std::ops::RangeInclusive<f64> = 5.0..=1000.0;
const RELIEF_DEPTH_MM: std::ops::RangeInclusive<f64> = 0.2..=50.0;
const BASE_MM: std::ops::RangeInclusive<f64> = 0.4..=50.0;
const BORDER_MM: std::ops::RangeInclusive<f64> = 0.0..=100.0;
const NOZZLE_MM: std::ops::RangeInclusive<f64> = 0.1..=2.0;
const COMPRESSION: std::ops::RangeInclusive<f64> = 0.0..=100.0;
const DETAIL_GAIN: std::ops::RangeInclusive<f64> = 0.0..=10.0;
const DEFAULT_NOZZLE_MM: f64 = 0.4;

// What the relief is for; each sets the defaults in `ReliefPreset::defaults`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReliefPreset {
    Plaque,
    Coin,
    Mold,
    Artwork,
}

// How the height field is made from the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReliefIntegration {
    #[default]
    DepthOnly,
    AnchoredNormals,
    FrankotChellappa,
}

impl ReliefIntegration {
    fn method(self) -> IntegrationMethod {
        match self {
            Self::DepthOnly => IntegrationMethod::DepthOnly,
            Self::AnchoredNormals => IntegrationMethod::AnchoredNormals,
            Self::FrankotChellappa => IntegrationMethod::FrankotChellappa,
        }
    }
}

/// The outline of the relief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReliefShape {
    /// The whole image, with an optional flat border around it.
    Rectangle,
    /// A round face inscribed in the image's central square, inside a raised
    /// rim `border_mm` wide; the square's corners stay at the base.
    Round,
}

/// A preset's starting values. None has been measured on printed reliefs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresetDefaults {
    pub shape: ReliefShape,
    pub relief_depth_mm: f64,
    pub base_mm: f64,
    pub border_mm: f64,
    pub compression: f64,
    pub mirror: bool,
    pub invert: bool,
    /// The analysis depth model, when the preset chooses one.
    pub depth_model: Option<DepthModel>,
}

impl ReliefPreset {
    pub fn defaults(self) -> PresetDefaults {
        let plaque = PresetDefaults {
            shape: ReliefShape::Rectangle,
            relief_depth_mm: 3.0,
            base_mm: 2.0,
            border_mm: 0.0,
            compression: 1.0,
            mirror: false,
            invert: false,
            depth_model: None,
        };
        match self {
            Self::Plaque => plaque,
            // A shallow face inside a rim, compressed harder so detail
            // survives the small depth.
            Self::Coin => PresetDefaults {
                shape: ReliefShape::Round,
                relief_depth_mm: 1.2,
                border_mm: 1.5,
                compression: 2.0,
                ..plaque
            },
            // Mirrored and inverted, so a cast reads the right way round,
            // inside a wall that encloses the cavity.
            Self::Mold => PresetDefaults {
                base_mm: 3.0,
                border_mm: 5.0,
                mirror: true,
                invert: true,
                ..plaque
            },
            // Depth Anything 3 gives painted artwork a sculpted reading;
            // stronger compression keeps its invented depth shallow.
            Self::Artwork => PresetDefaults {
                compression: 3.0,
                depth_model: Some(DepthModel::Da3MonoLarge),
                ..plaque
            },
        }
    }
}

// Optional fields are advertised without `null`; omitting one takes the
// preset's value.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReliefAdvanced {
    /// depth_only is the best measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ReliefIntegration")]
    pub integration: Option<ReliefIntegration>,
    // Slope compression, 0 to 100; 0 keeps proportions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub compression: Option<f64>,
    // Fine-detail multiplier, 0 to 10 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub detail_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub mirror: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub invert: Option<bool>,
    /// MoGe reads artwork as flat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "DepthModel")]
    pub depth_model: Option<DepthModel>,
    /// Analyse with Marigold V2 normals.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub normals: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReliefRequest {
    /// Photograph to analyse; or give analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub image: Option<String>,
    /// A finished analysis's cache.key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub analysis: Option<String>,
    /// New .stl; .height.png and .report.json go beside it.
    pub path: String,
    #[serde(default)]
    pub overwrite: bool,
    pub preset: ReliefPreset,
    /// mm; a coin's diameter.
    pub width_mm: f64,
    // Default keeps the image's proportions; not for coin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub height_mm: Option<f64>,
    // Lowest to highest point, 0.2 to 50 mm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub relief_depth_mm: Option<f64>,
    // Base below the relief, 0.4 to 50 mm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub base_mm: Option<f64>,
    /// Or a coin's rim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub border_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "AnalysisQuality")]
    pub quality: Option<AnalysisQuality>,
    // Nozzle for the narrow-detail check (default 0.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub nozzle_mm: Option<f64>,
    #[serde(default)]
    pub advanced: ReliefAdvanced,
}

/// Every relief choice with the preset's defaults filled in.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReliefSettings {
    pub preset: ReliefPreset,
    pub shape: ReliefShape,
    pub width_mm: f64,
    pub height_mm: Option<f64>,
    pub relief_depth_mm: f64,
    pub base_mm: f64,
    pub border_mm: f64,
    pub integration: ReliefIntegration,
    pub compression: f64,
    pub detail_gain: f64,
    pub mirror: bool,
    pub invert: bool,
    pub nozzle_mm: f64,
    pub layer_mm: f64,
}

fn within(name: &str, value: f64, range: &std::ops::RangeInclusive<f64>) -> Result<f64, ToolError> {
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(ToolError::Validation(format!(
            "{name} must be between {} and {}",
            range.start(),
            range.end()
        )))
    }
}

impl ReliefRequest {
    /// Check the request and fill in the preset's defaults.
    pub fn settings(&self) -> Result<ReliefSettings, ToolError> {
        let refuse = |message: &str| Err(ToolError::Validation(message.into()));
        match (&self.image, &self.analysis) {
            (Some(_), None) => self.analysis_request().validate()?,
            (None, Some(key)) => {
                if !is_cache_key(key) {
                    return refuse("analysis must be the cache.key of an analysis");
                }
                let advanced = &self.advanced;
                if self.quality.is_some() || advanced.depth_model.is_some() || advanced.normals {
                    return refuse(
                        "quality and analysis model choices apply only to a new analysis of image",
                    );
                }
            }
            _ => return refuse("give either image or analysis"),
        }
        if !self.path.ends_with(".stl") {
            return refuse("path must end in .stl");
        }
        let preset = self.preset.defaults();
        if preset.shape == ReliefShape::Round && self.height_mm.is_some() {
            return refuse("a coin is round: give its diameter as width_mm, without height_mm");
        }
        let width_mm = within("width_mm", self.width_mm, &WIDTH_MM)?;
        let height_mm = self
            .height_mm
            .map(|height| within("height_mm", height, &WIDTH_MM))
            .transpose()?;
        let border_mm = within(
            "border_mm",
            self.border_mm.unwrap_or(preset.border_mm),
            &BORDER_MM,
        )?;
        if preset.shape == ReliefShape::Round && border_mm >= width_mm / 4.0 {
            return refuse("a coin's rim, border_mm, must be less than a quarter of its diameter");
        }
        let advanced = &self.advanced;
        Ok(ReliefSettings {
            preset: self.preset,
            shape: preset.shape,
            width_mm,
            height_mm,
            relief_depth_mm: within(
                "relief_depth_mm",
                self.relief_depth_mm.unwrap_or(preset.relief_depth_mm),
                &RELIEF_DEPTH_MM,
            )?,
            base_mm: within("base_mm", self.base_mm.unwrap_or(preset.base_mm), &BASE_MM)?,
            border_mm,
            integration: advanced.integration.unwrap_or_default(),
            compression: within(
                "compression",
                advanced.compression.unwrap_or(preset.compression),
                &COMPRESSION,
            )?,
            detail_gain: within(
                "detail_gain",
                advanced.detail_gain.unwrap_or(1.0),
                &DETAIL_GAIN,
            )?,
            mirror: advanced.mirror.unwrap_or(preset.mirror),
            invert: advanced.invert.unwrap_or(preset.invert),
            nozzle_mm: within(
                "nozzle_mm",
                self.nozzle_mm.unwrap_or(DEFAULT_NOZZLE_MM),
                &NOZZLE_MM,
            )?,
            layer_mm: LAYER_MM,
        })
    }

    /// The shallow-subject analysis this relief reads, with the preset's
    /// and the caller's model choices.
    fn analysis_request(&self) -> AnalyzeRequest {
        let advanced = &self.advanced;
        AnalyzeRequest {
            image: self.image.clone().unwrap_or_default(),
            subject: Subject::Shallow,
            depth_model: advanced.depth_model.or(self.preset.defaults().depth_model),
            side: None,
            mask_model: None,
            focal_model: None,
            normals: advanced.normals,
            normals_model: None,
            device: None,
            quality: self.quality,
            focal_length_px: None,
        }
    }
}

fn is_cache_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The heightmap and report paths beside an STL path ending in `.stl`.
fn companion_paths(path: &str) -> (String, String) {
    let stem = &path[..path.len() - ".stl".len()];
    (format!("{stem}.height.png"), format!("{stem}.report.json"))
}

/// Build a relief from the photograph's analysis, or return the analysis
/// job when the analysis is not yet cached.
pub(super) async fn relief(
    workspace: Arc<Workspace>,
    settings: &Settings,
    request: ReliefRequest,
) -> Result<Value, ToolError> {
    let relief = request.settings()?;
    let (heightmap_path, report_path) = companion_paths(&request.path);
    for (path, suffix) in [
        (&request.path, ".stl"),
        (&heightmap_path, ".png"),
        (&report_path, ".json"),
    ] {
        validate_scad_destination(&workspace, path, suffix, request.overwrite)?;
    }
    let worker_bin = geometry_worker_path(settings)?;
    let key = match (&request.image, &request.analysis) {
        (Some(image), _) => {
            let client = settings.recon.as_ref().ok_or_else(|| {
                ToolError::Validation("the photograph worker is not configured".into())
            })?;
            workspace.stat_artifact(image)?;
            let answer = client.analyze(&request.analysis_request()).await?;
            if answer.get("job_id").is_some() {
                return Ok(json!({
                    "analysis_job": answer,
                    "next": "follow the job with status; once it succeeds, repeat this request to build the relief from the cached analysis",
                }));
            }
            answer["cache"]["key"]
                .as_str()
                .filter(|key| is_cache_key(key))
                .ok_or_else(|| {
                    ToolError::Validation("the analysis result has no cache key".into())
                })?
                .to_owned()
        }
        (None, Some(key)) => key.clone(),
        (None, None) => {
            return Err(ToolError::Validation(
                "give either image or analysis".into(),
            ));
        }
    };
    let memory_bytes = settings.geometry_worker_memory_bytes;
    let outputs = Outputs {
        stl: request.path,
        heightmap: heightmap_path,
        report: report_path,
        overwrite: request.overwrite,
    };
    geometry_blocking(move || build(&workspace, &worker_bin, memory_bytes, &key, relief, outputs))
        .await
}

struct Outputs {
    stl: String,
    heightmap: String,
    report: String,
    overwrite: bool,
}

/// What the geometry worker is asked to build.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    width: usize,
    height: usize,
    depth: PathBuf,
    mask: PathBuf,
    normals: Option<PathBuf>,
    confidence: Option<PathBuf>,
    focal_length_px: Option<f64>,
    settings: ReliefSettings,
}

fn changed_analysis() -> ToolError {
    ToolError::Validation(
        "the cached analysis no longer matches its manifest; run the analysis again".into(),
    )
}

/// A private copy of one analysis map, checked against its manifest.
fn snapshot_map(
    workspace: &Workspace,
    manifest: &Value,
    directory: &str,
    name: &str,
) -> Result<Snapshot, ToolError> {
    let description = &manifest["artifacts"][name];
    let path = description["artifact"]["path"]
        .as_str()
        .filter(|path| {
            path.strip_prefix(directory)
                .and_then(|file| file.strip_prefix('/'))
                .is_some_and(|file| !file.contains('/'))
        })
        .ok_or_else(changed_analysis)?;
    let snapshot = workspace.snapshot_artifact_bounded(path, MAX_MAP_BYTES)?;
    if description["sha256"].as_str() != Some(file_digest(snapshot.path())?.as_str()) {
        return Err(changed_analysis());
    }
    Ok(snapshot)
}

fn build(
    workspace: &Workspace,
    worker_bin: &Path,
    memory_bytes: u64,
    key: &str,
    settings: ReliefSettings,
    outputs: Outputs,
) -> Result<Value, ToolError> {
    let directory = format!("{ANALYSIS_CACHE_ROOT}/{key}");
    let bytes = match workspace.read_generated_bytes(&format!("{directory}/manifest.json")) {
        Ok((_, bytes)) => bytes,
        Err(WsError::NotFound(_)) => {
            return Err(ToolError::Validation(
                "no finished analysis has this cache key".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| changed_analysis())?;
    if manifest["cache"]["key"] != key {
        return Err(changed_analysis());
    }
    let size = |name: &str| {
        manifest[name]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(changed_analysis)
    };
    let (width, height) = (size("width")?, size("height")?);
    let integration = settings.integration;
    let mut snapshots = vec![
        snapshot_map(workspace, &manifest, &directory, "depth")?,
        snapshot_map(workspace, &manifest, &directory, "mask")?,
    ];
    let mut optional = |needed: bool, name: &str| -> Result<Option<PathBuf>, ToolError> {
        if !needed {
            return Ok(None);
        }
        let snapshot = snapshot_map(workspace, &manifest, &directory, name)?;
        let path = snapshot.path().to_owned();
        snapshots.push(snapshot);
        Ok(Some(path))
    };
    let normals = optional(integration != ReliefIntegration::DepthOnly, "normals")?;
    let confidence = optional(
        integration == ReliefIntegration::AnchoredNormals,
        "confidence",
    )?;
    printable_geom::limit::check()?;

    let staging = tempfile::Builder::new()
        .prefix("printable-relief-")
        .tempdir()?;
    let output = staging.path().join("output");
    fs::create_dir(&output)?;
    let request_path = staging.path().join("request.json");
    let request = WorkerRequest {
        width,
        height,
        depth: snapshots[0].path().to_owned(),
        mask: snapshots[1].path().to_owned(),
        normals,
        confidence,
        focal_length_px: manifest["focal_length"]["px"].as_f64(),
        settings: settings.clone(),
    };
    fs::write(&request_path, serde_json::to_vec(&request)?)?;
    let built = run_geometry_worker_process(
        worker_bin,
        memory_bytes,
        &[FLAG.as_ref(), request_path.as_os_str(), output.as_os_str()],
        None,
    )?;
    drop(snapshots);
    printable_geom::limit::check()?;

    let stl = workspace.commit_generated_artifact(
        &outputs.stl,
        &output.join(STL_FILE),
        outputs.overwrite,
    )?;
    let heightmap = workspace.commit_generated_artifact(
        &outputs.heightmap,
        &output.join(HEIGHTMAP_FILE),
        outputs.overwrite,
    )?;
    let analysis = json!({
        "cache_key": key,
        "image_sha256": manifest["image_sha256"],
        "width": width,
        "height": height,
        "settings": manifest["settings"],
        "depth": manifest["depth"],
        "focal_length": manifest["focal_length"],
        "scores": manifest["scores"],
        "warnings": manifest["warnings"],
    });
    let mut report = json!({
        "units": "millimetres",
        "settings": settings,
        "analysis": analysis,
        "relief": built,
    });
    let report_artifact = workspace.write_generated_bytes(
        &outputs.report,
        &serde_json::to_vec_pretty(&report)?,
        outputs.overwrite,
    )?;
    report["artifacts"] = json!({
        "stl": stl,
        "heightmap": heightmap,
        "report": report_artifact,
    });
    Ok(report)
}

/// A failure in the geometry worker, with its stable code.
#[derive(Debug)]
pub struct WorkerError {
    pub code: &'static str,
    pub message: String,
}

impl From<ReliefError> for WorkerError {
    fn from(error: ReliefError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

fn io_error(message: &str) -> impl FnOnce(std::io::Error) -> WorkerError + '_ {
    move |_| WorkerError {
        code: "geometry_worker_io",
        message: message.to_owned(),
    }
}

fn invalid_map(name: &str, problem: impl std::fmt::Display) -> WorkerError {
    WorkerError {
        code: "relief_invalid_map",
        message: format!("the analysis {name} map {problem}"),
    }
}

/// The depth map: one 32-bit floating-point sample per pixel.
fn read_depth(path: &Path, width: usize, height: usize) -> Result<Vec<f32>, WorkerError> {
    let file = fs::File::open(path).map_err(io_error("cannot open the depth map"))?;
    let mut decoder =
        tiff::decoder::Decoder::new(BufReader::new(file)).map_err(|e| invalid_map("depth", e))?;
    let dimensions = decoder.dimensions().map_err(|e| invalid_map("depth", e))?;
    if dimensions != (width as u32, height as u32) {
        return Err(invalid_map("depth", "has another size than its manifest"));
    }
    match decoder.read_image().map_err(|e| invalid_map("depth", e))? {
        tiff::decoder::DecodingResult::F32(values) if values.len() == width * height => Ok(values),
        _ => Err(invalid_map(
            "depth",
            "is not one 32-bit floating-point channel",
        )),
    }
}

/// A 16-bit PNG map with `channels` samples per pixel.
fn read_png16(
    path: &Path,
    name: &str,
    (width, height): (usize, usize),
    channels: usize,
) -> Result<Vec<u16>, WorkerError> {
    let image = ImageReader::open(path)
        .map_err(io_error("cannot open an analysis map"))?
        .with_guessed_format()
        .map_err(io_error("cannot read an analysis map"))?
        .decode()
        .map_err(|e| invalid_map(name, e))?;
    if (image.width() as usize, image.height() as usize) != (width, height) {
        return Err(invalid_map(name, "has another size than its manifest"));
    }
    match (image, channels) {
        (DynamicImage::ImageLuma16(image), 1) => Ok(image.into_raw()),
        (DynamicImage::ImageRgb16(image), 3) => Ok(image.into_raw()),
        _ => Err(invalid_map(
            name,
            "is not a 16-bit PNG of the expected channels",
        )),
    }
}

/// The values of the central `side` x `side` square of a `width`-wide grid.
fn crop<T: Copy>(values: &[T], width: usize, height: usize, side: usize) -> Vec<T> {
    let (left, top) = ((width - side) / 2, (height - side) / 2);
    (top..top + side)
        .flat_map(|row| {
            values[row * width + left..row * width + left + side]
                .iter()
                .copied()
        })
        .collect()
}

/// Distance in pixels from the centre of a `side` x `side` grid.
fn radius_at(index: usize, side: usize) -> f64 {
    let centre = (side - 1) as f64 / 2.0;
    let (x, y) = (
        (index % side) as f64 - centre,
        (index / side) as f64 - centre,
    );
    x.hypot(y)
}

/// Build the relief the request describes and write its STL and heightmap
/// into `output`. Runs inside the geometry worker.
pub fn worker_run(request_path: &Path, output: &Path) -> Result<Value, WorkerError> {
    let bytes = fs::read(request_path).map_err(io_error("cannot read the relief request"))?;
    let request: WorkerRequest = serde_json::from_slice(&bytes).map_err(|_| WorkerError {
        code: "validation",
        message: "geometry worker received an invalid relief request".into(),
    })?;
    let settings = &request.settings;
    let (mut width, mut height) = (request.width, request.height);
    let mut depth = read_depth(&request.depth, width, height)?;
    let mut mask: Vec<bool> = read_png16(&request.mask, "mask", (width, height), 1)?
        .into_iter()
        .map(|value| value >= 1 << 15)
        .collect();
    let mut confidence = match &request.confidence {
        Some(path) => Some(
            read_png16(path, "confidence", (width, height), 1)?
                .into_iter()
                .map(|value| f32::from(value) / 65535.0)
                .collect::<Vec<_>>(),
        ),
        None => None,
    };
    let mut normals = match &request.normals {
        Some(path) => Some(
            read_png16(path, "normals", (width, height), 3)?
                .chunks_exact(3)
                .map(decode_normal)
                .collect::<Vec<[f32; 3]>>(),
        ),
        None => None,
    };

    // A round relief uses the image's central square, so the principal
    // point stays at the image centre.
    let round = settings.shape == ReliefShape::Round;
    let mut face_px = f64::INFINITY;
    let mut outline_px = f64::INFINITY;
    if round {
        let side = width.min(height);
        depth = crop(&depth, width, height, side);
        mask = crop(&mask, width, height, side);
        confidence = confidence.map(|values| crop(&values, width, height, side));
        normals = normals.map(|values| crop(&values, width, height, side));
        (width, height) = (side, side);
        outline_px = (side - 1) as f64 / 2.0;
        face_px = outline_px - settings.border_mm * (side - 1) as f64 / settings.width_mm;
        for (index, inside) in mask.iter_mut().enumerate() {
            *inside &= radius_at(index, side) <= face_px;
        }
    }

    let analysis = Analysis {
        depth: Grid::new(width, height, depth)?,
        normals: normals
            .map(|values| Grid::new(width, height, values))
            .transpose()?,
        mask: Some(Grid::new(width, height, mask)?),
        confidence: confidence
            .map(|values| Grid::new(width, height, values))
            .transpose()?,
        focal_length_px: request.focal_length_px,
    };
    let field = integrate(
        &analysis,
        &IntegrationOptions {
            method: settings.integration.method(),
            ..IntegrationOptions::default()
        },
    )?;
    drop(analysis);
    let (mut map, compression) = compress(
        &field,
        &CompressionOptions {
            height_mm: settings.height_mm,
            compression: settings.compression,
            detail_gain: settings.detail_gain,
            ..CompressionOptions::new(settings.width_mm, settings.relief_depth_mm)
        },
    )?;
    let integration = field.report.clone();
    drop(field);
    if round {
        // The rim stands level with the relief's top; outside the outline
        // the plate stays at the base.
        let heights: Vec<f64> = map
            .heights_mm
            .values()
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let radius = radius_at(index, width);
                if radius > outline_px {
                    0.0
                } else if radius > face_px {
                    settings.relief_depth_mm
                } else {
                    *value
                }
            })
            .collect();
        map.heights_mm = Grid::new(width, height, heights)?;
    }

    // A border stands level with the relief's top: raised around a
    // positive relief, and as the wall enclosing an inverted cavity.
    let solid = relief_solid(
        &map,
        &SolidOptions {
            base_mm: settings.base_mm,
            border_mm: if round { 0.0 } else { settings.border_mm },
            border_height_mm: if settings.invert {
                0.0
            } else {
                settings.relief_depth_mm
            },
            mirror: settings.mirror,
            invert: settings.invert,
        },
    )?;
    // An inverted relief prints upside down, so its narrow ridges are the
    // relief's narrow grooves.
    let printed = if settings.invert {
        let high = highest(&map);
        ReliefMap {
            heights_mm: Grid::new(
                width,
                height,
                map.heights_mm
                    .values()
                    .iter()
                    .map(|value| high - value)
                    .collect(),
            )?,
            spacing_mm: map.spacing_mm,
        }
    } else {
        map.clone()
    };
    let checks = check_relief(
        &printed,
        &solid,
        &PrintabilityOptions {
            nozzle_mm: settings.nozzle_mm,
            layer_mm: settings.layer_mm,
            minimum_relief_mm: None,
            maximum_relief_mm: None,
        },
    )?;
    drop(printed);

    write_heightmap(&map, settings.relief_depth_mm, &output.join(HEIGHTMAP_FILE))?;
    let mesh = solid.into_mesh();
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in &mesh.vertices {
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    fs::write(output.join(STL_FILE), printable_geom::bust_stl(&mesh))
        .map_err(io_error("cannot write the relief STL"))?;
    Ok(json!({
        "size_mm": [high[0] - low[0], high[1] - low[1], high[2] - low[2]],
        "triangles": mesh.triangles.len(),
        "samples": [width, height],
        "sample_spacing_mm": map.spacing_mm,
        "integration": integration,
        "compression": compression,
        "checks": checks,
        "heightmap": {
            "width": width,
            "height": height,
            "bit_depth": 16,
            "full_scale_mm": settings.relief_depth_mm,
            "encoding": "height_mm = value / 65535 * full_scale_mm",
            "describes": "the relief before any border, mirroring or inversion, top row at the image's top",
        },
    }))
}

fn highest(map: &ReliefMap) -> f64 {
    map.heights_mm
        .values()
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
}

/// A 16-bit greyscale PNG whose full scale is `full_scale_mm`.
fn write_heightmap(map: &ReliefMap, full_scale_mm: f64, path: &Path) -> Result<(), WorkerError> {
    let grid = &map.heights_mm;
    let values: Vec<u16> = grid
        .values()
        .iter()
        .map(|value| {
            (value / full_scale_mm * 65535.0)
                .round()
                .clamp(0.0, 65535.0) as u16
        })
        .collect();
    let (width, height) = (grid.width() as u32, grid.height() as u32);
    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(width, height, values)
        .expect("one value per sample")
        .save_with_format(path, ImageFormat::Png)
        .map_err(|_| WorkerError {
            code: "geometry_worker_io",
            message: "cannot write the relief heightmap".into(),
        })
}

/// A unit normal from its 16-bit encoding `(c + 1) / 2 * 65535`.
fn decode_normal(encoded: &[u16]) -> [f32; 3] {
    let component = |value: u16| f32::from(value) / 65535.0 * 2.0 - 1.0;
    [
        component(encoded[0]),
        component(encoded[1]),
        component(encoded[2]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(preset: ReliefPreset) -> ReliefRequest {
        ReliefRequest {
            image: Some("photos/coin.jpg".into()),
            analysis: None,
            path: "relief/coin.stl".into(),
            overwrite: false,
            preset,
            width_mm: 40.0,
            height_mm: None,
            relief_depth_mm: None,
            base_mm: None,
            border_mm: None,
            quality: None,
            nozzle_mm: None,
            advanced: ReliefAdvanced::default(),
        }
    }

    #[test]
    fn each_preset_fills_in_its_documented_defaults() {
        let settings = |preset| request(preset).settings().unwrap();
        let plaque = settings(ReliefPreset::Plaque);
        assert_eq!(
            (
                plaque.shape,
                plaque.relief_depth_mm,
                plaque.base_mm,
                plaque.border_mm,
                plaque.compression,
                plaque.mirror,
                plaque.invert
            ),
            (ReliefShape::Rectangle, 3.0, 2.0, 0.0, 1.0, false, false)
        );
        let coin = settings(ReliefPreset::Coin);
        assert_eq!(
            (
                coin.shape,
                coin.relief_depth_mm,
                coin.border_mm,
                coin.compression
            ),
            (ReliefShape::Round, 1.2, 1.5, 2.0)
        );
        let mold = settings(ReliefPreset::Mold);
        assert_eq!(
            (mold.base_mm, mold.border_mm, mold.mirror, mold.invert),
            (3.0, 5.0, true, true)
        );
        let artwork = settings(ReliefPreset::Artwork);
        assert_eq!(artwork.compression, 3.0);
        assert_eq!(
            request(ReliefPreset::Artwork)
                .analysis_request()
                .depth_model,
            Some(DepthModel::Da3MonoLarge)
        );
        for preset in [
            ReliefPreset::Plaque,
            ReliefPreset::Coin,
            ReliefPreset::Mold,
            ReliefPreset::Artwork,
        ] {
            let settings = settings(preset);
            assert_eq!(settings.integration, ReliefIntegration::DepthOnly);
            assert_eq!((settings.detail_gain, settings.nozzle_mm), (1.0, 0.4));
            let analysis = request(preset).analysis_request();
            assert_eq!(analysis.subject, Subject::Shallow);
            assert!(!analysis.normals);
        }
    }

    #[test]
    fn caller_values_and_advanced_choices_replace_the_preset() {
        let mut chosen = request(ReliefPreset::Mold);
        chosen.relief_depth_mm = Some(4.0);
        chosen.border_mm = Some(0.0);
        chosen.quality = Some(AnalysisQuality::Fast);
        chosen.advanced = ReliefAdvanced {
            integration: Some(ReliefIntegration::AnchoredNormals),
            compression: Some(0.0),
            detail_gain: Some(2.0),
            mirror: Some(false),
            invert: Some(false),
            depth_model: Some(DepthModel::Moge3Vitl),
            normals: true,
        };
        let settings = chosen.settings().unwrap();
        assert_eq!(
            (
                settings.relief_depth_mm,
                settings.border_mm,
                settings.compression,
                settings.detail_gain,
                settings.mirror,
                settings.invert,
                settings.integration
            ),
            (
                4.0,
                0.0,
                0.0,
                2.0,
                false,
                false,
                ReliefIntegration::AnchoredNormals
            )
        );
        let analysis = chosen.analysis_request();
        assert_eq!(analysis.depth_model, Some(DepthModel::Moge3Vitl));
        assert_eq!(analysis.quality, Some(AnalysisQuality::Fast));
        assert!(analysis.normals);
    }

    #[test]
    fn invalid_requests_are_refused() {
        let mut both = request(ReliefPreset::Plaque);
        both.analysis = Some("a".repeat(64));
        let mut neither = request(ReliefPreset::Plaque);
        neither.image = None;
        let mut key = request(ReliefPreset::Plaque);
        key.image = None;
        key.analysis = Some("not a key".into());
        let mut models = request(ReliefPreset::Plaque);
        models.image = None;
        models.analysis = Some("a".repeat(64));
        models.advanced.depth_model = Some(DepthModel::Dp2L);
        let mut coin_height = request(ReliefPreset::Coin);
        coin_height.height_mm = Some(40.0);
        let mut wide_rim = request(ReliefPreset::Coin);
        wide_rim.border_mm = Some(10.0);
        let mut shallow = request(ReliefPreset::Plaque);
        shallow.relief_depth_mm = Some(0.1);
        let mut small = request(ReliefPreset::Plaque);
        small.width_mm = 1.0;
        let mut path = request(ReliefPreset::Plaque);
        path.path = "relief/coin.obj".into();
        let mut photo = request(ReliefPreset::Plaque);
        photo.image = Some("photos/coin.gif".into());
        for invalid in [
            both,
            neither,
            key,
            models,
            coin_height,
            wide_rim,
            shallow,
            small,
            path,
            photo,
        ] {
            assert_eq!(
                invalid.settings().unwrap_err().code(),
                "validation",
                "{invalid:?}"
            );
        }
        let mut cached = request(ReliefPreset::Plaque);
        cached.image = None;
        cached.analysis = Some("a".repeat(64));
        assert!(cached.settings().is_ok());
    }

    #[test]
    fn companions_sit_beside_the_stl() {
        assert_eq!(
            companion_paths("relief/coin.stl"),
            (
                "relief/coin.height.png".to_owned(),
                "relief/coin.report.json".to_owned()
            )
        );
    }

    #[test]
    fn a_crop_keeps_the_central_square() {
        let values: Vec<usize> = (0..12).collect();
        assert_eq!(crop(&values, 4, 3, 3), [0, 1, 2, 4, 5, 6, 8, 9, 10]);
        assert_eq!(crop(&values, 3, 4, 3), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
