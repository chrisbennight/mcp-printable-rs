//! The `head` workflow: FLAME heads evaluated in the server and delivered to
//! Blender as millimetre meshes, or prepared as printable busts by the
//! isolated geometry worker.

use super::*;

use printable_flame::{FlameModel, FlameParams, Pose, PosedHead};
use printable_geom::BustRequest;

/// Coefficients are in standard deviations of the heads FLAME was learned
/// from; well beyond this the model extrapolates into implausible shapes.
const COEFFICIENT_LIMIT: f32 = 5.0;
/// FLAME evaluates in metres; Printable's scene coordinates are millimetres.
const MILLIMETRES_PER_METRE: f32 = 1000.0;
const DEFAULT_NAME: &str = "Head";
const PRINT_HEIGHT_MM: std::ops::RangeInclusive<f64> = 20.0..=500.0;
const DEFAULT_PRINT_HEIGHT_MM: f64 = 100.0;
/// An edge of 0.4 mm keeps facets within about 0.02 mm of the smooth surface
/// on a 100 mm bust, below what a 0.2 mm nozzle reproduces.
const EDGE_LENGTH_MM: std::ops::RangeInclusive<f64> = 0.1..=5.0;
const DEFAULT_EDGE_LENGTH_MM: f64 = 0.4;
const MAX_TRIANGLES: std::ops::RangeInclusive<usize> = 10_000..=3_000_000;
const DEFAULT_MAX_TRIANGLES: usize = 1_500_000;

/// Joint rotations as axis-angle vectors in radians. The direction is the
/// rotation axis and the length, at most pi, is the angle. FLAME's frame has
/// y up and the face looking along +z.
#[derive(Clone, Copy, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadPose {
    /// Whole-head rotation about the base of the neck.
    #[serde(default)]
    global: [f32; 3],
    #[serde(default)]
    neck: [f32; 3],
    /// A positive rotation about +x opens the mouth.
    #[serde(default)]
    jaw: [f32; 3],
    /// The subject's left eye, on the +x side.
    #[serde(default)]
    left_eye: [f32; 3],
    #[serde(default)]
    right_eye: [f32; 3],
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadModelsParams {}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadCreateParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Model variant from the `models` action. Optional when one model is mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    /// Identity coefficients in standard deviations, in basis order, each
    /// within ±5. Omitted trailing coefficients are zero.
    #[serde(default)]
    identity: Vec<f32>,
    /// Expression coefficients in standard deviations, in basis order, each
    /// within ±5. Omitted trailing coefficients are zero.
    #[serde(default)]
    expression: Vec<f32>,
    #[serde(default)]
    pose: HeadPose,
    /// Unique name for the new object (default `Head`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadUpdateParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Head object previously made by `create`.
    object: String,
    /// Model variant the object was created from. Optional when one model is mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    /// Replacement identity coefficients, as for `create`.
    #[serde(default)]
    identity: Vec<f32>,
    /// Replacement expression coefficients, as for `create`.
    #[serde(default)]
    expression: Vec<f32>,
    #[serde(default)]
    pose: HeadPose,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadPreparePrintParams {
    /// Model variant from the `models` action. Optional when one model is mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    /// Identity coefficients, as for `create`.
    #[serde(default)]
    identity: Vec<f32>,
    /// Expression coefficients, as for `create`.
    #[serde(default)]
    expression: Vec<f32>,
    #[serde(default)]
    pose: HeadPose,
    /// Workspace path for the binary STL.
    path: String,
    /// Replace an existing file at `path`.
    #[serde(default)]
    overwrite: bool,
    /// Height of the bust from its flat base to the crown, 20 to 500 mm
    /// (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    height_mm: Option<f64>,
    /// Target facet edge length, 0.1 to 5 mm (default 0.4). Shorter edges
    /// follow the smooth surface more closely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_length_mm: Option<f64>,
    /// Upper bound on the triangle count, 10,000 to 3,000,000 (default
    /// 1,500,000). The edge length grows when needed to respect it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_triangles: Option<usize>,
}

#[derive(serde::Serialize)]
struct CreateCommand<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<&'a SceneExpectation>,
    name: &'a str,
    variant: &'a str,
    vertices: Vec<[f32; 3]>,
    faces: &'a [[u32; 3]],
}

#[derive(serde::Serialize)]
struct UpdateCommand<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<&'a SceneExpectation>,
    object: &'a str,
    variant: &'a str,
    vertices: Vec<[f32; 3]>,
}

pub(super) fn models(settings: &Settings) -> Result<Value, ToolError> {
    Ok(library(settings)?.report())
}

pub(super) async fn create(
    blender: &BlenderClient,
    settings: &Settings,
    params: HeadCreateParams,
) -> Result<Value, ToolError> {
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    let name = params.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let command = CreateCommand {
        expected_scene: params.expected_scene.as_ref(),
        name: &name,
        variant: &model.info().variant,
        vertices: millimetres(&head.vertices),
        faces: model.faces(),
    };
    let result = blender_command(blender, "create_flame_head", &command).await?;
    Ok(annotate(result, &model, &head))
}

pub(super) async fn update(
    blender: &BlenderClient,
    settings: &Settings,
    params: HeadUpdateParams,
) -> Result<Value, ToolError> {
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    let command = UpdateCommand {
        expected_scene: params.expected_scene.as_ref(),
        object: &params.object,
        variant: &model.info().variant,
        vertices: millimetres(&head.vertices),
    };
    let result = blender_command(blender, "update_flame_head", &command).await?;
    Ok(annotate(result, &model, &head))
}

