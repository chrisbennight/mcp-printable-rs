//! Printable busts from an evaluated head: one open head surface plus closed
//! attached parts such as eyeballs.
//!
//! Each boundary loop is closed with a fan of triangles, so every part becomes
//! a closed surface. The parts are rotated to stand upright, scaled to the
//! requested height, smoothed and refined to the requested edge length,
//! optionally raised by a displacement map over the head's texture
//! coordinates, and rebuilt as one valid solid, which is trimmed by a
//! horizontal plane just above the lowest opening to give a flat base. An
//! optional plinth is added below, and the result can be hollowed with drain
//! holes through the base.

use std::collections::{HashMap, HashSet};

use manifold_csg::{Manifold, MeshGL64, MeshGL64Options};
use serde::{Deserialize, Serialize};

use crate::print_check::{self, PrintChecks, SurfaceFinding};
use crate::{GeometryError, TriangleMesh, remesh};

/// Smoothing treats every edge as smooth. The kernel emits zero-area
/// triangles when smoothing preserves sharp edges, so parts are smoothed
/// before the flat base exists and the base is cut afterwards.
const SMOOTH_EVERY_EDGE_DEGREES: f64 = 180.0;
/// Faceting is estimated where facets meet at less than this angle; sharper
/// creases are intended features such as the base rim.
const SMOOTH_REGION_RADIANS: f64 = std::f64::consts::PI / 6.0;
/// Fraction of the way toward the centre of its neighbours that each vertex
/// at a touching point moves, up to a limit so that coarse triangles, such as
/// those inside a hollow bust, are not distorted.
const PINCH_SEPARATION: f64 = 0.25;
const PINCH_SEPARATION_LIMIT_MM: f64 = 0.05;
/// The trim plane sits this fraction of the head's height above the top of the
/// lowest opening, so the cut is a full section through the neck.
const NECK_MARGIN: f64 = 0.01;
/// Separate shells smaller than this fraction of the bust's volume are
/// discarded. An eyeball on a bust is roughly a thousandth of its volume.
const FRAGMENT_VOLUME_FRACTION: f64 = 1e-5;
/// Refinement to an edge length L yields about this many triangles per unit of
/// surface area divided by L squared.
const TRIANGLES_PER_AREA: f64 = 2.6;
/// Round parts are faceted so that no chord is farther than this from the
/// circle.
const CIRCLE_TOLERANCE_MM: f64 = 0.01;
/// A default plinth is this fraction of the bust's widest extent across, and
/// reaches at least this far beyond the base's outline, whose edge would
/// otherwise touch the plinth's.
const PLINTH_WIDTH_FRACTION: f64 = 0.7;
const PLINTH_MARGIN_MM: f64 = 1.0;
/// Marching tetrahedra with mean edge L yields about 3.1 triangles per unit
/// of area divided by L squared; this allows a margin for the drain holes.
const CAVITY_TRIANGLES_PER_AREA: f64 = 3.5;
/// Attempts at fitting the finished bust within its triangle limit.
const FINISH_ATTEMPTS: usize = 3;
/// Share of the triangle limit kept for the cavity of a hollow bust.
const CAVITY_SHARE: f64 = 0.25;
/// Drain holes reach this far into the cavity.
const DRAIN_OVERLAP_MM: f64 = 1.0;
/// The base outline is sampled this far above the base plane.
const SECTION_OFFSET_MM: f64 = 0.01;
/// Largest displacement map side accepted, in samples.
const MAX_DISPLACEMENT_SIDE: usize = 1024;
/// Unless sized, an extended neck is this share of the head's height, and
/// shoulders are these shares of the head's height, width and depth.
const NECK_EXTENSION_SHARE: f64 = 0.15;
const SHOULDER_HEIGHT_SHARE: f64 = 0.35;
const SHOULDER_WIDTH_SHARE: f64 = 1.6;
const SHOULDER_DEPTH_SHARE: f64 = 0.9;
/// The shoulders' top edge is this share of their height above their base,
/// and their top is the neck's section reduced to this share of its size.
const SHOULDER_LINE_SHARE: f64 = 0.6;
const SHOULDER_TOP_SHARE: f64 = 0.9;
/// Unsized shoulders are at least this share of the neck's span across, which
/// holds any section of that span reduced to their top's share.
const SHOULDER_NECK_SHARE: f64 = 1.3;
/// The neck's direction is taken between its base and this share of the
/// head's height above it, and a body leans at most this far from vertical.
const NECK_DIRECTION_SHARE: f64 = 0.08;
const MAX_LEAN_DEGREES: f64 = 30.0;
/// A part beside the head, such as an eyeball, is joined to it by a hidden
/// stem this share of its radius thick, reaching this share of its radius
/// from its centre toward the head's.
const STEM_RADIUS_SHARE: f64 = 0.35;
const STEM_LENGTH_SHARE: f64 = 2.5;
/// The head keeps at least this share of the height above any plinth.
const MIN_HEAD_SHARE: f64 = 0.5;
/// Life-size eye detail: a pupil 4 mm across, drilled 2 mm deep, in an iris
/// 11.5 mm across, whose engraved ring is a tenth of the iris wide.
const LIFE_PUPIL_MM: f64 = 4.0;
const LIFE_PUPIL_DEPTH_MM: f64 = 2.0;
const LIFE_IRIS_MM: f64 = 11.5;
const RING_WIDTH_SHARE: f64 = 0.1;
/// Eye detail must stay within these shares of the eyeball's radius: the
/// pupil and the depth within half of it, the iris within nine tenths.
const MAX_PUPIL_SHARE: f64 = 0.5;
const MAX_IRIS_SHARE: f64 = 0.9;
const MAX_DEPTH_SHARE: f64 = 0.5;
/// A mouth cap's centre may be set back at most this fraction of the head's
/// height, which keeps it well inside the head.
const MAX_RECESS_FRACTION: f64 = 0.2;
/// Displacement is surface detail; values beyond this fraction of the head's
/// height are not.
const MAX_DISPLACEMENT_FRACTION: f64 = 0.05;
/// Map samples and applied distances below this share of the largest
/// magnitude are not detail. Maps stored as 16-bit integers carry rounding
/// noise of about one part in 65,535 where they are meant to be zero.
const DETAIL_SHARE: f64 = 0.005;
/// `relief_mm` sets the depth of detail at this percentile of its magnitude.
const RELIEF_PERCENTILE: f64 = 95.0;
/// A map whose detail at that percentile is shallower than this, in the
/// request's millimetres, holds rounding noise rather than detail, and
/// `relief_mm` leaves it unscaled.
const MIN_RELIEF_REFERENCE_MM: f64 = 1e-3;
const MIN_CIRCLE_SEGMENTS: i32 = 16;
const MAX_CIRCLE_SEGMENTS: i32 = 2048;

/// Inputs to [`prepare_bust`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BustRequest {
    /// Head vertices in millimetres, with y up and the face looking along +z.
    pub vertices: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Height of the finished bust, base to crown.
    pub height_mm: f64,
    /// Target edge length after refinement.
    pub edge_length_mm: f64,
    /// Upper bound on the finished triangle count; the edge length grows to
    /// respect it.
    pub max_triangles: usize,
    /// A cylindrical plinth under the bust, included in `height_mm`.
    #[serde(default)]
    pub plinth: Option<Plinth>,
    /// Hollow the finished bust, leaving walls of this thickness, with drain
    /// holes through the base.
    #[serde(default)]
    pub hollow: Option<Hollow>,
    /// Fine surface detail raised along the normal of the smoothed surface.
    #[serde(default)]
    pub displacement: Option<Displacement>,
    /// Check the finished bust for overhangs and thin parts.
    #[serde(default)]
    pub checks: Option<PrintChecks>,
    /// Carve each eye with a pupil and, optionally, an iris ring.
    #[serde(default)]
    pub eyes: Option<EyeCarving>,
    /// A body below the head, within `height_mm` above any plinth.
    #[serde(default)]
    pub body: Option<Body>,
    /// Set the centre of the cap closing each opening other than the neck,
    /// the mouth, this many millimetres of the finished bust back into the
    /// head, along the cap's inward normal, so that an open mouth reads as a
    /// shadow rather than a flat surface.
    #[serde(default)]
    pub mouth_recess_mm: Option<f64>,
}

/// What stands below the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    /// The neck continued down along its own direction.
    ExtendedNeck,
    /// A block of chest and shoulders below the neck.
    Shoulders,
}

/// A body below the head, in millimetres on the finished bust. Unsized
/// measures default to shares of the head's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub kind: BodyKind,
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// Shoulders only.
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// Shoulders only.
    #[serde(default)]
    pub depth_mm: Option<f64>,
}

/// The body added, in millimetres on the finished bust.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BodyReport {
    pub kind: BodyKind,
    pub height_mm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_mm: Option<f64>,
    /// How far the neck leans from vertical, which an extended neck follows.
    pub lean_degrees: f64,
}

/// Carving for the eyes: a drilled pupil and, optionally, an engraved ring at
/// the edge of the iris, centred on each eye's gaze. Sizes are in
/// millimetres on the finished bust; each defaults to its life size scaled
/// with the bust, and to no less than the printer can form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EyeCarving {
    /// Each eye, in the request's units and frame.
    pub eyes: Vec<EyeAxis>,
    /// Engrave a ring at the edge of the iris as well.
    #[serde(default)]
    pub iris_ring: bool,
    #[serde(default)]
    pub pupil_diameter_mm: Option<f64>,
    #[serde(default)]
    pub iris_diameter_mm: Option<f64>,
    /// How deep the pupil is drilled; the ring is half as deep.
    #[serde(default)]
    pub depth_mm: Option<f64>,
    /// The narrowest feature the printer forms, such as its nozzle.
    #[serde(default)]
    pub smallest_mm: Option<f64>,
    /// The shallowest feature the printer forms, such as two layers.
    #[serde(default)]
    pub shallowest_mm: Option<f64>,
}

/// An eye: the centre of its eyeball and the point on the eyeball's surface
/// where its gaze leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EyeAxis {
    pub centre: [f64; 3],
    pub front: [f64; 3],
}

/// The eye carving applied, in millimetres on the finished bust.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EyeReport {
    pub pupil_diameter_mm: f64,
    pub depth_mm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iris_diameter_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_width_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_depth_mm: Option<f64>,
    /// The smallest eyeball carved, across.
    pub eyeball_diameter_mm: f64,
    /// The volume carved away; zero when the surface covers every cut.
    pub removed_mm3: f64,
}

/// A displacement map over the head's texture coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Displacement {
    pub width: usize,
    pub height: usize,
    /// Distance along the outward normal, in the units of `vertices`, row by
    /// row. Row 0 is v = 1 and column 0 is u = 0; samples are interpolated
    /// bilinearly.
    pub values: Vec<f32>,
    /// Texture coordinates of the three corners of each request triangle.
    pub triangle_uvs: Vec<[[f32; 2]; 3]>,
    /// Scale the map so that the 95th percentile of its magnitude, sampled at
    /// every triangle's corners and centre where it carries detail, is this
    /// deep on the finished bust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relief_mm: Option<f64>,
    /// Where `relief_mm` would raise detail beyond the limit for detail,
    /// reduce the relief to the limit instead of refusing the request.
    #[serde(default)]
    pub limit_relief: bool,
    /// Request vertices, in increasing order, where the detail fades to
    /// nothing, such as the lips of a closed mouth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faded: Vec<u32>,
}

impl Displacement {
    fn sample(&self, u: f64, v: f64) -> f64 {
        let x = u.clamp(0.0, 1.0) * (self.width - 1) as f64;
        let y = (1.0 - v.clamp(0.0, 1.0)) * (self.height - 1) as f64;
        let column = (x as usize).min(self.width - 2);
        let row = (y as usize).min(self.height - 2);
        let (across, down) = (x - column as f64, y - row as f64);
        let at = |row: usize, column: usize| f64::from(self.values[row * self.width + column]);
        let top = at(row, column) * (1.0 - across) + at(row, column + 1) * across;
        let bottom = at(row + 1, column) * (1.0 - across) + at(row + 1, column + 1) * across;
        top * (1.0 - down) + bottom * down
    }
}

/// A cylindrical plinth.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plinth {
    pub height_mm: f64,
    /// Defaults to 70% of the bust's widest extent, and reaches at least
    /// 1 mm beyond the base it supports.
    #[serde(default)]
    pub diameter_mm: Option<f64>,
    /// The bottom edge is cut back this far at 45 degrees, so that the first
    /// layers, which spread as they are pressed onto the build plate, stay
    /// within the plinth's outline. Zero for a square edge.
    #[serde(default)]
    pub chamfer_mm: f64,
}

/// Hollowing for resin printing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hollow {
    pub wall_mm: f64,
    pub drain_diameter_mm: f64,
}

/// The plinth that was added.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlinthReport {
    pub height_mm: f64,
    pub diameter_mm: f64,
    pub chamfer_mm: f64,
}

/// The cavity that was removed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HollowReport {
    pub wall_mm: f64,
    pub cavity_volume_mm3: f64,
    pub drain_diameter_mm: f64,
    /// Centres of the drain holes on the base, in millimetres.
    pub drain_holes_mm: Vec<[f64; 2]>,
}

/// Measurements of a prepared bust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BustReport {
    pub triangles: usize,
    /// Extent along x, y and z. The base rests on z = 0 and the face looks
    /// toward -y.
    pub size_mm: [f64; 3],
    pub volume_mm3: f64,
    pub requested_edge_length_mm: f64,
    /// Edge length used, larger than requested only when the triangle limit
    /// required it.
    pub edge_length_mm: f64,
    /// Estimated distance between the facets and the smooth surface they
    /// approximate, at the 99.9th percentile. Each edge contributes L·θ/8,
    /// the gap between a chord of length L and a circular arc turning
    /// through θ, the angle between the edge's two faces. Edges where faces
    /// meet at 30 degrees or more are creases rather than faceting and are
    /// left out. Measured before any plinth or hollowing. Absent when there
    /// is no such edge.
    pub estimated_deviation_p999_mm: Option<f64>,
    /// Whether a lowest opening was found and the head trimmed above it.
    pub trimmed_at_neck: bool,
    /// The bust's size as a fraction of the life-size head.
    pub life_size_fraction: f64,
    /// Separate shells of negligible volume that were discarded.
    pub discarded_fragments: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plinth: Option<PlinthReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hollow: Option<HollowReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub displacement: Option<DisplacementReport>,
    /// Surface facing down more steeply than the checked angle, measured
    /// before any plinth was added. The faces resting on the base are left
    /// out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhang: Option<SurfaceFinding>,
    /// Surface where the finished solid is thinner than the checked size,
    /// measured straight in from each face.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thin: Option<SurfaceFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eyes: Option<EyeReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyReport>,
    /// How far the mouth was set back on the finished bust: the requested
    /// depth, which was applied before smoothing, times the height
    /// correction that followed. Absent when no mouth was recessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouth_recess_mm: Option<f64>,
}

/// The detail that was applied, in millimetres at the finished size: the
/// deepest depression, negative, and the highest raise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplacementReport {
    pub lowest_mm: f64,
    pub highest_mm: f64,
    /// Percentiles of the distance applied at the surface's vertices where
    /// the map carries detail. Absent when no vertex does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentiles_mm: Option<DetailPercentiles>,
    /// The 95th percentile of the applied distance's magnitude at those
    /// vertices, raised and recessed alike: the depth `relief_mm` aims for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_p95_mm: Option<f64>,
    /// Factor the map was multiplied by to reach `relief_mm`; 1 without it.
    pub relief_scale: f64,
    /// Whether the relief was reduced to keep detail within its limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relief_limited: bool,
}

/// Percentiles of applied detail, in millimetres at the finished size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DetailPercentiles {
    pub p1: f64,
    pub p5: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Build a closed, printable bust from an open head surface and its parts.
pub fn prepare_bust(request: &BustRequest) -> Result<(TriangleMesh, BustReport), GeometryError> {
    validate(request)?;
    let mut minimum_edge = request.edge_length_mm;
    for _ in 0..FINISH_ATTEMPTS {
        match prepare_at(request, minimum_edge)? {
            Prepared::Done(mesh, report) => return Ok((mesh, *report)),
            // A plinth or cavity added more triangles than expected; coarsen
            // the whole bust in proportion and try again.
            Prepared::OverLimit { triangles, edge } => {
                minimum_edge =
                    edge * (triangles as f64 / request.max_triangles as f64).sqrt() * 1.05;
            }
        }
    }
    Err(GeometryError::Kernel(
        "the finished bust exceeds its triangle limit".into(),
    ))
}

