//! A watertight solid from a relief map.
//!
//! The top surface is the height field itself, one vertex per sample on a
//! regular grid and two triangles per cell, so it never overlaps itself.
//! A mirrored relief splits each cell along the other diagonal, so its top
//! is an exact reflection of the unmirrored one. Vertical walls run down
//! from every boundary vertex to the base plane Z = 0, and the bottom is a
//! coarse grid of cells whose outer sides carry the boundary vertices. Every
//! edge is therefore shared by exactly two triangles that traverse it in
//! opposite directions, and with a positive base thickness every triangle
//! has positive area: the result is a closed, consistently outward-facing
//! manifold. The top lies at least `base_mm` above the bottom and the walls
//! stand on the grid's outline, so no two faces cross: the solid is sound by
//! construction, without a pairwise self-intersection search.

use super::{ReliefError, ReliefMap, positive};
use crate::{TriangleMesh, limit};

/// Samples spanned by each bottom cell along each axis. Coarse cells keep
/// the bottom to a small fraction of the top's triangles.
const BOTTOM_STRIDE: usize = 16;

/// A closed relief solid made by [`relief_solid`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReliefSolid {
    pub(super) mesh: TriangleMesh,
    /// Samples along a row and down a column of the top surface, border
    /// included. The top's vertices come first, row by row, and its
    /// triangles come first, two per cell.
    pub(super) columns: usize,
    pub(super) rows: usize,
}

impl ReliefSolid {
    pub fn mesh(&self) -> &TriangleMesh {
        &self.mesh
    }

    pub fn into_mesh(self) -> TriangleMesh {
        self.mesh
    }

    /// The vertices of the top surface, row by row from the largest Y.
    pub(super) fn top_vertices(&self) -> &[[f64; 3]] {
        &self.mesh.vertices[..self.columns * self.rows]
    }

    /// The triangles of the top surface.
    #[cfg(test)]
    pub(super) fn top_triangles(&self) -> &[[u32; 3]] {
        &self.mesh.triangles[..2 * (self.columns - 1) * (self.rows - 1)]
    }
}

/// How a relief map becomes a solid.
#[derive(Debug, Clone, PartialEq)]
pub struct SolidOptions {
    /// Thickness of the flat base below the relief's lowest point.
    pub base_mm: f64,
    /// Width of a flat border around the relief; 0 for none.
    pub border_mm: f64,
    /// Height of the border above the relief's lowest point.
    pub border_height_mm: f64,
    /// Mirror the relief left to right, as a mould for casting needs.
    pub mirror: bool,
    /// Turn the relief upside down into a cavity, deepest where the relief
    /// is highest, with the cavity's floor `base_mm` above Z = 0.
    pub invert: bool,
}

impl Default for SolidOptions {
    fn default() -> Self {
        Self {
            base_mm: 2.0,
            border_mm: 0.0,
            border_height_mm: 0.0,
            mirror: false,
            invert: false,
        }
    }
}

impl SolidOptions {
    /// A mould for casting the relief: mirrored and inverted, so a cast
    /// taken from it reads the right way round.
    pub fn mould() -> Self {
        Self {
            mirror: true,
            invert: true,
            ..Self::default()
        }
    }
}