/// Write a closed, upright bust STL: the head is trimmed flat just above the
/// neck opening, the mouth is closed, the eyes are united with the head, and
/// the surface is smoothed and refined to the requested edge length.
pub(super) async fn prepare_print(
    workspace: Arc<Workspace>,
    settings: &Settings,
    params: HeadPreparePrintParams,
) -> Result<Value, ToolError> {
    validate_scad_destination(&workspace, &params.path, ".stl", params.overwrite)?;
    let height_mm = in_range(
        "height_mm",
        params.height_mm.unwrap_or(DEFAULT_PRINT_HEIGHT_MM),
        &PRINT_HEIGHT_MM,
    )?;
    let edge_length_mm = in_range(
        "edge_length_mm",
        params.edge_length_mm.unwrap_or(DEFAULT_EDGE_LENGTH_MM),
        &EDGE_LENGTH_MM,
    )?;
    let max_triangles = in_range(
        "max_triangles",
        params.max_triangles.unwrap_or(DEFAULT_MAX_TRIANGLES),
        &MAX_TRIANGLES,
    )?;
    let worker_bin = geometry_worker_path(settings)?;
    let worker_memory_bytes = settings.geometry_worker_memory_bytes;
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    let request = BustRequest {
        vertices: millimetres(&head.vertices)
            .into_iter()
            .map(|point| point.map(f64::from))
            .collect(),
        triangles: model.faces().to_vec(),
        height_mm,
        edge_length_mm,
        max_triangles,
    };
    let variant = model.info().variant.clone();
    let (path, overwrite) = (params.path, params.overwrite);
    geometry_blocking(move || {
        let staging = tempfile::Builder::new()
            .prefix("printable-bust-")
            .tempdir()?;
        let request_path = staging.path().join("request.json");
        let output_path = staging.path().join("bust.stl");
        std::fs::write(&request_path, serde_json::to_vec(&request)?)?;
        let report = run_geometry_worker_process(
            &worker_bin,
            worker_memory_bytes,
            &[
                std::ffi::OsStr::new("--bust"),
                request_path.as_os_str(),
                output_path.as_os_str(),
            ],
        )?;
        let bytes = std::fs::read(&output_path)?;
        let validation = analyze_stl(&bytes, ValidationOptions::default())?;
        let artifact = workspace.write_generated_bytes(&path, &bytes, overwrite)?;
        Ok::<_, ToolError>(json!({
            "artifact": artifact,
            "units": "millimetres",
            "variant": variant,
            "bust": report,
            "validation": validation,
        }))
    })
    .await
}

fn in_range<T: PartialOrd + std::fmt::Display>(
    name: &str,
    value: T,
    range: &std::ops::RangeInclusive<T>,
) -> Result<T, ToolError> {
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

fn library(settings: &Settings) -> Result<&crate::flame::FlameLibrary, ToolError> {
    settings
        .flame
        .as_deref()
        .filter(|library| library.is_available())
        .ok_or_else(|| ToolError::Validation("no FLAME head models are mounted".into()))
}

/// Validate every input before evaluating, so nothing reaches Blender unless
/// the whole request is acceptable.
async fn evaluate(
    settings: &Settings,
    variant: Option<&str>,
    identity: Vec<f32>,
    expression: Vec<f32>,
    pose: HeadPose,
) -> Result<(Arc<FlameModel>, PosedHead), ToolError> {
    let model = Arc::clone(
        library(settings)?
            .select(variant)
            .map_err(ToolError::Validation)?,
    );
    check_coefficients("identity", &identity)?;
    check_coefficients("expression", &expression)?;
    let pose = Pose {
        global: check_rotation("global", pose.global)?,
        neck: check_rotation("neck", pose.neck)?,
        jaw: check_rotation("jaw", pose.jaw)?,
        left_eye: check_rotation("left_eye", pose.left_eye)?,
        right_eye: check_rotation("right_eye", pose.right_eye)?,
    };
    let params = FlameParams {
        identity,
        expression,
        pose,
        translation: [0.0; 3],
    };
    let evaluated = Arc::clone(&model);
    let head = blocking(move || {
        evaluated
            .evaluate(&params)
            .map_err(|error| ToolError::Validation(error.to_string()))
    })
    .await?;
    Ok((model, head))
}

fn check_coefficients(kind: &str, values: &[f32]) -> Result<(), ToolError> {
    if values
        .iter()
        .all(|value| value.is_finite() && value.abs() <= COEFFICIENT_LIMIT)
    {
        Ok(())
    } else {
        Err(ToolError::Validation(format!(
            "{kind} coefficients must be finite and within ±{COEFFICIENT_LIMIT}"
        )))
    }
}

fn check_rotation(joint: &str, rotation: [f32; 3]) -> Result<[f32; 3], ToolError> {
    let angle = rotation
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if angle.is_finite() && angle <= std::f32::consts::PI {
        Ok(rotation)
    } else {
        Err(ToolError::Validation(format!(
            "pose.{joint} must be a finite rotation of at most pi radians"
        )))
    }
}

fn millimetres(points: &[[f32; 3]]) -> Vec<[f32; 3]> {
    points
        .iter()
        .map(|point| point.map(|coordinate| coordinate * MILLIMETRES_PER_METRE))
        .collect()
}

fn annotate(mut result: Value, model: &FlameModel, head: &PosedHead) -> Value {
    if let Some(object) = result.as_object_mut() {
        let [root, neck, jaw, left_eye, right_eye] = head
            .joints
            .map(|joint| joint.map(|coordinate| coordinate * MILLIMETRES_PER_METRE));
        object.insert(
            "head".into(),
            json!({
                "variant": model.info().variant,
                "units": "millimetres",
                "joints": {
                    "root": root, "neck": neck, "jaw": jaw,
                    "left_eye": left_eye, "right_eye": right_eye,
                },
            }),
        );
    }
    result
}
