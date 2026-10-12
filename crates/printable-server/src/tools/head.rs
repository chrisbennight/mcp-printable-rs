//! The `head` workflow: FLAME heads evaluated in the server and delivered to
//! Blender as millimetre meshes, or prepared as printable busts by the
//! isolated geometry worker.

use super::*;

use printable_flame::{FlameModel, FlameParams, Pose, PosedHead, texture};
use printable_geom::{BustRequest, Displacement, Hollow, Plinth};

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
/// About the most the geometry worker can prepare within its default 1 GiB
/// memory limit.
const DEFAULT_MAX_TRIANGLES: usize = 750_000;
const PLINTH_HEIGHT_MM: std::ops::RangeInclusive<f64> = 2.0..=100.0;
const PLINTH_DIAMETER_MM: std::ops::RangeInclusive<f64> = 10.0..=500.0;
/// The bust above a plinth is at least this tall.
const MIN_BUST_ABOVE_PLINTH_MM: f64 = 10.0;
const HOLLOW_WALL_MM: std::ops::RangeInclusive<f64> = 1.0..=20.0;
const DRAIN_DIAMETER_MM: std::ops::RangeInclusive<f64> = 1.0..=10.0;
const DEFAULT_DRAIN_DIAMETER_MM: f64 = 3.5;
/// Largest displacement map accepted, in pixels on each side.
const MAX_DISPLACEMENT_PIXELS: u32 = 1024;

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
    /// Workspace path of a PNG colour map in FLAME's texture layout, such as
    /// one made from photographs. The head shows it through a material.
    /// Needs a model converted with its texture layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    texture: Option<String>,
    /// How the head looks in Blender. Every field is optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    appearance: Option<HeadAppearance>,
}

/// How a head looks in Blender. Heads are shaded smooth; the materials
/// apply when the head has a colour map.
#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadAppearance {
    /// Subdivision levels, applied as a modifier so the mesh keeps FLAME's
    /// vertices, 0 to 2 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subdivision: Option<u8>,
    /// Close the neck opening with one face that adds no vertices (default
    /// true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    close_neck: Option<bool>,
    /// Skin roughness, 0 to 1 (default 0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skin_roughness: Option<f64>,
    /// Weight of light scattering beneath the skin, 0 to 1 (default 0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skin_subsurface: Option<f64>,
    /// How far light scatters beneath the skin, in millimetres, 0 to 100
    /// (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subsurface_scale_mm: Option<f64>,
    /// Eyeball roughness, 0 to 1 (default 0.05). The eyeballs are the parts
    /// of the mesh separate from the head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eye_roughness: Option<f64>,
    /// Show a displacement map as bump detail, so the Blender head matches
    /// a printed bust. Needs `texture`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bump: Option<HeadBump>,
}

/// A greyscale PNG displacement map in FLAME's texture layout, such as one
/// estimated from photographs, shown as bump.
#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadBump {
    /// Workspace path of the PNG.
    path: String,
    /// Millimetres between the map's lowest and highest values, 0 to 100:
    /// the map's `scale_mm`.
    scale_mm: f64,
    /// Exaggerate or soften the detail, 0 to 10 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strength: Option<f64>,
}

