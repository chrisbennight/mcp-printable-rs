//! The least-squares surface for target differences between every pair of
//! neighbouring pixels on a whole rectangle, by discrete cosine transforms.
//!
//! The normal equations are the grid's graph Laplacian, whose eigenvectors
//! are products of type-II cosines: the Poisson equation with zero normal
//! slope at the image border. Transforming, dividing by each eigenvalue and
//! transforming back solves it exactly in O(n log n). The constant mode is
//! free and set to zero, so the result has zero mean.

use std::f64::consts::PI;
use std::sync::Arc;

use rustdct::{DctPlanner, TransformType2And3};

use crate::{GeometryError, limit};

/// Target differences on every edge, as the right-hand side of the
/// Laplacian: `right[i]` is the target for `value[i + 1] - value[i]` and
/// `down[i]` for `value[i + width] - value[i]`; entries for edges past the
/// last column or row are ignored.
pub(super) fn divergence(width: usize, height: usize, right: &[f64], down: &[f64]) -> Vec<f64> {
    let mut rhs = vec![0.0; width * height];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if x + 1 < width {
                rhs[i] -= right[i];
                rhs[i + 1] += right[i];
            }
            if y + 1 < height {
                rhs[i] -= down[i];
                rhs[i + width] += down[i];
            }
        }
    }
    rhs
}

/// Replace `values`, the right-hand side of the grid Laplacian `L z = b`
/// with `(L z)_i` the sum over neighbours of `z_i - z_j`, by its zero-mean
/// solution. A right-hand side that does not sum to zero is solved in the
/// least-squares sense.
pub(super) fn solve_neumann(
    width: usize,
    height: usize,
    values: &mut [f64],
) -> Result<(), GeometryError> {
    let mut planner = DctPlanner::new();
    let rows = planner.plan_dct2(width);
    let columns = planner.plan_dct2(height);
    transform(values, width, &rows, true)?;
    let mut transposed = transpose(values, width, height);
    transform(&mut transposed, height, &columns, true)?;
    let row_eigen: Vec<f64> = (0..width)
        .map(|k| 2.0 - 2.0 * (PI * k as f64 / width as f64).cos())
        .collect();
    let column_eigen: Vec<f64> = (0..height)
        .map(|k| 2.0 - 2.0 * (PI * k as f64 / height as f64).cos())
        .collect();
    for (x, line) in transposed.chunks_exact_mut(height).enumerate() {
        for (y, value) in line.iter_mut().enumerate() {
            let eigenvalue = row_eigen[x] + column_eigen[y];
            *value = if eigenvalue > 0.0 {
                *value / eigenvalue
            } else {
                0.0
            };
        }
    }
    transform(&mut transposed, height, &columns, false)?;
    let back = transpose(&transposed, height, width);
    values.copy_from_slice(&back);
    // A type-III transform undoes a type-II transform of length n up to a
    // factor of n / 2 on each axis.
    let scale = 4.0 / (width * height) as f64;
    for value in values.iter_mut() {
        *value *= scale;
    }
    transform(values, width, &rows, false)
}

fn transform(
    values: &mut [f64],
    length: usize,
    plan: &Arc<dyn TransformType2And3<f64>>,
    forward: bool,
) -> Result<(), GeometryError> {
    let mut scratch = vec![0.0; plan.get_scratch_len()];
    for line in values.chunks_exact_mut(length) {
        if forward {
            plan.process_dct2_with_scratch(line, &mut scratch);
        } else {
            plan.process_dct3_with_scratch(line, &mut scratch);
        }
    }
    limit::check()
}

/// The `height` x `width` transpose of a `width` x `height` row-major grid.
fn transpose(values: &[f64], width: usize, height: usize) -> Vec<f64> {
    let mut out = vec![0.0; values.len()];
    for (y, row) in values.chunks_exact(width).enumerate() {
        for (x, value) in row.iter().enumerate() {
            out[x * height + y] = *value;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrable_differences_are_recovered_up_to_a_constant() {
        for (width, height) in [(2, 2), (7, 5), (16, 9), (13, 31)] {
            let truth: Vec<f64> = (0..width * height)
                .map(|i| {
                    let (x, y) = ((i % width) as f64, (i / width) as f64);
                    (x * 0.4).sin() * 3.0 + y * y * 0.05 - x * y * 0.1
                })
                .collect();
            let mut right = vec![0.0; width * height];
            let mut down = vec![0.0; width * height];
            for i in 0..width * height {
                if i % width + 1 < width {
                    right[i] = truth[i + 1] - truth[i];
                }
                if i / width + 1 < height {
                    down[i] = truth[i + width] - truth[i];
                }
            }
            let mut z = divergence(width, height, &right, &down);
            solve_neumann(width, height, &mut z).unwrap();
            let mean = truth.iter().sum::<f64>() / truth.len() as f64;
            for (found, expected) in z.iter().zip(&truth) {
                assert!(
                    (found - (expected - mean)).abs() < 1e-9,
                    "{width} x {height}: {found} against {}",
                    expected - mean
                );
            }
        }
    }
}
