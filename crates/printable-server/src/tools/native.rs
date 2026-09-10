use super::*;

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CaptureMethod {
    Viewport,
    Editor,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum ViewAxis {
    Front,
    Back,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum Perspective {
    Persp,
    Ortho,
    Camera,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum Shading {
    Wireframe,
    Solid,
    Material,
    Rendered,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ViewOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    axis: Option<ViewAxis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<[f64; 3]>,
    /// Blender quaternion components in w, x, y, z order.
    #[serde(skip_serializing_if = "Option::is_none")]
    rotation: Option<[f64; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.000001, max = 1000000000000_f64))]
    distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    perspective: Option<Perspective>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shading: Option<Shading>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlays: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    xray: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeViewParams {
    /// Optional PNG destination; omission retains a new observation artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<EditorContextParams>,
    /// Viewport draws natively; editor captures actual editor pixels. No fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<CaptureMethod>,
    /// Optional persistent viewport configuration; does not edit model geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    view: Option<ViewOptions>,
    /// Maximum returned image dimension, preserving aspect ratio.
    #[serde(default = "default_max_size")]
    #[schemars(range(min = 64, max = 2048))]
    max_size: u16,
    #[serde(default = "default_capture_timeout")]
    #[schemars(range(min = 0.1, max = 120))]
    timeout_seconds: f64,
    #[serde(default = "default_true")]
    include_inline: bool,
}

fn default_max_size() -> u16 {
    1024
}
fn default_capture_timeout() -> f64 {
    30.0
}

pub(super) async fn capture(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    mut params: NativeViewParams,
) -> Result<ToolOutput, ToolError> {
    if !(64..=2048).contains(&params.max_size)
        || !params.timeout_seconds.is_finite()
        || !(0.1..=120.0).contains(&params.timeout_seconds)
    {
        return Err(ToolError::Validation(
            "native capture size or timeout is outside its supported range".into(),
        ));
    }
    if params.path.is_none() {
        params.path = Some(format!(
            "observations/{}.png",
            crate::upload::random_hex_id()?
        ));
    }
    let path = params.path.as_ref().expect("capture path assigned").clone();
    validate_blender_artifact(&workspace, &path, ".png", false)?;
    let requested_method = match params.method {
        Some(CaptureMethod::Editor) => "editor",
        _ => "viewport",
    };
    let arguments = serde_json::to_value(&params)?;
    let mut arguments = arguments
        .as_object()
        .cloned()
        .expect("typed capture parameters");
    arguments.remove("include_inline");
    let mut value = blender
        .send_value_with_work_budget(
            "capture_native_view",
            arguments,
            validate_work_budget(params.timeout_seconds)?,
        )
        .await?;
    let width = value["width"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= u64::from(params.max_size));
    let height = value["height"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= u64::from(params.max_size));
    let scene: Option<printable_blender::SceneSnapshot> =
        serde_json::from_value(value["scene_state"].clone()).ok();
    let scene_matches = scene.as_ref().is_some_and(|scene| {
        scene.generation.len() == 36
            && scene.revision <= 9007199254740991
            && params.expected_scene.as_ref().is_none_or(|expected| {
                expected.generation == scene.generation && expected.revision == scene.revision
            })
    });
    let digest_valid = value["view_state"]["configuration_sha256"]
        .as_str()
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    let editor = requested_method == "editor";
    if value["path"] != path
        || value["method"] != requested_method
        || value["media_type"] != "image/png"
        || value["capture_source"]
            != if editor {
                "private_display"
            } else {
                "gpu_offscreen"
            }
        || width.is_none()
        || height.is_none()
        || value["freshness"]["dependency_evaluated"] != true
        || value["freshness"]["redraw"]
            != if editor {
                "window_draw_swap"
            } else {
                "offscreen_draw_view3d"
            }
        || value["fidelity"]
            != if editor {
                "editor_pixels"
            } else {
                "native_viewport_draw"
            }
        || value["overlay_fidelity"]
            != if editor {
                "editor_pixels"
            } else {
                "not_guaranteed"
            }
        || !matches!(
            value["convergence"].as_str(),
            Some("not_progressive" | "not_reported")
        )
        || !value["view_configuration"].is_object()
        || value["captured_at_unix_ms"].as_u64().is_none()
        || value["elapsed_ms"].as_u64().is_none()
        || !scene_matches
        || !digest_valid
    {
        return Err(ToolError::Validation(
            "native capture response does not match the requested observation".into(),
        ));
    }
    let bytes = verify_product_png_artifact(
        Arc::clone(&workspace),
        path.clone(),
        &value,
        width.expect("validated width"),
        height.expect("validated height"),
    )
    .await?;
    let metadata_path = format!("{path}.json");
    value["metadata_path"] = json!(metadata_path);
    let metadata = serde_json::to_vec(&value)?;
    blocking(move || workspace.write_artifact(&metadata_path, &metadata, true)).await?;
    let (inline, inline_png_base64) = capture_inline_png(params.include_inline, &bytes);
    value["inline"] = inline;
    Ok(ToolOutput {
        value,
        inline_png_base64,
    })
}