impl HeadAppearance {
    fn validate(&self) -> Result<(), ToolError> {
        if let Some(subdivision) = self.subdivision {
            in_range("appearance.subdivision", subdivision, &(0..=2))?;
        }
        for (name, value, range) in [
            (
                "appearance.skin_roughness",
                self.skin_roughness,
                &(0.0..=1.0),
            ),
            (
                "appearance.skin_subsurface",
                self.skin_subsurface,
                &(0.0..=1.0),
            ),
            (
                "appearance.subsurface_scale_mm",
                self.subsurface_scale_mm,
                &(0.0..=100.0),
            ),
            ("appearance.eye_roughness", self.eye_roughness, &(0.0..=1.0)),
            (
                "appearance.bump.scale_mm",
                self.bump.as_ref().map(|bump| bump.scale_mm),
                &(0.0..=100.0),
            ),
            (
                "appearance.bump.strength",
                self.bump.as_ref().and_then(|bump| bump.strength),
                &(0.0..=10.0),
            ),
        ] {
            if let Some(value) = value {
                in_range(name, value, range)?;
            }
        }
        Ok(())
    }
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
    /// Workspace path of a PNG colour map that replaces the head's own, as for
    /// `create`. The head must have been created from a model converted with
    /// its texture layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    texture: Option<String>,
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
    /// Joint rotations, as for `create`. `global` must be zero: the bust
    /// stands on its neck, so turn or tilt the head with `neck`.
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
    /// 750,000). The edge length grows when needed to respect it. Limits above
    /// the default need a larger geometry worker memory limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_triangles: Option<usize>,
    /// Height of a cylindrical plinth under the bust, 2 to 100 mm, counted
    /// within `height_mm`. No plinth when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_height_mm: Option<f64>,
    /// Plinth diameter, 10 to 500 mm. Defaults to 70% of the bust's widest
    /// extent and never less than the neck section. Requires
    /// `plinth_height_mm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_diameter_mm: Option<f64>,
    /// Hollow the bust for resin printing, leaving walls this thick, 1 to
    /// 20 mm. Drain holes open through the base. Solid when omitted, which
    /// suits FDM printing with infill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hollow_wall_mm: Option<f64>,
    /// Drain hole diameter, 1 to 10 mm (default 3.5). Requires
    /// `hollow_wall_mm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drain_hole_diameter_mm: Option<f64>,
    /// Fine surface detail from a displacement map in FLAME's texture layout,
    /// such as one estimated from photographs. Needs a model converted with
    /// its texture layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    displacement: Option<DisplacementParams>,
}

/// A greyscale PNG displacement map. A pixel at fraction f of the full
/// range, p / 65535 for 16-bit or p / 255 for 8-bit images, raises the
/// surface by `offset_mm + scale_mm × f` millimetres at life size, scaled
/// with the bust. Row 0 is v = 1 and column 0 is u = 0.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DisplacementParams {
    /// Workspace path of the PNG, at most 1024 pixels on each side.
    path: String,
    offset_mm: f64,
    scale_mm: f64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadExportParams {
    /// Model variant from the `models` action. Optional when one model is mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    /// Identity coefficients, as for `create`.
    #[serde(default)]
    identity: Vec<f32>,
    /// Expression coefficients, as for `create`.
    #[serde(default)]
    expression: Vec<f32>,
    /// Joint rotations, as for `create`.
    #[serde(default)]
    pose: HeadPose,
    /// Workspace path for the Wavefront OBJ file.
    path: String,
    /// Replace an existing file at `path`.
    #[serde(default)]
    overwrite: bool,
}

#[derive(serde::Serialize)]
struct CreateCommand<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<&'a SceneExpectation>,
    name: &'a str,
    variant: &'a str,
    vertices: Vec<[f32; 3]>,
    faces: &'a [[u32; 3]],
    /// Texture coordinates of every triangle corner, in triangle order.
    #[serde(skip_serializing_if = "Option::is_none")]
    uv: Option<Vec<[f32; 2]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    texture: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    appearance: Option<&'a HeadAppearance>,
}

#[derive(serde::Serialize)]
struct UpdateCommand<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<&'a SceneExpectation>,
    object: &'a str,
    variant: &'a str,
    vertices: Vec<[f32; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    texture: Option<&'a str>,
}

pub(super) fn models(settings: &Settings) -> Result<Value, ToolError> {
    Ok(library(settings)?.report())
}

