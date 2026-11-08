//! Signed distance of a mesh's surface from a reference mesh, after an
//! optional rigid best fit.
//!
//! Points spread over the compared mesh's surface are measured against the
//! nearest reference surface that faces the same way, so a point is never
//! measured against the far side of a thin wall. The sign comes from the
//! pseudonormal of the face, edge or vertex the nearest point lies on:
//! positive outside the reference, where the compared mesh has extra
//! material, and negative inside it, where material is missing.
//!
//! The best fit is rigid, rotation and translation without scale, so a size
//! error shows as deviation rather than being absorbed. Point-to-plane
//! iterative closest point refines each starting pose: the meshes as given,
//! and every proper rotation that maps one mesh's principal axes onto the
//! other's. The pose whose capped distances are smallest over every fitting
//! point is refined further and kept.

use std::collections::HashMap;

use nalgebra::{Matrix3, Matrix6, Rotation3, Vector3, Vector6};
use parry3d_f64::math::Vector;
use parry3d_f64::query::{PointQuery, PointQueryWithLocation, Ray, RayCast};
use parry3d_f64::shape::{FeatureId, TriMesh, TriMeshFlags, TrianglePointLocation};
use serde::Serialize;

use crate::print_check::root;
use crate::thickness::{sample_spacing, triangle_samples};
use crate::{GeometryError, MeshBounds, TriangleMesh};

/// At most this many regions are listed, largest first.
const MAX_REGIONS: usize = 10;
/// A reading along a point's normal steps past at most this many reference
/// surfaces that face the other way.
const MAX_RAY_SURFACES: usize = 8;
/// Points used to fit the alignment and to score each starting pose,
/// spread evenly over the surface samples.
const FIT_POINTS: usize = 4_000;
/// Points used to refine each starting pose before it is scored.
const START_POINTS: usize = 1_000;
const START_ITERATIONS: usize = 8;
const FIT_ITERATIONS: usize = 60;
/// When scoring a pose, distances are capped at this share of the
/// reference's diagonal, so points far from any match cannot dominate.
const SCORE_CAP_SHARE: f64 = 0.05;
/// A fitting step leaves out pairs farther apart than this many times the
/// median distance.
const OUTLIER_MEDIANS: f64 = 3.0;
/// A fitting pair's normals must agree to within this angle.
const PAIR_NORMAL_DEGREES: f64 = 60.0;
/// Without `max_distance_mm`, points farther than this share of the
/// reference's diagonal from any surface facing their way are unmeasured.
const DEFAULT_REACH_SHARE: f64 = 0.1;
/// Points within this fraction of their own coordinates' size, or of one
/// millimetre, lie on a surface.
const ON_SURFACE: f64 = 1e-9;
/// A barycentric weight this small puts a point on the opposite edge.
const EDGE_WEIGHT: f64 = 1e-9;

type V3 = Vector3<f64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    /// The meshes share a frame.
    None,
    /// A rigid best fit of the compared mesh onto the reference.
    BestFit,
}

