//! The `photo` tool's `lithophane` action: a backlit lithophane STL, a
//! preview of how it looks against the light and a report, from a workspace
//! image or as a calibration test strip.
//!
//! It needs no photograph worker. The server snapshots the image and the
//! memory-isolated geometry worker decodes it, maps its brightness to
//! thickness with [`printable_geom::relief::lithophane`] and writes the
//! files, which the server publishes.

use std::{
    f64::consts::PI,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{GenericImageView, ImageFormat, ImageReader, imageops::FilterType};
use printable_geom::ValidationOptions;
use printable_geom::relief::{
    DEFAULT_MAX_PIXELS, Grid, PrintabilityOptions, ReliefError, ReliefMap, check_relief,
    lithophane::{
        ToneMapping, bend_around_cylinder, linear_to_srgb, lithophane_map, lithophane_solid,
        srgb_to_linear, step_wedge,
    },
};
use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::Settings,
    error::ToolError,
    recon::{IMAGE_SUFFIXES, file_digest},
};

use super::{
    geometry_blocking, geometry_worker_path, relief::WorkerError, run_geometry_worker_process,
    validate_scad_destination,
};

pub const FLAG: &str = "--lithophane";
const STL_FILE: &str = "lithophane.stl";
const PREVIEW_FILE: &str = "preview.png";
/// Largest image snapshotted for a lithophane.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest image side decoded, in pixels.
const MAX_IMAGE_SIDE: u32 = 12_000;
/// The layer height the narrow-detail check counts losses in.
const LAYER_MM: f64 = 0.2;
/// A test strip's steps, and its width over its height.
const STRIP_STEPS: usize = 10;
const STRIP_ASPECT: f64 = 5.0;
const SIZE_MM: std::ops::RangeInclusive<f64> = 10.0..=300.0;
const MIN_THICKNESS_MM: std::ops::RangeInclusive<f64> = 0.4..=5.0;
const MAX_THICKNESS_MM: std::ops::RangeInclusive<f64> = 0.6..=10.0;
const ATTENUATION_PER_MM: std::ops::RangeInclusive<f64> = 0.05..=10.0;
const BORDER_MM: std::ops::RangeInclusive<f64> = 0.0..=50.0;
const CONTRAST: std::ops::RangeInclusive<f64> = 0.1..=4.0;
const NOZZLE_MM: std::ops::RangeInclusive<f64> = 0.2..=1.0;
const RADIUS_MM: std::ops::RangeInclusive<f64> = 10.0..=1000.0;
const ARC_DEGREES: std::ops::RangeInclusive<f64> = 1.0..=359.0;
const DEFAULT_MIN_THICKNESS_MM: f64 = 0.8;
const DEFAULT_MAX_THICKNESS_MM: f64 = 3.0;
const DEFAULT_NOZZLE_MM: f64 = 0.4;
const DEFAULT_ARC_DEGREES: f64 = 120.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LithophaneShape {
    #[default]
    Flat,
    Curved,
}

/// Filaments with documented starting attenuations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Filament {
    #[default]
    WhitePla,
    NaturalPla,
    WhitePetg,
}

impl Filament {
    /// Starting values per millimetre, not measured on prints; measure the
    /// filament and backlight with a test strip.
    pub fn attenuation_per_mm(self) -> f64 {
        match self {
            Self::WhitePla => 1.5,
            Self::NaturalPla => 0.7,
            Self::WhitePetg => 1.0,
        }
    }
}

// Optional fields are advertised without `null`; omitting one takes its
// documented default.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LithophaneRequest {
    /// Workspace image; omit for test_strip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub image: Option<String>,
    /// A step wedge to measure attenuation_per_mm.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test_strip: bool,
    /// New .stl; .preview.png and .report.json go beside it.
    pub path: String,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub shape: LithophaneShape,
    /// mm; give this or height_mm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub width_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub height_mm: Option<f64>,
    /// curved: this or arc_degrees (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub radius_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub arc_degrees: Option<f64>,
    /// Brightest; default 0.8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub min_thickness_mm: Option<f64>,
    /// Darkest; default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub max_thickness_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Filament")]
    pub filament: Option<Filament>,
    /// Measured k; replaces filament.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub attenuation_per_mm: Option<f64>,
    /// Frame at full thickness; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub border_mm: Option<f64>,
    /// [left, top, width, height] pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "[u32; 4]")]
    pub crop_px: Option<[u32; 4]>,
    /// Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub contrast: Option<f64>,
    // Nozzle diameter (default 0.4); samples are half of it apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "f64")]
    pub nozzle_mm: Option<f64>,
}

