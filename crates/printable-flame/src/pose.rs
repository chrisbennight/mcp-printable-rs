//! Shape blending, joint regression, pose correctives and linear blend skinning.

use thiserror::Error;

use crate::model::{FlameModel, JOINT_COUNT, PARENTS, POSE_COMPONENTS};

type Vector = [f64; 3];
type Matrix = [[f64; 3]; 3];

const IDENTITY: Matrix = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Joint rotations as axis-angle vectors in radians: the direction is the
/// rotation axis and the length is the angle.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pose {
    /// Rotation of the whole head about the root joint at the base of the neck.
    pub global: [f32; 3],
    pub neck: [f32; 3],
    pub jaw: [f32; 3],
    /// The subject's left eye, on the +x side.
    pub left_eye: [f32; 3],
    pub right_eye: [f32; 3],
}

impl Pose {
    fn joints(&self) -> [[f32; 3]; JOINT_COUNT] {
        [
            self.global,
            self.neck,
            self.jaw,
            self.left_eye,
            self.right_eye,
        ]
    }
}

/// Inputs to one evaluation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlameParams {
    /// Identity coefficients in basis order. Omitted trailing coefficients are zero.
    pub identity: Vec<f32>,
    /// Expression coefficients in basis order. Omitted trailing coefficients are zero.
    pub expression: Vec<f32>,
    pub pose: Pose,
    /// Offset applied after posing, in metres.
    pub translation: [f32; 3],
}

/// A posed head. Triangles are [`FlameModel::faces`].
#[derive(Debug, Clone, PartialEq)]
pub struct PosedHead {
    /// One position per model vertex, in metres.
    pub vertices: Vec<[f32; 3]>,
    /// Posed joint locations in [`Pose`] field order.
    pub joints: [[f32; 3]; JOINT_COUNT],
}

/// Reasons a set of inputs cannot be evaluated against a model.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("{kind} has {given} coefficients but the model provides {available}")]
    TooManyCoefficients {
        kind: &'static str,
        given: usize,
        available: usize,
    },
    #[error("{0} contains a value that is not finite")]
    NonFinite(&'static str),
}

impl FlameModel {
    /// Evaluate the model for one set of coefficients and joint rotations.
    pub fn evaluate(&self, params: &FlameParams) -> Result<PosedHead, ParamError> {
        check_coefficients("identity", &params.identity, self.info.identity_components)?;
        check_coefficients(
            "expression",
            &params.expression,
            self.info.expression_components,
        )?;
        let pose = params.pose.joints();
        if !pose.iter().flatten().all(|value| value.is_finite()) {
            return Err(ParamError::NonFinite("pose"));
        }
        if !params.translation.iter().all(|value| value.is_finite()) {
            return Err(ParamError::NonFinite("translation"));
        }

        let shaped = self.blend_shapes(params);
        let rest_joints = self.regress_joints(&shaped);
        let rotations = pose.map(|rotation| rodrigues(rotation.map(f64::from)));
        let posed = self.add_pose_correctives(shaped, &rotations);

        let world = chain(&rotations, &rest_joints);
        // Skinning maps rest-pose positions, so each joint's transform first
        // removes its rest location.
        let skinning: [(Matrix, Vector); JOINT_COUNT] = std::array::from_fn(|joint| {
            let (rotation, origin) = world[joint];
            (rotation, sub(origin, apply(&rotation, rest_joints[joint])))
        });
        let translation = params.translation.map(f64::from);

        let vertices = posed
            .chunks_exact(3)
            .zip(self.weights.chunks_exact(JOINT_COUNT))
            .map(|(point, weights)| {
                let mut rotation = [[0.0; 3]; 3];
                let mut offset = [0.0; 3];
                for (&weight, (joint_rotation, joint_offset)) in weights.iter().zip(&skinning) {
                    if weight == 0.0 {
                        continue;
                    }
                    let weight = f64::from(weight);
                    for (row, joint_row) in rotation.iter_mut().zip(joint_rotation) {
                        for (value, joint_value) in row.iter_mut().zip(joint_row) {
                            *value += weight * joint_value;
                        }
                    }
                    for (value, joint_value) in offset.iter_mut().zip(joint_offset) {
                        *value += weight * joint_value;
                    }
                }
                single(add(
                    add(apply(&rotation, [point[0], point[1], point[2]]), offset),
                    translation,
                ))
            })
            .collect();
        let joints = world.map(|(_, origin)| single(add(origin, translation)));
        Ok(PosedHead { vertices, joints })
    }

