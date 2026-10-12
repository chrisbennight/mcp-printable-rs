//! Checks of a bust's surface against what a printer can form: surface that
//! faces down more steeply than it can print without support, and parts
//! thinner than it can lay down. Each check reports the area found and its
//! largest connected regions.

use parry3d_f64::math::Vector;
use parry3d_f64::query::{Ray, RayCast};
use parry3d_f64::shape::FeatureId;
use parry3d_f64::shape::TriMesh;
use serde::{Deserialize, Serialize};

use crate::{GeometryError, TriangleMesh};

/// At most this many regions are reported for each check, largest first.
const MAX_REGIONS: usize = 5;
/// Regions smaller than this share of the check's whole area are not
/// reported on their own.
const MIN_REGION_SHARE: f64 = 0.01;
/// Faces within this distance of the lowest point, as a fraction of the
/// mesh's largest extent, rest on the build plate.
const BED_TOLERANCE: f64 = 1e-6;
/// Thickness rays start this far inside the surface, as a fraction of the
/// mesh's largest extent, so that they cannot hit the face they leave.
const RAY_START: f64 = 1e-7;

/// Print checks requested from [`crate::prepare_bust`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrintChecks {
    /// Surface facing down more steeply than this, in degrees from
    /// vertical, is reported as overhang.
    pub overhang_degrees: f64,
    /// Parts thinner than this, in millimetres, are reported. No thickness
    /// check when absent.
    #[serde(default)]
    pub min_feature_mm: Option<f64>,
}

/// Surface found by a check, in millimetres on the finished bust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceFinding {
    pub area_mm2: f64,
    /// The largest connected regions, largest first. Small regions are
    /// counted in `area_mm2` but not listed.
    pub regions: Vec<SurfaceRegion>,
}

/// One connected region of a [`SurfaceFinding`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRegion {
    pub area_mm2: f64,
    /// The centre of the region's face nearest its area-weighted centre, so
    /// the point lies on the region.
    pub centre_mm: [f64; 3],
    /// The request vertex nearest `centre_mm`, which tells which part of
    /// the head the region is on.
    pub nearest_vertex: u32,
}

/// Faces that face down more steeply than `degrees` from vertical, other
/// than those resting on the build plate at the mesh's lowest point.
pub(crate) fn overhang(
    mesh: &TriangleMesh,
    degrees: f64,
    head: &[(u32, [f64; 3])],
) -> SurfaceFinding {
    let steepest = degrees.to_radians().sin();
    let on_bed = bed_faces(mesh);
    let flagged: Vec<bool> = mesh
        .triangles
        .iter()
        .zip(&on_bed)
        .map(|(triangle, &on_bed)| {
            let points = triangle.map(|index| mesh.vertices[index as usize]);
            !on_bed && unit_normal(points).is_some_and(|normal| -normal[2] > steepest)
        })
        .collect();
    finding(mesh, &flagged, head)
}

/// Faces that face down and lie at the mesh's lowest point, resting on the
/// build plate.
fn bed_faces(mesh: &TriangleMesh) -> Vec<bool> {
    let lowest = mesh
        .vertices
        .iter()
        .map(|vertex| vertex[2])
        .fold(f64::INFINITY, f64::min);
    let tolerance = BED_TOLERANCE * extent(mesh);
    mesh.triangles
        .iter()
        .map(|triangle| {
            let points = triangle.map(|index| mesh.vertices[index as usize]);
            unit_normal(points).is_some_and(|normal| normal[2] < 0.0)
                && points.iter().all(|point| point[2] - lowest <= tolerance)
        })
        .collect()
}

/// Faces from which a ray cast straight into the solid leaves it again
/// within `min_mm`: the solid there is thinner than that. Faces lying
/// wholly at or below `base_mm`, such as a plinth's, are not checked, and a
/// ray that leaves through the base resting on the build plate does not
/// count, because the plate holds what lies above it.
pub(crate) fn thin(
    mesh: &TriangleMesh,
    min_mm: f64,
    base_mm: f64,
    head: &[(u32, [f64; 3])],
) -> Result<SurfaceFinding, GeometryError> {
    let size = extent(mesh);
    let start = RAY_START * size;
    let base = base_mm + BED_TOLERANCE * size;
    let on_bed = bed_faces(mesh);
    let surface = TriMesh::new(
        mesh.vertices
            .iter()
            .map(|vertex| Vector::from_array(*vertex))
            .collect(),
        mesh.triangles.clone(),
    )
    .map_err(|error| GeometryError::Kernel(format!("the bust surface is invalid: {error}")))?;
    let flagged: Vec<bool> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let points = triangle.map(|index| mesh.vertices[index as usize]);
            let Some(normal) = unit_normal(points) else {
                return false;
            };
            if points.iter().all(|point| point[2] <= base) {
                return false;
            }
            let centre =
                [0, 1, 2].map(|axis| points.iter().map(|point| point[axis]).sum::<f64>() / 3.0);
            let inward = Vector::from_array(normal.map(|value| -value));
            let ray = Ray::new(Vector::from_array(centre) + inward * start, inward);
            surface
                .cast_local_ray_and_get_normal(&ray, min_mm - start, false)
                .is_some_and(|hit| match hit.feature {
                    // Faces seen from behind are numbered after the others.
                    FeatureId::Face(face) => !on_bed[face as usize % on_bed.len()],
                    _ => true,
                })
        })
        .collect();
    drop(surface);
    Ok(finding(mesh, &flagged, head))
}

