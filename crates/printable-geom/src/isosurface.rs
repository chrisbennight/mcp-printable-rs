//! Marching tetrahedra over a cubic grid.
//!
//! Every cube is split into the same six tetrahedra around its main
//! diagonal, so neighbouring cubes agree on shared faces. Each output vertex
//! lies on a grid edge whose ends have opposite signs, and each output
//! triangle lies inside one tetrahedron. The result is therefore a closed,
//! consistently oriented surface that never touches or crosses itself,
//! provided no sample is exactly zero. This holds wherever along its edge
//! each vertex is placed, so an exact crossing can replace the linear
//! estimate from the two samples.

use std::collections::HashMap;

type Point = [f64; 3];

/// Corner offsets of a cube, indexed by bit: x is 1, y is 2 and z is 4.
const CORNERS: [[usize; 3]; 8] = [
    [0, 0, 0],
    [1, 0, 0],
    [0, 1, 0],
    [1, 1, 0],
    [0, 0, 1],
    [1, 0, 1],
    [0, 1, 1],
    [1, 1, 1],
];

/// The six tetrahedra sharing the diagonal from corner 0 to corner 7, one for
/// each order in which the axes are stepped.
const TETRAHEDRA: [[usize; 4]; 6] = [
    [0, 1, 3, 7],
    [0, 1, 5, 7],
    [0, 2, 3, 7],
    [0, 2, 6, 7],
    [0, 4, 5, 7],
    [0, 4, 6, 7],
];

/// Crossing points stay this fraction of an edge away from its ends, so that
/// vertices remain distinct after rounding to single precision.
pub(crate) const END_CLEARANCE: f64 = 0.01;

/// Where the surface crosses the segment from an outside sample to an inside
/// one, as a fraction of the way along it, when it can be found exactly.
pub(crate) type Crossing<'a> = &'a dyn Fn(Point, Point) -> Option<f64>;

/// The surface where `field` changes sign, positive inside, sampled every
/// `spacing` over the box from `low` to `high`. The field must be negative on
/// the faces of the box. Vertices are placed by `crossing` when it gives a
/// position, and by interpolating the two samples otherwise.
pub(crate) fn extract<F>(
    field: &F,
    low: Point,
    high: Point,
    spacing: f64,
    threads: usize,
    crossing: Option<Crossing<'_>>,
) -> (Vec<Point>, Vec<[u32; 3]>)
where
    F: Fn(Point) -> f64 + Sync,
{
    let size: [usize; 3] =
        std::array::from_fn(|axis| ((high[axis] - low[axis]) / spacing).ceil() as usize + 1);
    let position = |index: [usize; 3]| -> Point {
        std::array::from_fn(|axis| low[axis] + index[axis] as f64 * spacing)
    };
    let layer = |z: usize| sample_layer(field, size, z, &position, threads);

    let mut vertices: Vec<Point> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    let mut edges: HashMap<u64, u32> = HashMap::new();
    let mut below = layer(0);
    for z in 0..size[2] - 1 {
        let above = layer(z + 1);
        let value = |[x, y, dz]: [usize; 3]| {
            let values = if dz == 0 { &below } else { &above };
            values[y * size[0] + x]
        };
        for y in 0..size[1] - 1 {
            for x in 0..size[0] - 1 {
                let corner_values: [f64; 8] = std::array::from_fn(|corner| {
                    let [dx, dy, dz] = CORNERS[corner];
                    value([x + dx, y + dy, dz])
                });
                if corner_values.iter().all(|value| *value > 0.0)
                    || corner_values.iter().all(|value| *value <= 0.0)
                {
                    continue;
                }
                for tetrahedron in TETRAHEDRA {
                    let mut vertex_of = |a: usize, b: usize| -> u32 {
                        let (low_corner, high_corner) = if a < b { (a, b) } else { (b, a) };
                        let origin = CORNERS[low_corner];
                        let index = [x + origin[0], y + origin[1], z + origin[2]];
                        let key = ((index[2] * size[1] + index[1]) * size[0] + index[0]) as u64 * 8
                            + (high_corner - low_corner) as u64;
                        *edges.entry(key).or_insert_with(|| {
                            let (va, vb) = (corner_values[low_corner], corner_values[high_corner]);
                            let corner = |c: usize| {
                                let offset = CORNERS[c];
                                position([x + offset[0], y + offset[1], z + offset[2]])
                            };
                            let (pa, pb) = (corner(low_corner), corner(high_corner));
                            // Fraction of the way from `pa` to `pb`.
                            let exact = crossing.and_then(|crossing| {
                                if va > 0.0 {
                                    crossing(pb, pa).map(|fraction| 1.0 - fraction)
                                } else {
                                    crossing(pa, pb)
                                }
                            });
                            let t = exact
                                .filter(|fraction| fraction.is_finite())
                                .unwrap_or(va / (va - vb))
                                .clamp(END_CLEARANCE, 1.0 - END_CLEARANCE);
                            vertices.push(std::array::from_fn(|axis| {
                                pa[axis] + t * (pb[axis] - pa[axis])
                            }));
                            (vertices.len() - 1) as u32
                        })
                    };
                    emit(tetrahedron, &corner_values, &mut vertex_of, &mut triangles);
                }
            }
        }
        below = above;
    }
    (vertices, triangles)
}