    /// Template plus the weighted identity and expression basis, flattened as
    /// `[vertex][axis]`.
    fn blend_shapes(&self, params: &FlameParams) -> Vec<f64> {
        let components = self.info.identity_components + self.info.expression_components;
        let active: Vec<(usize, f64)> = params
            .identity
            .iter()
            .copied()
            .enumerate()
            .chain(
                params
                    .expression
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(index, value)| (self.info.identity_components + index, value)),
            )
            .filter(|&(_, value)| value != 0.0)
            .map(|(index, value)| (index, f64::from(value)))
            .collect();
        self.template
            .iter()
            .zip(self.shapedirs.chunks_exact(components))
            .map(|(&rest, basis)| {
                active.iter().fold(f64::from(rest), |sum, &(index, value)| {
                    sum + f64::from(basis[index]) * value
                })
            })
            .collect()
    }

    fn regress_joints(&self, shaped: &[f64]) -> [Vector; JOINT_COUNT] {
        std::array::from_fn(|joint| {
            let row = &self.regressor[joint * self.vertex_count..(joint + 1) * self.vertex_count];
            let mut location = [0.0; 3];
            for (&weight, vertex) in row.iter().zip(shaped.chunks_exact(3)) {
                if weight != 0.0 {
                    for (value, coordinate) in location.iter_mut().zip(vertex) {
                        *value += f64::from(weight) * coordinate;
                    }
                }
            }
            location
        })
    }

    /// Adds corrective shapes driven by each non-root joint's rotation minus
    /// the identity, read row by row.
    fn add_pose_correctives(
        &self,
        mut shaped: Vec<f64>,
        rotations: &[Matrix; JOINT_COUNT],
    ) -> Vec<f64> {
        let mut feature = [0.0; POSE_COMPONENTS];
        for (joint, rotation) in rotations.iter().enumerate().skip(1) {
            for (row, (values, identity_row)) in rotation.iter().zip(&IDENTITY).enumerate() {
                for (column, (value, identity)) in values.iter().zip(identity_row).enumerate() {
                    feature[(joint - 1) * 9 + row * 3 + column] = value - identity;
                }
            }
        }
        if feature.iter().all(|&value| value == 0.0) {
            return shaped;
        }
        for (value, basis) in shaped
            .iter_mut()
            .zip(self.posedirs.chunks_exact(POSE_COMPONENTS))
        {
            *value += basis
                .iter()
                .zip(&feature)
                .map(|(&component, &weight)| f64::from(component) * weight)
                .sum::<f64>();
        }
        shaped
    }
}

fn check_coefficients(
    kind: &'static str,
    values: &[f32],
    available: usize,
) -> Result<(), ParamError> {
    if values.len() > available {
        return Err(ParamError::TooManyCoefficients {
            kind,
            given: values.len(),
            available,
        });
    }
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(ParamError::NonFinite(kind))
    }
}

/// World rotation and position of every joint, composing each joint's local
/// rotation about its rest location onto its parent's transform.
fn chain(
    rotations: &[Matrix; JOINT_COUNT],
    joints: &[Vector; JOINT_COUNT],
) -> [(Matrix, Vector); JOINT_COUNT] {
    let mut world = [(IDENTITY, [0.0; 3]); JOINT_COUNT];
    for (joint, parent) in PARENTS.iter().enumerate() {
        world[joint] = match *parent {
            None => (rotations[joint], joints[joint]),
            Some(parent) => {
                let (parent_rotation, parent_origin) = world[parent];
                let offset = sub(joints[joint], joints[parent]);
                (
                    multiply(&parent_rotation, &rotations[joint]),
                    add(apply(&parent_rotation, offset), parent_origin),
                )
            }
        };
    }
    world
}