/// Every lithophane choice with its default filled in.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LithophaneSettings {
    pub test_strip: bool,
    pub shape: LithophaneShape,
    pub width_mm: Option<f64>,
    pub height_mm: Option<f64>,
    pub radius_mm: Option<f64>,
    pub arc_degrees: Option<f64>,
    pub min_thickness_mm: f64,
    pub max_thickness_mm: f64,
    /// The named filament, when its starting attenuation is used.
    pub filament: Option<Filament>,
    pub attenuation_per_mm: f64,
    pub border_mm: f64,
    pub crop_px: Option<[u32; 4]>,
    pub contrast: f64,
    pub nozzle_mm: f64,
    pub layer_mm: f64,
}

impl LithophaneSettings {
    fn tone(&self) -> ToneMapping {
        ToneMapping {
            min_thickness_mm: self.min_thickness_mm,
            max_thickness_mm: self.max_thickness_mm,
            attenuation_per_mm: self.attenuation_per_mm,
        }
    }
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

fn optional(
    name: &str,
    value: Option<f64>,
    range: &std::ops::RangeInclusive<f64>,
) -> Result<Option<f64>, ToolError> {
    value.map(|value| within(name, value, range)).transpose()
}

impl LithophaneRequest {
    /// Check the request and fill in the defaults.
    pub fn settings(&self) -> Result<LithophaneSettings, ToolError> {
        let refuse = |message: &str| Err(ToolError::Validation(message.into()));
        match (&self.image, self.test_strip) {
            (Some(image), false) => {
                let lower = image.to_ascii_lowercase();
                if !IMAGE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) {
                    return refuse("image must be a .jpg, .jpeg, .png or .webp file");
                }
            }
            (None, true) => {
                if self.crop_px.is_some() || self.contrast.is_some() {
                    return refuse("crop_px and contrast apply only to an image");
                }
            }
            _ => return refuse("give either image or test_strip: true"),
        }
        if !self.path.ends_with(".stl") {
            return refuse("path must end in .stl");
        }
        if self.width_mm.is_some() == self.height_mm.is_some() {
            return refuse(
                "give width_mm or height_mm, not both; the other follows the image's proportions",
            );
        }
        match self.shape {
            LithophaneShape::Flat if self.radius_mm.is_some() || self.arc_degrees.is_some() => {
                return refuse("radius_mm and arc_degrees apply only to a curved lithophane");
            }
            LithophaneShape::Curved if self.radius_mm.is_some() && self.arc_degrees.is_some() => {
                return refuse("give radius_mm or arc_degrees, not both");
            }
            _ => {}
        }
        let min_thickness_mm = within(
            "min_thickness_mm",
            self.min_thickness_mm.unwrap_or(DEFAULT_MIN_THICKNESS_MM),
            &MIN_THICKNESS_MM,
        )?;
        let max_thickness_mm = within(
            "max_thickness_mm",
            self.max_thickness_mm.unwrap_or(DEFAULT_MAX_THICKNESS_MM),
            &MAX_THICKNESS_MM,
        )?;
        if max_thickness_mm < min_thickness_mm + LAYER_MM {
            return refuse(
                "max_thickness_mm must be at least one 0.2 mm layer above min_thickness_mm",
            );
        }
        if let Some([_, _, width, height]) = self.crop_px
            && (width < 2 || height < 2)
        {
            return refuse("crop_px must be at least 2 pixels wide and high");
        }
        let (filament, attenuation_per_mm) = match self.attenuation_per_mm {
            Some(measured) => {
                if self.filament.is_some() {
                    return refuse("give filament or a measured attenuation_per_mm, not both");
                }
                (
                    None,
                    within("attenuation_per_mm", measured, &ATTENUATION_PER_MM)?,
                )
            }
            None => {
                let filament = self.filament.unwrap_or_default();
                (Some(filament), filament.attenuation_per_mm())
            }
        };
        let arc_degrees = match self.shape {
            LithophaneShape::Curved if self.radius_mm.is_none() => {
                Some(self.arc_degrees.unwrap_or(DEFAULT_ARC_DEGREES))
            }
            _ => None,
        };
        let nozzle_mm = within(
            "nozzle_mm",
            self.nozzle_mm.unwrap_or(DEFAULT_NOZZLE_MM),
            &NOZZLE_MM,
        )?;
        let border_mm = within("border_mm", self.border_mm.unwrap_or(0.0), &BORDER_MM)?;
        if border_mm > 0.0 && border_mm < nozzle_mm {
            return refuse("border_mm must be 0 or at least nozzle_mm, so the border prints");
        }
        Ok(LithophaneSettings {
            test_strip: self.test_strip,
            shape: self.shape,
            width_mm: optional("width_mm", self.width_mm, &SIZE_MM)?,
            height_mm: optional("height_mm", self.height_mm, &SIZE_MM)?,
            radius_mm: optional("radius_mm", self.radius_mm, &RADIUS_MM)?,
            arc_degrees: optional("arc_degrees", arc_degrees, &ARC_DEGREES)?,
            min_thickness_mm,
            max_thickness_mm,
            filament,
            attenuation_per_mm,
            border_mm,
            crop_px: self.crop_px,
            contrast: within("contrast", self.contrast.unwrap_or(1.0), &CONTRAST)?,
            nozzle_mm,
            layer_mm: LAYER_MM,
        })
    }
}

