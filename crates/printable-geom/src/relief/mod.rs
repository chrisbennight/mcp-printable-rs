//! Printable relief solids from a photograph's analysis.
//!
//! Four pure stages turn per-pixel depth, and optionally surface normals, a
//! subject mask and a depth confidence, into a closed solid:
//!
//! 1. [`integrate`] makes a height field. Depth alone is the default. Normal
//!    integration anchored to the depth and Frankot-Chellappa integration are
//!    available on request.
//! 2. [`compress`] turns the height field into millimetres for a requested
//!    size and relief depth, compressing large slopes logarithmically and
//!    removing the steps at silhouettes and occlusions.
//! 3. [`relief_solid`] closes the relief into a watertight mesh with a flat
//!    base on Z = 0, the relief facing +Z, an optional border, and optional
//!    mirrored or inverted output for moulds.
//! 4. [`check_relief`] checks the solid and its relief against what a
//!    printer can reproduce.
//!
//! Every stage bounds its memory by the input's pixel count and its work by
//! iteration limits, and stops at the geometry work limit applied to its
//! thread. The module performs no I/O.

mod compress;
mod heightfield;
mod integrate;
mod poisson;
mod printability;
mod solid;
mod solver;

pub use compress::{
    CompressionOptions, CompressionReport, MAX_DETAIL_SIGMA_PX, ReliefMap, compress,
};
pub use heightfield::{Analysis, DEFAULT_MAX_PIXELS, Grid, HeightField};
pub use integrate::{IntegrationMethod, IntegrationOptions, IntegrationReport, integrate};
pub use printability::{NarrowDetail, PrintabilityOptions, ReliefPrintability, check_relief};
pub use solid::{ReliefSolid, SolidOptions, relief_solid};

use crate::GeometryError;

/// An input or option that prevents a trustworthy relief.
#[derive(Debug, thiserror::Error)]
pub enum ReliefError {
    #[error("{name} has {values} values, but a {width} x {height} grid needs {expected}")]
    GridSize {
        name: &'static str,
        width: usize,
        height: usize,
        values: usize,
        expected: usize,
    },
    #[error(
        "{name} is {width} x {height} pixels, but the depth map is {depth_width} x {depth_height}"
    )]
    SizeMismatch {
        name: &'static str,
        width: usize,
        height: usize,
        depth_width: usize,
        depth_height: usize,
    },
    #[error("{pixels} pixels exceed the limit of {limit}; reduce the image first")]
    TooLarge { pixels: usize, limit: usize },
    #[error("{name} at pixel ({x}, {y}) {problem}")]
    InvalidValue {
        name: &'static str,
        x: usize,
        y: usize,
        problem: &'static str,
    },
    #[error("the mask selects no pixels")]
    EmptyMask,
    #[error("{0} integration needs {1}")]
    MissingInput(&'static str, &'static str),
    #[error("{0}")]
    InvalidOptions(String),
    #[error(transparent)]
    Geometry(#[from] GeometryError),
}

impl ReliefError {
    /// A stable machine-readable code for the failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::GridSize { .. } | Self::SizeMismatch { .. } => "relief_grid_size",
            Self::TooLarge { .. } => "relief_too_large",
            Self::InvalidValue { .. } => "relief_invalid_value",
            Self::EmptyMask => "relief_empty_mask",
            Self::MissingInput(..) => "relief_missing_input",
            Self::InvalidOptions(_) => "validation",
            Self::Geometry(error) => error.code(),
        }
    }
}

/// Whether `value` is finite and positive.
fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

#[cfg(test)]
mod tests;
