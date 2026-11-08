//! Wall thickness of a closed solid, measured from points spread evenly over
//! its surface.
//!
//! From each point, rays travel into the material: one along the inward face
//! normal and a ring around it. A ray's reading is the distance to where it
//! leaves the solid, and a point's reading is the shortest accepted ray. The
//! ring finds the narrow way across walls whose two faces are not parallel,
//! such as a chamfer or a tapered rib.
//!
//! A wall's far side faces back against the point's inward direction. A ray
//! that leaves through a surface turned more than
//! [`MAX_EXIT_DEVIATION_DEGREES`] from that has not crossed a wall: it has
//! reached a neighbouring face across a nearby edge. Such rays are not
//! counted, whichever way within the ring they travel, and a point with no
//! accepted ray is left unmeasured.
//!
//! Points are sampled, so a feature narrower than the sample spacing can fall
//! between them. The report states the spacing and how much of the surface
//! was measured.

use std::collections::HashMap;
use std::f64::consts::SQRT_2;

use parry3d_f64::math::Vector;
use parry3d_f64::query::{Ray, RayCast};
use parry3d_f64::shape::{FeatureId, TriMesh};
use serde::Serialize;

use crate::print_check::{area, extent, root, unit_normal};
use crate::{GeometryError, MeshBounds};

/// Half-angle of the ring of rays around the inward normal.
const CONE_HALF_ANGLE_DEGREES: f64 = 25.0;
/// Rays in the ring around the inward normal.
const RING_RAYS: usize = 6;
/// A ray leaving through a surface turned further than this from facing the
/// point's inward direction is not counted.
const MAX_EXIT_DEVIATION_DEGREES: f64 = 60.0;
/// The sample spacing grows beyond the minimum wall, where that helps, to
/// keep the number of surface points at or below this.
const MAX_SAMPLES: usize = 200_000;
/// At most this many thin regions are listed, largest first.
const MAX_REGIONS: usize = 10;
/// Rays start this far inside the surface, as a fraction of the point's
/// coordinates along the inward normal, so that they cannot hit the face
/// they leave. Ample for double precision, and far below the rounding
/// tolerance, so the offset cannot pass through a wall that could read thin.
const RAY_START: f64 = 1e-9;
/// The spacing is never finer than this fraction of the part's largest
/// extent: single precision coordinates cannot resolve a wall below it.
const FINEST_SPACING: f64 = 1e-6;
/// Readings this far below the minimum wall, as a fraction of the point's
/// coordinates along the reading's direction, are coordinate rounding rather
/// than thin walls: two units of the single precision that STL coordinates
/// carry.
const THIN_TOLERANCE: f64 = 2.0 * f32::EPSILON as f64;

/// Wall thickness measured over a closed solid's surface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WallThicknessReport {
    /// The wall thickness the solid is checked against.
    pub minimum_wall_mm: f64,
    /// Half-angle of the ring of rays around each inward normal.
    pub cone_half_angle_degrees: f64,
    /// Rays cast from each point, including the one along the normal.
    pub rays_per_sample: usize,
    /// Rays leaving through a surface turned further than this from facing
    /// the point's inward direction are not counted.
    pub max_exit_deviation_degrees: f64,
    /// Every point of the surface lies within this distance of a sample.
    pub sample_spacing_mm: f64,
    pub samples: usize,
    /// Points where every ray left through a glancing surface.
    pub unmeasured_samples: usize,
    /// Share of the surface area whose points were measured.
    pub measured_area_share: f64,
    /// Area-weighted distribution of the measured readings. Absent when no
    /// point was measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thickness_mm: Option<ThicknessDistribution>,
    /// Surface area whose readings are below the minimum wall.
    pub thin_area_mm2: f64,
    /// Number of thin regions, including any not listed.
    pub thin_region_count: usize,
    /// The largest thin regions, largest first.
    pub thin_regions: Vec<ThinRegion>,
}

/// Area-weighted thickness readings in millimetres.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThicknessDistribution {
    pub minimum: f64,
    pub p5: f64,
    pub p50: f64,
    pub p95: f64,
    pub maximum: f64,
}

/// One cluster of nearby readings thinner than the minimum wall.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThinRegion {
    pub area_mm2: f64,
    pub minimum_thickness_mm: f64,
    /// The surface point with the thinnest reading.
    pub thinnest_point_mm: [f64; 3],
    /// Bounds of the region's thin surface points.
    pub bounds: MeshBounds,
}

