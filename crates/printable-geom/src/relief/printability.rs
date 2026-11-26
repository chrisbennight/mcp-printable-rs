//! Whether a relief solid can be printed, and released from a mould.
//!
//! The solid is checked by the crate's mesh analysis: closed, manifold,
//! consistently oriented and free of self-intersections, with positive
//! volume. The relief map is checked against the printer: its height range
//! against minimum and maximum relief heights, and its detail against the
//! nozzle. Detail narrower than the nozzle is found by morphological
//! opening and closing with a disc of the nozzle's diameter: a ridge or
//! peak the disc cannot fit inside loses the height that the opening
//! removes, and a groove or pit the disc cannot fit into loses the depth
//! the closing fills. Losses smaller than one layer are not counted, since
//! a layer is the finest height a printer forms. A mould is checked for
//! undercuts along the direction it releases.

use serde::Serialize;

use super::{ReliefError, ReliefMap, positive};
use crate::{
    DraftOptions, DraftReport, TriangleMesh, ValidationOptions, ValidationReport, analyze_mesh,
    limit,
};

/// A disc this many samples across, or more, is refused: the check would
/// cost more than the relief it measures. Resample the relief more coarsely.
const MAX_DISC_RADIUS: usize = 64;

/// How a mould releases the relief.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Release {
    /// Direction the cast part, or the mould, moves away; +Z lifts a cast
    /// out of an upward-facing cavity.
    pub pull_direction: [f64; 3],
    /// Faces leaning less than this, in degrees, are reported as shallow.
    pub minimum_draft_degrees: f64,
}

/// Printer and mould limits for [`check_relief`].
#[derive(Debug, Clone, PartialEq)]
pub struct PrintabilityOptions {
    pub nozzle_mm: f64,
    /// The finest height step the printer forms.
    pub layer_mm: f64,
    /// The relief must rise at least this far; without it, one layer.
    pub minimum_relief_mm: Option<f64>,
    /// The relief must rise no further than this.
    pub maximum_relief_mm: Option<f64>,
    /// Check draft and undercuts along a mould's release.
    pub release: Option<Release>,
}

impl Default for PrintabilityOptions {
    fn default() -> Self {
        Self {
            nozzle_mm: 0.4,
            layer_mm: 0.2,
            minimum_relief_mm: None,
            maximum_relief_mm: None,
            release: None,
        }
    }
}

/// Relief detail a nozzle cannot reproduce.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NarrowDetail {
    /// Area where more than one layer of height is lost.
    pub area_mm2: f64,
    /// The most height lost anywhere.
    pub maximum_loss_mm: f64,
}

/// The printability of a relief solid.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReliefPrintability {
    /// Closed, manifold, consistently oriented, free of self-intersections
    /// and of positive volume, by the mesh analysis.
    pub solid: bool,
    pub volume_mm3: Option<f64>,
    /// Height from the relief's lowest point to its highest.
    pub relief_height_mm: f64,
    pub minimum_relief_mm: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_relief_mm: Option<f64>,
    pub relief_height_within_limits: bool,
    /// Ridges and peaks narrower than the nozzle.
    pub narrow_ridges: NarrowDetail,
    /// Grooves and pits narrower than the nozzle.
    pub narrow_grooves: NarrowDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftReport>,
    /// Whether the solid is sound, its height within limits and, for a
    /// mould, free of undercuts. Narrow detail prints but loses fidelity,
    /// so it is reported without failing the check.
    pub passed: bool,
    /// The full mesh analysis.
    pub validation: ValidationReport,
}

/// Check a relief map and the solid made from it.
pub fn check_relief(
    map: &ReliefMap,
    solid: &TriangleMesh,
    options: &PrintabilityOptions,
) -> Result<ReliefPrintability, ReliefError> {
    if !positive(options.nozzle_mm) || !positive(options.layer_mm) {
        return Err(ReliefError::InvalidOptions(
            "nozzle_mm and layer_mm must be finite positive numbers".to_string(),
        ));
    }
    let minimum_relief_mm = options.minimum_relief_mm.unwrap_or(options.layer_mm);
    if !(minimum_relief_mm.is_finite() && minimum_relief_mm >= 0.0)
        || options
            .maximum_relief_mm
            .is_some_and(|maximum| !positive(maximum))
    {
        return Err(ReliefError::InvalidOptions(
            "relief height limits must be finite, and the maximum positive".to_string(),
        ));
    }
    if !map.spacing_mm.iter().all(|spacing| positive(*spacing)) {
        return Err(ReliefError::InvalidOptions(
            "spacing_mm must be finite positive numbers".to_string(),
        ));
    }
    let heights = map.heights_mm.values();
    let low = heights.iter().copied().fold(f64::INFINITY, f64::min);
    let high = heights.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let relief_height_mm = high - low;
    let relief_height_within_limits = relief_height_mm >= minimum_relief_mm
        && options
            .maximum_relief_mm
            .is_none_or(|maximum| relief_height_mm <= maximum);
    let [narrow_ridges, narrow_grooves] = narrow_detail(map, options)?;

    let validation = analyze_mesh(
        solid,
        ValidationOptions {
            draft: options.release.map(|release| DraftOptions {
                pull_direction: release.pull_direction,
                minimum_draft_degrees: release.minimum_draft_degrees,
                parting_offset_mm: None,
            }),
            ..ValidationOptions::default()
        },
    )?;
    let volume_mm3 = validation
        .solid_properties
        .as_ref()
        .map(|properties| properties.volume_mm3);
    let solid_ok = validation.solid_geometry && volume_mm3.is_some_and(|volume| volume > 0.0);
    let draft = validation.draft.clone();
    let undercut_free = draft.as_ref().is_none_or(|draft| draft.undercut.faces == 0);
    Ok(ReliefPrintability {
        solid: solid_ok,
        volume_mm3,
        relief_height_mm,
        minimum_relief_mm,
        maximum_relief_mm: options.maximum_relief_mm,
        relief_height_within_limits,
        narrow_ridges,
        narrow_grooves,
        draft,
        passed: solid_ok && relief_height_within_limits && undercut_free,
        validation,
    })
}