pub(super) async fn create(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    settings: &Settings,
    params: HeadCreateParams,
) -> Result<Value, ToolError> {
    if let Some(texture) = &params.texture {
        check_colour_map(&workspace, texture).await?;
    }
    if let Some(appearance) = &params.appearance {
        appearance.validate()?;
        if let Some(bump) = &appearance.bump {
            if params.texture.is_none() {
                return Err(ToolError::Validation(
                    "appearance.bump needs a texture".into(),
                ));
            }
            check_colour_map(&workspace, &bump.path).await?;
        }
    }
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    let name = params.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let uv = model.uv_layout().map(|layout| {
        layout
            .faces
            .iter()
            .flat_map(|corners| corners.map(|index| layout.coords[index as usize]))
            .collect()
    });
    if params.texture.is_some() && uv.is_none() {
        return Err(no_layout_for_colour(&model));
    }
    let command = CreateCommand {
        expected_scene: params.expected_scene.as_ref(),
        name: &name,
        variant: &model.info().variant,
        vertices: millimetres(&head.vertices),
        faces: model.faces(),
        uv,
        texture: params.texture.as_deref(),
        appearance: params.appearance.as_ref(),
    };
    let result = blender_command(blender, "create_flame_head", &command).await?;
    Ok(annotate(result, &model, &head))
}

pub(super) async fn update(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    settings: &Settings,
    params: HeadUpdateParams,
) -> Result<Value, ToolError> {
    if let Some(texture) = &params.texture {
        check_colour_map(&workspace, texture).await?;
    }
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    if params.texture.is_some() && model.uv_layout().is_none() {
        return Err(no_layout_for_colour(&model));
    }
    let command = UpdateCommand {
        expected_scene: params.expected_scene.as_ref(),
        object: &params.object,
        variant: &model.info().variant,
        vertices: millimetres(&head.vertices),
        texture: params.texture.as_deref(),
    };
    let result = blender_command(blender, "update_flame_head", &command).await?;
    Ok(annotate(result, &model, &head))
}

/// Write the evaluated head to the workspace as a Wavefront OBJ in
/// millimetres, in the same frame as `create`, with FLAME's texture
/// coordinates when the model was converted with its texture layout.
pub(super) async fn export(
    workspace: Arc<Workspace>,
    settings: &Settings,
    params: HeadExportParams,
) -> Result<Value, ToolError> {
    validate_scad_destination(&workspace, &params.path, ".obj", params.overwrite)?;
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        params.identity,
        params.expression,
        params.pose,
    )
    .await?;
    let vertices = millimetres(&head.vertices);
    let bytes = head_obj(&model, &vertices);
    let (path, overwrite) = (params.path, params.overwrite);
    let artifact =
        blocking(move || workspace.write_generated_bytes(&path, &bytes, overwrite)).await?;
    let result = json!({
        "artifact": artifact,
        "units": "millimetres",
        "variant": model.info().variant,
        "vertices": vertices.len(),
        "triangles": model.faces().len(),
        "texture_coordinates": model.uv_layout().is_some(),
    });
    Ok(annotate(result, &model, &head))
}

