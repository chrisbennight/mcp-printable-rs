//! Per-pixel grids, the analysis of a photograph, and the height field made
//! from it.

use super::integrate::IntegrationReport;
use super::{ReliefError, positive};
use crate::print_check::root;

/// Pixels in an analysis, at most, unless the caller sets another limit.
/// Anchored normal integration holds about 100 bytes per pixel, so this
/// default keeps its working memory under half a gigabyte.
pub const DEFAULT_MAX_PIXELS: usize = 2048 * 2048;

/// The edge from a pixel to its right-hand neighbour joins one surface.
pub(super) const RIGHT: u8 = 1;
/// The edge from a pixel to the neighbour below it joins one surface.
pub(super) const DOWN: u8 = 2;

/// Values over a rectangle of pixels, row by row from the top-left corner.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid<T> {
    width: usize,
    height: usize,
    values: Vec<T>,
}

impl<T> Grid<T> {
    /// A grid of `width` x `height` values, which must number exactly
    /// `width * height`.
    pub fn new(width: usize, height: usize, values: Vec<T>) -> Result<Self, ReliefError> {
        let expected = width.checked_mul(height).ok_or(ReliefError::TooLarge {
            pixels: usize::MAX,
            limit: usize::MAX,
        })?;
        if values.len() != expected || expected == 0 {
            return Err(ReliefError::GridSize {
                name: "grid",
                width,
                height,
                values: values.len(),
                expected,
            });
        }
        Ok(Self {
            width,
            height,
            values,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The values, row by row from the top-left corner.
    pub fn values(&self) -> &[T] {
        &self.values
    }

    pub fn into_values(self) -> Vec<T> {
        self.values
    }

    /// The value in column `x` of row `y`, counting rows from the top.
    pub fn get(&self, x: usize, y: usize) -> &T {
        &self.values[y * self.width + x]
    }

    pub(super) fn same_size<U>(
        &self,
        other: &Grid<U>,
        name: &'static str,
    ) -> Result<(), ReliefError> {
        if self.width == other.width && self.height == other.height {
            Ok(())
        } else {
            Err(ReliefError::SizeMismatch {
                name,
                width: other.width,
                height: other.height,
                depth_width: self.width,
                depth_height: self.height,
            })
        }
    }
}

/// What a photograph's analysis says about each pixel.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    /// Distance from the camera along its optical axis, positive, in any
    /// consistent unit. Its scale need not be known.
    pub depth: Grid<f32>,
    /// Unit surface normals in the camera's frame: x to the right, y up and
    /// z towards the camera. A normal shorter than one half marks a pixel
    /// without one.
    pub normals: Option<Grid<[f32; 3]>>,
    /// The subject's pixels. Without a mask every pixel is the subject.
    pub mask: Option<Grid<bool>>,
    /// Trust in each depth value, from 0 to 1. Without it every depth value
    /// is fully trusted.
    pub confidence: Option<Grid<f32>>,
    /// Focal length in pixels of the processed image. The principal point
    /// is taken to be the image centre.
    pub focal_length_px: Option<f64>,
}

/// A validated analysis, ready to integrate.
pub(super) struct Prepared<'a> {
    pub width: usize,
    pub height: usize,
    pub depth: &'a [f32],
    pub mask: Vec<bool>,
    /// Focal length in pixels, when the analysis has one.
    pub focal: Option<f64>,
    /// [`RIGHT`] and [`DOWN`] bits for edges that join two subject pixels
    /// without a depth jump.
    pub open: Vec<u8>,
    pub regions: usize,
}

impl Analysis {
    /// Check sizes and values, and find the edges joining one surface.
    pub(super) fn prepare(
        &self,
        max_pixels: usize,
        max_jump_slope: f64,
    ) -> Result<Prepared<'_>, ReliefError> {
        let (width, height) = (self.depth.width, self.depth.height);
        let pixels = width * height;
        if pixels > max_pixels {
            return Err(ReliefError::TooLarge {
                pixels,
                limit: max_pixels,
            });
        }
        if width < 2 || height < 2 {
            return Err(ReliefError::InvalidOptions(
                "the depth map must be at least 2 x 2 pixels".to_string(),
            ));
        }
        if let Some(normals) = &self.normals {
            self.depth.same_size(normals, "normals")?;
        }
        if let Some(mask) = &self.mask {
            self.depth.same_size(mask, "mask")?;
        }
        if let Some(confidence) = &self.confidence {
            self.depth.same_size(confidence, "confidence")?;
        }
        if self.focal_length_px.is_some_and(|focal| !positive(focal)) {
            return Err(ReliefError::InvalidOptions(
                "focal_length_px must be a finite positive number".to_string(),
            ));
        }
        let mask = self
            .mask
            .as_ref()
            .map_or_else(|| vec![true; pixels], |mask| mask.values.clone());
        if !mask.contains(&true) {
            return Err(ReliefError::EmptyMask);
        }
        let invalid = |name, index: usize, problem| ReliefError::InvalidValue {
            name,
            x: index % width,
            y: index / width,
            problem,
        };
        for (index, &depth) in self.depth.values.iter().enumerate() {
            if mask[index] && !(depth.is_finite() && depth > 0.0) {
                return Err(invalid("depth", index, "is not a finite positive distance"));
            }
        }
        if let Some(confidence) = &self.confidence {
            for (index, &value) in confidence.values.iter().enumerate() {
                if mask[index] && !(0.0..=1.0).contains(&value) {
                    return Err(invalid("confidence", index, "is not between 0 and 1"));
                }
            }
        }
        if let Some(normals) = &self.normals {
            for (index, normal) in normals.values.iter().enumerate() {
                if mask[index] && normal.iter().any(|value| !value.is_finite()) {
                    return Err(invalid("normal", index, "is not finite"));
                }
            }
        }
        let jump_focal = self.focal_length_px.unwrap_or(width.max(height) as f64);
        let open = open_edges(
            width,
            height,
            &mask,
            Some(&self.depth.values),
            max_jump_slope / jump_focal,
        )?;
        let regions = count_regions(width, &mask, &open);
        Ok(Prepared {
            width,
            height,
            depth: &self.depth.values,
            mask,
            focal: self.focal_length_px,
            open,
            regions,
        })
    }
}