/// Compare the validated mesh with `reference`.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonOptions {
    pub reference: TriangleMesh,
    pub alignment: Alignment,
    /// Deviation either side that counts as matching.
    pub tolerance_mm: f64,
    /// Points farther than this from any reference surface facing their way
    /// are unmeasured.
    pub max_distance_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComparisonReport {
    pub alignment: AlignmentReport,
    pub tolerance_mm: f64,
    pub max_distance_mm: f64,
    pub sample_spacing_mm: f64,
    pub samples: usize,
    pub unmeasured_samples: usize,
    pub measured_area_share: f64,
    /// Area-weighted deviation of the measured points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deviation_mm: Option<DeviationDistribution>,
    /// Share of the measured area within the tolerance either side.
    pub within_tolerance_share: f64,
    pub region_count: usize,
    /// The largest regions beyond the tolerance, largest first.
    pub regions: Vec<DeviationRegion>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlignmentReport {
    pub method: Alignment,
    /// Rotation rows and translation that carry the compared mesh into the
    /// reference's frame.
    pub rotation: [[f64; 3]; 3],
    pub translation_mm: [f64; 3],
    pub rotation_degrees: f64,
    /// For a best fit, the points fitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit_points: Option<usize>,
    /// For a best fit, the points matched in the final step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_points: Option<usize>,
    /// For a best fit, the point-to-plane RMS of the final step's pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rms_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviationDistribution {
    pub minimum: f64,
    pub p5: f64,
    pub p50: f64,
    pub p95: f64,
    pub maximum: f64,
    pub mean: f64,
    pub rms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviationRegion {
    pub area_mm2: f64,
    /// The region's largest deviation: positive outside the reference,
    /// negative inside it.
    pub extreme_mm: f64,
    /// Where the extreme lies, in the reference's frame.
    pub extreme_point_mm: [f64; 3],
    /// Area-weighted centre, in the reference's frame.
    pub centre_mm: [f64; 3],
    pub bounds: MeshBounds,
}

/// Reject options that cannot produce a trustworthy report.
pub(crate) fn validate(options: &ComparisonOptions) -> Result<(), GeometryError> {
    let invalid = |message: &str| Err(GeometryError::InvalidOptions(message.to_string()));
    if !options.tolerance_mm.is_finite() || options.tolerance_mm <= 0.0 {
        return invalid("comparison tolerance_mm must be a finite positive number");
    }
    if options
        .max_distance_mm
        .is_some_and(|reach| !reach.is_finite() || reach <= 0.0)
    {
        return invalid("comparison max_distance_mm must be a finite positive number");
    }
    let reference = &options.reference;
    if reference.vertices.is_empty() || reference.triangles.is_empty() {
        return invalid("the comparison reference must contain triangles");
    }
    if reference
        .vertices
        .iter()
        .any(|vertex| vertex.iter().any(|value| !value.is_finite()))
    {
        return invalid("the comparison reference contains a non-finite coordinate");
    }
    if reference
        .triangles
        .iter()
        .flatten()
        .any(|index| *index as usize >= reference.vertices.len())
    {
        return invalid("the comparison reference refers to a missing vertex");
    }
    Ok(())
}

/// A rigid motion: rotate, then translate.
#[derive(Debug, Clone, Copy)]
struct Pose {
    rotation: Matrix3<f64>,
    translation: V3,
}

impl Pose {
    fn identity() -> Self {
        Self {
            rotation: Matrix3::identity(),
            translation: V3::zeros(),
        }
    }

    fn apply(&self, point: &V3) -> V3 {
        self.rotation * point + self.translation
    }
}

/// A point on the compared surface, with the area it stands for and its
/// face's normal.
struct SurfacePoint {
    position: V3,
    normal: V3,
    area: f64,
}

/// The reference surface with outward pseudonormals at its vertices and
/// edges.
struct Reference {
    surface: TriMesh,
    face_normals: Vec<Option<V3>>,
}

impl Reference {
    fn new(mesh: &TriangleMesh) -> Result<Self, GeometryError> {
        let vertices = mesh
            .vertices
            .iter()
            .map(|vertex| Vector::from_array(*vertex))
            .collect();
        let surface = TriMesh::with_flags(vertices, mesh.triangles.clone(), TriMeshFlags::ORIENTED)
            .map_err(|_| {
                GeometryError::InvalidOptions(
                    "the comparison reference has no usable triangles".to_string(),
                )
            })?;
        let face_normals = surface
            .triangles()
            .map(|triangle| triangle.normal().map(from_parry))
            .collect();
        Ok(Self {
            surface,
            face_normals,
        })
    }

    /// The nearest point within `reach`, the outward normal there, and
    /// whether `point` lies inside. The normal is the face's, or the
    /// pseudonormal of the edge or vertex the nearest point lies on.
    fn nearest(&self, point: &V3, reach: f64) -> Option<(V3, V3, bool)> {
        let (projection, (part, location)) = self
            .surface
            .project_local_point_and_get_location_with_max_dist(to_parry(point), false, reach)?;
        let normal = match on_edge_or_vertex(location) {
            TrianglePointLocation::OnFace(..) | TrianglePointLocation::OnSolid => {
                self.face_normals.get(part as usize).copied().flatten()?
            }
            TrianglePointLocation::OnEdge(edge, _) => {
                let normals = self.surface.pseudo_normals_if_oriented()?;
                from_parry(normals.edges_pseudo_normal.get(part as usize)?[edge as usize])
            }
            TrianglePointLocation::OnVertex(corner) => {
                let normals = self.surface.pseudo_normals_if_oriented()?;
                let vertex = self.surface.indices()[part as usize][corner as usize];
                from_parry(*normals.vertices_pseudo_normal.get(vertex as usize)?)
            }
        };
        let nearest = from_parry(projection.point);
        let normal = unit(normal)?;
        Some((nearest, normal, (point - nearest).dot(&normal) < 0.0))
    }

    /// Signed distance from the nearest reference surface that faces the
    /// way `normal` does. When the nearest surface faces the other way, as
    /// across a thin wall, the reading looks along the point's own normal.
    /// A point on the surface reads zero, whichever face of an edge its
    /// nearest point is credited to.
    fn reading(&self, point: &V3, normal: &V3, reach: f64) -> Option<f64> {
        if let Some((nearest, surface_normal, inside)) = self.nearest(point, reach) {
            let distance = (point - nearest).norm();
            if distance <= rounding(point) || surface_normal.dot(normal) > 0.0 {
                return Some(if inside { -distance } else { distance });
            }
        }
        [1.0, -1.0]
            .into_iter()
            .filter_map(|direction: f64| self.along(point, &(normal * direction), normal, reach))
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
    }

    /// Signed distance from `point` to the nearest point of the first
    /// reference face along `ray_direction` that faces the way `normal`
    /// does, stepping past faces that face the other way. The distance runs
    /// to the face itself, edges included, and is negative on the face's
    /// inner side. The ray may run up to twice `reach`, so a tilted normal
    /// still finds a face within `reach` of the point.
    fn along(&self, point: &V3, ray_direction: &V3, normal: &V3, reach: f64) -> Option<f64> {
        let limit = 2.0 * reach;
        let (mut origin, mut travelled) = (*point, 0.0);
        for _ in 0..MAX_RAY_SURFACES {
            let ray = Ray::new(to_parry(&origin), to_parry(ray_direction));
            let hit = self
                .surface
                .cast_local_ray_and_get_normal(&ray, limit - travelled, false)?;
            let FeatureId::Face(face) = hit.feature else {
                return None;
            };
            let at = origin + ray_direction * hit.time_of_impact;
            // Faces seen from behind are numbered after the others.
            let index = face as usize % self.face_normals.len();
            let face_normal = self.face_normals[index]?;
            if face_normal.dot(normal) > 0.0 {
                let nearest = from_parry(
                    self.surface
                        .triangle(index as u32)
                        .project_local_point(to_parry(point), false)
                        .point,
                );
                let offset = point - nearest;
                let distance = offset.norm();
                return (distance <= reach).then(|| {
                    if offset.dot(&face_normal) < 0.0 {
                        -distance
                    } else {
                        distance
                    }
                });
            }
            // Step just past the face, by about its coordinates' rounding.
            let step = 10.0 * rounding(&at);
            origin = at + ray_direction * step;
            travelled += hit.time_of_impact + step;
            if travelled >= limit {
                return None;
            }
        }
        None
    }
}

/// The rounding a point's coordinates can carry: points this close to the
/// surface lie on it.
fn rounding(point: &V3) -> f64 {
    ON_SURFACE * point.amax().max(1.0)
}

/// A nearest point on a triangle's edge or corner can be reported as on its
/// face with a barycentric weight of zero, or within rounding of it. It
/// belongs to the edge or vertex, whose pseudonormal faces between the
/// faces that meet there.
fn on_edge_or_vertex(location: TrianglePointLocation) -> TrianglePointLocation {
    let TrianglePointLocation::OnFace(_, weights) = location else {
        return location;
    };
    match weights.map(|weight| weight <= EDGE_WEIGHT) {
        [false, true, true] => TrianglePointLocation::OnVertex(0),
        [true, false, true] => TrianglePointLocation::OnVertex(1),
        [true, true, false] => TrianglePointLocation::OnVertex(2),
        [false, false, true] => TrianglePointLocation::OnEdge(0, [weights[0], weights[1]]),
        [true, false, false] => TrianglePointLocation::OnEdge(1, [weights[1], weights[2]]),
        [false, true, false] => TrianglePointLocation::OnEdge(2, [weights[2], weights[0]]),
        _ => location,
    }
}

/// Compare `mesh` with the reference in `options`.
pub(crate) fn measure(
    mesh: &TriangleMesh,
    options: &ComparisonOptions,
) -> Result<ComparisonReport, GeometryError> {
    let reference = Reference::new(&options.reference)?;
    let diagonal = diagonal(&options.reference.vertices);
    let reach = options
        .max_distance_mm
        .unwrap_or(DEFAULT_REACH_SHARE * diagonal);
    let spacing = sample_spacing(&mesh.vertices, &mesh.triangles, options.tolerance_mm);
    let mut points = Vec::new();
    for triangle in &mesh.triangles {
        let corners = triangle.map(|index| mesh.vertices[index as usize]);
        let [a, b, c] = corners.map(V3::from);
        let Some(normal) = unit((b - a).cross(&(c - a))) else {
            continue;
        };
        for (position, area) in triangle_samples(corners, spacing) {
            points.push(SurfacePoint {
                position: V3::from(position),
                normal,
                area,
            });
        }
    }

    let (pose, alignment) = match options.alignment {
        Alignment::None => (
            Pose::identity(),
            alignment_report(Alignment::None, &Pose::identity()),
        ),
        Alignment::BestFit => best_fit(mesh, &options.reference, &reference, &points, diagonal),
    };

    let readings: Vec<(V3, f64, Option<f64>)> = points
        .iter()
        .map(|point| {
            let position = pose.apply(&point.position);
            let normal = pose.rotation * point.normal;
            (
                position,
                point.area,
                reference.reading(&position, &normal, reach),
            )
        })
        .collect();
    let total_area: f64 = readings.iter().map(|(_, area, _)| area).sum();
    let mut measured: Vec<(f64, f64)> = readings
        .iter()
        .filter_map(|(_, area, reading)| reading.map(|value| (value, *area)))
        .collect();
    measured.sort_by(|a, b| a.0.total_cmp(&b.0));
    let measured_area: f64 = measured.iter().map(|(_, area)| area).sum();
    let within: f64 = measured
        .iter()
        .filter(|(value, _)| value.abs() <= options.tolerance_mm)
        .map(|(_, area)| area)
        .sum();
    let (region_count, regions) = regions(&readings, options.tolerance_mm, spacing);
    Ok(ComparisonReport {
        alignment,
        tolerance_mm: options.tolerance_mm,
        max_distance_mm: reach,
        sample_spacing_mm: spacing,
        samples: readings.len(),
        unmeasured_samples: readings
            .iter()
            .filter(|(_, _, reading)| reading.is_none())
            .count(),
        measured_area_share: share(measured_area, total_area),
        deviation_mm: distribution(&measured, measured_area),
        within_tolerance_share: share(within, measured_area),
        region_count,
        regions,
    })
}

/// Fit the compared mesh onto the reference from every starting pose and
/// keep the best.
fn best_fit(
    mesh: &TriangleMesh,
    reference_mesh: &TriangleMesh,
    reference: &Reference,
    points: &[SurfacePoint],
    diagonal: f64,
) -> (Pose, AlignmentReport) {
    let fit = spread(points, FIT_POINTS);
    let start = spread(points, START_POINTS);
    let cap = SCORE_CAP_SHARE * diagonal;
    let mut starts = vec![Pose::identity()];
    if let (Some((compared_centre, compared_axes)), Some((reference_centre, reference_axes))) =
        (principal_axes(mesh), principal_axes(reference_mesh))
    {
        for turn in axis_rotations() {
            let rotation = reference_axes * turn * compared_axes.transpose();
            starts.push(Pose {
                rotation,
                translation: reference_centre - rotation * compared_centre,
            });
        }
    }
    let score = |pose: &Pose| -> f64 {
        fit.iter()
            .map(|point| {
                let position = pose.apply(&point.position);
                let distance = reference
                    .surface
                    .project_local_point_and_get_location_with_max_dist(
                        to_parry(&position),
                        false,
                        cap,
                    )
                    .map_or(cap, |(projection, _)| {
                        (position - from_parry(projection.point)).norm()
                    });
                point.area * distance.min(cap).powi(2)
            })
            .sum()
    };
    let best = starts
        .into_iter()
        .map(|pose| {
            let (pose, ..) = refine(reference, &start, pose, START_ITERATIONS, cap, diagonal);
            (score(&pose), pose)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Pose::identity(), |(_, pose)| pose);
    let (pose, matched, rms) = refine(reference, &fit, best, FIT_ITERATIONS, cap, diagonal);
    let mut report = alignment_report(Alignment::BestFit, &pose);
    report.fit_points = Some(fit.len());
    report.matched_points = Some(matched);
    report.rms_mm = Some(rms);
    (pose, report)
}

/// Point-to-plane iterative closest point from `pose`. Each step pairs the
/// points with the nearest reference surface facing their way, leaves out
/// pairs far beyond the median distance, and solves the linearised motion
/// about the paired points' centroid so that parts far from the origin stay
/// well conditioned. Returns the pose, the pairs in the last step, and their
/// point-to-plane RMS.
fn refine(
    reference: &Reference,
    points: &[&SurfacePoint],
    mut pose: Pose,
    iterations: usize,
    cap: f64,
    diagonal: f64,
) -> (Pose, usize, f64) {
    let agree = PAIR_NORMAL_DEGREES.to_radians().cos();
    let (mut matched, mut rms) = (0, 0.0);
    for _ in 0..iterations {
        let mut pairs: Vec<(V3, V3, V3, f64, f64)> = points
            .iter()
            .filter_map(|point| {
                let position = pose.apply(&point.position);
                let (nearest, normal, _) = reference.nearest(&position, cap)?;
                (normal.dot(&(pose.rotation * point.normal)) >= agree).then(|| {
                    let distance = (position - nearest).norm();
                    (position, nearest, normal, point.area, distance)
                })
            })
            .collect();
        let mut distances: Vec<f64> = pairs.iter().map(|pair| pair.4).collect();
        distances.sort_by(f64::total_cmp);
        let Some(median) = distances.get(distances.len() / 2) else {
            break;
        };
        let limit = (OUTLIER_MEDIANS * median).max(1e-9 * diagonal);
        pairs.retain(|pair| pair.4 <= limit);
        if pairs.len() < 6 {
            break;
        }
        let total: f64 = pairs.iter().map(|pair| pair.3).sum();
        let centre = pairs.iter().map(|pair| pair.0 * pair.3).sum::<V3>() / total;
        let mut normal_matrix = Matrix6::zeros();
        let mut right = Vector6::zeros();
        let mut squared = 0.0;
        for (position, nearest, normal, weight, _) in &pairs {
            let arm = position - centre;
            let row =
                Vector6::from_iterator(arm.cross(normal).iter().chain(normal.iter()).copied());
            let residual = (position - nearest).dot(normal);
            normal_matrix += row * row.transpose() * *weight;
            right += row * (residual * weight);
            squared += weight * residual * residual;
        }
        matched = pairs.len();
        rms = (squared / total).sqrt();
        // A flat or round surface leaves some motions unconstrained; a
        // slight damping keeps them still instead of letting them drift.
        let damping = 1e-12 * normal_matrix.trace().max(f64::MIN_POSITIVE);
        for index in 0..6 {
            normal_matrix[(index, index)] += damping;
        }
        let Some(step) = normal_matrix.cholesky().map(|factor| factor.solve(&-right)) else {
            break;
        };
        let spin = V3::new(step[0], step[1], step[2]);
        let shift = V3::new(step[3], step[4], step[5]);
        let turn = Rotation3::new(spin).into_inner();
        pose = Pose {
            rotation: turn * pose.rotation,
            translation: turn * (pose.translation - centre) + centre + shift,
        };
        if spin.norm() < 1e-12 && shift.norm() < 1e-12 * diagonal {
            break;
        }
    }
    (pose, matched, rms)
}

/// The area-weighted centre of a mesh's surface and its principal axes, as
/// the columns of a proper rotation, largest spread first.
fn principal_axes(mesh: &TriangleMesh) -> Option<(V3, Matrix3<f64>)> {
    // Moments about a vertex of the mesh, so a part far from the origin
    // does not lose precision.
    let origin = V3::from(*mesh.vertices.first()?);
    let (mut area, mut first, mut second) = (0.0, V3::zeros(), Matrix3::zeros());
    for triangle in &mesh.triangles {
        let [a, b, c] = triangle.map(|index| V3::from(mesh.vertices[index as usize]) - origin);
        let piece = 0.5 * (b - a).cross(&(c - a)).norm();
        if piece == 0.0 {
            continue;
        }
        let middle = (a + b + c) / 3.0;
        area += piece;
        first += middle * piece;
        second += (a * a.transpose()
            + b * b.transpose()
            + c * c.transpose()
            + middle * middle.transpose() * 9.0)
            * (piece / 12.0);
    }
    if area <= 0.0 {
        return None;
    }
    let centre = first / area;
    let eigen = (second / area - centre * centre.transpose()).symmetric_eigen();
    let mut order = [0, 1, 2];
    order.sort_by(|a, b| eigen.eigenvalues[*b].total_cmp(&eigen.eigenvalues[*a]));
    let mut axes =
        Matrix3::from_columns(&order.map(|index| eigen.eigenvectors.column(index).into_owned()));
    if axes.determinant() < 0.0 {
        let flipped = -axes.column(2);
        axes.set_column(2, &flipped);
    }
    Some((centre + origin, axes))
}

/// The 24 rotations that map the coordinate axes onto themselves, as signed
/// permutations with determinant one.
fn axis_rotations() -> Vec<Matrix3<f64>> {
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut rotations = Vec::new();
    for order in orders {
        for signs in 0..8 {
            let mut turn = Matrix3::zeros();
            for (row, column) in order.into_iter().enumerate() {
                turn[(row, column)] = if signs >> row & 1 == 1 { -1.0 } else { 1.0 };
            }
            if turn.determinant() > 0.0 {
                rotations.push(turn);
            }
        }
    }
    rotations
}

fn alignment_report(method: Alignment, pose: &Pose) -> AlignmentReport {
    let rotation = pose.rotation;
    AlignmentReport {
        method,
        rotation: [0, 1, 2].map(|row| [0, 1, 2].map(|column| rotation[(row, column)])),
        translation_mm: pose.translation.into(),
        rotation_degrees: ((rotation.trace() - 1.0) * 0.5)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees(),
        fit_points: None,
        matched_points: None,
        rms_mm: None,
    }
}

/// Regions beyond the tolerance, outside and inside the reference
/// separately. Readings are placed on a grid of cells one sample spacing
/// wide; readings of the same sign in the same or touching cells join one
/// region.
fn regions(
    readings: &[(V3, f64, Option<f64>)],
    tolerance: f64,
    spacing: f64,
) -> (usize, Vec<DeviationRegion>) {
    let mut found = Vec::new();
    for sign in [1.0, -1.0] {
        let beyond: Vec<(V3, f64, f64)> = readings
            .iter()
            .filter_map(|(position, area, reading)| {
                reading
                    .filter(|value| sign * value > tolerance)
                    .map(|value| (*position, *area, value))
            })
            .collect();
        let cell_of = |point: &V3| point.map(|value| (value / spacing).floor() as i64);
        let mut parent: Vec<usize> = (0..beyond.len()).collect();
        let mut cells: HashMap<[i64; 3], usize> = HashMap::new();
        for (index, (position, ..)) in beyond.iter().enumerate() {
            let cell = cell_of(position);
            let first = *cells.entry([cell.x, cell.y, cell.z]).or_insert(index);
            let (a, b) = (root(&mut parent, index), root(&mut parent, first));
            parent[a] = b;
        }
        for (&[x, y, z], &first) in &cells {
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        if let Some(&other) = cells.get(&[x + dx, y + dy, z + dz]) {
                            let (a, b) = (root(&mut parent, first), root(&mut parent, other));
                            parent[a] = b;
                        }
                    }
                }
            }
        }
        let mut slot_of: HashMap<usize, usize> = HashMap::new();
        let start = found.len();
        for (index, (position, area, value)) in beyond.iter().enumerate() {
            let group = root(&mut parent, index);
            let slot = *slot_of.entry(group).or_insert_with(|| {
                found.push((
                    DeviationRegion {
                        area_mm2: 0.0,
                        extreme_mm: *value,
                        extreme_point_mm: (*position).into(),
                        centre_mm: [0.0; 3],
                        bounds: MeshBounds {
                            minimum_mm: (*position).into(),
                            maximum_mm: (*position).into(),
                            dimensions_mm: [0.0; 3],
                        },
                    },
                    V3::zeros(),
                ));
                found.len() - 1 - start
            });
            let (region, weighted) = &mut found[start + slot];
            region.area_mm2 += area;
            *weighted += position * *area;
            if value.abs() > region.extreme_mm.abs() {
                region.extreme_mm = *value;
                region.extreme_point_mm = (*position).into();
            }
            for axis in 0..3 {
                region.bounds.minimum_mm[axis] = region.bounds.minimum_mm[axis].min(position[axis]);
                region.bounds.maximum_mm[axis] = region.bounds.maximum_mm[axis].max(position[axis]);
            }
        }
    }
    let count = found.len();
    let mut regions: Vec<DeviationRegion> = found
        .into_iter()
        .map(|(mut region, weighted)| {
            region.centre_mm = (weighted / region.area_mm2).into();
            region.bounds.dimensions_mm = [0, 1, 2]
                .map(|axis| region.bounds.maximum_mm[axis] - region.bounds.minimum_mm[axis]);
            region
        })
        .collect();
    regions.sort_by(|a, b| b.area_mm2.total_cmp(&a.area_mm2));
    regions.truncate(MAX_REGIONS);
    (count, regions)
}