/// The head as Wavefront OBJ text: vertices, then texture coordinates when the
/// model has them, then triangles indexing both.
fn head_obj(model: &FlameModel, vertices: &[[f32; 3]]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut text = String::new();
    let mut line = |args: std::fmt::Arguments<'_>| {
        text.write_fmt(args)
            .expect("writing to a string cannot fail");
        text.push('\n');
    };
    line(format_args!(
        "# FLAME head ({}) in millimetres; y is up and the face looks along +z",
        model.info().variant
    ));
    for [x, y, z] in vertices {
        line(format_args!("v {x} {y} {z}"));
    }
    match model.uv_layout() {
        Some(layout) => {
            for [u, v] in &layout.coords {
                line(format_args!("vt {u} {v}"));
            }
            for (face, corners) in model.faces().iter().zip(&layout.faces) {
                let [a, b, c] = face.map(|index| index + 1);
                let [ta, tb, tc] = corners.map(|index| index + 1);
                line(format_args!("f {a}/{ta} {b}/{tb} {c}/{tc}"));
            }
        }
        None => {
            for face in model.faces() {
                let [a, b, c] = face.map(|index| index + 1);
                line(format_args!("f {a} {b} {c}"));
            }
        }
    }
    text.into_bytes()
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
    let plinth = match (params.plinth_height_mm, params.plinth_diameter_mm) {
        (None, None) => None,
        (None, Some(_)) => {
            return Err(ToolError::Validation(
                "plinth_diameter_mm requires plinth_height_mm".into(),
            ));
        }
        (Some(height), diameter) => Some(Plinth {
            height_mm: in_range("plinth_height_mm", height, &PLINTH_HEIGHT_MM)?,
            diameter_mm: diameter
                .map(|diameter| in_range("plinth_diameter_mm", diameter, &PLINTH_DIAMETER_MM))
                .transpose()?,
        }),
    };
    if let Some(plinth) = plinth
        && height_mm - plinth.height_mm < MIN_BUST_ABOVE_PLINTH_MM
    {
        return Err(ToolError::Validation(format!(
            "height_mm must exceed plinth_height_mm by at least {MIN_BUST_ABOVE_PLINTH_MM} mm"
        )));
    }
    let hollow = match (params.hollow_wall_mm, params.drain_hole_diameter_mm) {
        (None, None) => None,
        (None, Some(_)) => {
            return Err(ToolError::Validation(
                "drain_hole_diameter_mm requires hollow_wall_mm".into(),
            ));
        }
        (Some(wall), diameter) => Some(Hollow {
            wall_mm: in_range("hollow_wall_mm", wall, &HOLLOW_WALL_MM)?,
            drain_diameter_mm: in_range(
                "drain_hole_diameter_mm",
                diameter.unwrap_or(DEFAULT_DRAIN_DIAMETER_MM),
                &DRAIN_DIAMETER_MM,
            )?,
        }),
    };
    if params.pose.global != [0.0; 3] {
        return Err(ToolError::Validation(
            "pose.global must be zero for prepare_print: the bust stands on its neck; use pose.neck to turn or tilt the head".into(),
        ));
    }
    if let Some(map) = &params.displacement
        && !(map.offset_mm.is_finite() && map.scale_mm.is_finite())
    {
        return Err(ToolError::Validation(
            "displacement offset_mm and scale_mm must be finite".into(),
        ));
    }
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
    let displacement = match params.displacement {
        Some(map) => Some(displacement(&workspace, &model, map).await?),
        None => None,
    };
    let request = BustRequest {
        vertices: millimetres(&head.vertices)
            .into_iter()
            .map(|point| point.map(f64::from))
            .collect(),
        triangles: model.faces().to_vec(),
        height_mm,
        edge_length_mm,
        max_triangles,
        plinth,
        hollow,
        displacement,
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
        // The bust is written only when it is known to be free of
        // self-intersections, not merely when none were found.
        if !validation.solid_geometry || validation.topology.self_intersections != Some(0) {
            let issues: Vec<_> = validation
                .issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect();
            return Err(geometry_worker_error(
                "geometry",
                format!(
                    "the prepared bust failed mesh validation and was not written: {}",
                    issues.join("; ")
                ),
            ));
        }
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

/// Estimate a head from photographs with the reconstruction worker. The
/// coefficients belong to the mounted FLAME 2020 generic model and can be
/// passed straight to `create` or `prepare_print` with that variant.
pub(super) async fn reconstruct(
    workspace: Arc<Workspace>,
    settings: &Settings,
    request: crate::recon::ReconRequest,
) -> Result<Value, ToolError> {
    request.validate()?;
    let variant = library(settings)?
        .reconstruction_variant()
        .ok_or_else(|| {
            ToolError::Validation(
                "photo reconstruction needs the FLAME 2020 generic model mounted".into(),
            )
        })?
        .to_owned();
    let client = settings.recon.as_ref().ok_or_else(|| {
        ToolError::Validation("the reconstruction worker is not configured".into())
    })?;
    for image in &request.images {
        workspace.stat_artifact(image)?;
    }
    validate_scad_destination(
        &workspace,
        &request.displacement_path,
        ".png",
        request.overwrite,
    )?;
    let colour_permit = match &request.texture_path {
        Some(_) => Some(client.colour_map_permit()?),
        None => None,
    };
    let textured_model = match &request.texture_path {
        Some(path) => {
            validate_scad_destination(&workspace, path, ".png", request.overwrite)?;
            if workspace.resolve(path, false)?
                == workspace.resolve(&request.displacement_path, false)?
            {
                return Err(ToolError::Validation(
                    "texture_path and displacement_path name the same file".into(),
                ));
            }
            let model = Arc::clone(
                library(settings)?
                    .select(Some(&variant))
                    .map_err(ToolError::Validation)?,
            );
            if model.uv_layout().is_none() {
                return Err(ToolError::Validation(format!(
                    "the {variant} model has no texture layout; convert it again with --texture to make a colour map"
                )));
            }
            Some(model)
        }
        None => None,
    };
    // The colour map projects private snapshots taken here; the worker
    // reports the digest of the bytes it fitted, so a photograph replaced in
    // between is caught rather than misregistered. Snapshots stay on disk.
    let photos = match textured_model {
        Some(_) => {
            let workspace = Arc::clone(&workspace);
            let images = request.images.clone();
            Some(
                blocking(move || {
                    images
                        .iter()
                        .map(|image| snapshot_photo(&workspace, image))
                        .collect::<Result<Vec<_>, ToolError>>()
                })
                .await?,
            )
        }
        None => None,
    };
    let result = client.reconstruct(&request).await?;
    let field = |name: &str| result.get(name).cloned().unwrap_or(Value::Null);
    let mut response = json!({
        "variant": variant,
        "identity": field("identity"),
        "expression": field("expression"),
        "pose": {"jaw": field("jaw")},
        "displacement": field("displacement"),
        "identity_agreement": field("identity_agreement"),
        "clamped_coefficients": field("clamped_coefficients"),
        "head_rotation": field("head_rotation"),
    });
    if let (Some(model), Some(photos), Some(path)) =
        (textured_model, photos, request.texture_path.clone())
    {
        let images = request.images.clone();
        let overwrite = request.overwrite;
        let views = field("views");
        let digests = field("image_sha256");
        let texture = blocking(move || {
            // Held by the work itself, so a cancelled request cannot let
            // another map start while this one is still being made.
            let _permit = colour_permit;
            let views: Vec<ReconView> = serde_json::from_value(views).map_err(|_| {
                ToolError::Validation(
                    "the reconstruction worker did not describe its view of each photograph"
                        .into(),
                )
            })?;
            let fitted: Vec<String> = serde_json::from_value(digests).map_err(|_| {
                ToolError::Validation(
                    "the reconstruction worker did not identify the photographs it used".into(),
                )
            })?;
            let read: Vec<&String> = photos.iter().map(|(_, digest)| digest).collect();
            if fitted.iter().collect::<Vec<_>>() != read {
                return Err(ToolError::Validation(
                    "a photograph changed while it was being reconstructed".into(),
                ));
            }
            colour_map(&workspace, &model, &images, &photos, &views, &path, overwrite)
        })
        .await
        .map_err(|error| {
            ToolError::Validation(format!(
                "the reconstruction finished and wrote the displacement map, but the colour map was not made: {error}"
            ))
        })?;
        response["texture"] = texture;
    }
    Ok(response)
}

/// A private, bounded copy of a photograph and the SHA-256 of its bytes.
/// An oversized file is refused while it is copied.
fn snapshot_photo(workspace: &Workspace, image: &str) -> Result<(Snapshot, String), ToolError> {
    let snapshot = workspace.snapshot_artifact_bounded(image, crate::recon::MAX_IMAGE_BYTES)?;
    let digest = crate::recon::file_digest(snapshot.path())?;
    Ok((snapshot, digest))
}

/// Side of the colour map in texels.
const COLOUR_MAP_TEXELS: usize = 1024;
/// Largest photograph decoded for the colour map, in pixels.
const MAX_PHOTO_PIXELS: u64 = 50_000_000;

/// SMIRK's fit to one photograph: the head that matches the face and the
/// orthographic camera in its 224 pixel crop.
#[derive(serde::Deserialize)]
struct ReconView {
    shape: Vec<f32>,
    expression: Vec<f32>,
    jaw: [f32; 3],
    head_rotation: [f32; 3],
    camera: [f32; 3],
    crop: ReconCrop,
    image_size: [usize; 2],
}

#[derive(serde::Deserialize)]
struct ReconCrop {
    left: f64,
    top: f64,
    step: f64,
}

/// Project every photograph onto its fitted head, build the colour map and
/// write it as an RGB PNG.
fn colour_map(
    workspace: &Workspace,
    model: &FlameModel,
    images: &[String],
    photos: &[(Snapshot, String)],
    views: &[ReconView],
    path: &str,
    overwrite: bool,
) -> Result<Value, ToolError> {
    if views.len() != images.len() {
        return Err(ToolError::Validation(
            "the reconstruction worker described a different number of photographs".into(),
        ));
    }
    let layout = model
        .uv_layout()
        .ok_or_else(|| ToolError::Validation("the model has no texture layout".into()))?;
    let mut builder = texture::ColourMapBuilder::new(model.faces(), layout, COLOUR_MAP_TEXELS)
        .map_err(|error| ToolError::Validation(error.to_string()))?;
    // One photograph at a time: each is read, decoded, projected and dropped
    // before the next.
    for ((image, (snapshot, _)), view) in images.iter().zip(photos).zip(views) {
        let photo = decode_photo(&std::fs::read(snapshot.path())?)
            .map_err(|message| ToolError::Validation(format!("{image}: {message}")))?;
        if [photo.width() as usize, photo.height() as usize] != view.image_size {
            return Err(ToolError::Validation(format!(
                "{image} does not have the size the reconstruction worker read"
            )));
        }
        let head = model
            .evaluate(&FlameParams {
                identity: view.shape.clone(),
                expression: view.expression.clone(),
                pose: Pose {
                    global: view.head_rotation,
                    jaw: view.jaw,
                    ..Pose::default()
                },
                translation: [0.0; 3],
            })
            .map_err(|error| ToolError::Validation(format!("{image}: {error}")))?;
        builder
            .add(&texture::View {
                photo: texture::Photo {
                    width: photo.width() as usize,
                    height: photo.height() as usize,
                    rgb: photo.as_raw(),
                },
                vertices: &head.vertices,
                projection: texture::Projection {
                    scale: view.camera[0],
                    offset: [view.camera[1], view.camera[2]],
                    crop_left: view.crop.left,
                    crop_top: view.crop.top,
                    crop_step: view.crop.step,
                },
            })
            .map_err(|error| ToolError::Validation(format!("{image}: {error}")))?;
    }
    let map = builder
        .finish()
        .map_err(|error| ToolError::Validation(error.to_string()))?;
    let side = map.size as u32;
    let image = RgbImage::from_raw(side, side, map.rgb)
        .ok_or_else(|| ToolError::Validation("the colour map has the wrong size".into()))?;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|_| ToolError::Validation("the colour map could not be encoded".into()))?;
    let artifact = workspace.write_generated_bytes(path, &bytes.into_inner(), overwrite)?;
    Ok(json!({
        "artifact": artifact,
        "width": side,
        "height": side,
        "seen": map.seen,
    }))
}

