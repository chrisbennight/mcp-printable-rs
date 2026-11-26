//! Height fields from depth, and optionally from surface normals.
//!
//! Depth alone is the default. Normal integration has not yet been shown to
//! beat the best depth models on measured scenes, so it runs only on request.
//!
//! With a perspective camera of focal length `f` in pixels, a pixel at
//! offset `(u, v)` from the principal point sees along the ray
//! `r = (u / f, v / f, 1)` in camera coordinates with x right, y down and z
//! forward. For a surface with normal `n` there, the log of the depth `z`
//! changes along the image as
//!
//! ```text
//! a * d(log z)/du + n.x / f = 0,   a * d(log z)/dv + n.y / f = 0,   a = n . r
//! ```
//!
//! so normals fix the surface's log depth up to one offset per connected
//! surface, whatever the depth's unknown scale.
//!
//! Anchored integration solves for log depth with two terms. Each edge
//! between neighbouring pixels contributes the equations above for both of
//! its pixels, weighted as in bilateral normal integration (Cao et al.,
//! 2022): a pixel's forward and backward equations along each axis share a
//! unit weight, shifted by a sigmoid towards the side whose depth changes
//! less, so a surface is not pulled across an edge it does not continue
//! over. A depth prior asks each pixel's log depth to equal the depth map's,
//! weighted by its confidence over `anchor_sigma_px` squared. Detail finer
//! than about that many pixels therefore comes from the normals and broader
//! shape from the depth, and the prior fixes the offset of every region,
//! including regions that normals alone cannot relate. Edges that leave the
//! mask or cross a depth jump carry no equation, so the two sides of an
//! occlusion are integrated separately. The weighted least-squares problem
//! is solved by preconditioned conjugate gradients, the weights are updated
//! from the solution, and this repeats until the energy settles.
//!
//! Frankot-Chellappa integration is a fast preview: one exact least-squares
//! solve of the normals' slopes over the whole rectangle by cosine
//! transforms, with zero slope across silhouettes and depth jumps, and no
//! depth prior. Without a focal length it assumes an orthographic camera.

use serde::Serialize;

use super::heightfield::{DOWN, Prepared, RIGHT, heights_from_distances};
use super::poisson::{divergence, solve_neumann};
use super::solver::System;
use super::{Analysis, HeightField, ReliefError, positive};

/// A normal whose component along the viewing ray is smaller than this is
/// too close to edge-on to give a usable slope in Frankot-Chellappa
/// integration.
const MIN_FACING: f64 = 0.05;
/// Confidence below this counts as this, so that every pixel keeps some
/// depth prior and every region a defined offset.
const MIN_CONFIDENCE: f64 = 1e-3;

/// How a height field is made from an analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMethod {
    /// Heights are the depth map itself.
    #[default]
    DepthOnly,
    /// A fast preview from normals alone, without depth anchoring.
    FrankotChellappa,
    /// Normals for fine detail, anchored to depth for broad shape.
    AnchoredNormals,
    /// Heights supplied by the caller.
    Supplied,
}

/// Parameters for [`integrate`].
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationOptions {
    pub method: IntegrationMethod,
    /// Larger analyses are refused with [`ReliefError::TooLarge`].
    pub max_pixels: usize,
    /// Neighbouring pixels whose depths imply a surface steeper than this
    /// slope against the viewing ray lie on two sides of an occlusion. The
    /// default of 10, about 84 degrees, separates surfaces seen nearly
    /// edge-on, which a photograph rarely shows as continuous. Without a
    /// focal length, the longest image side in pixels stands in for it,
    /// about a 53-degree field of view.
    pub max_jump_slope: f64,
    /// Anchored integration: the scale, in pixels, below which normals
    /// rather than depth decide the shape. The default of 4 pixels matches
    /// the blur that scored best when depth and normals were combined in
    /// the depth benchmark; this method itself is not yet measured.
    pub anchor_sigma_px: f64,
    /// Anchored integration: how sharply the equations on either side of a
    /// pixel are weighted towards the smoother side. 2 is the published
    /// default of bilateral normal integration.
    pub discontinuity_sharpness: f64,
    /// Anchored integration: reweighting rounds, at most.
    pub max_reweighting_iterations: usize,
    /// Anchored integration: conjugate-gradient iterations over all rounds,
    /// at most.
    pub max_solver_iterations: usize,
    /// Anchored integration stops once a round changes the energy by less
    /// than this fraction.
    pub energy_tolerance: f64,
    /// Each solve stops once its residual is this fraction of its
    /// right-hand side.
    pub solver_tolerance: f64,
}