/// [`RIGHT`] and [`DOWN`] bits for each edge joining two subject pixels
/// whose depths differ by at most `max_log_step` in natural log.
pub(super) fn open_edges(
    width: usize,
    height: usize,
    mask: &[bool],
    depth: Option<&[f32]>,
    max_log_step: f64,
) -> Result<Vec<u8>, ReliefError> {
    let joined = |i: usize, j: usize| {
        mask[i]
            && mask[j]
            && depth.is_none_or(|depth| {
                (f64::from(depth[i]).ln() - f64::from(depth[j]).ln()).abs() <= max_log_step
            })
    };
    let mut open = vec![0u8; width * height];
    for y in 0..height {
        crate::limit::check()?;
        for x in 0..width {
            let i = y * width + x;
            if x + 1 < width && joined(i, i + 1) {
                open[i] |= RIGHT;
            }
            if y + 1 < height && joined(i, i + width) {
                open[i] |= DOWN;
            }
        }
    }
    Ok(open)
}

/// Connected groups of subject pixels joined by open edges.
pub(super) fn count_regions(width: usize, mask: &[bool], open: &[u8]) -> usize {
    let mut parent: Vec<usize> = (0..mask.len()).collect();
    for (i, &bits) in open.iter().enumerate() {
        for (bit, j) in [(RIGHT, i + 1), (DOWN, i + width)] {
            if bits & bit != 0 {
                let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    (0..mask.len())
        .filter(|&i| mask[i] && root(&mut parent, i) == i)
        .count()
}

/// Heights of the subject's surface towards the camera.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightField {
    /// Height of each subject pixel above the subject's farthest point, in
    /// the depth map's units, or in pixel widths for Frankot-Chellappa
    /// integration without a focal length. Pixels outside the subject hold
    /// zero.
    pub heights: Grid<f64>,
    /// The subject's pixels, fixed with the edges found from them.
    pub(super) mask: Grid<bool>,
    pub report: IntegrationReport,
    /// [`RIGHT`] and [`DOWN`] bits for edges joining one surface, with no
    /// silhouette or depth jump between their pixels.
    pub(super) open: Vec<u8>,
}

impl HeightField {
    /// The subject's pixels.
    pub fn mask(&self) -> &Grid<bool> {
        &self.mask
    }

    /// A height field from heights the caller already has, such as a
    /// designed relief, with every edge between two subject pixels joining
    /// one surface.
    pub fn from_heights(heights: Grid<f64>, mask: Option<Grid<bool>>) -> Result<Self, ReliefError> {
        let (width, height) = (heights.width, heights.height);
        if width < 2 || height < 2 {
            return Err(ReliefError::InvalidOptions(
                "the height field must be at least 2 x 2 pixels".to_string(),
            ));
        }
        let mask = match mask {
            Some(mask) => {
                heights.same_size(&mask, "mask")?;
                mask
            }
            None => Grid::new(width, height, vec![true; width * height])?,
        };
        if !mask.values.contains(&true) {
            return Err(ReliefError::EmptyMask);
        }
        if let Some(index) = heights
            .values
            .iter()
            .zip(&mask.values)
            .position(|(value, inside)| *inside && !value.is_finite())
        {
            return Err(ReliefError::InvalidValue {
                name: "height",
                x: index % width,
                y: index / width,
                problem: "is not finite",
            });
        }
        let open = open_edges(width, height, &mask.values, None, f64::INFINITY)?;
        let regions = count_regions(width, &mask.values, &open);
        let mut heights = heights;
        for (value, inside) in heights.values.iter_mut().zip(&mask.values) {
            if !inside {
                *value = 0.0;
            }
        }
        Ok(Self {
            heights,
            mask,
            report: IntegrationReport::supplied(regions),
            open,
        })
    }

    pub(super) fn width(&self) -> usize {
        self.heights.width
    }

    pub(super) fn height(&self) -> usize {
        self.heights.height
    }
}

/// Heights above the farthest subject point from per-pixel distances, with
/// zero outside the subject.
pub(super) fn heights_from_distances(
    width: usize,
    height: usize,
    distances: Vec<f64>,
    mask: &[bool],
) -> Result<Grid<f64>, ReliefError> {
    let far = distances
        .iter()
        .zip(mask)
        .filter(|(_, inside)| **inside)
        .map(|(distance, _)| *distance)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut heights = distances;
    for (value, inside) in heights.iter_mut().zip(mask) {
        *value = if *inside { far - *value } else { 0.0 };
    }
    Grid::new(width, height, heights)
}

impl Prepared<'_> {
    pub(super) fn mask_grid(&self) -> Result<Grid<bool>, ReliefError> {
        Grid::new(self.width, self.height, self.mask.clone())
    }
}