/// Area-weighted percentiles, mean and RMS of readings sorted from lowest.
fn distribution(sorted: &[(f64, f64)], total: f64) -> Option<DeviationDistribution> {
    let (first, last) = (sorted.first()?.0, sorted.last()?.0);
    if total <= 0.0 {
        return None;
    }
    let percentile = |share: f64| {
        let target = share * total;
        let mut running = 0.0;
        for &(value, area) in sorted {
            running += area;
            if running >= target {
                return value;
            }
        }
        last
    };
    let mean = sorted.iter().map(|(value, area)| value * area).sum::<f64>() / total;
    let rms = (sorted
        .iter()
        .map(|(value, area)| value * value * area)
        .sum::<f64>()
        / total)
        .sqrt();
    Some(DeviationDistribution {
        minimum: first,
        p5: percentile(0.05),
        p50: percentile(0.5),
        p95: percentile(0.95),
        maximum: last,
        mean,
        rms,
    })
}

/// At most `count` points taken evenly through `points`.
fn spread(points: &[SurfacePoint], count: usize) -> Vec<&SurfacePoint> {
    let step = points.len().div_ceil(count).max(1);
    points.iter().step_by(step).collect()
}

fn diagonal(vertices: &[[f64; 3]]) -> f64 {
    let bounds = crate::mesh_bounds(vertices);
    V3::from(bounds.dimensions_mm).norm().max(f64::MIN_POSITIVE)
}

