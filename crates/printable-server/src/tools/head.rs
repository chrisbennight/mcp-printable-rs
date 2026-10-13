//! The `head` workflow: FLAME heads evaluated in the server and delivered to
//! Blender as millimetre meshes, or prepared as printable busts by the
//! isolated geometry worker.

use super::*;

use printable_flame::{FlameModel, FlameParams, Pose, PosedHead, detail, texture};
use printable_geom::{
    Body, BodyKind, BustRequest, Displacement, EyeAxis, EyeCarving, Hollow, Plinth, PrintChecks,
};

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
/// Busts under 100 mm default to facets this share of their height.
const EDGE_LENGTH_SHARE: f64 = 0.004;
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
/// Detail filter widths, at life size, from a millimetre to the width of a face.
const HIGH_PASS_MM: std::ops::RangeInclusive<f64> = 1.0..=100.0;
const CLIP_PERCENTILE: std::ops::RangeInclusive<f64> = 50.0..=100.0;
const DETAIL_GAIN: std::ops::RangeInclusive<f64> = 0.0..=10.0;
const RELIEF_MM: std::ops::RangeInclusive<f64> = 0.01..=10.0;
const EXPRESSION_SCALE: std::ops::RangeInclusive<f64> = 0.0..=1.0;
const MOUTH_RECESS_MM: std::ops::RangeInclusive<f64> = 0.5..=10.0;
const PUPIL_DIAMETER_MM: std::ops::RangeInclusive<f64> = 0.2..=10.0;
const IRIS_DIAMETER_MM: std::ops::RangeInclusive<f64> = 0.5..=20.0;
const EYE_DETAIL_DEPTH_MM: std::ops::RangeInclusive<f64> = 0.05..=5.0;
const NECK_EXTENSION_MM: std::ops::RangeInclusive<f64> = 2.0..=200.0;
const SHOULDER_WIDTH_MM: std::ops::RangeInclusive<f64> = 10.0..=500.0;
const SHOULDER_DEPTH_MM: std::ops::RangeInclusive<f64> = 5.0..=300.0;
const SHOULDER_HEIGHT_MM: std::ops::RangeInclusive<f64> = 2.0..=200.0;
/// Carved eye detail is at least this many layers deep.
const EYE_DETAIL_LAYERS: f64 = 2.0;
const DEFAULT_MOUTH_RECESS_MM: f64 = 2.0;
/// The model region over which detail fades out on a closed or recessed
/// mouth, so that the lips are not raised into a ridge where they meet.
const LIPS_REGION: &str = "lips";
const PLINTH_CHAMFER_MM: std::ops::RangeInclusive<f64> = 0.0..=5.0;
/// An FDM bust stands on a plinth this fraction of its height by default.
const FDM_PLINTH_FRACTION: f64 = 0.08;
/// A chamfer about two layers deep keeps the first layers, which spread as
/// they are pressed onto the build plate, within the plinth's outline.
const DEFAULT_FDM_CHAMFER_MM: f64 = 0.5;
const NOZZLE_DIAMETER_MM: std::ops::RangeInclusive<f64> = 0.1..=2.0;
const DEFAULT_NOZZLE_DIAMETER_MM: f64 = 0.4;
const LAYER_HEIGHT_MM: std::ops::RangeInclusive<f64> = 0.01..=1.0;
const DEFAULT_FDM_LAYER_HEIGHT_MM: f64 = 0.2;
const DEFAULT_RESIN_LAYER_HEIGHT_MM: f64 = 0.05;
const OVERHANG_DEGREES: std::ops::RangeInclusive<f64> = 0.0..=90.0;
const DEFAULT_OVERHANG_DEGREES: f64 = 45.0;
const MINIMUM_WALL_MM: std::ops::RangeInclusive<f64> = 0.05..=2.0;
/// With a printer, surface detail defaults to this many layers deep, so it
/// shows on surfaces facing up as well as on walls, but to no more than this
/// fraction of the bust's height above any plinth. On a small bust, detail
/// much deeper than that can push the eyelids clear of the eyes.
const DEFAULT_RELIEF_LAYERS: f64 = 2.0;
const DEFAULT_RELIEF_FRACTION: f64 = 0.01;

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
    /// Multiply the expression coefficients, the jaw rotation and the
    /// eyelids by this, 0 to 1 (default 1), to soften the expression; 0
    /// gives a neutral face.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expression_scale: Option<f64>,
    /// How far each eyelid is closed, left then right, each 0 (as the model
    /// shapes it, the default) to 1 (closed). Needs a model converted with
    /// an eyelid basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eyelids: Option<[f32; 2]>,
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
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expression_scale: Option<f64>,
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eyelids: Option<[f32; 2]>,
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
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expression_scale: Option<f64>,
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eyelids: Option<[f32; 2]>,
    /// How the mouth is printed. `as_posed` (the default) keeps the pose and
    /// closes the mouth opening flat behind the lips. `closed` sets the jaw
    /// rotation to zero, replacing `pose.jaw`, which brings the lips together
    /// unless the expression itself parts them. `recessed` keeps the pose
    /// and sets the centre of the surface closing the mouth back by
    /// `mouth_recess_mm`, so an open mouth reads as a shadow. With `closed`
    /// or `recessed`, surface detail fades out over the lips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mouth: Option<MouthShape>,
    /// How far `recessed` sets the mouth back on the finished bust, 0.5 to
    /// 10 mm (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mouth_recess_mm: Option<f64>,
    /// How the eyes are carved, centred on each eye's gaze: `plain` (the
    /// default) leaves them smooth, `pupil` drills the pupil, and
    /// `iris_and_pupil` also engraves a ring at the edge of the iris.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eyes: Option<EyeStyle>,
    /// Pupil diameter on the finished bust, 0.2 to 10 mm. Defaults to 4 mm at
    /// life size scaled with the bust, and to no less than two nozzle widths
    /// with an FDM `printer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pupil_diameter_mm: Option<f64>,
    /// Iris diameter on the finished bust, 0.5 to 20 mm, for
    /// `iris_and_pupil`. Defaults to 11.5 mm at life size scaled with the
    /// bust, and wide enough to leave the printer's narrowest feature between
    /// the ring and the pupil.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iris_diameter_mm: Option<f64>,
    /// How deep the pupil is drilled on the finished bust, 0.05 to 5 mm; the
    /// ring is half as deep, and with a `printer` no shallower than two
    /// layers. Defaults to 2 mm at life size scaled with the bust, and to no
    /// less than two layers with a `printer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eye_detail_depth_mm: Option<f64>,
    /// What stands below the head, within `height_mm` above any plinth:
    /// `neck` (the default) ends at the neck's cut, `extended_neck` continues
    /// the neck down along its own lean, and `shoulders` adds a block of
    /// chest and shoulders. The head keeps at least half the height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<BodyShape>,
    /// How much `extended_neck` adds, 2 to 200 mm. Defaults to 15% of the
    /// head's height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    neck_extension_mm: Option<f64>,
    /// The `shoulders` block's width, 10 to 500 mm. Defaults to 1.6 times the
    /// head's width, and at least 1.3 times the neck's. Shoulders too small to
    /// hold the neck are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shoulder_width_mm: Option<f64>,
    /// The `shoulders` block's depth, 5 to 300 mm. Defaults to 0.9 times the
    /// head's depth, and at least 1.3 times the neck's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shoulder_depth_mm: Option<f64>,
    /// The `shoulders` block's height, 2 to 200 mm. Defaults to 35% of the
    /// head's height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shoulder_height_mm: Option<f64>,
    /// Workspace path for the binary STL.
    path: String,
    /// Replace an existing file at `path`.
    #[serde(default)]
    overwrite: bool,
    /// Height of the bust from its flat base to the crown, 20 to 500 mm
    /// (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    height_mm: Option<f64>,
    /// Target facet edge length, 0.1 to 5 mm. Defaults to 0.4, or to 0.4% of
    /// `height_mm` on busts under 100 mm and no less than 0.1. Shorter edges
    /// follow the smooth surface more closely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_length_mm: Option<f64>,
    /// Upper bound on the triangle count, 10,000 to 3,000,000 (default
    /// 750,000). The edge length grows when needed to respect it. Limits above
    /// the default need a larger geometry worker memory limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_triangles: Option<usize>,
    /// Height of a cylindrical plinth under the bust, 2 to 100 mm, counted
    /// within `height_mm`, or 0 for none. Defaults to 8% of `height_mm` for an
    /// FDM `printer` and to no plinth otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_height_mm: Option<f64>,
    /// Plinth diameter, 10 to 500 mm. Defaults to 70% of the bust's widest
    /// extent and reaches at least 1 mm beyond the bust's base. Requires a
    /// plinth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_diameter_mm: Option<f64>,
    /// Cut the plinth's bottom edge back this far at 45 degrees, 0 to 5 mm,
    /// so that the first layers stay within its outline. Defaults to 0.5 for
    /// an FDM `printer` and to 0 otherwise. Requires a plinth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_chamfer_mm: Option<f64>,
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
    /// The printer the bust is for. Sets the defaults for the plinth and
    /// detail relief, and reports overhangs, thin parts and detail too
    /// shallow to print. Without it, nothing is assumed about the printer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    printer: Option<PrinterParams>,
}