/// Rotation matrix for an axis-angle vector (Rodrigues' formula).
fn rodrigues(rotation: Vector) -> Matrix {
    let angle = rotation
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    let [x, y, z] = rotation;
    if angle < 1e-12 {
        // First-order expansion; the omitted terms are below double precision.
        return [[1.0, -z, y], [z, 1.0, -x], [-y, x, 1.0]];
    }
    let [x, y, z] = [x / angle, y / angle, z / angle];
    let (sin, cos) = angle.sin_cos();
    let versine = 1.0 - cos;
    [
        [
            versine * x * x + cos,
            versine * x * y - sin * z,
            versine * x * z + sin * y,
        ],
        [
            versine * x * y + sin * z,
            versine * y * y + cos,
            versine * y * z - sin * x,
        ],
        [
            versine * x * z - sin * y,
            versine * y * z + sin * x,
            versine * z * z + cos,
        ],
    ]
}

fn multiply(left: &Matrix, right: &Matrix) -> Matrix {
    std::array::from_fn(|row| {
        std::array::from_fn(|column| (0..3).map(|k| left[row][k] * right[k][column]).sum())
    })
}

fn apply(matrix: &Matrix, vector: Vector) -> Vector {
    matrix.map(|row| row[0] * vector[0] + row[1] * vector[1] + row[2] * vector[2])
}

