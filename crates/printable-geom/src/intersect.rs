//! Exact detection of triangles that touch or cross without sharing a vertex.
//!
//! Orientation signs come from adaptive exact predicates, so a reported
//! intersection is a property of the stored coordinates rather than of
//! floating-point rounding. Triangles that share a vertex are mesh neighbours
//! and are not compared.

use std::collections::{BTreeMap, BTreeSet};

use parry3d_f64::bounding_volume::Aabb;
use parry3d_f64::math::Vector;
use parry3d_f64::partitioning::{Bvh, BvhBuildStrategy};
use robust::{Coord, Coord3D, orient2d, orient3d};

type Point = [f64; 3];

/// Candidate pairs, by overlapping bounds, examined per triangle before the
/// search gives up. Ordinary meshes need a few dozen; long fan triangles in a
/// finely divided circle can need about as many as the circle has segments.
const CANDIDATES_PER_TRIANGLE: usize = 2048;
/// An absolute bound on candidate pairs, so that no input, however large,
/// occupies the server for more than a few seconds. Printable busts need
/// about 16 per triangle, so meshes of about three million triangles fit.
const MAX_CANDIDATES: usize = 50_000_000;
/// Exact tests per triangle and in total, counted only for pairs that the
/// quick separating-axis test cannot separate.
const EXACT_TESTS_PER_TRIANGLE: usize = 256;
const MAX_EXACT_TESTS: usize = 1_000_000;
/// The search stops after confirming this many crossings within shells; the
/// mesh is already invalid and the count is reported as a lower bound.
const MAX_CROSSINGS: usize = 10_000;
/// Relative margin for the quick separating-axis test, far above
/// floating-point error.
const SEPARATION_MARGIN: f64 = 1e-9;

/// Intersecting triangle pairs, split by whether both triangles belong to the
/// same connected shell.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Intersections {
    /// Whether every candidate pair was examined. When the search budget runs
    /// out, the counts are the crossings confirmed before it stopped.
    pub complete: bool,
    pub within_shells: usize,
    pub between_shells: usize,
    /// Pairs of distinct shells that touch or cross, smaller index first.
    pub overlapping_pairs: BTreeSet<(usize, usize)>,
}

impl Intersections {
    /// Shells that touch or cross another shell.
    pub fn overlapping_shells(&self) -> BTreeSet<usize> {
        self.overlapping_pairs
            .iter()
            .flat_map(|(a, b)| [*a, *b])
            .collect()
    }

    /// For each overlapping shell, the smallest shell index among those it is
    /// joined to through overlaps; shells with the same value unite into one
    /// solid.
    pub fn overlap_groups(&self) -> BTreeMap<usize, usize> {
        let mut parent: BTreeMap<usize, usize> = BTreeMap::new();
        fn root(parent: &mut BTreeMap<usize, usize>, shell: usize) -> usize {
            let mut current = shell;
            while let Some(&next) = parent.get(&current) {
                if next == current {
                    break;
                }
                current = next;
            }
            parent.insert(shell, current);
            current
        }
        for &(a, b) in &self.overlapping_pairs {
            parent.entry(a).or_insert(a);
            parent.entry(b).or_insert(b);
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            parent.insert(ra.max(rb), ra.min(rb));
        }
        let shells: Vec<usize> = parent.keys().copied().collect();
        shells
            .into_iter()
            .map(|shell| (shell, root(&mut parent, shell)))
            .collect()
    }
}