impl Default for IntegrationOptions {
    fn default() -> Self {
        Self {
            method: IntegrationMethod::DepthOnly,
            max_pixels: super::DEFAULT_MAX_PIXELS,
            max_jump_slope: 10.0,
            anchor_sigma_px: 4.0,
            discontinuity_sharpness: 2.0,
            max_reweighting_iterations: 20,
            max_solver_iterations: 5000,
            energy_tolerance: 1e-4,
            solver_tolerance: 1e-6,
        }
    }
}

impl IntegrationOptions {
    fn validate(&self) -> Result<(), ReliefError> {
        let invalid = |message: &str| Err(ReliefError::InvalidOptions(message.to_string()));
        if self.method == IntegrationMethod::Supplied {
            return invalid(
                "supplied heights become a height field through HeightField::from_heights",
            );
        }
        if !positive(self.max_jump_slope) && self.max_jump_slope != f64::INFINITY {
            return invalid("max_jump_slope must be positive");
        }
        if !positive(self.anchor_sigma_px) {
            return invalid("anchor_sigma_px must be a finite positive number");
        }
        if !(self.discontinuity_sharpness.is_finite() && self.discontinuity_sharpness >= 0.0) {
            return invalid("discontinuity_sharpness must be finite and not negative");
        }
        if !(positive(self.energy_tolerance) && positive(self.solver_tolerance)) {
            return invalid("tolerances must be finite positive numbers");
        }
        if self.max_reweighting_iterations == 0 || self.max_solver_iterations == 0 {
            return invalid("iteration limits must be at least 1");
        }
        Ok(())
    }
}

/// How a height field was made.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntegrationReport {
    pub method: IntegrationMethod,
    /// Connected subject regions, separated by the mask and depth jumps.
    pub regions: usize,
    /// Reweighting rounds run.
    pub reweighting_iterations: usize,
    /// Conjugate-gradient iterations over all rounds.
    pub solver_iterations: usize,
    /// Whether the last solve met its tolerance and, for anchored
    /// integration, the energy settled within the round limit.
    pub converged: bool,
}

impl IntegrationReport {
    fn direct(method: IntegrationMethod, regions: usize) -> Self {
        Self {
            method,
            regions,
            reweighting_iterations: 0,
            solver_iterations: 0,
            converged: true,
        }
    }

    pub(super) fn supplied(regions: usize) -> Self {
        Self::direct(IntegrationMethod::Supplied, regions)
    }
}