struct Sample {
    point: [f64; 3],
    area: f64,
    thickness: Option<f64>,
    /// Size of the coordinates the reading was taken across, which sets how
    /// much rounding it can carry.
    rounding_scale: f64,
}

/// Measure the wall thickness of a closed, consistently outward-oriented
/// solid against `minimum_wall_mm`.
pub(crate) fn measure(
    vertices: &[[f64; 3]],
    triangles: &[[u32; 3]],
    minimum_wall_mm: f64,
) -> Result<WallThicknessReport, GeometryError> {
    let size = extent(vertices);
    let surface = TriMesh::new(
        vertices
            .iter()
            .map(|vertex| Vector::from_array(*vertex))
            .collect(),
        triangles.to_vec(),
    )
    .map_err(|error| GeometryError::Kernel(format!("the solid surface is invalid: {error}")))?;
    let normals: Vec<Option<[f64; 3]>> = triangles
        .iter()
        .map(|triangle| unit_normal(corners(vertices, triangle)))
        .collect();
    let spacing = sample_spacing(vertices, triangles, minimum_wall_mm);
    // Every ray from inside a closed solid leaves it within the diagonal of
    // its bounds.
    let ray_length = 2.0 * size * 3f64.sqrt();
    let exit_cosine = MAX_EXIT_DEVIATION_DEGREES.to_radians().cos();

    let mut samples = Vec::new();
    for (index, triangle) in triangles.iter().enumerate() {
        let Some(normal) = normals[index] else {
            continue;
        };
        for (point, area) in triangle_samples(corners(vertices, triangle), spacing) {
            let inward = normal.map(|value| -value);
            let start = RAY_START * scale_along(point, inward);
            let reading = cone(inward)
                .filter_map(|direction| {
                    let origin = Vector::from_array(point) + Vector::from_array(inward) * start;
                    let ray = Ray::new(origin, Vector::from_array(direction));
                    let hit = surface.cast_local_ray_and_get_normal(&ray, ray_length, false)?;
                    let FeatureId::Face(face) = hit.feature else {
                        return None;
                    };
                    // Faces seen from behind are numbered after the others.
                    let exit = normals[face as usize % triangles.len()]?;
                    (dot(exit, inward) >= exit_cosine)
                        .then_some((hit.time_of_impact + start, direction))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0));
            samples.push(Sample {
                point,
                area,
                thickness: reading.map(|(thickness, _)| thickness),
                rounding_scale: reading.map_or(1.0, |(thickness, direction)| {
                    scale_along(point, direction) + thickness
                }),
            });
        }
    }
    drop(surface);

    let total_area: f64 = samples.iter().map(|sample| sample.area).sum();
    let mut measured: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|sample| sample.thickness.map(|thickness| (thickness, sample.area)))
        .collect();
    measured.sort_by(|a, b| a.0.total_cmp(&b.0));
    let measured_area: f64 = measured.iter().map(|(_, area)| area).sum();
    let (thin_area_mm2, thin_region_count, thin_regions) =
        thin_regions(&samples, minimum_wall_mm, spacing);

    Ok(WallThicknessReport {
        minimum_wall_mm,
        cone_half_angle_degrees: CONE_HALF_ANGLE_DEGREES,
        rays_per_sample: RING_RAYS + 1,
        max_exit_deviation_degrees: MAX_EXIT_DEVIATION_DEGREES,
        sample_spacing_mm: spacing,
        samples: samples.len(),
        unmeasured_samples: samples.len() - measured.len(),
        measured_area_share: if total_area > 0.0 {
            measured_area / total_area
        } else {
            0.0
        },
        thickness_mm: distribution(&measured, measured_area),
        thin_area_mm2,
        thin_region_count,
        thin_regions,
    })
}

