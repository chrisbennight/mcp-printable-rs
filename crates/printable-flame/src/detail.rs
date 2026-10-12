//! Surface detail maps in FLAME's texture layout, filtered for printing.
//!
//! A displacement map holds a distance along the surface normal for each
//! texel. Detail is where the map's magnitude is a noticeable share of its
//! largest value. Filtering changes only the detail, and sets everything else
//! to exactly zero, so rounding noise there cannot later be taken for detail.
//! Blurs are taken over the head's surface, the texels inside a triangle of
//! the texture layout, so the gaps between the layout's pieces do not pull
//! nearby detail toward zero.

use thiserror::Error;

use crate::UvLayout;

/// Texels whose magnitude is below this share of the map's largest magnitude
/// are not detail. Maps stored as 16-bit integers carry rounding noise of
/// about one part in 65,535 where they are meant to be zero.
const DETAIL_SHARE: f32 = 0.005;
/// Successive box blurs that together approximate a Gaussian blur. Three
/// keep the error within a few percent, and each costs the same whatever
/// its width.
const BOX_PASSES: usize = 3;

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

const NO_FACE: u32 = u32::MAX;

/// What filtering found and did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterReport {
    /// Millimetres on the head's surface per texel of the map: over the
    /// texels that carry detail, the median of their triangle's scale.
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
    let covering = covering_faces(width, height, layout);
    let millimetres_per_texel =
        surface_scale(&detail, &covering, faces, layout, vertices, [width, height]);
    let mut report = FilterReport {
        millimetres_per_texel,
        high_pass_texels: None,
        clipped_at: None,
    };
    if !detail.contains(&true)
        || (filter.high_pass_mm.is_none() && filter.clip_percentile.is_none())
    {
        return Ok(report);
    }
    for (value, inside) in values.iter_mut().zip(&detail) {
        if !inside {
            *value = 0.0;
        }
    }
    if let Some(width_mm) = filter.high_pass_mm
        && millimetres_per_texel > 0.0
    {
        // A Gaussian with standard deviation s passes about half of a
        // feature of width 2.5 s, so features much wider are removed.
        let sigma = width_mm / millimetres_per_texel / 2.5;
        if high_pass(values, &detail, &covering, [width, height], sigma) {
            report.high_pass_texels = Some(sigma);
        }
    }
    if let Some(percentile) = filter.clip_percentile {
        // Taken over what still carries detail after the high-pass, so
        // texels it emptied cannot pull the limit toward zero.
        let remaining = detail_mask(values);
        let magnitudes: Vec<f32> = values
            .iter()
            .zip(&remaining)
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

/// For each texel, the triangle whose texture coordinates cover its centre,
/// or `NO_FACE` off the head's surface.
fn covering_faces(width: usize, height: usize, layout: &UvLayout) -> Vec<u32> {
    let mut covering = vec![NO_FACE; width * height];
    for (index, corners) in layout.faces.iter().enumerate() {
        let [a, b, c] = corners.map(|corner| {
            let [u, v] = layout.coords[corner as usize];
            [
                f64::from(u) * width as f64,
                (1.0 - f64::from(v)) * height as f64,
            ]
        });
        let area = edge(a, b, c);
        if area.abs() < 1e-12 {
            continue;
        }
        let low = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1])];
        let high = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1])];
        let span = |low: f64, high: f64, size: usize| {
            ((low - 0.5).floor().max(0.0) as usize).min(size)
                ..(((high - 0.5).ceil().max(-1.0) + 1.0) as usize).min(size)
        };
        for row in span(low[1], high[1], height) {
            for column in span(low[0], high[0], width) {
                let centre = [column as f64 + 0.5, row as f64 + 0.5];
                let shares = [edge(b, c, centre), edge(c, a, centre), edge(a, b, centre)];
                if shares.iter().all(|share| share / area >= -1e-9) {
                    covering[row * width + column] = index as u32;
                }
            }
        }
    }
    covering
}

fn edge(a: [f64; 2], b: [f64; 2], point: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (point[1] - a[1]) - (b[1] - a[1]) * (point[0] - a[0])
}