/// How the mouth of a bust is printed.
#[derive(
    Clone, Copy, PartialEq, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum MouthShape {
    /// As the pose leaves it.
    #[default]
    AsPosed,
    /// With the jaw closed.
    Closed,
    /// With the surface closing the mouth set back into the head.
    Recessed,
}

/// What stands below the head of a bust.
#[derive(
    Clone, Copy, PartialEq, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum BodyShape {
    /// The neck as cut.
    #[default]
    Neck,
    /// The neck continued down along its own lean.
    ExtendedNeck,
    /// A block of chest and shoulders.
    Shoulders,
}

/// How the eyes of a bust are carved.
#[derive(
    Clone, Copy, PartialEq, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum EyeStyle {
    /// Smooth, as modelled.
    #[default]
    Plain,
    /// A drilled pupil.
    Pupil,
    /// A drilled pupil within an engraved iris ring.
    IrisAndPupil,
}

/// How the bust will be printed.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PrinterParams {
    process: PrintProcess,
    /// FDM nozzle diameter, 0.1 to 2 mm (default 0.4). FDM only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nozzle_diameter_mm: Option<f64>,
    /// Layer height, 0.01 to 1 mm and no more than the nozzle diameter.
    /// Defaults to 0.2 for FDM and 0.05 for resin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layer_height_mm: Option<f64>,
    /// Report surface facing down more steeply than this many degrees from
    /// vertical, 0 to 90 (default 45).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maximum_overhang_degrees: Option<f64>,
    /// Report parts of the bust thinner than this, 0.05 to 2 mm. Defaults to
    /// the nozzle diameter for FDM; resin busts are not checked unless it is
    /// given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    minimum_wall_mm: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum PrintProcess {
    /// Filament deposited through a nozzle.
    Fdm,
    /// Resin cured layer by layer.
    Resin,
}

/// Printer settings with their defaults filled in.
#[derive(Clone, Copy, serde::Serialize)]
struct Printer {
    process: PrintProcess,
    #[serde(skip_serializing_if = "Option::is_none")]
    nozzle_diameter_mm: Option<f64>,
    layer_height_mm: f64,
    maximum_overhang_degrees: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_wall_mm: Option<f64>,
}

impl PrinterParams {
    fn resolve(&self) -> Result<Printer, ToolError> {
        let fdm = self.process == PrintProcess::Fdm;
        if !fdm && self.nozzle_diameter_mm.is_some() {
            return Err(ToolError::Validation(
                "printer.nozzle_diameter_mm applies only to fdm".into(),
            ));
        }
        let nozzle = fdm
            .then(|| {
                in_range(
                    "printer.nozzle_diameter_mm",
                    self.nozzle_diameter_mm
                        .unwrap_or(DEFAULT_NOZZLE_DIAMETER_MM),
                    &NOZZLE_DIAMETER_MM,
                )
            })
            .transpose()?;
        let layer = in_range(
            "printer.layer_height_mm",
            self.layer_height_mm.unwrap_or(if fdm {
                DEFAULT_FDM_LAYER_HEIGHT_MM
            } else {
                DEFAULT_RESIN_LAYER_HEIGHT_MM
            }),
            &LAYER_HEIGHT_MM,
        )?;
        if nozzle.is_some_and(|nozzle| layer > nozzle) {
            return Err(ToolError::Validation(
                "printer.layer_height_mm must not exceed printer.nozzle_diameter_mm".into(),
            ));
        }
        Ok(Printer {
            process: self.process,
            nozzle_diameter_mm: nozzle,
            layer_height_mm: layer,
            maximum_overhang_degrees: in_range(
                "printer.maximum_overhang_degrees",
                self.maximum_overhang_degrees
                    .unwrap_or(DEFAULT_OVERHANG_DEGREES),
                &OVERHANG_DEGREES,
            )?,
            minimum_wall_mm: self
                .minimum_wall_mm
                .or(nozzle)
                .map(|wall| in_range("printer.minimum_wall_mm", wall, &MINIMUM_WALL_MM))
                .transpose()?,
        })
    }
}