/// Ridges, then grooves, narrower than the nozzle.
fn narrow_detail(
    map: &ReliefMap,
    options: &PrintabilityOptions,
) -> Result<[NarrowDetail; 2], ReliefError> {
    let grid = &map.heights_mm;
    let (width, height) = (grid.width(), grid.height());
    let radius = options.nozzle_mm / 2.0;
    let [radius_x, radius_y] = map.spacing_mm.map(|spacing| radius / spacing);
    let none = || NarrowDetail {
        area_mm2: 0.0,
        maximum_loss_mm: 0.0,
    };
    // A disc within one sample of a point fits inside any detail the
    // samples can show.
    if radius_x < 1.0 && radius_y < 1.0 {
        return Ok([none(), none()]);
    }
    if radius_x.max(radius_y) >= MAX_DISC_RADIUS as f64 {
        return Err(ReliefError::InvalidOptions(format!(
            "the nozzle spans more than {MAX_DISC_RADIUS} relief samples in radius; resample the relief more coarsely"
        )));
    }
    // Half-width in samples of the disc's row at each vertical offset.
    let rows = radius_y.floor() as usize;
    let half_widths: Vec<usize> = (0..=rows)
        .map(|offset| {
            let fraction = offset as f64 / radius_y.max(1.0);
            (radius_x * (1.0 - fraction * fraction).max(0.0).sqrt()).floor() as usize
        })
        .collect();
    let heights = grid.values();
    let opened = filter(
        &filter(heights, width, height, &half_widths, Extreme::Min)?,
        width,
        height,
        &half_widths,
        Extreme::Max,
    )?;
    let closed = filter(
        &filter(heights, width, height, &half_widths, Extreme::Max)?,
        width,
        height,
        &half_widths,
        Extreme::Min,
    )?;
    let cell = map.spacing_mm[0] * map.spacing_mm[1];
    let measure = |loss: &dyn Fn(usize) -> f64| {
        let mut detail = none();
        for i in 0..heights.len() {
            let lost = loss(i);
            if lost > options.layer_mm {
                detail.area_mm2 += cell;
            }
            detail.maximum_loss_mm = detail.maximum_loss_mm.max(lost);
        }
        detail
    };
    Ok([
        measure(&|i| heights[i] - opened[i]),
        measure(&|i| closed[i] - heights[i]),
    ])
}

#[derive(Clone, Copy, PartialEq)]
enum Extreme {
    Min,
    Max,
}

/// The least or greatest value under a disc, given as the half-width of its
/// row at each vertical offset, centred on each sample. The disc is cut off
/// at the edges of the grid.
fn filter(
    values: &[f64],
    width: usize,
    height: usize,
    half_widths: &[usize],
    extreme: Extreme,
) -> Result<Vec<f64>, ReliefError> {
    let better = |a: f64, b: f64| match extreme {
        Extreme::Min => a.min(b),
        Extreme::Max => a.max(b),
    };
    let mut out = vec![
        match extreme {
            Extreme::Min => f64::INFINITY,
            Extreme::Max => f64::NEG_INFINITY,
        };
        values.len()
    ];
    let mut line = vec![0.0; width];
    for (offset, &half_width) in half_widths.iter().enumerate() {
        limit::check()?;
        for source in 0..height {
            sliding(
                &values[source * width..(source + 1) * width],
                half_width,
                extreme,
                &mut line,
            );
            // The disc's rows at this offset above and below each target
            // row read this source row.
            let below = source.checked_sub(offset).filter(|_| offset > 0);
            for target in [Some(source + offset), below].into_iter().flatten() {
                if target < height {
                    let row = &mut out[target * width..(target + 1) * width];
                    for (value, candidate) in row.iter_mut().zip(&line) {
                        *value = better(*value, *candidate);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The least or greatest value within `half_width` of each entry of `row`.
fn sliding(row: &[f64], half_width: usize, extreme: Extreme, out: &mut [f64]) {
    let wins = |a: f64, b: f64| match extreme {
        Extreme::Min => a <= b,
        Extreme::Max => a >= b,
    };
    // Indices whose values are monotonic from the front of the window.
    let mut window = std::collections::VecDeque::with_capacity(2 * half_width + 1);
    let length = row.len();
    let mut next = 0;
    for (position, value) in out.iter_mut().enumerate() {
        let end = (position + half_width + 1).min(length);
        while next < end {
            while window
                .back()
                .is_some_and(|&last: &usize| wins(row[next], row[last]))
            {
                window.pop_back();
            }
            window.push_back(next);
            next += 1;
        }
        while window
            .front()
            .is_some_and(|&first| first + half_width < position)
        {
            window.pop_front();
        }
        *value = row[*window.front().expect("the window holds its centre")];
    }
}
