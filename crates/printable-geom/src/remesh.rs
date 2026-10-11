//! Rebuild the union of closed, possibly self-intersecting surfaces as one
//! valid solid.
//!
//! The solid is the set of points with a positive winding count: an upward
//! ray from the point crosses more outward-facing surface going up than
//! inward-facing. The winding count is exact, so overlapping parts and
//! surfaces that pass through themselves give the union. A signed distance
//! built from that sign and the distance to the nearest triangle is meshed by
//! the kernel's level-set extraction, which always produces a manifold.

use std::sync::atomic::{AtomicU8, Ordering};

use manifold_csg::Manifold;
use parry3d_f64::math::Vector;
use parry3d_f64::query::PointQuery;
use parry3d_f64::shape::TriMesh;
use robust::{Coord, Coord3D, orient2d, orient3d};

use crate::GeometryError;

type Point = [f64; 3];

/// Exact distances are computed within this many output edge lengths of the
/// surface; farther away only the sign matters.
const BAND_EDGES: f64 = 2.0;

/// The smallest distance magnitude returned, as a fraction of the edge
/// length. The extracted surface moves by at most this much.
const FLOOR_FRACTION: f64 = 1e-4;

const UNKNOWN: u8 = 0;
const NEAR: u8 = 1;
const INSIDE: u8 = 2;
const OUTSIDE: u8 = 3;

/// Level-set extraction yields edges about this fraction of its grid spacing.
const EDGE_PER_SPACING: f64 = 0.6;

/// Mesh the union of the closed surfaces in `triangles` with edges of about
/// `edge` millimetres.
pub(crate) fn remesh_union(
    vertices: Vec<Point>,
    triangles: Vec<[u32; 3]>,
    edge: f64,
) -> Result<Manifold, GeometryError> {
    let edge = edge / EDGE_PER_SPACING;
    let field = Field::new(vertices, triangles, edge)?;
    let margin = 2.0 * edge;
    let low = field.low.map(|value| value - margin);
    let high = field.high.map(|value| value + margin);
    let solid = Manifold::from_sdf(
        |x, y, z| field.signed_distance([x, y, z]),
        (low, high),
        edge,
        0.0,
        // Interpolate crossings between grid samples; the distance field is
        // close to linear near the surface.
        -1.0,
    );
    solid
        .status()
        .map_err(|error| GeometryError::Kernel(format!("level-set remeshing failed: {error}")))?;
    if solid.is_empty() {
        return Err(GeometryError::Kernel(
            "level-set remeshing produced no solid".into(),
        ));
    }
    Ok(solid)
}

struct Field {
    surface: TriMesh,
    low: Point,
    high: Point,
    band: f64,
    floor: f64,
    columns: Columns,
    voxels: Voxels,
}

impl Field {
    fn new(
        vertices: Vec<Point>,
        triangles: Vec<[u32; 3]>,
        edge: f64,
    ) -> Result<Self, GeometryError> {
        // The conversion reuses the vertex allocation; the triangle mesh is
        // the only copy of the surface kept.
        let surface = TriMesh::new(
            vertices.into_iter().map(Vector::from_array).collect(),
            triangles,
        )
        .map_err(|error| GeometryError::Kernel(format!("remeshing input is invalid: {error}")))?;
        let vertices = surface.vertices();
        let triangles = surface.indices();
        let mut low = [f64::INFINITY; 3];
        let mut high = [f64::NEG_INFINITY; 3];
        for vertex in vertices {
            for axis in 0..3 {
                low[axis] = low[axis].min(vertex[axis]);
                high[axis] = high[axis].max(vertex[axis]);
            }
        }
        let band = BAND_EDGES * edge;
        let columns = Columns::new(vertices, triangles, low, high, edge);
        let voxels = Voxels::new(vertices, triangles, low, high, band)?;
        Ok(Self {
            surface,
            low,
            high,
            band,
            floor: edge * FLOOR_FRACTION,
            columns,
            voxels,
        })
    }

    /// Positive inside, negative outside, clamped to the band.
    fn signed_distance(&self, point: Point) -> f64 {
        let voxel = self.voxels.index(point);
        let state = voxel.map_or(OUTSIDE, |index| {
            self.voxels.state[index].load(Ordering::Relaxed)
        });
        match state {
            INSIDE => return self.band,
            OUTSIDE => return -self.band,
            UNKNOWN => {
                // No surface reaches this voxel, so its sign is uniform.
                let index = voxel.expect("unknown voxels are inside the grid");
                let inside = self.winding(self.voxels.centre(index)) > 0;
                self.voxels.state[index]
                    .store(if inside { INSIDE } else { OUTSIDE }, Ordering::Relaxed);
                return if inside { self.band } else { -self.band };
            }
            _ => {}
        }
        let distance = self
            .surface
            .project_local_point_with_max_dist(Vector::from_array(point), false, self.band)
            .map_or(self.band, |projection| {
                (projection.point - Vector::from_array(point)).length()
            });
        // Surfaces buried inside the union have zero distance but no change of
        // sign; a small floor keeps them off the zero level.
        let distance = distance.max(self.floor);
        if self.winding(point) > 0 {
            distance
        } else {
            -distance
        }
    }