/// Compare every pair of non-degenerate triangles whose bounds overlap and
/// that share no vertex. `shell_of` gives each triangle's connected shell.
/// The search stops once the candidate pairs exceed its budget.
pub(crate) fn find_intersections(
    vertices: &[Point],
    triangles: &[[u32; 3]],
    shell_of: &[usize],
) -> Intersections {
    let mut candidates = CANDIDATES_PER_TRIANGLE
        .saturating_mul(triangles.len())
        .min(MAX_CANDIDATES);
    let mut exact_tests = EXACT_TESTS_PER_TRIANGLE
        .saturating_mul(triangles.len())
        .min(MAX_EXACT_TESTS);
    let corners = |triangle: &[u32; 3]| triangle.map(|index| vertices[index as usize]);
    let bounds: Vec<Aabb> = triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = corners(triangle);
            let low = std::array::from_fn(|axis| a[axis].min(b[axis]).min(c[axis]));
            let high = std::array::from_fn(|axis| a[axis].max(b[axis]).max(c[axis]));
            Aabb::new(Vector::from_array(low), Vector::from_array(high))
        })
        .collect();
    let degenerate: Vec<bool> = triangles
        .iter()
        .map(|triangle| is_degenerate(corners(triangle)))
        .collect();
    let bvh = Bvh::from_leaves(BvhBuildStrategy::default(), &bounds);
    let mut found = Intersections {
        complete: true,
        ..Intersections::default()
    };
    for (first, triangle) in triangles.iter().enumerate() {
        if degenerate[first] {
            continue;
        }
        for second in bvh.intersect_aabb(&bounds[first]) {
            let Some(remaining) = candidates.checked_sub(1) else {
                found.complete = false;
                return found;
            };
            candidates = remaining;
            let second = second as usize;
            if second <= first || degenerate[second] {
                continue;
            }
            let other = &triangles[second];
            if triangle.iter().any(|index| other.contains(index)) {
                continue;
            }
            let (this, that) = (corners(triangle), corners(other));
            if clearly_separated(this, that) {
                continue;
            }
            let Some(remaining) = exact_tests.checked_sub(1) else {
                found.complete = false;
                return found;
            };
            exact_tests = remaining;
            if triangles_intersect(this, that) {
                let (a, b) = (shell_of[first], shell_of[second]);
                if a == b {
                    found.within_shells += 1;
                    if found.within_shells >= MAX_CROSSINGS {
                        found.complete = false;
                        return found;
                    }
                } else {
                    found.between_shells += 1;
                    found.overlapping_pairs.insert((a.min(b), a.max(b)));
                }
            }
        }
    }
    found
}

/// Whether a separating axis keeps the triangles apart by a margin that
/// floating-point error cannot cross. The axes are both normals, the cross
/// products of edge pairs, and each edge's in-plane normal, which together
/// separate any two disjoint triangles, including coplanar ones.
fn clearly_separated(first: [Point; 3], second: [Point; 3]) -> bool {
    let edges = |[a, b, c]: [Point; 3]| [sub(b, a), sub(c, b), sub(a, c)];
    let (first_edges, second_edges) = (edges(first), edges(second));
    let first_normal = cross(first_edges[0], sub(first[2], first[0]));
    let second_normal = cross(second_edges[0], sub(second[2], second[0]));
    let origin = first[0];
    let reach = first
        .iter()
        .chain(&second)
        .map(|point| dot(sub(*point, origin), sub(*point, origin)))
        .fold(0.0, f64::max)
        .sqrt();
    let separates = |axis: Point| {
        let length = dot(axis, axis).sqrt();
        if length == 0.0 {
            return false;
        }
        let project = |points: [Point; 3]| {
            let values = points.map(|point| dot(axis, sub(point, origin)));
            (
                values[0].min(values[1]).min(values[2]),
                values[0].max(values[1]).max(values[2]),
            )
        };
        let ((low, high), (other_low, other_high)) = (project(first), project(second));
        let margin = SEPARATION_MARGIN * length * reach;
        high + margin < other_low || other_high + margin < low
    };
    // The normals separate most pairs, so they are tried first.
    separates(first_normal)
        || separates(second_normal)
        || first_edges.iter().any(|edge| {
            separates(cross(first_normal, *edge))
                || second_edges
                    .iter()
                    .any(|other| separates(cross(*edge, *other)))
        })
        || second_edges
            .iter()
            .any(|edge| separates(cross(second_normal, *edge)))
}

fn sub(p: Point, q: Point) -> Point {
    [p[0] - q[0], p[1] - q[1], p[2] - q[2]]
}