fn add(left: Vector, right: Vector) -> Vector {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn sub(left: Vector, right: Vector) -> Vector {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn single(vector: Vector) -> [f32; 3] {
    vector.map(|value| value as f32)
}

#[cfg(test)]
mod tests {
    use std::f64::consts::FRAC_PI_2;

    use super::*;
    use crate::fixture::{Fixture, ROOT};

    const TOLERANCE: f32 = 1e-6;

    fn close(actual: [f32; 3], expected: [f64; 3]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                (f64::from(*a) - e).abs() < f64::from(TOLERANCE),
                "{actual:?} != {expected:?}"
            );
        }
    }

    fn template(fixture: &Fixture, vertex: usize) -> Vector {
        std::array::from_fn(|axis| f64::from(fixture.template[vertex * 3 + axis]))
    }

    #[test]
    fn rest_pose_reproduces_the_template_and_regressed_joints() {
        let fixture = Fixture::default();
        let head = fixture.model().evaluate(&FlameParams::default()).unwrap();
        for (vertex, position) in head.vertices.iter().enumerate() {
            close(*position, template(&fixture, vertex));
        }
        // Joints 0-3 sit on vertices 0-3; joint 4 is the midpoint of vertices 1 and 2.
        close(head.joints[2], template(&fixture, 2));
        close(head.joints[4], [0.5, 0.5, 0.0]);
    }

    #[test]
    fn coefficients_select_identity_then_expression_components() {
        let fixture = Fixture::default();
        let params = FlameParams {
            identity: vec![0.5],
            expression: vec![2.0],
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        for (vertex, position) in head.vertices.iter().enumerate() {
            let expected = std::array::from_fn(|axis| {
                let row = vertex * 3 + axis;
                template(&fixture, vertex)[axis]
                    + 0.5 * f64::from(fixture.shapedirs[row * 3])
                    + 2.0 * f64::from(fixture.shapedirs[row * 3 + 2])
            });
            close(*position, expected);
        }
    }

    #[test]
    fn global_rotation_moves_every_vertex_rigidly_about_the_root() {
        let mut fixture = Fixture::default();
        // Non-zero correctives prove the root rotation does not drive them.
        fixture.posedirs.iter_mut().for_each(|value| *value = 0.25);
        let params = FlameParams {
            pose: Pose {
                global: [0.0, 0.0, FRAC_PI_2 as f32],
                ..Pose::default()
            },
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        for (vertex, position) in head.vertices.iter().enumerate() {
            let [x, y, z] = sub(template(&fixture, vertex), template(&fixture, ROOT));
            close(*position, add([-y, x, z], template(&fixture, ROOT)));
        }
    }

    #[test]
    fn jaw_rotation_turns_jaw_vertices_about_the_jaw_joint() {
        let mut fixture = Fixture::default();
        fixture.bind(3, 2);
        let params = FlameParams {
            pose: Pose {
                jaw: [FRAC_PI_2 as f32, 0.0, 0.0],
                ..Pose::default()
            },
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        let [x, y, z] = sub(template(&fixture, 3), template(&fixture, 2));
        close(head.vertices[3], add([x, -z, y], template(&fixture, 2)));
        for vertex in 0..3 {
            close(head.vertices[vertex], template(&fixture, vertex));
        }
    }

    #[test]
    fn child_joints_follow_their_parent_rotation() {
        let fixture = Fixture::default();
        let params = FlameParams {
            pose: Pose {
                neck: [0.0, 0.0, FRAC_PI_2 as f32],
                ..Pose::default()
            },
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        let [x, y, z] = sub(template(&fixture, 2), template(&fixture, 1));
        close(head.joints[2], add([-y, x, z], template(&fixture, 1)));
        close(head.joints[1], template(&fixture, 1));
    }

    #[test]
    fn pose_correctives_read_each_rotation_minus_identity_row_by_row() {
        let mut fixture = Fixture::default();
        let angle = 0.3_f64;
        // Vertex 0 stays on the unrotated root, so only correctives move it.
        // Component 1 is the neck's (row 0, column 1); component 12 is the jaw's
        // (row 1, column 0).
        fixture.posedirs[1] = 1.0;
        fixture.posedirs[POSE_COMPONENTS + 12] = 1.0;
        let rotation = [0.0, 0.0, angle as f32];
        let params = FlameParams {
            pose: Pose {
                neck: rotation,
                jaw: rotation,
                ..Pose::default()
            },
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        let rest = template(&fixture, 0);
        close(
            head.vertices[0],
            [rest[0] - angle.sin(), rest[1] + angle.sin(), rest[2]],
        );
    }

    #[test]
    fn translation_offsets_vertices_and_joints() {
        let fixture = Fixture::default();
        let params = FlameParams {
            translation: [1.0, -2.0, 0.5],
            ..FlameParams::default()
        };
        let head = fixture.model().evaluate(&params).unwrap();
        close(
            head.vertices[1],
            add(template(&fixture, 1), [1.0, -2.0, 0.5]),
        );
        close(head.joints[0], add(template(&fixture, 0), [1.0, -2.0, 0.5]));
    }

    #[test]
    fn surplus_coefficients_are_refused() {
        let params = FlameParams {
            expression: vec![0.0; 2],
            ..FlameParams::default()
        };
        assert_eq!(
            Fixture::default().model().evaluate(&params),
            Err(ParamError::TooManyCoefficients {
                kind: "expression",
                given: 2,
                available: 1
            })
        );
    }

    #[test]
    fn non_finite_inputs_are_refused() {
        let model = Fixture::default().model();
        let identity = FlameParams {
            identity: vec![f32::NAN],
            ..FlameParams::default()
        };
        assert_eq!(
            model.evaluate(&identity),
            Err(ParamError::NonFinite("identity"))
        );
        let pose = FlameParams {
            pose: Pose {
                left_eye: [f32::INFINITY, 0.0, 0.0],
                ..Pose::default()
            },
            ..FlameParams::default()
        };
        assert_eq!(model.evaluate(&pose), Err(ParamError::NonFinite("pose")));
    }

    #[test]
    fn rotations_are_stable_at_tiny_and_full_turn_angles() {
        let tiny = rodrigues([1e-15, 0.0, 0.0]);
        let full = rodrigues([0.0, std::f64::consts::TAU, 0.0]);
        for matrix in [tiny, full] {
            for (row, identity_row) in matrix.iter().zip(&IDENTITY) {
                for (value, identity) in row.iter().zip(identity_row) {
                    assert!((value - identity).abs() < 1e-12);
                }
            }
        }
    }
}