/// The preview and report paths beside an STL path ending in `.stl`.
fn companion_paths(path: &str) -> (String, String) {
    let stem = &path[..path.len() - ".stl".len()];
    (format!("{stem}.preview.png"), format!("{stem}.report.json"))
}

/// Build a lithophane or a test strip.
pub(super) async fn lithophane(
    workspace: Arc<Workspace>,
    settings: &Settings,
    request: LithophaneRequest,
) -> Result<Value, ToolError> {
    let lithophane = request.settings()?;
    let (preview, report) = companion_paths(&request.path);
    for (path, suffix) in [
        (&request.path, ".stl"),
        (&preview, ".png"),
        (&report, ".json"),
    ] {
        validate_scad_destination(&workspace, path, suffix, request.overwrite)?;
    }
    let worker_bin = geometry_worker_path(settings)?;
    let memory_bytes = settings.geometry_worker_memory_bytes;
    let outputs = Outputs {
        stl: request.path,
        preview,
        report,
        overwrite: request.overwrite,
    };
    let image = request.image;
    geometry_blocking(move || {
        build(
            &workspace,
            &worker_bin,
            memory_bytes,
            image.as_deref(),
            lithophane,
            outputs,
        )
    })
    .await
}

struct Outputs {
    stl: String,
    preview: String,
    report: String,
    overwrite: bool,
}

/// What the geometry worker is asked to build.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    image: Option<PathBuf>,
    settings: LithophaneSettings,
}

fn build(
    workspace: &Workspace,
    worker_bin: &Path,
    memory_bytes: u64,
    image: Option<&str>,
    settings: LithophaneSettings,
    outputs: Outputs,
) -> Result<Value, ToolError> {
    let snapshot = image
        .map(|path| workspace.snapshot_artifact_bounded(path, MAX_IMAGE_BYTES))
        .transpose()?;
    let source = match (image, &snapshot) {
        (Some(path), Some(snapshot)) => {
            json!({"path": path, "sha256": file_digest(snapshot.path())?})
        }
        _ => Value::Null,
    };
    printable_geom::limit::check()?;

    let staging = tempfile::Builder::new()
        .prefix("printable-lithophane-")
        .tempdir()?;
    let output = staging.path().join("output");
    fs::create_dir(&output)?;
    let request_path = staging.path().join("request.json");
    let request = WorkerRequest {
        image: snapshot.as_ref().map(|snapshot| snapshot.path().to_owned()),
        settings: settings.clone(),
    };
    fs::write(&request_path, serde_json::to_vec(&request)?)?;
    let built = run_geometry_worker_process(
        worker_bin,
        memory_bytes,
        &[FLAG.as_ref(), request_path.as_os_str(), output.as_os_str()],
        None,
    )?;
    drop(snapshot);
    printable_geom::limit::check()?;

    let stl = workspace.commit_generated_artifact(
        &outputs.stl,
        &output.join(STL_FILE),
        outputs.overwrite,
    )?;
    let preview = workspace.commit_generated_artifact(
        &outputs.preview,
        &output.join(PREVIEW_FILE),
        outputs.overwrite,
    )?;
    let mut report = json!({
        "units": "millimetres",
        "settings": settings,
        "image": source,
        "lithophane": built,
    });
    let report_artifact = workspace.write_generated_bytes(
        &outputs.report,
        &serde_json::to_vec_pretty(&report)?,
        outputs.overwrite,
    )?;
    report["artifacts"] = json!({
        "stl": stl,
        "preview": preview,
        "report": report_artifact,
    });
    Ok(report)
}

