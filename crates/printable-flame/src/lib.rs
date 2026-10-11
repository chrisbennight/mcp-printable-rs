//! FLAME statistical head model evaluation.
//!
//! [`FlameModel::from_safetensors`] parses a model written by the repository's
//! `scripts/flame_model_convert.py` from an in-memory buffer and validates its
//! layout. [`FlameModel::evaluate`] applies identity and expression blend
//! shapes, regresses joint locations from the shaped mesh, adds pose-corrective
//! shapes and poses the mesh with linear blend skinning.
//!
//! Coordinates are in metres with y up and the face looking along +z, so the
//! subject's left is +x. Model data is float32; evaluation accumulates in f64.
//! The crate performs no filesystem, network, or async I/O.

mod model;
mod pose;

pub use model::{FlameModel, JOINT_COUNT, MODEL_FORMAT, ModelError, ModelInfo, UvLayout};
pub use pose::{FlameParams, ParamError, Pose, PosedHead};

#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