/// A greyscale PNG displacement map. A pixel at fraction f of the full
/// range, p / 65535 for 16-bit or p / 255 for 8-bit images, raises the
/// surface by `offset_mm + scale_mm × f` millimetres at life size, scaled
/// with the bust. Row 0 is v = 1 and column 0 is u = 0. The optional filters
/// apply in this order: `high_pass_mm`, `clip_percentile`, then `gain` or
/// `relief_mm`.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DisplacementParams {
    /// Workspace path of the PNG, at most 1024 pixels on each side.
    path: String,
    offset_mm: f64,
    scale_mm: f64,
    /// Remove features wider than about this many millimetres on the
    /// life-size head, 1 to 100, keeping finer detail such as wrinkles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    high_pass_mm: Option<f64>,
    /// Limit the detail to this percentile of its magnitude, 50 to 100, so a
    /// few extreme values do not dominate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clip_percentile: Option<f64>,
    /// Multiply the detail by this factor, 0 to 10 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gain: Option<f64>,
    /// Scale the detail so that the 95th percentile of the map's detail is
    /// about this many millimetres deep on the finished bust, 0.01 to 10,
    /// whatever the bust's height. Not with `gain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relief_mm: Option<f64>,
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
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expression_scale: Option<f64>,
    /// As for `create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eyelids: Option<[f32; 2]>,
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
        (
            params.identity,
            params.expression,
            params.expression_scale,
            params.eyelids,
        ),
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
        (
            params.identity,
            params.expression,
            params.expression_scale,
            params.eyelids,
        ),
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
        (
            params.identity,
            params.expression,
            params.expression_scale,
            params.eyelids,
        ),
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
    // A small bust keeps the same detail relative to its size: thin parts
    // such as the ears stay several facets thick.
    let default_edge =
        (EDGE_LENGTH_SHARE * height_mm).clamp(*EDGE_LENGTH_MM.start(), DEFAULT_EDGE_LENGTH_MM);
    let edge_length_mm = in_range(
        "edge_length_mm",
        params.edge_length_mm.unwrap_or(default_edge),
        &EDGE_LENGTH_MM,
    )?;
    let max_triangles = in_range(
        "max_triangles",
        params.max_triangles.unwrap_or(DEFAULT_MAX_TRIANGLES),
        &MAX_TRIANGLES,
    )?;
    let printer = params
        .printer
        .as_ref()
        .map(PrinterParams::resolve)
        .transpose()?;
    let fdm = printer.is_some_and(|printer| printer.process == PrintProcess::Fdm);
    let plinth_height = match params.plinth_height_mm {
        Some(0.0) => None,
        Some(height) => Some(in_range("plinth_height_mm", height, &PLINTH_HEIGHT_MM)?),
        None if fdm => Some((FDM_PLINTH_FRACTION * height_mm).max(*PLINTH_HEIGHT_MM.start())),
        None => None,
    };
    let plinth = match plinth_height {
        None if params.plinth_diameter_mm.is_some() || params.plinth_chamfer_mm.is_some() => {
            return Err(ToolError::Validation(
                "plinth_diameter_mm and plinth_chamfer_mm require a plinth".into(),
            ));
        }
        None => None,
        Some(height) => Some(Plinth {
            height_mm: height,
            diameter_mm: params
                .plinth_diameter_mm
                .map(|diameter| in_range("plinth_diameter_mm", diameter, &PLINTH_DIAMETER_MM))
                .transpose()?,
            chamfer_mm: match params.plinth_chamfer_mm {
                Some(chamfer) => in_range("plinth_chamfer_mm", chamfer, &PLINTH_CHAMFER_MM)?,
                None if fdm => DEFAULT_FDM_CHAMFER_MM,
                None => 0.0,
            },
        }),
    };
    if let Some(plinth) = plinth
        && plinth.chamfer_mm >= plinth.height_mm
    {
        return Err(ToolError::Validation(
            "plinth_chamfer_mm must be less than the plinth's height".into(),
        ));
    }
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
    if let Some(map) = &params.displacement {
        if !(map.offset_mm.is_finite() && map.scale_mm.is_finite()) {
            return Err(ToolError::Validation(
                "displacement offset_mm and scale_mm must be finite".into(),
            ));
        }
        if map.gain.is_some() && map.relief_mm.is_some() {
            return Err(ToolError::Validation(
                "displacement takes gain or relief_mm, not both".into(),
            ));
        }
        for (name, value, range) in [
            ("displacement.high_pass_mm", map.high_pass_mm, &HIGH_PASS_MM),
            (
                "displacement.clip_percentile",
                map.clip_percentile,
                &CLIP_PERCENTILE,
            ),
            ("displacement.gain", map.gain, &DETAIL_GAIN),
            ("displacement.relief_mm", map.relief_mm, &RELIEF_MM),
        ] {
            if let Some(value) = value {
                in_range(name, value, range)?;
            }
        }
    }
    let eye_style = params.eyes.unwrap_or_default();
    let eye_sizes = [
        params.pupil_diameter_mm,
        params.iris_diameter_mm,
        params.eye_detail_depth_mm,
    ];
    if eye_style == EyeStyle::Plain && eye_sizes.iter().any(Option::is_some) {
        return Err(ToolError::Validation(
            "pupil_diameter_mm, iris_diameter_mm and eye_detail_depth_mm need eyes pupil or iris_and_pupil".into(),
        ));
    }
    if eye_style == EyeStyle::Pupil && params.iris_diameter_mm.is_some() {
        return Err(ToolError::Validation(
            "iris_diameter_mm needs eyes iris_and_pupil".into(),
        ));
    }
    for (name, value, range) in [
        (
            "pupil_diameter_mm",
            params.pupil_diameter_mm,
            &PUPIL_DIAMETER_MM,
        ),
        (
            "iris_diameter_mm",
            params.iris_diameter_mm,
            &IRIS_DIAMETER_MM,
        ),
        (
            "eye_detail_depth_mm",
            params.eye_detail_depth_mm,
            &EYE_DETAIL_DEPTH_MM,
        ),
    ] {
        if let Some(value) = value {
            in_range(name, value, range)?;
        }
    }
    let shoulder_sizes = [
        params.shoulder_width_mm,
        params.shoulder_depth_mm,
        params.shoulder_height_mm,
    ];
    let body_shape = params.body.unwrap_or_default();
    if body_shape != BodyShape::ExtendedNeck && params.neck_extension_mm.is_some() {
        return Err(ToolError::Validation(
            "neck_extension_mm needs body extended_neck".into(),
        ));
    }
    if body_shape != BodyShape::Shoulders && shoulder_sizes.iter().any(Option::is_some) {
        return Err(ToolError::Validation(
            "shoulder_width_mm, shoulder_depth_mm and shoulder_height_mm need body shoulders"
                .into(),
        ));
    }
    for (name, value, range) in [
        (
            "neck_extension_mm",
            params.neck_extension_mm,
            &NECK_EXTENSION_MM,
        ),
        (
            "shoulder_width_mm",
            params.shoulder_width_mm,
            &SHOULDER_WIDTH_MM,
        ),
        (
            "shoulder_depth_mm",
            params.shoulder_depth_mm,
            &SHOULDER_DEPTH_MM,
        ),
        (
            "shoulder_height_mm",
            params.shoulder_height_mm,
            &SHOULDER_HEIGHT_MM,
        ),
    ] {
        if let Some(value) = value {
            in_range(name, value, range)?;
        }
    }
    let body = match body_shape {
        BodyShape::Neck => None,
        BodyShape::ExtendedNeck => Some(Body {
            kind: BodyKind::ExtendedNeck,
            height_mm: params.neck_extension_mm,
            width_mm: None,
            depth_mm: None,
        }),
        BodyShape::Shoulders => Some(Body {
            kind: BodyKind::Shoulders,
            height_mm: params.shoulder_height_mm,
            width_mm: params.shoulder_width_mm,
            depth_mm: params.shoulder_depth_mm,
        }),
    };
    let mouth = params.mouth.unwrap_or_default();
    let mouth_recess_mm = match (mouth, params.mouth_recess_mm) {
        (MouthShape::Recessed, recess) => Some(in_range(
            "mouth_recess_mm",
            recess.unwrap_or(DEFAULT_MOUTH_RECESS_MM),
            &MOUTH_RECESS_MM,
        )?),
        (_, Some(_)) => {
            return Err(ToolError::Validation(
                "mouth_recess_mm requires mouth recessed".into(),
            ));
        }
        (_, None) => None,
    };
    let mut pose = params.pose;
    if mouth == MouthShape::Closed {
        pose.jaw = [0.0; 3];
    }
    let worker_bin = geometry_worker_path(settings)?;
    let worker_memory_bytes = settings.geometry_worker_memory_bytes;
    let (model, head) = evaluate(
        settings,
        params.variant.as_deref(),
        (
            params.identity,
            params.expression,
            params.expression_scale,
            params.eyelids,
        ),
        pose,
    )
    .await?;
    let eyes = match eye_style {
        EyeStyle::Plain => None,
        EyeStyle::Pupil | EyeStyle::IrisAndPupil => Some(EyeCarving {
            eyes: eye_axes(&model, &head).await?,
            iris_ring: eye_style == EyeStyle::IrisAndPupil,
            pupil_diameter_mm: params.pupil_diameter_mm,
            iris_diameter_mm: params.iris_diameter_mm,
            depth_mm: params.eye_detail_depth_mm,
            smallest_mm: printer
                .and_then(|printer| printer.nozzle_diameter_mm.or(printer.minimum_wall_mm)),
            shallowest_mm: printer.map(|printer| EYE_DETAIL_LAYERS * printer.layer_height_mm),
        }),
    };
    // A closed or recessed mouth takes no detail on the lips, when the model
    // knows where they are.
    let faded = match mouth {
        MouthShape::AsPosed => None,
        MouthShape::Closed | MouthShape::Recessed => model.region(LIPS_REGION),
    };
    let mut mouth_report = (mouth != MouthShape::AsPosed).then(|| {
        json!({
            "shape": mouth,
            "recess_mm": mouth_recess_mm,
        })
    });
    let (displacement, detail_filter) = match params.displacement {
        Some(mut map) => {
            // With a printer, detail is made deep enough to print unless the
            // caller chose its depth; on a small bust that depth may exceed
            // the limit for detail, and is then reduced to it.
            let default_relief = map.gain.is_none() && map.relief_mm.is_none();
            let limit_relief = match printer {
                Some(printer) if default_relief => {
                    let bust_mm = height_mm - plinth.map_or(0.0, |plinth| plinth.height_mm);
                    map.relief_mm = Some(
                        (DEFAULT_RELIEF_LAYERS * printer.layer_height_mm)
                            .min(DEFAULT_RELIEF_FRACTION * bust_mm),
                    );
                    true
                }
                _ => false,
            };
            if let Some(report) = &mut mouth_report {
                report["detail_faded_over"] = json!(faded.map(|_| LIPS_REGION));
            }
            let faded = faded.map(<[u32]>::to_vec).unwrap_or_default();
            let (map, report) =
                displacement(&workspace, &model, &head, map, (limit_relief, faded)).await?;
            (Some(map), Some(report))
        }
        None => (None, None),
    };
    let head_vertices = millimetres(&head.vertices);
    let request = BustRequest {
        vertices: head_vertices
            .iter()
            .map(|point| point.map(f64::from))
            .collect(),
        triangles: model.faces().to_vec(),
        height_mm,
        edge_length_mm,
        max_triangles,
        plinth,
        hollow,
        displacement,
        checks: printer.map(|printer| PrintChecks {
            overhang_degrees: printer.maximum_overhang_degrees,
            min_feature_mm: printer.minimum_wall_mm,
        }),
        eyes,
        body,
        mouth_recess_mm,
    };
    let variant = model.info().variant.clone();
    let validation_options = ValidationOptions {
        overhang_angle_degrees: printer.map_or(
            ValidationOptions::default().overhang_angle_degrees,
            |printer| printer.maximum_overhang_degrees,
        ),
        ..ValidationOptions::default()
    };
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
        let validation = analyze_stl(&bytes, validation_options)?;
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
        let mut report = serde_json::to_value(report)?;
        if let (Some(filter), Some(applied)) = (detail_filter, report.get_mut("displacement")) {
            applied["filter"] = filter;
        }
        name_regions(&model, &head_vertices, &mut report);
        if let Some(carved) = report.get_mut("eyes") {
            carved["style"] = json!(eye_style);
            if carved["removed_mm3"].as_f64().is_some_and(|removed| removed <= 0.0) {
                carved["note"] =
                    json!("the head's surface covers the eyes where they would be carved; nothing was removed");
            }
        }
        let mut response = json!({
            "artifact": artifact,
            "units": "millimetres",
            "variant": variant,
            "bust": report,
            "validation": validation,
        });
        if let Some(printer) = printer {
            response["warnings"] = json!(print_warnings(&printer, &response["bust"]));
            response["printer"] = serde_json::to_value(printer)?;
        }
        if let Some(mut mouth) = mouth_report {
            // The depth applied on the finished bust, which the smoothing's
            // height correction moves slightly from the request.
            if mouth_recess_mm.is_some() {
                mouth["recess_mm"] = response["bust"]["mouth_recess_mm"].clone();
                if mouth["recess_mm"].is_null() {
                    mouth["note"] = json!("the model has no mouth opening to recess");
                }
            }
            response["mouth"] = mouth;
        }
        Ok::<_, ToolError>(response)
    })
    .await
}