/// Millimetres per texel. Each triangle's scale is the square root of its
/// surface area over its area in the map; the result is the median of that
/// scale over the texels that carry detail, over every texel on the surface
/// when none does, and over the triangles when no texel is covered.
fn surface_scale(
    detail: &[bool],
    covering: &[u32],
    faces: &[[u32; 3]],
    layout: &UvLayout,
    vertices: &[[f32; 3]],
    [width, height]: [usize; 2],
) -> f64 {
    let scales: Vec<Option<f32>> = faces
        .iter()
        .zip(&layout.faces)
        .map(|(face, corners)| {
            let [a, b, c] = face.map(|vertex| vertices[vertex as usize].map(f64::from));
            let surface = 0.5 * norm(cross(sub(b, a), sub(c, a)));
            let [p, q, r] = corners.map(|corner| {
                let [u, v] = layout.coords[corner as usize];
                [f64::from(u) * width as f64, f64::from(v) * height as f64]
            });
            let texels = 0.5 * edge(p, q, r).abs();
            (texels >= 1e-9 && surface.is_finite()).then(|| (surface / texels).sqrt() as f32)
        })
        .collect();
    let over = |wanted: &dyn Fn(usize) -> bool| -> Vec<f32> {
        covering
            .iter()
            .enumerate()
            .filter(|&(index, &face)| face != NO_FACE && wanted(index))
            .filter_map(|(_, &face)| scales[face as usize])
            .collect()
    };
    [
        over(&|index| detail[index]),
        over(&|_| true),
        scales.iter().flatten().copied().collect(),
    ]
    .into_iter()
    .find(|chosen| !chosen.is_empty())
    .and_then(|chosen| percentile_of(chosen, 50.0))
    .map_or(0.0, f64::from)
}

/// Subtract a blur of the map, taken over the head's surface, from the
/// detail. The blur approximates a Gaussian of standard deviation `sigma`
/// texels. Whether anything was subtracted: a blur no wider than a texel is
/// the map itself, and removes nothing.
fn high_pass(
    values: &mut [f32],
    detail: &[bool],
    covering: &[u32],
    [width, height]: [usize; 2],
    sigma: f64,
) -> bool {
    if sigma <= 0.0 || !sigma.is_finite() {
        return false;
    }
    let widths = box_widths(sigma);
    if widths.iter().all(|&width| width == 1) {
        return false;
    }
    let mut total: Vec<f64> = covering
        .iter()
        .map(|&face| f64::from(u8::from(face != NO_FACE)))
        .collect();
    let mut sum: Vec<f64> = values
        .iter()
        .zip(&total)
        .map(|(value, weight)| f64::from(*value) * weight)
        .collect();
    for box_width in widths {
        let radius = (box_width / 2).min(width.max(height));
        for field in [&mut sum, &mut total] {
            box_rows(field, width, height, radius);
            box_columns(field, width, height, radius);
        }
    }
    for (index, value) in values.iter_mut().enumerate() {
        if detail[index] && total[index] > 1e-9 {
            *value -= (sum[index] / total[index]) as f32;
        }
    }
    true
}

/// Odd widths of the box blurs whose succession has the variance of a
/// Gaussian with standard deviation `sigma`: the smaller width for the first
/// passes and the next odd width for the rest.
fn box_widths(sigma: f64) -> [usize; BOX_PASSES] {
    let passes = BOX_PASSES as f64;
    let ideal = (12.0 * sigma * sigma / passes + 1.0).sqrt();
    let mut lower = ideal.floor().max(1.0) as usize;
    if lower.is_multiple_of(2) {
        lower -= 1;
    }
    let small = lower as f64;
    let narrow =
        ((12.0 * sigma * sigma - passes * small * small - 4.0 * passes * small - 3.0 * passes)
            / (-4.0 * small - 4.0))
            .round()
            .clamp(0.0, passes) as usize;
    std::array::from_fn(|pass| if pass < narrow { lower } else { lower + 2 })
}

/// Replace each value with the sum of the values within `radius` along its
/// row. Values beyond the map's edge count as zero.
fn box_rows(field: &mut [f64], width: usize, height: usize, radius: usize) {
    let mut prefix = vec![0.0; width + 1];
    for line in field.chunks_exact_mut(width).take(height) {
        for (index, value) in line.iter().enumerate() {
            prefix[index + 1] = prefix[index] + value;
        }
        for (index, value) in line.iter_mut().enumerate() {
            *value = prefix[(index + radius + 1).min(width)] - prefix[index.saturating_sub(radius)];
        }
    }
}

