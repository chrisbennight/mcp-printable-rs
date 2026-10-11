//! Converted model parsing and layout validation.

use std::collections::HashMap;

use safetensors::tensor::TensorView;
use safetensors::{Dtype, SafeTensorError, SafeTensors};
use thiserror::Error;

/// Format identifier written by the repository's model converter.
pub const MODEL_FORMAT: &str = "printable-flame/1";

/// FLAME's joints, in order: root, neck, jaw, left eye and right eye.
pub const JOINT_COUNT: usize = 5;

/// Parent of each joint. The neck hangs from the root; the jaw and both eyes
/// hang from the neck.
pub(crate) const PARENTS: [Option<usize>; JOINT_COUNT] = [None, Some(0), Some(1), Some(1), Some(1)];

/// One 3x3 rotation basis per non-root joint.
pub(crate) const POSE_COMPONENTS: usize = 9 * (JOINT_COUNT - 1);

const WEIGHT_TOLERANCE: f32 = 1e-3;
const MAX_LABEL_BYTES: usize = 64;

/// Reasons a converted model cannot be used.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model is not a valid safetensors file")]
    Format(#[source] SafeTensorError),
    #[error("model metadata `{0}` is missing or invalid")]
    Metadata(&'static str),
    #[error("model tensor `{0}` is missing")]
    MissingTensor(&'static str),
    #[error("model tensor `{name}` has type {found:?} and shape {shape:?}; expected {expected}")]
    Tensor {
        name: &'static str,
        found: Dtype,
        shape: Vec<usize>,
        expected: String,
    },
    #[error("model tensor `{0}` contains values that are not finite")]
    NonFinite(&'static str),
    #[error("model faces reference a vertex outside the template")]
    FaceIndex,
    #[error("model skinning weights do not sum to one for every vertex")]
    Weights,
    #[error("model kinematic tree is not FLAME's root, neck, jaw and two eyes")]
    KinematicTree,
}

/// Provenance and basis sizes recorded by the converter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Operator-assigned label, for example `flame2023_open`.
    pub variant: String,
    /// SHA-256 of the published file the model was converted from.
    pub source_sha256: String,
    /// Leading shape-basis components that describe identity.
    pub identity_components: usize,
    /// Trailing shape-basis components that describe expression.
    pub expression_components: usize,
    /// Model the expression basis comes from, such as `flame` or `SUPR_head`.
    pub expression_source: String,
}

/// A validated FLAME model held in memory.
#[derive(Debug, Clone)]
pub struct FlameModel {
    pub(crate) info: ModelInfo,
    pub(crate) vertex_count: usize,
    /// Rest vertices, flattened as `[vertex][axis]`.
    pub(crate) template: Vec<f32>,
    pub(crate) faces: Vec<[u32; 3]>,
    /// Shape basis, flattened as `[vertex][axis][component]`.
    pub(crate) shapedirs: Vec<f32>,
    /// Pose-corrective basis, flattened as `[vertex][axis][component]`.
    pub(crate) posedirs: Vec<f32>,
    /// Skinning weights, flattened as `[vertex][joint]`.
    pub(crate) weights: Vec<f32>,
    /// Joint regressor, flattened as `[joint][vertex]`.
    pub(crate) regressor: Vec<f32>,
}

impl FlameModel {
    /// Parse and validate a converted model.
    pub fn from_safetensors(bytes: &[u8]) -> Result<Self, ModelError> {
        let (_, header) = SafeTensors::read_metadata(bytes).map_err(ModelError::Format)?;
        let tensors = SafeTensors::deserialize(bytes).map_err(ModelError::Format)?;
        let metadata = header
            .metadata()
            .as_ref()
            .ok_or(ModelError::Metadata("format"))?;
        let info = parse_info(metadata)?;

        let template_view = view(&tensors, "v_template")?;
        let vertex_count = match (template_view.dtype(), template_view.shape()) {
            (Dtype::F32, &[vertices, 3]) if vertices > 0 => vertices,
            _ => return Err(mismatch("v_template", &template_view, "F32 [vertices, 3]")),
        };
        let template = floats("v_template", &template_view)?;

        let faces_view = view(&tensors, "faces")?;
        if faces_view.dtype() != Dtype::U32
            || !matches!(faces_view.shape(), &[triangles, 3] if triangles > 0)
        {
            return Err(mismatch("faces", &faces_view, "U32 [triangles, 3]"));
        }
        let faces: Vec<[u32; 3]> = words(faces_view.data())
            .collect::<Vec<_>>()
            .chunks_exact(3)
            .map(|face| [face[0], face[1], face[2]])
            .collect();
        if faces
            .iter()
            .flatten()
            .any(|&index| index as usize >= vertex_count)
        {
            return Err(ModelError::FaceIndex);
        }

        // `parse_info` guarantees this sum is non-zero and does not overflow.
        let components = info.identity_components + info.expression_components;
        let shapedirs = shaped_floats(&tensors, "shapedirs", &[vertex_count, 3, components])?;
        let posedirs = shaped_floats(&tensors, "posedirs", &[vertex_count, 3, POSE_COMPONENTS])?;
        let weights = shaped_floats(&tensors, "lbs_weights", &[vertex_count, JOINT_COUNT])?;
        let regressor = shaped_floats(&tensors, "joint_regressor", &[JOINT_COUNT, vertex_count])?;
        if weights
            .chunks_exact(JOINT_COUNT)
            .any(|row| (row.iter().sum::<f32>() - 1.0).abs() > WEIGHT_TOLERANCE)
        {
            return Err(ModelError::Weights);
        }

        let parents_view = view(&tensors, "kintree_parents")?;
        if parents_view.dtype() != Dtype::I32 || parents_view.shape() != [JOINT_COUNT] {
            return Err(mismatch("kintree_parents", &parents_view, "I32 [5]"));
        }
        let expected = PARENTS.map(|parent| parent.map_or(-1, |index| index as i32));
        if !words(parents_view.data())
            .map(|word| word as i32)
            .eq(expected)
        {
            return Err(ModelError::KinematicTree);
        }

        Ok(Self {
            info,
            vertex_count,
            template,
            faces,
            shapedirs,
            posedirs,
            weights,
            regressor,
        })
    }

    /// Provenance and basis sizes recorded by the converter.
    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    /// Number of mesh vertices.
    pub fn vertex_count(&self) -> usize {
        self.vertex_count
    }

    /// Mesh triangles as vertex indices; identical for every pose.
    pub fn faces(&self) -> &[[u32; 3]] {
        &self.faces
    }
}

fn parse_info(metadata: &HashMap<String, String>) -> Result<ModelInfo, ModelError> {
    let text = |key: &'static str| {
        metadata
            .get(key)
            .map(String::as_str)
            .ok_or(ModelError::Metadata(key))
    };
    let require = |key: &'static str, expected: &str| {
        if text(key)? == expected {
            Ok(())
        } else {
            Err(ModelError::Metadata(key))
        }
    };
    let count = |key: &'static str| {
        text(key)?
            .parse::<usize>()
            .map_err(|_| ModelError::Metadata(key))
    };
    let label = |key: &'static str, allowed: fn(u8) -> bool| {
        let value = text(key)?;
        let valid = !value.is_empty()
            && value.len() <= MAX_LABEL_BYTES
            && value
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric())
            && value.bytes().all(allowed);
        if valid {
            Ok(value.to_owned())
        } else {
            Err(ModelError::Metadata(key))
        }
    };

    require("format", MODEL_FORMAT)?;
    require("skinning", "lbs")?;
    require("pose_feature", "lrotmin")?;
    require("units", "metres")?;
    let source_sha256 = text("source_sha256")?;
    if source_sha256.len() != 64
        || !source_sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ModelError::Metadata("source_sha256"));
    }
    let identity_components = count("identity_components")?;
    let expression_components = count("expression_components")?;
    // Evaluation relies on a non-empty shape basis whose size is representable.
    if identity_components
        .checked_add(expression_components)
        .is_none_or(|total| total == 0)
    {
        return Err(ModelError::Metadata("identity_components"));
    }
    Ok(ModelInfo {
        variant: label("variant", |byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })?,
        source_sha256: source_sha256.to_owned(),
        identity_components,
        expression_components,
        expression_source: label("expression_source", |byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
        })?,
    })
}