/// Each eyeball's centre and the point where its gaze leaves it, in
/// millimetres. The eyeballs are the parts separate from the head, and the
/// gaze leaves each through its vertex furthest forward when the head is at
/// rest, which the eye's rotation carries with it.
async fn eye_axes(model: &Arc<FlameModel>, head: &PosedHead) -> Result<Vec<EyeAxis>, ToolError> {
    let model = Arc::clone(model);
    let posed = millimetres(&head.vertices);
    blocking(move || {
        let faces = model.faces();
        let mut parent: Vec<usize> = (0..posed.len()).collect();
        fn root(parent: &mut [usize], mut index: usize) -> usize {
            while parent[index] != index {
                parent[index] = parent[parent[index]];
                index = parent[index];
            }
            index
        }
        let minor = texture::minor_parts(faces);
        for face in faces
            .iter()
            .zip(&minor)
            .filter(|(_, minor)| **minor)
            .map(|(face, _)| face)
        {
            for pair in [[face[0], face[1]], [face[1], face[2]]] {
                let (a, b) = (
                    root(&mut parent, pair[0] as usize),
                    root(&mut parent, pair[1] as usize),
                );
                parent[a] = b;
            }
        }
        let mut parts: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
        let mut seen = vec![false; posed.len()];
        for face in faces
            .iter()
            .zip(&minor)
            .filter(|(_, minor)| **minor)
            .map(|(face, _)| face)
        {
            for &vertex in face {
                let vertex = vertex as usize;
                if !std::mem::replace(&mut seen[vertex], true) {
                    let part = root(&mut parent, vertex);
                    parts.entry(part).or_default().push(vertex);
                }
            }
        }
        if !(1..=2).contains(&parts.len()) {
            return Err(ToolError::Validation(format!(
                "the {} model has no separate eyeballs to carve",
                model.info().variant
            )));
        }
        let rest = model
            .evaluate(&FlameParams::default())
            .map_err(|error| ToolError::Validation(error.to_string()))?;
        Ok(parts
            .into_values()
            .map(|vertices| {
                let count = vertices.len() as f64;
                let centre = [0, 1, 2].map(|axis| {
                    vertices
                        .iter()
                        .map(|&vertex| f64::from(posed[vertex][axis]))
                        .sum::<f64>()
                        / count
                });
                let pole = vertices
                    .iter()
                    .copied()
                    .max_by(|&a, &b| rest.vertices[a][2].total_cmp(&rest.vertices[b][2]))
                    .expect("a part has vertices");
                EyeAxis {
                    centre,
                    front: posed[pole].map(f64::from),
                }
            })
            .collect())
    })
    .await
}

