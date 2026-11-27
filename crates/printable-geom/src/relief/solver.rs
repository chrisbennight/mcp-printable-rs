//! Weighted least-squares integration of target differences on a pixel grid.
//!
//! Each edge between neighbouring pixels asks that the difference between
//! their values equal a target, with a weight, and each pixel may ask that
//! its value equal a prior target. The normal equations form a weighted
//! graph Laplacian plus a diagonal: symmetric, and positive definite when
//! every connected group of pixels has some prior weight. Edges of zero
//! weight leave the groups on either side uncoupled, so solving the whole
//! grid at once is the same as solving each group separately. The matrix is
//! applied directly from the edge weights and never stored.

use crate::{GeometryError, limit};

/// The normal equations of one weighted least-squares integration.
pub(super) struct System {
    pub width: usize,
    pub height: usize,
    /// Weight of the edge from each pixel to its right-hand neighbour; zero
    /// in the last column.
    pub right: Vec<f32>,
    /// Weight of the edge from each pixel to the neighbour below it; zero in
    /// the last row.
    pub down: Vec<f32>,
    /// Weight of each pixel's prior target.
    pub prior: Vec<f32>,
    /// Right-hand side: for each edge, its weight times its target added at
    /// the far pixel and subtracted at the near one, plus each pixel's prior
    /// weight times its prior target.
    pub rhs: Vec<f64>,
}

/// How an iterative solve ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Solve {
    pub iterations: usize,
    pub converged: bool,
}

impl System {
    pub fn new(width: usize, height: usize) -> Self {
        let pixels = width * height;
        Self {
            width,
            height,
            right: vec![0.0; pixels],
            down: vec![0.0; pixels],
            prior: vec![0.0; pixels],
            rhs: vec![0.0; pixels],
        }
    }

    /// Ask that `value[far] - value[near]` equal `target` with `weight`.
    pub fn add_target(&mut self, near: usize, far: usize, weight: f64, target: f64) {
        let flux = weight * target;
        self.rhs[near] -= flux;
        self.rhs[far] += flux;
    }

    /// `out = A x`.
    fn apply(&self, x: &[f64], out: &mut [f64]) {
        let width = self.width;
        for ((value, prior), x) in out.iter_mut().zip(&self.prior).zip(x) {
            *value = f64::from(*prior) * x;
        }
        for row in 0..self.height {
            let start = row * width;
            for i in start..start + width - 1 {
                let flow = f64::from(self.right[i]) * (x[i] - x[i + 1]);
                out[i] += flow;
                out[i + 1] -= flow;
            }
        }
        for i in 0..(self.height - 1) * width {
            let flow = f64::from(self.down[i]) * (x[i] - x[i + width]);
            out[i] += flow;
            out[i + width] -= flow;
        }
    }

    /// The reciprocal of each diagonal entry, or one where it is zero.
    fn inverse_diagonal(&self) -> Vec<f64> {
        let width = self.width;
        let mut diagonal: Vec<f64> = self.prior.iter().map(|&prior| f64::from(prior)).collect();
        for i in 0..self.right.len() {
            let right = f64::from(self.right[i]);
            let down = f64::from(self.down[i]);
            diagonal[i] += right + down;
            if right != 0.0 {
                diagonal[i + 1] += right;
            }
            if down != 0.0 {
                diagonal[i + width] += down;
            }
        }
        diagonal
            .into_iter()
            .map(|value| if value > 0.0 { 1.0 / value } else { 1.0 })
            .collect()
    }

    /// Solve `A x = rhs` by conjugate gradients preconditioned with the
    /// diagonal, starting from `x`, until the residual falls to `tolerance`
    /// times the right-hand side or `max_iterations` have run.
    pub fn solve(
        &self,
        x: &mut [f64],
        max_iterations: usize,
        tolerance: f64,
    ) -> Result<Solve, GeometryError> {
        let pixels = x.len();
        let target = tolerance * norm(&self.rhs);
        if target == 0.0 {
            x.fill(0.0);
            return Ok(Solve {
                iterations: 0,
                converged: true,
            });
        }
        let inverse = self.inverse_diagonal();
        let mut residual = vec![0.0; pixels];
        self.apply(x, &mut residual);
        for (residual, rhs) in residual.iter_mut().zip(&self.rhs) {
            *residual = rhs - *residual;
        }
        let mut direction: Vec<f64> = residual.iter().zip(&inverse).map(|(r, m)| r * m).collect();
        let mut product = vec![0.0; pixels];
        let mut rz: f64 = residual.iter().zip(&direction).map(|(r, z)| r * z).sum();
        let mut residual_norm = norm(&residual);
        for iteration in 0..max_iterations {
            if residual_norm <= target {
                return Ok(Solve {
                    iterations: iteration,
                    converged: true,
                });
            }
            limit::check()?;
            self.apply(&direction, &mut product);
            let curvature: f64 = direction.iter().zip(&product).map(|(p, q)| p * q).sum();
            if curvature.is_nan() || curvature <= 0.0 {
                break;
            }
            let step = rz / curvature;
            let mut next_rz = 0.0;
            let mut squared = 0.0;
            for i in 0..pixels {
                x[i] += step * direction[i];
                residual[i] -= step * product[i];
                next_rz += residual[i] * residual[i] * inverse[i];
                squared += residual[i] * residual[i];
            }
            residual_norm = squared.sqrt();
            let beta = next_rz / rz;
            rz = next_rz;
            for i in 0..pixels {
                direction[i] = inverse[i] * residual[i] + beta * direction[i];
            }
        }
        Ok(Solve {
            iterations: max_iterations,
            converged: residual_norm <= target,
        })
    }
}

fn norm(values: &[f64]) -> f64 {
    values.iter().map(|value| value * value).sum::<f64>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_differences_with_one_anchor_are_recovered() {
        let (width, height) = (7, 5);
        let truth: Vec<f64> = (0..width * height)
            .map(|i| ((i % width) as f64 * 0.3).sin() + (i / width) as f64 * 0.7)
            .collect();
        let mut system = System::new(width, height);
        for i in 0..width * height {
            if i % width + 1 < width {
                system.right[i] = 1.0;
                system.add_target(i, i + 1, 1.0, truth[i + 1] - truth[i]);
            }
            if i / width + 1 < height {
                system.down[i] = 2.0;
                system.add_target(i, i + width, 2.0, truth[i + width] - truth[i]);
            }
        }
        system.prior[0] = 1.0;
        system.rhs[0] += truth[0];
        let mut x = vec![0.0; width * height];
        let solve = system.solve(&mut x, 1000, 1e-12).unwrap();
        assert!(solve.converged);
        for (found, expected) in x.iter().zip(&truth) {
            assert!((found - expected).abs() < 1e-8);
        }
    }

    #[test]
    fn an_iteration_budget_stops_the_solve_unconverged() {
        let mut system = System::new(64, 64);
        system.right.iter_mut().enumerate().for_each(|(i, w)| {
            *w = if i % 64 < 63 { 1.0 } else { 0.0 };
        });
        system.prior.fill(1e-6);
        system.rhs[0] = 1.0;
        let mut x = vec![0.0; 64 * 64];
        let solve = system.solve(&mut x, 3, 1e-12).unwrap();
        assert_eq!(solve.iterations, 3);
        assert!(!solve.converged);
    }
}