/// As `box_rows`, along each column.
fn box_columns(field: &mut [f64], width: usize, height: usize, radius: usize) {
    let mut prefix = vec![0.0; height + 1];
    for column in 0..width {
        for row in 0..height {
            prefix[row + 1] = prefix[row] + field[row * width + column];
        }
        for row in 0..height {
            field[row * width + column] =
                prefix[(row + radius + 1).min(height)] - prefix[row.saturating_sub(radius)];
        }
    }
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
    fn rounding_noise_outside_the_detail_becomes_zero_when_filtered() {
        let (faces, layout, vertices, side) = square(64);
        let mut values = map(side, |x, _| if x < 32.0 { 1e-6 } else { 1.0 });
        let original = values.clone();
        filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter::default(),
        )
        .unwrap();
        assert_eq!(values, original, "no filter, no change");
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
    }

    #[test]
    fn a_map_without_detail_is_left_alone() {
        let (faces, layout, vertices, side) = square(16);
        let mut values = vec![0.0; side * side];
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: Some(50.0),
                clip_percentile: Some(90.0),
            },
        )
        .unwrap();
        assert_eq!((report.high_pass_texels, report.clipped_at), (None, None));
        assert!(values.iter().all(|&value| value == 0.0));
    }

    #[test]
    fn a_filter_narrower_than_a_texel_removes_nothing() {
        // A texel is 100 / 64 mm, so a 1 mm filter is far below a texel.
        let (faces, layout, vertices, side) = square(64);
        let mut values = map(side, |x, y| {
            ((x * 0.3).sin() * (y * 0.2).cos()) as f32 + 2.0
        });
        let original = values.clone();
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: Some(1.0),
                clip_percentile: None,
            },
        )
        .unwrap();
        assert_eq!(report.high_pass_texels, None);
        assert_eq!(values, original);
    }

    #[test]
    fn an_isolated_crease_on_a_flat_surface_keeps_its_depth() {
        let (faces, layout, vertices, side) = square(128);
        let mut values = map(
            side,
            |x, _| if (63.0..65.0).contains(&x) { -0.5 } else { 0.0 },
        );
        filter_detail(
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
        let crease = values[64 * side + 63];
        assert!(crease < -0.35, "{crease}");
        assert_eq!(values[64 * side + 10], 0.0, "the flat surface stays flat");
    }

    #[test]
    fn the_scale_comes_from_the_texels_that_carry_detail() {
        // Two squares of the same shape in the map: on the left 100 mm
        // across, on the right 10 mm. A narrow line of detail on the right
        // misses the centres of the right-hand triangles.
        let vertices = vec![
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [100.0, 100.0, 0.0],
            [0.0, 100.0, 0.0],
            [200.0, 0.0, 0.0],
            [210.0, 0.0, 0.0],
            [210.0, 10.0, 0.0],
            [200.0, 10.0, 0.0],
        ];
        let faces = vec![[0, 1, 2], [0, 2, 3], [4, 5, 6], [4, 6, 7]];
        let layout = UvLayout {
            coords: vec![
                [0.0, 0.0],
                [0.5, 0.0],
                [0.5, 1.0],
                [0.0, 1.0],
                [0.5, 0.0],
                [1.0, 0.0],
                [1.0, 1.0],
                [0.5, 1.0],
            ],
            faces: faces.clone(),
        };
        let side = 64;
        let mut values = map(
            side,
            |x, _| if (40.0..41.0).contains(&x) { 1.0 } else { 0.0 },
        );
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
        // 10 by 10 mm over 32 by 64 texels.
        let expected = (100.0f64 / (32.0 * 64.0)).sqrt();
        assert!(
            (report.millimetres_per_texel - expected).abs() < 1e-4,
            "{report:?}"
        );
    }

    #[test]
    fn clipping_after_a_high_pass_counts_only_the_detail_that_remains() {
        // An even 1 mm raise, which the high-pass removes, beside a crease it
        // keeps; clipping at the 95th percentile must keep the crease.
        let (faces, layout, vertices, side) = square(256);
        let mut values = map(side, |x, _| {
            if (127.0..129.0).contains(&x) {
                0.5
            } else {
                1.0
            }
        });
        let report = filter_detail(
            &mut values,
            side,
            side,
            &faces,
            &layout,
            &vertices,
            DetailFilter {
                high_pass_mm: Some(3.0),
                clip_percentile: Some(95.0),
            },
        )
        .unwrap();
        assert!(
            report.clipped_at.is_some_and(|limit| limit > 0.1),
            "{report:?}"
        );
        let crease = values[128 * side + 127];
        assert!(crease < -0.1, "{crease}");
    }

    #[test]
    fn box_blurs_match_the_gaussian_variance() {
        // Below a texel the boxes cannot blur at all, so nothing is removed.
        assert_eq!(box_widths(0.4), [1, 1, 1]);
        for sigma in [1.0, 3.0, 7.5, 40.0] {
            let widths = box_widths(sigma);
            assert!(widths.iter().all(|width| width % 2 == 1), "{widths:?}");
            // A box of odd width w has variance (w² - 1) / 12.
            let variance: f64 = widths
                .iter()
                .map(|&width| ((width * width - 1) as f64) / 12.0)
                .sum();
            assert!(
                (variance.sqrt() - sigma).abs() < 0.35_f64.max(0.05 * sigma),
                "{sigma}: {widths:?}"
            );
        }
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