/// An upright RGB photograph, turned as its EXIF orientation says, as the
/// reconstruction worker reads it.
fn decode_photo(bytes: &[u8]) -> Result<RgbImage, String> {
    fn unreadable<E>(_: E) -> String {
        "it could not be decoded as a JPEG, PNG or WebP photograph".into()
    }
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(unreadable)?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)
    ) {
        return Err("it is not a JPEG, PNG or WebP photograph".into());
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_PHOTO_PIXELS * 4);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(unreadable)?;
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    if u64::from(width) * u64::from(height) > MAX_PHOTO_PIXELS {
        return Err(format!("it has more than {MAX_PHOTO_PIXELS} pixels"));
    }
    let orientation = image::ImageDecoder::orientation(&mut decoder).map_err(unreadable)?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(unreadable)?;
    image.apply_orientation(orientation);
    Ok(image.into_rgb8())
}

/// Read a displacement map from the workspace and pair it with the model's
/// texture coordinates.
async fn displacement(
    workspace: &Arc<Workspace>,
    model: &FlameModel,
    params: DisplacementParams,
) -> Result<Displacement, ToolError> {
    let layout = model.uv_layout().ok_or_else(|| {
        ToolError::Validation(format!(
            "the {} model has no texture layout; convert it again with --texture to use a displacement map",
            model.info().variant
        ))
    })?;
    let triangle_uvs = layout
        .faces
        .iter()
        .map(|face| face.map(|index| layout.coords[index as usize]))
        .collect();
    let workspace = Arc::clone(workspace);
    let path = params.path;
    let (_, bytes) = blocking(move || workspace.read_artifact(&path)).await?;
    let (width, height, fractions) = decode_displacement(&bytes)?;
    Ok(Displacement {
        width,
        height,
        values: fractions
            .into_iter()
            .map(|fraction| (params.offset_mm + params.scale_mm * fraction) as f32)
            .collect(),
        triangle_uvs,
    })
}

