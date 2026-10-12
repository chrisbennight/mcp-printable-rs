//! Surface detail maps in FLAME's texture layout, filtered for printing.
//!
//! A displacement map holds a distance along the surface normal for each
//! texel. Detail is where the map's magnitude is a noticeable share of its
//! largest value; elsewhere the map is left as it is. Filtering works on the
//! detail alone, so the empty surroundings of a face-only map do not pull its
//! edges toward zero.

use thiserror::Error;

use crate::UvLayout;

/// Texels whose magnitude is below this share of the map's largest magnitude
/// are not detail. Maps stored as 16-bit integers carry rounding noise of
/// about one part in 65,535 where they are meant to be zero.
const DETAIL_SHARE: f32 = 0.005;
/// A Gaussian kernel reaches this many standard deviations each way.
const KERNEL_REACH: f64 = 3.0;

/// How to filter a displacement map before it is applied.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DetailFilter {
    /// Remove features wider than about this many millimetres on the head's
    /// surface, keeping finer detail.
    pub high_pass_mm: Option<f64>,
    /// Limit magnitudes to this percentile, 0 to 100, of the detail's
    /// magnitudes.
    pub clip_percentile: Option<f64>,
}

#[derive(Debug, Error, PartialEq)]
pub enum DetailError {
    #[error("the displacement values do not match the map size")]
    Size,
    #[error("the head and its texture layout do not match")]
    Layout,
}

/// What filtering found and did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterReport {
    /// Millimetres on the head's surface per texel of the map, the median
    /// over the triangles that carry detail.
    pub millimetres_per_texel: f64,
    /// The high-pass standard deviation used, in texels.
    pub high_pass_texels: Option<f64>,
    /// The magnitude values were limited to.
    pub clipped_at: Option<f32>,
}

/// Filter `values`, a `width` by `height` map in millimetres with row 0 at
/// v = 1, laid out over the head whose posed vertices in millimetres are
/// `vertices`.
pub fn filter_detail(
    values: &mut [f32],
    width: usize,
    height: usize,
    faces: &[[u32; 3]],
    layout: &UvLayout,
    vertices: &[[f32; 3]],
    filter: DetailFilter,
) -> Result<FilterReport, DetailError> {
    if width == 0 || height == 0 || values.len() != width * height {
        return Err(DetailError::Size);
    }
    if layout.faces.len() != faces.len()
        || faces
            .iter()
            .flatten()
            .any(|&vertex| vertex as usize >= vertices.len())
        || layout
            .faces
            .iter()
            .flatten()
            .any(|&corner| corner as usize >= layout.coords.len())
    {
        return Err(DetailError::Layout);
    }
    let detail = detail_mask(values);
    let millimetres_per_texel = surface_scale(values, width, height, faces, layout, vertices);
    let mut report = FilterReport {
        millimetres_per_texel,
        high_pass_texels: None,
        clipped_at: None,
    };
    if let Some(width_mm) = filter.high_pass_mm
        && millimetres_per_texel > 0.0
    {
        // A Gaussian with standard deviation s passes about half of a
        // feature of width 2.5 s, so features much wider are removed.
        let sigma = width_mm / millimetres_per_texel / 2.5;
        high_pass(values, &detail, width, height, sigma);
        report.high_pass_texels = Some(sigma);
    }
    if let Some(percentile) = filter.clip_percentile {
        let magnitudes: Vec<f32> = values
            .iter()
            .zip(&detail)
            .filter(|(_, inside)| **inside)
            .map(|(value, _)| value.abs())
            .collect();
        if let Some(limit) = percentile_of(magnitudes, percentile) {
            for value in values.iter_mut() {
                *value = value.clamp(-limit, limit);
            }
            report.clipped_at = Some(limit);
        }
    }
    Ok(report)
}

/// Texels that carry detail.
pub fn detail_mask(values: &[f32]) -> Vec<bool> {
    let largest = values
        .iter()
        .fold(0.0f32, |most, value| most.max(value.abs()));
    let threshold = largest * DETAIL_SHARE;
    values
        .iter()
        .map(|value| largest > 0.0 && value.abs() > threshold)
        .collect()
}