fn view<'a>(tensors: &SafeTensors<'a>, name: &'static str) -> Result<TensorView<'a>, ModelError> {
    tensors
        .tensor(name)
        .map_err(|_| ModelError::MissingTensor(name))
}

fn mismatch(name: &'static str, view: &TensorView<'_>, expected: &str) -> ModelError {
    ModelError::Tensor {
        name,
        found: view.dtype(),
        shape: view.shape().to_vec(),
        expected: expected.to_owned(),
    }
}

fn shaped_floats(
    tensors: &SafeTensors<'_>,
    name: &'static str,
    shape: &[usize],
) -> Result<Vec<f32>, ModelError> {
    let tensor = view(tensors, name)?;
    if tensor.dtype() != Dtype::F32 || tensor.shape() != shape {
        return Err(mismatch(name, &tensor, &format!("F32 {shape:?}")));
    }
    floats(name, &tensor)
}

fn floats(name: &'static str, view: &TensorView<'_>) -> Result<Vec<f32>, ModelError> {
    let values: Vec<f32> = words(view.data()).map(f32::from_bits).collect();
    if values.iter().all(|value| value.is_finite()) {
        Ok(values)
    } else {
        Err(ModelError::NonFinite(name))
    }
}

/// Little-endian 32-bit words; safetensors data carries no alignment guarantee.
fn words(data: &[u8]) -> impl Iterator<Item = u32> + '_ {
    data.chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Fixture;

    fn rebuilt(edit: impl FnOnce(&mut Fixture)) -> Result<FlameModel, ModelError> {
        let mut fixture = Fixture::default();
        edit(&mut fixture);
        FlameModel::from_safetensors(&fixture.bytes())
    }

    #[test]
    fn valid_model_exposes_its_provenance_and_topology() {
        let model = Fixture::default().model();
        assert_eq!(model.vertex_count(), 4);
        assert_eq!(model.faces(), &[[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]]);
        assert_eq!(
            model.info(),
            &ModelInfo {
                variant: "test".into(),
                source_sha256: "0".repeat(64),
                identity_components: 2,
                expression_components: 1,
                expression_source: "flame".into(),
            }
        );
    }

    #[test]
    fn unreadable_bytes_are_refused() {
        assert!(matches!(
            FlameModel::from_safetensors(b"not a model"),
            Err(ModelError::Format(_))
        ));
    }

    #[test]
    fn other_formats_are_refused() {
        let result = rebuilt(|fixture| {
            fixture
                .metadata
                .insert("format".into(), "printable-flame/2".into());
        });
        assert!(matches!(result, Err(ModelError::Metadata("format"))));
    }

    #[test]
    fn basis_sizes_must_match_the_shape_basis() {
        let result = rebuilt(|fixture| {
            fixture
                .metadata
                .insert("identity_components".into(), "3".into());
        });
        assert!(matches!(
            result,
            Err(ModelError::Tensor {
                name: "shapedirs",
                ..
            })
        ));
    }

    #[test]
    fn basis_sizes_that_overflow_or_sum_to_zero_are_refused() {
        for (identity, expression) in [(usize::MAX.to_string(), "1"), ("0".to_string(), "0")] {
            let result = rebuilt(|fixture| {
                fixture
                    .metadata
                    .insert("identity_components".into(), identity);
                fixture
                    .metadata
                    .insert("expression_components".into(), expression.into());
            });
            assert!(matches!(
                result,
                Err(ModelError::Metadata("identity_components"))
            ));
        }
    }

    #[test]
    fn malformed_labels_are_refused() {
        let result = rebuilt(|fixture| {
            fixture
                .metadata
                .insert("variant".into(), "Not A Label".into());
        });
        assert!(matches!(result, Err(ModelError::Metadata("variant"))));
    }

    #[test]
    fn missing_tensors_are_refused() {
        let result = rebuilt(|fixture| fixture.omit.push("posedirs"));
        assert!(matches!(result, Err(ModelError::MissingTensor("posedirs"))));
    }

    #[test]
    fn faces_outside_the_template_are_refused() {
        let result = rebuilt(|fixture| fixture.faces[11] = 4);
        assert!(matches!(result, Err(ModelError::FaceIndex)));
    }

    #[test]
    fn non_finite_values_are_refused() {
        let result = rebuilt(|fixture| fixture.shapedirs[5] = f32::NAN);
        assert!(matches!(result, Err(ModelError::NonFinite("shapedirs"))));
    }

    #[test]
    fn weights_that_do_not_sum_to_one_are_refused() {
        let result = rebuilt(|fixture| fixture.weights[0] = 0.5);
        assert!(matches!(result, Err(ModelError::Weights)));
    }

    #[test]
    fn other_kinematic_trees_are_refused() {
        let result = rebuilt(|fixture| fixture.parents = vec![-1, 0, 1, 2, 1]);
        assert!(matches!(result, Err(ModelError::KinematicTree)));
    }
}