/// Group flagged faces into regions joined by shared vertices.
fn finding(mesh: &TriangleMesh, flagged: &[bool], head: &[(u32, [f64; 3])]) -> SurfaceFinding {
    let mut parent: Vec<usize> = (0..mesh.triangles.len()).collect();
    let mut owner = vec![usize::MAX; mesh.vertices.len()];
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        if !flagged[face] {
            continue;
        }
        for &vertex in triangle {
            let first = &mut owner[vertex as usize];
            if *first == usize::MAX {
                *first = face;
            } else {
                let (a, b) = (root(&mut parent, face), root(&mut parent, *first));
                parent[a] = b;
            }
        }
    }
    // Area and area-weighted centre of each region, keyed by its root face.
    let mut totals: Vec<(usize, f64, [f64; 3])> = Vec::new();
    let mut slot = vec![usize::MAX; mesh.triangles.len()];
    let mut area_mm2 = 0.0;
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        if !flagged[face] {
            continue;
        }
        let points = triangle.map(|index| mesh.vertices[index as usize]);
        let area = area(points);
        area_mm2 += area;
        let region = root(&mut parent, face);
        if slot[region] == usize::MAX {
            slot[region] = totals.len();
            totals.push((region, 0.0, [0.0; 3]));
        }
        let total = &mut totals[slot[region]];
        total.1 += area;
        for axis in 0..3 {
            total.2[axis] += area * centroid(points)[axis];
        }
    }
    totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    totals.retain(|(_, area, _)| *area >= MIN_REGION_SHARE * area_mm2 && *area > 0.0);
    totals.truncate(MAX_REGIONS);
    // Move each centre onto its region: to the nearest centre of its faces.
    let targets: Vec<[f64; 3]> = totals
        .iter()
        .map(|(_, area, sum)| sum.map(|value| value / area))
        .collect();
    let mut nearest = vec![(f64::INFINITY, [0.0; 3]); totals.len()];
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        if !flagged[face] {
            continue;
        }
        let region = root(&mut parent, face);
        let Some(index) = totals.iter().position(|(root, _, _)| *root == region) else {
            continue;
        };
        let centre = centroid(triangle.map(|index| mesh.vertices[index as usize]));
        let distance = squared_distance(centre, targets[index]);
        if distance < nearest[index].0 {
            nearest[index] = (distance, centre);
        }
    }
    let regions = totals
        .iter()
        .zip(nearest)
        .map(|((_, area, _), (_, centre))| SurfaceRegion {
            area_mm2: *area,
            centre_mm: centre,
            nearest_vertex: head
                .iter()
                .min_by(|a, b| {
                    squared_distance(a.1, centre).total_cmp(&squared_distance(b.1, centre))
                })
                .map_or(0, |(vertex, _)| *vertex),
        })
        .collect();
    SurfaceFinding { area_mm2, regions }
}