/// The value at `percentile`, 0 to 100, of `values`, by nearest rank; `None`
/// when there are none.
pub fn percentile_of(mut values: Vec<f32>, percentile: f64) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f32::total_cmp);
    let rank = (percentile.clamp(0.0, 100.0) / 100.0 * (values.len() - 1) as f64).round();
    Some(values[rank as usize])
}

/// Millimetres per texel: for each triangle, the square root of its surface
/// area over its area in the map, the median over triangles whose centre
/// carries detail, or over all triangles when none does.
fn surface_scale(
    values: &[f32],
    width: usize,
    height: usize,
    faces: &[[u32; 3]],
    layout: &UvLayout,
    vertices: &[[f32; 3]],
) -> f64 {
    let detail = detail_mask(values);
    let mut on_detail = Vec::new();
    let mut everywhere = Vec::new();
    for (face, corners) in faces.iter().zip(&layout.faces) {
        let [a, b, c] = face.map(|vertex| vertices[vertex as usize].map(f64::from));
        let surface = 0.5 * norm(cross(sub(b, a), sub(c, a)));
        let [p, q, r] = corners.map(|corner| {
            let [u, v] = layout.coords[corner as usize];
            [f64::from(u) * width as f64, f64::from(v) * height as f64]
        });
        let texels = 0.5 * ((q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])).abs();
        if texels < 1e-9 || !surface.is_finite() {
            continue;
        }
        let scale = (surface / texels).sqrt();
        everywhere.push(scale as f32);
        let centre = [(p[0] + q[0] + r[0]) / 3.0, (p[1] + q[1] + r[1]) / 3.0];
        let column = (centre[0].floor().max(0.0) as usize).min(width - 1);
        let row = ((height as f64 - centre[1]).floor().max(0.0) as usize).min(height - 1);
        if detail[row * width + column] {
            on_detail.push(scale as f32);
        }
    }
    let chosen = if on_detail.is_empty() {
        everywhere
    } else {
        on_detail
    };
    percentile_of(chosen, 50.0).map_or(0.0, f64::from)
}