/// Make a height field from an analysis.
pub fn integrate(
    analysis: &Analysis,
    options: &IntegrationOptions,
) -> Result<HeightField, ReliefError> {
    options.validate()?;
    let prepared = analysis.prepare(options.max_pixels, options.max_jump_slope)?;
    let (distances, report) = match options.method {
        IntegrationMethod::DepthOnly => (
            prepared.depth.iter().map(|&d| f64::from(d)).collect(),
            IntegrationReport::direct(IntegrationMethod::DepthOnly, prepared.regions),
        ),
        IntegrationMethod::FrankotChellappa => {
            let normals = analysis
                .normals
                .as_ref()
                .ok_or(ReliefError::MissingInput("Frankot-Chellappa", "normals"))?;
            (
                frankot_chellappa(&prepared, normals.values())?,
                IntegrationReport::direct(IntegrationMethod::FrankotChellappa, prepared.regions),
            )
        }
        IntegrationMethod::AnchoredNormals => {
            let normals = analysis
                .normals
                .as_ref()
                .ok_or(ReliefError::MissingInput("anchored normal", "normals"))?;
            let focal = prepared.focal.ok_or(ReliefError::MissingInput(
                "anchored normal",
                "a focal length",
            ))?;
            let confidence = analysis.confidence.as_ref().map(|grid| grid.values());
            anchored(&prepared, normals.values(), confidence, focal, options)?
        }
        IntegrationMethod::Supplied => unreachable!("rejected by validation"),
    };
    let heights =
        heights_from_distances(prepared.width, prepared.height, distances, &prepared.mask)?;
    Ok(HeightField {
        heights,
        mask: prepared.mask_grid()?,
        report,
        open: prepared.open,
    })
}

/// The camera-frame normal with x right, y down and z forward, or `None`
/// for a pixel without one.
fn camera_normal(normal: [f32; 3]) -> Option<[f64; 3]> {
    let [x, y, z] = normal.map(f64::from);
    let length = (x * x + y * y + z * z).sqrt();
    (length >= 0.5).then(|| [x / length, -y / length, -z / length])
}

/// Per-pixel coefficients of the log-depth equations `a * slope + b = 0`
/// along each image axis: `[a, b_u, b_v]`, all zero without a usable normal.
fn perspective_terms(prepared: &Prepared, normals: &[[f32; 3]], focal: f64) -> Vec<[f32; 3]> {
    let (width, height) = (prepared.width, prepared.height);
    let centre = ((width - 1) as f64 / 2.0, (height - 1) as f64 / 2.0);
    normals
        .iter()
        .enumerate()
        .map(|(i, &normal)| {
            let Some(n) = camera_normal(normal).filter(|_| prepared.mask[i]) else {
                return [0.0; 3];
            };
            let ray = [
                ((i % width) as f64 - centre.0) / focal,
                ((i / width) as f64 - centre.1) / focal,
                1.0,
            ];
            let a = n[0] * ray[0] + n[1] * ray[1] + n[2];
            // A normal facing away from the camera describes no visible
            // surface.
            if a < 0.0 {
                [a as f32, (n[0] / focal) as f32, (n[1] / focal) as f32]
            } else {
                [0.0; 3]
            }
        })
        .collect()
}

/// Distances from Frankot-Chellappa integration of the normals.
fn frankot_chellappa(prepared: &Prepared, normals: &[[f32; 3]]) -> Result<Vec<f64>, ReliefError> {
    let (width, height) = (prepared.width, prepared.height);
    let pixels = width * height;
    // Per-pixel slopes along each axis: of log depth for a perspective
    // camera, of depth in pixel widths for an orthographic one.
    let slopes: Vec<Option<[f64; 2]>> = match prepared.focal {
        Some(focal) => perspective_terms(prepared, normals, focal)
            .into_iter()
            .map(|[a, bu, bv]| {
                let a = f64::from(a);
                (a < -MIN_FACING).then(|| [-f64::from(bu) / a, -f64::from(bv) / a])
            })
            .collect(),
        None => normals
            .iter()
            .enumerate()
            .map(|(i, &normal)| {
                camera_normal(normal)
                    .filter(|n| prepared.mask[i] && n[2] < -MIN_FACING)
                    .map(|n| [-n[0] / n[2], -n[1] / n[2]])
            })
            .collect(),
    };
    let edge = |i: usize, j: usize, axis: usize| match (slopes[i], slopes[j]) {
        (Some(a), Some(b)) => (a[axis] + b[axis]) / 2.0,
        (Some(a), None) => a[axis],
        (None, Some(b)) => b[axis],
        (None, None) => 0.0,
    };
    let mut right = vec![0.0; pixels];
    let mut down = vec![0.0; pixels];
    for i in 0..pixels {
        if prepared.open[i] & RIGHT != 0 {
            right[i] = edge(i, i + 1, 0);
        }
        if prepared.open[i] & DOWN != 0 {
            down[i] = edge(i, i + width, 1);
        }
    }
    drop(slopes);
    let mut z = divergence(width, height, &right, &down);
    drop((right, down));
    solve_neumann(width, height, &mut z)?;
    if prepared.focal.is_some() {
        // Match the depth map's mean log depth over the subject, so that the
        // heights share its units.
        let offset = subject_mean(prepared, |i| f64::from(prepared.depth[i]).ln() - z[i]);
        for value in &mut z {
            *value = (*value + offset).exp();
        }
    }
    Ok(z)
}