/// Close a relief map into a solid in millimetres: the relief faces +Z, the
/// base lies on Z = 0, and the image's top-left corner is at the largest Y
/// and smallest X, so the relief reads as the image does from above.
pub fn relief_solid(map: &ReliefMap, options: &SolidOptions) -> Result<ReliefSolid, ReliefError> {
    let invalid = |message: &str| Err(ReliefError::InvalidOptions(message.to_string()));
    if !positive(options.base_mm) {
        return invalid("base_mm must be a finite positive number");
    }
    let finite_not_negative = |value: f64| value.is_finite() && value >= 0.0;
    if !finite_not_negative(options.border_mm) || !finite_not_negative(options.border_height_mm) {
        return invalid("border_mm and border_height_mm must be finite and not negative");
    }
    if !map.spacing_mm.iter().all(|spacing| positive(*spacing)) {
        return invalid("spacing_mm must be finite positive numbers");
    }
    let grid = &map.heights_mm;
    let (width, height) = (grid.width(), grid.height());
    if width < 2 || height < 2 {
        return invalid("a relief map needs at least 2 x 2 samples");
    }
    if grid.values().iter().any(|value| !value.is_finite()) {
        return invalid("relief heights must be finite");
    }
    let lowest = grid.values().iter().copied().fold(f64::INFINITY, f64::min);

    // Sample positions along each axis, with two extra samples on each side
    // for a border: its outer edge and a point just outside the relief.
    let axis = |count: usize, spacing: f64| {
        let span = spacing * (count - 1) as f64;
        let mut positions = Vec::with_capacity(count + 4);
        let border = options.border_mm;
        if border > 0.0 {
            let gap = spacing.min(border / 2.0);
            positions.extend([0.0, border - gap]);
        }
        positions.extend((0..count).map(|index| border + spacing * index as f64));
        if border > 0.0 {
            let gap = spacing.min(border / 2.0);
            positions.extend([border + span + gap, 2.0 * border + span]);
        }
        positions
    };
    let xs = axis(width, map.spacing_mm[0]);
    // Rows run down the image, so Y falls with the row.
    let mut ys = axis(height, map.spacing_mm[1]);
    ys.reverse();
    let increasing = |positions: &[f64]| positions.windows(2).all(|pair| pair[0] < pair[1]);
    if !increasing(&xs) || !ys.windows(2).all(|pair| pair[0] > pair[1]) {
        return invalid("spacing_mm is too small to separate samples beside the border");
    }
    let pad = if options.border_mm > 0.0 { 2 } else { 0 };
    let (columns, rows) = (xs.len(), ys.len());
    let vertex_count = columns * rows
        + 2 * (columns + rows)
        + 2 * (columns / BOTTOM_STRIDE + 2) * (rows / BOTTOM_STRIDE + 2);
    if u32::try_from(vertex_count).is_err() {
        return Err(crate::GeometryError::TooManyVertices.into());
    }

    let mut top = vec![options.border_height_mm; columns * rows];
    for y in 0..height {
        limit::check()?;
        for x in 0..width {
            let column = if options.mirror { width - 1 - x } else { x };
            top[(y + pad) * columns + column + pad] = grid.get(x, y) - lowest;
        }
    }
    if options.invert {
        let highest = top.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        for value in &mut top {
            *value = highest - *value;
        }
    }

    let mut vertices = Vec::with_capacity(vertex_count);
    for (row, &y) in ys.iter().enumerate() {
        limit::check()?;
        for (column, &x) in xs.iter().enumerate() {
            vertices.push([x, y, options.base_mm + top[row * columns + column]]);
        }
    }
    drop(top);
    let index = |row: usize, column: usize| (row * columns + column) as u32;
    let mut triangles = Vec::with_capacity(2 * (columns - 1) * (rows - 1) + 6 * (columns + rows));
    for row in 0..rows - 1 {
        limit::check()?;
        for column in 0..columns - 1 {
            // Seen from above: lower-left, lower-right, upper-right,
            // upper-left. Row + 1 is lower, at smaller Y.
            let lower_left = index(row + 1, column);
            let lower_right = index(row + 1, column + 1);
            let upper_right = index(row, column + 1);
            let upper_left = index(row, column);
            if options.mirror {
                triangles.push([lower_left, lower_right, upper_left]);
                triangles.push([lower_right, upper_right, upper_left]);
            } else {
                triangles.push([lower_left, lower_right, upper_right]);
                triangles.push([lower_left, upper_right, upper_left]);
            }
        }
    }

    // The boundary, anticlockwise from above: along the lowest row, up the
    // last column, back along the top row and down the first column.
    let slot = |row: usize, column: usize| -> Option<usize> {
        if row == rows - 1 {
            Some(column)
        } else if row == 0 {
            Some(columns + rows - 2 + columns - 1 - column)
        } else if column == columns - 1 {
            Some(columns + rows - 2 - row)
        } else if column == 0 {
            Some(2 * columns + rows - 3 + row)
        } else {
            None
        }
    };
    let mut boundary = Vec::with_capacity(2 * (columns + rows) - 4);
    boundary.extend((0..columns).map(|column| (rows - 1, column)));
    boundary.extend((1..rows - 1).rev().map(|row| (row, columns - 1)));
    boundary.extend((0..columns).rev().map(|column| (0, column)));
    boundary.extend((1..rows - 1).map(|row| (row, 0)));
    let first_bottom = vertices.len() as u32;
    for &(row, column) in &boundary {
        vertices.push([xs[column], ys[row], 0.0]);
    }
    let count = boundary.len();
    for k in 0..count {
        let next = (k + 1) % count;
        let (top_here, top_next) = (
            index(boundary[k].0, boundary[k].1),
            index(boundary[next].0, boundary[next].1),
        );
        let (bottom_here, bottom_next) = (first_bottom + k as u32, first_bottom + next as u32);
        triangles.push([bottom_here, bottom_next, top_next]);
        triangles.push([bottom_here, top_next, top_here]);
    }

    // The bottom: a coarse grid of cells, each spanning up to BOTTOM_STRIDE
    // samples. Interior cells are two triangles. A cell on the boundary
    // carries every boundary vertex along its outer sides and is a fan
    // around its own centre, which lies strictly inside it, so no triangle
    // is degenerate and none reaches beyond its cell.
    let coarse = |count: usize| {
        let mut positions: Vec<usize> = (0..count).step_by(BOTTOM_STRIDE).collect();
        if positions.last() != Some(&(count - 1)) {
            positions.push(count - 1);
        }
        positions
    };
    let (coarse_columns, coarse_rows) = (coarse(columns), coarse(rows));
    let first_interior = vertices.len() as u32;
    let inner_columns = coarse_columns.len().saturating_sub(2);
    for &row in &coarse_rows[1..coarse_rows.len() - 1] {
        for &column in &coarse_columns[1..coarse_columns.len() - 1] {
            vertices.push([xs[column], ys[row], 0.0]);
        }
    }
    let bottom = |row_index: usize, column_index: usize| -> u32 {
        let (row, column) = (coarse_rows[row_index], coarse_columns[column_index]);
        match slot(row, column) {
            Some(position) => first_bottom + position as u32,
            None => first_interior + ((row_index - 1) * inner_columns + column_index - 1) as u32,
        }
    };
    let mut polygon = Vec::new();
    for b in 0..coarse_rows.len() - 1 {
        limit::check()?;
        for a in 0..coarse_columns.len() - 1 {
            let (upper, lower) = (coarse_rows[b], coarse_rows[b + 1]);
            let (left, right) = (coarse_columns[a], coarse_columns[a + 1]);
            // Anticlockwise from above: along the lower side, up the right,
            // back along the upper side and down the left.
            polygon.clear();
            polygon.push(bottom(b + 1, a));
            if lower == rows - 1 {
                polygon.extend((left + 1..right).map(|column| first_bottom + column as u32));
            }
            polygon.push(bottom(b + 1, a + 1));
            if right == columns - 1 {
                polygon.extend(
                    (upper + 1..lower)
                        .rev()
                        .filter_map(|row| slot(row, right))
                        .map(|position| first_bottom + position as u32),
                );
            }
            polygon.push(bottom(b, a + 1));
            if upper == 0 {
                polygon.extend(
                    (left + 1..right)
                        .rev()
                        .filter_map(|column| slot(0, column))
                        .map(|position| first_bottom + position as u32),
                );
            }
            polygon.push(bottom(b, a));
            if left == 0 {
                polygon.extend(
                    (upper + 1..lower)
                        .filter_map(|row| slot(row, 0))
                        .map(|position| first_bottom + position as u32),
                );
            }
            if polygon.len() == 4 {
                let [lower_left, lower_right, upper_right, upper_left] =
                    [polygon[0], polygon[1], polygon[2], polygon[3]];
                triangles.push([lower_left, upper_right, lower_right]);
                triangles.push([lower_left, upper_left, upper_right]);
            } else {
                let centre = vertices.len() as u32;
                vertices.push([
                    (xs[left] + xs[right]) / 2.0,
                    (ys[upper] + ys[lower]) / 2.0,
                    0.0,
                ]);
                for k in 0..polygon.len() {
                    triangles.push([centre, polygon[(k + 1) % polygon.len()], polygon[k]]);
                }
            }
        }
    }
    Ok(ReliefSolid {
        mesh: TriangleMesh {
            vertices,
            triangles,
        },
        columns,
        rows,
    })
}
