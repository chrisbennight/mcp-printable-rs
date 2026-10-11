//! A small serialized model for unit tests.
//!
//! Four vertices form a tetrahedron. Joints 0-3 sit on vertices 0-3 and joint 4
//! is the midpoint of vertices 1 and 2. Every vertex starts bound to the root,
//! whose rest location is off the origin so rotations about it are observable.

use std::collections::HashMap;

use safetensors::tensor::TensorView;
use safetensors::{Dtype, serialize};

use crate::model::{FlameModel, JOINT_COUNT, POSE_COMPONENTS};

pub(crate) const ROOT: usize = 0;
const VERTICES: usize = 4;
const IDENTITY_COMPONENTS: usize = 2;
const EXPRESSION_COMPONENTS: usize = 1;
const COMPONENTS: usize = IDENTITY_COMPONENTS + EXPRESSION_COMPONENTS;

pub(crate) struct Fixture {
    pub template: Vec<f32>,
    pub faces: Vec<u32>,
    pub shapedirs: Vec<f32>,
    pub posedirs: Vec<f32>,
    pub weights: Vec<f32>,
    pub regressor: Vec<f32>,
    pub parents: Vec<i32>,
    pub metadata: HashMap<String, String>,
    pub omit: Vec<&'static str>,
}

impl Default for Fixture {
    fn default() -> Self {
        let mut regressor = vec![0.0; JOINT_COUNT * VERTICES];
        for joint in 0..VERTICES {
            regressor[joint * VERTICES + joint] = 1.0;
        }
        regressor[4 * VERTICES + 1] = 0.5;
        regressor[4 * VERTICES + 2] = 0.5;
        let mut weights = vec![0.0; VERTICES * JOINT_COUNT];
        for vertex in 0..VERTICES {
            weights[vertex * JOINT_COUNT + ROOT] = 1.0;
        }
        let metadata = [
            ("format", "printable-flame/1"),
            ("variant", "test"),
            ("source_sha256", &"0".repeat(64)),
            ("identity_components", "2"),
            ("expression_components", "1"),
            ("expression_source", "flame"),
            ("skinning", "lbs"),
            ("pose_feature", "lrotmin"),
            ("units", "metres"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        Self {
            template: vec![0.25, 0.5, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            faces: vec![0, 1, 2, 0, 1, 3, 0, 2, 3, 1, 2, 3],
            shapedirs: (0..VERTICES * 3 * COMPONENTS)
                .map(|index| index as f32 * 0.125)
                .collect(),
            posedirs: vec![0.0; VERTICES * 3 * POSE_COMPONENTS],
            weights,
            regressor,
            parents: vec![-1, 0, 1, 1, 1],
            metadata,
            omit: Vec::new(),
        }
    }
}

impl Fixture {
    /// Bind one vertex entirely to one joint.
    pub fn bind(&mut self, vertex: usize, joint: usize) {
        let row = &mut self.weights[vertex * JOINT_COUNT..(vertex + 1) * JOINT_COUNT];
        row.fill(0.0);
        row[joint] = 1.0;
    }

    pub fn bytes(&self) -> Vec<u8> {
        let floats = |values: &[f32]| {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        let tensors = [
            (
                "v_template",
                Dtype::F32,
                vec![VERTICES, 3],
                floats(&self.template),
            ),
            (
                "faces",
                Dtype::U32,
                vec![self.faces.len() / 3, 3],
                self.faces
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect(),
            ),
            (
                "shapedirs",
                Dtype::F32,
                vec![VERTICES, 3, COMPONENTS],
                floats(&self.shapedirs),
            ),
            (
                "posedirs",
                Dtype::F32,
                vec![VERTICES, 3, POSE_COMPONENTS],
                floats(&self.posedirs),
            ),
            (
                "lbs_weights",
                Dtype::F32,
                vec![VERTICES, JOINT_COUNT],
                floats(&self.weights),
            ),
            (
                "joint_regressor",
                Dtype::F32,
                vec![JOINT_COUNT, VERTICES],
                floats(&self.regressor),
            ),
            (
                "kintree_parents",
                Dtype::I32,
                vec![self.parents.len()],
                self.parents
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect(),
            ),
        ];
        let views: Vec<(&str, TensorView<'_>)> = tensors
            .iter()
            .filter(|(name, ..)| !self.omit.contains(name))
            .map(|(name, dtype, shape, data)| {
                (
                    *name,
                    TensorView::new(*dtype, shape.clone(), data)
                        .expect("fixture tensor matches its shape"),
                )
            })
            .collect();
        serialize(views, Some(self.metadata.clone())).expect("fixture serializes")
    }

    pub fn model(&self) -> FlameModel {
        FlameModel::from_safetensors(&self.bytes()).expect("fixture model is valid")
    }
}