/// The largest suggested neck rotation: a half turn less a margin that
/// rounding to single precision, in which poses are read and checked,
/// cannot cross.
const MAX_SUGGESTED_ANGLE: f64 = std::f64::consts::PI - 1e-5;

/// A neck rotation that keeps a share of the head's rotation in a
/// photograph. The rotation is split into a turn about y, then a pitch about
/// x, then a roll about z; the turn is scaled by `turn` and the pitch and
/// roll by `tilt`, and they are put back together as an axis-angle vector.
fn suggested_neck(rotation: [f64; 3], tilt: f64, turn: f64) -> [f64; 3] {
    let matrix = axis_angle_matrix(rotation);
    let pitch = (-matrix[1][2]).clamp(-1.0, 1.0).asin();
    let yaw = matrix[0][2].atan2(matrix[2][2]);
    let roll = matrix[1][0].atan2(matrix[1][1]);
    let multiply = |a: [[f64; 3]; 3], b: [[f64; 3]; 3]| {
        std::array::from_fn(|row| {
            std::array::from_fn(|column| (0..3).map(|k| a[row][k] * b[k][column]).sum())
        })
    };
    let kept = multiply(
        multiply(
            axis_angle_matrix([0.0, turn * yaw, 0.0]),
            axis_angle_matrix([tilt * pitch, 0.0, 0.0]),
        ),
        axis_angle_matrix([0.0, 0.0, tilt * roll]),
    );
    // Through a unit quaternion with a non-negative scalar part, which is
    // stable at every angle and gives an angle of at most a half turn.
    let [w, x, y, z] = matrix_quaternion(kept);
    let [w, x, y, z] = if w < 0.0 {
        [-w, -x, -y, -z]
    } else {
        [w, x, y, z]
    };
    let length = (x * x + y * y + z * z).sqrt();
    if length < 1e-12 {
        return [0.0; 3];
    }
    let angle = (2.0 * length.atan2(w)).min(MAX_SUGGESTED_ANGLE);
    [x, y, z].map(|value| value / length * angle)
}

/// The unit quaternion, scalar first, of a rotation matrix, by Shepperd's
/// method: from whichever of its four parts is largest, so that no division
/// is by a small number.
fn matrix_quaternion(m: [[f64; 3]; 3]) -> [f64; 4] {
    let trace = m[0][0] + m[1][1] + m[2][2];
    let quaternion = if trace > m[0][0].max(m[1][1]).max(m[2][2]) {
        let s = (1.0 + trace).sqrt() * 2.0;
        [
            s / 4.0,
            (m[2][1] - m[1][2]) / s,
            (m[0][2] - m[2][0]) / s,
            (m[1][0] - m[0][1]) / s,
        ]
    } else if m[0][0] >= m[1][1] && m[0][0] >= m[2][2] {
        let s = (1.0 + m[0][0] - m[1][1] - m[2][2]).sqrt() * 2.0;
        [
            (m[2][1] - m[1][2]) / s,
            s / 4.0,
            (m[0][1] + m[1][0]) / s,
            (m[0][2] + m[2][0]) / s,
        ]
    } else if m[1][1] >= m[2][2] {
        let s = (1.0 + m[1][1] - m[0][0] - m[2][2]).sqrt() * 2.0;
        [
            (m[0][2] - m[2][0]) / s,
            (m[0][1] + m[1][0]) / s,
            s / 4.0,
            (m[1][2] + m[2][1]) / s,
        ]
    } else {
        let s = (1.0 + m[2][2] - m[0][0] - m[1][1]).sqrt() * 2.0;
        [
            (m[1][0] - m[0][1]) / s,
            (m[0][2] + m[2][0]) / s,
            (m[1][2] + m[2][1]) / s,
            s / 4.0,
        ]
    };
    let norm = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    quaternion.map(|value| value / norm)
}