fn share(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part / whole } else { 0.0 }
}

fn unit(vector: V3) -> Option<V3> {
    let length = vector.norm();
    (length.is_finite() && length > 0.0).then(|| vector / length)
}

fn to_parry(point: &V3) -> Vector {
    Vector::new(point.x, point.y, point.z)
}

fn from_parry(vector: Vector) -> V3 {
    V3::new(vector.x, vector.y, vector.z)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ValidationOptions, analyze_mesh};

    /// A closed prism: a section in X/Z, with its cap triangles, extruded
    /// along Y from 0 to `depth` and wound outward.
    fn prism(section: &[[f64; 2]], caps: &[[u32; 3]], depth: f64) -> TriangleMesh {
        let mut vertices = Vec::new();
        for y in [0.0, depth] {
            for [x, z] in section {
                vertices.push([*x, y, *z]);
            }
        }
        let n = section.len() as u32;
        let mut triangles = Vec::new();
        for [a, b, c] in caps {
            triangles.push([*a, *b, *c]);
            triangles.push([a + n, c + n, b + n]);
        }
        for i in 0..n {
            let j = (i + 1) % n;
            triangles.push([i, j + n, j]);
            triangles.push([i, i + n, j + n]);
        }
        let volume: f64 = triangles
            .iter()
            .map(|triangle| {
                let [a, b, c] = triangle.map(|index| V3::from(vertices[index as usize]));
                a.dot(&b.cross(&c))
            })
            .sum();
        if volume < 0.0 {
            for triangle in &mut triangles {
                triangle.swap(1, 2);
            }
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    fn block(width: f64, height: f64, depth: f64) -> TriangleMesh {
        prism(
            &[[0.0, 0.0], [width, 0.0], [width, height], [0.0, height]],
            &[[0, 1, 2], [0, 2, 3]],
            depth,
        )
    }

    /// An L-shaped bracket with unequal arms, so no rotation maps it onto
    /// itself.
    fn bracket() -> TriangleMesh {
        prism(
            &[
                [0.0, 0.0],
                [30.0, 0.0],
                [30.0, 6.0],
                [8.0, 6.0],
                [8.0, 20.0],
                [0.0, 20.0],
            ],
            &[[0, 1, 2], [0, 2, 3], [0, 3, 4], [0, 4, 5]],
            12.0,
        )
    }

    /// A closed round boss along Z whose side corners lie on the circle.
    fn boss(radius: f64, height: f64, segments: u32) -> TriangleMesh {
        let mut vertices = vec![[0.0, 0.0, 0.0], [0.0, 0.0, height]];
        for index in 0..segments {
            let angle = std::f64::consts::TAU * f64::from(index) / f64::from(segments);
            let (x, y) = (radius * angle.cos(), radius * angle.sin());
            vertices.push([x, y, 0.0]);
            vertices.push([x, y, height]);
        }
        let mut triangles = Vec::new();
        for i in 0..segments {
            let j = (i + 1) % segments;
            let (bottom_i, top_i, bottom_j, top_j) = (2 + 2 * i, 3 + 2 * i, 2 + 2 * j, 3 + 2 * j);
            triangles.extend([
                [0, bottom_j, bottom_i],
                [1, top_i, top_j],
                [bottom_i, bottom_j, top_j],
                [bottom_i, top_j, top_i],
            ]);
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    fn moved(mesh: &TriangleMesh, pose: &Pose) -> TriangleMesh {
        TriangleMesh {
            vertices: mesh
                .vertices
                .iter()
                .map(|vertex| pose.apply(&V3::from(*vertex)).into())
                .collect(),
            triangles: mesh.triangles.clone(),
        }
    }

    fn compare(
        mesh: &TriangleMesh,
        reference: &TriangleMesh,
        alignment: Alignment,
        tolerance_mm: f64,
    ) -> ComparisonReport {
        measure(
            mesh,
            &ComparisonOptions {
                reference: reference.clone(),
                alignment,
                tolerance_mm,
                max_distance_mm: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_moved_copy_aligns_back() {
        let axis = V3::new(0.3, -0.5, 0.8).normalize();
        for translation in [V3::new(30.0, -12.0, 7.0), V3::new(1000.0, -2000.0, 500.0)] {
            let pose = Pose {
                rotation: Rotation3::new(axis * 50_f64.to_radians()).into_inner(),
                translation,
            };
            let report = compare(
                &moved(&bracket(), &pose),
                &bracket(),
                Alignment::BestFit,
                0.01,
            );
            assert!(
                (report.alignment.rotation_degrees - 50.0).abs() < 1e-6,
                "{:?}",
                report.alignment
            );
            let deviation = report.deviation_mm.unwrap();
            assert!(
                deviation.minimum.abs() < 1e-6 && deviation.maximum.abs() < 1e-6,
                "{deviation:?}"
            );
            assert_eq!(report.region_count, 0);
            assert_eq!(report.unmeasured_samples, 0);
            assert!((report.within_tolerance_share - 1.0).abs() < 1e-12);
            assert!(report.alignment.rms_mm.unwrap() < 1e-6);
        }
    }

    #[test]
    fn a_bump_shows_as_one_raised_region() {
        // A sheet over the block's top face with a smooth bump 0.5 mm high.
        let (centre, height, width) = ([25.0, 15.0], 0.5, 3.0);
        let mut vertices = Vec::new();
        for j in 0..=40 {
            for i in 0..=40 {
                let (x, y) = (f64::from(i), f64::from(j));
                let squared = (x - centre[0]).powi(2) + (y - centre[1]).powi(2);
                vertices.push([
                    x,
                    y,
                    5.0 + height * (-squared / (2.0 * width * width)).exp(),
                ]);
            }
        }
        let at = |i: u32, j: u32| j * 41 + i;
        let mut triangles = Vec::new();
        for j in 0..40 {
            for i in 0..40 {
                triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        let sheet = TriangleMesh {
            vertices,
            triangles,
        };
        let report = compare(&sheet, &block(40.0, 5.0, 40.0), Alignment::None, 0.05);
        assert_eq!(report.region_count, 1, "{:?}", report.regions);
        let region = &report.regions[0];
        assert!((region.extreme_mm - height).abs() < 0.01, "{region:?}");
        let [x, y, _] = region.extreme_point_mm;
        assert!((x - centre[0]).hypot(y - centre[1]) < 1.0, "{region:?}");
        assert!(report.deviation_mm.unwrap().p50.abs() < 0.01);
    }

    #[test]
    fn a_scaled_copy_shows_its_size_error() {
        let reference = bracket();
        let centre = reference
            .vertices
            .iter()
            .map(|vertex| V3::from(*vertex))
            .sum::<V3>()
            / reference.vertices.len() as f64;
        let scaled = TriangleMesh {
            vertices: reference
                .vertices
                .iter()
                .map(|vertex| (centre + (V3::from(*vertex) - centre) * 1.01).into())
                .collect(),
            triangles: reference.triangles.clone(),
        };
        let report = compare(&scaled, &reference, Alignment::BestFit, 0.05);
        assert!(
            report.alignment.rotation_degrees < 0.5,
            "{:?}",
            report.alignment
        );
        let deviation = report.deviation_mm.unwrap();
        assert!(
            deviation.maximum > 0.08 && deviation.rms > 0.03,
            "{deviation:?}"
        );
        assert!(report.region_count > 0);
    }

    #[test]
    fn surface_beyond_a_rim_is_measured_from_the_rim() {
        // The wider boss's end faces reach past the reference's rim, whose
        // nearest points lie on edges between a side and an end.
        let report = compare(
            &boss(10.3, 20.0, 64),
            &boss(10.0, 20.0, 64),
            Alignment::None,
            0.1,
        );
        assert_eq!(report.unmeasured_samples, 0, "{report:?}");
        let deviation = report.deviation_mm.unwrap();
        assert!(deviation.minimum > -1e-9, "{deviation:?}");
        assert!((deviation.maximum - 0.3).abs() < 1e-3, "{deviation:?}");
    }

    #[test]
    fn a_thin_wall_is_measured_against_the_side_facing_the_same_way() {
        // A plate thinner than twice its offset: each face of the copy is
        // nearer the reference's opposite face, which faces the other way.
        let reference = block(40.0, 0.4, 40.0);
        let lifted = moved(
            &reference,
            &Pose {
                rotation: Matrix3::identity(),
                translation: V3::new(0.0, 0.0, 0.3),
            },
        );
        let report = compare(&lifted, &reference, Alignment::None, 0.05);
        let lowest = report
            .regions
            .iter()
            .min_by(|a, b| a.extreme_mm.total_cmp(&b.extreme_mm))
            .unwrap();
        assert!(
            (lowest.extreme_mm + 0.3).abs() < 1e-9,
            "{:?}",
            report.regions
        );
        assert!(lowest.area_mm2 > 1500.0);
        let highest = report
            .regions
            .iter()
            .max_by(|a, b| a.extreme_mm.total_cmp(&b.extreme_mm))
            .unwrap();
        assert!(
            (highest.extreme_mm - 0.3).abs() < 1e-9,
            "{:?}",
            report.regions
        );
    }

    #[test]
    fn a_plate_lifted_clear_of_the_reference_is_measured_on_both_faces() {
        // The lifted underside's nearest reference surface is the top, which
        // faces the other way; the matching underside lies beyond it.
        for shift in [0.0, 1_000_000.0] {
            let along = |mesh: &TriangleMesh, lift: f64| {
                moved(
                    mesh,
                    &Pose {
                        rotation: Matrix3::identity(),
                        translation: V3::new(shift, 0.0, lift),
                    },
                )
            };
            let reference = along(&block(40.0, 0.4, 40.0), 0.0);
            let lifted = along(&block(40.0, 0.4, 40.0), 0.5);
            let report = compare(&lifted, &reference, Alignment::None, 0.05);
            // A point exactly over the reference's outer edge casts along its
            // side and can miss; every other point is measured.
            assert!(report.measured_area_share > 0.9999, "{shift} {report:?}");
            let lowest = report
                .regions
                .iter()
                .min_by(|a, b| a.extreme_mm.total_cmp(&b.extreme_mm))
                .unwrap();
            assert!(
                (lowest.extreme_mm + 0.5).abs() < 1e-6,
                "{shift} {:?}",
                report.regions
            );
            assert!(lowest.area_mm2 > 1500.0);
        }
    }

    #[test]
    fn a_tilted_face_inside_a_thin_slab_reads_square_to_the_matching_face() {
        // A small face tilted 30 degrees from facing down, 0.3 mm above the
        // slab's underside and 0.1 mm below its top, which faces the other
        // way and is nearer.
        let (sin, cos) = 30_f64.to_radians().sin_cos();
        let centre = V3::new(20.0, 20.0, 0.3);
        let (u, v) = (V3::x(), V3::new(0.0, cos, sin));
        let corners = [
            centre - u * 0.02 - v * 0.02,
            centre + u * 0.02 - v * 0.02,
            centre + v * 0.02,
        ];
        let mut face = TriangleMesh {
            vertices: corners.iter().map(|corner| (*corner).into()).collect(),
            triangles: vec![[0, 1, 2]],
        };
        let [a, b, c] = corners;
        if (b - a).cross(&(c - a)).z > 0.0 {
            face.triangles[0].swap(1, 2);
        }
        let report = measure(
            &face,
            &ComparisonOptions {
                reference: block(40.0, 0.4, 40.0),
                alignment: Alignment::None,
                tolerance_mm: 0.01,
                max_distance_mm: Some(0.32),
            },
        )
        .unwrap();
        assert_eq!(report.unmeasured_samples, 0, "{report:?}");
        let mean = report.deviation_mm.unwrap().mean;
        assert!((mean + 0.3).abs() < 0.01, "{mean}");
    }

    #[test]
    fn a_reading_past_a_rim_runs_to_the_face_edge() {
        // Beside a 1 mm block's right face, below its top, facing down and
        // inwards: the nearest surface faces the other way, and the matching
        // underside is nearest at its edge, about 0.316 mm away.
        let normal = V3::new(-0.6, 0.0, -0.8);
        let centre = V3::new(1.1, 0.5, 0.3);
        let u = V3::y();
        let v = normal.cross(&u);
        let corners = [
            centre - u * 0.002 - v * 0.002,
            centre + u * 0.002 - v * 0.002,
            centre + v * 0.002,
        ];
        let mut face = TriangleMesh {
            vertices: corners.iter().map(|corner| (*corner).into()).collect(),
            triangles: vec![[0, 1, 2]],
        };
        let [a, b, c] = corners;
        if (b - a).cross(&(c - a)).dot(&normal) < 0.0 {
            face.triangles[0].swap(1, 2);
        }
        let measure_within = |reach: f64| {
            measure(
                &face,
                &ComparisonOptions {
                    reference: block(1.0, 0.4, 1.0),
                    alignment: Alignment::None,
                    tolerance_mm: 0.01,
                    max_distance_mm: Some(reach),
                },
            )
            .unwrap()
        };
        let near = measure_within(0.32);
        assert_eq!(near.unmeasured_samples, 0, "{near:?}");
        let reading = near.deviation_mm.unwrap().mean;
        assert!(
            (reading.abs() - 0.1_f64.hypot(0.3)).abs() < 0.005,
            "{reading}"
        );
        let far = measure_within(0.31);
        assert_eq!(far.unmeasured_samples, far.samples, "{far:?}");
    }

    #[test]
    fn interleaved_facets_are_all_measured() {
        // A sheet on the block's top and a sheet raised 0.5 mm over it,
        // their triangles alternating.
        let cells = 255_u32;
        let size = 40.0 / f64::from(cells);
        let grid = |height: f64| {
            let mut vertices = Vec::new();
            for j in 0..=cells {
                for i in 0..=cells {
                    vertices.push([f64::from(i) * size, f64::from(j) * size, height]);
                }
            }
            vertices
        };
        let mut vertices = grid(5.0);
        vertices.extend(grid(5.5));
        let offset = (cells + 1) * (cells + 1);
        let at = |i: u32, j: u32| j * (cells + 1) + i;
        let mut triangles = Vec::new();
        for j in 0..cells {
            for i in 0..cells {
                for [a, b, c] in [
                    [at(i, j), at(i + 1, j), at(i + 1, j + 1)],
                    [at(i, j), at(i + 1, j + 1), at(i, j + 1)],
                ] {
                    triangles.push([a, b, c]);
                    triangles.push([a + offset, b + offset, c + offset]);
                }
            }
        }
        let sheets = TriangleMesh {
            vertices,
            triangles,
        };
        let report = compare(&sheets, &block(40.0, 5.0, 40.0), Alignment::None, 0.05);
        assert!(report.samples >= sheets.triangles.len());
        assert!(
            (report.within_tolerance_share - 0.5).abs() < 0.05,
            "{}",
            report.within_tolerance_share
        );
        assert!(
            report
                .regions
                .iter()
                .any(|region| (region.extreme_mm - 0.5).abs() < 1e-6)
        );
    }

    #[test]
    fn a_dense_mesh_keeps_a_point_on_every_triangle() {
        let cells = 360;
        let mut vertices = Vec::new();
        for j in 0..=cells {
            for i in 0..=cells {
                vertices.push([f64::from(i) / 9.0, f64::from(j) / 9.0, 5.0]);
            }
        }
        let at = |i: u32, j: u32| j * (cells + 1) + i;
        let mut triangles = Vec::new();
        for j in 0..cells {
            for i in 0..cells {
                triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        let sheet = TriangleMesh {
            vertices,
            triangles,
        };
        let report = compare(&sheet, &block(40.0, 5.0, 40.0), Alignment::None, 0.05);
        assert!(
            report.samples >= sheet.triangles.len(),
            "{}",
            report.samples
        );
        assert_eq!(report.unmeasured_samples, 0);
        assert!((report.within_tolerance_share - 1.0).abs() < 1e-12);
    }

    #[test]
    fn comparison_options_are_validated() {
        let options = |tolerance_mm: f64, max_distance_mm: Option<f64>, reference: TriangleMesh| {
            ComparisonOptions {
                reference,
                alignment: Alignment::None,
                tolerance_mm,
                max_distance_mm,
            }
        };
        let block = block(1.0, 1.0, 1.0);
        assert!(validate(&options(0.1, None, block.clone())).is_ok());
        assert!(validate(&options(0.0, None, block.clone())).is_err());
        assert!(validate(&options(f64::NAN, None, block.clone())).is_err());
        assert!(validate(&options(0.1, Some(-1.0), block.clone())).is_err());
        let empty = TriangleMesh {
            vertices: Vec::new(),
            triangles: Vec::new(),
        };
        assert!(validate(&options(0.1, None, empty)).is_err());
        let mut broken = block;
        broken.triangles[0][0] = 99;
        assert!(validate(&options(0.1, None, broken)).is_err());
    }

    #[test]
    fn deviation_beyond_tolerance_is_a_warning() {
        let reference = block(20.0, 10.0, 20.0);
        let validate = |mesh: &TriangleMesh| {
            analyze_mesh(
                mesh,
                ValidationOptions {
                    comparison: Some(ComparisonOptions {
                        reference: reference.clone(),
                        alignment: Alignment::None,
                        tolerance_mm: 0.05,
                        max_distance_mm: None,
                    }),
                    ..ValidationOptions::default()
                },
            )
            .unwrap()
        };
        let same = validate(&reference);
        assert_eq!(same.comparison.unwrap().region_count, 0);
        assert!(
            same.issues
                .iter()
                .all(|issue| issue.code != "deviation_beyond_tolerance")
        );
        let taller = validate(&block(20.0, 10.5, 20.0));
        assert!(taller.comparison.unwrap().region_count > 0);
        assert!(
            taller
                .issues
                .iter()
                .any(|issue| issue.code == "deviation_beyond_tolerance")
        );
    }
}