    /// Outward-facing minus inward-facing crossings of the ray from `point`
    /// along +z.
    fn winding(&self, point: Point) -> i32 {
        let mut winding = 0;
        for &triangle in self.columns.candidates(point) {
            let [a, b, c] = self.surface.indices()[triangle as usize]
                .map(|index| self.surface.vertices()[index as usize].to_array());
            let facing = orient2(a, b, c);
            if facing == 0.0 {
                continue;
            }
            // Order the corners counterclockwise in projection.
            let (a, b, c) = if facing > 0.0 { (a, b, c) } else { (a, c, b) };
            if !covers(a, b, point) || !covers(b, c, point) || !covers(c, a, point) {
                continue;
            }
            // Positive when `point` lies below the plane of the reordered corners.
            if orient3d(coord3(a), coord3(b), coord3(c), coord3(point)) > 0.0 {
                winding += if facing > 0.0 { 1 } else { -1 };
            }
        }
        winding
    }
}

/// Whether `point` projects onto the closed side of the counterclockwise edge
/// from `from` to `to`, with each boundary point owned by exactly one of the
/// two triangles sharing the edge.
fn covers(from: Point, to: Point, point: Point) -> bool {
    let side = orient2d(
        Coord {
            x: from[0],
            y: from[1],
        },
        Coord { x: to[0], y: to[1] },
        Coord {
            x: point[0],
            y: point[1],
        },
    );
    side > 0.0 || (side == 0.0 && (to[1] < from[1] || (to[1] == from[1] && to[0] > from[0])))
}

fn orient2(a: Point, b: Point, c: Point) -> f64 {
    orient2d(
        Coord { x: a[0], y: a[1] },
        Coord { x: b[0], y: b[1] },
        Coord { x: c[0], y: c[1] },
    )
}

fn coord3(point: Point) -> Coord3D<f64> {
    Coord3D {
        x: point[0],
        y: point[1],
        z: point[2],
    }
}

/// Triangles bucketed by the grid cells their projection onto the xy plane
/// overlaps.
struct Columns {
    low: [f64; 2],
    cell: f64,
    size: [usize; 2],
    starts: Vec<u32>,
    entries: Vec<u32>,
}

impl Columns {
    fn new(
        vertices: &[Vector],
        triangles: &[[u32; 3]],
        low: Point,
        high: Point,
        cell: f64,
    ) -> Self {
        let size = [0, 1].map(|axis| ((high[axis] - low[axis]) / cell).floor() as usize + 1);
        let mut columns = Self {
            low: [low[0], low[1]],
            cell,
            size,
            starts: vec![0; size[0] * size[1] + 1],
            entries: Vec::new(),
        };
        let spans: Vec<_> = triangles
            .iter()
            .map(|triangle| {
                let corners = triangle.map(|index| vertices[index as usize]);
                let span = |axis: usize| {
                    let low = corners
                        .iter()
                        .map(|corner| corner[axis])
                        .fold(f64::INFINITY, f64::min);
                    let high = corners
                        .iter()
                        .map(|corner| corner[axis])
                        .fold(f64::NEG_INFINITY, f64::max);
                    (columns.cell_of(axis, low), columns.cell_of(axis, high))
                };
                (span(0), span(1))
            })
            .collect();
        for ((x0, x1), (y0, y1)) in &spans {
            for y in *y0..=*y1 {
                for x in *x0..=*x1 {
                    columns.starts[y * size[0] + x + 1] += 1;
                }
            }
        }
        for index in 1..columns.starts.len() {
            columns.starts[index] += columns.starts[index - 1];
        }
        let mut fill = columns.starts.clone();
        columns.entries = vec![0; *columns.starts.last().unwrap_or(&0) as usize];
        for (triangle, ((x0, x1), (y0, y1))) in spans.iter().enumerate() {
            for y in *y0..=*y1 {
                for x in *x0..=*x1 {
                    let slot = &mut fill[y * size[0] + x];
                    columns.entries[*slot as usize] = triangle as u32;
                    *slot += 1;
                }
            }
        }
        columns
    }

    fn cell_of(&self, axis: usize, value: f64) -> usize {
        (((value - self.low[axis]) / self.cell).floor().max(0.0) as usize).min(self.size[axis] - 1)
    }

    fn candidates(&self, point: Point) -> &[u32] {
        let inside = (0..2).all(|axis| {
            point[axis] >= self.low[axis]
                && point[axis] <= self.low[axis] + self.cell * self.size[axis] as f64
        });
        if !inside {
            return &[];
        }
        let cell = self.cell_of(1, point[1]) * self.size[0] + self.cell_of(0, point[0]);
        &self.entries[self.starts[cell] as usize..self.starts[cell + 1] as usize]
    }
}

