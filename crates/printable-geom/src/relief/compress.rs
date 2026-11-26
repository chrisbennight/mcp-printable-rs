//! Bas-relief compression of a height field into millimetres.
//!
//! The height field's slopes between neighbouring pixels are compressed and
//! integrated again, after Weyrich et al. (2007), "Digital bas-relief from 3D
//! scenes":
//!
//! 1. Slopes across the subject's silhouette, and across depth jumps inside
//!    it, become zero, so the subject rises from a flat background and
//!    occluding parts no longer stand on tall steps.
//! 2. The remaining slopes are divided by their mean magnitude, which makes
//!    the compression independent of the depth map's unknown scale.
//! 3. Each slope is split into a base, its average over nearby subject
//!    edges, and detail, the rest, as in two-scale relief methods such as
//!    Photo2Relief. Both are scaled by the logarithmic factor
//!    `ln(1 + alpha * |base|) / (alpha * |base|)` of the base, so steep broad
//!    shape is flattened most, and detail is further multiplied by the
//!    detail gain.
//! 4. The compressed slopes are integrated by least squares with the
//!    background held at zero height, and the result is scaled to the
//!    requested relief depth.

use serde::Serialize;

use super::heightfield::{DOWN, RIGHT};
use super::poisson::{divergence, solve_neumann};
use super::solver::System;
use super::{Grid, HeightField, ReliefError, positive};

/// Prior weight holding background pixels at zero height: large against the
/// unit weight of each edge, so the background moves by about a millionth
/// of the slopes that pull on it.
const PIN: f32 = 1e6;

/// Parameters for [`compress`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompressionOptions {
    /// Width of the relief from the first to the last pixel column.
    pub width_mm: f64,
    /// Height from the first to the last pixel row. Without it, pixels are
    /// square.
    pub height_mm: Option<f64>,
    /// Height from the relief's lowest point to its highest.
    pub relief_depth_mm: f64,
    /// Logarithmic compression of slopes, measured in multiples of the mean
    /// slope; 0 leaves slopes in proportion. The default of 1 scales a slope
    /// of 2.5 times the mean by one half and a slope of 10 times the mean by
    /// about a quarter; it is a starting point, not a measured optimum.
    pub compression: f64,
    /// Multiplier for detail finer than `detail_sigma_px`; 1 compresses
    /// detail as much as the shape it lies on.
    pub detail_gain: f64,
    /// Gaussian scale in pixels separating base from detail; 0 compresses
    /// each slope by its own magnitude.
    pub detail_sigma_px: f64,
    /// Keep the steps at depth jumps inside the subject instead of removing
    /// them. Silhouette steps are always removed.
    pub keep_depth_steps: bool,
    /// Conjugate-gradient iterations, at most, for the solve with a held
    /// background.
    pub max_solver_iterations: usize,
    /// The solve stops once its residual is this fraction of its right-hand
    /// side.
    pub solver_tolerance: f64,
}

impl CompressionOptions {
    /// Defaults for a relief `width_mm` wide and `relief_depth_mm` deep.
    pub fn new(width_mm: f64, relief_depth_mm: f64) -> Self {
        Self {
            width_mm,
            height_mm: None,
            relief_depth_mm,
            compression: 1.0,
            detail_gain: 1.0,
            detail_sigma_px: 4.0,
            keep_depth_steps: false,
            max_solver_iterations: 20_000,
            solver_tolerance: 1e-6,
        }
    }

    fn validate(&self) -> Result<(), ReliefError> {
        let invalid = |message: &str| Err(ReliefError::InvalidOptions(message.to_string()));
        if !positive(self.width_mm) || self.height_mm.is_some_and(|height| !positive(height)) {
            return invalid("width_mm and height_mm must be finite positive numbers");
        }
        if !positive(self.relief_depth_mm) {
            return invalid("relief_depth_mm must be a finite positive number");
        }
        let not_negative = |value: f64| value.is_finite() && value >= 0.0;
        if !(not_negative(self.compression)
            && not_negative(self.detail_gain)
            && not_negative(self.detail_sigma_px))
        {
            return invalid(
                "compression, detail_gain and detail_sigma_px must be finite and not negative",
            );
        }
        if self.max_solver_iterations == 0 || !positive(self.solver_tolerance) {
            return invalid("the solver needs at least one iteration and a positive tolerance");
        }
        Ok(())
    }
}

/// Relief heights in millimetres on a regular grid.
#[derive(Debug, Clone, PartialEq)]
pub struct ReliefMap {
    /// Height above the relief's lowest point, row by row from the top of
    /// the image.
    pub heights_mm: Grid<f64>,
    /// Distance between neighbouring samples along a row and down a column.
    pub spacing_mm: [f64; 2],
}

/// How a relief was compressed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompressionReport {
    /// Mean slope magnitude before compression, in height-field units per
    /// pixel.
    pub mean_slope: f64,
    pub solver_iterations: usize,
    /// Whether the solve met its tolerance; true when no iterative solve
    /// was needed.
    pub converged: bool,
    /// Height of the background, when the subject does not fill the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_mm: Option<f64>,
}

