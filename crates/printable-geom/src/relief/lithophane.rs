//! Lithophanes: thin plates whose thickness varies so that light shining
//! through them shows an image.
//!
//! Light passing through a plate `t` millimetres thick keeps a fraction
//! `exp(-k * t)` of its intensity (the Beer-Lambert law), where `k` is the
//! filament's attenuation per millimetre. [`ToneMapping`] gives each sample's
//! linear-light brightness a transmission between the thickest plate's and
//! the thinnest plate's, in proportion, and inverts the law to find the
//! thickness. Seen against an even backlight, the plate's brightness then
//! follows the image's linear brightness within the plate's contrast range.
//!
//! [`lithophane_solid`] closes the thickness map into a plate whose flat face
//! lies on Z = 0 and whose relief faces +Z. The flat face is the one viewed,
//! with the relief facing the light, so the image is mirrored to read the
//! right way round from the flat side. [`bend_around_cylinder`] wraps such a
//! plate around a vertical cylinder, flat face outward.

use super::{Grid, ReliefError, ReliefMap, ReliefSolid, SolidOptions, positive, relief_solid};
use crate::{TriangleMesh, limit};

/// The value an sRGB-encoded component in 0 to 1 stands for in linear light.
pub fn srgb_to_linear(encoded: f64) -> f64 {
    let encoded = encoded.clamp(0.0, 1.0);
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB encoding of a linear-light value in 0 to 1.
pub fn linear_to_srgb(linear: f64) -> f64 {
    let linear = linear.clamp(0.0, 1.0);
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// How brightness becomes plate thickness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneMapping {
    /// The thickness of the brightest parts.
    pub min_thickness_mm: f64,
    /// The thickness of the darkest parts.
    pub max_thickness_mm: f64,
    /// The filament's attenuation `k`: light keeps `exp(-k * t)` of its
    /// intensity through `t` millimetres.
    pub attenuation_per_mm: f64,
}

impl ToneMapping {
    pub fn validate(&self) -> Result<(), ReliefError> {
        if !positive(self.min_thickness_mm)
            || !self.max_thickness_mm.is_finite()
            || self.max_thickness_mm <= self.min_thickness_mm
            || !positive(self.attenuation_per_mm)
        {
            return Err(ReliefError::InvalidOptions(
                "the thicknesses must be positive with the maximum above the minimum, and the attenuation positive".into(),
            ));
        }
        Ok(())
    }

    /// The fraction of light a plate `thickness_mm` thick transmits.
    pub fn transmission(&self, thickness_mm: f64) -> f64 {
        (-self.attenuation_per_mm * thickness_mm).exp()
    }

    /// The thickness that shows `linear` brightness, from 0 for black to 1
    /// for white, clamped to the thickness range.
    pub fn thickness_mm(&self, linear: f64) -> f64 {
        let darkest = self.transmission(self.max_thickness_mm);
        let brightest = self.transmission(self.min_thickness_mm);
        let wanted = darkest + linear.clamp(0.0, 1.0) * (brightest - darkest);
        (-wanted.ln() / self.attenuation_per_mm).clamp(self.min_thickness_mm, self.max_thickness_mm)
    }

    /// The linear brightness a plate `thickness_mm` thick shows on the scale
    /// [`thickness_mm`](Self::thickness_mm) maps from: 0 at the thickest
    /// plate and 1 at the thinnest.
    pub fn shown(&self, thickness_mm: f64) -> f64 {
        let darkest = self.transmission(self.max_thickness_mm);
        let brightest = self.transmission(self.min_thickness_mm);
        (self.transmission(thickness_mm) - darkest) / (brightest - darkest)
    }

    /// The brightness of a plate `thickness_mm` thick relative to the
    /// thinnest plate's, as a backlit print looks: the thickest plate is
    /// not black but `1 / contrast_ratio`.
    pub fn backlit(&self, thickness_mm: f64) -> f64 {
        self.transmission(thickness_mm) / self.transmission(self.min_thickness_mm)
    }

    /// How many times brighter the thinnest plate is than the thickest.
    pub fn contrast_ratio(&self) -> f64 {
        (self.attenuation_per_mm * (self.max_thickness_mm - self.min_thickness_mm)).exp()
    }
}

/// The plate thickness for every sample of a linear-light brightness grid,
/// as a relief map whose heights are the thicknesses.
pub fn lithophane_map(
    brightness: &Grid<f64>,
    spacing_mm: [f64; 2],
    tone: &ToneMapping,
) -> Result<ReliefMap, ReliefError> {
    tone.validate()?;
    let mut thickness = Vec::with_capacity(brightness.values().len());
    for row in brightness.values().chunks(brightness.width()) {
        limit::check()?;
        thickness.extend(row.iter().map(|value| tone.thickness_mm(*value)));
    }
    Ok(ReliefMap {
        heights_mm: Grid::new(brightness.width(), brightness.height(), thickness)?,
        spacing_mm,
    })
}

/// A calibration strip of `steps` equal steps from the thinnest to the
/// thickest plate, left to right, in a `columns` x `rows` grid. Returns the
/// map and each step's thickness.
pub fn step_wedge(
    columns: usize,
    rows: usize,
    spacing_mm: [f64; 2],
    steps: usize,
    tone: &ToneMapping,
) -> Result<(ReliefMap, Vec<f64>), ReliefError> {
    tone.validate()?;
    if steps < 2 || columns < steps {
        return Err(ReliefError::InvalidOptions(
            "a test strip needs at least two steps and one sample per step".into(),
        ));
    }
    let range = tone.max_thickness_mm - tone.min_thickness_mm;
    // Clamped, so rounding cannot carry the last step past the maximum.
    let thicknesses: Vec<f64> = (0..steps)
        .map(|step| {
            (tone.min_thickness_mm + range * step as f64 / (steps - 1) as f64)
                .clamp(tone.min_thickness_mm, tone.max_thickness_mm)
        })
        .collect();
    let row: Vec<f64> = (0..columns)
        .map(|column| thicknesses[column * steps / columns])
        .collect();
    let values = row.repeat(rows);
    Ok((
        ReliefMap {
            heights_mm: Grid::new(columns, rows, values)?,
            spacing_mm,
        },
        thicknesses,
    ))
}

/// Close a thickness map into a flat lithophane plate. The flat face lies on
/// Z = 0, each sample's top stands at its thickness, and the image is
/// mirrored so that it reads the right way round from the flat face. A plate
/// to be bent needs `fine_bottom`, a flat-face vertex under every sample.
pub fn lithophane_solid(
    map: &ReliefMap,
    tone: &ToneMapping,
    fine_bottom: bool,
) -> Result<ReliefSolid, ReliefError> {
    tone.validate()?;
    let thinnest = map
        .heights_mm
        .values()
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    if thinnest.is_nan() || thinnest < tone.min_thickness_mm {
        return Err(ReliefError::InvalidOptions(
            "the thickness map is thinner than the minimum thickness".into(),
        ));
    }
    // The solid stands the relief's lowest point `base_mm` above Z = 0, so
    // a base of the thinnest sample keeps every thickness absolute.
    relief_solid(
        map,
        &SolidOptions {
            base_mm: thinnest,
            border_mm: 0.0,
            border_height_mm: 0.0,
            mirror: true,
            invert: false,
            fine_bottom,
        },
    )
}

/// Surround a thickness map with a border `border_mm` wide, rounded to whole
/// samples, of `thickness_mm`, at the map's own sample spacing. Returns the
/// bordered map and the border's samples along a row and down a column.
pub fn with_border(
    map: &ReliefMap,
    border_mm: f64,
    thickness_mm: f64,
) -> Result<(ReliefMap, [usize; 2]), ReliefError> {
    if !(border_mm.is_finite() && border_mm >= 0.0) || !positive(thickness_mm) {
        return Err(ReliefError::InvalidOptions(
            "the border must be finite and not negative, and its thickness positive".into(),
        ));
    }
    let pad = map
        .spacing_mm
        .map(|spacing| (border_mm / spacing).round() as usize);
    let grid = &map.heights_mm;
    let columns = grid.width() + 2 * pad[0];
    let rows = grid.height() + 2 * pad[1];
    let mut values = vec![thickness_mm; columns * rows];
    for (y, row) in grid.values().chunks(grid.width()).enumerate() {
        let start = (y + pad[1]) * columns + pad[0];
        values[start..start + row.len()].copy_from_slice(row);
    }
    Ok((
        ReliefMap {
            heights_mm: Grid::new(columns, rows, values)?,
            spacing_mm: map.spacing_mm,
        },
        pad,
    ))
}

/// How far the chord across one cell `spacing_mm` wide falls inside a
/// cylinder of `radius_mm`, where that arc length is measured.
pub fn chord_sag_mm(spacing_mm: f64, radius_mm: f64) -> f64 {
    radius_mm * (1.0 - (spacing_mm / (2.0 * radius_mm)).cos())
}

/// Wrap a flat lithophane plate around a vertical cylinder whose axis is the
/// Z axis. The plate's flat face becomes the outside at `radius_mm`, its
/// relief faces the axis, and the plate's width becomes the arc length of
/// the outside. The image's top is at the largest Z and the plate's middle
/// at the largest Y.
///
/// Bending moves vertices without changing which triangles join, so a
/// closed plate stays closed. Each sample's flat-face and top vertices land
/// on one radial line, so the plate stays free of self-intersections when it
/// is built with `fine_bottom` and every top vertex lies deeper than the
/// [`chord_sag_mm`] of one sample spacing: then each cell's top lies inside
/// its flat-face chord, and radial walls separate neighbouring cells.
pub fn bend_around_cylinder(mesh: &mut TriangleMesh, radius_mm: f64) -> Result<(), ReliefError> {
    let refuse = |message: String| Err(ReliefError::InvalidOptions(message));
    if !positive(radius_mm) {
        return refuse("radius_mm must be positive".into());
    }
    let (mut low, mut high) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for vertex in &mesh.vertices {
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    let width = high[0] - low[0];
    if width / radius_mm >= std::f64::consts::TAU {
        return refuse(format!(
            "a {width:.1} mm wide plate does not fit once around a {radius_mm:.1} mm radius"
        ));
    }
    if high[2] >= radius_mm {
        return refuse("radius_mm must be larger than the plate's thickness".into());
    }
    let middle = (low[0] + high[0]) / 2.0;
    for (index, vertex) in mesh.vertices.iter_mut().enumerate() {
        if index % 65_536 == 0 {
            limit::check()?;
        }
        let [x, y, z] = *vertex;
        let angle = (x - middle) / radius_mm;
        let radius = radius_mm - z;
        *vertex = [radius * angle.sin(), radius * angle.cos(), y - low[1]];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relief::{PrintabilityOptions, check_relief};
    use crate::{ValidationOptions, analyze_mesh};

    /// White PLA's documented starting attenuation and the default range.
    const TONE: ToneMapping = ToneMapping {
        min_thickness_mm: 0.8,
        max_thickness_mm: 3.0,
        attenuation_per_mm: 1.5,
    };
    /// Largest difference, on the sRGB scale from 0 to 1, between a grey
    /// ramp and the simulated backlit brightness of its lithophane.
    const TOLERANCE: f64 = 0.01;

    /// A horizontal grey ramp of sRGB-encoded values from black to white.
    fn ramp(columns: usize, rows: usize) -> (Vec<f64>, Grid<f64>) {
        let encoded: Vec<f64> = (0..columns)
            .map(|column| column as f64 / (columns - 1) as f64)
            .collect();
        let linear: Vec<f64> = encoded.iter().map(|value| srgb_to_linear(*value)).collect();
        (
            encoded,
            Grid::new(columns, rows, linear.repeat(rows)).unwrap(),
        )
    }

    #[test]
    fn srgb_encoding_round_trips() {
        for step in 0..=255 {
            let value = f64::from(step) / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(value)) - value).abs() < 1e-12);
        }
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-12);
        assert!((srgb_to_linear(0.5) - 0.214).abs() < 1e-3);
    }

    #[test]
    fn a_grey_ramp_shows_through_as_it_was_and_a_linear_mapping_does_not() {
        let (encoded, linear) = ramp(256, 3);
        let map = lithophane_map(&linear, [0.2, 0.2], &TONE).unwrap();
        let mut error = 0.0_f64;
        let mut linear_mapping_error = 0.0_f64;
        for (column, value) in encoded.iter().enumerate() {
            let thickness = *map.heights_mm.get(column, 1);
            assert!((TONE.min_thickness_mm..=TONE.max_thickness_mm).contains(&thickness));
            error = error.max((linear_to_srgb(TONE.shown(thickness)) - value).abs());
            // The common shortcut: thickness falls in proportion to the
            // encoded grey value.
            let proportional =
                TONE.max_thickness_mm - value * (TONE.max_thickness_mm - TONE.min_thickness_mm);
            linear_mapping_error =
                linear_mapping_error.max((linear_to_srgb(TONE.shown(proportional)) - value).abs());
        }
        assert!(error < TOLERANCE, "{error}");
        assert!(
            linear_mapping_error > 5.0 * TOLERANCE,
            "{linear_mapping_error}"
        );
    }

    #[test]
    fn black_and_white_reach_the_thickness_limits() {
        assert!((TONE.thickness_mm(0.0) - 3.0).abs() < 1e-12);
        assert!((TONE.thickness_mm(1.0) - 0.8).abs() < 1e-12);
        // Values outside 0 to 1 are clamped.
        assert_eq!(TONE.thickness_mm(-1.0), TONE.thickness_mm(0.0));
        assert_eq!(TONE.thickness_mm(2.0), TONE.thickness_mm(1.0));
        // Brighter is always thinner.
        let thicknesses: Vec<f64> = (0..=100)
            .map(|step| TONE.thickness_mm(f64::from(step) / 100.0))
            .collect();
        assert!(thicknesses.windows(2).all(|pair| pair[0] > pair[1]));
        assert!((TONE.contrast_ratio() - (1.5_f64 * 2.2).exp()).abs() < 1e-9);
        assert!((TONE.backlit(3.0) * TONE.contrast_ratio() - 1.0).abs() < 1e-12);
        let invalid = ToneMapping {
            max_thickness_mm: 0.8,
            ..TONE
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn a_step_wedge_climbs_in_equal_steps() {
        let (map, thicknesses) = step_wedge(50, 4, [0.2, 0.2], 10, &TONE).unwrap();
        assert_eq!(thicknesses.len(), 10);
        assert!((thicknesses[0] - 0.8).abs() < 1e-12);
        assert!((thicknesses[9] - 3.0).abs() < 1e-12);
        for pair in thicknesses.windows(2) {
            assert!((pair[1] - pair[0] - 2.2 / 9.0).abs() < 1e-12);
        }
        for row in 0..4 {
            for column in 0..50 {
                assert_eq!(*map.heights_mm.get(column, row), thicknesses[column / 5]);
            }
        }
        assert!(step_wedge(5, 4, [0.2, 0.2], 10, &TONE).is_err());
        // The last step is exactly the maximum, even where rounding would
        // carry it past.
        let thick = ToneMapping {
            max_thickness_mm: 4.5,
            ..TONE
        };
        let (map, thicknesses) = step_wedge(50, 4, [0.2, 0.2], 10, &thick).unwrap();
        assert_eq!(thicknesses[9], 4.5);
        let highest = map.heights_mm.values().iter().copied().fold(0.0, f64::max);
        assert!(highest <= 4.5);
    }

    fn plate(border_mm: f64, fine_bottom: bool) -> (ReliefMap, ReliefSolid) {
        let (_, linear) = ramp(40, 30);
        let map = lithophane_map(&linear, [0.2, 0.2], &TONE).unwrap();
        let (map, _) = with_border(&map, border_mm, TONE.max_thickness_mm).unwrap();
        let solid = lithophane_solid(&map, &TONE, fine_bottom).unwrap();
        (map, solid)
    }

    #[test]
    fn a_flat_plate_is_closed_within_the_thickness_range_and_mirrored() {
        for (border, fine_bottom) in [(0.0, false), (2.0, false), (2.0, true)] {
            let (map, solid) = plate(border, fine_bottom);
            let report = analyze_mesh(solid.mesh(), ValidationOptions::default()).unwrap();
            assert!(report.topology.watertight, "{:?}", report.issues);
            assert_eq!(report.topology.self_intersections.unwrap_or(0), 0);
            assert!(report.signed_volume_mm3 > 0.0);
            let bounds = &report.bounds;
            assert!(bounds.minimum_mm[2].abs() < 1e-12);
            assert!((bounds.maximum_mm[2] - TONE.max_thickness_mm).abs() < 1e-9);
            let top = solid.top_vertices();
            assert!(
                top.iter()
                    .all(|vertex| vertex[2] >= TONE.min_thickness_mm - 1e-12)
            );
            let checks = check_relief(
                &map,
                &solid,
                &PrintabilityOptions {
                    minimum_relief_mm: Some(0.0),
                    ..PrintabilityOptions::default()
                },
            )
            .unwrap();
            assert!(checks.solid && checks.passed, "{checks:?}");
        }
        // The ramp brightens to the right, so the mirrored plate is thinnest
        // at its left edge, as seen from above.
        let (_, solid) = plate(0.0, false);
        let top = solid.top_vertices();
        assert!((top[0][2] - TONE.min_thickness_mm).abs() < 1e-9);
        assert!((top[39][2] - TONE.max_thickness_mm).abs() < 1e-9);
    }

    /// Bend a fine-bottomed ramp plate `width_mm` wide through `degrees`,
    /// and check it is a sound solid whose flat face stays within the
    /// chord sag of the cylinder.
    fn bent(
        (width_mm, rows, spacing): (f64, usize, f64),
        border_mm: f64,
        degrees: f64,
        tone: &ToneMapping,
    ) {
        let columns = (width_mm / spacing).round() as usize + 1;
        let (_, linear) = ramp(columns, rows);
        let map = lithophane_map(&linear, [spacing, spacing], tone).unwrap();
        let (map, _) = with_border(&map, border_mm, tone.max_thickness_mm).unwrap();
        let mut mesh = lithophane_solid(&map, tone, true).unwrap().into_mesh();
        let total = width_mm + 2.0 * border_mm;
        let radius = total / degrees.to_radians();
        let sag = chord_sag_mm(spacing, radius);
        assert!(sag <= 0.01 * tone.min_thickness_mm, "{sag}");
        bend_around_cylinder(&mut mesh, radius).unwrap();
        let report = analyze_mesh(&mesh, ValidationOptions::default()).unwrap();
        assert!(report.solid_geometry, "{:?}", report.issues);
        assert_eq!(report.topology.self_intersections, Some(0));
        assert!(report.signed_volume_mm3 > 0.0);
        for vertex in &mesh.vertices {
            let distance = vertex[0].hypot(vertex[1]);
            assert!(
                distance <= radius + 1e-9 && distance >= radius - tone.max_thickness_mm - 1e-9,
                "{vertex:?}"
            );
            assert!(vertex[2] >= -1e-12);
        }
    }

    #[test]
    fn bent_plates_stay_sound_solids() {
        // A small plate bent through half a turn with a thin minimum.
        let thin = ToneMapping {
            min_thickness_mm: 0.4,
            ..TONE
        };
        bent((10.0, 20, 0.2), 0.0, 180.0, &thin);
        // A wide border on a tight curve, which coarse cells cut through.
        bent((20.0, 11, 1.0), 50.0, 120.0, &TONE);
        // An ordinary curve.
        bent((7.8, 30, 0.2), 1.0, 30.0, &TONE);
    }

    #[test]
    fn a_bend_must_fit_around_its_cylinder() {
        let (_, solid) = plate(1.0, true);
        let mut mesh = solid.into_mesh();
        // Too tight to fit once around, or within the plate's thickness.
        assert!(bend_around_cylinder(&mut mesh, 1.0).is_err());
        assert!(bend_around_cylinder(&mut mesh, 2.9).is_err());
    }

    #[test]
    fn a_border_is_whole_samples_of_its_thickness() {
        let (_, linear) = ramp(4, 3);
        let map = lithophane_map(&linear, [0.2, 0.25], &TONE).unwrap();
        let (bordered, pad) = with_border(&map, 0.5, 3.0).unwrap();
        assert_eq!(pad, [3, 2]);
        assert_eq!(
            (bordered.heights_mm.width(), bordered.heights_mm.height()),
            (10, 7)
        );
        assert_eq!(*bordered.heights_mm.get(0, 0), 3.0);
        assert_eq!(bordered.heights_mm.get(3, 2), map.heights_mm.get(0, 0));
        assert!((chord_sag_mm(0.2, 10.0) - 0.0005).abs() < 1e-6);
    }
}
