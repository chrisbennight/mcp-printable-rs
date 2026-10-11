//! Checks against a converted published FLAME model.
//!
//! These tests are ignored by default because the model must be downloaded by a
//! registered user. Convert one with `scripts/flame_model_convert.py`, then run:
//!
//! ```sh
//! PRINTABLE_TEST_FLAME_MODEL=/models/flame/flame2023_open.safetensors \
//!   cargo test -p printable-flame --test published_model -- --ignored
//! ```

use printable_flame::{FlameModel, FlameParams, Pose, PosedHead};

fn published() -> FlameModel {
    let path = std::env::var_os("PRINTABLE_TEST_FLAME_MODEL")
        .expect("set PRINTABLE_TEST_FLAME_MODEL to a converted FLAME model");
    let bytes = std::fs::read(path).expect("converted model is readable");
    FlameModel::from_safetensors(&bytes).expect("converted model is valid")
}

fn posed(model: &FlameModel, pose: Pose) -> PosedHead {
    model
        .evaluate(&FlameParams {
            pose,
            ..FlameParams::default()
        })
        .expect("pose evaluates")
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(p, q)| (p - q) * (p - q))
        .sum::<f32>()
        .sqrt()
}

#[test]
#[ignore = "requires PRINTABLE_TEST_FLAME_MODEL"]
fn rest_head_has_flame_topology_at_head_scale() {
    let model = published();
    let head = posed(&model, Pose::default());
    assert_eq!(model.vertex_count(), 5023);
    assert_eq!(model.faces().len(), 9976);
    for axis in 0..3 {
        let values = head.vertices.iter().map(|vertex| vertex[axis]);
        let extent = values.clone().fold(f32::MIN, f32::max) - values.fold(f32::MAX, f32::min);
        assert!(
            (0.15..0.45).contains(&extent),
            "axis {axis} spans {extent} m"
        );
    }
}

#[test]
#[ignore = "requires PRINTABLE_TEST_FLAME_MODEL"]
fn left_eye_is_on_the_subject_left_above_the_jaw() {
    let [_, _, jaw, left, right] = posed(&published(), Pose::default()).joints;
    assert!(
        left[0] > 0.0 && right[0] < 0.0,
        "left {left:?}, right {right:?}"
    );
    assert!(left[1] > jaw[1] && right[1] > jaw[1]);
}

#[test]
#[ignore = "requires PRINTABLE_TEST_FLAME_MODEL"]
fn global_rotation_preserves_every_distance_to_the_root() {
    let model = published();
    let rest = posed(&model, Pose::default());
    let turned = posed(
        &model,
        Pose {
            global: [0.2, 0.7, -0.1],
            ..Pose::default()
        },
    );
    assert!(distance(rest.joints[0], turned.joints[0]) < 1e-6);
    for (before, after) in rest.vertices.iter().zip(&turned.vertices) {
        let change = distance(*before, rest.joints[0]) - distance(*after, turned.joints[0]);
        assert!(
            change.abs() < 1e-5,
            "distance to root changed by {change} m"
        );
    }
}

#[test]
#[ignore = "requires PRINTABLE_TEST_FLAME_MODEL"]
fn opening_the_jaw_moves_the_chin_far_more_than_the_crown() {
    // FLAME's learned skinning weights and pose correctives give the jaw a
    // small influence beyond the jaw itself, so the crown moves a little.
    // The invariant is locality, not zero motion elsewhere.
    let model = published();
    let rest = posed(&model, Pose::default());
    let open = posed(
        &model,
        Pose {
            jaw: [0.3, 0.0, 0.0],
            ..Pose::default()
        },
    );
    let eye_height = rest.joints[3][1];
    let (mut crown, mut chin) = (0.0_f32, 0.0_f32);
    for (before, after) in rest.vertices.iter().zip(&open.vertices) {
        let shift = distance(*before, *after);
        if before[1] > eye_height + 0.05 {
            crown = crown.max(shift);
        } else if before[1] < rest.joints[2][1] - 0.03 {
            chin = chin.max(shift);
        }
    }
    assert!(chin > 0.01, "chin moved only {chin} m");
    assert!(
        crown < chin / 10.0,
        "crown moved {crown} m against chin {chin} m"
    );
}
