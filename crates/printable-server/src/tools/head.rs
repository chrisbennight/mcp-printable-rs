//! The `head` workflow: FLAME heads evaluated in the server and delivered to
//! Blender as millimetre meshes.

use super::*;

use printable_flame::{FlameModel, FlameParams, Pose, PosedHead};

/// Coefficients are in standard deviations of the heads FLAME was learned
/// from; well beyond this the model extrapolates into implausible shapes.
const COEFFICIENT_LIMIT: f32 = 5.0;
/// FLAME evaluates in metres; Printable's scene coordinates are millimetres.
const MILLIMETRES_PER_METRE: f32 = 1000.0;
const DEFAULT_NAME: &str = "Head";

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