/// The spacing between surface points: the minimum wall, but no finer than
/// [`FINEST_SPACING`] of the part, widened when that would need more than
/// [`MAX_SAMPLES`] points. The floor keeps the count estimate finite, and
/// widening then reaches the budget in a step or two. Every triangle has at
/// least one point, so widening stops once it no longer reduces the count.
pub(crate) fn sample_spacing(
    vertices: &[[f64; 3]],
    triangles: &[[u32; 3]],
    minimum_wall_mm: f64,
) -> f64 {
    // Each triangle's length along its longest edge and height across it.
    let shapes: Vec<(f64, f64)> = triangles
        .iter()
        .map(|triangle| {
            let points = corners(vertices, triangle);
            let (longest, _) = longest_edge(points);
            let height = if longest > 0.0 {
                2.0 * area(points) / longest
            } else {
                0.0
            };
            (longest, height)
        })
        .collect();
    // The cells laid over every triangle, which bound the samples taken.
    let estimate = |spacing: f64| -> f64 {
        let cell = spacing / SQRT_2;
        shapes
            .iter()
            .map(|(longest, height)| {
                (longest / cell).ceil().max(1.0) * (height / cell).ceil().max(1.0)
            })
            .sum()
    };
    let budget = MAX_SAMPLES as f64;
    let mut spacing = minimum_wall_mm.max(FINEST_SPACING * extent(vertices));
    let mut count = estimate(spacing);
    for _ in 0..64 {
        if count <= budget {
            break;
        }
        let wider = spacing * (count / budget).sqrt().max(1.05);
        let wider_count = estimate(wider);
        if wider_count >= count {
            break;
        }
        (spacing, count) = (wider, wider_count);
    }
    spacing
}

/// The longest edge's length, and the corners rotated so that it runs from
/// the first to the second.
fn longest_edge([a, b, c]: [[f64; 3]; 3]) -> (f64, [[f64; 3]; 3]) {
    [(a, b, c), (b, c, a), (c, a, b)]
        .into_iter()
        .map(|(p, q, r)| (length(sub(q, p)), [p, q, r]))
        .max_by(|x, y| x.0.total_cmp(&y.0))
        .expect("a triangle has three edges")
}

/// Points spread over a triangle, each with the area it stands for. Square
/// cells whose diagonal is `spacing` are laid in the triangle's plane, along
/// its longest edge, and each is clipped to the triangle; a non-empty piece
/// gives its centroid and exact area. Every point of the triangle therefore
/// lies within `spacing` of a sample, and a sliver gets one row of cells
/// along its length.
pub(crate) fn triangle_samples(points: [[f64; 3]; 3], spacing: f64) -> Vec<([f64; 3], f64)> {
    let (longest, [p, q, r]) = longest_edge(points);
    if longest == 0.0 {
        return Vec::new();
    }
    let u = sub(q, p).map(|value| value / longest);
    let offset = sub(r, p);
    let along = dot(offset, u);
    let across = [0, 1, 2].map(|axis| offset[axis] - along * u[axis]);
    let height = length(across);
    if height == 0.0 {
        return Vec::new();
    }
    let v = across.map(|value| value / height);
    // The triangle in its own plane: base along u, apex above it. The apex
    // lies over the base because the base is the longest edge.
    let triangle = [[0.0, 0.0], [longest, 0.0], [along, height]];
    let cell = spacing / SQRT_2;
    let rows = (height / cell).ceil().max(1.0) as usize;
    let columns = (longest / cell).ceil().max(1.0) as usize;
    let mut samples = Vec::new();
    for row in 0..rows {
        let (low, high) = (row as f64 * cell, ((row + 1) as f64 * cell).min(height));
        // The triangle's span along u is widest at the bottom of the row.
        let left = low / height * along;
        let right = longest - low / height * (longest - along);
        let first = (left / cell).floor() as usize;
        let last = ((right / cell).ceil() as usize).min(columns);
        for column in first..last {
            let (start, end) = (
                column as f64 * cell,
                ((column + 1) as f64 * cell).min(longest),
            );
            let Some((centre, piece)) = clipped(triangle, [start, low], [end, high]) else {
                continue;
            };
            let point = [0, 1, 2].map(|axis| p[axis] + centre[0] * u[axis] + centre[1] * v[axis]);
            samples.push((point, piece));
        }
    }
    samples
}