fn subject_mean(prepared: &Prepared, value: impl Fn(usize) -> f64) -> f64 {
    let (sum, count) = (0..prepared.mask.len())
        .filter(|&i| prepared.mask[i])
        .fold((0.0, 0usize), |(sum, count), i| (sum + value(i), count + 1));
    sum / count as f64
}

/// Weight of a pixel's forward or backward equation along one axis, given
/// the forward equation's share and whether the pixel has an equation on
/// the other side too; a lone equation takes the whole weight.
fn equation_weight(forward_share: f32, other_side: bool, forward: bool) -> f64 {
    match (other_side, forward) {
        (false, _) => 1.0,
        (true, true) => f64::from(forward_share),
        (true, false) => 1.0 - f64::from(forward_share),
    }
}

fn sigmoid(value: f64) -> f64 {
    1.0 / (1.0 + (-value).exp())
}

/// Distances from depth-anchored bilateral normal integration.
fn anchored(
    prepared: &Prepared,
    normals: &[[f32; 3]],
    confidence: Option<&[f32]>,
    focal: f64,
    options: &IntegrationOptions,
) -> Result<(Vec<f64>, IntegrationReport), ReliefError> {
    let (width, height) = (prepared.width, prepared.height);
    let pixels = width * height;
    let terms = perspective_terms(prepared, normals, focal);
    let open = &prepared.open;
    let mean_log = subject_mean(prepared, |i| f64::from(prepared.depth[i]).ln());
    let target: Vec<f64> = (0..pixels)
        .map(|i| {
            if prepared.mask[i] {
                f64::from(prepared.depth[i]).ln() - mean_log
            } else {
                0.0
            }
        })
        .collect();
    let anchor = 1.0 / (options.anchor_sigma_px * options.anchor_sigma_px);
    let prior: Vec<f32> = (0..pixels)
        .map(|i| {
            if prepared.mask[i] {
                let trust = confidence.map_or(1.0, |values| f64::from(values[i]));
                (anchor * trust.max(MIN_CONFIDENCE)) as f32
            } else {
                1.0
            }
        })
        .collect();
    // Weight of each pixel's forward equation along each axis; its backward
    // equation has the rest. A pixel with an equation on one side only gives
    // it the whole weight.
    let mut forward_u = vec![0.5f32; pixels];
    let mut forward_v = vec![0.5f32; pixels];
    let has_left = |i: usize| !i.is_multiple_of(width) && open[i - 1] & RIGHT != 0;
    let has_up = |i: usize| i >= width && open[i - width] & DOWN != 0;

    let mut z = target.clone();
    let mut previous_energy: Option<f64> = None;
    let mut solver_iterations = 0;
    let mut rounds = 0;
    let mut converged = false;
    while rounds < options.max_reweighting_iterations
        && solver_iterations < options.max_solver_iterations
    {
        rounds += 1;
        let mut system = System::new(width, height);
        system.prior.copy_from_slice(&prior);
        for i in 0..pixels {
            system.rhs[i] += f64::from(prior[i]) * target[i];
        }
        // Each edge carries the forward equation of its near pixel and the
        // backward equation of its far pixel; both target the difference of
        // the two pixels' log depths.
        let edge =
            |near: usize, far: usize, axis: usize, forward: &[f32], near_back, far_forward| {
                let [a_near, b_near] = [terms[near][0], terms[near][1 + axis]].map(f64::from);
                let [a_far, b_far] = [terms[far][0], terms[far][1 + axis]].map(f64::from);
                let w_near = equation_weight(forward[near], near_back, true);
                let w_far = equation_weight(forward[far], far_forward, false);
                let weight = w_near * a_near * a_near + w_far * a_far * a_far;
                let flux = -(w_near * a_near * b_near + w_far * a_far * b_far);
                (weight, flux)
            };
        for i in 0..pixels {
            if open[i] & RIGHT != 0 {
                let far_forward = open[i + 1] & RIGHT != 0;
                let (weight, flux) = edge(i, i + 1, 0, &forward_u, has_left(i), far_forward);
                system.right[i] = weight as f32;
                system.rhs[i] -= flux;
                system.rhs[i + 1] += flux;
            }
            if open[i] & DOWN != 0 {
                let far_forward = open[i + width] & DOWN != 0;
                let (weight, flux) = edge(i, i + width, 1, &forward_v, has_up(i), far_forward);
                system.down[i] = weight as f32;
                system.rhs[i] -= flux;
                system.rhs[i + width] += flux;
            }
        }
        let budget = options.max_solver_iterations - solver_iterations;
        let solve = system.solve(&mut z, budget, options.solver_tolerance)?;
        solver_iterations += solve.iterations;
        drop(system);

        let mut energy = 0.0;
        for i in 0..pixels {
            if !prepared.mask[i] {
                continue;
            }
            let residual = z[i] - target[i];
            energy += f64::from(prior[i]) * residual * residual;
            let [a, bu, bv] = terms[i].map(f64::from);
            for (b, forward, step, back_open, front_open) in [
                (bu, &forward_u, 1, has_left(i), open[i] & RIGHT != 0),
                (bv, &forward_v, width, has_up(i), open[i] & DOWN != 0),
            ] {
                if front_open {
                    let w = equation_weight(forward[i], back_open, true);
                    let r = a * (z[i + step] - z[i]) + b;
                    energy += w * r * r;
                }
                if back_open {
                    let w = equation_weight(forward[i], front_open, false);
                    let r = a * (z[i] - z[i - step]) + b;
                    energy += w * r * r;
                }
            }
        }
        let settled = previous_energy.is_some_and(|previous: f64| {
            (previous - energy).abs() <= options.energy_tolerance * previous.max(f64::MIN_POSITIVE)
        });
        converged = solve.converged && settled;
        if converged {
            break;
        }
        previous_energy = Some(energy);

        // Favour, at each pixel, the side whose slope against the viewing
        // ray is smaller.
        let sharpness = options.discontinuity_sharpness;
        for i in 0..pixels {
            let a = f64::from(terms[i][0]) * focal;
            if open[i] & RIGHT != 0 && has_left(i) {
                let front = a * (z[i + 1] - z[i]);
                let back = a * (z[i] - z[i - 1]);
                forward_u[i] = sigmoid(sharpness * (back * back - front * front)) as f32;
            }
            if open[i] & DOWN != 0 && has_up(i) {
                let front = a * (z[i + width] - z[i]);
                let back = a * (z[i] - z[i - width]);
                forward_v[i] = sigmoid(sharpness * (back * back - front * front)) as f32;
            }
        }
    }
    for value in &mut z {
        *value = (*value + mean_log).exp();
    }
    Ok((
        z,
        IntegrationReport {
            method: IntegrationMethod::AnchoredNormals,
            regions: prepared.regions,
            reweighting_iterations: rounds,
            solver_iterations,
            converged,
        },
    ))
}