/// A coarse grid recording which cells lie within the band of some triangle,
/// and caching the sign of every other cell.
struct Voxels {
    low: Point,
    cell: f64,
    size: [usize; 3],
    state: Vec<AtomicU8>,
}

impl Voxels {
    fn new(
        vertices: &[Vector],
        triangles: &[[u32; 3]],
        low: Point,
        high: Point,
        band: f64,
    ) -> Result<Self, GeometryError> {
        let cell = band;
        let low = low.map(|value| value - 4.0 * band);
        let size = [0, 1, 2]
            .map(|axis| ((high[axis] + 4.0 * band - low[axis]) / cell).ceil() as usize + 1);
        let count = size
            .iter()
            .try_fold(1usize, |total, extent| total.checked_mul(*extent))
            .ok_or_else(|| GeometryError::Kernel("remeshing grid is too large".into()))?;
        let voxels = Self {
            low,
            cell,
            size,
            state: (0..count).map(|_| AtomicU8::new(UNKNOWN)).collect(),
        };
        for triangle in triangles {
            let corners = triangle.map(|index| vertices[index as usize]);
            let range = |axis: usize| {
                let low = corners
                    .iter()
                    .map(|corner| corner[axis])
                    .fold(f64::INFINITY, f64::min)
                    - band;
                let high = corners
                    .iter()
                    .map(|corner| corner[axis])
                    .fold(f64::NEG_INFINITY, f64::max)
                    + band;
                (voxels.cell_of(axis, low), voxels.cell_of(axis, high))
            };
            let ((x0, x1), (y0, y1), (z0, z1)) = (range(0), range(1), range(2));
            for z in z0..=z1 {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        voxels.state[(z * size[1] + y) * size[0] + x]
                            .store(NEAR, Ordering::Relaxed);
                    }
                }
            }
        }
        Ok(voxels)
    }

    fn cell_of(&self, axis: usize, value: f64) -> usize {
        (((value - self.low[axis]) / self.cell).floor().max(0.0) as usize).min(self.size[axis] - 1)
    }

    fn index(&self, point: Point) -> Option<usize> {
        let mut cells = [0; 3];
        for axis in 0..3 {
            let offset = (point[axis] - self.low[axis]) / self.cell;
            if !(offset >= 0.0 && offset < self.size[axis] as f64) {
                return None;
            }
            cells[axis] = offset as usize;
        }
        Some((cells[2] * self.size[1] + cells[1]) * self.size[0] + cells[0])
    }

    fn centre(&self, index: usize) -> Point {
        let x = index % self.size[0];
        let y = (index / self.size[0]) % self.size[1];
        let z = index / (self.size[0] * self.size[1]);
        let cells = [x, y, z];
        std::array::from_fn(|axis| self.low[axis] + (cells[axis] as f64 + 0.5) * self.cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(low: Point, size: f64) -> (Vec<Point>, Vec<[u32; 3]>) {
        let vertices = (0..8)
            .map(|corner| {
                [0, 1, 2].map(|axis| low[axis] + if corner >> axis & 1 == 1 { size } else { 0.0 })
            })
            .collect();
        let triangles = vec![
            [0, 2, 1],
            [1, 2, 3],
            [4, 5, 6],
            [5, 7, 6],
            [0, 1, 4],
            [1, 5, 4],
            [2, 6, 3],
            [3, 6, 7],
            [0, 4, 2],
            [2, 4, 6],
            [1, 3, 5],
            [3, 7, 5],
        ];
        (vertices, triangles)
    }

    #[test]
    fn winding_counts_overlapping_closed_surfaces() {
        let (mut vertices, mut triangles) = cube([0.0; 3], 10.0);
        let (other, other_triangles) = cube([5.0; 3], 10.0);
        let offset = vertices.len() as u32;
        vertices.extend(other);
        triangles.extend(
            other_triangles
                .iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        let field = Field::new(vertices, triangles, 1.0).unwrap();
        assert_eq!(field.winding([2.0, 2.0, 2.0]), 1);
        assert_eq!(field.winding([7.0, 7.0, 7.0]), 2);
        assert_eq!(field.winding([12.0, 12.0, 12.0]), 1);
        assert_eq!(field.winding([12.0, 2.0, 2.0]), 0);
        // A ray through shared edges and corners is counted once.
        assert_eq!(field.winding([5.0, 5.0, 2.0]), 1);
        assert_eq!(field.winding([10.0, 10.0, 12.0]), 1);
    }

    #[test]
    fn overlapping_cubes_become_one_valid_solid() {
        let (mut vertices, mut triangles) = cube([0.0; 3], 10.0);
        let (other, other_triangles) = cube([5.0; 3], 10.0);
        let offset = vertices.len() as u32;
        vertices.extend(other);
        triangles.extend(
            other_triangles
                .iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        let solid = remesh_union(vertices, triangles, 0.5).unwrap();
        // Two 1000 mm^3 cubes overlapping in a 125 mm^3 cube.
        assert!((solid.volume() - 1875.0).abs() < 60.0, "{}", solid.volume());
        assert_eq!(solid.decompose().len(), 1);
    }
}
