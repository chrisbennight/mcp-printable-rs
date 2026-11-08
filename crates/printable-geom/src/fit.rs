//! Shapes fitted to regions of a mesh, and dimensions measured between them.
//!
//! Each feature starts from a seed point. The nearest face, or the nearest
//! face agreeing with an optional direction hint, seeds a region that grows
//! across shared edges while each face's corners lie on the fitted shape
//! within three times the fit's spread and its normal agrees with the
//! shape's. The shape is refitted to the region's vertices until the region
//! stops changing, so a seed beside an edge does not take in the
//! neighbouring surface. On a tessellated surface the vertices lie on the
//! designed shape and the flat faces between them fall inside it, so the fit
//! reports the designed size and the deviation reports the facets' departure.

use std::collections::{HashMap, HashSet, VecDeque};

use nalgebra::{Matrix3, Matrix4, SMatrix, SVector, Vector3, Vector4};
use parry3d_f64::math::Vector;
use parry3d_f64::query::PointQuery;
use parry3d_f64::shape::Triangle;
use serde::Serialize;

use crate::assessment::CriterionStatus;
use crate::{GeometryError, MeshBounds, TriangleMesh};

/// At most this many features and requirements are accepted in one call.
pub const MAX_FEATURES: usize = 32;
pub const MAX_REQUIREMENTS: usize = 64;
const MAX_NAME_CHARS: usize = 64;
/// A face whose corners stray more than this many standard deviations of
/// the fit from the shape leaves the region.
const SIGMA_TOLERANCE: f64 = 3.0;
/// A face whose normal turns more than this from the shape's leaves the
/// region.
const MAX_NORMAL_DEVIATION_DEGREES: f64 = 45.0;
/// Faces turned more than this from their neighbour, or from the seed, do
/// not join the patch used for a curved shape's first estimate, so it stays
/// off a face across an end even beyond a chamfer. A circle tessellated into
/// five sides, the fewest OpenSCAD uses, has sides meeting at 72 degrees.
const MAX_START_DIHEDRAL_DEGREES: f64 = 75.0;
/// Faces on an edge shared by at most this many faces all link to each
/// other.
const MAX_PAIRED_EDGE_FACES: usize = 8;
/// Edge steps from the seed face in the patch used for a first estimate.
const START_RING: usize = 8;
/// Faces bending less than this from the seed are treated as coplanar with
/// it when estimating an axis.
const AXIS_MIN_BEND_DEGREES: f64 = 0.5;
/// Faces within this angle of running along an axis support it.
const ALONG_AXIS_DEGREES: f64 = 5.0;
/// A cylinder's first circle uses faces within this angle of running along
/// the axis, so a side tapering by more than this stays out of it.
const START_ALONG_AXIS_DEGREES: f64 = 1.0;
/// The fewest points that place a first circle: two rings of three.
const START_MIN_POINTS: usize = 6;
/// Axis directions tried, and faces scored against each, at most.
const MAX_AXIS_CANDIDATES: usize = 200;
const MAX_AXIS_FACES: usize = 2_000;
/// A curved shape's starting patch widens until its normals turn this far
/// from the seed's.
const START_TURN_DEGREES: f64 = 20.0;
/// A plane starts from nearby faces whose normals agree with the seed's
/// within the first angle when they lie exactly on one plane, and within
/// the second otherwise.
const PLANE_EXACT_DEGREES: f64 = 0.5;
const PLANE_START_DEGREES: f64 = 5.0;
/// Axis directions within this angle of the best agree with it.
const AXIS_AGREEMENT_DEGREES: f64 = 2.0;
/// A direction hint selects faces within this many degrees of it.
const HINT_DEGREES: f64 = 30.0;
const MAX_GROWTH_ROUNDS: usize = 12;
const MAX_SOLVER_ITERATIONS: usize = 100;
/// STL coordinates carry rounding: ASCII STL keeps about six significant
/// digits. A distance from the shape below this fraction of a point's
/// coordinates along the shape's normal, or of one millimetre, counts as on
/// the shape.
const COORDINATE_RESOLUTION: f64 = 1e-5;
/// A hole's fit starts from a wall face within this angle of vertical.
const HOLE_WALL_DEGREES: f64 = 10.0;
/// A hole's fitted axis lies within this angle of vertical.
const HOLE_AXIS_DEGREES: f64 = 1.0;
/// A hole's wall covers its whole circle, to within rounding.
const HOLE_ARC_DEGREES: f64 = 359.9;

type V3 = Vector3<f64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Plane,
    Cylinder,
    Sphere,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Shape::Plane => "plane",
            Shape::Cylinder => "cylinder",
            Shape::Sphere => "sphere",
        }
    }
}

/// A named shape to fit around a seed point.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureRequest {
    pub name: String,
    pub shape: Shape,
    pub seed_mm: [f64; 3],
    /// A plane's approximate outward normal or a cylinder's approximate
    /// axis, which selects the seed face. Not used for spheres.
    pub direction: Option<[f64; 3]>,
    /// Refit without vertices farther than this many standard deviations
    /// from the first fit: 1, 2 or 3.
    pub exclude_beyond_sigma: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    Diameter,
    Distance,
    Angle,
}

impl Measure {
    fn label(self) -> &'static str {
        match self {
            Measure::Diameter => "diameter",
            Measure::Distance => "distance",
            Measure::Angle => "angle",
        }
    }
}

