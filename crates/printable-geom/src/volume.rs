//! Missing and extra material between two closed meshes, counted on a grid
//! of cubic cells.
//!
//! A cell belongs to a mesh when its centre has a positive winding count, as
//! in remeshing: an upward ray from it crosses more outward-facing surface
//! than inward-facing. Each column of cells is swept once against the
//! surface crossings along it. Cells inside one mesh and not the other form
//! connected lumps of extra or missing material.

use std::collections::HashMap;

use parry3d_f64::math::Vector;
use serde::Serialize;

use crate::remesh::{Columns, covers, orient2};
use crate::{GeometryError, MeshBounds, TriangleMesh};

/// Cells in the grid, at most.
const MAX_CELLS: usize = 8_000_000;
/// Lumps with fewer cells are counted in the totals but not listed, as too
/// small to locate.
const MIN_REGION_CELLS: usize = 8;
/// Lumps listed, at most.
const MAX_REGIONS: usize = 10;
/// Triangles bucketed under each column, summed over the columns, at most:
/// this bounds the work of sweeping the columns, even for long slivers that
/// lie across many of them.
const MAX_COLUMN_ENTRIES: usize = 4 * MAX_CELLS;

const IN_MESH: u8 = 1;
const IN_REFERENCE: u8 = 2;
const COUNTED: u8 = 4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VolumeComparison {
    /// Edge of the grid's cubic cells.
    pub cell_mm: f64,
    pub cells: usize,
    pub reference_volume_mm3: f64,
    /// Inside the compared mesh and outside the reference.
    pub extra_volume_mm3: f64,
    /// Inside the reference and outside the compared mesh.
    pub missing_volume_mm3: f64,
    /// Connected lumps of at least [`MIN_REGION_CELLS`] cells.
    pub region_count: usize,
    /// The largest lumps, largest first, in the reference's frame.
    pub regions: Vec<VolumeRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Material {
    Extra,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VolumeRegion {
    pub material: Material,
    pub volume_mm3: f64,
    pub centre_mm: [f64; 3],
    pub bounds: MeshBounds,
}

/// Refuse a mesh whose inside is undefined because its faces do not close up
/// consistently. A mesh turned inside out is refused while its cells are
/// counted.
pub(crate) fn require_closed(mesh: &TriangleMesh, name: &str) -> Result<(), GeometryError> {
    // Each edge of a closed, consistently oriented surface is used once in
    // each direction.
    let mut balance: HashMap<(u32, u32), i32> = HashMap::new();
    for triangle in &mesh.triangles {
        for corner in 0..3 {
            let (from, to) = (triangle[corner], triangle[(corner + 1) % 3]);
            if from != to {
                let (key, step) = if from < to {
                    ((from, to), 1)
                } else {
                    ((to, from), -1)
                };
                *balance.entry(key).or_default() += step;
            }
        }
    }
    if balance.values().any(|&count| count != 0) {
        return Err(GeometryError::InvalidOptions(format!(
            "comparison volume needs closed meshes with consistently oriented faces; the {name} has open or flipped edges, so repair it or leave volume off"
        )));
    }
    Ok(())
}

/// Count the cells inside `mesh` and not `reference`, and the reverse, on a
/// grid over both whose cells are at least `minimum_cell` across. Both meshes
/// must be in the same frame and pass [`require_closed`].
pub(crate) fn compare(
    mesh: &TriangleMesh,
    reference: &TriangleMesh,
    minimum_cell: f64,
) -> Result<VolumeComparison, GeometryError> {
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in mesh.vertices.iter().chain(&reference.vertices) {
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    let extent: [f64; 3] = std::array::from_fn(|axis| high[axis] - low[axis]);
    let longest = extent.iter().copied().fold(0.0, f64::max);
    // No side may need more cells than the budget on its own, so each step
    // below grows the cell; and a cell need never be larger than the space,
    // which keeps every volume finite.
    let mut cell = minimum_cell
        .max((extent.iter().product::<f64>() / MAX_CELLS as f64).cbrt())
        .max(longest / MAX_CELLS as f64)
        .max(f64::MIN_POSITIVE)
        .min(longest.max(1.0));
    // Rounding each side up to whole cells can overshoot on a thin box.
    let size = loop {
        let size = extent.map(|side| ((side / cell).ceil() as usize).max(1));
        if size.iter().map(|&count| count as f64).product::<f64>() <= MAX_CELLS as f64 {
            break size;
        }
        cell *= 1.25;
    };
    let count = size[0] * size[1] * size[2];
    let centre = |axis: usize, at: usize| low[axis] + (at as f64 + 0.5) * cell;

    let mut state = vec![0u8; count];
    for (surface, flag, name) in [
        (mesh, IN_MESH, "mesh"),
        (reference, IN_REFERENCE, "reference"),
    ] {
        let vertices: Vec<Vector> = surface
            .vertices
            .iter()
            .map(|vertex| Vector::from_array(*vertex))
            .collect();
        let columns = Columns::new(
            &vertices,
            &surface.triangles,
            low,
            high,
            cell,
            MAX_COLUMN_ENTRIES,
        )
        .ok_or_else(|| {
            GeometryError::InvalidOptions(format!(
                "comparison volume cannot grid the {name}: its faces span too many cells; leave volume off"
            ))
        })?;
        let mut crossings: Vec<(f64, i32)> = Vec::new();
        for y in 0..size[1] {
            for x in 0..size[0] {
                crate::limit::check()?;
                let point = [centre(0, x), centre(1, y), 0.0];
                crossings.clear();
                for &triangle in columns.candidates(point) {
                    let [a, b, c] = surface.triangles[triangle as usize]
                        .map(|index| surface.vertices[index as usize]);
                    let facing = orient2(a, b, c);
                    if facing == 0.0 {
                        continue;
                    }
                    // Order the corners counterclockwise in projection.
                    let (a, b, c) = if facing > 0.0 { (a, b, c) } else { (a, c, b) };
                    if covers(a, b, point) && covers(b, c, point) && covers(c, a, point) {
                        let sign = if facing > 0.0 { 1 } else { -1 };
                        crossings.push((height(a, b, c, point), sign));
                    }
                }
                // Walking down the column, each crossing passed adds its sign
                // to the winding count. A cavity's inward faces bring it back
                // to zero; only faces turned inside out around material take
                // it below.
                crossings.sort_by(|a, b| b.0.total_cmp(&a.0));
                let mut above = crossings.iter().peekable();
                let mut winding = 0;
                for z in (0..size[2]).rev() {
                    let level = centre(2, z);
                    while let Some((_, sign)) = above.next_if(|(height, _)| *height > level) {
                        winding += sign;
                    }
                    if winding < 0 {
                        return Err(GeometryError::InvalidOptions(format!(
                            "comparison volume needs the {name} to face outward, but part of it is turned inside out; recalculate its normals or leave volume off"
                        )));
                    }
                    if winding > 0 {
                        state[(z * size[1] + y) * size[0] + x] |= flag;
                    }
                }
            }
        }
    }

    let cell_volume = cell.powi(3);
    let inside_reference = state.iter().filter(|&&s| s & IN_REFERENCE != 0).count();
    let extra = state.iter().filter(|&&s| s == IN_MESH).count();
    let missing = state.iter().filter(|&&s| s == IN_REFERENCE).count();

    // Lumps are cells of one material joined across faces.
    let mut regions: Vec<VolumeRegion> = Vec::new();
    let mut region_count = 0;
    let mut stack: Vec<u32> = Vec::new();
    for start in 0..count {
        let kind = state[start];
        let material = match kind {
            IN_MESH => Material::Extra,
            IN_REFERENCE => Material::Missing,
            _ => continue,
        };
        crate::limit::check()?;
        state[start] = COUNTED;
        stack.push(start as u32);
        let mut cells = 0usize;
        let mut sum = [0.0; 3];
        let mut lowest = [usize::MAX; 3];
        let mut highest = [0usize; 3];
        while let Some(index) = stack.pop() {
            let index = index as usize;
            let at = [
                index % size[0],
                index / size[0] % size[1],
                index / (size[0] * size[1]),
            ];
            cells += 1;
            for axis in 0..3 {
                sum[axis] += at[axis] as f64;
                lowest[axis] = lowest[axis].min(at[axis]);
                highest[axis] = highest[axis].max(at[axis]);
            }
            let stride = [1, size[0], size[0] * size[1]];
            for axis in 0..3 {
                let mut visit = |neighbour: usize| {
                    if state[neighbour] == kind {
                        state[neighbour] = COUNTED;
                        stack.push(neighbour as u32);
                    }
                };
                if at[axis] > 0 {
                    visit(index - stride[axis]);
                }
                if at[axis] + 1 < size[axis] {
                    visit(index + stride[axis]);
                }
            }
        }
        if cells < MIN_REGION_CELLS {
            continue;
        }
        region_count += 1;
        let minimum_mm: [f64; 3] =
            std::array::from_fn(|axis| low[axis] + lowest[axis] as f64 * cell);
        let maximum_mm: [f64; 3] =
            std::array::from_fn(|axis| low[axis] + (highest[axis] + 1) as f64 * cell);
        regions.push(VolumeRegion {
            material,
            volume_mm3: cells as f64 * cell_volume,
            centre_mm: std::array::from_fn(|axis| {
                centre(axis, 0) + sum[axis] / cells as f64 * cell
            }),
            bounds: MeshBounds {
                minimum_mm,
                maximum_mm,
                dimensions_mm: std::array::from_fn(|axis| maximum_mm[axis] - minimum_mm[axis]),
            },
        });
        // Only the largest are kept, however many small lumps there are.
        if regions.len() >= 2 * MAX_REGIONS {
            regions.sort_by(|a, b| b.volume_mm3.total_cmp(&a.volume_mm3));
            regions.truncate(MAX_REGIONS);
        }
    }
    regions.sort_by(|a, b| b.volume_mm3.total_cmp(&a.volume_mm3));
    regions.truncate(MAX_REGIONS);
    Ok(VolumeComparison {
        cell_mm: cell,
        cells: count,
        reference_volume_mm3: inside_reference as f64 * cell_volume,
        extra_volume_mm3: extra as f64 * cell_volume,
        missing_volume_mm3: missing as f64 * cell_volume,
        region_count,
        regions,
    })
}

/// The height of the plane through `a`, `b` and `c` above `point`, which the
/// triangle's projection covers.
fn height(a: [f64; 3], b: [f64; 3], c: [f64; 3], point: [f64; 3]) -> f64 {
    let [a, b, c] = [a, b, c].map(Vector::from_array);
    let normal = (b - a).cross(c - a);
    a.z - (normal.x * (point[0] - a.x) + normal.y * (point[1] - a.y)) / normal.z
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed, outward-facing box from `low` to `high`.
    fn cuboid(low: [f64; 3], high: [f64; 3]) -> TriangleMesh {
        let corner = |index: usize| {
            std::array::from_fn(|axis| {
                if (index >> axis) & 1 == 1 {
                    high[axis]
                } else {
                    low[axis]
                }
            })
        };
        TriangleMesh {
            vertices: (0..8).map(corner).collect(),
            triangles: vec![
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
            ],
        }
    }

    fn joined(parts: &[TriangleMesh]) -> TriangleMesh {
        let mut mesh = TriangleMesh {
            vertices: Vec::new(),
            triangles: Vec::new(),
        };
        for part in parts {
            let offset = mesh.vertices.len() as u32;
            mesh.vertices.extend(&part.vertices);
            mesh.triangles.extend(
                part.triangles
                    .iter()
                    .map(|triangle| triangle.map(|index| index + offset)),
            );
        }
        mesh
    }

    #[test]
    fn the_same_solid_has_no_difference() {
        let block = cuboid([0.0; 3], [20.0, 10.0, 6.0]);
        let report = compare(&block, &block, 0.1).unwrap();
        assert_eq!(report.extra_volume_mm3, 0.0);
        assert_eq!(report.missing_volume_mm3, 0.0);
        assert_eq!(report.region_count, 0);
        assert!(
            (report.reference_volume_mm3 - 1200.0).abs() < 1e-6,
            "{report:?}"
        );
    }

    #[test]
    fn a_bump_is_extra_and_a_notch_is_missing() {
        let block = cuboid([0.0; 3], [20.0, 10.0, 6.0]);
        // A 4 by 4 by 2 bump stands on top; it overlaps the block, which the
        // winding count joins into one solid.
        let bumped = joined(&[block.clone(), cuboid([3.0, 3.0, 5.0], [7.0, 7.0, 8.0])]);
        let report = compare(&bumped, &block, 0.25).unwrap();
        assert!((report.extra_volume_mm3 - 32.0).abs() < 1e-6, "{report:?}");
        assert_eq!(report.missing_volume_mm3, 0.0);
        assert_eq!(report.region_count, 1);
        let bump = &report.regions[0];
        assert_eq!(bump.material, Material::Extra);
        assert!((bump.centre_mm[0] - 5.0).abs() < 1e-6 && (bump.centre_mm[2] - 7.0).abs() < 1e-6);
        assert_eq!(bump.bounds.minimum_mm, [3.0, 3.0, 6.0]);
        assert_eq!(bump.bounds.maximum_mm, [7.0, 7.0, 8.0]);

        // Seen the other way round, the bump is material the block lacks.
        let report = compare(&block, &bumped, 0.25).unwrap();
        assert_eq!(report.extra_volume_mm3, 0.0);
        assert!(
            (report.missing_volume_mm3 - 32.0).abs() < 1e-6,
            "{report:?}"
        );
        assert_eq!(report.regions[0].material, Material::Missing);
    }

    #[test]
    fn a_lump_smaller_than_eight_cells_counts_but_is_not_listed() {
        let block = cuboid([0.0; 3], [10.0, 10.0, 10.0]);
        // One half-millimetre cell's worth of extra material.
        let speck = joined(&[block.clone(), cuboid([2.0, 2.0, 10.0], [2.5, 2.5, 10.5])]);
        let report = compare(&speck, &block, 0.5).unwrap();
        assert_eq!(report.cell_mm, 0.5);
        assert!((report.extra_volume_mm3 - 0.125).abs() < 1e-9, "{report:?}");
        assert_eq!(report.region_count, 0);
        assert!(report.regions.is_empty());
    }

    #[test]
    fn the_grid_stays_within_its_cell_budget() {
        // Whole cells along each side would overshoot the budget on this
        // thin bar at the first cell size tried.
        let bar = cuboid([0.0; 3], [1000.0, 1.0, 0.3]);
        let report = compare(&bar, &bar, 1e-4).unwrap();
        assert!(report.cells <= MAX_CELLS, "{}", report.cells);
        assert!(
            (report.reference_volume_mm3 - 300.0).abs() < 15.0,
            "{report:?}"
        );
    }

    #[test]
    fn extreme_tolerances_still_size_the_grid() {
        // A flat, double-sided sheet has no thickness to divide.
        let sheet = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 1]],
        };
        let report = compare(&sheet, &sheet, 1e-323).unwrap();
        assert!(report.cells <= MAX_CELLS, "{report:?}");
        assert_eq!(report.reference_volume_mm3, 0.0);
        // A cell never grows past the space, so the volumes stay finite.
        let block = cuboid([0.0; 3], [1.0, 1.0, 1.0]);
        let report = compare(&block, &block, 1e200).unwrap();
        assert_eq!(report.cell_mm, 1.0);
        assert_eq!(report.reference_volume_mm3, 1.0);
        assert_eq!(report.missing_volume_mm3, 0.0);
    }

    #[test]
    fn faces_lying_across_too_many_columns_are_refused() {
        let block = cuboid([0.0; 3], [10.0, 10.0, 10.0]);
        // Many faces, each lying across every column of the grid.
        let mut slivers = block.clone();
        slivers.triangles.extend([[0, 3, 7]; 1_000]);
        let error = compare(&slivers, &block, 1e-4).unwrap_err().to_string();
        assert!(error.contains("faces span too many cells"), "{error}");
    }

    #[test]
    fn open_and_inside_out_meshes_are_refused() {
        let block = cuboid([0.0; 3], [10.0, 10.0, 10.0]);
        assert!(require_closed(&block, "mesh").is_ok());
        let mut open = block.clone();
        open.triangles.pop();
        let error = require_closed(&open, "mesh").unwrap_err().to_string();
        assert!(error.contains("open or flipped edges"), "{error}");
        let mut flipped = block.clone();
        flipped.triangles[0].swap(1, 2);
        assert!(require_closed(&flipped, "mesh").is_err());
        let mut inside_out = cuboid([12.0, 0.0, 0.0], [14.0, 2.0, 2.0]);
        for triangle in &mut inside_out.triangles {
            triangle.swap(1, 2);
        }
        let error = compare(&block, &inside_out, 0.5).unwrap_err().to_string();
        assert!(error.contains("reference to face outward"), "{error}");
        // An inside-out part beside an outward one is found too, although
        // the whole mesh still encloses a positive volume.
        let mixed = joined(&[block.clone(), inside_out]);
        assert!(require_closed(&mixed, "mesh").is_ok());
        let error = compare(&mixed, &block, 0.5).unwrap_err().to_string();
        assert!(error.contains("mesh to face outward"), "{error}");
    }

    #[test]
    fn a_cavity_facing_inward_is_empty() {
        let block = cuboid([0.0; 3], [10.0, 10.0, 10.0]);
        let mut cavity = cuboid([3.0; 3], [7.0; 3]);
        for triangle in &mut cavity.triangles {
            triangle.swap(1, 2);
        }
        let hollow = joined(&[block.clone(), cavity]);
        let report = compare(&hollow, &block, 0.5).unwrap();
        assert_eq!(report.extra_volume_mm3, 0.0);
        assert!(
            (report.missing_volume_mm3 - 64.0).abs() < 1e-9,
            "{report:?}"
        );
        assert_eq!(report.regions[0].material, Material::Missing);
    }
}
