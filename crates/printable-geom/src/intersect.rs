//! Exact detection of triangles that touch or cross without sharing a vertex.
//!
//! Orientation signs come from adaptive exact predicates, so a reported
//! intersection is a property of the stored coordinates rather than of
//! floating-point rounding. Triangles that share a vertex are mesh neighbours
//! and are not compared.

use std::collections::BTreeSet;

use parry3d_f64::bounding_volume::Aabb;
use parry3d_f64::math::Vector;
use parry3d_f64::partitioning::{Bvh, BvhBuildStrategy};
use robust::{Coord, Coord3D, orient2d, orient3d};

type Point = [f64; 3];

/// Intersecting triangle pairs, split by whether both triangles belong to the
/// same connected shell.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Intersections {
    pub within_shells: usize,
    pub between_shells: usize,
    /// Shells that touch or cross another shell.
    pub overlapping_shells: BTreeSet<usize>,
}

/// Compare every pair of non-degenerate triangles whose bounds overlap and
/// that share no vertex. `shell_of` gives each triangle's connected shell.
pub(crate) fn find_intersections(
    vertices: &[Point],
    triangles: &[[u32; 3]],
    shell_of: &[usize],
) -> Intersections {
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
    let mut found = Intersections::default();
    for (first, triangle) in triangles.iter().enumerate() {
        if degenerate[first] {
            continue;
        }
        for second in bvh.intersect_aabb(&bounds[first]) {
            let second = second as usize;
            if second <= first || degenerate[second] {
                continue;
            }
            let other = &triangles[second];
            if triangle.iter().any(|index| other.contains(index)) {
                continue;
            }
            if triangles_intersect(corners(triangle), corners(other)) {
                if shell_of[first] == shell_of[second] {
                    found.within_shells += 1;
                } else {
                    found.between_shells += 1;
                    found.overlapping_shells.insert(shell_of[first]);
                    found.overlapping_shells.insert(shell_of[second]);
                }
            }
        }
    }
    found
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
        assert_eq!((same.within_shells, same.between_shells), (1, 0));
        let separate = find_intersections(&vertices, &triangles, &[0, 0, 1]);
        assert_eq!((separate.within_shells, separate.between_shells), (0, 1));
    }
}