/// A nominal value with a symmetric tolerance: millimetres for a diameter
/// or distance, degrees for an angle.
#[derive(Debug, Clone, PartialEq)]
pub struct DimensionRequirement {
    pub measure: Measure,
    /// One feature for a diameter, two for a distance or angle.
    pub features: Vec<String>,
    pub nominal: f64,
    pub tolerance: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DimensionOptions {
    pub features: Vec<FeatureRequest>,
    pub requirements: Vec<DimensionRequirement>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DimensionReport {
    pub features: Vec<FeatureFit>,
    pub requirements: Vec<RequirementCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeatureFit {
    pub name: String,
    pub shape: Shape,
    /// Distance from the seed to the face the region grew from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_distance_mm: Option<f64>,
    /// Why the shape could not be fitted, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmeasured_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fit {
    pub faces: usize,
    pub area_mm2: f64,
    /// Vertices the shape was fitted to, and those left out by
    /// `exclude_beyond_sigma`.
    pub points: usize,
    pub excluded_points: usize,
    /// Root-mean-square distance of the fitted vertices from the shape.
    pub sigma_mm: f64,
    /// The region surface's largest signed distance inside and outside the
    /// shape: along a plane's normal, or away from a cylinder's axis or a
    /// sphere's centre.
    pub deviation_mm: [f64; 2],
    pub form_error_mm: f64,
    pub region: MeshBounds,
    #[serde(flatten)]
    pub geometry: Geometry,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Geometry {
    Plane {
        point_mm: [f64; 3],
        normal: [f64; 3],
    },
    Cylinder {
        /// The axis point midway along the region.
        axis_point_mm: [f64; 3],
        axis_direction: [f64; 3],
        radius_mm: f64,
        diameter_mm: f64,
        length_mm: f64,
        /// How much of the circle the region's faces cover.
        arc_degrees: f64,
    },
    Sphere {
        centre_mm: [f64; 3],
        radius_mm: f64,
        diameter_mm: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequirementCheck {
    pub measure: Measure,
    pub features: Vec<String>,
    pub nominal: f64,
    pub tolerance: f64,
    pub status: CriterionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    /// The smallest and largest value over the surface; both must lie
    /// within the tolerance. For a diameter this spans the facets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    /// The nominal is far outside the tolerance: a mistyped value or the
    /// wrong feature is more likely than a manufacturing error.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub nominal_far_from_measured: bool,
}

/// Reject options that cannot produce a trustworthy report.
pub(crate) fn validate(options: &DimensionOptions) -> Result<(), GeometryError> {
    let invalid = |message: String| Err(GeometryError::InvalidOptions(message));
    if options.features.len() > MAX_FEATURES {
        return invalid(format!("dimensions accept at most {MAX_FEATURES} features"));
    }
    if options.requirements.len() > MAX_REQUIREMENTS {
        return invalid(format!(
            "dimensions accept at most {MAX_REQUIREMENTS} requirements"
        ));
    }
    let mut shapes = HashMap::new();
    for feature in &options.features {
        let name = &feature.name;
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return invalid(format!(
                "feature names must have 1 to {MAX_NAME_CHARS} characters"
            ));
        }
        if shapes.insert(name.as_str(), feature.shape).is_some() {
            return invalid(format!("feature name {name:?} is used twice"));
        }
        if feature.seed_mm.iter().any(|value| !value.is_finite()) {
            return invalid(format!("feature {name:?} needs a finite seed_mm"));
        }
        if let Some(direction) = feature.direction {
            if feature.shape == Shape::Sphere {
                return invalid(format!(
                    "feature {name:?} is a sphere, which takes no direction"
                ));
            }
            if unit(V3::from(direction)).is_none() {
                return invalid(format!(
                    "feature {name:?} needs a finite non-zero direction"
                ));
            }
        }
        if feature
            .exclude_beyond_sigma
            .is_some_and(|sigma| !(1..=3).contains(&sigma))
        {
            return invalid(format!(
                "feature {name:?} exclude_beyond_sigma must be 1, 2 or 3"
            ));
        }
    }
    for requirement in &options.requirements {
        let wanted = match requirement.measure {
            Measure::Diameter => 1,
            Measure::Distance | Measure::Angle => 2,
        };
        if requirement.features.len() != wanted
            || (wanted == 2 && requirement.features[0] == requirement.features[1])
        {
            return invalid(format!(
                "a {} requirement names {wanted} different features",
                requirement.measure.label()
            ));
        }
        for name in &requirement.features {
            let Some(shape) = shapes.get(name.as_str()) else {
                return invalid(format!("requirement names unknown feature {name:?}"));
            };
            let unsuitable = match requirement.measure {
                Measure::Diameter => *shape == Shape::Plane,
                Measure::Angle => *shape == Shape::Sphere,
                Measure::Distance => false,
            };
            if unsuitable {
                return invalid(format!(
                    "a {} requirement cannot use the {} feature {name:?}",
                    requirement.measure.label(),
                    shape.label()
                ));
            }
        }
        let (nominal, tolerance) = (requirement.nominal, requirement.tolerance);
        let nominal_range = match requirement.measure {
            Measure::Angle => 0.0..=90.0,
            Measure::Diameter | Measure::Distance => 0.0..=f64::MAX,
        };
        if !nominal.is_finite() || !nominal_range.contains(&nominal) {
            return invalid(
                "requirement nominal must be finite and at least 0, and at most 90 degrees for an angle"
                    .to_string(),
            );
        }
        if !tolerance.is_finite() || tolerance < 0.0 {
            return invalid("requirement tolerance must be finite and at least 0".to_string());
        }
    }
    Ok(())
}

/// Fit every requested feature and check every requirement.
pub(crate) fn measure(mesh: &TriangleMesh, options: &DimensionOptions) -> DimensionReport {
    // Requirements name features, so without features there is nothing to do.
    if options.features.is_empty() {
        return DimensionReport {
            features: Vec::new(),
            requirements: Vec::new(),
        };
    }
    let surface = Surface::new(mesh);
    let features: Vec<FeatureFit> = options
        .features
        .iter()
        .map(|request| fit_feature(&surface, request))
        .collect();
    let fits: HashMap<&str, &FeatureFit> = features
        .iter()
        .map(|feature| (feature.name.as_str(), feature))
        .collect();
    let requirements = options
        .requirements
        .iter()
        .map(|requirement| check(requirement, &fits))
        .collect();
    DimensionReport {
        features,
        requirements,
    }
}

/// The dimensional criterion: failed if any requirement failed, unmeasured
/// if any could not be measured, otherwise passed.
pub(crate) fn criterion(report: &DimensionReport) -> Option<CriterionStatus> {
    if report.requirements.is_empty() {
        return None;
    }
    let any = |status| {
        report
            .requirements
            .iter()
            .any(|requirement| requirement.status == status)
    };
    Some(if any(CriterionStatus::Failed) {
        CriterionStatus::Failed
    } else if any(CriterionStatus::Unmeasured) {
        CriterionStatus::Unmeasured
    } else {
        CriterionStatus::Passed
    })
}

/// A complete vertical cylindrical inner wall: a hole's centre in X and Y,
/// its fitted radius, and the smallest and largest radius over its wall, from
/// facets that cut inside the circle to a taper's wide end.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VerticalHole {
    pub center_mm: [f64; 2],
    pub radius_mm: f64,
    pub radius_range_mm: [f64; 2],
}

/// The complete vertical cylindrical inner walls around declared holes, each
/// given as its centre in X and Y and its radius. Every inward-facing wall
/// face with its corners within twice the declared radius of the centre may
/// seed a cylinder fit. Each group of wall faces joined across edges is
/// fitted once, from its face whose farthest corner lies nearest the
/// declared radius: a tessellated bore's corners lie on its circle while its
/// flats cut inside. A fit counts when its axis is vertical and its wall
/// covers the whole circle and faces the axis, so every section of a stepped
/// opening that is a complete hole is reported, and one that is not, such as
/// a nut trap's flats, hides nothing. The fitted centre and radius are
/// reported, so a hole that moved or changed size is measured as it is, not
/// as declared, and a hole that more than one declaration reaches is listed
/// once.
pub fn vertical_holes(mesh: &TriangleMesh, declared: &[([f64; 2], f64)]) -> Vec<VerticalHole> {
    let surface = Surface::new(mesh);
    let level = HOLE_WALL_DEGREES.to_radians().sin();
    let is_wall = |face: usize| surface.normals[face].is_some_and(|normal| normal.z.abs() <= level);
    // Wall faces in order of X, so each search scans only a strip.
    let mut walls: Vec<(V3, usize)> = (0..surface.normals.len())
        .filter(|&face| is_wall(face))
        .map(|face| (centroid(&surface.corners(face)), face))
        .collect();
    walls.sort_by(|a, b| a.0.x.total_cmp(&b.0.x));
    let mut found = Vec::new();
    // The faces of the walls found.
    let mut claimed = HashSet::new();
    for &(center, radius) in declared {
        let reach = 2.0 * radius;
        let first = walls.partition_point(|(point, _)| point.x < center[0] - reach);
        let mut seeds: Vec<(f64, usize)> = walls[first..]
            .iter()
            .take_while(|(point, _)| point.x <= center[0] + reach)
            .filter_map(|&(point, face)| {
                let normal = surface.normals[face]?;
                let inward =
                    normal.x * (point.x - center[0]) + normal.y * (point.y - center[1]) < 0.0;
                let farthest = surface
                    .corners(face)
                    .map(|corner| (corner.x - center[0]).hypot(corner.y - center[1]))
                    .into_iter()
                    .fold(0.0, f64::max);
                (inward && farthest <= reach).then_some(((farthest - radius).abs(), face))
            })
            .collect();
        seeds.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut tried = HashSet::new();
        for (_, face) in seeds {
            if claimed.contains(&face) || tried.contains(&face) {
                continue;
            }
            tried.extend(surface.grow(face, |_, next| is_wall(next)));
            if let Some((hole, wall)) = fit_hole(&surface, face)
                && !wall.iter().any(|face| claimed.contains(face))
            {
                claimed.extend(wall);
                found.push(hole);
            }
        }
    }
    found
}

/// A complete vertical inner wall fitted from `face`, with its wall's faces
/// in order.
fn fit_hole(surface: &Surface, face: usize) -> Option<(VerticalHole, Vec<usize>)> {
    let fit = fit_from(surface, face, Shape::Cylinder, Some(V3::z()), None).ok()?;
    let Geometry::Cylinder {
        axis_point_mm,
        axis_direction,
        radius_mm,
        arc_degrees,
        ..
    } = fit.geometry
    else {
        return None;
    };
    let (point, axis) = (V3::from(axis_point_mm), V3::from(axis_direction));
    if axis.z.abs() < HOLE_AXIS_DEGREES.to_radians().cos() || arc_degrees < HOLE_ARC_DEGREES {
        return None;
    }
    // An inner wall faces its axis; a boss's wall faces away from it.
    let facing_axis = |face: usize| {
        let offset = centroid(&surface.corners(face)) - point;
        let radial = offset - axis * offset.dot(&axis);
        surface.normals[face].is_some_and(|normal| normal.dot(&radial) < 0.0)
    };
    if !facing_axis(face) {
        return None;
    }
    // The whole wall runs from the seed over faces that stay nearly vertical
    // and face the axis, ending at rims, chamfers and steps. The fit can stop
    // short of it on a taper, so the radius range is taken over all of it.
    let level = HOLE_WALL_DEGREES.to_radians().sin();
    let wall = surface.grow(face, |_, next| {
        surface.normals[next].is_some_and(|normal| normal.z.abs() <= level) && facing_axis(next)
    });
    let model = Model::Cylinder {
        point,
        axis,
        radius: radius_mm,
    };
    let (lowest, highest) = wall
        .iter()
        .map(|&face| model.face_range(&surface.corners(face)))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), (a, b)| {
            (low.min(a), high.max(b))
        });
    Some((
        VerticalHole {
            center_mm: [point.x, point.y],
            radius_mm,
            radius_range_mm: [radius_mm + lowest, radius_mm + highest],
        },
        wall,
    ))
}

/// The mesh with face normals, areas and edge neighbours.
struct Surface {
    vertices: Vec<V3>,
    triangles: Vec<[u32; 3]>,
    /// None for a face without area.
    normals: Vec<Option<V3>>,
    areas: Vec<f64>,
    neighbours: Vec<Vec<usize>>,
}

impl Surface {
    fn new(mesh: &TriangleMesh) -> Self {
        let vertices: Vec<V3> = mesh
            .vertices
            .iter()
            .map(|vertex| V3::from(*vertex))
            .collect();
        let mut normals = Vec::with_capacity(mesh.triangles.len());
        let mut areas = Vec::with_capacity(mesh.triangles.len());
        let mut edges = Vec::with_capacity(mesh.triangles.len() * 3);
        for (face, triangle) in mesh.triangles.iter().enumerate() {
            let [a, b, c] = triangle.map(|index| vertices[index as usize]);
            let cross = (b - a).cross(&(c - a));
            let doubled = cross.norm();
            normals.push((doubled > 0.0).then(|| cross / doubled));
            areas.push(doubled * 0.5);
            for (from, to) in [(0, 1), (1, 2), (2, 0)] {
                let (p, q) = (triangle[from], triangle[to]);
                edges.push(((p.min(q), p.max(q)), face));
            }
        }
        edges.sort_unstable();
        let mut neighbours = vec![Vec::new(); mesh.triangles.len()];
        for group in edges.chunk_by(|a, b| a.0 == b.0) {
            if group.len() <= MAX_PAIRED_EDGE_FACES {
                for (index, &(_, face)) in group.iter().enumerate() {
                    for &(_, other) in &group[index + 1..] {
                        if face != other {
                            neighbours[face].push(other);
                            neighbours[other].push(face);
                        }
                    }
                }
            } else {
                // A degenerate edge shared by many faces links them through
                // the first, so the lists grow linearly with the mesh.
                let (_, first) = group[0];
                for &(_, face) in &group[1..] {
                    if face != first {
                        neighbours[first].push(face);
                        neighbours[face].push(first);
                    }
                }
            }
        }
        Self {
            vertices,
            triangles: mesh.triangles.clone(),
            normals,
            areas,
            neighbours,
        }
    }

    fn corners(&self, face: usize) -> [V3; 3] {
        self.triangles[face].map(|index| self.vertices[index as usize])
    }

    /// The faces reachable from `seed` across shared edges that `accept`
    /// admits, in a stable order. The seed is always included.
    fn grow(&self, seed: usize, accept: impl Fn(usize, usize) -> bool) -> Vec<usize> {
        let mut seen = HashSet::from([seed]);
        let mut queue = VecDeque::from([seed]);
        let mut region = Vec::new();
        while let Some(face) = queue.pop_front() {
            region.push(face);
            for &next in &self.neighbours[face] {
                if !seen.contains(&next) && accept(face, next) {
                    seen.insert(next);
                    queue.push_back(next);
                }
            }
        }
        region.sort_unstable();
        region
    }

    /// The faces within `steps` edge steps of `seed` that `accept` admits.
    fn ring(&self, seed: usize, steps: usize, accept: impl Fn(usize, usize) -> bool) -> Vec<usize> {
        let mut depth = HashMap::from([(seed, 0)]);
        let mut queue = VecDeque::from([seed]);
        while let Some(face) = queue.pop_front() {
            let next_depth = depth[&face] + 1;
            if next_depth > steps {
                continue;
            }
            for &next in &self.neighbours[face] {
                if !depth.contains_key(&next) && accept(face, next) {
                    depth.insert(next, next_depth);
                    queue.push_back(next);
                }
            }
        }
        let mut faces: Vec<usize> = depth.into_keys().collect();
        faces.sort_unstable();
        faces
    }

    /// The region's distinct vertices, each weighted by a third of the area
    /// of the region faces around it.
    fn points(&self, region: &[usize]) -> (Vec<V3>, Vec<f64>) {
        let mut weight: HashMap<u32, f64> = HashMap::new();
        for &face in region {
            for &vertex in &self.triangles[face] {
                *weight.entry(vertex).or_default() += self.areas[face] / 3.0;
            }
        }
        let mut entries: Vec<(u32, f64)> = weight.into_iter().collect();
        entries.sort_unstable_by_key(|(vertex, _)| *vertex);
        // A region of faces without area still constrains the fit through
        // its vertices.
        let total: f64 = entries.iter().map(|(_, weight)| weight).sum();
        entries
            .into_iter()
            .map(|(vertex, weight)| {
                let weight = if total > 0.0 { weight } else { 1.0 };
                (self.vertices[vertex as usize], weight)
            })
            .unzip()
    }
}

#[derive(Debug, Clone, Copy)]
enum Model {
    Plane { point: V3, normal: V3 },
    Cylinder { point: V3, axis: V3, radius: f64 },
    Sphere { centre: V3, radius: f64 },
}

impl Model {
    /// Signed distance from the shape: along a plane's normal, or away from
    /// a cylinder's axis or a sphere's centre.
    fn distance(&self, point: &V3) -> f64 {
        match *self {
            Model::Plane {
                point: origin,
                normal,
            } => (point - origin).dot(&normal),
            Model::Cylinder {
                point: origin,
                axis,
                radius,
            } => radial(point - origin, axis).norm() - radius,
            Model::Sphere { centre, radius } => (point - centre).norm() - radius,
        }
    }

    /// The shape's unit normal at a point, pointing along a plane's normal
    /// or away from a cylinder's axis or a sphere's centre.
    fn normal_at(&self, point: &V3) -> Option<V3> {
        match *self {
            Model::Plane { normal, .. } => Some(normal),
            Model::Cylinder {
                point: origin,
                axis,
                ..
            } => unit(radial(point - origin, axis)),
            Model::Sphere { centre, .. } => unit(point - centre),
        }
    }

    /// Whether a point lies within `tolerance` of the shape, or within the
    /// rounding its coordinates can carry. Rounding in a coordinate moves the
    /// distance only through the shape's normal, so a part far from the
    /// origin along its own surface keeps full precision.
    fn near(&self, point: &V3, tolerance: f64) -> bool {
        let reach = self.normal_at(point).map_or_else(
            || point.abs().sum(),
            |normal| point.component_mul(&normal).abs().sum(),
        );
        self.distance(point).abs() <= tolerance.max(COORDINATE_RESOLUTION * reach.max(1.0))
    }

    /// The smallest and largest signed distance over a face.
    fn face_range(&self, corners: &[V3; 3]) -> (f64, f64) {
        let values = corners.map(|corner| self.distance(&corner));
        let lowest = values.iter().copied().fold(f64::INFINITY, f64::min);
        let highest = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        // Distance from an axis or centre is convex, so its largest value
        // over a face is at a corner but its smallest can be inside.
        let inner = match *self {
            Model::Plane { .. } => lowest,
            Model::Cylinder {
                point: origin,
                axis,
                radius,
            } => {
                let (u, v) = basis(axis);
                let flat = corners.map(|corner| {
                    let offset = corner - origin;
                    [offset.dot(&u), offset.dot(&v)]
                });
                distance_to_flat_triangle(flat) - radius
            }
            Model::Sphere { centre, radius } => {
                let [a, b, c] = corners.map(|corner| Vector::new(corner.x, corner.y, corner.z));
                Triangle::new(a, b, c)
                    .distance_to_local_point(Vector::new(centre.x, centre.y, centre.z), true)
                    - radius
            }
        };
        (inner.min(lowest), highest)
    }
}

fn fit_feature(surface: &Surface, request: &FeatureRequest) -> FeatureFit {
    let unmeasured = |seed_distance_mm, reason| FeatureFit {
        name: request.name.clone(),
        shape: request.shape,
        seed_distance_mm,
        unmeasured_reason: Some(reason),
        fit: None,
    };
    let seed = V3::from(request.seed_mm);
    let hint = request
        .direction
        .and_then(|direction| unit(V3::from(direction)));
    let Some((seed_face, seed_distance)) = seed_face(surface, &seed, request.shape, hint) else {
        return unmeasured(None, "no face near the seed agrees with the direction");
    };
    match fit_from(
        surface,
        seed_face,
        request.shape,
        hint,
        request.exclude_beyond_sigma,
    ) {
        Ok(fit) => FeatureFit {
            name: request.name.clone(),
            shape: request.shape,
            seed_distance_mm: Some(seed_distance),
            unmeasured_reason: None,
            fit: Some(fit),
        },
        Err(reason) => unmeasured(Some(seed_distance), reason),
    }
}

/// Fit `shape` to the region grown from `seed_face`, or say why it cannot be
/// fitted.
fn fit_from(
    surface: &Surface,
    seed_face: usize,
    shape: Shape,
    hint: Option<V3>,
    exclude_beyond_sigma: Option<u8>,
) -> Result<Fit, &'static str> {
    let Some((mut model, mut sigma)) = first_estimate(surface, seed_face, shape, hint) else {
        return Err("the surface around the seed does not curve like the requested shape");
    };

    // Grow the region on the shape, refit, and repeat until it settles.
    let mut region = Vec::new();
    for _ in 0..MAX_GROWTH_ROUNDS {
        let tolerance = SIGMA_TOLERANCE * sigma;
        let facing = facing(surface, seed_face, &model);
        let grown = surface.grow(seed_face, |_, face| {
            on_shape(surface, face, &model, facing, tolerance)
        });
        let (points, weights) = surface.points(&grown);
        if points.len() < minimum_points(shape) {
            return Err("too few vertices around the seed lie on the shape");
        }
        let Some(next) = refit(&model, &points, &weights) else {
            return Err("the fit did not converge");
        };
        model = next;
        sigma = spread(&model, &points, &weights);
        let settled = grown == region;
        region = grown;
        if settled {
            break;
        }
    }

    let (mut points, mut weights) = surface.points(&region);
    let mut excluded_points = 0;
    if let Some(limit) = exclude_beyond_sigma {
        let limit = f64::from(limit) * sigma;
        let kept: Vec<(V3, f64)> = points
            .iter()
            .zip(&weights)
            .filter(|(point, _)| model.near(point, limit))
            .map(|(point, weight)| (*point, *weight))
            .collect();
        excluded_points = points.len() - kept.len();
        (points, weights) = kept.into_iter().unzip();
        if points.len() < minimum_points(shape) {
            return Err("too few vertices remain after the exclusion");
        }
        match refit(&model, &points, &weights) {
            Some(next) => model = next,
            None => return Err("the fit did not converge"),
        }
        sigma = spread(&model, &points, &weights);
    }

    let (mut lowest, mut highest) = (f64::INFINITY, f64::NEG_INFINITY);
    for &face in &region {
        let (low, high) = model.face_range(&surface.corners(face));
        lowest = lowest.min(low);
        highest = highest.max(high);
    }
    let region_vertices: Vec<[f64; 3]> = region
        .iter()
        .flat_map(|&face| surface.corners(face))
        .map(|corner| corner.into())
        .collect();
    Ok(Fit {
        faces: region.len(),
        area_mm2: region.iter().map(|&face| surface.areas[face]).sum(),
        points: points.len(),
        excluded_points,
        sigma_mm: sigma,
        deviation_mm: [lowest, highest],
        form_error_mm: highest - lowest,
        region: crate::mesh_bounds(&region_vertices),
        geometry: geometry(surface, &region, &model, &points),
    })
}

/// The fewest vertices that fix the shape.
fn minimum_points(shape: Shape) -> usize {
    match shape {
        Shape::Plane => 3,
        Shape::Sphere => 4,
        Shape::Cylinder => 5,
    }
}

/// The nearest face to the seed, or the nearest agreeing with the hint.
fn seed_face(surface: &Surface, seed: &V3, shape: Shape, hint: Option<V3>) -> Option<(usize, f64)> {
    let limit = HINT_DEGREES.to_radians();
    let point = Vector::new(seed.x, seed.y, seed.z);
    let mut best: Option<(usize, f64)> = None;
    for (face, normal) in surface.normals.iter().enumerate() {
        let Some(normal) = normal else { continue };
        if let Some(hint) = hint {
            let agrees = match shape {
                Shape::Plane => normal.dot(&hint) >= limit.cos(),
                Shape::Cylinder => normal.dot(&hint).abs() <= limit.sin(),
                Shape::Sphere => true,
            };
            if !agrees {
                continue;
            }
        }
        let [a, b, c] = surface
            .corners(face)
            .map(|corner| Vector::new(corner.x, corner.y, corner.z));
        let distance = Triangle::new(a, b, c).distance_to_local_point(point, true);
        if best.is_none_or(|(_, nearest)| distance < nearest) {
            best = Some((face, distance));
        }
    }
    best
}

/// A first shape from the seed face and the patch around it, with its
/// spread over that patch.
fn first_estimate(
    surface: &Surface,
    seed: usize,
    shape: Shape,
    hint: Option<V3>,
) -> Option<(Model, f64)> {
    let seed_normal = surface.normals[seed]?;
    let smooth = |from: usize, to: usize| {
        let limit = MAX_START_DIHEDRAL_DEGREES.to_radians().cos();
        matches!(
            (surface.normals[from], surface.normals[to]),
            (Some(a), Some(b)) if a.dot(&b) >= limit && b.dot(&seed_normal) >= limit
        )
    };
    match shape {
        Shape::Plane => {
            // Nearby faces that agree with the seed. A clean plane's faces
            // agree exactly, so the start keeps to them and stops at any
            // crease. A noisy plane's faces disagree slightly, so its start
            // takes in faces a few degrees off to measure that noise.
            let start = |limit: f64| {
                let agree = limit.to_radians().cos();
                let patch = surface.ring(seed, START_RING, |_, to| {
                    surface.normals[to].is_some_and(|normal| normal.dot(&seed_normal) >= agree)
                });
                let (points, weights) = surface.points(&patch);
                let model = fit_plane(&points, &weights, seed_normal)?;
                Some((model, points, weights))
            };
            let (model, points, weights) = start(PLANE_EXACT_DEGREES)
                .filter(|(model, points, _)| {
                    points.len() > 3 && points.iter().all(|point| model.near(point, 0.0))
                })
                .or_else(|| start(PLANE_START_DEGREES))?;
            Some((model, spread(&model, &points, &weights)))
        }
        Shape::Cylinder => {
            // The surface fixes the axis; an approximate direction stands in
            // only when the surface cannot, or disagrees with it entirely.
            let estimate = axis_from_normals(surface, seed, &start_patch(surface, seed, &smooth));
            let axis = match (estimate, hint) {
                (Some(axis), Some(hint))
                    if axis.dot(&hint).abs() >= HINT_DEGREES.to_radians().cos() =>
                {
                    axis
                }
                (_, Some(hint)) => hint,
                (estimate, None) => estimate?,
            };
            // The first circle uses only faces that run along the axis, which
            // leaves out a taper, a chamfer or the face across an end. Across a
            // coarse polygon these faces meet at sharp angles. A noisy surface
            // may need the wider limit to give enough points.
            [START_ALONG_AXIS_DEGREES, ALONG_AXIS_DEGREES]
                .into_iter()
                .find_map(|limit| {
                    let along = limit.to_radians().sin();
                    let patch = start_patch(surface, seed, &|_, to| {
                        surface.normals[to].is_some_and(|normal| normal.dot(&axis).abs() <= along)
                    });
                    let (points, weights) = surface.points(&patch);
                    if points.len() < START_MIN_POINTS {
                        return None;
                    }
                    let model = circle_estimate(&points, &weights, axis)?;
                    Some((model, spread(&model, &points, &weights)))
                })
        }
        Shape::Sphere => {
            let patch = start_patch(surface, seed, &smooth);
            let (points, weights) = surface.points(&patch);
            let model = sphere_estimate(&points, &weights)?;
            Some((model, spread(&model, &points, &weights)))
        }
    }
}

/// The faces around the seed that `accept` admits, widened until their
/// normals turn far enough from the seed's to fix a curved shape, or until
/// they stop growing. A fine tessellation turns little over a few faces.
fn start_patch(
    surface: &Surface,
    seed: usize,
    accept: &impl Fn(usize, usize) -> bool,
) -> Vec<usize> {
    let turned = START_TURN_DEGREES.to_radians().cos();
    let mut patch = Vec::new();
    for steps in [START_RING, 4 * START_RING, 16 * START_RING] {
        let wider = surface.ring(seed, steps, accept);
        let enough = matches!(surface.normals[seed], Some(seed_normal) if wider.iter().any(|&face| {
            surface.normals[face].is_some_and(|normal| normal.dot(&seed_normal) <= turned)
        }));
        let settled = wider.len() == patch.len();
        patch = wider;
        if enough || settled {
            break;
        }
    }
    patch
}

/// Whether a curved shape's faces point away from its axis or centre (+1),
/// as on a boss or ball, or towards it (-1), as in a hole.
fn facing(surface: &Surface, seed: usize, model: &Model) -> f64 {
    let centre = centroid(&surface.corners(seed));
    match (surface.normals[seed], model.normal_at(&centre)) {
        (Some(face), Some(shape)) if face.dot(&shape) < 0.0 => -1.0,
        _ => 1.0,
    }
}

/// Whether a face lies on the shape: its corners within the tolerance and
/// its normal near the shape's.
fn on_shape(surface: &Surface, face: usize, model: &Model, facing: f64, tolerance: f64) -> bool {
    let corners = surface.corners(face);
    if !corners.iter().all(|corner| model.near(corner, tolerance)) {
        return false;
    }
    let Some(normal) = surface.normals[face] else {
        return true;
    };
    model.normal_at(&centroid(&corners)).is_some_and(|shape| {
        facing * normal.dot(&shape) >= MAX_NORMAL_DEVIATION_DEGREES.to_radians().cos()
    })
}

fn refit(model: &Model, points: &[V3], weights: &[f64]) -> Option<Model> {
    match *model {
        Model::Plane { normal, .. } => fit_plane(points, weights, normal),
        Model::Cylinder { .. } => fit_cylinder(*model, points, weights),
        Model::Sphere { .. } => fit_sphere(*model, points, weights),
    }
}

/// Weighted root-mean-square distance of the points from the shape.
fn spread(model: &Model, points: &[V3], weights: &[f64]) -> f64 {
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let sum: f64 = points
        .iter()
        .zip(weights)
        .map(|(point, weight)| weight * model.distance(point).powi(2))
        .sum();
    (sum / total).sqrt()
}

/// The total least-squares plane, its normal turned towards `facing`.
fn fit_plane(points: &[V3], weights: &[f64], facing: V3) -> Option<Model> {
    let total: f64 = weights.iter().sum();
    if points.len() < 3 || total <= 0.0 {
        return None;
    }
    let centre = points
        .iter()
        .zip(weights)
        .map(|(point, weight)| point * *weight)
        .sum::<V3>()
        / total;
    let mut scatter = Matrix3::zeros();
    for (point, weight) in points.iter().zip(weights) {
        let offset = point - centre;
        scatter += offset * offset.transpose() * *weight;
    }
    let eigen = scatter.symmetric_eigen();
    let smallest = eigen.eigenvalues.imin();
    let mut normal = unit(eigen.eigenvectors.column(smallest).into_owned())?;
    if normal.dot(&facing) < 0.0 {
        normal = -normal;
    }
    Some(Model::Plane {
        point: centre,
        normal,
    })
}

/// The axis of a curved patch. The seed face and any other face of a
/// cylinder both lie along its axis, so the cross product of their normals
/// points along it, and every such pair gives the same direction. Pairs with
/// a chamfer, taper or fillet beside the seed give directions that differ
/// from each other. The direction most other candidates agree with wins,
/// with ties going to the one most patch faces run along, and the
/// directions agreeing with it are averaged. None when no face bends away
/// from the seed, as on a flat patch.
fn axis_from_normals(surface: &Surface, seed: usize, patch: &[usize]) -> Option<V3> {
    let seed_normal = surface.normals[seed]?;
    let faces: Vec<(V3, f64)> = patch
        .iter()
        .filter_map(|&face| surface.normals[face].map(|normal| (normal, surface.areas[face])))
        .collect();
    let bend = AXIS_MIN_BEND_DEGREES.to_radians().sin();
    let candidates: Vec<(V3, f64)> = faces
        .iter()
        .filter_map(|(normal, area)| {
            let cross = seed_normal.cross(normal);
            (cross.norm() > bend).then(|| (cross.normalize(), *area))
        })
        .collect();
    let candidates = evenly(&candidates, MAX_AXIS_CANDIDATES);
    let scored = evenly(&faces, MAX_AXIS_FACES);
    let agree = AXIS_AGREEMENT_DEGREES.to_radians().cos();
    let along = ALONG_AXIS_DEGREES.to_radians().sin();
    let score = |axis: &V3| -> (f64, f64) {
        let agreement = candidates
            .iter()
            .filter(|(other, _)| axis.dot(other).abs() >= agree)
            .map(|(_, area)| area)
            .sum();
        let support = scored
            .iter()
            .filter(|(normal, _)| normal.dot(axis).abs() <= along)
            .map(|(_, area)| area)
            .sum();
        (agreement, support)
    };
    let best = candidates
        .iter()
        .map(|(axis, _)| (*axis, score(axis)))
        .max_by(|a, b| a.1.0.total_cmp(&b.1.0).then(a.1.1.total_cmp(&b.1.1)))?
        .0;
    let mean = candidates
        .iter()
        .filter(|(other, _)| best.dot(other).abs() >= agree)
        .map(|(other, area)| other * best.dot(other).signum() * *area)
        .sum::<V3>();
    unit(mean)
}

/// At most `count` items taken evenly through `items`.
fn evenly<T: Copy>(items: &[T], count: usize) -> Vec<T> {
    let step = items.len().div_ceil(count).max(1);
    items.iter().step_by(step).copied().collect()
}

/// A cylinder along `axis` through the algebraic circle fitted to the
/// points projected across the axis.
fn circle_estimate(points: &[V3], weights: &[f64], axis: V3) -> Option<Model> {
    let total: f64 = weights.iter().sum();
    if points.len() < 3 || total <= 0.0 {
        return None;
    }
    let centre = points
        .iter()
        .zip(weights)
        .map(|(point, weight)| point * *weight)
        .sum::<V3>()
        / total;
    let (u, v) = basis(axis);
    let mut normal = Matrix3::zeros();
    let mut right = Vector3::zeros();
    for (point, weight) in points.iter().zip(weights) {
        let offset = point - centre;
        let (x, y) = (offset.dot(&u), offset.dot(&v));
        let row = Vector3::new(2.0 * x, 2.0 * y, 1.0);
        normal += row * row.transpose() * *weight;
        right += row * ((x * x + y * y) * weight);
    }
    let solution = normal.lu().solve(&right)?;
    let squared = solution[2] + solution[0].powi(2) + solution[1].powi(2);
    if !solution.iter().all(|value| value.is_finite()) || squared <= 0.0 {
        return None;
    }
    Some(Model::Cylinder {
        point: centre + u * solution[0] + v * solution[1],
        axis,
        radius: squared.sqrt(),
    })
}

/// The algebraic sphere through the points.
fn sphere_estimate(points: &[V3], weights: &[f64]) -> Option<Model> {
    let total: f64 = weights.iter().sum();
    if points.len() < 4 || total <= 0.0 {
        return None;
    }
    let centre = points
        .iter()
        .zip(weights)
        .map(|(point, weight)| point * *weight)
        .sum::<V3>()
        / total;
    let mut normal = Matrix4::zeros();
    let mut right = Vector4::zeros();
    for (point, weight) in points.iter().zip(weights) {
        let offset = point - centre;
        let row = Vector4::new(2.0 * offset.x, 2.0 * offset.y, 2.0 * offset.z, 1.0);
        normal += row * row.transpose() * *weight;
        right += row * (offset.norm_squared() * weight);
    }
    let solution = normal.lu().solve(&right)?;
    let middle = Vector3::new(solution[0], solution[1], solution[2]);
    let squared = solution[3] + middle.norm_squared();
    if !solution.iter().all(|value| value.is_finite()) || squared <= 0.0 {
        return None;
    }
    Some(Model::Sphere {
        centre: centre + middle,
        radius: squared.sqrt(),
    })
}

/// Refine a cylinder by damped Gauss-Newton over its five degrees of
/// freedom: the axis's two tilts, its two offsets, and the radius.
fn fit_cylinder(start: Model, points: &[V3], weights: &[f64]) -> Option<Model> {
    if points.len() < 5 {
        return None;
    }
    let Model::Cylinder {
        point,
        axis,
        radius,
    } = start
    else {
        return None;
    };
    let evaluate = |&(point, axis, radius): &(V3, V3, f64), sample: &V3| {
        let offset = sample - point;
        let across = radial(offset, axis);
        let distance = across.norm();
        let (u, v) = basis(axis);
        let gradient = match unit(across) {
            Some(direction) => {
                let along = offset.dot(&axis);
                SVector::<f64, 5>::from([
                    -direction.dot(&u),
                    -direction.dot(&v),
                    -along * direction.dot(&u),
                    -along * direction.dot(&v),
                    -1.0,
                ])
            }
            None => SVector::<f64, 5>::from([0.0, 0.0, 0.0, 0.0, -1.0]),
        };
        (distance - radius, gradient)
    };
    let apply = |&(point, axis, radius): &(V3, V3, f64), step: &SVector<f64, 5>| {
        let (u, v) = basis(axis);
        let radius = radius + step[4];
        (radius > 0.0).then_some(())?;
        Some((
            point + u * step[0] + v * step[1],
            unit(axis + u * step[2] + v * step[3])?,
            radius,
        ))
    };
    let (point, axis, radius) = refine((point, axis, radius), points, weights, evaluate, apply);
    Some(Model::Cylinder {
        point,
        axis,
        radius,
    })
}

/// Refine a sphere's centre and radius by damped Gauss-Newton.
fn fit_sphere(start: Model, points: &[V3], weights: &[f64]) -> Option<Model> {
    if points.len() < 4 {
        return None;
    }
    let Model::Sphere { centre, radius } = start else {
        return None;
    };
    let evaluate = |&(centre, radius): &(V3, f64), sample: &V3| {
        let offset = sample - centre;
        let gradient = match unit(offset) {
            Some(direction) => {
                SVector::<f64, 4>::from([-direction.x, -direction.y, -direction.z, -1.0])
            }
            None => SVector::<f64, 4>::from([0.0, 0.0, 0.0, -1.0]),
        };
        (offset.norm() - radius, gradient)
    };
    let apply = |&(centre, radius): &(V3, f64), step: &SVector<f64, 4>| {
        let radius = radius + step[3];
        (radius > 0.0).then_some((centre + Vector3::new(step[0], step[1], step[2]), radius))
    };
    let (centre, radius) = refine((centre, radius), points, weights, evaluate, apply);
    Some(Model::Sphere { centre, radius })
}

/// Levenberg-Marquardt minimisation of the weighted squared residuals.
/// `evaluate` gives a sample's residual and its gradient in the step's
/// coordinates, and `apply` takes a step, or refuses it.
fn refine<S: Copy, const N: usize>(
    mut state: S,
    points: &[V3],
    weights: &[f64],
    evaluate: impl Fn(&S, &V3) -> (f64, SVector<f64, N>),
    apply: impl Fn(&S, &SVector<f64, N>) -> Option<S>,
) -> S {
    let cost = |state: &S| -> f64 {
        points
            .iter()
            .zip(weights)
            .map(|(point, weight)| weight * evaluate(state, point).0.powi(2))
            .sum()
    };
    let mut current = cost(&state);
    let mut damping = 1e-3;
    for _ in 0..MAX_SOLVER_ITERATIONS {
        let mut normal = SMatrix::<f64, N, N>::zeros();
        let mut gradient = SVector::<f64, N>::zeros();
        for (point, weight) in points.iter().zip(weights) {
            let (residual, row) = evaluate(&state, point);
            normal += row * row.transpose() * *weight;
            gradient += row * (residual * weight);
        }
        let mut accepted = None;
        while damping < 1e12 {
            let mut damped = normal;
            for index in 0..N {
                damped[(index, index)] += damping * normal[(index, index)].max(1e-12);
            }
            let candidate = damped
                .cholesky()
                .map(|factor| factor.solve(&-gradient))
                .and_then(|step| apply(&state, &step));
            if let Some(candidate) = candidate {
                let next = cost(&candidate);
                if next.is_finite() && next <= current {
                    accepted = Some((candidate, next));
                    break;
                }
            }
            damping *= 10.0;
        }
        let Some((candidate, next)) = accepted else {
            break;
        };
        let settled = current - next <= 1e-14 * current;
        state = candidate;
        current = next;
        damping = (damping * 0.1).max(1e-12);
        if settled {
            break;
        }
    }
    state
}

/// Shape parameters to report, with a cylinder's axis point moved to the
/// middle of the region along the axis.
fn geometry(surface: &Surface, region: &[usize], model: &Model, points: &[V3]) -> Geometry {
    match *model {
        Model::Plane { point, normal } => Geometry::Plane {
            point_mm: point.into(),
            normal: normal.into(),
        },
        Model::Cylinder {
            point,
            axis,
            radius,
        } => {
            let heights = points.iter().map(|sample| (sample - point).dot(&axis));
            let lowest = heights.clone().fold(f64::INFINITY, f64::min);
            let highest = heights.fold(f64::NEG_INFINITY, f64::max);
            Geometry::Cylinder {
                axis_point_mm: (point + axis * ((lowest + highest) * 0.5)).into(),
                axis_direction: axis.into(),
                radius_mm: radius,
                diameter_mm: 2.0 * radius,
                length_mm: highest - lowest,
                arc_degrees: arc_degrees(surface, region, point, axis),
            }
        }
        Model::Sphere { centre, radius } => Geometry::Sphere {
            centre_mm: centre.into(),
            radius_mm: radius,
            diameter_mm: 2.0 * radius,
        },
    }
}

/// The angle around the axis covered by the region's faces, in degrees.
fn arc_degrees(surface: &Surface, region: &[usize], point: V3, axis: V3) -> f64 {
    use std::f64::consts::TAU;
    let (u, v) = basis(axis);
    let mut spans = Vec::new();
    for &face in region {
        let mut angles = surface.corners(face).map(|corner| {
            let offset = corner - point;
            offset.dot(&v).atan2(offset.dot(&u)).rem_euclid(TAU)
        });
        angles.sort_by(f64::total_cmp);
        // The face covers the circle except its corners' largest gap.
        let gaps = [
            angles[1] - angles[0],
            angles[2] - angles[1],
            angles[0] + TAU - angles[2],
        ];
        let widest = (0..3)
            .max_by(|a, b| gaps[*a].total_cmp(&gaps[*b]))
            .unwrap_or(2);
        let start = angles[(widest + 1) % 3];
        let length = TAU - gaps[widest];
        if start + length > TAU {
            spans.push((start, TAU));
            spans.push((0.0, start + length - TAU));
        } else {
            spans.push((start, start + length));
        }
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut covered, mut open) = (0.0, None::<(f64, f64)>);
    for (start, end) in spans {
        open = match open {
            Some((low, high)) if start <= high => Some((low, high.max(end))),
            Some((low, high)) => {
                covered += high - low;
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((low, high)) = open {
        covered += high - low;
    }
    covered.to_degrees().min(360.0)
}

/// Check one requirement against the fitted features.
fn check(
    requirement: &DimensionRequirement,
    fits: &HashMap<&str, &FeatureFit>,
) -> RequirementCheck {
    let mut result = RequirementCheck {
        measure: requirement.measure,
        features: requirement.features.clone(),
        nominal: requirement.nominal,
        tolerance: requirement.tolerance,
        status: CriterionStatus::Unmeasured,
        measured: None,
        range: None,
        nominal_far_from_measured: false,
    };
    let geometries: Option<Vec<(&Geometry, [f64; 2])>> = requirement
        .features
        .iter()
        .map(|name| {
            let fit = fits.get(name.as_str())?.fit.as_ref()?;
            Some((&fit.geometry, fit.deviation_mm))
        })
        .collect();
    let Some(geometries) = geometries else {
        return result;
    };
    let (measured, range) = match (requirement.measure, geometries.as_slice()) {
        (Measure::Diameter, [(geometry, [inside, outside])]) => {
            let radius = match geometry {
                Geometry::Cylinder { radius_mm, .. } | Geometry::Sphere { radius_mm, .. } => {
                    *radius_mm
                }
                Geometry::Plane { .. } => return result,
            };
            (
                2.0 * radius,
                [2.0 * (radius + inside), 2.0 * (radius + outside)],
            )
        }
        (Measure::Distance, [(first, _), (second, _)]) => {
            let distance = datum_distance(&Datum::of(first), &Datum::of(second));
            (distance, [distance, distance])
        }
        (Measure::Angle, [(first, _), (second, _)]) => {
            let Some(angle) = datum_angle(&Datum::of(first), &Datum::of(second)) else {
                return result;
            };
            (angle, [angle, angle])
        }
        _ => return result,
    };
    let slack = 1e-9 * requirement.nominal.abs().max(1.0);
    let passed = range[0] >= requirement.nominal - requirement.tolerance - slack
        && range[1] <= requirement.nominal + requirement.tolerance + slack;
    let error = (measured - requirement.nominal).abs();
    result.status = if passed {
        CriterionStatus::Passed
    } else {
        CriterionStatus::Failed
    };
    result.measured = Some(measured);
    result.range = (requirement.measure == Measure::Diameter).then_some(range);
    result.nominal_far_from_measured = !passed
        && error
            > (10.0 * requirement.tolerance)
                .max(0.25 * requirement.nominal.abs())
                .max(1.0);
    result
}

/// What a feature's dimensions are measured from.
enum Datum {
    Plane { point: V3, normal: V3 },
    Line { point: V3, direction: V3 },
    Point(V3),
}

impl Datum {
    fn of(geometry: &Geometry) -> Self {
        match geometry {
            Geometry::Plane { point_mm, normal } => Datum::Plane {
                point: V3::from(*point_mm),
                normal: V3::from(*normal),
            },
            Geometry::Cylinder {
                axis_point_mm,
                axis_direction,
                ..
            } => Datum::Line {
                point: V3::from(*axis_point_mm),
                direction: V3::from(*axis_direction),
            },
            Geometry::Sphere { centre_mm, .. } => Datum::Point(V3::from(*centre_mm)),
        }
    }

    fn centre(&self) -> V3 {
        match self {
            Datum::Plane { point, .. } | Datum::Line { point, .. } | Datum::Point(point) => *point,
        }
    }

    fn direction(&self) -> Option<V3> {
        match self {
            Datum::Plane { normal, .. } => Some(*normal),
            Datum::Line { direction, .. } => Some(*direction),
            Datum::Point(_) => None,
        }
    }
}

/// Distance to a plane when either feature is one, otherwise to an axis,
/// otherwise between centres. Between two planes or two axes it is the
/// mean of each centre's distance to the other.
fn datum_distance(first: &Datum, second: &Datum) -> f64 {
    let to = |datum: &Datum, point: V3| match datum {
        Datum::Plane {
            point: origin,
            normal,
        } => (point - origin).dot(normal).abs(),
        Datum::Line {
            point: origin,
            direction,
        } => radial(point - origin, *direction).norm(),
        Datum::Point(origin) => (point - origin).norm(),
    };
    let rank = |datum: &Datum| match datum {
        Datum::Plane { .. } => 0,
        Datum::Line { .. } => 1,
        Datum::Point(_) => 2,
    };
    match rank(first).cmp(&rank(second)) {
        std::cmp::Ordering::Less => to(first, second.centre()),
        std::cmp::Ordering::Greater => to(second, first.centre()),
        std::cmp::Ordering::Equal => {
            (to(first, second.centre()) + to(second, first.centre())) * 0.5
        }
    }
}

/// The angle between two planes, two axes, or an axis and a plane, from 0
/// for parallel to 90 for perpendicular.
fn datum_angle(first: &Datum, second: &Datum) -> Option<f64> {
    let (a, b) = (first.direction()?, second.direction()?);
    let cosine = a.dot(&b).abs().min(1.0);
    let mixed = matches!(
        (first, second),
        (Datum::Plane { .. }, Datum::Line { .. }) | (Datum::Line { .. }, Datum::Plane { .. })
    );
    Some(if mixed {
        cosine.asin().to_degrees()
    } else {
        cosine.acos().to_degrees()
    })
}

/// The distance from the origin to a triangle in the plane, zero inside it.
fn distance_to_flat_triangle(corners: [[f64; 2]; 3]) -> f64 {
    let cross = |a: [f64; 2], b: [f64; 2], c: [f64; 2]| {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    };
    let origin = [0.0, 0.0];
    let sides = [0, 1, 2].map(|index| cross(corners[index], corners[(index + 1) % 3], origin));
    let area = cross(corners[0], corners[1], corners[2]);
    if area != 0.0 && sides.iter().all(|side| side * area >= 0.0) {
        return 0.0;
    }
    (0..3)
        .map(|index| {
            let (a, b) = (corners[index], corners[(index + 1) % 3]);
            let edge = [b[0] - a[0], b[1] - a[1]];
            let length = edge[0] * edge[0] + edge[1] * edge[1];
            let along = if length > 0.0 {
                ((-a[0] * edge[0] - a[1] * edge[1]) / length).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (a[0] + edge[0] * along).hypot(a[1] + edge[1] * along)
        })
        .fold(f64::INFINITY, f64::min)
}

/// The part of `offset` across `axis`.
fn radial(offset: V3, axis: V3) -> V3 {
    offset - axis * offset.dot(&axis)
}

fn unit(vector: V3) -> Option<V3> {
    let length = vector.norm();
    (length.is_finite() && length > 0.0).then(|| vector / length)
}

fn centroid(corners: &[V3; 3]) -> V3 {
    (corners[0] + corners[1] + corners[2]) / 3.0
}

/// Two unit vectors across `axis`, perpendicular to it and to each other.
fn basis(axis: V3) -> (V3, V3) {
    let helper = if axis.x.abs() < 0.9 { V3::x() } else { V3::y() };
    let u = axis.cross(&helper).normalize();
    (u, axis.cross(&u))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ValidationOptions, analyze_mesh};

    /// Turn a face so its normal agrees with `wanted`.
    fn oriented(vertices: &[[f64; 3]], face: [u32; 3], wanted: V3) -> [u32; 3] {
        let [a, b, c] = face.map(|index| V3::from(vertices[index as usize]));
        if (b - a).cross(&(c - a)).dot(&wanted) < 0.0 {
            [face[0], face[2], face[1]]
        } else {
            face
        }
    }

    /// A closed square plate, `2 * half` across and `height` thick, with a
    /// through hole of `radius` along Z tessellated into `segments` flat
    /// sides whose corners lie on the circle, as OpenSCAD tessellates.
    fn plate_with_hole(half: f64, height: f64, radius: f64, segments: usize) -> TriangleMesh {
        plate(half, height, &[(radius, 0.0), (radius, height)], segments)
    }

    /// A closed square plate with a through hole along Z whose side follows
    /// `profile`, radius and height pairs from the bottom face to the top.
    /// `segments` must be a multiple of four so the plate's corners are
    /// vertices.
    fn plate(half: f64, height: f64, profile: &[(f64, f64)], segments: usize) -> TriangleMesh {
        let stride = profile.len() + 2;
        let mut vertices = Vec::new();
        let mut directions = Vec::new();
        for index in 0..segments {
            let angle = (45.0 + 360.0 * index as f64 / segments as f64).to_radians();
            let (cos, sin) = (angle.cos(), angle.sin());
            let reach = half / cos.abs().max(sin.abs());
            for (radius, z) in profile {
                vertices.push([radius * cos, radius * sin, *z]);
            }
            for z in [0.0, height] {
                vertices.push([reach * cos, reach * sin, z]);
            }
            directions.push(V3::new(cos, sin, 0.0));
        }
        let (last, outer_low, outer_high) = (profile.len() - 1, profile.len(), profile.len() + 1);
        let mut triangles = Vec::new();
        for i in 0..segments {
            let j = (i + 1) % segments;
            let at = |segment: usize, corner: usize| (stride * segment + corner) as u32;
            let outward = directions[i] + directions[j];
            let mut faces = vec![
                ([at(i, last), at(i, outer_high), at(j, outer_high)], V3::z()),
                ([at(i, last), at(j, outer_high), at(j, last)], V3::z()),
                ([at(i, 0), at(i, outer_low), at(j, outer_low)], -V3::z()),
                ([at(i, 0), at(j, outer_low), at(j, 0)], -V3::z()),
                (
                    [at(i, outer_low), at(j, outer_low), at(j, outer_high)],
                    outward,
                ),
                (
                    [at(i, outer_low), at(j, outer_high), at(i, outer_high)],
                    outward,
                ),
            ];
            for k in 0..last {
                let ((r1, z1), (r2, z2)) = (profile[k], profile[k + 1]);
                // Into the hole, square to this step of the profile.
                let into_hole = -outward * (z2 - z1) + V3::z() * (r2 - r1);
                faces.push(([at(i, k), at(j, k), at(j, k + 1)], into_hole));
                faces.push(([at(i, k), at(j, k + 1), at(i, k + 1)], into_hole));
            }
            for (corners, wanted) in faces {
                triangles.push(oriented(&vertices, corners, wanted));
            }
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    /// A closed latitude-longitude sphere whose vertices lie on the sphere.
    fn sphere(centre: V3, radius: f64, around: usize, down: usize) -> TriangleMesh {
        let point = |polar: f64, azimuth: f64| -> [f64; 3] {
            (centre
                + V3::new(
                    polar.sin() * azimuth.cos(),
                    polar.sin() * azimuth.sin(),
                    polar.cos(),
                ) * radius)
                .into()
        };
        let mut vertices = vec![point(0.0, 0.0), point(std::f64::consts::PI, 0.0)];
        for ring in 1..down {
            for step in 0..around {
                vertices.push(point(
                    std::f64::consts::PI * ring as f64 / down as f64,
                    std::f64::consts::TAU * step as f64 / around as f64,
                ));
            }
        }
        let at = |ring: usize, step: usize| (2 + (ring - 1) * around + step % around) as u32;
        let mut faces = Vec::new();
        for step in 0..around {
            faces.push([0, at(1, step), at(1, step + 1)]);
            faces.push([1, at(down - 1, step), at(down - 1, step + 1)]);
            for ring in 1..down - 1 {
                faces.push([at(ring, step), at(ring + 1, step), at(ring + 1, step + 1)]);
                faces.push([at(ring, step), at(ring + 1, step + 1), at(ring, step + 1)]);
            }
        }
        let triangles = faces
            .into_iter()
            .map(|face| {
                let middle = face
                    .iter()
                    .map(|index| V3::from(vertices[*index as usize]))
                    .sum::<V3>()
                    / 3.0;
                oriented(&vertices, face, middle - centre)
            })
            .collect();
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    fn feature(name: &str, shape: Shape, seed: [f64; 3]) -> FeatureRequest {
        FeatureRequest {
            name: name.to_string(),
            shape,
            seed_mm: seed,
            direction: None,
            exclude_beyond_sigma: None,
        }
    }

    fn requirement(
        measure: Measure,
        features: &[&str],
        nominal: f64,
        tolerance: f64,
    ) -> DimensionRequirement {
        DimensionRequirement {
            measure,
            features: features.iter().map(|name| name.to_string()).collect(),
            nominal,
            tolerance,
        }
    }

    fn cylinder(fit: &FeatureFit) -> (f64, [f64; 3], f64, f64) {
        match fit.fit.as_ref().map(|fit| &fit.geometry) {
            Some(Geometry::Cylinder {
                radius_mm,
                axis_direction,
                length_mm,
                arc_degrees,
                ..
            }) => (*radius_mm, *axis_direction, *length_mm, *arc_degrees),
            other => panic!("expected a cylinder, got {other:?}"),
        }
    }

    fn close(actual: f64, expected: f64, within: f64) -> bool {
        (actual - expected).abs() <= within
    }

    /// The meshes, each moved by its offset, as one mesh.
    fn joined(meshes: &[(TriangleMesh, [f64; 3])]) -> TriangleMesh {
        let mut joined = TriangleMesh {
            vertices: Vec::new(),
            triangles: Vec::new(),
        };
        for (mesh, offset) in meshes {
            let base = joined.vertices.len() as u32;
            joined.vertices.extend(
                mesh.vertices
                    .iter()
                    .map(|vertex| [0, 1, 2].map(|axis| vertex[axis] + offset[axis])),
            );
            joined.triangles.extend(
                mesh.triangles
                    .iter()
                    .map(|face| face.map(|index| index + base)),
            );
        }
        joined
    }

    /// A closed vertical post of `radius` from `low` to `high` on Z,
    /// tessellated into `segments` sides whose corners lie on the circle.
    fn post(radius: f64, low: f64, high: f64, segments: usize) -> TriangleMesh {
        let mut vertices = vec![[0.0, 0.0, low], [0.0, 0.0, high]];
        for index in 0..segments {
            let angle = std::f64::consts::TAU * index as f64 / segments as f64;
            vertices.push([radius * angle.cos(), radius * angle.sin(), low]);
            vertices.push([radius * angle.cos(), radius * angle.sin(), high]);
        }
        let mut triangles = Vec::new();
        for i in 0..segments as u32 {
            let j = (i + 1) % segments as u32;
            let (bottom, top, next_bottom, next_top) = (2 + 2 * i, 3 + 2 * i, 2 + 2 * j, 3 + 2 * j);
            let outward =
                V3::from(vertices[bottom as usize]) + V3::from(vertices[next_bottom as usize]);
            for (face, wanted) in [
                ([0, next_bottom, bottom], -V3::z()),
                ([1, top, next_top], V3::z()),
                ([bottom, next_bottom, next_top], outward),
                ([bottom, next_top, top], outward),
            ] {
                triangles.push(oriented(&vertices, face, wanted));
            }
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    #[test]
    fn declared_holes_are_measured_where_they_are() {
        let plate = plate_with_hole(5.0, 4.0, 2.0, 32);
        let corners = [[-12.0, -8.0], [-12.0, 8.0], [12.0, -8.0], [12.0, 8.0]];
        let mesh = joined(&corners.map(|[x, y]| (plate.clone(), [x, y, 0.0])));
        let declared = corners.map(|center| (center, 2.0));
        let holes = vertical_holes(&mesh, &declared);
        assert_eq!(holes.len(), 4, "{holes:?}");
        for (hole, center) in holes.iter().zip(corners) {
            assert!(close(hole.center_mm[0], center[0], 1e-9), "{hole:?}");
            assert!(close(hole.center_mm[1], center[1], 1e-9), "{hole:?}");
            assert!(close(hole.radius_mm, 2.0, 1e-9), "{hole:?}");
            // From the flats, which cut inside the circle, to the corners.
            let flats = 2.0 * (std::f64::consts::PI / 32.0).cos();
            assert!(close(hole.radius_range_mm[0], flats, 1e-6), "{hole:?}");
            assert!(close(hole.radius_range_mm[1], 2.0, 1e-6), "{hole:?}");
        }

        // A hole moved half a millimetre, or declared at half its size, is
        // reported as it is.
        let moved = joined(&[(plate.clone(), [0.5, 0.0, 0.0])]);
        let holes = vertical_holes(&moved, &[([0.0, 0.0], 2.0)]);
        assert_eq!(holes.len(), 1);
        assert!(close(holes[0].center_mm[0], 0.5, 1e-9), "{holes:?}");
        let holes = vertical_holes(&plate, &[([0.0, 0.0], 1.0)]);
        assert!(close(holes[0].radius_mm, 2.0, 1e-9), "{holes:?}");

        // Two declared positions on one hole find it once.
        let holes = vertical_holes(&plate, &[([0.0, 0.0], 2.0), ([0.2, 0.0], 2.0)]);
        assert_eq!(holes.len(), 1, "{holes:?}");

        // Nothing near a declared position.
        assert!(vertical_holes(&plate, &[([20.0, 0.0], 2.0)]).is_empty());
    }

    #[test]
    fn a_tapered_bore_reports_the_radius_it_spans() {
        // Under a degree of taper. Whether or not a taper fits as a complete
        // cylinder, a hole found on it spans both of its ends.
        let covers = |holes: &[VerticalHole], low: f64, high: f64| {
            holes.iter().all(|hole| {
                hole.radius_range_mm[0] <= low && hole.radius_range_mm[1] >= high - 1e-9
            })
        };
        // Built in rings, it fits as a cylinder over part of its height from
        // the ring nearest 2.01 mm.
        let rings: Vec<(f64, f64)> = (0..=10)
            .map(|step| (1.95 + 0.01 * f64::from(step), 2.0 * f64::from(step)))
            .collect();
        let holes = vertical_holes(&plate(5.0, 20.0, &rings, 32), &[([0.0, 0.0], 2.01)]);
        assert_eq!(holes.len(), 1, "{holes:?}");
        assert!(covers(&holes, 1.95, 2.05), "{holes:?}");
        // As one cone, as OpenSCAD builds it.
        let cone = plate(5.0, 20.0, &[(1.9, 0.0), (2.1, 20.0)], 32);
        let holes = vertical_holes(&cone, &[([0.0, 0.0], 2.0)]);
        assert!(covers(&holes, 1.9, 2.1), "{holes:?}");
    }

    #[test]
    fn each_section_of_a_stepped_bore_is_its_own_hole() {
        // A bore 2.012 mm in radius below and 2 mm above. Each section is
        // measured on its own, and finding the one nearest the declared
        // radius does not stop the search for the other.
        let stepped = plate(
            5.0,
            8.0,
            &[(2.012, 0.0), (2.012, 4.0), (2.0, 4.0), (2.0, 8.0)],
            32,
        );
        let holes = vertical_holes(&stepped, &[([0.0, 0.0], 2.0)]);
        assert_eq!(holes.len(), 2, "{holes:?}");
        // Its flats cut inside the circle, so the wider section's face
        // centres lie nearer 2 mm than the narrower's, but its corners do
        // not, and the narrower section is fitted first.
        assert!(close(holes[0].radius_mm, 2.0, 1e-9), "{holes:?}");
        assert!(close(holes[0].radius_range_mm[1], 2.0, 1e-9), "{holes:?}");
        assert!(close(holes[1].radius_mm, 2.012, 1e-9), "{holes:?}");
        assert!(close(holes[1].radius_range_mm[1], 2.012, 1e-9), "{holes:?}");
        let both = vertical_holes(&stepped, &[([0.0, 0.0], 2.0), ([0.0, 0.0], 2.012)]);
        assert_eq!(both, holes);
    }

    #[test]
    fn a_section_of_another_shape_does_not_hide_the_bore() {
        // A bore 2.02 mm in radius below a square opening whose corners lie
        // 2.01 mm from the axis, nearer the declared 2 mm than the bore's.
        let mut stepped = plate(
            5.0,
            8.0,
            &[(2.02, 0.0), (2.02, 4.0), (2.01, 4.0), (2.01, 8.0)],
            32,
        );
        for segment in 0..32 {
            for ring in [2, 3] {
                let vertex = &mut stepped.vertices[6 * segment + ring];
                let scale =
                    2.01 / (std::f64::consts::SQRT_2 * vertex[0].abs().max(vertex[1].abs()));
                vertex[0] *= scale;
                vertex[1] *= scale;
            }
        }
        let holes = vertical_holes(&stepped, &[([0.0, 0.0], 2.0)]);
        assert_eq!(holes.len(), 1, "{holes:?}");
        assert!(close(holes[0].radius_mm, 2.02, 1e-9), "{holes:?}");
    }

    #[test]
    fn nearby_holes_far_from_the_origin_stay_apart() {
        let plate = plate_with_hole(1.5, 2.0, 1.0, 32);
        let mesh = joined(&[
            (plate.clone(), [1_000_000.0, 0.0, 0.0]),
            (plate, [1_000_003.5, 0.0, 0.0]),
        ]);
        let holes = vertical_holes(
            &mesh,
            &[([1_000_000.0, 0.0], 1.0), ([1_000_003.5, 0.0], 1.0)],
        );
        assert_eq!(holes.len(), 2, "{holes:?}");
    }

    #[test]
    fn a_bore_filled_by_another_shell_is_not_a_hole() {
        let options = || ValidationOptions {
            holes: vec![([0.0, 0.0], 2.0)],
            ..ValidationOptions::default()
        };
        let plate = plate_with_hole(5.0, 4.0, 2.0, 32);
        let open = crate::analyze_mesh(&plate, options()).unwrap();
        assert_eq!(open.vertical_holes.as_ref().map(Vec::len), Some(1));
        // A post through the bore, taller than the plate, fills it: the
        // united solid has no hole although the plate's own wall is intact.
        let filled = joined(&[(plate, [0.0; 3]), (post(2.5, -1.0, 5.0, 32), [0.0; 3])]);
        let report = crate::analyze_mesh(&filled, options()).unwrap();
        assert!(report.solid_geometry, "{:?}", report.topology);
        assert_eq!(report.vertical_holes, Some(Vec::new()));
        // Nothing is measured on a mesh that is not a valid solid.
        let mut open_mesh = plate_with_hole(5.0, 4.0, 2.0, 32);
        open_mesh.triangles.pop();
        let report = crate::analyze_mesh(&open_mesh, options()).unwrap();
        assert_eq!(report.vertical_holes, None);
    }

    #[test]
    fn bosses_tilted_and_partial_bores_are_not_holes() {
        // A solid post: its wall faces away from its axis.
        let post = post(2.0, 0.0, 4.0, 32);
        assert!(vertical_holes(&post, &[([0.0, 0.0], 2.0)]).is_empty());

        // A bore leaning 10 degrees.
        let (sin, cos) = 10_f64.to_radians().sin_cos();
        let mut tilted = plate_with_hole(5.0, 4.0, 2.0, 32);
        for vertex in &mut tilted.vertices {
            let [x, y, z] = *vertex;
            *vertex = [x, y * cos - z * sin, y * sin + z * cos];
        }
        assert!(vertical_holes(&tilted, &[([0.0, 0.0], 2.0)]).is_empty());

        // Half a bore.
        let mut half = plate_with_hole(5.0, 4.0, 2.0, 32);
        let vertices = half.vertices.clone();
        half.triangles.retain(|face| {
            face.iter()
                .map(|&index| vertices[index as usize][0])
                .sum::<f64>()
                > 0.0
        });
        assert!(vertical_holes(&half, &[([0.0, 0.0], 2.0)]).is_empty());
    }

    #[test]
    fn a_tessellated_bore_reports_its_design_diameter_and_facet_error() {
        let segments = 48;
        let mesh = plate_with_hole(20.0, 10.0, 4.0, segments);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0])],
                requirements: vec![
                    requirement(Measure::Diameter, &["bore"], 8.0, 0.05),
                    requirement(Measure::Diameter, &["bore"], 8.0, 0.005),
                ],
            },
        );
        let bore = &report.features[0];
        let (radius, axis, length, arc) = cylinder(bore);
        assert!(close(radius, 4.0, 1e-9), "{bore:?}");
        assert!(close(axis[2].abs(), 1.0, 1e-9));
        assert!(close(length, 10.0, 1e-9));
        assert!(close(arc, 360.0, 1e-6));
        let fit = bore.fit.as_ref().unwrap();
        // Only the bore's sides: the plate faces around the rim stay out.
        assert_eq!(fit.faces, 2 * segments);
        let sagitta = 4.0 * (1.0 - (std::f64::consts::PI / segments as f64).cos());
        assert!(close(fit.deviation_mm[0], -sagitta, 1e-9), "{fit:?}");
        assert!(close(fit.deviation_mm[1], 0.0, 1e-9));
        assert!(fit.sigma_mm < 1e-9);

        let [loose, tight] = &report.requirements[..] else {
            panic!("two requirements");
        };
        assert_eq!(loose.status, CriterionStatus::Passed, "{loose:?}");
        assert!(close(loose.measured.unwrap(), 8.0, 1e-9));
        let range = loose.range.unwrap();
        assert!(close(range[0], 8.0 - 2.0 * sagitta, 1e-9) && close(range[1], 8.0, 1e-9));
        // The flat sides come closer to the axis than a tight tolerance allows.
        assert_eq!(tight.status, CriterionStatus::Failed);
        assert!(!tight.nominal_far_from_measured);
    }

    #[test]
    fn a_coarse_bore_fails_its_nominal_diameter() {
        // A coarse hole like OpenSCAD's default thirteen-sided 8 mm hole;
        // the plate needs a multiple of four sides.
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 12);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0])],
                requirements: vec![requirement(Measure::Diameter, &["bore"], 8.0, 0.1)],
            },
        );
        let check = &report.requirements[0];
        assert_eq!(check.status, CriterionStatus::Failed, "{check:?}");
        let inscribed = 8.0 * (std::f64::consts::PI / 12.0).cos();
        assert!(close(check.range.unwrap()[0], inscribed, 1e-9));
        assert!(close(check.measured.unwrap(), 8.0, 1e-9));
    }

    #[test]
    fn a_seed_beside_the_rim_needs_the_axis_to_find_the_bore() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        // Closer to the plate's top face than to the bore's side.
        let seed = [4.05, 0.3, 9.98];
        let without = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, seed)],
                requirements: Vec::new(),
            },
        );
        assert!(
            without.features[0].fit.is_none(),
            "{:?}",
            without.features[0]
        );
        assert!(without.features[0].unmeasured_reason.is_some());

        let mut hinted = feature("bore", Shape::Cylinder, seed);
        hinted.direction = Some([0.0, 0.0, 1.0]);
        let with = measure(
            &mesh,
            &DimensionOptions {
                features: vec![hinted],
                requirements: Vec::new(),
            },
        );
        let (radius, ..) = cylinder(&with.features[0]);
        assert!(close(radius, 4.0, 1e-9));
        assert_eq!(with.features[0].fit.as_ref().unwrap().faces, 96);
    }

    #[test]
    fn the_fit_stops_where_a_chamfer_begins() {
        let mesh = plate(20.0, 10.0, &[(4.0, 0.0), (4.0, 9.0), (5.0, 10.0)], 48);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [4.0, 0.3, 4.0])],
                requirements: Vec::new(),
            },
        );
        let (radius, _, length, _) = cylinder(&report.features[0]);
        assert!(close(radius, 4.0, 1e-9));
        assert!(close(length, 9.0, 1e-9));
        assert_eq!(report.features[0].fit.as_ref().unwrap().faces, 96);
    }

    #[test]
    fn rounded_coordinates_still_fit_the_whole_bore() {
        // A small fine hole far from the origin, written with six
        // significant digits as an ASCII STL is.
        let significant = |value: f64| {
            if value == 0.0 {
                return 0.0;
            }
            let scale = 10_f64.powi(5 - value.abs().log10().floor() as i32);
            (value * scale).round() / scale
        };
        let mut mesh = plate_with_hole(6.0, 4.0, 1.6, 64);
        for vertex in &mut mesh.vertices {
            vertex[0] = significant(vertex[0] + 100.0);
            vertex[1] = significant(vertex[1] + 100.0);
            vertex[2] = significant(vertex[2]);
        }
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [101.6, 100.05, 2.0])],
                requirements: vec![requirement(Measure::Diameter, &["bore"], 3.2, 0.01)],
            },
        );
        let (radius, ..) = cylinder(&report.features[0]);
        assert!(close(radius, 1.6, 1e-3), "{radius}");
        assert_eq!(report.features[0].fit.as_ref().unwrap().faces, 128);
        assert_eq!(report.requirements[0].status, CriterionStatus::Passed);
    }

    #[test]
    fn planes_and_axes_give_distances_and_angles() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![
                    feature("top", Shape::Plane, [12.0, 12.0, 10.0]),
                    feature("bottom", Shape::Plane, [12.0, 12.0, 0.0]),
                    feature("side", Shape::Plane, [20.0, 5.0, 5.0]),
                    feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0]),
                ],
                requirements: vec![
                    requirement(Measure::Distance, &["top", "bottom"], 10.0, 0.01),
                    requirement(Measure::Angle, &["top", "bottom"], 0.0, 0.1),
                    requirement(Measure::Distance, &["bore", "side"], 20.0, 0.01),
                    requirement(Measure::Angle, &["bore", "top"], 90.0, 0.1),
                    requirement(Measure::Angle, &["side", "top"], 90.0, 0.1),
                ],
            },
        );
        let top = report.features[0].fit.as_ref().unwrap();
        assert_eq!(top.faces, 96, "{top:?}");
        assert!(top.form_error_mm < 1e-9);
        for check in &report.requirements {
            assert_eq!(check.status, CriterionStatus::Passed, "{check:?}");
            assert!(check.range.is_none());
        }
        assert!(close(report.requirements[0].measured.unwrap(), 10.0, 1e-9));
        assert!(close(report.requirements[2].measured.unwrap(), 20.0, 1e-9));
        assert_eq!(criterion(&report), Some(CriterionStatus::Passed));
    }

    #[test]
    fn a_sphere_fits_its_vertices() {
        let centre = V3::new(1.0, 2.0, 3.0);
        let mesh = sphere(centre, 6.0, 32, 16);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("ball", Shape::Sphere, [7.2, 2.0, 3.0])],
                requirements: vec![requirement(Measure::Diameter, &["ball"], 12.0, 0.5)],
            },
        );
        let fit = report.features[0].fit.as_ref().unwrap();
        let Geometry::Sphere {
            centre_mm,
            radius_mm,
            ..
        } = fit.geometry
        else {
            panic!("expected a sphere, got {fit:?}");
        };
        assert!(close(radius_mm, 6.0, 1e-9), "{fit:?}");
        assert!((V3::from(centre_mm) - centre).norm() < 1e-9);
        assert_eq!(fit.faces, mesh.triangles.len());
        assert!(fit.deviation_mm[0] < 0.0 && close(fit.deviation_mm[1], 0.0, 1e-9));
        assert_eq!(report.requirements[0].status, CriterionStatus::Passed);
    }

    /// Normally distributed test noise from a fixed sequence.
    fn noise(state: &mut u64) -> f64 {
        let mut uniform = || {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((*state >> 11) as f64 + 0.5) / (1_u64 << 53) as f64
        };
        let (a, b) = (uniform(), uniform());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }

    #[test]
    fn noisy_samples_report_their_noise_as_sigma() {
        let level = 0.02;
        let mut state = 7;
        let mut plane = Vec::new();
        for x in 0..40 {
            for y in 0..40 {
                plane.push(V3::new(x as f64, y as f64, 5.0 + level * noise(&mut state)));
            }
        }
        let weights = vec![1.0; plane.len()];
        let fitted = fit_plane(&plane, &weights, V3::z()).unwrap();
        let sigma = spread(&fitted, &plane, &weights);
        assert!(close(sigma, level, 0.1 * level), "{sigma}");

        let mut tube = Vec::new();
        for step in 0..120 {
            let angle = std::f64::consts::TAU * step as f64 / 120.0;
            for height in 0..12 {
                let radius = 10.0 + level * noise(&mut state);
                tube.push(V3::new(
                    radius * angle.cos(),
                    radius * angle.sin(),
                    height as f64,
                ));
            }
        }
        let weights = vec![1.0; tube.len()];
        let start = circle_estimate(&tube, &weights, V3::new(0.05, 0.0, 1.0).normalize()).unwrap();
        let fitted = fit_cylinder(start, &tube, &weights).unwrap();
        let Model::Cylinder { axis, radius, .. } = fitted else {
            panic!("expected a cylinder");
        };
        assert!(close(radius, 10.0, 0.01), "{radius}");
        assert!(axis.z.abs() > (0.1_f64).to_radians().cos());
        let sigma = spread(&fitted, &tube, &weights);
        assert!(close(sigma, level, 0.1 * level), "{sigma}");
    }

    #[test]
    fn a_noisy_plane_grows_over_its_surface() {
        let level = 0.01;
        let mut state = 11;
        let mut vertices = Vec::new();
        for j in 0..=30 {
            for i in 0..=30 {
                vertices.push([f64::from(i), f64::from(j), level * noise(&mut state)]);
            }
        }
        let at = |i: u32, j: u32| j * 31 + i;
        let mut triangles = Vec::new();
        for j in 0..30 {
            for i in 0..30 {
                triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        let sheet = TriangleMesh {
            vertices,
            triangles,
        };
        let report = measure(
            &sheet,
            &DimensionOptions {
                features: vec![feature("face", Shape::Plane, [15.3, 15.6, 0.0])],
                requirements: Vec::new(),
            },
        );
        let fit = report.features[0].fit.as_ref().unwrap();
        assert!(fit.faces * 10 >= sheet.triangles.len() * 9, "{fit:?}");
        assert!(close(fit.sigma_mm, level, 0.25 * level), "{fit:?}");
    }

    #[test]
    fn a_finely_tessellated_bore_fits_without_a_direction() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 1024);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [4.0, 0.01, 5.0])],
                requirements: Vec::new(),
            },
        );
        let (radius, axis, ..) = cylinder(&report.features[0]);
        assert!(close(radius, 4.0, 1e-9));
        assert!(close(axis[2].abs(), 1.0, 1e-9));
        assert_eq!(report.features[0].fit.as_ref().unwrap().faces, 2048);
    }

    #[test]
    fn a_coarse_bore_stops_at_a_shallow_chamfer() {
        // Six sides, as OpenSCAD's $fn = 6 gives, with a chamfer turned
        // about 11 or about 3 degrees above a 1 mm band.
        let seed = [
            3.5 * 75_f64.to_radians().cos(),
            3.5 * 75_f64.to_radians().sin(),
            0.5,
        ];
        for (top, direction) in [4.2, 4.05]
            .into_iter()
            .flat_map(|top| [(top, None), (top, Some([0.0, 0.0, 1.0]))])
        {
            let mesh = plate(20.0, 2.0, &[(4.0, 0.0), (4.0, 1.0), (top, 2.0)], 6);
            let mut request = feature("bore", Shape::Cylinder, seed);
            request.direction = direction;
            let report = measure(
                &mesh,
                &DimensionOptions {
                    features: vec![request],
                    requirements: Vec::new(),
                },
            );
            let (radius, _, length, _) = cylinder(&report.features[0]);
            assert!(close(radius, 4.0, 1e-9), "{top} {direction:?} {radius}");
            assert!(close(length, 1.0, 1e-9));
            assert_eq!(report.features[0].fit.as_ref().unwrap().faces, 12);
        }
    }

    #[test]
    fn an_approximate_direction_still_finds_the_bore() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        let tilt = 10_f64.to_radians();
        let mut request = feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0]);
        request.direction = Some([tilt.sin(), 0.0, tilt.cos()]);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![request],
                requirements: Vec::new(),
            },
        );
        let (radius, axis, ..) = cylinder(&report.features[0]);
        assert!(close(radius, 4.0, 1e-9));
        assert!(close(axis[2].abs(), 1.0, 1e-9));
        assert_eq!(report.features[0].fit.as_ref().unwrap().faces, 96);
    }

    /// A sheet folded along the line y = 5 by `angle` degrees: flat before
    /// the fold and turned up beyond it, moved `shift` along the fold.
    fn folded(angle: f64, shift: f64) -> TriangleMesh {
        let (sin, cos) = angle.to_radians().sin_cos();
        let mut vertices = Vec::new();
        for j in 0..=10 {
            for i in 0..=10 {
                let (x, y) = (f64::from(i) + shift, f64::from(j));
                let beyond = (y - 5.0).max(0.0);
                vertices.push([x, y.min(5.0) + beyond * cos, beyond * sin]);
            }
        }
        let at = |i: u32, j: u32| j * 11 + i;
        let mut triangles = Vec::new();
        for j in 0..10 {
            for i in 0..10 {
                triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    #[test]
    fn a_fold_keeps_its_two_planes_apart() {
        for (angle, shift) in [(4.0, 0.0), (30.0, 1_000_000.0)] {
            let (sin, cos) = f64::to_radians(angle).sin_cos();
            let report = measure(
                &folded(angle, shift),
                &DimensionOptions {
                    features: vec![
                        feature("flat", Shape::Plane, [shift + 4.3, 2.6, 0.0]),
                        feature(
                            "turned",
                            Shape::Plane,
                            [shift + 4.3, 5.0 + 2.6 * cos, 2.6 * sin],
                        ),
                    ],
                    requirements: vec![requirement(Measure::Angle, &["flat", "turned"], 0.0, 0.5)],
                },
            );
            for fit in &report.features {
                assert_eq!(fit.fit.as_ref().unwrap().faces, 100, "{angle} {fit:?}");
            }
            let check = &report.requirements[0];
            assert_eq!(check.status, CriterionStatus::Failed);
            assert!(close(check.measured.unwrap(), angle, 1e-6), "{check:?}");
        }
    }

    #[test]
    fn faces_sharing_an_edge_all_link() {
        let mesh = TriangleMesh {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.5, 1.0, 0.0],
                [0.5, -1.0, 0.0],
                [0.5, 0.0, 1.0],
            ],
            triangles: vec![[0, 1, 2], [1, 0, 3], [0, 1, 4]],
        };
        let surface = Surface::new(&mesh);
        for face in 0..3 {
            let mut linked = surface.neighbours[face].clone();
            linked.sort_unstable();
            let others: Vec<usize> = (0..3).filter(|other| *other != face).collect();
            assert_eq!(linked, others);
        }
    }

    #[test]
    fn faces_sharing_one_edge_link_linearly() {
        let copies = 2_000;
        let mesh = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2]; copies],
        };
        let links: usize = Surface::new(&mesh).neighbours.iter().map(Vec::len).sum();
        assert!(links <= 6 * copies, "{links}");
    }

    #[test]
    fn a_radius_given_as_the_diameter_is_flagged() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0])],
                requirements: vec![requirement(Measure::Diameter, &["bore"], 4.0, 0.1)],
            },
        );
        let check = &report.requirements[0];
        assert_eq!(check.status, CriterionStatus::Failed);
        assert!(check.nominal_far_from_measured);
    }

    #[test]
    fn exclusion_records_the_vertices_left_out() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        let mut request = feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0]);
        request.exclude_beyond_sigma = Some(3);
        let report = measure(
            &mesh,
            &DimensionOptions {
                features: vec![request],
                requirements: Vec::new(),
            },
        );
        let fit = report.features[0].fit.as_ref().unwrap();
        assert_eq!(fit.points + fit.excluded_points, 96);
    }

    #[test]
    fn dimension_options_are_validated() {
        let bore = feature("bore", Shape::Cylinder, [0.0; 3]);
        let top = feature("top", Shape::Plane, [0.0; 3]);
        let mut ball = feature("ball", Shape::Sphere, [0.0; 3]);
        let cases = [
            (vec![bore.clone(), bore.clone()], vec![]),
            (
                vec![top.clone()],
                vec![requirement(Measure::Diameter, &["top"], 1.0, 0.1)],
            ),
            (
                vec![bore.clone()],
                vec![requirement(Measure::Diameter, &["hole"], 1.0, 0.1)],
            ),
            (
                vec![bore.clone(), top.clone()],
                vec![requirement(Measure::Angle, &["bore", "top"], 120.0, 0.1)],
            ),
            (
                vec![bore.clone(), top.clone()],
                vec![requirement(Measure::Distance, &["bore"], 1.0, 0.1)],
            ),
            (
                vec![bore.clone()],
                vec![requirement(Measure::Diameter, &["bore"], 1.0, -0.1)],
            ),
            (vec![feature("", Shape::Plane, [0.0; 3])], vec![]),
            (
                vec![feature("far", Shape::Plane, [f64::NAN, 0.0, 0.0])],
                vec![],
            ),
            (
                {
                    ball.direction = Some([0.0, 0.0, 1.0]);
                    vec![ball.clone()]
                },
                vec![],
            ),
        ];
        for (features, requirements) in cases {
            let options = DimensionOptions {
                features,
                requirements,
            };
            assert!(validate(&options).is_err(), "{options:?}");
        }
        let mut excluded = bore.clone();
        excluded.exclude_beyond_sigma = Some(4);
        assert!(
            validate(&DimensionOptions {
                features: vec![excluded],
                requirements: vec![]
            })
            .is_err()
        );
        assert!(
            validate(&DimensionOptions {
                features: vec![bore, top],
                requirements: vec![requirement(Measure::Angle, &["bore", "top"], 90.0, 0.1)],
            })
            .is_ok()
        );
    }

    #[test]
    fn requirements_set_the_dimensional_criterion() {
        let mesh = plate_with_hole(20.0, 10.0, 4.0, 48);
        let validate = |nominal: f64| {
            analyze_mesh(
                &mesh,
                ValidationOptions {
                    dimensions: Some(DimensionOptions {
                        features: vec![feature("bore", Shape::Cylinder, [4.0, 0.3, 5.0])],
                        requirements: vec![requirement(
                            Measure::Diameter,
                            &["bore"],
                            nominal,
                            0.05,
                        )],
                    }),
                    ..ValidationOptions::default()
                },
            )
            .unwrap()
        };
        let passed = validate(8.0);
        assert!(passed.solid_geometry, "{:?}", passed.issues);
        let criterion = &passed.assessment.criteria["dimensional_requirements"];
        assert_eq!(criterion.status, CriterionStatus::Passed);
        assert_eq!(criterion.evidence, Some("/dimensions"));
        assert_eq!(
            passed.assessment.status,
            crate::AssessmentStatus::Incomplete
        );

        let failed = validate(8.5);
        assert_eq!(
            failed.assessment.criteria["dimensional_requirements"].status,
            CriterionStatus::Failed
        );
        assert_eq!(failed.assessment.status, crate::AssessmentStatus::Failed);
        assert!(
            failed
                .issues
                .iter()
                .any(|issue| issue.code == "dimensions_out_of_tolerance")
        );

        let unmeasured = analyze_mesh(&mesh, ValidationOptions::default()).unwrap();
        assert_eq!(
            unmeasured.assessment.criteria["dimensional_requirements"].status,
            CriterionStatus::Unmeasured
        );
    }
}