/// Compress a height field into a relief of the requested size and depth.
pub fn compress(
    field: &HeightField,
    options: &CompressionOptions,
) -> Result<(ReliefMap, CompressionReport), ReliefError> {
    options.validate()?;
    let (width, height) = (field.width(), field.height());
    let pixels = width * height;
    let heights = field.heights.values();
    let mask = field.mask.values();
    let valid = |i: usize, j: usize, bit: u8| {
        mask[i] && mask[j] && (options.keep_depth_steps || field.open[i] & bit != 0)
    };
    let mut right = vec![0.0; pixels];
    let mut down = vec![0.0; pixels];
    let mut right_valid = vec![0.0; pixels];
    let mut down_valid = vec![0.0; pixels];
    let mut total = 0.0;
    let mut count = 0usize;
    for i in 0..pixels {
        if i % width + 1 < width && valid(i, i + 1, RIGHT) {
            right[i] = heights[i + 1] - heights[i];
            right_valid[i] = 1.0;
            total += right[i].abs();
            count += 1;
        }
        if i + width < pixels && valid(i, i + width, DOWN) {
            down[i] = heights[i + width] - heights[i];
            down_valid[i] = 1.0;
            total += down[i].abs();
            count += 1;
        }
    }
    let mean_slope = if count > 0 { total / count as f64 } else { 0.0 };
    if mean_slope > 0.0 {
        for (slopes, valid) in [(&mut right, &right_valid), (&mut down, &down_valid)] {
            for slope in slopes.iter_mut() {
                *slope /= mean_slope;
            }
            let base = if options.detail_sigma_px > 0.0 {
                blur(slopes, valid, width, height, options.detail_sigma_px)?
            } else {
                slopes.clone()
            };
            for i in 0..pixels {
                slopes[i] = if valid[i] > 0.0 {
                    let detail = slopes[i] - base[i];
                    log_factor(options.compression, base[i].abs())
                        * (base[i] + options.detail_gain * detail)
                } else {
                    0.0
                };
            }
        }
    }
    drop((right_valid, down_valid));

    let mut z = divergence(width, height, &right, &down);
    solve_neumann(width, height, &mut z)?;
    let background = mask.iter().filter(|inside| !**inside).count();
    let mut solver_iterations = 0;
    let mut converged = true;
    if background > 0 && mean_slope > 0.0 {
        let level =
            (0..pixels).filter(|&i| !mask[i]).map(|i| z[i]).sum::<f64>() / background as f64;
        for value in &mut z {
            *value -= level;
        }
        let mut system = System::new(width, height);
        for i in 0..pixels {
            if i % width + 1 < width {
                system.right[i] = 1.0;
                system.add_target(i, i + 1, 1.0, right[i]);
            }
            if i + width < pixels {
                system.down[i] = 1.0;
                system.add_target(i, i + width, 1.0, down[i]);
            }
            if !mask[i] {
                system.prior[i] = PIN;
            }
        }
        drop((right, down));
        let solve = system.solve(
            &mut z,
            options.max_solver_iterations,
            options.solver_tolerance,
        )?;
        solver_iterations = solve.iterations;
        converged = solve.converged;
    }

    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for &value in &z {
        low = low.min(value);
        high = high.max(value);
    }
    let scale = if high > low {
        options.relief_depth_mm / (high - low)
    } else {
        0.0
    };
    for value in &mut z {
        *value = (*value - low) * scale;
    }
    let background_mm = (background > 0)
        .then(|| (0..pixels).filter(|&i| !mask[i]).map(|i| z[i]).sum::<f64>() / background as f64);
    let spacing_x = options.width_mm / (width - 1) as f64;
    let spacing_y = options
        .height_mm
        .map_or(spacing_x, |height_mm| height_mm / (height - 1) as f64);
    Ok((
        ReliefMap {
            heights_mm: Grid::new(width, height, z)?,
            spacing_mm: [spacing_x, spacing_y],
        },
        CompressionReport {
            mean_slope,
            solver_iterations,
            converged,
            background_mm,
        },
    ))
}

/// `ln(1 + alpha x) / (alpha x)`, which falls from 1 at small `x`.
fn log_factor(alpha: f64, magnitude: f64) -> f64 {
    let scaled = alpha * magnitude;
    if scaled < 1e-12 {
        1.0
    } else {
        scaled.ln_1p() / scaled
    }
}

/// The weighted Gaussian average of `values` over neighbouring entries with
/// positive `weights`, or zero where none are near.
fn blur(
    values: &[f64],
    weights: &[f64],
    width: usize,
    height: usize,
    sigma: f64,
) -> Result<Vec<f64>, ReliefError> {
    let radius = (3.0 * sigma).ceil() as usize;
    let kernel: Vec<f64> = (0..=radius)
        .map(|offset| (-((offset * offset) as f64) / (2.0 * sigma * sigma)).exp())
        .collect();
    let weighted: Vec<f64> = values.iter().zip(weights).map(|(v, w)| v * w).collect();
    let mut sums = [weighted, weights.to_vec()];
    for sum in &mut sums {
        *sum = pass(sum, &kernel, width, height, 1, width);
        crate::limit::check()?;
        *sum = pass(sum, &kernel, height, width, width, 1);
        crate::limit::check()?;
    }
    let [numerator, denominator] = sums;
    Ok(numerator
        .iter()
        .zip(&denominator)
        .map(|(n, d)| if *d > 1e-12 { n / d } else { 0.0 })
        .collect())
}

/// Convolve each line of `length` entries, `stride` apart, with the
/// symmetric `kernel`; `lines` lines start `line_stride` apart.
fn pass(
    values: &[f64],
    kernel: &[f64],
    length: usize,
    lines: usize,
    stride: usize,
    line_stride: usize,
) -> Vec<f64> {
    let mut out = vec![0.0; values.len()];
    for line in 0..lines {
        let start = line * line_stride;
        for position in 0..length {
            let mut sum = values[start + position * stride] * kernel[0];
            for (offset, weight) in kernel.iter().enumerate().skip(1) {
                if position >= offset {
                    sum += values[start + (position - offset) * stride] * weight;
                }
                if position + offset < length {
                    sum += values[start + (position + offset) * stride] * weight;
                }
            }
            out[start + position * stride] = sum;
        }
    }
    out
}