fn io_error(message: &str) -> impl FnOnce(std::io::Error) -> WorkerError + '_ {
    move |_| WorkerError {
        code: "geometry_worker_io",
        message: message.to_owned(),
    }
}

fn invalid_image(problem: impl std::fmt::Display) -> WorkerError {
    WorkerError {
        code: "lithophane_invalid_image",
        message: format!("the image {problem}"),
    }
}

fn invalid(message: String) -> WorkerError {
    WorkerError {
        code: "validation",
        message,
    }
}

/// The image's linear-light brightness, turned upright as its EXIF
/// orientation says, cropped and with its contrast adjusted, as a grid of its
/// pixels. Transparent parts count as white.
fn read_brightness(path: &Path, settings: &LithophaneSettings) -> Result<Grid<f64>, WorkerError> {
    let mut reader = ImageReader::open(path)
        .map_err(io_error("cannot open the image"))?
        .with_guessed_format()
        .map_err(io_error("cannot read the image"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    reader.limits(limits);
    // Turned upright as the photograph decoder does, so crop_px and the
    // proportions refer to the image as it is viewed.
    let mut decoder = reader.into_decoder().map_err(invalid_image)?;
    let orientation = image::ImageDecoder::orientation(&mut decoder).map_err(invalid_image)?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(invalid_image)?;
    image.apply_orientation(orientation);
    let [left, top, width, height] =
        settings
            .crop_px
            .unwrap_or([0, 0, image.width(), image.height()]);
    let inside = |start: u32, length: u32, limit: u32| {
        start.checked_add(length).is_some_and(|end| end <= limit)
    };
    if !inside(left, width, image.width()) || !inside(top, height, image.height()) {
        return Err(invalid(format!(
            "crop_px reaches outside the {} x {} pixel image",
            image.width(),
            image.height()
        )));
    }
    let linear: Vec<f64> = (0..=255u8)
        .map(|value| srgb_to_linear(f64::from(value) / 255.0))
        .collect();
    let contrast = settings.contrast;
    let view = image.view(left, top, width, height);
    let mut values = Vec::with_capacity(width as usize * height as usize);
    for (_, _, pixel) in view.pixels() {
        let [red, green, blue, alpha] = pixel.0;
        let colour = 0.2126 * linear[usize::from(red)]
            + 0.7152 * linear[usize::from(green)]
            + 0.0722 * linear[usize::from(blue)];
        let alpha = f64::from(alpha) / 255.0;
        let mut brightness = alpha * colour + (1.0 - alpha);
        if contrast != 1.0 {
            let shown = 0.5 + contrast * (linear_to_srgb(brightness) - 0.5);
            brightness = srgb_to_linear(shown);
        }
        values.push(brightness);
    }
    Ok(Grid::new(width as usize, height as usize, values)?)
}

/// Resample a brightness grid to `columns` x `rows` samples with a triangle
/// filter, which averages the pixels each sample covers when reducing.
fn resample(grid: &Grid<f64>, columns: usize, rows: usize) -> Result<Grid<f64>, WorkerError> {
    let source = image::ImageBuffer::<image::Luma<f32>, Vec<f32>>::from_raw(
        grid.width() as u32,
        grid.height() as u32,
        grid.values().iter().map(|value| *value as f32).collect(),
    )
    .expect("one value per pixel");
    let resized =
        image::imageops::resize(&source, columns as u32, rows as u32, FilterType::Triangle);
    Ok(Grid::new(
        columns,
        rows,
        resized
            .into_raw()
            .into_iter()
            .map(|value| f64::from(value).clamp(0.0, 1.0))
            .collect(),
    )?)
}

/// Build the lithophane the request describes and write its STL and
/// preview into `output`. Runs inside the geometry worker.
pub fn worker_run(request_path: &Path, output: &Path) -> Result<Value, WorkerError> {
    let bytes = fs::read(request_path).map_err(io_error("cannot read the lithophane request"))?;
    let request: WorkerRequest = serde_json::from_slice(&bytes).map_err(|_| WorkerError {
        code: "validation",
        message: "geometry worker received an invalid lithophane request".into(),
    })?;
    let settings = &request.settings;
    let tone = settings.tone();
    let brightness = request
        .image
        .as_deref()
        .map(|path| read_brightness(path, settings))
        .transpose()?;
    // The width over the height of the image, or of the test strip.
    let aspect = brightness.as_ref().map_or(STRIP_ASPECT, |grid| {
        grid.width() as f64 / grid.height() as f64
    });
    let (width_mm, height_mm) = match (settings.width_mm, settings.height_mm) {
        (Some(width), _) => (width, width / aspect),
        (None, Some(height)) => (height * aspect, height),
        (None, None) => return Err(invalid("give width_mm or height_mm".into())),
    };
    // Samples half a nozzle apart: the nozzle cannot draw finer detail.
    let target = settings.nozzle_mm / 2.0;
    let count = |length: f64| (length / target).round().max(1.0) as usize + 1;
    let (columns, rows) = (count(width_mm), count(height_mm));
    if columns.saturating_mul(rows) > DEFAULT_MAX_PIXELS {
        return Err(invalid(format!(
            "a {width_mm:.0} x {height_mm:.0} mm lithophane needs {} samples at half the nozzle's width, more than {DEFAULT_MAX_PIXELS}; make it smaller or use a larger nozzle_mm",
            columns * rows
        )));
    }
    let spacing_mm = [
        width_mm / (columns - 1) as f64,
        height_mm / (rows - 1) as f64,
    ];
    let (map, steps) = match &brightness {
        Some(grid) => (
            lithophane_map(&resample(grid, columns, rows)?, spacing_mm, &tone)?,
            None,
        ),
        None => {
            let (map, thicknesses) = step_wedge(columns, rows, spacing_mm, STRIP_STEPS, &tone)?;
            (map, Some(thicknesses))
        }
    };
    drop(brightness);

    let solid = lithophane_solid(&map, &tone, settings.border_mm)?;
    let mut checks = serde_json::to_value(check_relief(
        &map,
        &solid,
        &PrintabilityOptions {
            nozzle_mm: settings.nozzle_mm,
            layer_mm: settings.layer_mm,
            // An even image makes an even plate, which is still a lithophane.
            minimum_relief_mm: Some(0.0),
            maximum_relief_mm: None,
        },
    )?)
    .expect("checks serialize");
    let heights = map.heights_mm.values();
    let thinnest = heights.iter().copied().fold(f64::INFINITY, f64::min);
    // A border is as thick as the darkest parts may be.
    let thickest = heights
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(if settings.border_mm > 0.0 {
            settings.max_thickness_mm
        } else {
            f64::NEG_INFINITY
        });
    let within_range =
        thinnest >= settings.min_thickness_mm && thickest <= settings.max_thickness_mm;
    checks["thickness_within_range"] = json!(within_range);
    checks["passed"] = json!(checks["passed"] == true && within_range);
    if checks["passed"] != true {
        return Err(WorkerError {
            code: "relief_not_printable",
            message: format!(
                "the lithophane failed its printability check (solid: {}, thickness within range: {within_range})",
                checks["solid"]
            ),
        });
    }

    let mut mesh = solid.into_mesh();
    let shape = match settings.shape {
        LithophaneShape::Flat => json!({"kind": "flat"}),
        LithophaneShape::Curved => {
            let total_width = width_mm + 2.0 * settings.border_mm;
            let radius = match (settings.radius_mm, settings.arc_degrees) {
                (Some(radius), _) => radius,
                (None, Some(arc)) => total_width / arc.to_radians(),
                (None, None) => return Err(invalid("give radius_mm or arc_degrees".into())),
            };
            bend_around_cylinder(&mut mesh, radius)?;
            json!({
                "kind": "curved",
                "radius_mm": radius,
                "arc_degrees": total_width / radius * 180.0 / PI,
                "axis": "the Z axis; the flat face is outside, facing the viewer",
            })
        }
    };
    let stl = printable_geom::bust_stl(&mesh);
    if settings.shape == LithophaneShape::Curved {
        // Bending keeps the plate closed but can make wide flat triangles
        // cut through it, so the bent mesh, as the STL stores it, is checked
        // in full and replaces the flat plate's solid check.
        let bent = printable_geom::analyze_stl(&stl, ValidationOptions::default())
            .map_err(ReliefError::from)?;
        let self_intersections = bent.topology.self_intersections;
        checks["solid"] = json!(bent.solid_geometry);
        checks["volume_mm3"] = json!(bent.signed_volume_mm3);
        checks["self_intersections"] = json!(self_intersections);
        if !bent.solid_geometry {
            return Err(WorkerError {
                code: "relief_not_printable",
                message: format!(
                    "the curved lithophane is not a sound solid once bent (self-intersections: {}); use a larger radius_mm, a smaller arc_degrees or a narrower border_mm",
                    self_intersections.map_or("not counted".into(), |count| count.to_string())
                ),
            });
        }
    }
    write_preview(&map, &tone, &output.join(PREVIEW_FILE))?;
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in &mesh.vertices {
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    fs::write(output.join(STL_FILE), stl).map_err(io_error("cannot write the lithophane STL"))?;
    let darkest = tone.transmission(settings.max_thickness_mm);
    let brightest = tone.transmission(settings.min_thickness_mm);
    let mut report = json!({
        "size_mm": [high[0] - low[0], high[1] - low[1], high[2] - low[2]],
        "image_mm": [width_mm, height_mm],
        "triangles": mesh.triangles.len(),
        "samples": [columns, rows],
        "sample_spacing_mm": spacing_mm,
        "thickness_mm": {"min": thinnest, "max": thickest},
        "shape": shape,
        "tone_mapping": {
            "brightness": "linear light from sRGB, Y = 0.2126 R + 0.7152 G + 0.0722 B",
            "transmission": "exp(-attenuation_per_mm * thickness_mm)",
            "thickness": "-ln(T_dark + Y * (T_bright - T_dark)) / attenuation_per_mm",
            "attenuation_per_mm": settings.attenuation_per_mm,
            "attenuation_source": match settings.filament {
                Some(_) => "the filament's starting value, not measured on prints",
                None => "measured by the caller",
            },
            "transmission_range": [darkest, brightest],
            "contrast_ratio": tone.contrast_ratio(),
        },
        "checks": checks,
        "preview": "8-bit sRGB brightness against an even backlight, relative to the thinnest plate, as seen from the flat face; the border is not shown",
    });
    if let Some(thicknesses) = steps {
        report["test_strip"] = json!({
            "order": "thinnest at the left, seen from the flat face",
            "steps": thicknesses.iter().map(|thickness| json!({
                "thickness_mm": thickness,
                "predicted_brightness": tone.backlit(*thickness),
            })).collect::<Vec<_>>(),
        });
    }
    Ok(report)
}

/// An 8-bit greyscale PNG of how the plate looks against an even backlight.
fn write_preview(map: &ReliefMap, tone: &ToneMapping, path: &Path) -> Result<(), WorkerError> {
    let grid = &map.heights_mm;
    let values: Vec<u8> = grid
        .values()
        .iter()
        .map(|thickness| (linear_to_srgb(tone.backlit(*thickness)) * 255.0).round() as u8)
        .collect();
    image::GrayImage::from_raw(grid.width() as u32, grid.height() as u32, values)
        .expect("one value per sample")
        .save_with_format(path, ImageFormat::Png)
        .map_err(|_| WorkerError {
            code: "geometry_worker_io",
            message: "cannot write the lithophane preview".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LithophaneRequest {
        LithophaneRequest {
            image: Some("photos/cat.jpg".into()),
            test_strip: false,
            path: "lithophanes/cat.stl".into(),
            overwrite: false,
            shape: LithophaneShape::Flat,
            width_mm: Some(100.0),
            height_mm: None,
            radius_mm: None,
            arc_degrees: None,
            min_thickness_mm: None,
            max_thickness_mm: None,
            filament: None,
            attenuation_per_mm: None,
            border_mm: None,
            crop_px: None,
            contrast: None,
            nozzle_mm: None,
        }
    }

    #[test]
    fn defaults_are_the_documented_starting_values() {
        let settings = request().settings().unwrap();
        assert_eq!(
            (
                settings.min_thickness_mm,
                settings.max_thickness_mm,
                settings.filament,
                settings.attenuation_per_mm,
                settings.border_mm,
                settings.contrast,
                settings.nozzle_mm,
                settings.arc_degrees,
            ),
            (0.8, 3.0, Some(Filament::WhitePla), 1.5, 0.0, 1.0, 0.4, None)
        );
        let mut curved = request();
        curved.shape = LithophaneShape::Curved;
        assert_eq!(curved.settings().unwrap().arc_degrees, Some(120.0));
        curved.radius_mm = Some(60.0);
        let settings = curved.settings().unwrap();
        assert_eq!(
            (settings.radius_mm, settings.arc_degrees),
            (Some(60.0), None)
        );
        let mut measured = request();
        measured.attenuation_per_mm = Some(2.0);
        let settings = measured.settings().unwrap();
        assert_eq!(
            (settings.filament, settings.attenuation_per_mm),
            (None, 2.0)
        );
        let mut strip = request();
        strip.image = None;
        strip.test_strip = true;
        assert!(strip.settings().unwrap().test_strip);
        let mut upper = request();
        upper.image = Some("photos/CAT.JPG".into());
        upper.border_mm = Some(0.4);
        assert_eq!(upper.settings().unwrap().border_mm, 0.4);
    }

    #[test]
    fn invalid_requests_are_refused() {
        let mut cases = Vec::new();
        let mut both = request();
        both.test_strip = true;
        cases.push(both);
        let mut neither = request();
        neither.image = None;
        cases.push(neither);
        let mut gif = request();
        gif.image = Some("photos/cat.gif".into());
        cases.push(gif);
        let mut path = request();
        path.path = "lithophanes/cat.obj".into();
        cases.push(path);
        let mut sizes = request();
        sizes.height_mm = Some(50.0);
        cases.push(sizes);
        let mut no_size = request();
        no_size.width_mm = None;
        cases.push(no_size);
        let mut flat_radius = request();
        flat_radius.radius_mm = Some(50.0);
        cases.push(flat_radius);
        let mut curved_both = request();
        curved_both.shape = LithophaneShape::Curved;
        curved_both.radius_mm = Some(50.0);
        curved_both.arc_degrees = Some(90.0);
        cases.push(curved_both);
        let mut full_circle = request();
        full_circle.shape = LithophaneShape::Curved;
        full_circle.arc_degrees = Some(360.0);
        cases.push(full_circle);
        let mut thin_range = request();
        thin_range.min_thickness_mm = Some(2.0);
        thin_range.max_thickness_mm = Some(2.1);
        cases.push(thin_range);
        let mut two_attenuations = request();
        two_attenuations.filament = Some(Filament::NaturalPla);
        two_attenuations.attenuation_per_mm = Some(1.0);
        cases.push(two_attenuations);
        let mut tiny_crop = request();
        tiny_crop.crop_px = Some([0, 0, 1, 10]);
        cases.push(tiny_crop);
        let mut strip_crop = request();
        strip_crop.image = None;
        strip_crop.test_strip = true;
        strip_crop.crop_px = Some([0, 0, 10, 10]);
        cases.push(strip_crop);
        let mut nozzle = request();
        nozzle.nozzle_mm = Some(0.1);
        cases.push(nozzle);
        // A border narrower than the nozzle cannot print.
        let mut hairline = request();
        hairline.border_mm = Some(1e-8);
        cases.push(hairline);
        let mut narrow = request();
        narrow.border_mm = Some(0.3);
        cases.push(narrow);
        for case in cases {
            assert_eq!(
                case.settings().unwrap_err().code(),
                "validation",
                "{case:?}"
            );
        }
    }

    #[test]
    fn companions_sit_beside_the_stl() {
        assert_eq!(
            companion_paths("lithophanes/cat.stl"),
            (
                "lithophanes/cat.preview.png".to_owned(),
                "lithophanes/cat.report.json".to_owned()
            )
        );
    }
}