/// Samples one z layer, splitting its rows between threads.
fn sample_layer<F>(
    field: &F,
    size: [usize; 3],
    z: usize,
    position: &(impl Fn([usize; 3]) -> Point + Sync),
    threads: usize,
) -> Vec<f64>
where
    F: Fn(Point) -> f64 + Sync,
{
    let mut values = vec![0.0; size[0] * size[1]];
    let rows_per_thread = size[1].div_ceil(threads.max(1));
    std::thread::scope(|scope| {
        for (chunk, rows) in values.chunks_mut(rows_per_thread * size[0]).enumerate() {
            scope.spawn(move || {
                for (offset, value) in rows.iter_mut().enumerate() {
                    let y = chunk * rows_per_thread + offset / size[0];
                    let x = offset % size[0];
                    *value = field(position([x, y, z]));
                }
            });
        }
    });
    values
}

/// Triangles separating the inside corners of one tetrahedron from the
/// outside ones, facing outward.
fn emit(
    tetrahedron: [usize; 4],
    values: &[f64; 8],
    vertex_of: &mut impl FnMut(usize, usize) -> u32,
    triangles: &mut Vec<[u32; 3]>,
) {
    let (inside, outside): (Vec<usize>, Vec<usize>) = tetrahedron
        .iter()
        .partition(|corner| values[**corner] > 0.0);
    // Each triangle is listed by the grid edges its vertices lie on.
    let faces: Vec<[(usize, usize); 3]> = match (inside.as_slice(), outside.as_slice()) {
        ([i], [a, b, c]) => vec![[(*i, *a), (*i, *b), (*i, *c)]],
        ([a, b, c], [o]) => vec![[(*a, *o), (*b, *o), (*c, *o)]],
        ([i, j], [o, p]) => vec![
            [(*i, *o), (*i, *p), (*j, *p)],
            [(*i, *o), (*j, *p), (*j, *o)],
        ],
        _ => return,
    };
    let reference = inside[0];
    for face in faces {
        // Orientation is decided exactly from edge midpoints in doubled grid
        // units. Moving each vertex along its edge cannot flip it, because no
        // triangle can pass through a corner of its tetrahedron.
        let midpoint = |(a, b): (usize, usize)| -> [i64; 3] {
            std::array::from_fn(|axis| (CORNERS[a][axis] + CORNERS[b][axis]) as i64)
        };
        let [p, q, r] = face.map(midpoint);
        let corner: [i64; 3] = CORNERS[reference].map(|value| 2 * value as i64);
        let outward = side(p, q, r, corner) < 0;
        let [a, b, c] = face.map(|(x, y)| vertex_of(x, y));
        triangles.push(if outward { [a, b, c] } else { [a, c, b] });
    }
}

/// Positive when `point` is on the side of triangle (a, b, c) that its
/// right-hand normal points to.
fn side(a: [i64; 3], b: [i64; 3], c: [i64; 3], point: [i64; 3]) -> i64 {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let w = [point[0] - a[0], point[1] - a[1], point[2] - a[2]];
    let normal = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    normal[0] * w[0] + normal[1] * w[1] + normal[2] * w[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifold_csg::Manifold;

    #[test]
    fn a_sphere_becomes_a_closed_outward_facing_surface() {
        let sphere = |point: Point| {
            let offset = 1e-7; // keeps every sample off zero
            5.0 - (point[0] * point[0] + point[1] * point[1] + point[2] * point[2]).sqrt() + offset
        };
        let (vertices, triangles) = extract(&sphere, [-7.0; 3], [7.0; 3], 0.5, 3, None);
        let flat: Vec<f64> = vertices.iter().flatten().copied().collect();
        let indices: Vec<u64> = triangles
            .iter()
            .flatten()
            .map(|&index| u64::from(index))
            .collect();
        let solid = Manifold::from_mesh_f64(&flat, 3, &indices).expect("closed manifold");
        let expected = 4.0 / 3.0 * std::f64::consts::PI * 125.0;
        assert!(
            (solid.volume() - expected).abs() < 0.02 * expected,
            "{}",
            solid.volume()
        );
        assert_eq!(solid.genus(), 0);
        let found =
            crate::intersect::find_intersections(&vertices, &triangles, &vec![0; triangles.len()]);
        assert!(found.complete);
        assert_eq!(found.within_shells, 0);
    }
}