enum Prepared {
    Done(TriangleMesh, Box<BustReport>),
    OverLimit { triangles: usize, edge: f64 },
}

fn prepare_at(request: &BustRequest, minimum_edge: f64) -> Result<Prepared, GeometryError> {
    let available = request.height_mm - request.plinth.map_or(0.0, |plinth| plinth.height_mm);
    // The head's own height; any body takes the rest above the plinth.
    let bust_height = match request.body {
        Some(Body {
            height_mm: Some(height),
            ..
        }) => available - height,
        Some(body) => available / (1.0 + body.kind.default_share()),
        None => available,
    };
    if bust_height < MIN_HEAD_SHARE * available {
        return Err(GeometryError::InvalidOptions(format!(
            "the body would leave the head less than {:.0}% of the height above the plinth",
            MIN_HEAD_SHARE * 100.0
        )));
    }
    let mut parts = closed_parts(&request.vertices, &request.triangles)?;
    let heights = request.vertices.iter().map(|vertex| vertex[1]);
    let span = heights.clone().fold(f64::MIN, f64::max) - heights.fold(f64::MAX, f64::min);
    // Closed parts have no openings, so the lowest opening is the neck.
    let cut = parts
        .iter()
        .flat_map(|part| &part.openings)
        .min_by(|a, b| a.mean_height.total_cmp(&b.mean_height))
        .map(|neck| neck.top_height + NECK_MARGIN * span);

    // Rotate from y up with the face toward +z to z up with the face toward -y.
    let upright = |[x, y, z]: [f64; 3]| [x, -z, y];
    let kept_height = |vertex: &[f64; 3]| cut.is_none_or(|height| vertex[1] >= height);
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in parts
        .iter()
        .flat_map(|part| &part.vertices)
        .filter(|vertex| kept_height(vertex))
    {
        let vertex = upright(*vertex);
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    // The base is the cut plane itself, not the lowest vertex above it, so
    // the trim below happens exactly where the neck section was chosen.
    if let Some(height) = cut {
        low[2] = height;
    }
    // Vertices are finite, so this also catches the case of none kept.
    if high[2] <= low[2] {
        return Err(GeometryError::Kernel("the trimmed bust is empty".into()));
    }
    // Place and scale before smoothing, so refinement works in final units.
    let scale = bust_height / (high[2] - low[2]);
    let recessed = match request.mouth_recess_mm {
        Some(recess) => (recess_mouth(&mut parts, recess / scale, span)? > 0).then_some(recess),
        None => None,
    };
    let centre = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0, low[2]];
    let place = |vertex: [f64; 3]| {
        let vertex = upright(vertex);
        [0, 1, 2].map(|axis| (vertex[axis] - centre[axis]) * scale)
    };
    let eyeballs = match &request.eyes {
        Some(carving) => eyeball_parts(&parts, carving)?,
        None => Vec::new(),
    };
    // A carved eyeball keeps the surface its carving is measured from, and
    // skin detail does not belong on it anyway, so its vertices are faded.
    let detail = request.displacement.as_ref().map(|map| {
        let mut map = map.clone();
        map.faded.extend(
            eyeballs
                .iter()
                .flat_map(|&part| parts[part].origins.iter().copied()),
        );
        map.faded.sort_unstable();
        map.faded.dedup();
        map
    });
    let rims: Vec<Vec<u32>> = parts
        .iter()
        .flat_map(|part| part.openings.iter().map(|opening| opening.rim.clone()))
        .collect();
    let relieved = match &detail {
        Some(map) => Some(with_relief(
            map,
            (&request.vertices, &request.triangles, &rims),
            scale,
            span,
        )?),
        None => None,
    };
    let displacement = relieved.as_ref().map(|(map, _, _)| map);
    let smooth = parts
        .iter()
        .map(|part| {
            part_solid(part, place, displacement)
                .map(|solid| solid.smooth_out(SMOOTH_EVERY_EDGE_DEGREES, 0.0))
                .map_err(|error| {
                    GeometryError::Kernel(format!(
                        "a closed head part is not a closed surface: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let base = cut.map(|_| 0.0);

    // A cavity needs triangles too; it gets a share of the limit.
    let surface_limit = if request.hollow.is_some() {
        (request.max_triangles as f64 * (1.0 - CAVITY_SHARE)) as usize
    } else {
        request.max_triangles
    };
    let limit = surface_limit as f64;
    let area: f64 = smooth.iter().map(Manifold::surface_area).sum();
    let mut edge = minimum_edge.max((TRIANGLES_PER_AREA * area / limit).sqrt());
    // The smoothed parts overlap one another and can pass through themselves
    // where FLAME's surface folds, at the lips and ears. Remeshing their
    // union yields one valid solid; the flat base is cut afterwards.
    let build = |edge: f64| -> Result<(Manifold, Vec<f64>), GeometryError> {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        let mut distances = Vec::new();
        for part in &smooth {
            let refined = part.refine_to_length(edge);
            let mesh = match displacement {
                Some(map) => {
                    let (mesh, applied) = displaced(&refined, map, scale);
                    distances.extend(applied);
                    mesh
                }
                None => mesh_from(&refined),
            };
            drop(refined);
            let offset =
                u32::try_from(vertices.len()).map_err(|_| GeometryError::TooManyVertices)?;
            vertices.extend(mesh.vertices);
            triangles.extend(
                mesh.triangles
                    .iter()
                    .map(|triangle| triangle.map(|index| index + offset)),
            );
        }
        let solid = remesh::remesh_union(vertices, triangles, edge)?;
        let solid = match base {
            Some(height) => solid.trim_by_plane([0.0, 0.0, 1.0], height),
            None => solid,
        };
        Ok((solid, distances))
    };
    // Large meshes are dropped as soon as they are replaced, so that no more
    // than two copies are alive within the worker's memory limit.
    let (mut refined, mut distances) = build(edge)?;
    if refined.num_tri() > surface_limit {
        // The area estimate was low; size the edge from the observed count.
        edge *= (refined.num_tri() as f64 / limit).sqrt() * 1.02;
        drop(refined);
        (refined, distances) = build(edge)?;
    }
    if refined.num_tri() > surface_limit {
        return Err(GeometryError::Kernel(
            "the refined bust exceeds its triangle limit".into(),
        ));
    }
    refined.status().map_err(|error| {
        GeometryError::Kernel(format!("the refined bust is not a valid solid: {error}"))
    })?;
    let refined = join_parts(refined, &parts, place)?;
    // Thin gaps can leave specks of almost no volume as separate shells. They
    // cannot print, so drop them; anything larger means the parts did not join.
    let pieces = refined.decompose();
    drop(refined);
    let total: f64 = pieces.iter().map(Manifold::volume).sum();
    let count = pieces.len();
    let mut kept: Vec<Manifold> = pieces
        .into_iter()
        .filter(|piece| piece.volume() > FRAGMENT_VOLUME_FRACTION * total)
        .collect();
    if kept.len() != 1 {
        return Err(GeometryError::Kernel(format!(
            "the bust is {} separate solids rather than one",
            kept.len()
        )));
    }
    let fragments = count - 1;
    let piece = kept.pop().expect("one piece is kept");
    // Smoothing moves the surface slightly off the coarse mesh, so restore the
    // exact requested height and recentre.
    let bounds = piece
        .bounding_box()
        .ok_or_else(|| GeometryError::Kernel("the refined bust has no extent".into()))?;
    let (low, high) = (bounds.min(), bounds.max());
    let correction = bust_height / (high[2] - low[2]);
    let solid = piece
        .translate(
            -(low[0] + high[0]) / 2.0,
            -(low[1] + high[1]) / 2.0,
            -low[2],
        )
        .scale(correction, correction, correction);
    drop(piece);
    // Edges were sized before the height correction; retries need that size.
    let refined_edge = edge;
    edge *= correction;
    // The request vertices that survive the trim, where they sit on the
    // finished bust before any plinth raises it. They name the parts of the
    // head where checks find something.
    let offset = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0, low[2]];
    let to_final = |point: [f64; 3]| {
        let placed = place(point);
        [0, 1, 2].map(|axis| (placed[axis] - offset[axis]) * correction)
    };
    let (solid, eyes) = match &request.eyes {
        Some(carving) => {
            let placing = (offset, correction, scale * correction);
            let eyes = (&eyeballs[..], &parts[..]);
            let (solid, report) = carve_eyes(solid, carving, eyes, place, to_final, placing, edge)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let body_height = available - bust_height;
    let (solid, body) = match request.body {
        Some(_) if cut.is_none() => {
            return Err(GeometryError::InvalidOptions(
                "a body needs a neck opening to join".into(),
            ));
        }
        Some(body) => {
            let (solid, report) = add_body(solid, body, (body_height, bust_height), edge)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let mut head: Vec<(u32, [f64; 3])> = match request.checks {
        Some(_) => request
            .vertices
            .iter()
            .enumerate()
            .filter(|(_, vertex)| kept_height(vertex))
            .map(|(index, vertex)| {
                let [x, y, z] = to_final(*vertex);
                (index as u32, [x, y, z + body_height])
            })
            .collect(),
        None => Vec::new(),
    };
    // Measured on the bust alone: the plinth is exact to the circle tolerance
    // and a cavity's coarser facets are hidden inside. Overhangs are found
    // before the plinth, whose chamfer is meant to slope.
    let surface = mesh_from(&solid);
    let estimated_deviation_p999_mm = estimated_deviation_p999(&surface);
    let mut overhang = request
        .checks
        .map(|checks| print_check::overhang(&surface, checks.overhang_degrees, &head));
    drop(surface);
    let (solid, plinth) = match request.plinth {
        Some(plinth) if cut.is_some() => {
            let (solid, report) = add_plinth(solid, plinth)?;
            (solid, Some(report))
        }
        Some(_) => {
            return Err(GeometryError::InvalidOptions(
                "a plinth needs a neck opening to stand on".into(),
            ));
        }
        None => (solid, None),
    };
    // A plinth raises the bust.
    let raise = plinth.map_or(0.0, |plinth| plinth.height_mm);
    for (_, point) in &mut head {
        point[2] += raise;
    }
    for region in overhang.iter_mut().flat_map(|found| &mut found.regions) {
        region.centre_mm[2] += raise;
    }
    let (solid, hollow) = match request.hollow {
        Some(hollow) => {
            let budget = request.max_triangles.saturating_sub(solid.num_tri());
            let (solid, report) = hollow_out(solid, hollow, budget)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let size = solid
        .bounding_box()
        .map(|bounds| bounds.dimensions())
        .ok_or_else(|| GeometryError::Kernel("the refined bust has no extent".into()))?;
    let volume_mm3 = solid.volume();
    let mesh = single_precision(&solid)?;
    drop(solid);
    if mesh.triangles.len() > request.max_triangles {
        return Ok(Prepared::OverLimit {
            triangles: mesh.triangles.len(),
            edge: refined_edge,
        });
    }
    // Measured on the finished solid, so a hollow bust's walls count, but
    // not on the plinth.
    let thin = match request.checks.and_then(|checks| checks.min_feature_mm) {
        Some(min_mm) => Some(print_check::thin(&mesh, min_mm, raise, &head)?),
        None => None,
    };
    let report = BustReport {
        triangles: mesh.triangles.len(),
        size_mm: size,
        volume_mm3,
        requested_edge_length_mm: request.edge_length_mm,
        edge_length_mm: edge,
        estimated_deviation_p999_mm,
        trimmed_at_neck: cut.is_some(),
        life_size_fraction: scale * correction,
        discarded_fragments: fragments,
        plinth,
        hollow,
        displacement: relieved.map(|(_, relief_scale, limited)| {
            detail_report(&mut distances, correction, (relief_scale, limited))
        }),
        overhang,
        thin,
        eyes,
        body,
        mouth_recess_mm: recessed.map(|recess| recess * correction),
    };
    Ok(Prepared::Done(mesh, Box::new(report)))
}

impl BodyKind {
    /// The body's height as a share of the head's, unless sized.
    fn default_share(self) -> f64 {
        match self {
            Self::ExtendedNeck => NECK_EXTENSION_SHARE,
            Self::Shoulders => SHOULDER_HEIGHT_SHARE,
        }
    }
}

/// Join each part beside the head, such as an eyeball, to the head with a
/// stem from its centre toward the head's. An eyeball sits in a pocket of
/// the head's surface and touches it only where the eyelids overlap it, a
/// joint thinner than the rebuild can keep on a small bust; the stem holds
/// it from behind, inside the head, where the eyeball hides it. The head is
/// the part with the neck opening, or the first part without one.
fn join_parts(
    solid: Manifold,
    parts: &[ClosedPart],
    place: impl Fn([f64; 3]) -> [f64; 3],
) -> Result<Manifold, GeometryError> {
    if parts.len() < 2 {
        return Ok(solid);
    }
    let head = parts
        .iter()
        .enumerate()
        .flat_map(|(index, part)| {
            part.openings
                .iter()
                .map(move |opening| (index, opening.mean_height))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(0, |(index, _)| index);
    let placed = |part: &ClosedPart| -> Vec<[f64; 3]> {
        part.vertices.iter().map(|vertex| place(*vertex)).collect()
    };
    let centre = |points: &[[f64; 3]]| {
        let count = points.len().max(1) as f64;
        [0, 1, 2].map(|axis| points.iter().map(|point| point[axis]).sum::<f64>() / count)
    };
    let head_centre = centre(&placed(&parts[head]));
    let stems: Vec<Manifold> = parts
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != head)
        .filter_map(|(_, part)| {
            let points = placed(part);
            let middle = centre(&points);
            let radius = points
                .iter()
                .map(|point| norm(sub(*point, middle)))
                .sum::<f64>()
                / points.len().max(1) as f64;
            let towards = sub(head_centre, middle);
            (radius > 0.0 && norm(towards) > 0.0).then(|| {
                let thickness = STEM_RADIUS_SHARE * radius;
                let stem = Manifold::cylinder(
                    STEM_LENGTH_SHARE * radius,
                    thickness,
                    thickness,
                    circle_segments(thickness),
                    false,
                );
                along(stem, middle, normalize(towards))
            })
        })
        .collect();
    if stems.is_empty() {
        return Ok(solid);
    }
    let joined = solid.union(&Manifold::batch_union(&stems));
    joined
        .status()
        .map_err(|error| GeometryError::Kernel(format!("joining the parts failed: {error}")))?;
    Ok(joined)
}

/// Raise the head, whose base rests on z = 0, by `height` and add a body
/// below it. The neck is continued by stretching its lowest band, two edges
/// tall, down to the build plate, so the body begins with the neck's own
/// surface and no join. An extended neck follows the neck's lean; shoulders
/// are centred under the neck's base, with the neck inside them straight.
fn add_body(
    head: Manifold,
    body: Body,
    (height, head_height): (f64, f64),
    edge: f64,
) -> Result<(Manifold, BodyReport), GeometryError> {
    let section = head.slice_at_z(SECTION_OFFSET_MM);
    let (base, _) = section_centre(&section)
        .ok_or_else(|| GeometryError::Kernel("the bust has no flat base for a body".into()))?;
    let rise = NECK_DIRECTION_SHARE * head_height;
    let upper = section_centre(&head.slice_at_z(rise)).map_or(base, |(centre, _)| centre);
    // Sideways movement per unit of height, at most the steepest lean.
    let mut neck_lean = [(upper[0] - base[0]) / rise, (upper[1] - base[1]) / rise];
    let slope = neck_lean[0].hypot(neck_lean[1]);
    let steepest = MAX_LEAN_DEGREES.to_radians().tan();
    if slope > steepest {
        neck_lean = neck_lean.map(|value| value * steepest / slope);
    }
    let lean = match body.kind {
        BodyKind::ExtendedNeck => neck_lean,
        BodyKind::Shoulders => [0.0; 2],
    };
    let size = head
        .bounding_box()
        .map(|bounds| bounds.dimensions())
        .ok_or_else(|| GeometryError::Kernel("the bust has no extent".into()))?;
    let band = (2.0 * edge).min(0.5 * rise);
    // How far the stretched neck lies back along the lean at a height.
    let back = move |z: f64| {
        let share = 1.0 - (z / (band + height)).clamp(0.0, 1.0);
        [-lean[0] * height * share, -lean[1] * height * share]
    };
    // Split the surface where it crosses the band's top, so that every
    // triangle lies wholly on one side, then raise what is above and stretch
    // the band: each side's map is affine and the two meet at the band's top,
    // so the surface stays closed and never crosses itself.
    let mut mesh = split_at_height(&mesh_from(&head), band);
    drop(head);
    for vertex in &mut mesh.vertices {
        let [x, y, z] = *vertex;
        *vertex = if z >= band {
            [x, y, z + height]
        } else {
            let raised = z.max(0.0) / band * (band + height);
            let [dx, dy] = back(raised);
            [x + dx, y + dy, raised]
        };
    }
    let flat: Vec<f64> = mesh.vertices.iter().flatten().copied().collect();
    let indices: Vec<u64> = mesh
        .triangles
        .iter()
        .flatten()
        .map(|&index| u64::from(index))
        .collect();
    let neck = Manifold::from_mesh_f64(&flat, 3, &indices)
        .map_err(|error| GeometryError::Kernel(format!("stretching the neck failed: {error}")))?;
    let (solid, width, depth) = match body.kind {
        BodyKind::ExtendedNeck => (neck, None, None),
        BodyKind::Shoulders => {
            let span = |axis: usize| {
                let values = section.iter().flatten().map(|point| point[axis]);
                values.clone().fold(f64::NEG_INFINITY, f64::max)
                    - values.fold(f64::INFINITY, f64::min)
            };
            // Unsized, the shoulders are shares of the head, and always wide
            // enough to hold the neck.
            let width = body
                .width_mm
                .unwrap_or((SHOULDER_WIDTH_SHARE * size[0]).max(SHOULDER_NECK_SHARE * span(0)));
            let depth = body
                .depth_mm
                .unwrap_or((SHOULDER_DEPTH_SHARE * size[1]).max(SHOULDER_NECK_SHARE * span(1)));
            let foot = back(0.0);
            let centre = [base[0] + foot[0], base[1] + foot[1]];
            let segments = circle_segments(width.max(depth) / 2.0);
            let ring = |z: f64| {
                (0..segments).map(move |index| {
                    let angle = std::f64::consts::TAU * f64::from(index) / f64::from(segments);
                    [
                        centre[0] + width / 2.0 * angle.cos(),
                        centre[1] + depth / 2.0 * angle.sin(),
                        z,
                    ]
                })
            };
            // The block's top is the neck's section, a little smaller and a
            // band below the head, but no more than a quarter of the block's
            // height, so that it lies inside the stretched neck, above the
            // plate, and its sides cross the neck's rather than meeting them.
            let top = height - band.min(0.25 * height);
            let [dx, dy] = back(top);
            let crown: Vec<[f64; 3]> = section
                .iter()
                .flatten()
                .map(|point| {
                    [
                        base[0] + dx + SHOULDER_TOP_SHARE * (point[0] - base[0]),
                        base[1] + dy + SHOULDER_TOP_SHARE * (point[1] - base[1]),
                        top,
                    ]
                })
                .collect();
            // Shoulders that cannot contain that top would be wider at the
            // top than their stated size, so they are refused.
            let inside = |point: &[f64; 3]| {
                ((point[0] - centre[0]) / (width / 2.0)).powi(2)
                    + ((point[1] - centre[1]) / (depth / 2.0)).powi(2)
                    <= 1.0
            };
            if !crown.iter().all(inside) {
                return Err(GeometryError::InvalidOptions(format!(
                    "shoulders {width:.1} by {depth:.1} mm cannot hold the neck, which is {:.1} by {:.1} mm across; make them wider and deeper",
                    span(0),
                    span(1)
                )));
            }
            let points: Vec<[f64; 3]> = crown
                .into_iter()
                .chain(ring(SHOULDER_LINE_SHARE * height))
                .chain(ring(0.0))
                .collect();
            let block = Manifold::hull_pts(&points);
            (neck.union(&block), Some(width), Some(depth))
        }
    };
    solid
        .status()
        .map_err(|error| GeometryError::Kernel(format!("adding the body failed: {error}")))?;
    Ok((
        solid,
        BodyReport {
            kind: body.kind,
            height_mm: height,
            width_mm: width,
            depth_mm: depth,
            lean_degrees: neck_lean[0].hypot(neck_lean[1]).atan().to_degrees(),
        },
    ))
}

/// Split each triangle that crosses the plane z = `height` along it, sharing
/// the new vertices between neighbours, so that every triangle lies wholly
/// on one side and the surface keeps its orientation.
fn split_at_height(mesh: &TriangleMesh, height: f64) -> TriangleMesh {
    let mut vertices = mesh.vertices.clone();
    let mut cuts: HashMap<(u32, u32), u32> = HashMap::new();
    let mut cut = |a: u32, b: u32, vertices: &mut Vec<[f64; 3]>| -> u32 {
        *cuts.entry((a.min(b), a.max(b))).or_insert_with(|| {
            let (p, q) = (vertices[a as usize], vertices[b as usize]);
            let share = (height - p[2]) / (q[2] - p[2]);
            vertices.push([
                p[0] + share * (q[0] - p[0]),
                p[1] + share * (q[1] - p[1]),
                height,
            ]);
            (vertices.len() - 1) as u32
        })
    };
    let side = |z: f64| (z > height) as i8 - (z < height) as i8;
    let mut triangles = Vec::with_capacity(mesh.triangles.len());
    for &triangle in &mesh.triangles {
        let sides = triangle.map(|vertex| side(vertices[vertex as usize][2]));
        if !(sides.contains(&1) && sides.contains(&-1)) {
            triangles.push(triangle);
            continue;
        }
        // Turn the triangle so that its first corner is the one on the
        // plane, or else the one alone on its side.
        let first = (0..3)
            .find(|&corner| sides[corner] == 0)
            .or_else(|| {
                (0..3).find(|&corner| sides.iter().filter(|&&s| s == sides[corner]).count() == 1)
            })
            .expect("a crossing triangle has a corner on the plane or alone");
        let [a, b, c] = [0, 1, 2].map(|step| triangle[(first + step) % 3]);
        if sides[first] == 0 {
            let middle = cut(b, c, &mut vertices);
            triangles.extend([[a, b, middle], [a, middle, c]]);
        } else {
            let (near_b, near_c) = (cut(a, b, &mut vertices), cut(a, c, &mut vertices));
            triangles.extend([[a, near_b, near_c], [near_b, b, c], [near_b, c, near_c]]);
        }
    }
    TriangleMesh {
        vertices,
        triangles,
    }
}

/// Drill each eye's pupil and engrave its iris ring along its gaze. Only the
/// eyeball, enlarged by an edge so that the cut passes cleanly through its
/// rebuilt surface, is cut, so the eyelids around it keep their shape.
fn carve_eyes(
    solid: Manifold,
    carving: &EyeCarving,
    (eyeballs, parts): (&[usize], &[ClosedPart]),
    place: impl Fn([f64; 3]) -> [f64; 3],
    to_final: impl Fn([f64; 3]) -> [f64; 3],
    (offset, correction, fraction): ([f64; 3], f64, f64),
    edge: f64,
) -> Result<(Manifold, EyeReport), GeometryError> {
    let smallest = carving.smallest_mm.unwrap_or(0.0);
    let shallowest = carving.shallowest_mm.unwrap_or(0.0);
    let pupil = carving
        .pupil_diameter_mm
        .unwrap_or((LIFE_PUPIL_MM * fraction).max(2.0 * smallest));
    let depth = carving
        .depth_mm
        .unwrap_or((LIFE_PUPIL_DEPTH_MM * fraction).max(shallowest));
    // The ring is a tenth of the iris wide and at least the narrowest
    // feature; a default iris is wide enough to leave one narrowest feature
    // of iris between the ring and the pupil, whichever bound sets the width.
    let ring = carving.iris_ring.then(|| {
        let iris = carving.iris_diameter_mm.unwrap_or(
            (LIFE_IRIS_MM * fraction)
                .max((pupil + 2.0 * smallest) / (1.0 - 2.0 * RING_WIDTH_SHARE))
                .max(pupil + 4.0 * smallest),
        );
        let width = (RING_WIDTH_SHARE * iris).max(smallest);
        (iris, width, (depth / 2.0).max(shallowest))
    });
    let mut cuts = Vec::new();
    let mut smallest_radius = f64::INFINITY;
    for (eye, &part) in carving.eyes.iter().zip(eyeballs) {
        let centre = to_final(eye.centre);
        let gaze = normalize(sub(to_final(eye.front), centre));
        // Built as the bust's eyeball was, without the properties a
        // displacement map adds, which a cut would carry into the bust.
        // Smoothing takes effect when the part is refined. The eyeball's
        // radius is how far its surface reaches along the gaze, where the
        // detail is measured from.
        let eyeball = part_solid(&parts[part], &place, None)
            .map_err(|error| {
                GeometryError::Kernel(format!("an eyeball is not a closed surface: {error}"))
            })?
            .smooth_out(SMOOTH_EVERY_EDGE_DEGREES, 0.0)
            .refine_to_length(edge / correction)
            .translate(-offset[0], -offset[1], -offset[2])
            .scale(correction, correction, correction);
        let radius = mesh_from(&eyeball)
            .vertices
            .iter()
            .map(|vertex| dot(sub(*vertex, centre), gaze))
            .fold(0.0, f64::max);
        smallest_radius = smallest_radius.min(radius);
        let fits = pupil / 2.0 < MAX_PUPIL_SHARE * radius
            && depth < MAX_DEPTH_SHARE * radius
            && ring.is_none_or(|(iris, width, _)| {
                iris / 2.0 < MAX_IRIS_SHARE * radius && iris / 2.0 - width > pupil / 2.0
            });
        if !fits {
            return Err(GeometryError::InvalidOptions(format!(
                "the eye detail does not fit eyes {:.1} mm across; choose a taller bust or smaller eye detail",
                2.0 * radius
            )));
        }
        let grow = 1.0 + edge / radius;
        let eyeball = eyeball
            .translate(-centre[0], -centre[1], -centre[2])
            .scale(grow, grow, grow)
            .translate(centre[0], centre[1], centre[2]);
        let at = |height: f64| [0, 1, 2].map(|axis| centre[axis] + gaze[axis] * height);
        // Depths are measured at the edge of each cut, where the eyeball's
        // surface is lowest, so that the cut shows its full width.
        let rim = |half: f64| (radius * radius - half * half).sqrt();
        let start = rim(pupil / 2.0) - depth;
        let mut tool = along(
            Manifold::cylinder(
                2.0 * radius - start,
                pupil / 2.0,
                pupil / 2.0,
                circle_segments(pupil / 2.0),
                false,
            ),
            at(start),
            gaze,
        );
        if let Some((iris, width, ring_depth)) = ring {
            let outer = iris / 2.0;
            let start = rim(outer) - ring_depth;
            let height = 2.0 * radius - start;
            let segments = circle_segments(outer);
            let band = Manifold::cylinder(height, outer, outer, segments, false).difference(
                &Manifold::cylinder(height + 2.0, outer - width, outer - width, segments, false)
                    .translate(0.0, 0.0, -1.0),
            );
            tool = tool.union(&along(band, at(start), gaze));
        }
        cuts.push(tool.intersection(&eyeball));
    }
    let before = solid.volume();
    let carved = solid.difference(&Manifold::batch_union(&cuts));
    drop(solid);
    carved
        .status()
        .map_err(|error| GeometryError::Kernel(format!("carving the eyes failed: {error}")))?;
    // Where the head's surface covers a cut entirely, as an eyelid can, the
    // cut is a sealed void inside the solid. Voids are filled back in.
    let mut kept: Vec<Manifold> = carved
        .decompose()
        .into_iter()
        .filter(|piece| piece.volume() > 0.0)
        .collect();
    let carved = match kept.len() {
        1 => kept.pop().expect("one piece is kept"),
        _ => Manifold::batch_union(&kept),
    };
    let removed_mm3 = before - carved.volume();
    Ok((
        carved,
        EyeReport {
            pupil_diameter_mm: pupil,
            depth_mm: depth,
            iris_diameter_mm: ring.map(|(iris, _, _)| iris),
            ring_width_mm: ring.map(|(_, width, _)| width),
            ring_depth_mm: ring.map(|(_, _, ring_depth)| ring_depth),
            eyeball_diameter_mm: 2.0 * smallest_radius,
            removed_mm3,
        },
    ))
}

/// For each eye, the part whose centre is nearest the eye's: its eyeball.
fn eyeball_parts(parts: &[ClosedPart], carving: &EyeCarving) -> Result<Vec<usize>, GeometryError> {
    let centroid = |part: &ClosedPart| {
        let count = part.vertices.len().max(1) as f64;
        [0, 1, 2].map(|axis| part.vertices.iter().map(|vertex| vertex[axis]).sum::<f64>() / count)
    };
    carving
        .eyes
        .iter()
        .map(|eye| {
            (0..parts.len())
                .min_by(|&a, &b| {
                    let distance = |index: usize| norm(sub(centroid(&parts[index]), eye.centre));
                    distance(a).total_cmp(&distance(b))
                })
                .ok_or_else(|| {
                    GeometryError::InvalidOptions(
                        "the eyes are not separate parts of the head".into(),
                    )
                })
        })
        .collect()
}

/// Turn a solid built along +z so that +z points along `direction`, and move
/// its origin to `base`.
fn along(solid: Manifold, base: [f64; 3], direction: [f64; 3]) -> Manifold {
    let helper = if direction[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let across = normalize(cross(helper, direction));
    let up = cross(direction, across);
    solid.transform(&[
        across[0],
        across[1],
        across[2],
        up[0],
        up[1],
        up[2],
        direction[0],
        direction[1],
        direction[2],
        base[0],
        base[1],
        base[2],
    ])
}

/// Move the centre of the cap closing every opening but the lowest, the
/// neck, `depth` into the head along the cap's inward normal, whatever the
/// head's pose. Returns how many caps were moved.
fn recess_mouth(parts: &mut [ClosedPart], depth: f64, span: f64) -> Result<usize, GeometryError> {
    if depth > MAX_RECESS_FRACTION * span {
        return Err(GeometryError::InvalidOptions(format!(
            "the mouth recess would reach more than {:.0}% of the head's height into it",
            MAX_RECESS_FRACTION * 100.0
        )));
    }
    let neck = parts
        .iter()
        .enumerate()
        .flat_map(|(part, found)| {
            found
                .openings
                .iter()
                .enumerate()
                .map(move |(opening, found)| ((part, opening), found.mean_height))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(place, _)| place);
    let mut recessed = 0;
    for (index, part) in parts.iter_mut().enumerate() {
        let centres: Vec<(u32, [f64; 3])> = part
            .openings
            .iter()
            .enumerate()
            .filter(|(opening, _)| neck != Some((index, *opening)))
            .map(|(_, opening)| (opening.centre, opening.inward))
            .collect();
        for (centre, inward) in &centres {
            let vertex = &mut part.vertices[*centre as usize];
            *vertex = [0, 1, 2].map(|axis| vertex[axis] + inward[axis] * depth);
        }
        recessed += centres.len();
    }
    Ok(recessed)
}

/// A head's vertices, its triangles and the request vertices around each of
/// its openings.
type HeadSurface<'a> = (&'a [[f64; 3]], &'a [[u32; 3]], &'a [Vec<u32>]);

/// The map to apply, the factor it was scaled by and whether that factor was
/// limited: with `relief_mm`, the map is scaled so that its 95th-percentile
/// magnitude over samples that carry detail reaches that depth once the head
/// is scaled by `scale`.
fn with_relief(
    map: &Displacement,
    (vertices, triangles, rims): HeadSurface<'_>,
    scale: f64,
    head_height: f64,
) -> Result<(Displacement, f64, bool), GeometryError> {
    let Some(relief) = map.relief_mm else {
        return Ok((map.clone(), 1.0, false));
    };
    // The reference is the map as the head samples it: at every triangle's
    // corners and centre, interpolated as the bust interpolates it, and
    // weighted by the triangle's area, as the refined surface spreads its
    // points by area. Map values the head never reaches do not count, nor do
    // faded corners, and a centre takes the share of its corners that are
    // not faded, so the reference is the detail that is applied.
    let mut samples: Vec<(f64, f64)> = map
        .triangle_uvs
        .iter()
        .zip(triangles)
        .flat_map(|(corners, triangle)| {
            let [a, b, c] = triangle.map(|vertex| vertices[vertex as usize]);
            let area = 0.5 * norm(cross(sub(b, a), sub(c, a)));
            let kept = triangle.map(|vertex| {
                if map.faded.binary_search(&vertex).is_ok() {
                    0.0
                } else {
                    1.0
                }
            });
            let centre = [0, 1].map(|axis| {
                corners
                    .iter()
                    .map(|corner| f64::from(corner[axis]))
                    .sum::<f64>()
                    / 3.0
            });
            // Each sample enters at the magnitude it is applied with: the
            // map's value times the share of the detail kept there.
            corners
                .iter()
                .map(|corner| corner.map(f64::from))
                .zip(kept)
                .chain([(centre, kept.iter().sum::<f64>() / 3.0)])
                .filter(|(_, share)| *share > 0.0)
                .map(move |([u, v], share)| (share * map.sample(u, v).abs(), area / 4.0))
                .collect::<Vec<_>>()
        })
        .collect();
    let sampled_largest = samples
        .iter()
        .map(|(magnitude, _)| *magnitude)
        .fold(0.0, f64::max);
    samples.retain(|(magnitude, _)| *magnitude > DETAIL_SHARE * sampled_largest);
    let largest = applied_largest(map, triangles, rims);
    let Some(reference) = weighted_percentile(&mut samples, RELIEF_PERCENTILE)
        .filter(|reference| *reference >= MIN_RELIEF_REFERENCE_MM)
    else {
        // A map without detail has nothing to scale.
        return Ok((map.clone(), 1.0, false));
    };
    let mut factor = relief / (reference * scale);
    let limit = MAX_DISPLACEMENT_FRACTION * head_height;
    let limited = largest * factor > limit;
    if limited {
        if !map.limit_relief {
            return Err(GeometryError::InvalidOptions(format!(
                "relief_mm would raise detail beyond {:.0}% of the head's height",
                MAX_DISPLACEMENT_FRACTION * 100.0
            )));
        }
        factor = limit / largest;
    }
    let mut scaled = map.clone();
    for value in &mut scaled.values {
        *value = (f64::from(*value) * factor) as f32;
    }
    Ok((scaled, factor, limited))
}

/// The largest magnitude among the map's values that the head can apply:
/// those a bilinear sample within the texture-space bounds of a triangle
/// with a corner that is not faded can read, or within the bounds of an
/// opening's rim, whose cap takes texture coordinates between the rim's.
/// Bounds over-cover, so this errs toward the larger value.
fn applied_largest(map: &Displacement, triangles: &[[u32; 3]], rims: &[Vec<u32>]) -> f64 {
    let (width, height) = (map.width, map.height);
    let faded = |vertex: &u32| map.faded.binary_search(vertex).is_ok();
    let mut reached = vec![false; width * height];
    let mut mark = |corners: Vec<[f32; 2]>| {
        let span = |axis: usize, side: usize, flip: bool| {
            let at = |corner: &[f32; 2]| {
                let value = f64::from(corner[axis]).clamp(0.0, 1.0);
                (if flip { 1.0 - value } else { value }) * (side - 1) as f64
            };
            let low = corners.iter().map(at).fold(f64::INFINITY, f64::min);
            let high = corners.iter().map(at).fold(f64::NEG_INFINITY, f64::max);
            (low.floor() as usize)..=(high.ceil() as usize).min(side - 1)
        };
        for row in span(1, height, true) {
            for column in span(0, width, false) {
                reached[row * width + column] = true;
            }
        }
    };
    for (corners, triangle) in map.triangle_uvs.iter().zip(triangles) {
        if !triangle.iter().all(faded) {
            mark(corners.to_vec());
        }
    }
    for rim in rims.iter().filter(|rim| !rim.iter().all(faded)) {
        let rim: HashSet<u32> = rim.iter().copied().collect();
        let corners: Vec<[f32; 2]> = map
            .triangle_uvs
            .iter()
            .zip(triangles)
            .flat_map(|(corners, triangle)| {
                corners
                    .iter()
                    .zip(triangle)
                    .filter(|(_, vertex)| rim.contains(vertex))
                    .map(|(corner, _)| *corner)
                    .collect::<Vec<_>>()
            })
            .collect();
        if !corners.is_empty() {
            mark(corners);
        }
    }
    map.values
        .iter()
        .zip(reached)
        .filter(|(_, reached)| *reached)
        .fold(0.0, |most: f64, (value, _)| {
            most.max(f64::from(value.abs()))
        })
}

/// The applied range and, over the vertices that carry detail, its
/// percentiles, scaled by `correction` to the finished size.
fn detail_report(
    distances: &mut Vec<f64>,
    correction: f64,
    (relief_scale, relief_limited): (f64, bool),
) -> DisplacementReport {
    let lowest = distances.iter().copied().fold(f64::INFINITY, f64::min);
    let highest = distances.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let largest = lowest.abs().max(highest.abs());
    distances.retain(|distance| distance.abs() > DETAIL_SHARE * largest);
    let mut at = |rank: f64| percentile(distances, rank).map(|value| value * correction);
    let percentiles_mm = match (at(1.0), at(5.0), at(50.0), at(95.0), at(99.0)) {
        (Some(p1), Some(p5), Some(p50), Some(p95), Some(p99)) => Some(DetailPercentiles {
            p1,
            p5,
            p50,
            p95,
            p99,
        }),
        _ => None,
    };
    let mut magnitudes: Vec<f64> = distances.iter().map(|distance| distance.abs()).collect();
    let depth_p95_mm =
        percentile(&mut magnitudes, RELIEF_PERCENTILE).map(|value| value * correction);
    DisplacementReport {
        lowest_mm: lowest * correction,
        highest_mm: highest * correction,
        percentiles_mm,
        depth_p95_mm,
        relief_scale,
        relief_limited,
    }
}

/// The value below which `rank` percent of the total weight lies, of
/// `(value, weight)` pairs; `None` when there is no weight.
fn weighted_percentile(samples: &mut [(f64, f64)], rank: f64) -> Option<f64> {
    samples.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = samples.iter().map(|(_, weight)| weight).sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let target = rank.clamp(0.0, 100.0) / 100.0 * total;
    let mut reached = 0.0;
    for (value, weight) in samples.iter() {
        reached += weight;
        if reached >= target {
            return Some(*value);
        }
    }
    samples.last().map(|(value, _)| *value)
}

/// The value at `rank`, 0 to 100, by nearest rank; `None` when empty.
fn percentile(values: &mut [f64], rank: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let index = (rank.clamp(0.0, 100.0) / 100.0 * (values.len() - 1) as f64).round();
    Some(values[index as usize])
}

/// Raise the bust onto a cylinder centred under its flat base.
fn add_plinth(bust: Manifold, plinth: Plinth) -> Result<(Manifold, PlinthReport), GeometryError> {
    let (centre, reach) = section_centre(&bust.slice_at_z(SECTION_OFFSET_MM))
        .ok_or_else(|| GeometryError::Kernel("the bust has no flat base for a plinth".into()))?;
    let size = bust
        .bounding_box()
        .map(|bounds| bounds.dimensions())
        .ok_or_else(|| GeometryError::Kernel("the bust has no extent".into()))?;
    let diameter = plinth.diameter_mm.unwrap_or_else(|| {
        (PLINTH_WIDTH_FRACTION * size[0].max(size[1])).max(2.0 * (reach + PLINTH_MARGIN_MM))
    });
    if diameter < 2.0 * reach {
        return Err(GeometryError::InvalidOptions(format!(
            "a plinth {diameter} mm across does not cover the {:.1} mm neck section",
            2.0 * reach
        )));
    }
    let radius = diameter / 2.0;
    let chamfer = plinth.chamfer_mm;
    if chamfer >= radius {
        return Err(GeometryError::InvalidOptions(format!(
            "a {chamfer} mm chamfer does not fit a plinth {diameter:.1} mm across"
        )));
    }
    let segments = circle_segments(radius);
    let base = if chamfer > 0.0 {
        let foot = Manifold::cylinder(chamfer, radius - chamfer, radius, segments, false);
        let body = Manifold::cylinder(plinth.height_mm - chamfer, radius, radius, segments, false)
            .translate(0.0, 0.0, chamfer);
        foot.union(&body)
    } else {
        Manifold::cylinder(plinth.height_mm, radius, radius, segments, false)
    }
    .translate(centre[0], centre[1], 0.0);
    let raised = bust.translate(0.0, 0.0, plinth.height_mm);
    drop(bust);
    let solid = raised.union(&base);
    solid
        .status()
        .map_err(|error| GeometryError::Kernel(format!("adding the plinth failed: {error}")))?;
    Ok((
        solid,
        PlinthReport {
            height_mm: plinth.height_mm,
            diameter_mm: diameter,
            chamfer_mm: chamfer,
        },
    ))
}

/// Remove the region deeper than the wall, with drain holes from the base
/// into the lowest part of it. Pockets that no drain hole reaches stay solid,
/// so no liquid resin is sealed inside.
fn hollow_out(
    solid: Manifold,
    hollow: Hollow,
    budget: usize,
) -> Result<(Manifold, HollowReport), GeometryError> {
    // The cavity is hidden, so its grid is as coarse as the wall allows, and
    // coarser still if its triangles would exceed the budget. Its area is
    // less than the outer surface's.
    let area = solid.surface_area();
    let spacing = (hollow.wall_mm / 2.0).clamp(0.5, 2.0).max(
        (CAVITY_TRIANGLES_PER_AREA * area / budget.max(1) as f64).sqrt() / remesh::EDGE_PER_SPACING,
    );
    let mesh = mesh_from(&solid);
    let cavity = remesh::inner_offset(mesh.vertices, mesh.triangles, hollow.wall_mm, spacing)?;
    if cavity.is_empty() {
        return Err(GeometryError::InvalidOptions(format!(
            "no part of the bust is thicker than two {} mm walls",
            hollow.wall_mm
        )));
    }
    let lowest = cavity
        .bounding_box()
        .ok_or_else(|| GeometryError::Kernel("the cavity has no extent".into()))?
        .min()[2];
    let section = cavity.slice_at_z(lowest + spacing);
    let (centre, _) = section_centre(&section)
        .ok_or_else(|| GeometryError::Kernel("the cavity has no floor for a drain hole".into()))?;
    let (west, east) = section
        .iter()
        .flatten()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(west, east), point| {
            (west.min(point[0]), east.max(point[0]))
        });
    let radius = hollow.drain_diameter_mm / 2.0;
    // Two holes let air in while resin drains, when the floor is wide enough.
    let offset = (east - west) / 4.0;
    let positions = if offset > 2.0 * hollow.drain_diameter_mm {
        vec![
            [centre[0] - offset, centre[1]],
            [centre[0] + offset, centre[1]],
        ]
    } else {
        vec![centre]
    };
    let depth = lowest + spacing + DRAIN_OVERLAP_MM + 1.0;
    let mut holes = Vec::new();
    let mut drain_holes_mm = Vec::new();
    for [x, y] in positions {
        let hole = Manifold::cylinder(depth, radius, radius, circle_segments(radius), false)
            .translate(x, y, -1.0);
        if !hole.intersection(&cavity).is_empty() {
            holes.push(hole);
            drain_holes_mm.push([x, y]);
        }
    }
    if holes.is_empty() {
        return Err(GeometryError::Kernel(
            "no drain hole reaches the cavity".into(),
        ));
    }
    let drained: Vec<Manifold> = cavity
        .decompose()
        .into_iter()
        .filter(|pocket| {
            holes
                .iter()
                .any(|hole| !hole.intersection(pocket).is_empty())
        })
        .collect();
    drop(cavity);
    let cavity_volume_mm3 = drained.iter().map(Manifold::volume).sum();
    let mut parts = vec![solid];
    parts.extend(drained);
    parts.extend(holes);
    let hollowed = Manifold::batch_difference(&parts);
    drop(parts);
    hollowed
        .status()
        .map_err(|error| GeometryError::Kernel(format!("hollowing failed: {error}")))?;
    if hollowed.decompose().len() != 1 {
        return Err(GeometryError::Kernel(
            "hollowing left a sealed pocket or a separate piece".into(),
        ));
    }
    Ok((
        hollowed,
        HollowReport {
            wall_mm: hollow.wall_mm,
            cavity_volume_mm3,
            drain_diameter_mm: hollow.drain_diameter_mm,
            drain_holes_mm,
        },
    ))
}

/// The area-weighted centre of a cross-section and the farthest distance from
/// it to the outline.
fn section_centre(section: &[Vec<[f64; 2]>]) -> Option<([f64; 2], f64)> {
    let (mut area, mut x, mut y) = (0.0, 0.0, 0.0);
    for ring in section {
        for (index, a) in ring.iter().enumerate() {
            let b = ring[(index + 1) % ring.len()];
            let cross = a[0] * b[1] - b[0] * a[1];
            area += cross;
            x += (a[0] + b[0]) * cross;
            y += (a[1] + b[1]) * cross;
        }
    }
    if area.abs() < f64::EPSILON {
        return None;
    }
    let centre = [x / (3.0 * area), y / (3.0 * area)];
    let reach = section
        .iter()
        .flatten()
        .map(|point| (point[0] - centre[0]).hypot(point[1] - centre[1]))
        .fold(0.0, f64::max);
    Some((centre, reach))
}

/// Facets for a circle of `radius` within the circle tolerance.
fn circle_segments(radius: f64) -> i32 {
    if radius <= CIRCLE_TOLERANCE_MM {
        return MIN_CIRCLE_SEGMENTS;
    }
    let step = (1.0 - CIRCLE_TOLERANCE_MM / radius).acos();
    ((std::f64::consts::PI / step).ceil() as i32).clamp(MIN_CIRCLE_SEGMENTS, MAX_CIRCLE_SEGMENTS)
}

/// Encode a mesh as binary STL with facet normals.
pub fn bust_stl(mesh: &TriangleMesh) -> Vec<u8> {
    let triangles: Vec<_> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| mesh.vertices[index as usize]);
            let normal = normalize(cross(sub(b, a), sub(c, a)));
            stl_io::Triangle {
                normal: stl_io::Normal::new(normal.map(|value| value as f32)),
                vertices: [a, b, c]
                    .map(|vertex| stl_io::Vertex::new(vertex.map(|value| value as f32))),
            }
        })
        .collect();
    let mut bytes = Vec::with_capacity(84 + 50 * triangles.len());
    stl_io::write_stl(&mut bytes, triangles.iter()).expect("writing STL to memory cannot fail");
    bytes
}

fn validate(request: &BustRequest) -> Result<(), GeometryError> {
    let invalid = |message: &str| Err(GeometryError::InvalidOptions(message.into()));
    if !(request.height_mm.is_finite() && request.height_mm > 0.0) {
        return invalid("bust height must be a positive number of millimetres");
    }
    if !(request.edge_length_mm.is_finite() && request.edge_length_mm > 0.0) {
        return invalid("edge length must be a positive number of millimetres");
    }
    if request.max_triangles == 0 {
        return invalid("the triangle limit must be positive");
    }
    if let Some(plinth) = request.plinth {
        if !(plinth.height_mm.is_finite() && plinth.height_mm > 0.0) {
            return invalid("plinth height must be a positive number of millimetres");
        }
        if plinth.height_mm >= request.height_mm {
            return invalid("the plinth must be lower than the whole bust");
        }
        if plinth
            .diameter_mm
            .is_some_and(|diameter| !(diameter.is_finite() && diameter > 0.0))
        {
            return invalid("plinth diameter must be a positive number of millimetres");
        }
        if !(plinth.chamfer_mm.is_finite()
            && plinth.chamfer_mm >= 0.0
            && plinth.chamfer_mm < plinth.height_mm)
        {
            return invalid(
                "a plinth chamfer must be zero or more and less than the plinth's height",
            );
        }
    }
    if request
        .mouth_recess_mm
        .is_some_and(|recess| !(recess.is_finite() && recess > 0.0))
    {
        return invalid("the mouth recess must be a positive number of millimetres");
    }
    if let Some(body) = request.body
        && ![body.height_mm, body.width_mm, body.depth_mm]
            .iter()
            .all(|value| value.is_none_or(|value| value.is_finite() && value > 0.0))
    {
        return invalid("body sizes must be positive numbers of millimetres");
    }
    if let Some(body) = request.body
        && body.kind == BodyKind::ExtendedNeck
        && (body.width_mm.is_some() || body.depth_mm.is_some())
    {
        return invalid("an extended neck takes no width or depth");
    }
    if let Some(carving) = &request.eyes {
        let positive =
            |value: Option<f64>| value.is_none_or(|value| value.is_finite() && value > 0.0);
        let at_least_zero =
            |value: Option<f64>| value.is_none_or(|value| value.is_finite() && value >= 0.0);
        if !(1..=2).contains(&carving.eyes.len())
            || !carving.eyes.iter().all(|eye| {
                eye.centre
                    .iter()
                    .chain(&eye.front)
                    .all(|value| value.is_finite())
                    && eye.centre != eye.front
            })
        {
            return invalid(
                "eye carving needs one or two eyes, each with a distinct centre and front",
            );
        }
        if !(positive(carving.pupil_diameter_mm)
            && positive(carving.iris_diameter_mm)
            && positive(carving.depth_mm)
            && at_least_zero(carving.smallest_mm)
            && at_least_zero(carving.shallowest_mm))
        {
            return invalid("eye detail sizes must be positive numbers of millimetres");
        }
    }
    if let Some(checks) = request.checks {
        if !(checks.overhang_degrees.is_finite() && (0.0..=90.0).contains(&checks.overhang_degrees))
        {
            return invalid("the overhang angle must be 0 to 90 degrees");
        }
        if checks
            .min_feature_mm
            .is_some_and(|size| !(size.is_finite() && size > 0.0))
        {
            return invalid("the smallest feature size must be a positive number of millimetres");
        }
    }
    if let Some(hollow) = request.hollow
        && !(hollow.wall_mm.is_finite()
            && hollow.wall_mm > 0.0
            && hollow.drain_diameter_mm.is_finite()
            && hollow.drain_diameter_mm > 0.0)
    {
        return invalid("hollow wall and drain diameter must be positive numbers of millimetres");
    }
    if request.vertices.is_empty() || request.triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    if let Some(index) = request
        .vertices
        .iter()
        .position(|vertex| !vertex.iter().all(|value| value.is_finite()))
    {
        return Err(GeometryError::NonFiniteVertex { index });
    }
    let count = request.vertices.len();
    for (triangle, indices) in request.triangles.iter().enumerate() {
        if indices.iter().any(|&index| index as usize >= count) {
            return Err(GeometryError::InvalidIndex { triangle });
        }
        if indices[0] == indices[1] || indices[1] == indices[2] || indices[0] == indices[2] {
            return invalid("a head triangle repeats a vertex");
        }
    }
    if let Some(map) = &request.displacement {
        if !(2..=MAX_DISPLACEMENT_SIDE).contains(&map.width)
            || !(2..=MAX_DISPLACEMENT_SIDE).contains(&map.height)
        {
            return invalid(&format!(
                "a displacement map must be 2 to {MAX_DISPLACEMENT_SIDE} samples on each side"
            ));
        }
        if map.values.len() != map.width * map.height {
            return invalid("the displacement values do not match the map size");
        }
        let heights = request.vertices.iter().map(|vertex| vertex[1]);
        let span = heights.clone().fold(f64::MIN, f64::max) - heights.fold(f64::MAX, f64::min);
        let limit = MAX_DISPLACEMENT_FRACTION * span;
        if !map
            .values
            .iter()
            .all(|&value| value.is_finite() && f64::from(value).abs() <= limit)
        {
            return invalid(&format!(
                "displacement values must be finite and within {:.0}% of the head's height",
                MAX_DISPLACEMENT_FRACTION * 100.0
            ));
        }
        if map.triangle_uvs.len() != request.triangles.len() {
            return invalid("there must be texture coordinates for every head triangle");
        }
        if map
            .relief_mm
            .is_some_and(|relief| !(relief.is_finite() && relief > 0.0))
        {
            return invalid("relief must be a positive number of millimetres");
        }
        if !map.faded.windows(2).all(|pair| pair[0] < pair[1])
            || map
                .faded
                .last()
                .is_some_and(|&vertex| vertex as usize >= count)
        {
            return invalid("faded vertices must be distinct head vertices in increasing order");
        }
        if !map
            .triangle_uvs
            .iter()
            .flatten()
            .flatten()
            .all(|value| value.is_finite())
        {
            return invalid("texture coordinates must be finite");
        }
    }
    Ok(())
}

struct Opening {
    mean_height: f64,
    top_height: f64,
    /// The part vertex at the centre of the fan that closes it.
    centre: u32,
    /// The unit normal of that fan, pointing into the part.
    inward: [f64; 3],
    /// The request vertices around the opening.
    rim: Vec<u32>,
}

struct ClosedPart {
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
    /// The input triangle each part triangle came from; `None` for the fan
    /// triangles that close an opening.
    sources: Vec<Option<usize>>,
    openings: Vec<Opening>,
    /// The request vertex each part vertex came from, for every vertex but
    /// the fan centres, which follow them.
    origins: Vec<u32>,
}

/// Split the mesh into connected parts, largest first, closing every boundary
/// loop with a fan of triangles around its centroid.
fn closed_parts(
    vertices: &[[f64; 3]],
    triangles: &[[u32; 3]],
) -> Result<Vec<ClosedPart>, GeometryError> {
    let mut parent: Vec<usize> = (0..vertices.len()).collect();
    fn root(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for triangle in triangles {
        for pair in [[triangle[0], triangle[1]], [triangle[1], triangle[2]]] {
            let (a, b) = (
                root(&mut parent, pair[0] as usize),
                root(&mut parent, pair[1] as usize),
            );
            parent[a] = b;
        }
    }
    let mut groups: HashMap<usize, Vec<(usize, [u32; 3])>> = HashMap::new();
    for (index, triangle) in triangles.iter().enumerate() {
        groups
            .entry(root(&mut parent, triangle[0] as usize))
            .or_default()
            .push((index, *triangle));
    }
    let mut groups: Vec<Vec<(usize, [u32; 3])>> = groups.into_values().collect();
    // Largest first; ties broken by first triangle so the order is deterministic.
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].1.cmp(&b[0].1)));
    groups
        .into_iter()
        .map(|group| close_part(vertices, group))
        .collect()
}

fn close_part(
    vertices: &[[f64; 3]],
    group: Vec<(usize, [u32; 3])>,
) -> Result<ClosedPart, GeometryError> {
    let mut sources: Vec<Option<usize>> = group.iter().map(|(index, _)| Some(*index)).collect();
    let mut triangles: Vec<[u32; 3]> = group.into_iter().map(|(_, triangle)| triangle).collect();
    let invalid = |message: &str| GeometryError::InvalidOptions(message.into());
    let mut directed = HashSet::with_capacity(triangles.len() * 3);
    for [a, b, c] in &triangles {
        for edge in [(*a, *b), (*b, *c), (*c, *a)] {
            if !directed.insert(edge) {
                return Err(invalid(
                    "the head surface is not consistently oriented and manifold",
                ));
            }
        }
    }
    let mut next: HashMap<u32, u32> = HashMap::new();
    for &(a, b) in &directed {
        if !directed.contains(&(b, a)) && next.insert(a, b).is_some() {
            return Err(invalid(
                "the head surface boundary is not a set of simple loops",
            ));
        }
    }

    let mut used: Vec<u32> = triangles.iter().flatten().copied().collect();
    used.sort_unstable();
    used.dedup();
    let local: HashMap<u32, u32> = used
        .iter()
        .enumerate()
        .map(|(position, &index)| (index, position as u32))
        .collect();
    let mut part_vertices: Vec<[f64; 3]> =
        used.iter().map(|&index| vertices[index as usize]).collect();
    for triangle in &mut triangles {
        *triangle = triangle.map(|index| local[&index]);
    }

    let mut starts: Vec<u32> = next.keys().copied().collect();
    starts.sort_unstable();
    let mut visited = HashSet::new();
    let mut openings = Vec::new();
    for start in starts {
        if visited.contains(&start) {
            continue;
        }
        let mut boundary = Vec::new();
        let mut current = start;
        while visited.insert(current) {
            boundary.push(current);
            current = *next
                .get(&current)
                .ok_or_else(|| invalid("the head surface boundary is not a set of simple loops"))?;
        }
        if current != start {
            return Err(invalid(
                "the head surface boundary is not a set of simple loops",
            ));
        }
        let points: Vec<[f64; 3]> = boundary
            .iter()
            .map(|&index| vertices[index as usize])
            .collect();
        let count = points.len() as f64;
        let centre = points
            .iter()
            .fold([0.0; 3], |sum, point| {
                [sum[0] + point[0], sum[1] + point[1], sum[2] + point[2]]
            })
            .map(|value| value / count);
        let centre_index = part_vertices.len() as u32;
        part_vertices.push(centre);
        // Each cap triangle runs against its boundary edge, keeping orientation
        // consistent, so the cap faces out of the part.
        let mut outward = [0.0; 3];
        for (position, &a) in boundary.iter().enumerate() {
            let b = boundary[(position + 1) % boundary.len()];
            triangles.push([local[&b], local[&a], centre_index]);
            sources.push(None);
            let face = cross(
                sub(vertices[a as usize], vertices[b as usize]),
                sub(centre, vertices[b as usize]),
            );
            outward = [0, 1, 2].map(|axis| outward[axis] + face[axis]);
        }
        openings.push(Opening {
            mean_height: points.iter().map(|point| point[1]).sum::<f64>() / count,
            top_height: points.iter().map(|point| point[1]).fold(f64::MIN, f64::max),
            centre: centre_index,
            inward: normalize(outward.map(|value| -value)),
            rim: boundary.clone(),
        });
    }
    Ok(ClosedPart {
        vertices: part_vertices,
        triangles,
        sources,
        openings,
        origins: used,
    })
}

/// Vertex properties of a displaced part: position, texture coordinates and
/// the weight given to the map.
const DISPLACED_PROPERTIES: usize = 6;

/// A placed part as a solid. With a displacement map, each vertex carries its
/// texture coordinates and a weight as properties, which smoothing and
/// refinement interpolate. Where texture coordinates differ across an edge,
/// a vertex is repeated with each set and the copies are merged. A fan
/// centre takes the mean coordinates of the vertices around it and weight
/// zero, so detail fades out across the fan.
fn part_solid(
    part: &ClosedPart,
    place: impl Fn([f64; 3]) -> [f64; 3],
    displacement: Option<&Displacement>,
) -> Result<Manifold, manifold_csg::CsgError> {
    let Some(map) = displacement else {
        let flat: Vec<f64> = part
            .vertices
            .iter()
            .flat_map(|vertex| place(*vertex))
            .collect();
        let indices: Vec<u64> = part
            .triangles
            .iter()
            .flatten()
            .map(|&index| u64::from(index))
            .collect();
        return Manifold::from_mesh_f64(&flat, 3, &indices);
    };
    let mut built = PropertyMesh {
        properties: Vec::new(),
        first: vec![None; part.vertices.len()],
        merge_from: Vec::new(),
        merge_to: Vec::new(),
    };
    let mut ids: HashMap<(u32, [u32; 2]), u64> = HashMap::new();
    let mut indices = vec![0u64; part.triangles.len() * 3];
    for (slot, (triangle, source)) in part.triangles.iter().zip(&part.sources).enumerate() {
        let Some(source) = *source else { continue };
        for corner in 0..3 {
            let vertex = triangle[corner];
            let uv = map.triangle_uvs[source][corner];
            let faded = map
                .faded
                .binary_search(&part.origins[vertex as usize])
                .is_ok();
            indices[slot * 3 + corner] =
                *ids.entry((vertex, uv.map(f32::to_bits)))
                    .or_insert_with(|| {
                        built.add(
                            place(part.vertices[vertex as usize]),
                            vertex,
                            uv.map(f64::from),
                            if faded { 0.0 } else { 1.0 },
                        )
                    });
        }
    }
    // Every fan vertex except its centre belongs to a surface triangle.
    let mut centres: HashMap<u32, ([f64; 2], usize)> = HashMap::new();
    for (triangle, source) in part.triangles.iter().zip(&part.sources) {
        if source.is_some() {
            continue;
        }
        let rim: Vec<[f64; 2]> = triangle
            .iter()
            .filter_map(|&vertex| built.first[vertex as usize].map(|id| built.uv(id)))
            .collect();
        for &vertex in triangle {
            if built.first[vertex as usize].is_none() {
                let (sum, count) = centres.entry(vertex).or_insert(([0.0; 2], 0));
                for uv in &rim {
                    sum[0] += uv[0];
                    sum[1] += uv[1];
                    *count += 1;
                }
            }
        }
    }
    let mut centres: Vec<_> = centres.into_iter().collect();
    centres.sort_by_key(|(vertex, _)| *vertex);
    for (vertex, (sum, count)) in centres {
        let mean = sum.map(|value| value / count.max(1) as f64);
        built.add(place(part.vertices[vertex as usize]), vertex, mean, 0.0);
    }
    for (slot, (triangle, source)) in part.triangles.iter().zip(&part.sources).enumerate() {
        if source.is_none() {
            for corner in 0..3 {
                indices[slot * 3 + corner] =
                    built.first[triangle[corner] as usize].expect("every vertex has properties");
            }
        }
    }
    let PropertyMesh {
        properties,
        merge_from,
        merge_to,
        ..
    } = built;
    let mesh = MeshGL64::new_with_options(
        &properties,
        DISPLACED_PROPERTIES,
        &indices,
        MeshGL64Options::new().merge_vertices(&merge_from, &merge_to),
    )?;
    Manifold::from_meshgl64(&mesh)
}

/// Property vertices under construction, with the first copy of each part
/// vertex and the later copies to merge into it.
struct PropertyMesh {
    properties: Vec<f64>,
    first: Vec<Option<u64>>,
    merge_from: Vec<u64>,
    merge_to: Vec<u64>,
}

impl PropertyMesh {
    fn add(&mut self, position: [f64; 3], vertex: u32, uv: [f64; 2], weight: f64) -> u64 {
        let id = (self.properties.len() / DISPLACED_PROPERTIES) as u64;
        self.properties.extend(position);
        self.properties.extend([uv[0], uv[1], weight]);
        match self.first[vertex as usize] {
            Some(original) => {
                self.merge_from.push(id);
                self.merge_to.push(original);
            }
            None => self.first[vertex as usize] = Some(id),
        }
        id
    }

    fn uv(&self, id: u64) -> [f64; 2] {
        let at = id as usize * DISPLACED_PROPERTIES;
        [self.properties[at + 3], self.properties[at + 4]]
    }
}

/// The refined surface of a displaced part, each vertex moved along its
/// normal by the map value at its texture coordinates times its weight,
/// scaled from the request's units to the bust's, and the distance each
/// vertex moved.
fn displaced(solid: &Manifold, map: &Displacement, scale: f64) -> (TriangleMesh, Vec<f64>) {
    let mesh = solid.to_meshgl64();
    let width = mesh.num_prop();
    let properties = mesh.vert_properties();
    let count = properties.len() / width;
    // Copies of a vertex with different texture coordinates share one
    // position; merge them so that they move together.
    let mut canonical: Vec<usize> = (0..count).collect();
    for (from, to) in mesh.merge_from_vert().into_iter().zip(mesh.merge_to_vert()) {
        canonical[from as usize] = to as usize;
    }
    for index in 0..count {
        let mut root = canonical[index];
        while canonical[root] != root {
            root = canonical[root];
        }
        canonical[index] = root;
    }
    let position = |index: usize| {
        let at = index * width;
        [properties[at], properties[at + 1], properties[at + 2]]
    };
    let triangles: Vec<[usize; 3]> = mesh
        .tri_verts()
        .chunks_exact(3)
        .map(|triangle| [0, 1, 2].map(|corner| canonical[triangle[corner] as usize]))
        .collect();
    let mut offsets = vec![(0.0, 0u32); count];
    for index in 0..count {
        let at = index * width;
        let value = properties[at + 5] * map.sample(properties[at + 3], properties[at + 4]);
        let entry = &mut offsets[canonical[index]];
        entry.0 += value;
        entry.1 += 1;
    }
    // Area-weighted normals.
    let mut normals = vec![[0.0; 3]; count];
    for &[a, b, c] in &triangles {
        let (pa, pb, pc) = (position(a), position(b), position(c));
        let normal = cross(sub(pb, pa), sub(pc, pa));
        for corner in [a, b, c] {
            for axis in 0..3 {
                normals[corner][axis] += normal[axis];
            }
        }
    }
    let mut remap = vec![u32::MAX; count];
    let mut vertices = Vec::new();
    let mut distances = Vec::new();
    let triangles = triangles
        .iter()
        .map(|triangle| {
            triangle.map(|index| {
                if remap[index] == u32::MAX {
                    remap[index] = vertices.len() as u32;
                    let (sum, copies) = offsets[index];
                    let distance = sum / f64::from(copies.max(1)) * scale;
                    distances.push(distance);
                    let normal = normals[index];
                    let length = norm(normal);
                    let point = position(index);
                    vertices.push(if length > 0.0 {
                        [0, 1, 2].map(|axis| point[axis] + normal[axis] / length * distance)
                    } else {
                        point
                    });
                }
                remap[index]
            })
        })
        .collect();
    (
        TriangleMesh {
            vertices,
            triangles,
        },
        distances,
    )
}

/// The mesh as an STL stores it, checked to still be a closed solid.
fn single_precision(manifold: &Manifold) -> Result<TriangleMesh, GeometryError> {
    let mut mesh = mesh_from(manifold);
    separate_pinches(&mut mesh);
    // An STL reader joins vertices with equal coordinates, so do the same.
    let mut welded: HashMap<[u32; 3], u32> = HashMap::with_capacity(mesh.vertices.len());
    let mut vertices = Vec::with_capacity(mesh.vertices.len());
    let remap: Vec<u32> = mesh
        .vertices
        .iter()
        .map(|vertex| {
            let rounded = vertex.map(|coordinate| coordinate as f32);
            *welded.entry(rounded.map(f32::to_bits)).or_insert_with(|| {
                vertices.push(rounded.map(f64::from));
                (vertices.len() - 1) as u32
            })
        })
        .collect();
    let mesh = TriangleMesh {
        vertices,
        triangles: mesh
            .triangles
            .iter()
            .map(|triangle| triangle.map(|index| remap[index as usize]))
            .collect(),
    };
    // Each edge of a closed, consistently oriented surface is used once in
    // each direction.
    let mut directed = HashSet::with_capacity(mesh.triangles.len() * 3);
    for [a, b, c] in &mesh.triangles {
        for edge in [(*a, *b), (*b, *c), (*c, *a)] {
            if edge.0 == edge.1 || !directed.insert(edge) {
                return Err(GeometryError::Kernel(
                    "the bust is not a consistently oriented surface at STL precision".into(),
                ));
            }
        }
    }
    if directed.iter().any(|(a, b)| !directed.contains(&(*b, *a))) {
        return Err(GeometryError::Kernel(
            "the bust is not a closed surface at STL precision".into(),
        ));
    }
    Ok(mesh)
}

/// Where two parts of the surface touch at a point, the kernel keeps a
/// separate vertex for each part at the same position. An STL cannot express
/// that, so move each such vertex part of the way toward the centre of its
/// own neighbours, which separates the parts without changing the topology.
fn separate_pinches(mesh: &mut TriangleMesh) {
    let mut positions: HashMap<[u32; 3], Vec<usize>> = HashMap::new();
    for (index, vertex) in mesh.vertices.iter().enumerate() {
        positions
            .entry(vertex.map(|coordinate| (coordinate as f32).to_bits()))
            .or_default()
            .push(index);
    }
    let pinched: HashSet<usize> = positions
        .into_values()
        .filter(|group| group.len() > 1)
        .flatten()
        .collect();
    if pinched.is_empty() {
        return;
    }
    let mut sums: HashMap<usize, ([f64; 3], usize)> = HashMap::new();
    for triangle in &mesh.triangles {
        for (corner, &index) in triangle.iter().enumerate() {
            let index = index as usize;
            if !pinched.contains(&index) {
                continue;
            }
            let entry = sums.entry(index).or_insert(([0.0; 3], 0));
            for other in [triangle[(corner + 1) % 3], triangle[(corner + 2) % 3]] {
                for (total, value) in entry.0.iter_mut().zip(mesh.vertices[other as usize]) {
                    *total += value;
                }
                entry.1 += 1;
            }
        }
    }
    for (index, (sum, count)) in sums {
        let vertex = mesh.vertices[index];
        let toward = sub(sum.map(|value| value / count as f64), vertex);
        let step = (PINCH_SEPARATION * norm(toward)).min(PINCH_SEPARATION_LIMIT_MM);
        let direction = normalize(toward);
        mesh.vertices[index] = std::array::from_fn(|axis| vertex[axis] + step * direction[axis]);
    }
}

fn mesh_from(manifold: &Manifold) -> TriangleMesh {
    let (vertices, properties, triangles) = manifold.to_mesh_f64();
    TriangleMesh {
        vertices: vertices
            .chunks_exact(properties)
            .map(|vertex| [vertex[0], vertex[1], vertex[2]])
            .collect(),
        triangles: triangles
            .chunks_exact(3)
            .map(|triangle| [triangle[0] as u32, triangle[1] as u32, triangle[2] as u32])
            .collect(),
    }
}

/// Chordal deviation of an edge between facets meeting at angle theta is about
/// length times theta over eight.
fn estimated_deviation_p999(mesh: &TriangleMesh) -> Option<f64> {
    let normals: Vec<[f64; 3]> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| mesh.vertices[index as usize]);
            normalize(cross(sub(b, a), sub(c, a)))
        })
        .collect();
    let mut edges: Vec<(u32, u32, u32)> = mesh
        .triangles
        .iter()
        .enumerate()
        .flat_map(|(face, [a, b, c])| {
            [(*a, *b), (*b, *c), (*c, *a)].map(|(p, q)| (p.min(q), p.max(q), face as u32))
        })
        .collect();
    edges.sort_unstable();
    let mut deviations: Vec<f64> = edges
        .windows(2)
        .filter(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        .filter_map(|pair| {
            let cosine =
                dot(normals[pair[0].2 as usize], normals[pair[1].2 as usize]).clamp(-1.0, 1.0);
            let angle = cosine.acos();
            (angle.is_finite() && angle < SMOOTH_REGION_RADIANS).then(|| {
                let length = norm(sub(
                    mesh.vertices[pair[0].1 as usize],
                    mesh.vertices[pair[0].0 as usize],
                ));
                length * angle / 8.0
            })
        })
        .collect();
    if deviations.is_empty() {
        return None;
    }
    let index = ((deviations.len() as f64 * 0.999) as usize).min(deviations.len() - 1);
    Some(*deviations.select_nth_unstable_by(index, f64::total_cmp).1)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let length = norm(a);
    if length > 0.0 {
        a.map(|value| value / length)
    } else {
        [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open-bottomed box standing on y: a closed cube with its lowest face
    /// removed, so it has one boundary loop, plus a separate closed cube.
    fn open_box_with_part() -> (Vec<[f64; 3]>, Vec<[u32; 3]>) {
        let cube = |offset: [f64; 3], size: f64| {
            let v: Vec<[f64; 3]> = (0..8)
                .map(|i| {
                    [
                        offset[0] + size * f64::from(i & 1),
                        offset[1] + size * f64::from((i >> 1) & 1),
                        offset[2] + size * f64::from((i >> 2) & 1),
                    ]
                })
                .collect();
            // Outward-facing quads as triangles; vertex bit 0 is x, bit 1 is y, bit 2 is z.
            let quads = [
                [0, 4, 6, 2], // -x
                [1, 3, 7, 5], // +x
                [0, 1, 5, 4], // -y
                [2, 6, 7, 3], // +y
                [0, 2, 3, 1], // -z
                [4, 5, 7, 6], // +z
            ];
            (v, quads)
        };
        let (head, quads) = cube([0.0, 0.0, 0.0], 40.0);
        let mut triangles: Vec<[u32; 3]> = quads
            .iter()
            .enumerate()
            .filter(|(face, _)| *face != 2) // remove the lowest (-y) face
            .flat_map(|(_, [a, b, c, d])| [[*a, *b, *c], [*a, *c, *d]])
            .collect();
        let (part, quads) = cube([15.0, 25.0, 39.0], 10.0);
        let mut vertices = head;
        let base = vertices.len() as u32;
        vertices.extend(part);
        triangles.extend(
            quads
                .iter()
                .flat_map(|[a, b, c, d]| [[*a, *b, *c], [*a, *c, *d]])
                .map(|triangle| triangle.map(|index| index + base)),
        );
        (vertices, triangles)
    }

    fn request(edge: f64, max_triangles: usize) -> BustRequest {
        let (vertices, triangles) = open_box_with_part();
        BustRequest {
            vertices,
            triangles,
            height_mm: 50.0,
            edge_length_mm: edge,
            max_triangles,
            plinth: None,
            hollow: None,
            displacement: None,
            checks: None,
            eyes: None,
            body: None,
            mouth_recess_mm: None,
        }
    }

    /// Each face of the box and the part gets its own island in texture
    /// space, so every edge is a seam where texture coordinates change.
    fn with_map(mut request: BustRequest, value: impl Fn(f64, f64) -> f32) -> BustRequest {
        let side = 64;
        let values = (0..side * side)
            .map(|index| {
                let (row, column) = (index / side, index % side);
                let u = column as f64 / (side - 1) as f64;
                let v = 1.0 - row as f64 / (side - 1) as f64;
                value(u, v)
            })
            .collect();
        let triangle_uvs = request
            .triangles
            .iter()
            .enumerate()
            .map(|(index, triangle)| {
                let island = (index / 2) as f32;
                let normal = cross(
                    sub(
                        request.vertices[triangle[1] as usize],
                        request.vertices[triangle[0] as usize],
                    ),
                    sub(
                        request.vertices[triangle[2] as usize],
                        request.vertices[triangle[0] as usize],
                    ),
                );
                let axis = (0..3)
                    .max_by(|&a, &b| normal[a].abs().total_cmp(&normal[b].abs()))
                    .unwrap();
                let (first, second) = ((axis + 1) % 3, (axis + 2) % 3);
                triangle.map(|vertex| {
                    let point = request.vertices[vertex as usize];
                    [
                        (island + point[first] as f32 / 50.0) / 12.0,
                        point[second] as f32 / 50.0,
                    ]
                })
            })
            .collect();
        request.displacement = Some(Displacement {
            width: side,
            height: side,
            values,
            triangle_uvs,
            relief_mm: None,
            limit_relief: false,
            faded: Vec::new(),
        });
        request
    }

    #[test]
    fn zero_displacement_leaves_the_bust_unchanged_despite_texture_seams() {
        let (plain_mesh, plain) = prepare_bust(&request(1.0, 1_000_000)).unwrap();
        let (mesh, report) = prepare_bust(&with_map(request(1.0, 1_000_000), |_, _| 0.0)).unwrap();
        assert!(
            (report.volume_mm3 - plain.volume_mm3).abs() < 1e-9 * plain.volume_mm3,
            "{report:?} {plain:?}"
        );
        assert_eq!(mesh.triangles.len(), plain_mesh.triangles.len());
    }

    #[test]
    fn a_uniform_map_raises_every_point_along_its_normal() {
        let sphere = Manifold::sphere(10.0, 48);
        let mesh = mesh_from(&sphere);
        let part = ClosedPart {
            sources: (0..mesh.triangles.len()).map(Some).collect(),
            origins: (0..mesh.vertices.len() as u32).collect(),
            vertices: mesh.vertices,
            triangles: mesh.triangles,
            openings: Vec::new(),
        };
        let map = Displacement {
            width: 2,
            height: 2,
            values: vec![0.25; 4],
            triangle_uvs: vec![[[0.5, 0.5]; 3]; part.triangles.len()],
            relief_mm: None,
            limit_relief: false,
            faded: Vec::new(),
        };
        let solid = part_solid(&part, |vertex| vertex, Some(&map)).unwrap();
        let (raised, distances) = displaced(&solid, &map, 2.0);
        assert!(distances.iter().all(|&distance| distance == 0.5));
        for vertex in &raised.vertices {
            let radius = norm(*vertex);
            assert!((radius - 10.5).abs() < 0.01, "radius {radius}");
        }
    }

    #[test]
    fn detail_fades_to_nothing_at_the_centre_of_an_opening() {
        let (vertices, triangles) = open_box_with_part();
        let parts = closed_parts(&vertices, &triangles).unwrap();
        let head = parts
            .iter()
            .find(|part| !part.openings.is_empty())
            .expect("the box has an opening");
        assert_eq!(head.openings.len(), 1);
        let map = Displacement {
            width: 2,
            height: 2,
            values: vec![1.0; 4],
            triangle_uvs: (0..triangles.len())
                .map(|index| [[0.2, 0.2], [0.4, 0.2], [0.2 + 0.01 * index as f32, 0.6]])
                .collect(),
            relief_mm: None,
            limit_relief: false,
            faded: Vec::new(),
        };
        let solid = part_solid(head, |vertex| vertex, Some(&map)).unwrap();
        let mesh = solid.to_meshgl64();
        let properties = mesh.vert_properties();
        let weights: Vec<[f64; 3]> = properties
            .chunks_exact(DISPLACED_PROPERTIES)
            .map(|vertex| [vertex[3], vertex[4], vertex[5]])
            .collect();
        // One fan centre has weight zero and the mean coordinates of its rim;
        // every other vertex carries the map at full weight.
        let centres: Vec<_> = weights.iter().filter(|vertex| vertex[2] == 0.0).collect();
        assert_eq!(centres.len(), 1, "{weights:?}");
        assert!(centres[0][0] > 0.0 && centres[0][1] > 0.0);
        assert!(
            weights
                .iter()
                .all(|vertex| vertex[2] == 0.0 || vertex[2] == 1.0)
        );
        // Corners whose triangles disagree on texture coordinates are
        // repeated and merged back to one position.
        assert!(!mesh.merge_from_vert().is_empty());
    }

    #[test]
    fn unusable_displacement_maps_are_refused() {
        let base = with_map(request(2.0, 1_000_000), |_, _| 0.0);
        type Change = fn(&mut Displacement);
        let cases: [(&str, Change); 5] = [
            ("samples on each side", |map| map.width = 1),
            ("do not match the map size", |map| {
                map.values.pop();
            }),
            ("within 5%", |map| map.values[0] = 3.0),
            ("within 5%", |map| map.values[0] = f32::NAN),
            ("every head triangle", |map| {
                map.triangle_uvs.pop();
            }),
        ];
        for (message, change) in cases {
            let mut bad = base.clone();
            change(bad.displacement.as_mut().unwrap());
            match prepare_bust(&bad) {
                Err(GeometryError::InvalidOptions(text)) => {
                    assert!(text.contains(message), "{text}")
                }
                other => panic!("{message}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_displaced_bust_is_one_valid_solid_and_reports_the_applied_range() {
        let (mesh, report) = prepare_bust(&with_map(request(1.0, 1_000_000), |u, v| {
            (1.5 * (12.0 * u).sin() * (12.0 * v).cos()) as f32
        }))
        .unwrap();
        let applied = report.displacement.expect("displacement report");
        assert!(
            applied.lowest_mm < -0.5 && applied.highest_mm > 0.5,
            "{applied:?}"
        );
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);
        assert_eq!(validation.topology.self_intersections, Some(0));
    }

    #[test]
    fn relief_scales_detail_to_the_requested_depth_and_reports_percentiles() {
        let wave = |u: f64, v: f64| (0.5 * (12.0 * u).sin() * (12.0 * v).cos()) as f32;
        let (_, plain) = prepare_bust(&with_map(request(1.0, 1_000_000), wave)).unwrap();
        let plain = plain.displacement.expect("displacement report");
        assert_eq!(plain.relief_scale, 1.0);
        let percentiles = plain.percentiles_mm.expect("detail percentiles");
        assert!(percentiles.p1 < percentiles.p50 && percentiles.p50 < percentiles.p99);
        assert!(plain.lowest_mm <= percentiles.p1 && percentiles.p99 <= plain.highest_mm);

        let mut relieved = with_map(request(1.0, 1_000_000), wave);
        relieved.displacement.as_mut().unwrap().relief_mm = Some(0.3);
        let (mesh, report) = prepare_bust(&relieved).unwrap();
        let applied = report.displacement.expect("displacement report");
        assert!(applied.relief_scale != 1.0);
        // The surface samples the map unevenly, so the applied 95th
        // percentile lands near, not exactly on, the requested depth.
        let p95 = applied.depth_p95_mm.expect("detail depth");
        assert!((0.15..0.45).contains(&p95), "{applied:?}");
        assert!(validated(&mesh).solid_geometry);

        relieved.displacement.as_mut().unwrap().relief_mm = Some(1000.0);
        assert!(matches!(
            prepare_bust(&relieved),
            Err(GeometryError::InvalidOptions(text)) if text.contains("relief_mm")
        ));
        // Allowed to limit it, the relief stops at 5% of the head's height,
        // here 40 mm before scaling.
        relieved.displacement.as_mut().unwrap().limit_relief = true;
        let (mesh, report) = prepare_bust(&relieved).unwrap();
        let applied = report.displacement.expect("displacement report");
        assert!(applied.relief_limited, "{applied:?}");
        let largest = applied.lowest_mm.abs().max(applied.highest_mm);
        let limit = MAX_DISPLACEMENT_FRACTION * 40.0 * report.life_size_fraction;
        assert!(largest <= limit * 1.01, "{largest} {limit}");
        assert!(validated(&mesh).solid_geometry);
        relieved.displacement.as_mut().unwrap().limit_relief = false;
        relieved.displacement.as_mut().unwrap().relief_mm = Some(-1.0);
        assert!(matches!(
            prepare_bust(&relieved),
            Err(GeometryError::InvalidOptions(text)) if text.contains("relief")
        ));
    }

    #[test]
    fn the_detail_depth_is_the_percentile_of_its_magnitude() {
        // Mostly shallow detail both ways, with a little deeper detail on
        // each side: the signed 5th and 95th percentiles stay shallow.
        let mut distances: Vec<f64> = [(-0.3, 4), (-0.1, 46), (0.1, 46), (0.3, 4)]
            .into_iter()
            .flat_map(|(value, count)| std::iter::repeat_n(value, count))
            .collect();
        let report = detail_report(&mut distances, 2.0, (1.0, false));
        let percentiles = report.percentiles_mm.expect("detail percentiles");
        assert_eq!((percentiles.p5, percentiles.p95), (-0.2, 0.2));
        assert_eq!(report.depth_p95_mm, Some(0.6));
    }

    #[test]
    fn map_values_the_head_never_samples_do_not_decide_the_relief() {
        let wave = |u: f64, v: f64| (0.2 * (40.0 * u).sin() * (12.0 * v).cos()) as f32;
        let relieved = |background: f32| {
            let mut request = with_map(request(1.0, 1_000_000), wave);
            let map = request.displacement.as_mut().unwrap();
            map.relief_mm = Some(0.3);
            // The head samples only the left half of the map.
            for corner in map.triangle_uvs.iter_mut().flatten() {
                corner[0] *= 0.5;
            }
            let reach = map
                .triangle_uvs
                .iter()
                .flatten()
                .map(|corner| corner[0])
                .fold(0.0f32, f32::max);
            // Well beyond every sampled point, past the reach of interpolation.
            let start = (f64::from(reach) * (map.width - 1) as f64).ceil() as usize + 2;
            assert!(start + 8 < map.width, "the test needs unsampled columns");
            for row in 0..map.height {
                for column in start..map.width {
                    map.values[row * map.width + column] = background;
                }
            }
            prepare_bust(&request)
                .unwrap()
                .1
                .displacement
                .unwrap()
                .relief_scale
        };
        let (quiet, loud) = (relieved(0.0), relieved(1.0));
        assert!((quiet - loud).abs() < 1e-9 * quiet, "{quiet} {loud}");
    }

    #[test]
    fn a_map_without_detail_is_left_unscaled_by_relief() {
        let mut flat = with_map(request(1.0, 1_000_000), |_, _| 0.0);
        flat.displacement.as_mut().unwrap().relief_mm = Some(0.3);
        let (_, report) = prepare_bust(&flat).unwrap();
        let applied = report.displacement.expect("displacement report");
        assert_eq!(applied.relief_scale, 1.0);
        assert_eq!(applied.percentiles_mm, None);
        // Rounding noise is not detail either.
        let mut noise = with_map(request(1.0, 1_000_000), |u, _| (1e-5 * u) as f32);
        noise.displacement.as_mut().unwrap().relief_mm = Some(0.3);
        let (_, report) = prepare_bust(&noise).unwrap();
        assert_eq!(report.displacement.unwrap().relief_scale, 1.0);
    }

    fn validated(mesh: &TriangleMesh) -> crate::ValidationReport {
        crate::analyze_stl(&bust_stl(mesh), crate::ValidationOptions::default()).unwrap()
    }

    #[test]
    fn a_plinth_raises_the_bust_within_the_requested_height() {
        let mut with_plinth = request(2.0, 1_000_000);
        with_plinth.plinth = Some(Plinth {
            height_mm: 8.0,
            diameter_mm: None,
            chamfer_mm: 0.0,
        });
        let (mesh, report) = prepare_bust(&with_plinth).unwrap();
        let plinth = report.plinth.expect("plinth report");
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6, "{report:?}");
        // The default plinth is at least 70% of the bust's widest extent.
        assert!(plinth.diameter_mm >= 0.7 * 50.0, "{plinth:?}");
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);

        with_plinth.plinth = Some(Plinth {
            height_mm: 8.0,
            diameter_mm: Some(10.0),
            chamfer_mm: 0.0,
        });
        assert!(matches!(
            prepare_bust(&with_plinth),
            Err(GeometryError::InvalidOptions(_))
        ));
    }

    #[test]
    fn a_plinth_near_the_triangle_limit_coarsens_the_bust_instead_of_failing() {
        let mut tight = request(0.05, 12_000);
        tight.plinth = Some(Plinth {
            height_mm: 8.0,
            diameter_mm: Some(400.0),
            chamfer_mm: 0.0,
        });
        let (mesh, report) = prepare_bust(&tight).unwrap();
        assert!(mesh.triangles.len() <= 12_000, "{report:?}");
        assert!(validated(&mesh).solid_geometry);
    }

    /// A 40 mm box open at the bottom, its neck, with a 20 by 15 mm mouth
    /// in its front face, which faces +z.
    fn box_with_a_mouth() -> BustRequest {
        let (vertices, triangles) = open_box_with_part();
        let mut request = request(1.0, 1_000_000);
        request.vertices = vertices[..8].to_vec();
        request.vertices.extend([
            [10.0, 15.0, 40.0],
            [30.0, 15.0, 40.0],
            [30.0, 30.0, 40.0],
            [10.0, 30.0, 40.0],
        ]);
        // The open box's faces are -x, +x, +y, -z and +z, two triangles
        // each; the front, +z, becomes a ring around the mouth.
        request.triangles = triangles[..8].to_vec();
        let (outer, inner) = ([4, 5, 7, 6], [8, 9, 10, 11]);
        for side in 0..4 {
            let next = (side + 1) % 4;
            request
                .triangles
                .push([outer[side], outer[next], inner[next]]);
            request
                .triangles
                .push([outer[side], inner[next], inner[side]]);
        }
        request
    }

    #[test]
    fn a_recessed_mouth_cap_is_set_back_into_the_head() {
        let (flat_mesh, flat) = prepare_bust(&box_with_a_mouth()).unwrap();
        assert!(validated(&flat_mesh).solid_geometry);
        let mut recessed = box_with_a_mouth();
        recessed.mouth_recess_mm = Some(5.0);
        let (mesh, report) = prepare_bust(&recessed).unwrap();
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);
        // A cone 5 mm deep over the 20 by 15 mm mouth, about 500 mm³, is
        // taken out of the front.
        let removed = flat.volume_mm3 - report.volume_mm3;
        assert!((300.0..900.0).contains(&removed), "{removed} {flat:?}");
        // The neck stays flat on the plate.
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6);
        assert!(
            validation.overhang.bed_contact.area_mm2 > 100.0,
            "{validation:?}"
        );

        recessed.mouth_recess_mm = Some(20.0);
        assert!(matches!(
            prepare_bust(&recessed),
            Err(GeometryError::InvalidOptions(text)) if text.contains("mouth recess")
        ));
        recessed.mouth_recess_mm = Some(-1.0);
        assert!(matches!(
            prepare_bust(&recessed),
            Err(GeometryError::InvalidOptions(_))
        ));
    }

    #[test]
    fn a_turned_mouth_is_recessed_into_the_head() {
        // The box turned a quarter turn about its vertical axis, so that its
        // mouth faces +x rather than +z.
        let turned = || {
            let mut request = box_with_a_mouth();
            for vertex in &mut request.vertices {
                let [x, y, z] = *vertex;
                *vertex = [z, y, -x];
            }
            request
        };
        let (_, flat) = prepare_bust(&turned()).unwrap();
        let mut recessed = turned();
        recessed.mouth_recess_mm = Some(5.0);
        let (mesh, report) = prepare_bust(&recessed).unwrap();
        assert!(validated(&mesh).solid_geometry);
        let removed = flat.volume_mm3 - report.volume_mm3;
        assert!((300.0..900.0).contains(&removed), "{removed} {flat:?}");
    }

    #[test]
    fn faded_detail_does_not_set_the_relief() {
        // Weak detail on the box and strong detail on the part, whose
        // islands lie beyond u = 5 / 12; the part is faded.
        let wave = |u: f64, v: f64| {
            let strength = if u >= 5.0 / 12.0 { 2.0 } else { 0.2 };
            (strength * (40.0 * u).sin() * (12.0 * v).cos()) as f32
        };
        let mut faded = with_map(request(1.0, 1_000_000), wave);
        let map = faded.displacement.as_mut().unwrap();
        map.faded = (8..16).collect();
        map.relief_mm = Some(0.3);
        let (_, report) = prepare_bust(&faded).unwrap();
        let depth = report
            .displacement
            .expect("displacement report")
            .depth_p95_mm
            .expect("detail depth");
        assert!((0.15..0.45).contains(&depth), "{depth}");
    }

    #[test]
    fn a_partly_faded_centre_counts_at_its_applied_depth() {
        // One triangle with two faded corners: its centre keeps a third of
        // the map's value there. The map is 1 around the centre and 0.2 at
        // the corners.
        let vertices = [[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]];
        let triangles = [[0, 1, 2]];
        let corners = [[0.1f32, 0.1], [0.9, 0.1], [0.1, 0.9]];
        let centre = [0.1 + 0.8 / 3.0, 0.1 + 0.8 / 3.0];
        let side = 64;
        let values = (0..side * side)
            .map(|index| {
                let u = (index % side) as f64 / (side - 1) as f64;
                let v = 1.0 - (index / side) as f64 / (side - 1) as f64;
                if (u - centre[0]).hypot(v - centre[1]) < 0.1 {
                    1.0
                } else {
                    0.2
                }
            })
            .collect();
        let map = Displacement {
            width: side,
            height: side,
            values,
            triangle_uvs: vec![corners],
            relief_mm: Some(0.5),
            limit_relief: false,
            faded: vec![1, 2],
        };
        let (_, factor, _) = with_relief(&map, (&vertices, &triangles, &[]), 1.0, 1000.0).unwrap();
        // The reference is the centre's applied third, not its full value.
        assert!((factor - 1.5).abs() < 0.05, "{factor}");
    }

    #[test]
    fn values_a_cap_can_reach_count_toward_the_limit() {
        // Weak detail on every face's island and strong values in the middle
        // of the gaps between islands, beyond what samples on the islands'
        // edges read, which only the neck cap, whose rim spans four faces,
        // can reach.
        // The open box alone: its islands span less than the 1/12 they are
        // spaced by, while the stand-in eye's would reach into the gaps.
        let mut open_box = request(1.0, 1_000_000);
        open_box.vertices.truncate(8);
        open_box.triangles.truncate(10);
        let mut capped = with_map(open_box, |_, _| 0.0);
        let side = 256;
        let map = capped.displacement.as_mut().unwrap();
        map.width = side;
        map.height = side;
        map.values = (0..side * side)
            .map(|index| {
                let u = (index % side) as f64 / (side - 1) as f64;
                if (0.87..0.93).contains(&(u * 12.0).fract()) {
                    1.0
                } else {
                    (0.01 * (40.0 * u).sin()) as f32
                }
            })
            .collect();
        map.relief_mm = Some(1.0);
        assert!(matches!(
            prepare_bust(&capped),
            Err(GeometryError::InvalidOptions(text)) if text.contains("relief_mm")
        ));
    }

    #[test]
    fn faded_vertices_take_no_detail() {
        let mut faded = with_map(request(1.0, 1_000_000), |_, _| 0.5);
        faded.displacement.as_mut().unwrap().faded = (0..16).collect();
        let (mesh, report) = prepare_bust(&faded).unwrap();
        let applied = report.displacement.expect("displacement report");
        assert_eq!((applied.lowest_mm, applied.highest_mm), (0.0, 0.0));
        assert!(validated(&mesh).solid_geometry);
        // Only the part's vertices: the box keeps its detail.
        faded.displacement.as_mut().unwrap().faded = (8..16).collect();
        let applied = prepare_bust(&faded).unwrap().1.displacement.unwrap();
        assert!(applied.highest_mm > 0.1, "{applied:?}");
        for unusable in [vec![3, 2], vec![2, 2], vec![16]] {
            faded.displacement.as_mut().unwrap().faded = unusable;
            assert!(matches!(
                prepare_bust(&faded),
                Err(GeometryError::InvalidOptions(_))
            ));
        }
    }

    /// The fixture's small cube, stretched to jut 24 mm out of the box's
    /// face, stands in for an eyeball centred 5 mm behind its end and
    /// looking along +z.
    fn with_an_eye(mut request: BustRequest) -> BustRequest {
        for vertex in &mut request.vertices[8..] {
            if vertex[2] > 40.0 {
                vertex[2] += 15.0;
            }
        }
        request
    }

    fn carving(iris_ring: bool) -> EyeCarving {
        EyeCarving {
            eyes: vec![EyeAxis {
                centre: [20.0, 30.0, 59.0],
                front: [20.0, 30.0, 64.0],
            }],
            iris_ring,
            pupil_diameter_mm: Some(2.0),
            iris_diameter_mm: Some(6.0),
            depth_mm: Some(1.0),
            smallest_mm: None,
            shallowest_mm: None,
        }
    }

    #[test]
    fn eyes_are_carved_along_their_gaze() {
        let (_, plain) = prepare_bust(&with_an_eye(request(1.0, 1_000_000))).unwrap();
        for iris_ring in [false, true] {
            let mut carved = with_an_eye(request(1.0, 1_000_000));
            carved.eyes = Some(carving(iris_ring));
            let (mesh, report) = prepare_bust(&carved).unwrap();
            let validation = validated(&mesh);
            assert!(validation.solid_geometry, "{validation:?}");
            assert_eq!(validation.topology.connected_components, 1);
            let eyes = report.eyes.expect("eye report");
            assert_eq!((eyes.pupil_diameter_mm, eyes.depth_mm), (2.0, 1.0));
            assert_eq!(eyes.iris_diameter_mm, iris_ring.then_some(6.0));
            // Smoothing rounds the stretched part's end out to about 7.7 mm
            // from the eye's centre.
            assert!((13.0..18.0).contains(&eyes.eyeball_diameter_mm), "{eyes:?}");
            // A pupil 2 mm across and 1 mm deep takes about 3 mm³; the ring
            // adds a groove around it.
            let removed = plain.volume_mm3 - report.volume_mm3;
            let expected = if iris_ring { 4.0..40.0 } else { 1.5..6.0 };
            assert!(expected.contains(&removed), "{removed} {eyes:?}");
            assert!(
                (eyes.removed_mm3 - removed).abs() < 0.01 * removed,
                "{eyes:?}"
            );
        }
    }

    #[test]
    fn a_shallow_pupil_shows_its_full_width() {
        let mut shallow = with_an_eye(request(1.0, 1_000_000));
        shallow.eyes = Some(EyeCarving {
            pupil_diameter_mm: Some(4.0),
            depth_mm: Some(0.05),
            ..carving(false)
        });
        let (mesh, report) = prepare_bust(&shallow).unwrap();
        assert!(validated(&mesh).solid_geometry);
        // A 4 mm disc 0.05 mm deep at its rim, plus the dome of the eyeball
        // above the rim: about 1.5 mm³. Measured from the pole instead, the
        // cut would show only a spot near the middle.
        let removed = report.eyes.expect("eye report").removed_mm3;
        assert!((0.8..4.0).contains(&removed), "{removed}");
    }

    #[test]
    fn detail_does_not_move_a_carved_eye() {
        let removed = |map: bool| {
            let mut carved = with_an_eye(request(1.0, 1_000_000));
            if map {
                carved = with_map(carved, |_, _| 0.5);
            }
            carved.eyes = Some(carving(true));
            prepare_bust(&carved)
                .unwrap()
                .1
                .eyes
                .expect("eye report")
                .removed_mm3
        };
        let (plain, detailed) = (removed(false), removed(true));
        assert!((plain - detailed).abs() < 0.1 * plain, "{plain} {detailed}");
    }

    #[test]
    fn the_ring_is_a_tenth_of_the_iris_used() {
        let mut wide = with_an_eye(request(1.0, 1_000_000));
        wide.eyes = Some(EyeCarving {
            iris_diameter_mm: Some(8.0),
            ..carving(true)
        });
        let eyes = prepare_bust(&wide).unwrap().1.eyes.expect("eye report");
        assert_eq!(eyes.ring_width_mm, Some(0.8));
    }

    #[test]
    fn a_cut_the_surface_covers_leaves_no_void() {
        // The small cube moved to the middle of the box, sealed inside it:
        // its pupil cannot reach the surface.
        let mut covered = request(1.0, 1_000_000);
        for vertex in &mut covered.vertices[8..] {
            vertex[1] -= 10.0;
            vertex[2] -= 24.0;
        }
        covered.eyes = Some(EyeCarving {
            eyes: vec![EyeAxis {
                centre: [20.0, 20.0, 20.0],
                front: [20.0, 20.0, 25.0],
            }],
            ..carving(false)
        });
        let (mesh, report) = prepare_bust(&covered).unwrap();
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(
            validation.topology.connected_components, 1,
            "{validation:?}"
        );
        assert!(report.eyes.expect("eye report").removed_mm3.abs() < 1e-6);
    }

    #[test]
    fn the_ring_keeps_the_printers_minimum_under_a_shallow_pupil() {
        let mut shallow = with_an_eye(request(1.0, 1_000_000));
        shallow.eyes = Some(EyeCarving {
            depth_mm: Some(0.1),
            shallowest_mm: Some(0.4),
            ..carving(true)
        });
        let eyes = prepare_bust(&shallow).unwrap().1.eyes.expect("eye report");
        assert_eq!((eyes.depth_mm, eyes.ring_depth_mm), (0.1, Some(0.4)));
    }

    #[test]
    fn eye_detail_that_does_not_fit_is_refused() {
        let mut request = with_an_eye(request(1.0, 1_000_000));
        let mut eyes = carving(true);
        // An iris wider than the eyeball.
        eyes.iris_diameter_mm = Some(15.0);
        request.eyes = Some(eyes.clone());
        assert!(matches!(
            prepare_bust(&request),
            Err(GeometryError::InvalidOptions(text)) if text.contains("does not fit")
        ));
        // A ring that would swallow the pupil.
        eyes.iris_diameter_mm = Some(2.5);
        request.eyes = Some(eyes.clone());
        assert!(matches!(
            prepare_bust(&request),
            Err(GeometryError::InvalidOptions(text)) if text.contains("does not fit")
        ));
        for unusable in [
            EyeCarving {
                eyes: Vec::new(),
                ..carving(false)
            },
            EyeCarving {
                depth_mm: Some(-1.0),
                ..carving(false)
            },
            EyeCarving {
                eyes: vec![EyeAxis {
                    centre: [20.0, 30.0, 59.0],
                    front: [20.0, 30.0, 59.0],
                }],
                ..carving(false)
            },
        ] {
            request.eyes = Some(unusable);
            assert!(matches!(
                prepare_bust(&request),
                Err(GeometryError::InvalidOptions(_))
            ));
        }
    }

    fn body(kind: BodyKind, height_mm: Option<f64>) -> Option<Body> {
        Some(Body {
            kind,
            height_mm,
            width_mm: None,
            depth_mm: None,
        })
    }

    /// The mean x of the vertices resting on the build plate.
    fn foot_x(mesh: &TriangleMesh) -> f64 {
        let foot: Vec<f64> = mesh
            .vertices
            .iter()
            .filter(|vertex| vertex[2] < 1e-6)
            .map(|vertex| vertex[0])
            .collect();
        foot.iter().sum::<f64>() / foot.len() as f64
    }

    #[test]
    fn an_extended_neck_or_shoulders_stand_below_the_head() {
        let (_, plain) = prepare_bust(&request(2.0, 1_000_000)).unwrap();
        let mut extended = request(2.0, 1_000_000);
        extended.body = body(BodyKind::ExtendedNeck, Some(10.0));
        let (mesh, report) = prepare_bust(&extended).unwrap();
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6, "{report:?}");
        let neck = report.body.expect("body report");
        assert_eq!((neck.kind, neck.height_mm), (BodyKind::ExtendedNeck, 10.0));
        // Smoothing rounds the box's foot a little unevenly.
        assert!(neck.lean_degrees < 8.0, "{neck:?}");
        // The head shrinks to 40 mm to make room.
        assert!(
            (report.life_size_fraction / plain.life_size_fraction - 0.8).abs() < 0.01,
            "{report:?}"
        );

        let mut shoulders = request(2.0, 1_000_000);
        shoulders.body = body(BodyKind::Shoulders, None);
        shoulders.plinth = Some(Plinth {
            height_mm: 5.0,
            diameter_mm: None,
            chamfer_mm: 0.5,
        });
        let (mesh, report) = prepare_bust(&shoulders).unwrap();
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);
        let block = report.body.expect("body report");
        // 45 mm above the plinth: a head of 45 / 1.35 and shoulders of the rest.
        assert!(
            (block.height_mm - 45.0 * 0.35 / 1.35).abs() < 1e-9,
            "{block:?}"
        );
        assert!(
            block.width_mm.unwrap() > report.size_mm[0] * 0.9,
            "{report:?}"
        );
    }

    #[test]
    fn shoulders_narrower_than_the_neck_are_refused() {
        let mut narrow = request(2.0, 1_000_000);
        narrow.body = Some(Body {
            kind: BodyKind::Shoulders,
            height_mm: Some(10.0),
            width_mm: Some(10.0),
            depth_mm: None,
        });
        assert!(matches!(
            prepare_bust(&narrow),
            Err(GeometryError::InvalidOptions(text)) if text.contains("cannot hold the neck")
        ));
    }

    #[test]
    fn short_shoulders_stay_above_the_plate() {
        // On a 100 mm bust with 2 mm edges, the stretched band is about
        // 4 mm, twice the block's height.
        let mut short = request(2.0, 1_000_000);
        short.height_mm = 100.0;
        short.body = body(BodyKind::Shoulders, Some(2.0));
        let (mesh, report) = prepare_bust(&short).unwrap();
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert!((report.size_mm[2] - 100.0).abs() < 1e-6, "{report:?}");
        assert!(mesh.vertices.iter().all(|vertex| vertex[2] >= -1e-9));
    }

    #[test]
    fn a_body_follows_a_leaning_neck() {
        // The box leans toward +x as it rises.
        let mut leaning = request(2.0, 1_000_000);
        for vertex in &mut leaning.vertices {
            vertex[0] += 0.3 * vertex[1];
        }
        leaning.body = body(BodyKind::ExtendedNeck, Some(10.0));
        let (mesh, report) = prepare_bust(&leaning).unwrap();
        assert!(validated(&mesh).solid_geometry);
        let neck = report.body.expect("body report");
        assert!((10.0..25.0).contains(&neck.lean_degrees), "{neck:?}");
        // The neck's foot lies back toward -x of the head's base.
        let mut upright = leaning.clone();
        upright.body = None;
        let (head, _) = prepare_bust(&upright).unwrap();
        assert!(
            foot_x(&mesh) < foot_x(&head) - 1.0,
            "{} {}",
            foot_x(&mesh),
            foot_x(&head)
        );
    }

    #[test]
    fn splitting_at_a_height_keeps_a_closed_surface() {
        let solid = Manifold::sphere(10.0, 24).rotate(10.0, 20.0, 0.0);
        let mesh = mesh_from(&solid);
        // Through vertices on the plane as well as across edges.
        for height in [0.0, 3.3, mesh.vertices[5][2]] {
            let split = split_at_height(&mesh, height);
            assert!(split.triangles.len() > mesh.triangles.len());
            for triangle in &split.triangles {
                let sides = triangle.map(|vertex| {
                    let z = split.vertices[vertex as usize][2];
                    (z > height) as i8 - (z < height) as i8
                });
                assert!(!(sides.contains(&1) && sides.contains(&-1)), "{sides:?}");
            }
            let mut directed = HashSet::new();
            for [a, b, c] in &split.triangles {
                for edge in [(*a, *b), (*b, *c), (*c, *a)] {
                    assert!(directed.insert(edge));
                }
            }
            assert!(directed.iter().all(|(a, b)| directed.contains(&(*b, *a))));
        }
    }

    #[test]
    fn a_body_must_leave_the_head_room() {
        let mut tall = request(2.0, 1_000_000);
        tall.body = body(BodyKind::Shoulders, Some(30.0));
        assert!(matches!(
            prepare_bust(&tall),
            Err(GeometryError::InvalidOptions(text)) if text.contains("less than 50%")
        ));
        tall.body = Some(Body {
            kind: BodyKind::ExtendedNeck,
            height_mm: Some(5.0),
            width_mm: Some(40.0),
            depth_mm: None,
        });
        assert!(matches!(
            prepare_bust(&tall),
            Err(GeometryError::InvalidOptions(_))
        ));
    }

    #[test]
    fn a_part_apart_from_the_head_is_joined_by_a_stem() {
        // The open box and, 5 mm beyond its +x face, a separate cube.
        let (mut vertices, mut triangles) = open_box_with_part();
        vertices.truncate(8);
        triangles.truncate(10);
        let cube = Manifold::cube(10.0, 10.0, 10.0, false).translate(45.0, 15.0, 15.0);
        let cube_mesh = mesh_from(&cube);
        let offset = vertices.len() as u32;
        vertices.extend(cube_mesh.vertices);
        triangles.extend(
            cube_mesh
                .triangles
                .iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        let parts = closed_parts(&vertices, &triangles).unwrap();
        let solid = parts
            .iter()
            .map(|part| part_solid(part, |vertex| vertex, None).unwrap())
            .reduce(|a, b| a.union(&b))
            .unwrap();
        assert_eq!(solid.decompose().len(), 2);
        let joined = join_parts(solid, &parts, |vertex| vertex).unwrap();
        assert_eq!(joined.decompose().len(), 1);
    }

    #[test]
    fn a_chamfered_plinth_stands_on_a_smaller_footprint() {
        let mut chamfered = request(2.0, 1_000_000);
        chamfered.plinth = Some(Plinth {
            height_mm: 8.0,
            diameter_mm: Some(60.0),
            chamfer_mm: 2.0,
        });
        let (mesh, report) = prepare_bust(&chamfered).unwrap();
        assert_eq!(report.plinth.expect("plinth report").chamfer_mm, 2.0);
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6, "{report:?}");
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        assert_eq!(validation.topology.connected_components, 1);
        // The plate holds a disc 2 mm narrower all round.
        let footprint = std::f64::consts::PI * 28.0 * 28.0;
        let bed = validation.overhang.bed_contact.area_mm2;
        assert!(
            (bed - footprint).abs() < 0.01 * footprint,
            "{bed} {footprint}"
        );

        for chamfer in [8.0, -1.0, f64::NAN] {
            chamfered.plinth = Some(Plinth {
                height_mm: 8.0,
                diameter_mm: Some(60.0),
                chamfer_mm: chamfer,
            });
            assert!(matches!(
                prepare_bust(&chamfered),
                Err(GeometryError::InvalidOptions(_))
            ));
        }
    }

    #[test]
    fn checks_find_the_overhanging_part_and_name_it_by_a_nearby_vertex() {
        // The part, stretched to 25 mm long, juts out of the box's face 25 to
        // 35 mm up, so its underside overhangs. The box's eight vertices come
        // first, then the part's.
        let mut checked = request(1.0, 1_000_000);
        for vertex in &mut checked.vertices[8..] {
            if vertex[2] > 40.0 {
                vertex[2] += 15.0;
            }
        }
        checked.plinth = Some(Plinth {
            height_mm: 5.0,
            diameter_mm: None,
            chamfer_mm: 1.0,
        });
        checked.checks = Some(PrintChecks {
            overhang_degrees: 45.0,
            min_feature_mm: None,
        });
        let (_, report) = prepare_bust(&checked).unwrap();
        // The head is 39.6 mm tall above the cut. Smoothing bows the box's
        // top upward, so the 45 mm above the plinth hold a little less of it.
        let fraction = report.life_size_fraction;
        assert!(
            (0.7 * 45.0..45.0).contains(&(fraction * 39.6)),
            "{fraction}"
        );
        let overhang = report.overhang.expect("overhang report");
        assert!(overhang.area_mm2 > 0.0);
        let on_part = |region: &crate::SurfaceRegion| region.nearest_vertex >= 8;
        let part = overhang
            .regions
            .iter()
            .find(|region| on_part(region))
            .expect("an overhang on the part");
        // On the finished bust, raised by the plinth, near the part's
        // underside, 25 mm up before scaling and 0.4 mm above the cut.
        let height = (part.centre_mm[2] - 5.0) / fraction;
        assert!((20.0..30.0).contains(&height), "{part:?}");
        assert!(report.thin.is_none());

        checked.checks = Some(PrintChecks {
            overhang_degrees: 90.0,
            min_feature_mm: None,
        });
        let (_, report) = prepare_bust(&checked).unwrap();
        assert_eq!(report.overhang.expect("overhang report").area_mm2, 0.0);
        checked.checks = Some(PrintChecks {
            overhang_degrees: 91.0,
            min_feature_mm: None,
        });
        assert!(matches!(
            prepare_bust(&checked),
            Err(GeometryError::InvalidOptions(_))
        ));
    }

    #[test]
    fn walls_thinner_than_the_smallest_feature_are_reported() {
        let mut hollow = request(2.0, 1_000_000);
        hollow.hollow = Some(Hollow {
            wall_mm: 3.0,
            drain_diameter_mm: 3.0,
        });
        hollow.checks = Some(PrintChecks {
            overhang_degrees: 45.0,
            min_feature_mm: Some(4.0),
        });
        let (mesh, report) = prepare_bust(&hollow).unwrap();
        // The outer surface and the cavity are 3 mm apart almost everywhere.
        let surface = validated(&mesh).surface_area_mm2;
        let thin = report.thin.expect("thin report");
        assert!(thin.area_mm2 > 0.5 * surface, "{} {surface}", thin.area_mm2);
        hollow.checks = Some(PrintChecks {
            overhang_degrees: 45.0,
            min_feature_mm: Some(2.0),
        });
        let thin = prepare_bust(&hollow).unwrap().1.thin.expect("thin report");
        assert!(
            thin.area_mm2 < 0.01 * surface,
            "{} {surface}",
            thin.area_mm2
        );
    }

    #[test]
    fn hollowing_leaves_walls_and_drains_through_the_base() {
        let solid = prepare_bust(&request(2.0, 1_000_000)).unwrap().1;
        let mut hollow = request(2.0, 1_000_000);
        hollow.hollow = Some(Hollow {
            wall_mm: 3.0,
            drain_diameter_mm: 3.0,
        });
        let (mesh, report) = prepare_bust(&hollow).unwrap();
        let cavity = report.hollow.clone().expect("hollow report");
        assert!(!cavity.drain_holes_mm.is_empty());
        assert!(
            cavity.cavity_volume_mm3 > 0.5 * solid.volume_mm3,
            "{report:?}"
        );
        assert!(
            (report.volume_mm3 + cavity.cavity_volume_mm3 - solid.volume_mm3).abs()
                < 0.05 * solid.volume_mm3
        );
        let validation = validated(&mesh);
        assert!(validation.solid_geometry, "{validation:?}");
        // One shell: the cavity opens to the outside through the drain holes.
        assert_eq!(validation.topology.connected_components, 1);

        hollow.hollow = Some(Hollow {
            wall_mm: 40.0,
            drain_diameter_mm: 3.0,
        });
        assert!(matches!(
            prepare_bust(&hollow),
            Err(GeometryError::InvalidOptions(_))
        ));
    }

    #[test]
    fn open_surface_and_part_become_one_upright_solid_on_the_build_plate() {
        let (mesh, report) = prepare_bust(&request(2.0, 1_000_000)).unwrap();
        assert!(report.trimmed_at_neck);
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6, "{report:?}");
        // The 40 mm box is cut just above its open bottom, so almost all of
        // its height is kept and its width scales with it.
        assert!(report.size_mm[0] < 70.0, "{report:?}");
        assert!(report.volume_mm3 > 0.0);
        let lowest = mesh
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::MAX, f64::min);
        assert!(lowest.abs() < 1e-9, "base rests at {lowest}");
        let manifold = Manifold::from_mesh_f64(
            &mesh.vertices.iter().flatten().copied().collect::<Vec<_>>(),
            3,
            &mesh
                .triangles
                .iter()
                .flatten()
                .map(|&index| u64::from(index))
                .collect::<Vec<_>>(),
        )
        .expect("output is a valid closed solid");
        assert_eq!(manifold.genus(), 0, "the part is united with the head");
    }

    #[test]
    fn triangle_limit_coarsens_the_edge_length() {
        let (_, report) = prepare_bust(&request(0.05, 20_000)).unwrap();
        assert!(report.triangles <= 20_000, "{report:?}");
        assert!(report.edge_length_mm > report.requested_edge_length_mm);
    }

    #[test]
    fn stl_round_trips_through_the_validation_pipeline() {
        let (mesh, _) = prepare_bust(&request(2.0, 1_000_000)).unwrap();
        let report =
            crate::analyze_stl(&bust_stl(&mesh), crate::ValidationOptions::default()).unwrap();
        assert!(report.printable, "{report:?}");
        assert_eq!(report.triangles, mesh.triangles.len());
    }

    #[test]
    fn invalid_requests_are_refused() {
        let mut bad = request(2.0, 1_000);
        bad.height_mm = 0.0;
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
        let mut bad = request(2.0, 1_000);
        bad.triangles[0] = [0, 0, 1];
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
        let mut bad = request(2.0, 1_000);
        bad.triangles.push(bad.triangles[0]);
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
    }
}