/// Pixels of an 8- or 16-bit greyscale PNG as fractions of the full range,
/// row by row from the top.
fn decode_displacement(bytes: &[u8]) -> Result<(usize, usize, Vec<f64>), ToolError> {
    let invalid = || {
        ToolError::Validation("the displacement map must be an 8- or 16-bit greyscale PNG".into())
    };
    let reader = || {
        let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), ImageFormat::Png);
        reader.limits(Limits::default());
        reader
    };
    let (width, height) = reader().into_dimensions().map_err(|_| invalid())?;
    if !(2..=MAX_DISPLACEMENT_PIXELS).contains(&width)
        || !(2..=MAX_DISPLACEMENT_PIXELS).contains(&height)
    {
        return Err(ToolError::Validation(format!(
            "the displacement map must be 2 to {MAX_DISPLACEMENT_PIXELS} pixels on each side"
        )));
    }
    let fractions = match reader().decode().map_err(|_| invalid())? {
        image::DynamicImage::ImageLuma16(image) => image
            .into_raw()
            .into_iter()
            .map(|level| f64::from(level) / f64::from(u16::MAX))
            .collect(),
        image::DynamicImage::ImageLuma8(image) => image
            .into_raw()
            .into_iter()
            .map(|level| f64::from(level) / f64::from(u8::MAX))
            .collect(),
        _ => return Err(invalid()),
    };
    Ok((width as usize, height as usize, fractions))
}