/// The rotation matrix of an axis-angle vector.
fn axis_angle_matrix(rotation: [f64; 3]) -> [[f64; 3]; 3] {
    let angle = rotation
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if angle < 1e-12 {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let [x, y, z] = rotation.map(|value| value / angle);
    let (sin, cos) = angle.sin_cos();
    let versine = 1.0 - cos;
    [
        [
            versine * x * x + cos,
            versine * x * y - sin * z,
            versine * x * z + sin * y,
        ],
        [
            versine * x * y + sin * z,
            versine * y * y + cos,
            versine * y * z - sin * x,
        ],
        [
            versine * x * z - sin * y,
            versine * y * z + sin * x,
            versine * z * z + cos,
        ],
    ]
}

/// Replace the vertex index of each region a print check found with the name
/// of the smallest model region that holds that vertex, or else that holds
/// the nearest vertex in any region. Unnamed when the model has no regions.
fn name_regions(model: &FlameModel, vertices: &[[f32; 3]], report: &mut Value) {
    let smallest = |vertex: u32| {
        model
            .regions()
            .iter()
            .filter(|(_, members)| members.binary_search(&vertex).is_ok())
            .min_by_key(|(_, members)| members.len())
            .map(|(name, _)| name.clone())
    };
    for check in ["overhang", "thin"] {
        let Some(regions) = report
            .get_mut(check)
            .and_then(|found| found.get_mut("regions"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for region in regions.iter_mut().filter_map(Value::as_object_mut) {
            let vertex = region
                .remove("nearest_vertex")
                .and_then(|vertex| vertex.as_u64())
                .and_then(|vertex| usize::try_from(vertex).ok())
                .filter(|&vertex| vertex < vertices.len());
            let name = vertex.and_then(|vertex| {
                smallest(vertex as u32).or_else(|| {
                    let at = vertices[vertex];
                    let distance = |other: &[f32; 3]| -> f32 {
                        (0..3).map(|axis| (other[axis] - at[axis]).powi(2)).sum()
                    };
                    model
                        .regions()
                        .values()
                        .flatten()
                        .min_by(|a, b| {
                            distance(&vertices[**a as usize])
                                .total_cmp(&distance(&vertices[**b as usize]))
                        })
                        .and_then(|&nearest| smallest(nearest))
                })
            });
            if let Some(name) = name {
                region.insert("region".into(), json!(name));
            }
        }
    }
}

/// What the printer may fail to reproduce, from the bust report.
fn print_warnings(printer: &Printer, bust: &Value) -> Vec<String> {
    let mut warnings = Vec::new();
    if let (Some(wall), Some(thin)) = (printer.minimum_wall_mm, bust.get("thin")) {
        let area = thin["area_mm2"].as_f64().unwrap_or(0.0);
        if area > 0.0 {
            let mut places: Vec<&str> = Vec::new();
            for region in thin["regions"].as_array().into_iter().flatten() {
                if let Some(name) = region["region"].as_str()
                    && !places.contains(&name)
                {
                    places.push(name);
                }
            }
            let near = if places.is_empty() {
                String::new()
            } else {
                format!(", mostly near {}", places.join(", "))
            };
            warnings.push(format!(
                "{area:.1} mm² of the surface is on parts thinner than {wall} mm{near}; they may not print"
            ));
        }
    }
    if let Some(detail) = bust.get("displacement") {
        if detail["relief_limited"].as_bool() == Some(true) {
            warnings.push(
                "the default detail relief was reduced to keep the detail within 5% of the head's height".into(),
            );
        }
        if let Some(depth) = detail["depth_p95_mm"].as_f64()
            && depth < printer.layer_height_mm
        {
            warnings.push(format!(
                "95% of the surface detail is less than {depth:.2} mm deep, shallower than one {} mm layer, so little of it will show; raise displacement.relief_mm",
                printer.layer_height_mm
            ));
        }
    }
    warnings
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
    // A worker that does not report the primary photograph used the first.
    let primary = match result.get("primary_image") {
        None => 0,
        Some(index) => index
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .filter(|&index| index < request.images.len())
            .ok_or_else(|| {
                ToolError::Validation(
                    "the reconstruction finished and wrote the displacement map, but named a primary photograph that was not given".into(),
                )
            })?,
    };
    let mut response = json!({
        "variant": variant,
        "identity": field("identity"),
        "expression": field("expression"),
        "pose": {"jaw": field("jaw")},
        "displacement": field("displacement"),
        "identity_agreement": field("identity_agreement"),
        "clamped_coefficients": field("clamped_coefficients"),
        "head_rotation": field("head_rotation"),
        "primary_image": {"index": primary, "image": request.images[primary]},
    });
    let (tilt, turn) = (
        request.neck_tilt.unwrap_or(0.0),
        request.neck_turn.unwrap_or(0.0),
    );
    if tilt > 0.0 || turn > 0.0 {
        let rotation: Option<[f64; 3]> = serde_json::from_value(field("head_rotation")).ok();
        if let Some(rotation) = rotation {
            response["pose"]["neck"] = json!(suggested_neck(rotation, tilt, turn));
        }
    }
    if let (Some(model), Some(photos), Some(path)) =
        (textured_model, photos, request.texture_path.clone())
    {
        let images = request.images.clone();
        let overwrite = request.overwrite;
        let colouring = Colouring {
            scalp: request.scalp.unwrap_or_default(),
            hair_colour: request.hair_colour,
            lighting: request.lighting.unwrap_or_default(),
            size: request
                .texture_size
                .map_or(COLOUR_MAP_TEXELS, |size| size as usize),
            eyes: request.eyes.unwrap_or_default(),
            iris_colour: request.iris_colour,
            sclera_colour: request.sclera_colour,
        };
        let views = field("views");
        let digests = field("image_sha256");
        let identity = field("identity");
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
            let identity: Vec<f32> = serde_json::from_value(identity).map_err(|_| {
                ToolError::Validation("the reconstruction worker returned no identity".into())
            })?;
            let read: Vec<&String> = photos.iter().map(|(_, digest)| digest).collect();
            if fitted.iter().collect::<Vec<_>>() != read {
                return Err(ToolError::Validation(
                    "a photograph changed while it was being reconstructed".into(),
                ));
            }
            colour_map(
                &workspace,
                &model,
                ColourMapRequest {
                    images: &images,
                    photos: &photos,
                    views: &views,
                    identity: &identity,
                    path: &path,
                    overwrite,
                    colouring,
                },
            )
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

/// Side of the colour map in texels unless the caller chooses another.
const COLOUR_MAP_TEXELS: usize = 1024;
/// Painted eyes: life-size iris and pupil diameters in metres, the model's
/// units, and the default colour of the white.
const PAINTED_IRIS_M: f32 = 0.0115;
const PAINTED_PUPIL_M: f32 = 0.004;
const DEFAULT_SCLERA_RGB: [u8; 3] = [240, 236, 230];
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
    /// What each pixel around the face shows, when the worker segmented it.
    #[serde(default)]
    segmentation: Option<ReconSegmentation>,
}

/// The segmenter's labels for a square around the face, as a greyscale PNG,
/// and the hair colour it found there.
#[derive(serde::Deserialize)]
struct ReconSegmentation {
    labels_png: String,
    classes: Vec<String>,
    crop: ReconCrop,
    hair_rgb: Option<[u8; 3]>,
    hair_pixels: u64,
}

/// The label numbering the colour map expects, which is the segmenter's.
const SEGMENT_CLASSES: [&str; 6] = [
    "background",
    "hair",
    "body_skin",
    "face_skin",
    "clothes",
    "other",
];
/// Largest label image accepted from the worker, in pixels on each side and
/// in encoded bytes.
const MAX_LABEL_SIDE: u32 = 1024;
const MAX_LABEL_PNG_BYTES: usize = 1024 * 1024;

/// How the caller asked for the scalp to be coloured.
#[derive(Clone, Copy)]
struct Colouring {
    scalp: crate::recon::ScalpChoice,
    hair_colour: Option<[u8; 3]>,
    lighting: crate::recon::LightingChoice,
    size: usize,
    eyes: crate::recon::EyeChoice,
    iris_colour: Option<[u8; 3]>,
    sclera_colour: Option<[u8; 3]>,
}

/// Decode a view's labels: a square greyscale PNG whose values name the
/// segmenter's classes.
fn decode_labels(segmentation: &ReconSegmentation) -> Result<(usize, Vec<u8>), String> {
    if segmentation.classes != SEGMENT_CLASSES {
        return Err(
            "the reconstruction worker labelled the photograph with unexpected classes".into(),
        );
    }
    if segmentation.labels_png.len() > MAX_LABEL_PNG_BYTES * 4 / 3 + 4 {
        return Err("the photograph's labels are too large".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&segmentation.labels_png)
        .map_err(|_| "the photograph's labels are not base64".to_string())?;
    let not_png = || "the photograph's labels are not a greyscale PNG".to_string();
    // The header says the size, so nothing larger than a label square is
    // ever decoded.
    let (width, height) = ImageReader::with_format(std::io::Cursor::new(&bytes), ImageFormat::Png)
        .into_dimensions()
        .map_err(|_| not_png())?;
    if width != height || !(1..=MAX_LABEL_SIDE).contains(&width) {
        return Err("the photograph's labels are not a square of a sensible size".into());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_LABEL_SIDE);
    limits.max_image_height = Some(MAX_LABEL_SIDE);
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), ImageFormat::Png);
    reader.limits(limits);
    let labels = reader
        .decode()
        .ok()
        .and_then(|image| match image {
            image::DynamicImage::ImageLuma8(labels) => Some(labels),
            _ => None,
        })
        .ok_or_else(not_png)?;
    if labels.dimensions() != (width, height) {
        return Err(not_png());
    }
    let data = labels.into_raw();
    if data
        .iter()
        .any(|&label| usize::from(label) >= SEGMENT_CLASSES.len())
    {
        return Err("the photograph's labels name an unknown class".into());
    }
    Ok((width as usize, data))
}

/// The part of the head each model triangle belongs to: the scalp and the
/// eyeballs from the model's regions when it has them, and otherwise the
/// eyeballs as the parts beside the head.
fn face_kinds(model: &FlameModel) -> Vec<texture::FaceKind> {
    let set = |names: &[&str]| -> Option<std::collections::HashSet<u32>> {
        let mut vertices = std::collections::HashSet::new();
        for name in names {
            vertices.extend(model.region(name)?.iter().copied());
        }
        Some(vertices)
    };
    let within = |set: &std::collections::HashSet<u32>, face: &[u32; 3]| {
        face.iter().all(|vertex| set.contains(vertex))
    };
    let scalp = set(&["scalp"]);
    let eyes = set(&["left_eyeball", "right_eyeball"]);
    let beside = eyes.is_none().then(|| texture::minor_parts(model.faces()));
    model
        .faces()
        .iter()
        .enumerate()
        .map(|(index, face)| {
            let eye = match (&eyes, &beside) {
                (Some(eyes), _) => within(eyes, face),
                (None, Some(beside)) => beside[index],
                (None, None) => false,
            };
            if eye {
                texture::FaceKind::Eye
            } else if scalp.as_ref().is_some_and(|scalp| within(scalp, face)) {
                texture::FaceKind::Scalp
            } else {
                texture::FaceKind::Skin
            }
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct ReconCrop {
    left: f64,
    top: f64,
    step: f64,
}

/// Project every photograph onto its fitted head, build the colour map and
/// write it as an RGB PNG.
/// The photographs to colour a head from and where the map goes.
struct ColourMapRequest<'a> {
    images: &'a [String],
    photos: &'a [(Snapshot, String)],
    views: &'a [ReconView],
    /// The identity the reconstruction returns, which shapes the head that
    /// `create` makes from it.
    identity: &'a [f32],
    path: &'a str,
    overwrite: bool,
    colouring: Colouring,
}

fn colour_map(
    workspace: &Workspace,
    model: &FlameModel,
    request: ColourMapRequest<'_>,
) -> Result<Value, ToolError> {
    use crate::recon::ScalpChoice;
    let ColourMapRequest {
        images,
        photos,
        views,
        identity,
        path,
        overwrite,
        colouring,
    } = request;
    if views.len() != images.len() {
        return Err(ToolError::Validation(
            "the reconstruction worker described a different number of photographs".into(),
        ));
    }
    let layout = model
        .uv_layout()
        .ok_or_else(|| ToolError::Validation("the model has no texture layout".into()))?;
    let kinds = face_kinds(model);
    let knows_scalp = kinds.contains(&texture::FaceKind::Scalp);
    // The hair colour the photographs show, weighted by how much hair each shows.
    let (hair_pixels, hair_sum) = views
        .iter()
        .filter_map(|view| view.segmentation.as_ref())
        .filter_map(|segmentation| {
            segmentation
                .hair_rgb
                .map(|rgb| (segmentation.hair_pixels, rgb))
        })
        .fold((0u64, [0.0f64; 3]), |(count, sum), (pixels, rgb)| {
            let weight = pixels as f64;
            (
                count + pixels,
                std::array::from_fn(|channel| sum[channel] + weight * f64::from(rgb[channel])),
            )
        });
    let found_hair = (hair_pixels > 0).then(|| {
        hair_sum.map(|channel| (channel / hair_pixels as f64).round().clamp(0.0, 255.0) as u8)
    });
    let hair = colouring.hair_colour.or(found_hair);
    let (scalp, applied, note) = match colouring.scalp {
        // Without a scalp region every face is skin, which takes colour
        // only from skin, whatever was asked for the scalp.
        _ if !knows_scalp => (
            texture::ScalpColour::Skin,
            "skin",
            Some(
                "the model has no scalp region; convert it again with --masks to colour the scalp separately",
            ),
        ),
        ScalpChoice::Photo => (texture::ScalpColour::Photo, "photo", None),
        ScalpChoice::Skin => (texture::ScalpColour::Skin, "skin", None),
        ScalpChoice::Hair => match hair {
            Some(colour) => (texture::ScalpColour::Hair(colour), "hair", None),
            None => (
                texture::ScalpColour::Skin,
                "skin",
                Some("no hair was found in the photographs"),
            ),
        },
    };
    let mut builder = texture::ColourMapBuilder::new(
        model.faces(),
        layout,
        colouring.size,
        &kinds,
        texture::Colouring {
            scalp,
            lighting: match colouring.lighting {
                crate::recon::LightingChoice::Remove => texture::Lighting::Remove,
                crate::recon::LightingChoice::Keep => texture::Lighting::Keep,
            },
        },
    )
    .map_err(|error| ToolError::Validation(error.to_string()))?;
    let knows_eyes = kinds.contains(&texture::FaceKind::Eye);
    if colouring.eyes == crate::recon::EyeChoice::Painted && knows_eyes {
        // The eyes are painted on the returned head at rest, the head that
        // `create` makes, where each looks along +z.
        let rest = model
            .evaluate(&FlameParams {
                identity: identity.to_vec(),
                expression: Vec::new(),
                pose: Pose::default(),
                eyelids: [0.0; 2],
                translation: [0.0; 3],
            })
            .map_err(|error| ToolError::Validation(error.to_string()))?;
        builder
            .paint_eyes(
                &rest.vertices,
                texture::PaintedEyes {
                    iris: colouring.iris_colour,
                    sclera: colouring.sclera_colour.unwrap_or(DEFAULT_SCLERA_RGB),
                    iris_diameter: PAINTED_IRIS_M,
                    pupil_diameter: PAINTED_PUPIL_M,
                },
            )
            .map_err(|error| ToolError::Validation(error.to_string()))?;
    }
    let mut segmented = 0usize;
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
                eyelids: [0.0; 2],
                translation: [0.0; 3],
            })
            .map_err(|error| ToolError::Validation(format!("{image}: {error}")))?;
        let labels = view
            .segmentation
            .as_ref()
            .map(|segmentation| decode_labels(segmentation).map(|labels| (segmentation, labels)))
            .transpose()
            .map_err(|message| ToolError::Validation(format!("{image}: {message}")))?;
        segmented += usize::from(labels.is_some());
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
                labels: labels
                    .as_ref()
                    .map(|(segmentation, (side, data))| texture::Labels {
                        side: *side,
                        data,
                        left: segmentation.crop.left,
                        top: segmentation.crop.top,
                        step: segmentation.crop.step,
                    }),
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
    let mut scalp_report = json!({
        "requested": colouring.scalp,
        "applied": applied,
        "hair_colour": if applied == "hair" { hair.map(|colour| json!(colour)) } else { None },
        "hair_found": found_hair,
    });
    if let Some(note) = note {
        scalp_report["note"] = json!(note);
    }
    let mut eye_report = json!({"requested": colouring.eyes});
    match map.iris {
        Some((iris, source)) => {
            eye_report["applied"] = json!("painted");
            eye_report["iris_colour"] = json!(iris);
            eye_report["iris_source"] = json!(match source {
                texture::IrisSource::Given => "given",
                texture::IrisSource::Measured => "measured",
                texture::IrisSource::Default => "default",
            });
        }
        None => {
            eye_report["applied"] = json!("photo");
            if colouring.eyes == crate::recon::EyeChoice::Painted {
                eye_report["note"] = json!("the model has no separate eyeballs to paint");
            }
        }
    }
    Ok(json!({
        "artifact": artifact,
        "width": side,
        "height": side,
        "seen": map.seen,
        "skin_only": segmented == views.len(),
        "scalp": scalp_report,
        "eyes": eye_report,
        "lighting": {
            "requested": colouring.lighting,
            // Each photograph's shading in linear light, as ambient plus
            // direction · normal per colour channel with the direction in
            // the head's own frame, or null where it was kept or too little
            // skin was seen to estimate it.
            "shading": map.shading.iter().zip(views.iter()).map(|(shading, view)| {
                shading.map(|shading| {
                    let shading = shading.turned_back(view.head_rotation);
                    json!({"ambient": shading.ambient, "direction": shading.direction})
                })
            }).collect::<Vec<_>>(),
        },
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

/// Read a displacement map from the workspace, filter it as requested and
/// pair it with the model's texture coordinates. Also describes the filtering.
/// With `limit_relief`, a relief beyond the limit for detail is reduced to it
/// rather than refused. Detail fades to nothing at the `faded` vertices.
async fn displacement(
    workspace: &Arc<Workspace>,
    model: &Arc<FlameModel>,
    head: &PosedHead,
    params: DisplacementParams,
    (limit_relief, faded): (bool, Vec<u32>),
) -> Result<(Displacement, Value), ToolError> {
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
    let path = params.path.clone();
    let (_, bytes) = blocking(move || workspace.read_artifact(&path)).await?;
    let (width, height, fractions) = decode_displacement(&bytes)?;
    let mut values: Vec<f32> = fractions
        .into_iter()
        .map(|fraction| (params.offset_mm + params.scale_mm * fraction) as f32)
        .collect();
    let model = Arc::clone(model);
    let vertices = millimetres(&head.vertices);
    let filter = detail::DetailFilter {
        high_pass_mm: params.high_pass_mm,
        clip_percentile: params.clip_percentile,
    };
    let gain = params.gain.unwrap_or(1.0);
    let (values, report) = blocking(move || {
        let layout = model
            .uv_layout()
            .ok_or_else(|| ToolError::Validation("the model has no texture layout".into()))?;
        let report = detail::filter_detail(
            &mut values,
            width,
            height,
            model.faces(),
            layout,
            &vertices,
            filter,
        )
        .map_err(|error| ToolError::Validation(error.to_string()))?;
        for value in &mut values {
            *value = (f64::from(*value) * gain) as f32;
        }
        Ok::<_, ToolError>((values, report))
    })
    .await?;
    let description = json!({
        "millimetres_per_texel": report.millimetres_per_texel,
        "high_pass_mm": params.high_pass_mm,
        "clip_percentile": params.clip_percentile,
        "clipped_at_mm": report.clipped_at,
        "gain": gain,
        "relief_mm": params.relief_mm,
    });
    Ok((
        Displacement {
            width,
            height,
            values,
            triangle_uvs,
            relief_mm: params.relief_mm,
            limit_relief,
            faded,
        },
        description,
    ))
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
    (identity, expression, expression_scale, eyelids): (
        Vec<f32>,
        Vec<f32>,
        Option<f64>,
        Option<[f32; 2]>,
    ),
    pose: HeadPose,
) -> Result<(Arc<FlameModel>, PosedHead), ToolError> {
    let model = Arc::clone(
        library(settings)?
            .select(variant)
            .map_err(ToolError::Validation)?,
    );
    check_coefficients("identity", &identity)?;
    check_coefficients("expression", &expression)?;
    let scale = in_range(
        "expression_scale",
        expression_scale.unwrap_or(1.0),
        &EXPRESSION_SCALE,
    )? as f32;
    let expression = expression.iter().map(|value| value * scale).collect();
    let eyelids = eyelids.unwrap_or_default();
    if !eyelids.iter().all(|share| (0.0..=1.0).contains(share)) {
        return Err(ToolError::Validation(
            "eyelids must each be between 0 and 1".into(),
        ));
    }
    if eyelids.iter().any(|&share| share > 0.0) && !model.has_eyelids() {
        return Err(ToolError::Validation(format!(
            "the {} model has no eyelid basis; convert it again with --eyelids to close the eyelids",
            model.info().variant
        )));
    }
    let eyelids = eyelids.map(|share| share * scale);
    let pose = Pose {
        global: check_rotation("global", pose.global)?,
        neck: check_rotation("neck", pose.neck)?,
        jaw: check_rotation("jaw", pose.jaw)?.map(|value| value * scale),
        left_eye: check_rotation("left_eye", pose.left_eye)?,
        right_eye: check_rotation("right_eye", pose.right_eye)?,
    };
    let params = FlameParams {
        identity,
        expression,
        pose,
        eyelids,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        (0..3).all(|axis| (a[axis] - b[axis]).abs() < 1e-9)
    }

    #[test]
    fn the_whole_rotation_is_kept_with_full_shares() {
        let rotation = [0.2, 0.1, 0.05];
        let neck = suggested_neck(rotation, 1.0, 1.0);
        assert!(close(neck, rotation), "{neck:?}");
        assert_eq!(suggested_neck(rotation, 0.0, 0.0), [0.0; 3]);
    }

    #[test]
    fn a_half_turn_is_accepted_as_a_pose() {
        let half = f64::from(std::f32::consts::PI);
        let neck = suggested_neck([0.0, 0.0, half], 1.0, 1.0);
        let length = neck.iter().map(|value| value * value).sum::<f64>().sqrt();
        assert!((length - std::f64::consts::PI).abs() < 1e-4, "{neck:?}");
        assert!(neck[0].abs() < 1e-6 && neck[1].abs() < 1e-6, "{neck:?}");
        // Read in single precision, as `create` reads a pose, every
        // suggestion near a half turn passes its check.
        let mut rotations = vec![[1.0471975803375244, 2.094395160675049, 2.094395160675049]];
        for step in 0..400 {
            let (polar, around) = (f64::from(step % 20) * 0.16, f64::from(step / 20) * 0.33);
            let axis = [
                polar.sin() * around.cos(),
                polar.cos(),
                polar.sin() * around.sin(),
            ];
            for angle in [std::f64::consts::PI, half, std::f64::consts::PI - 1e-7] {
                rotations.push(axis.map(|value| value * angle));
            }
        }
        for rotation in rotations {
            let neck = suggested_neck(rotation, 1.0, 1.0).map(|value| value as f32);
            assert!(
                check_rotation("neck", neck).is_ok(),
                "{rotation:?} {neck:?}"
            );
        }
    }

    #[test]
    fn turn_and_tilt_are_kept_separately() {
        // A pure turn is dropped by the tilt and halved by a half turn.
        assert!(close(suggested_neck([0.0, 0.3, 0.0], 1.0, 0.0), [0.0; 3]));
        assert!(close(
            suggested_neck([0.0, 0.3, 0.0], 0.0, 0.5),
            [0.0, 0.15, 0.0]
        ));
        // A pure roll or pitch follows the tilt alone.
        assert!(close(
            suggested_neck([0.0, 0.0, 0.2], 0.5, 1.0),
            [0.0, 0.0, 0.1]
        ));
        assert!(close(
            suggested_neck([0.2, 0.0, 0.0], 0.5, 0.0),
            [0.1, 0.0, 0.0]
        ));
    }
}
