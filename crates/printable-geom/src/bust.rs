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
/// A default plinth is this fraction of the bust's widest extent across.
const PLINTH_WIDTH_FRACTION: f64 = 0.7;
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
    /// Defaults to 70% of the bust's widest extent, and never less than the
    /// neck section it supports.
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
    let bust_height = request.height_mm - request.plinth.map_or(0.0, |plinth| plinth.height_mm);
    let parts = closed_parts(&request.vertices, &request.triangles)?;
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
    let centre = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0, low[2]];
    let place = |vertex: [f64; 3]| {
        let vertex = upright(vertex);
        [0, 1, 2].map(|axis| (vertex[axis] - centre[axis]) * scale)
    };
    let relieved = match &request.displacement {
        Some(map) => Some(with_relief(
            map,
            (&request.vertices, &request.triangles),
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
    let mut head: Vec<(u32, [f64; 3])> = match request.checks {
        Some(_) => request
            .vertices
            .iter()
            .enumerate()
            .filter(|(_, vertex)| kept_height(vertex))
            .map(|(index, vertex)| {
                let placed = place(*vertex);
                let point = [0, 1, 2].map(|axis| (placed[axis] - offset[axis]) * correction);
                (index as u32, point)
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
    };
    Ok(Prepared::Done(mesh, Box::new(report)))
}

/// The map to apply, the factor it was scaled by and whether that factor was
/// limited: with `relief_mm`, the map is scaled so that its 95th-percentile
/// magnitude over samples that carry detail reaches that depth once the head
/// is scaled by `scale`.
fn with_relief(
    map: &Displacement,
    (vertices, triangles): (&[[f64; 3]], &[[u32; 3]]),
    scale: f64,
    head_height: f64,
) -> Result<(Displacement, f64, bool), GeometryError> {
    let Some(relief) = map.relief_mm else {
        return Ok((map.clone(), 1.0, false));
    };
    // The reference is the map as the head samples it: at every triangle's
    // corners and centre, interpolated as the bust interpolates it, and
    // weighted by the triangle's area, as the refined surface spreads its
    // points by area. Map values the head never reaches do not count.
    let mut samples: Vec<(f64, f64)> = map
        .triangle_uvs
        .iter()
        .zip(triangles)
        .flat_map(|(corners, triangle)| {
            let [a, b, c] = triangle.map(|vertex| vertices[vertex as usize]);
            let area = 0.5 * norm(cross(sub(b, a), sub(c, a)));
            let centre = [0, 1].map(|axis| {
                corners
                    .iter()
                    .map(|corner| f64::from(corner[axis]))
                    .sum::<f64>()
                    / 3.0
            });
            corners
                .iter()
                .map(|corner| corner.map(f64::from))
                .chain([centre])
                .map(move |[u, v]| (map.sample(u, v).abs(), area / 4.0))
                .collect::<Vec<_>>()
        })
        .collect();
    let sampled_largest = samples
        .iter()
        .map(|(magnitude, _)| *magnitude)
        .fold(0.0, f64::max);
    samples.retain(|(magnitude, _)| *magnitude > DETAIL_SHARE * sampled_largest);
    let largest = map
        .values
        .iter()
        .fold(0.0, |most: f64, value| most.max(f64::from(value.abs())));
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
    DisplacementReport {
        lowest_mm: lowest * correction,
        highest_mm: highest * correction,
        percentiles_mm,
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
    let diameter = plinth
        .diameter_mm
        .unwrap_or_else(|| (PLINTH_WIDTH_FRACTION * size[0].max(size[1])).max(2.0 * reach));
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
}

struct ClosedPart {
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
    /// The input triangle each part triangle came from; `None` for the fan
    /// triangles that close an opening.
    sources: Vec<Option<usize>>,
    openings: Vec<Opening>,
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
        // Each cap triangle runs against its boundary edge, keeping orientation consistent.
        for (position, &a) in boundary.iter().enumerate() {
            let b = boundary[(position + 1) % boundary.len()];
            triangles.push([local[&b], local[&a], centre_index]);
            sources.push(None);
        }
        openings.push(Opening {
            mean_height: points.iter().map(|point| point[1]).sum::<f64>() / count,
            top_height: points.iter().map(|point| point[1]).fold(f64::MIN, f64::max),
        });
    }
    Ok(ClosedPart {
        vertices: part_vertices,
        triangles,
        sources,
        openings,
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
            indices[slot * 3 + corner] =
                *ids.entry((vertex, uv.map(f32::to_bits)))
                    .or_insert_with(|| {
                        built.add(
                            place(part.vertices[vertex as usize]),
                            vertex,
                            uv.map(f64::from),
                            1.0,
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
        let p95 = applied
            .percentiles_mm
            .map(|percentiles| percentiles.p95.max(-percentiles.p5))
            .expect("detail percentiles");
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