/// The centroid and area of a triangle clipped to an axis-aligned rectangle,
/// or nothing when they do not overlap.
fn clipped(triangle: [[f64; 2]; 3], low: [f64; 2], high: [f64; 2]) -> Option<([f64; 2], f64)> {
    let mut polygon = triangle.to_vec();
    for axis in 0..2 {
        for (bound, keep_above) in [(low[axis], true), (high[axis], false)] {
            let inside = |point: &[f64; 2]| {
                if keep_above {
                    point[axis] >= bound
                } else {
                    point[axis] <= bound
                }
            };
            let mut kept = Vec::with_capacity(polygon.len() + 1);
            for (index, current) in polygon.iter().enumerate() {
                let next = polygon[(index + 1) % polygon.len()];
                if inside(current) {
                    kept.push(*current);
                }
                if inside(current) != inside(&next) {
                    let t = (bound - current[axis]) / (next[axis] - current[axis]);
                    kept.push([0, 1].map(|i| current[i] + t * (next[i] - current[i])));
                }
            }
            polygon = kept;
            if polygon.len() < 3 {
                return None;
            }
        }
    }
    // Shoelace area and centroid.
    let (mut doubled, mut x, mut y) = (0.0, 0.0, 0.0);
    for (index, a) in polygon.iter().enumerate() {
        let b = polygon[(index + 1) % polygon.len()];
        let cross = a[0] * b[1] - b[0] * a[1];
        doubled += cross;
        x += (a[0] + b[0]) * cross;
        y += (a[1] + b[1]) * cross;
    }
    if doubled <= 0.0 {
        return None;
    }
    Some(([x / (3.0 * doubled), y / (3.0 * doubled)], 0.5 * doubled))
}