/// Largest colour map accepted, in bytes and in pixels on each side. The
/// Blender add-on stages a map in its 16 MiB run directory before loading it.
const MAX_COLOUR_MAP_BYTES: u64 = 12 * 1024 * 1024;
const MAX_COLOUR_MAP_PIXELS: u32 = 4096;

/// A colour map must be a workspace PNG of a sensible size. Its header is read
/// through a bounded snapshot before Blender is asked to load it.
async fn check_colour_map(workspace: &Arc<Workspace>, path: &str) -> Result<(), ToolError> {
    if !path.ends_with(".png") {
        return Err(ToolError::Validation(
            "texture must be a .png colour map".into(),
        ));
    }
    let workspace = Arc::clone(workspace);
    let path = path.to_owned();
    blocking(move || {
        let snapshot = workspace.snapshot_artifact_bounded(&path, MAX_COLOUR_MAP_BYTES)?;
        let file = std::io::BufReader::new(std::fs::File::open(snapshot.path())?);
        let (width, height) = ImageReader::with_format(file, ImageFormat::Png)
            .into_dimensions()
            .map_err(|_| ToolError::Validation(format!("{path} is not a PNG image")))?;
        if !(1..=MAX_COLOUR_MAP_PIXELS).contains(&width)
            || !(1..=MAX_COLOUR_MAP_PIXELS).contains(&height)
        {
            return Err(ToolError::Validation(format!(
                "a colour map must be at most {MAX_COLOUR_MAP_PIXELS} pixels on each side"
            )));
        }
        Ok::<_, ToolError>(())
    })
    .await
}

fn no_layout_for_colour(model: &FlameModel) -> ToolError {
    ToolError::Validation(format!(
        "the {} model has no texture layout; convert it again with --texture to show a colour map",
        model.info().variant
    ))
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