/// Subtract a Gaussian blur of the detail, taken over the detail alone, from
/// the detail. Texels outside the detail are left unchanged.
fn high_pass(values: &mut [f32], detail: &[bool], width: usize, height: usize, sigma: f64) {
    if sigma <= 0.0 || !sigma.is_finite() {
        return;
    }
    let reach = (KERNEL_REACH * sigma).ceil().min(width.max(height) as f64) as usize;
    let kernel: Vec<f64> = (0..=reach)
        .map(|offset| (-(offset as f64).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();
    let weight: Vec<f64> = detail
        .iter()
        .map(|&inside| f64::from(u8::from(inside)))
        .collect();
    let weighted: Vec<f64> = values
        .iter()
        .zip(&weight)
        .map(|(value, weight)| f64::from(*value) * weight)
        .collect();
    let blur = |field: &[f64]| {
        blur_rows(
            &blur_columns(field, width, height, &kernel),
            width,
            height,
            &kernel,
        )
    };
    let (sum, total) = (blur(&weighted), blur(&weight));
    for (index, value) in values.iter_mut().enumerate() {
        if detail[index] && total[index] > 1e-9 {
            *value -= (sum[index] / total[index]) as f32;
        }
    }
}

fn blur_rows(field: &[f64], width: usize, height: usize, kernel: &[f64]) -> Vec<f64> {
    let reach = kernel.len() - 1;
    let mut out = vec![0.0; field.len()];
    for row in 0..height {
        let line = &field[row * width..(row + 1) * width];
        for column in 0..width {
            let first = column.saturating_sub(reach);
            let last = (column + reach).min(width - 1);
            out[row * width + column] = (first..=last)
                .map(|at| line[at] * kernel[at.abs_diff(column)])
                .sum();
        }
    }
    out
}

fn blur_columns(field: &[f64], width: usize, height: usize, kernel: &[f64]) -> Vec<f64> {
    let reach = kernel.len() - 1;
    let mut out = vec![0.0; field.len()];
    for row in 0..height {
        let first = row.saturating_sub(reach);
        let last = (row + reach).min(height - 1);
        for column in 0..width {
            out[row * width + column] = (first..=last)
                .map(|at| field[at * width + column] * kernel[at.abs_diff(row)])
                .sum();
        }
    }
    out
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: [f64; 3]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat 100 by 100 mm square laid over the whole map, so one texel of
    /// a `side` texel map is 100 / side millimetres.
    fn square(side: usize) -> (Vec<[u32; 3]>, UvLayout, Vec<[f32; 3]>, usize) {
        let vertices = vec![
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [100.0, 100.0, 0.0],
            [0.0, 100.0, 0.0],
        ];
        let faces = vec![[0, 1, 2], [0, 2, 3]];
        let layout = UvLayout {
            coords: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            faces: faces.clone(),
        };
        (faces, layout, vertices, side)
    }

    fn map(side: usize, value: impl Fn(f64, f64) -> f32) -> Vec<f32> {
        (0..side * side)
            .map(|index| {
                let (row, column) = (index / side, index % side);
                value(column as f64 + 0.5, row as f64 + 0.5)
            })
            .collect()
    }

    #[test]
    fn the_surface_scale_comes_from_the_head() {
        let (faces, layout, vertices, side) = square(64);
        let mut values = map(side, |_, _| 1.0);
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter::default(),
        )
        .unwrap();
        assert!((report.millimetres_per_texel - 100.0 / 64.0).abs() < 1e-6);
        assert!(
            values.iter().all(|&value| value == 1.0),
            "no filter, no change"
        );
    }

    #[test]
    fn high_pass_removes_a_broad_bulge_and_keeps_a_narrow_crease() {
        let (faces, layout, vertices, side) = square(128);
        // A bulge 40 mm wide plus a crease about 2 mm wide down the middle.
        let bulge = |x: f64, y: f64| {
            let (dx, dy) = (x - 64.0, y - 64.0);
            (-(dx * dx + dy * dy) / (2.0 * 13.0 * 13.0)).exp()
        };
        let crease = |x: f64| -0.5 * (-(x - 64.0).powi(2) / 2.0).exp();
        let mut values = map(side, |x, y| (bulge(x, y) + crease(x)) as f32);
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: Some(8.0),
                clip_percentile: None,
            },
        )
        .unwrap();
        assert!(report.high_pass_texels.is_some());
        // Away from the crease, the bulge's 1 mm crown is almost gone.
        let at = |x: usize, y: usize| values[y * side + x];
        assert!(at(80, 64).abs() < 0.1, "{}", at(80, 64));
        // The crease keeps most of its depth relative to its surroundings.
        assert!(
            at(64, 64) - at(70, 64) < -0.3,
            "{} {}",
            at(64, 64),
            at(70, 64)
        );
    }

    #[test]
    fn clipping_limits_isolated_spikes() {
        let (faces, layout, vertices, side) = square(32);
        let mut values = map(side, |x, _| if x < 1.0 { 3.0 } else { 0.2 });
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: None,
                clip_percentile: Some(90.0),
            },
        )
        .unwrap();
        assert_eq!(report.clipped_at, Some(0.2));
        assert!(values.iter().all(|&value| value == 0.2));
    }

    #[test]
    fn texels_without_detail_stay_unchanged() {
        let (faces, layout, vertices, side) = square(64);
        let mut values = map(side, |x, _| if x < 32.0 { 0.0 } else { 1.0 });
        filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: Some(10.0),
                clip_percentile: None,
            },
        )
        .unwrap();
        assert!(values[..32].iter().all(|&value| value == 0.0));
        // A uniform patch of detail has no finer detail in it.
        assert!(values[48 * 64 + 48].abs() < 1e-4);
    }

    #[test]
    fn mismatched_inputs_are_refused() {
        let (faces, layout, vertices, side) = square(8);
        let mut values = vec![0.0; side * side - 1];
        assert_eq!(
            filter_detail(
                &mut values,
                side,
                side,
                &faces,
                &layout,
                &vertices,
                DetailFilter::default()
            )
            .unwrap_err(),
            DetailError::Size
        );
        let mut values = vec![0.0; side * side];
        assert_eq!(
            filter_detail(
                &mut values,
                side,
                side,
                &faces,
                &layout,
                &vertices[..3],
                DetailFilter::default()
            )
            .unwrap_err(),
            DetailError::Layout
        );
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<f32> = (0..=100).map(|value| value as f32).collect();
        assert_eq!(percentile_of(values.clone(), 95.0), Some(95.0));
        assert_eq!(percentile_of(values, 0.0), Some(0.0));
        assert_eq!(percentile_of(Vec::new(), 50.0), None);
    }
}