/// The inward normal and a ring of directions around it.
fn cone(inward: [f64; 3]) -> impl Iterator<Item = [f64; 3]> {
    // Any axis not parallel to the normal gives a basis across it.
    let axis = if inward[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let u = normalize(cross(inward, axis));
    let v = cross(inward, u);
    let (sin, cos) = CONE_HALF_ANGLE_DEGREES.to_radians().sin_cos();
    std::iter::once(inward).chain((0..RING_RAYS).map(move |ray| {
        let angle = std::f64::consts::TAU * ray as f64 / RING_RAYS as f64;
        let (across_sin, across_cos) = angle.sin_cos();
        [0, 1, 2]
            .map(|axis| inward[axis] * cos + (u[axis] * across_cos + v[axis] * across_sin) * sin)
    }))
}

/// Thin area, region count and the largest regions. Thin readings are placed
/// on a grid of cells one sample spacing wide; readings in the same or
/// touching cells join one region. A region is therefore a cluster of thin
/// readings within about two spacings of each other: both faces of a thin
/// wall form one region, and patches parted by a wider band of thicker
/// surface stay apart. The work grows linearly with the thin readings.
fn thin_regions(
    samples: &[Sample],
    minimum_wall_mm: f64,
    spacing: f64,
) -> (f64, usize, Vec<ThinRegion>) {
    let thin: Vec<&Sample> = samples
        .iter()
        .filter(|sample| {
            sample.thickness.is_some_and(|value| {
                value < minimum_wall_mm - THIN_TOLERANCE * sample.rounding_scale
            })
        })
        .collect();
    let cell_of = |point: [f64; 3]| point.map(|value| (value / spacing).floor() as i64);
    // One representative reading per occupied cell; the others join it.
    let mut parent: Vec<usize> = (0..thin.len()).collect();
    let mut cells: HashMap<[i64; 3], usize> = HashMap::new();
    for (index, sample) in thin.iter().enumerate() {
        let first = *cells.entry(cell_of(sample.point)).or_insert(index);
        let (a, b) = (root(&mut parent, index), root(&mut parent, first));
        parent[a] = b;
    }
    for (&[x, y, z], &first) in &cells {
        for neighbour in [-1, 0, 1].into_iter().flat_map(|dx| {
            [-1, 0, 1]
                .into_iter()
                .flat_map(move |dy| [-1, 0, 1].map(move |dz| [x + dx, y + dy, z + dz]))
        }) {
            if let Some(&other) = cells.get(&neighbour) {
                let (a, b) = (root(&mut parent, first), root(&mut parent, other));
                parent[a] = b;
            }
        }
    }

    let mut region_of: HashMap<usize, usize> = HashMap::new();
    let mut regions: Vec<ThinRegion> = Vec::new();
    let mut thin_area = 0.0;
    for (index, sample) in thin.iter().enumerate() {
        let thickness = sample.thickness.expect("thin samples are measured");
        thin_area += sample.area;
        let group = root(&mut parent, index);
        let slot = *region_of.entry(group).or_insert_with(|| {
            regions.push(ThinRegion {
                area_mm2: 0.0,
                minimum_thickness_mm: thickness,
                thinnest_point_mm: sample.point,
                bounds: MeshBounds {
                    minimum_mm: sample.point,
                    maximum_mm: sample.point,
                    dimensions_mm: [0.0; 3],
                },
            });
            regions.len() - 1
        });
        let region = &mut regions[slot];
        region.area_mm2 += sample.area;
        if thickness < region.minimum_thickness_mm {
            region.minimum_thickness_mm = thickness;
            region.thinnest_point_mm = sample.point;
        }
        for (axis, value) in sample.point.into_iter().enumerate() {
            region.bounds.minimum_mm[axis] = region.bounds.minimum_mm[axis].min(value);
            region.bounds.maximum_mm[axis] = region.bounds.maximum_mm[axis].max(value);
        }
    }
    let count = regions.len();
    regions.sort_by(|a, b| b.area_mm2.total_cmp(&a.area_mm2));
    regions.truncate(MAX_REGIONS);
    for region in &mut regions {
        region.bounds.dimensions_mm =
            [0, 1, 2].map(|axis| region.bounds.maximum_mm[axis] - region.bounds.minimum_mm[axis]);
    }
    (thin_area, count, regions)
}

/// Area-weighted percentiles of readings sorted from thinnest.
fn distribution(sorted: &[(f64, f64)], total: f64) -> Option<ThicknessDistribution> {
    let (first, last) = (sorted.first()?.0, sorted.last()?.0);
    let percentile = |share: f64| {
        let target = share * total;
        let mut running = 0.0;
        for &(thickness, area) in sorted {
            running += area;
            if running >= target {
                return thickness;
            }
        }
        last
    };
    Some(ThicknessDistribution {
        minimum: first,
        p5: percentile(0.05),
        p50: percentile(0.5),
        p95: percentile(0.95),
        maximum: last,
    })
}

/// How large a point's coordinates are along a unit direction, and at least
/// one millimetre: the coordinates a reading in that direction depends on.
fn scale_along(point: [f64; 3], direction: [f64; 3]) -> f64 {
    1.0 + (0..3)
        .map(|axis| (point[axis] * direction[axis]).abs())
        .sum::<f64>()
}

fn corners(vertices: &[[f64; 3]], triangle: &[u32; 3]) -> [[f64; 3]; 3] {
    triangle.map(|index| vertices[index as usize])
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let size = length(a);
    a.map(|value| value / size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CriterionStatus, TriangleMesh, ValidationOptions, analyze_mesh};

    /// A closed box, wound outward, or inward for a cavity.
    fn cuboid(low: [f64; 3], high: [f64; 3], cavity: bool) -> TriangleMesh {
        let [a, b] = [low, high];
        let vertices = vec![
            [a[0], a[1], a[2]],
            [b[0], a[1], a[2]],
            [b[0], b[1], a[2]],
            [a[0], b[1], a[2]],
            [a[0], a[1], b[2]],
            [b[0], a[1], b[2]],
            [b[0], b[1], b[2]],
            [a[0], b[1], b[2]],
        ];
        let mut triangles = vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ];
        if cavity {
            for triangle in &mut triangles {
                triangle.swap(1, 2);
            }
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    fn join(mut first: TriangleMesh, second: TriangleMesh) -> TriangleMesh {
        let offset = first.vertices.len() as u32;
        first.vertices.extend(second.vertices);
        first.triangles.extend(
            second
                .triangles
                .into_iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        first
    }

    fn walls(mesh: &TriangleMesh, minimum_wall_mm: f64) -> crate::ValidationReport {
        analyze_mesh(
            mesh,
            ValidationOptions {
                minimum_wall_mm: Some(minimum_wall_mm),
                ..ValidationOptions::default()
            },
        )
        .unwrap()
    }

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn a_slab_thinner_than_the_minimum_is_thin_on_both_faces() {
        let report = walls(&cuboid([0.0; 3], [20.0, 20.0, 1.0], false), 1.5);
        let thickness = report.wall_thickness.as_ref().unwrap();
        // The top and bottom read the slab's thickness; its narrow sides read
        // across its width.
        assert!(close(thickness.thin_area_mm2, 800.0), "{thickness:?}");
        // Both faces of the one thin wall form one region.
        assert_eq!(thickness.thin_region_count, 1);
        assert!(close(thickness.thin_regions[0].area_mm2, 800.0));
        assert!(close(thickness.thin_regions[0].minimum_thickness_mm, 1.0));
        let distribution = thickness.thickness_mm.as_ref().unwrap();
        assert!(close(distribution.minimum, 1.0));
        assert!(close(distribution.maximum, 20.0));
        assert_eq!(thickness.unmeasured_samples, 0);
        assert!(close(thickness.measured_area_share, 1.0));
        assert_eq!(
            report.assessment.criteria["wall_thickness"].status,
            CriterionStatus::Failed
        );
        assert_eq!(
            report.assessment.criteria["wall_thickness"].evidence,
            Some("/wall_thickness")
        );
        assert_eq!(report.assessment.status, crate::AssessmentStatus::Failed);
        assert!(report.issues.iter().any(|issue| issue.code == "thin_walls"));
    }

    #[test]
    fn a_wall_exactly_at_the_minimum_passes() {
        let report = walls(&cuboid([0.0; 3], [20.0, 20.0, 2.0], false), 2.0);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert_eq!(thickness.thin_area_mm2, 0.0);
        assert!(thickness.thin_regions.is_empty());
        assert_eq!(
            report.assessment.criteria["wall_thickness"].status,
            CriterionStatus::Passed
        );
        assert_eq!(
            report.assessment.status,
            crate::AssessmentStatus::Incomplete
        );
    }

    #[test]
    fn rays_escaping_through_square_edges_do_not_read_thin() {
        // Points close to every edge send ring rays out through the
        // neighbouring face after a short distance.
        let report = walls(&cuboid([0.0; 3], [10.0, 10.0, 10.0], false), 3.0);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert_eq!(thickness.thin_area_mm2, 0.0);
        let distribution = thickness.thickness_mm.as_ref().unwrap();
        assert!(close(distribution.minimum, 10.0));
        assert!(close(distribution.maximum, 10.0));
    }

    #[test]
    fn edges_sharper_than_square_do_not_read_thin() {
        // A thick prism whose trapezoidal section has 73 degree edges along
        // its base. Rays tilted towards such an edge reach the slanted side
        // soon after leaving the base, but that side is a neighbour, not the
        // far side of a wall.
        let run = 10.0 / 73.0_f64.to_radians().tan();
        let section = [[0.0, 0.0], [20.0, 0.0], [20.0 - run, 10.0], [run, 10.0]];
        let mut vertices = Vec::new();
        for y in [0.0, 20.0] {
            for [x, z] in section {
                vertices.push([x, y, z]);
            }
        }
        let mesh = TriangleMesh {
            vertices,
            triangles: vec![
                [0, 1, 2],
                [0, 2, 3],
                [4, 6, 5],
                [4, 7, 6],
                [0, 5, 1],
                [0, 4, 5],
                [1, 6, 2],
                [1, 5, 6],
                [2, 7, 3],
                [2, 6, 7],
                [3, 4, 0],
                [3, 7, 4],
            ],
        };
        let report = walls(&mesh, 3.0);
        assert!(report.solid_geometry);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert_eq!(thickness.thin_area_mm2, 0.0, "{thickness:?}");
    }

    #[test]
    fn a_hollow_box_reads_its_wall_from_outside_and_inside() {
        let mesh = join(
            cuboid([0.0; 3], [20.0; 3], false),
            cuboid([2.0; 3], [18.0; 3], true),
        );
        let thin = walls(&mesh, 2.5);
        let thickness = thin.wall_thickness.as_ref().unwrap();
        // Every face of the cavity, and the outside opposite it, reads the
        // wall. The outside's rim over each side wall reads down that wall's
        // height instead; the side wall's own faces show it is thin. Samples
        // near the rim's edge stand for area on both sides of it.
        assert!(thickness.thin_area_mm2 > 0.9 * 12.0 * 256.0);
        assert!(thickness.thin_area_mm2 < 6.0 * 400.0 + 6.0 * 256.0);
        assert!(close(thickness.thickness_mm.as_ref().unwrap().minimum, 2.0));
        let thick = walls(&mesh, 1.5);
        assert_eq!(thick.wall_thickness.as_ref().unwrap().thin_area_mm2, 0.0);
    }

    #[test]
    fn overlapping_parts_are_measured_as_their_union() {
        // A post whose top is buried in a thin shelf. The buried face is not
        // a wall, so the shelf above the post is not read as thin.
        let mesh = join(
            cuboid([8.0, 8.0, 0.0], [12.0, 12.0, 10.0], false),
            cuboid([0.0, 0.0, 9.0], [20.0, 20.0, 10.5], false),
        );
        let report = walls(&mesh, 2.0);
        let thickness = report.wall_thickness.as_ref().unwrap();
        let distribution = thickness.thickness_mm.as_ref().unwrap();
        assert!(close(distribution.minimum, 1.5), "{distribution:?}");
        // The shelf's faces away from the post are thin; the top above the
        // post reads down through it.
        assert!(thickness.thin_area_mm2 > 2.0 * (400.0 - 36.0));
        assert!(thickness.thin_area_mm2 < 2.0 * (400.0 - 4.0));
    }

    #[test]
    fn a_solid_whose_rays_all_glance_off_is_unmeasured_not_passed() {
        // Every exit from a regular tetrahedron's face is a neighbouring face
        // turned about 70.5 degrees from facing back, so no point is measured.
        let mesh = TriangleMesh {
            vertices: vec![
                [10.0, 10.0, 10.0],
                [10.0, -10.0, -10.0],
                [-10.0, 10.0, -10.0],
                [-10.0, -10.0, 10.0],
            ],
            triangles: vec![[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]],
        };
        let report = walls(&mesh, 100.0);
        assert!(report.solid_geometry);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert_eq!(thickness.unmeasured_samples, thickness.samples);
        assert_eq!(thickness.measured_area_share, 0.0);
        assert!(thickness.thickness_mm.is_none());
        let criterion = &report.assessment.criteria["wall_thickness"];
        assert_eq!(criterion.status, CriterionStatus::Unmeasured);
        assert_eq!(criterion.evidence, Some("/wall_thickness"));
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "wall_thickness_unmeasured")
        );
    }

    #[test]
    fn an_open_surface_is_not_measured() {
        let mut mesh = cuboid([0.0; 3], [10.0; 3], false);
        mesh.triangles.truncate(10);
        let report = walls(&mesh, 1.0);
        assert!(report.wall_thickness.is_none());
        assert_eq!(
            report.assessment.criteria["wall_thickness"].status,
            CriterionStatus::Unmeasured
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "wall_thickness_unmeasured")
        );
    }

    #[test]
    fn the_minimum_wall_must_be_positive_and_finite() {
        let mesh = cuboid([0.0; 3], [10.0; 3], false);
        for wall in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let error = analyze_mesh(
                &mesh,
                ValidationOptions {
                    minimum_wall_mm: Some(wall),
                    ..ValidationOptions::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.code(), "validation");
        }
    }

    #[test]
    fn distant_parts_do_not_hide_thin_walls() {
        // Rounding is judged where each reading is taken, so a part 200 m
        // away neither widens the tolerance near the origin nor hides a
        // 0.1 mm shortfall of its own.
        let mesh = join(
            cuboid([0.0; 3], [20.0, 20.0, 1.0], false),
            cuboid([200_000.0, 0.0, 0.0], [200_020.0, 20.0, 1.0], false),
        );
        let report = walls(&mesh, 1.1);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(
            (thickness.thin_area_mm2 - 1600.0).abs() < 1e-3,
            "{thickness:?}"
        );
        assert_eq!(
            report.assessment.criteria["wall_thickness"].status,
            CriterionStatus::Failed
        );
    }

    #[test]
    fn a_wall_far_along_its_own_plane_keeps_its_tolerance() {
        // Coordinates along the wall do not round its thickness.
        let mesh = cuboid([1_000_000.0, 0.0, 0.0], [1_000_020.0, 20.0, 1.0], false);
        let report = walls(&mesh, 1.1);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(close(thickness.thin_area_mm2, 800.0), "{thickness:?}");
    }

    #[test]
    fn thin_patches_parted_by_thick_material_are_separate_regions() {
        // A 1 mm plate thickened to 10 mm across its middle, so its two thin
        // ends lie 10 mm apart beneath shared faces.
        let mesh = join(
            cuboid([0.0; 3], [40.0, 20.0, 1.0], false),
            cuboid([15.0, 0.0, 0.0], [25.0, 20.0, 10.0], false),
        );
        let report = walls(&mesh, 2.0);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert_eq!(thickness.thin_region_count, 2, "{thickness:?}");
        // Cells straddling the block's edge give a little area to the ends.
        for region in &thickness.thin_regions {
            assert!((region.area_mm2 - 600.0).abs() < 12.0, "{region:?}");
            let bounds = &region.bounds;
            assert!(
                bounds.maximum_mm[0] < 16.0 || bounds.minimum_mm[0] > 24.0,
                "{region:?}"
            );
        }
    }

    #[test]
    fn a_hair_thin_wall_far_from_the_origin_is_still_measured() {
        // The ray start offset must stay inside a 0.0625 mm wall at z = 1 km.
        let mesh = cuboid(
            [0.0, 0.0, 1_000_000.0],
            [20.0, 20.0, 1_000_000.062_5],
            false,
        );
        let report = walls(&mesh, 1.0);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(close(thickness.thin_area_mm2, 800.0), "{thickness:?}");
    }

    #[test]
    fn an_absurdly_small_minimum_gives_a_finite_report() {
        let report = walls(&cuboid([0.0; 3], [10.0; 3], false), 1e-200);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(thickness.sample_spacing_mm.is_finite());
        assert!(thickness.samples > 0 && thickness.samples <= MAX_SAMPLES);
        assert_eq!(thickness.thin_area_mm2, 0.0);
    }

    #[test]
    fn a_needle_stays_within_the_sample_budget() {
        let report = walls(&cuboid([0.0; 3], [1000.0, 0.001, 0.001], false), 0.1);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(thickness.samples <= MAX_SAMPLES, "{}", thickness.samples);
        assert!(thickness.thin_area_mm2 > 0.0);
    }

    #[test]
    fn a_vanishing_minimum_still_gives_a_bounded_report() {
        let report = walls(&cuboid([0.0; 3], [10.0; 3], false), 1e-100);
        let thickness = report.wall_thickness.as_ref().unwrap();
        assert!(thickness.samples <= MAX_SAMPLES, "{}", thickness.samples);
        assert!(thickness.sample_spacing_mm > 1e-3);
        assert_eq!(thickness.thin_area_mm2, 0.0);
    }

    #[test]
    fn samples_cover_each_triangle_at_the_spacing() {
        // A large triangle and a long sliver of the kind tessellated
        // cylinders produce.
        for (points, spacing) in [
            ([[0.0, 0.0, 0.0], [30.0, 0.0, 0.0], [10.0, 20.0, 0.0]], 1.0),
            ([[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [40.0, 0.01, 0.0]], 0.5),
            ([[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [20.0, 0.01, 0.0]], 0.5),
        ] {
            let samples = triangle_samples(points, spacing);
            let total: f64 = samples.iter().map(|(_, area)| area).sum();
            assert!(close(total, area(points)));
            // Every sample lies on the triangle, and each part of the long
            // edge has a sample within the spacing.
            for (point, _) in &samples {
                assert!(close(point[2], 0.0));
            }
            for step in 0..=80 {
                let x = 40.0_f64.min(points[1][0]) * step as f64 / 80.0;
                let nearest = samples
                    .iter()
                    .map(|(point, _)| (point[0] - x).abs())
                    .fold(f64::INFINITY, f64::min);
                assert!(nearest <= spacing, "no sample near x = {x}");
            }
        }
    }

    #[test]
    fn spacing_widens_to_keep_large_parts_bounded() {
        let mesh = cuboid([0.0; 3], [1000.0; 3], false);
        let spacing = sample_spacing(&mesh.vertices, &mesh.triangles, 0.1);
        assert!(spacing > 0.1);
        let samples: usize = mesh
            .triangles
            .iter()
            .map(|triangle| triangle_samples(corners(&mesh.vertices, triangle), spacing).len())
            .sum();
        assert!(samples <= MAX_SAMPLES, "{samples}");
        assert!(samples >= MAX_SAMPLES / 8, "{samples}");
        assert_eq!(
            sample_spacing(&mesh.vertices, &mesh.triangles, 100.0),
            100.0
        );
    }
}