fn root(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

fn extent(mesh: &TriangleMesh) -> f64 {
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in &mesh.vertices {
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    (0..3)
        .map(|axis| high[axis] - low[axis])
        .fold(0.0, f64::max)
}

fn unit_normal([a, b, c]: [[f64; 3]; 3]) -> Option<[f64; 3]> {
    let normal = cross(sub(b, a), sub(c, a));
    let length = (0..3)
        .map(|axis| normal[axis] * normal[axis])
        .sum::<f64>()
        .sqrt();
    (length > 0.0).then(|| normal.map(|value| value / length))
}

fn area([a, b, c]: [[f64; 3]; 3]) -> f64 {
    let normal = cross(sub(b, a), sub(c, a));
    0.5 * (0..3)
        .map(|axis| normal[axis] * normal[axis])
        .sum::<f64>()
        .sqrt()
}

fn centroid(points: [[f64; 3]; 3]) -> [f64; 3] {
    [0, 1, 2].map(|axis| points.iter().map(|point| point[axis]).sum::<f64>() / 3.0)
}

fn squared_distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|axis| (a[axis] - b[axis]).powi(2)).sum()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An axis-aligned box from `low` to `high`, outward facing.
    fn cuboid(low: [f64; 3], high: [f64; 3]) -> TriangleMesh {
        let vertices = (0..8)
            .map(|i| {
                [0, 1, 2].map(|axis| {
                    if (i >> axis) & 1 == 1 {
                        high[axis]
                    } else {
                        low[axis]
                    }
                })
            })
            .collect();
        let quads = [
            [0, 4, 6, 2],
            [1, 3, 7, 5],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 2, 3, 1],
            [4, 5, 7, 6],
        ];
        TriangleMesh {
            vertices,
            triangles: quads
                .iter()
                .flat_map(|[a, b, c, d]| [[*a, *b, *c], [*a, *c, *d]])
                .collect(),
        }
    }

    /// A tall post with a wide, thin shelf on top: a T shape. The shelf's
    /// underside overhangs and the shelf is 1 mm thick.
    fn tee() -> TriangleMesh {
        let mut mesh = cuboid([-2.0, -2.0, 0.0], [2.0, 2.0, 20.0]);
        let shelf = cuboid([-10.0, -10.0, 20.0], [10.0, 10.0, 21.0]);
        let offset = mesh.vertices.len() as u32;
        mesh.vertices.extend(shelf.vertices);
        mesh.triangles.extend(
            shelf
                .triangles
                .iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        mesh
    }

    #[test]
    fn the_underside_of_a_shelf_overhangs_but_the_base_does_not() {
        let head = [(7, [0.0, 0.0, 20.0]), (9, [0.0, 0.0, 0.0])];
        let found = overhang(&tee(), 45.0, &head);
        // The shelf's underside, 20 by 20 mm; the post's foot rests on the bed.
        assert!((found.area_mm2 - 400.0).abs() < 1e-9, "{found:?}");
        assert_eq!(found.regions.len(), 1);
        let region = &found.regions[0];
        assert!((region.centre_mm[2] - 20.0).abs() < 1e-9, "{region:?}");
        assert_eq!(region.nearest_vertex, 7);
        // Walls are vertical, so nothing overhangs at 90 degrees.
        assert_eq!(overhang(&tee(), 90.0, &head).area_mm2, 0.0);
    }

    #[test]
    fn a_shelf_thinner_than_the_limit_is_thin() {
        // The tee's shelf is 1 mm thick, 20 mm above the plate; its post is
        // 4 mm across.
        let head = [(3, [0.0, 0.0, 21.0])];
        let found = thin(&tee(), 1.5, 0.0, &head).unwrap();
        // Its top and bottom, 20 by 20 mm each, are separate regions.
        assert!((found.area_mm2 - 800.0).abs() < 1e-9, "{found:?}");
        assert_eq!(found.regions.len(), 2);
        assert!(
            found
                .regions
                .iter()
                .all(|region| region.nearest_vertex == 3)
        );
        let thick = thin(&tee(), 0.9, 0.0, &head).unwrap();
        assert_eq!(thick.area_mm2, 0.0);
        assert!(thick.regions.is_empty());
    }

    #[test]
    fn a_slab_on_the_plate_is_held_by_it() {
        let head = [(0, [0.0; 3])];
        let slab = cuboid([0.0, 0.0, 0.0], [10.0, 10.0, 1.0]);
        // Rays from the top leave through the base resting on the plate,
        // and the base itself is left out.
        assert_eq!(thin(&slab, 1.5, 0.0, &head).unwrap().area_mm2, 0.0);
        // Unless the base is checked: its rays leave through the top.
        let base = thin(&slab, 1.5, f64::NEG_INFINITY, &head).unwrap();
        assert!((base.area_mm2 - 100.0).abs() < 1e-9, "{base:?}");
        assert_eq!(base.regions[0].centre_mm[2], 0.0);
    }

    #[test]
    fn regions_are_listed_largest_first_and_small_ones_left_out() {
        // Two separate overhanging shelves of different sizes, and a speck.
        let mut mesh = cuboid([0.0, 0.0, 10.0], [10.0, 10.0, 11.0]);
        for (low, high) in [
            ([20.0, 0.0, 10.0], [40.0, 20.0, 11.0]),
            ([50.0, 0.0, 10.0], [50.1, 0.1, 11.0]),
            ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
        ] {
            let part = cuboid(low, high);
            let offset = mesh.vertices.len() as u32;
            mesh.vertices.extend(part.vertices);
            mesh.triangles.extend(
                part.triangles
                    .iter()
                    .map(|triangle| triangle.map(|index| index + offset)),
            );
        }
        let found = overhang(&mesh, 45.0, &[(0, [0.0; 3])]);
        let areas: Vec<f64> = found.regions.iter().map(|region| region.area_mm2).collect();
        assert_eq!(areas.len(), 2, "{found:?}");
        assert!((areas[0] - 400.0).abs() < 1e-9 && (areas[1] - 100.0).abs() < 1e-9);
        assert!((found.area_mm2 - 500.01).abs() < 1e-9);
    }
}