fn cross(u: Point, v: Point) -> Point {
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

fn dot(p: Point, q: Point) -> f64 {
    p[0] * q[0] + p[1] * q[1] + p[2] * q[2]
}

fn coord3(point: Point) -> Coord3D<f64> {
    Coord3D {
        x: point[0],
        y: point[1],
        z: point[2],
    }
}

fn orient(a: Point, b: Point, c: Point, d: Point) -> f64 {
    orient3d(coord3(a), coord3(b), coord3(c), coord3(d))
}

/// A triangle is degenerate when its corners are collinear in every
/// coordinate projection.
fn is_degenerate([a, b, c]: [Point; 3]) -> bool {
    (0..3).all(|dropped| {
        orient2(
            project(a, dropped),
            project(b, dropped),
            project(c, dropped),
        ) == 0.0
    })
}

/// Whether two closed, non-degenerate triangles share at least one point.
fn triangles_intersect(first: [Point; 3], second: [Point; 3]) -> bool {
    let [a, b, c] = first;
    let sides_of_second = second.map(|point| orient(a, b, c, point));
    if strictly_one_side(sides_of_second) {
        return false;
    }
    let [p, q, r] = second;
    if strictly_one_side(first.map(|point| orient(p, q, r, point))) {
        return false;
    }
    if sides_of_second.iter().all(|side| *side == 0.0) {
        let dropped = dominant_axis(first);
        return triangles_overlap_2d(
            first.map(|point| project(point, dropped)),
            second.map(|point| project(point, dropped)),
        );
    }
    edges(second)
        .into_iter()
        .any(|(start, end)| segment_meets_triangle(start, end, first))
        || edges(first)
            .into_iter()
            .any(|(start, end)| segment_meets_triangle(start, end, second))
}

fn strictly_one_side(sides: [f64; 3]) -> bool {
    sides.iter().all(|side| *side > 0.0) || sides.iter().all(|side| *side < 0.0)
}

fn edges([a, b, c]: [Point; 3]) -> [(Point, Point); 3] {
    [(a, b), (b, c), (c, a)]
}

/// Whether the closed segment meets the closed triangle.
fn segment_meets_triangle(start: Point, end: Point, triangle: [Point; 3]) -> bool {
    let [a, b, c] = triangle;
    let (from, to) = (orient(a, b, c, start), orient(a, b, c, end));
    if from * to > 0.0 {
        return false;
    }
    if from == 0.0 && to == 0.0 {
        let dropped = dominant_axis(triangle);
        return segment_meets_triangle_2d(
            project(start, dropped),
            project(end, dropped),
            triangle.map(|point| project(point, dropped)),
        );
    }
    let sides = [
        orient(start, end, a, b),
        orient(start, end, b, c),
        orient(start, end, c, a),
    ];
    sides.iter().all(|side| *side >= 0.0) || sides.iter().all(|side| *side <= 0.0)
}

/// The coordinate axis most nearly perpendicular to the triangle. Dropping
/// it projects the triangle without collapsing it, and the projection itself
/// is exact.
fn dominant_axis([a, b, c]: [Point; 3]) -> usize {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let normal = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    (0..3)
        .max_by(|left, right| normal[*left].abs().total_cmp(&normal[*right].abs()))
        .unwrap_or(2)
}

type Point2 = [f64; 2];

fn project(point: Point, dropped: usize) -> Point2 {
    match dropped {
        0 => [point[1], point[2]],
        1 => [point[0], point[2]],
        _ => [point[0], point[1]],
    }
}

fn orient2(a: Point2, b: Point2, c: Point2) -> f64 {
    orient2d(
        Coord { x: a[0], y: a[1] },
        Coord { x: b[0], y: b[1] },
        Coord { x: c[0], y: c[1] },
    )
}

fn triangles_overlap_2d(first: [Point2; 3], second: [Point2; 3]) -> bool {
    let edges = |[a, b, c]: [Point2; 3]| [(a, b), (b, c), (c, a)];
    edges(first).into_iter().any(|(start, end)| {
        edges(second)
            .into_iter()
            .any(|(from, to)| segments_meet_2d(start, end, from, to))
    }) || point_in_triangle_2d(first[0], second)
        || point_in_triangle_2d(second[0], first)
}

fn segment_meets_triangle_2d(start: Point2, end: Point2, triangle: [Point2; 3]) -> bool {
    let [a, b, c] = triangle;
    point_in_triangle_2d(start, triangle)
        || [(a, b), (b, c), (c, a)]
            .into_iter()
            .any(|(from, to)| segments_meet_2d(start, end, from, to))
}

fn point_in_triangle_2d(point: Point2, [a, b, c]: [Point2; 3]) -> bool {
    let sides = [
        orient2(a, b, point),
        orient2(b, c, point),
        orient2(c, a, point),
    ];
    sides.iter().all(|side| *side >= 0.0) || sides.iter().all(|side| *side <= 0.0)
}

fn segments_meet_2d(a: Point2, b: Point2, c: Point2, d: Point2) -> bool {
    let (d1, d2) = (orient2(a, b, c), orient2(a, b, d));
    let (d3, d4) = (orient2(c, d, a), orient2(c, d, b));
    if d1 * d2 < 0.0 && d3 * d4 < 0.0 {
        return true;
    }
    let within = |p: Point2, q: Point2, r: Point2| {
        (0..2).all(|axis| p[axis].min(q[axis]) <= r[axis] && r[axis] <= p[axis].max(q[axis]))
    };
    (d1 == 0.0 && within(a, b, c))
        || (d2 == 0.0 && within(a, b, d))
        || (d3 == 0.0 && within(c, d, a))
        || (d4 == 0.0 && within(c, d, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT: [Point; 3] = [[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [0.0, 4.0, 0.0]];

    #[test]
    fn crossing_touching_and_separate_triangles_are_classified() {
        let crossing = [[1.0, 1.0, -1.0], [1.0, 1.0, 1.0], [2.0, 2.0, 1.0]];
        assert!(triangles_intersect(FLAT, crossing));
        let touching = [[1.0, 1.0, 0.0], [1.0, 1.0, 2.0], [2.0, 1.0, 2.0]];
        assert!(triangles_intersect(FLAT, touching));
        let above = [[1.0, 1.0, 0.5], [1.0, 1.0, 2.0], [2.0, 1.0, 2.0]];
        assert!(!triangles_intersect(FLAT, above));
        let beside = [[5.0, 5.0, -1.0], [5.0, 5.0, 1.0], [6.0, 6.0, 1.0]];
        assert!(!triangles_intersect(FLAT, beside));
    }

    #[test]
    fn coplanar_triangles_overlap_only_when_they_share_area_or_boundary() {
        let overlapping = [[1.0, 1.0, 0.0], [5.0, 1.0, 0.0], [1.0, 5.0, 0.0]];
        assert!(triangles_intersect(FLAT, overlapping));
        let inside = [[0.5, 0.5, 0.0], [1.0, 0.5, 0.0], [0.5, 1.0, 0.0]];
        assert!(triangles_intersect(FLAT, inside));
        let apart = [[3.0, 3.0, 0.0], [5.0, 3.0, 0.0], [3.0, 5.0, 0.0]];
        assert!(!triangles_intersect(FLAT, apart));
    }

    #[test]
    fn an_edge_lying_in_the_other_plane_is_tested_in_that_plane() {
        let along = [[1.0, 1.0, 0.0], [2.0, 1.0, 0.0], [1.0, 1.0, 3.0]];
        assert!(triangles_intersect(FLAT, along));
        let outside = [[5.0, 1.0, 0.0], [6.0, 1.0, 0.0], [5.0, 1.0, 3.0]];
        assert!(!triangles_intersect(FLAT, outside));
    }

    #[test]
    fn a_cone_with_long_fan_triangles_is_checked_completely() {
        // Every triangle's bounds contain the centre of the base.
        let segments = 600;
        let mut vertices = vec![[0.0, 0.0, 10.0], [0.0, 0.0, 0.0]];
        vertices.extend((0..segments).map(|index| {
            let angle = std::f64::consts::TAU * index as f64 / segments as f64;
            [5.0 * angle.cos(), 5.0 * angle.sin(), 0.0]
        }));
        let ring = |index: usize| (2 + index % segments) as u32;
        let triangles: Vec<[u32; 3]> = (0..segments)
            .flat_map(|index| {
                [
                    [ring(index), ring(index + 1), 0],
                    [1, ring(index + 1), ring(index)],
                ]
            })
            .collect();
        let found = find_intersections(&vertices, &triangles, &vec![0; triangles.len()]);
        assert!(found.complete);
        assert_eq!(found.within_shells, 0);
    }

    #[test]
    fn heavily_overlapping_input_stops_at_the_search_budget() {
        // Many copies of one triangle, each shifted slightly, all overlap.
        let count = 4 * EXACT_TESTS_PER_TRIANGLE;
        let vertices: Vec<Point> = (0..count)
            .flat_map(|copy| {
                let shift = copy as f64 * 1e-3;
                [
                    [shift, 0.0, 0.0],
                    [4.0 + shift, 0.0, 0.0],
                    [shift, 4.0, 0.0],
                ]
            })
            .collect();
        let triangles: Vec<[u32; 3]> = (0..count as u32)
            .map(|copy| [3 * copy, 3 * copy + 1, 3 * copy + 2])
            .collect();
        let found = find_intersections(&vertices, &triangles, &vec![0; count]);
        assert!(!found.complete);
        // Crossings confirmed before the search stopped are kept.
        assert!(found.within_shells > 0);
    }

    #[test]
    fn neighbours_are_skipped_and_shells_are_distinguished() {
        let vertices = [
            [0.0, 0.0, 0.0],
            [4.0, 0.0, 0.0],
            [0.0, 4.0, 0.0],
            [4.0, 4.0, 0.0],
            [1.0, 1.0, -1.0],
            [1.0, 1.0, 1.0],
            [2.0, 2.0, 1.0],
        ];
        // Triangles 0 and 1 share an edge; triangle 2 crosses triangle 0.
        let triangles = [[0, 1, 2], [1, 3, 2], [4, 5, 6]];
        let same = find_intersections(&vertices, &triangles, &[0, 0, 0]);
        assert!(same.complete);
        assert_eq!((same.within_shells, same.between_shells), (1, 0));
        let separate = find_intersections(&vertices, &triangles, &[0, 0, 1]);
        assert_eq!((separate.within_shells, separate.between_shells), (0, 1));
    }
}
