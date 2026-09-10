use manifold_csg::Manifold;
use parry3d_f64::math::{Pose, Vector};
use parry3d_f64::query::{
    ClosestPoints, NonlinearRigidMotion, ShapeCastOptions, ShapeCastStatus, cast_shapes,
    closest_points, distance,
};
use parry3d_f64::shape::TriMesh;
use serde::{Deserialize, Serialize};

use super::{
    GeometryError, MeshBounds, PreparedMesh, TriangleMesh, ValidationOptions, decode_stl,
    normalized_direction, prepare_mesh,
};

/// Optional rigid linear motion to evaluate after the static assembly relationship.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinearMotion {
    pub direction: [f64; 3],
    pub travel_mm: f64,
    pub target_clearance_mm: Option<f64>,
}

/// Optional rigid rotation around a world-space pivot and right-hand-rule axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RotationalMotion {
    pub pivot_mm: [f64; 3],
    pub axis: [f64; 3],
    pub angle_degrees: f64,
    pub target_clearance_mm: Option<f64>,
}

/// Caller-selected static and motion-clearance requirements.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AssemblyOptions {
    pub required_clearance_mm: Option<f64>,
    pub motion: Option<LinearMotion>,
    #[serde(default)]
    pub rotation: Option<RotationalMotion>,
}

impl AssemblyOptions {
    /// Validate scalar requirements before artifact decoding or geometry allocation.
    pub fn validate(self) -> Result<(), GeometryError> {
        validate_options(self).map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssemblyRelation {
    Separated,
    Contact,
    Interfering,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceWitnesses {
    pub fixed_mm: [f64; 3],
    pub moving_mm: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssemblyPartSummary {
    pub vertices: usize,
    pub triangles: usize,
    pub bounds: MeshBounds,
    pub volume_mm3: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssemblyStaticReport {
    pub relation: AssemblyRelation,
    pub clearance_mm: f64,
    pub surface_gap_mm: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closest_surface_points: Option<SurfaceWitnesses>,
    pub interference_volume_mm3: f64,
    pub fixed_interference_fraction: f64,
    pub moving_interference_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_clearance_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meets_required_clearance: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionBlockReason {
    InitialInterference,
    InsufficientInitialClearance,
    ClearanceThreshold,
    Contact,
    ClearanceNotCertified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearMotionReport {
    pub direction: [f64; 3],
    pub travel_mm: f64,
    pub target_clearance_mm: f64,
    pub can_translate_full_distance: bool,
    /// True only when physical interference or zero-clearance contact blocks the path.
    pub retained: bool,
    /// First limiting distance under the requested clearance envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_blocked_at_mm: Option<f64>,
    /// Distinguishes a design-clearance limit from physical geometry retention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<MotionBlockReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clearance_at_end_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationalMotionReport {
    pub pivot_mm: [f64; 3],
    pub axis: [f64; 3],
    pub angle_degrees: f64,
    pub target_clearance_mm: f64,
    pub can_rotate_full_angle: bool,
    /// True only when physical interference or zero-clearance contact blocks the path.
    pub retained: bool,
    /// Conservative interval containing the earliest observed or uncertified limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_limit_interval_degrees: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<MotionBlockReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clearance_at_end_mm: Option<f64>,
    /// Proven lower bound over the complete angular path when the path is certified clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_certified_clearance_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssemblyReport {
    pub fixed: AssemblyPartSummary,
    pub moving: AssemblyPartSummary,
    pub static_analysis: AssemblyStaticReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motion: Option<LinearMotionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<RotationalMotionReport>,
}

struct SolidPart {
    summary: AssemblyPartSummary,
    trimesh: TriMesh,
    manifold: Manifold,
}

#[derive(Debug)]
struct ValidatedMotion {
    direction: [f64; 3],
    travel_mm: f64,
    target_clearance_mm: f64,
}

#[derive(Debug, Clone, Copy)]
struct ValidatedRotation {
    pivot_mm: [f64; 3],
    axis: [f64; 3],
    angle_degrees: f64,
    target_clearance_mm: f64,
}

#[derive(Debug, Default)]
struct ValidatedOptions {
    motion: Option<ValidatedMotion>,
    rotation: Option<ValidatedRotation>,
}

const ROTATION_CLEARANCE_QUERY_BUDGET: usize = 4_096;

/// Decode two STL snapshots and analyze their static and optional rigid-motion relationship.
pub fn analyze_assembly_stl(
    fixed: &[u8],
    moving: &[u8],
    options: AssemblyOptions,
) -> Result<AssemblyReport, GeometryError> {
    let validated_motion = validate_options(options)?;
    let fixed = decode_stl(fixed).map_err(|error| invalid_part("fixed", error.to_string()))?;
    let moving = decode_stl(moving).map_err(|error| invalid_part("moving", error.to_string()))?;
    analyze_validated_assembly(&fixed, &moving, options, validated_motion)
}

/// Analyze two neutral meshes in the same millimetre coordinate system.
pub fn analyze_assembly(
    fixed: &TriangleMesh,
    moving: &TriangleMesh,
    options: AssemblyOptions,
) -> Result<AssemblyReport, GeometryError> {
    let validated_motion = validate_options(options)?;
    analyze_validated_assembly(fixed, moving, options, validated_motion)
}

fn analyze_validated_assembly(
    fixed: &TriangleMesh,
    moving: &TriangleMesh,
    options: AssemblyOptions,
    validated: ValidatedOptions,
) -> Result<AssemblyReport, GeometryError> {
    let fixed = prepare_solid("fixed", fixed)?;
    let moving = prepare_solid("moving", moving)?;
    let static_analysis = static_analysis(&fixed, &moving, options.required_clearance_mm)?;
    let motion = validated
        .motion
        .map(|motion| linear_motion_analysis(&fixed, &moving, &static_analysis, motion))
        .transpose()?;
    let rotation = validated
        .rotation
        .map(|rotation| {
            rotation_analysis(
                &fixed,
                &moving,
                &static_analysis,
                rotation,
                ROTATION_CLEARANCE_QUERY_BUDGET,
            )
        })
        .transpose()?;

    Ok(AssemblyReport {
        fixed: fixed.summary,
        moving: moving.summary,
        static_analysis,
        motion,
        rotation,
    })
}

fn validate_options(options: AssemblyOptions) -> Result<ValidatedOptions, GeometryError> {
    if options
        .required_clearance_mm
        .is_some_and(|clearance| !clearance.is_finite() || clearance < 0.0)
    {
        return Err(GeometryError::InvalidOptions(
            "required_clearance_mm must be finite and non-negative".to_string(),
        ));
    }
    let motion = options
        .motion
        .map(|motion| {
            if !motion.travel_mm.is_finite() || motion.travel_mm <= 0.0 {
                return Err(GeometryError::InvalidOptions(
                    "motion.travel_mm must be finite and positive".to_string(),
                ));
            }
            if motion
                .target_clearance_mm
                .is_some_and(|clearance| !clearance.is_finite() || clearance < 0.0)
            {
                return Err(GeometryError::InvalidOptions(
                    "motion.target_clearance_mm must be finite and non-negative".to_string(),
                ));
            }
            Ok(ValidatedMotion {
                direction: normalized_direction(motion.direction, "motion.direction")?,
                travel_mm: motion.travel_mm,
                target_clearance_mm: motion
                    .target_clearance_mm
                    .or(options.required_clearance_mm)
                    .unwrap_or(0.0),
            })
        })
        .transpose()?;
    let rotation = options
        .rotation
        .map(|rotation| {
            if rotation
                .pivot_mm
                .iter()
                .any(|coordinate| !coordinate.is_finite())
            {
                return Err(GeometryError::InvalidOptions(
                    "rotation.pivot_mm must contain only finite coordinates".to_string(),
                ));
            }
            if !rotation.angle_degrees.is_finite() || rotation.angle_degrees <= 0.0 {
                return Err(GeometryError::InvalidOptions(
                    "rotation.angle_degrees must be finite and positive".to_string(),
                ));
            }
            if rotation
                .target_clearance_mm
                .is_some_and(|clearance| !clearance.is_finite() || clearance < 0.0)
            {
                return Err(GeometryError::InvalidOptions(
                    "rotation.target_clearance_mm must be finite and non-negative".to_string(),
                ));
            }
            Ok(ValidatedRotation {
                pivot_mm: rotation.pivot_mm,
                axis: normalized_direction(rotation.axis, "rotation.axis")?,
                angle_degrees: rotation.angle_degrees,
                target_clearance_mm: rotation
                    .target_clearance_mm
                    .or(options.required_clearance_mm)
                    .unwrap_or(0.0),
            })
        })
        .transpose()?;
    Ok(ValidatedOptions { motion, rotation })
}

fn prepare_solid(part: &'static str, mesh: &TriangleMesh) -> Result<SolidPart, GeometryError> {
    let PreparedMesh {
        report,
        trimesh,
        manifold,
    } = prepare_mesh(mesh, ValidationOptions::default())
        .map_err(|error| invalid_part(part, error.to_string()))?;
    let manifold = manifold.ok_or_else(|| {
        let reason = report
            .issues
            .iter()
            .filter(|issue| issue.severity == super::IssueSeverity::Error)
            .map(|issue| format!("{}: {}", issue.code, issue.message))
            .collect::<Vec<_>>()
            .join("; ");
        invalid_part(
            part,
            if reason.is_empty() {
                "geometry kernel did not produce a solid".to_string()
            } else {
                reason
            },
        )
    })?;
    let properties = report.solid_properties.ok_or_else(|| {
        invalid_part(
            part,
            "geometry kernel did not produce solid properties".to_string(),
        )
    })?;
    if properties.volume_mm3 <= 0.0 {
        return Err(invalid_part(
            part,
            "solid volume is not positive".to_string(),
        ));
    }
    Ok(SolidPart {
        summary: AssemblyPartSummary {
            vertices: report.vertices,
            triangles: report.triangles,
            bounds: report.bounds,
            volume_mm3: properties.volume_mm3,
        },
        trimesh,
        manifold,
    })
}

fn static_analysis(
    fixed: &SolidPart,
    moving: &SolidPart,
    required_clearance_mm: Option<f64>,
) -> Result<AssemblyStaticReport, GeometryError> {
    let intersection = fixed.manifold.intersection(&moving.manifold);
    intersection
        .status()
        .map_err(|error| GeometryError::Kernel(format!("solid intersection failed: {error}")))?;
    let interference_volume_mm3 =
        finite_nonnegative(intersection.volume(), "solid intersection volume")?;

    let (surface_gap_mm, closest_surface_points) = match closest_points(
        &Pose::identity(),
        &fixed.trimesh,
        &Pose::identity(),
        &moving.trimesh,
        f64::MAX,
    )
    .map_err(|_| GeometryError::Kernel("surface proximity query is unsupported".to_string()))?
    {
        ClosestPoints::Intersecting => (0.0, None),
        ClosestPoints::WithinMargin(fixed, moving) => (
            (moving - fixed).length(),
            Some(SurfaceWitnesses {
                fixed_mm: fixed.to_array(),
                moving_mm: moving.to_array(),
            }),
        ),
        ClosestPoints::Disjoint => {
            return Err(GeometryError::Kernel(
                "surface proximity query did not return closest points".to_string(),
            ));
        }
    };
    let surface_gap_mm = finite_nonnegative(surface_gap_mm, "surface gap")?;

    let relation = if interference_volume_mm3 > 0.0 {
        AssemblyRelation::Interfering
    } else if surface_gap_mm == 0.0 {
        AssemblyRelation::Contact
    } else {
        AssemblyRelation::Separated
    };
    let clearance_mm = if relation == AssemblyRelation::Separated {
        surface_gap_mm
    } else {
        0.0
    };
    Ok(AssemblyStaticReport {
        relation,
        clearance_mm,
        surface_gap_mm,
        closest_surface_points,
        interference_volume_mm3,
        fixed_interference_fraction: interference_volume_mm3 / fixed.summary.volume_mm3,
        moving_interference_fraction: interference_volume_mm3 / moving.summary.volume_mm3,
        required_clearance_mm,
        meets_required_clearance: required_clearance_mm
            .map(|required| relation != AssemblyRelation::Interfering && clearance_mm >= required),
    })
}

fn linear_motion_analysis(
    fixed: &SolidPart,
    moving: &SolidPart,
    static_analysis: &AssemblyStaticReport,
    motion: ValidatedMotion,
) -> Result<LinearMotionReport, GeometryError> {
    if static_analysis.interference_volume_mm3 > 0.0 {
        return Ok(blocked_motion(
            motion,
            MotionBlockReason::InitialInterference,
        ));
    }
    if static_analysis.clearance_mm < motion.target_clearance_mm {
        return Ok(blocked_motion(
            motion,
            MotionBlockReason::InsufficientInitialClearance,
        ));
    }
    if static_analysis.clearance_mm == motion.target_clearance_mm {
        let reason = if motion.target_clearance_mm > 0.0 {
            MotionBlockReason::ClearanceThreshold
        } else {
            MotionBlockReason::Contact
        };
        return Ok(blocked_motion(motion, reason));
    }

    let direction = Vector::from_array(motion.direction);
    let hit = cast_shapes(
        &Pose::identity(),
        Vector::ZERO,
        &fixed.trimesh,
        &Pose::identity(),
        direction,
        &moving.trimesh,
        ShapeCastOptions {
            max_time_of_impact: motion.travel_mm,
            target_distance: motion.target_clearance_mm,
            stop_at_penetration: true,
            compute_impact_geometry_on_penetration: true,
        },
    )
    .map_err(|_| GeometryError::Kernel("linear mesh sweep is unsupported".to_string()))?;

    if let Some(hit) = hit {
        match hit.status {
            ShapeCastStatus::Converged | ShapeCastStatus::PenetratingOrWithinTargetDist => {}
            ShapeCastStatus::OutOfIterations | ShapeCastStatus::Failed => {
                return Err(GeometryError::Kernel(
                    "linear mesh sweep did not converge".to_string(),
                ));
            }
        }
        let contact_distance_mm = validated_contact_distance(hit.time_of_impact, motion.travel_mm)?;
        let block_reason = if motion.target_clearance_mm > 0.0 {
            MotionBlockReason::ClearanceThreshold
        } else {
            MotionBlockReason::Contact
        };
        return Ok(LinearMotionReport {
            direction: motion.direction,
            travel_mm: motion.travel_mm,
            target_clearance_mm: motion.target_clearance_mm,
            can_translate_full_distance: false,
            retained: block_reason == MotionBlockReason::Contact,
            first_blocked_at_mm: Some(contact_distance_mm),
            block_reason: Some(block_reason),
            clearance_at_end_mm: None,
        });
    }

    let moving_end = translated_pose(motion.direction, motion.travel_mm);
    let clearance_at_end_mm = surface_distance(fixed, moving, &moving_end)?;
    Ok(clear_motion(motion, clearance_at_end_mm))
}

fn rotation_analysis(
    fixed: &SolidPart,
    moving: &SolidPart,
    static_analysis: &AssemblyStaticReport,
    rotation: ValidatedRotation,
    query_budget: usize,
) -> Result<RotationalMotionReport, GeometryError> {
    if static_analysis.interference_volume_mm3 > 0.0 {
        return Ok(blocked_rotation(
            rotation,
            MotionBlockReason::InitialInterference,
            [0.0, 0.0],
        ));
    }
    if static_analysis.clearance_mm < rotation.target_clearance_mm {
        return Ok(blocked_rotation(
            rotation,
            MotionBlockReason::InsufficientInitialClearance,
            [0.0, 0.0],
        ));
    }
    if static_analysis.clearance_mm == rotation.target_clearance_mm {
        let reason = if rotation.target_clearance_mm > 0.0 {
            MotionBlockReason::ClearanceThreshold
        } else {
            MotionBlockReason::Contact
        };
        return Ok(blocked_rotation(rotation, reason, [0.0, 0.0]));
    }

    let rigid_motion = rotational_rigid_motion(&rotation);
    let maximum_radius_mm = moving
        .trimesh
        .vertices()
        .iter()
        .map(|vertex| (*vertex - Vector::from_array(rotation.pivot_mm)).length())
        .fold(0.0, f64::max);
    let sweep = certify_rotation_clearance(
        fixed,
        moving,
        &rigid_motion,
        &rotation,
        maximum_radius_mm,
        query_budget,
    )?;
    match sweep {
        RotationSweep::Clear {
            minimum_clearance_mm,
        } => {
            let end_pose = rigid_motion.position_at_time(1.0);
            let clearance_at_end_mm = surface_distance(fixed, moving, &end_pose)?;
            Ok(RotationalMotionReport {
                pivot_mm: rotation.pivot_mm,
                axis: rotation.axis,
                angle_degrees: rotation.angle_degrees,
                target_clearance_mm: rotation.target_clearance_mm,
                can_rotate_full_angle: true,
                retained: false,
                first_limit_interval_degrees: None,
                block_reason: None,
                clearance_at_end_mm: Some(clearance_at_end_mm),
                minimum_certified_clearance_mm: Some(minimum_clearance_mm),
            })
        }
        RotationSweep::Blocked { interval, reason } => Ok(blocked_rotation(
            rotation,
            reason,
            interval.map(|time| time * rotation.angle_degrees),
        )),
        RotationSweep::NotCertified { interval } => Ok(blocked_rotation(
            rotation,
            MotionBlockReason::ClearanceNotCertified,
            interval.map(|time| time * rotation.angle_degrees),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
struct RotationInterval {
    start: f64,
    end: f64,
}

impl RotationInterval {
    fn map(self, transform: impl Fn(f64) -> f64) -> [f64; 2] {
        [transform(self.start), transform(self.end)]
    }
}

enum RotationSweep {
    Clear {
        minimum_clearance_mm: f64,
    },
    Blocked {
        interval: RotationInterval,
        reason: MotionBlockReason,
    },
    NotCertified {
        interval: RotationInterval,
    },
}

fn certify_rotation_clearance(
    fixed: &SolidPart,
    moving: &SolidPart,
    rigid_motion: &NonlinearRigidMotion,
    rotation: &ValidatedRotation,
    maximum_radius_mm: f64,
    query_budget: usize,
) -> Result<RotationSweep, GeometryError> {
    let mut pending = vec![RotationInterval {
        start: 0.0,
        end: 1.0,
    }];
    let mut queries = 0;
    let mut minimum_clearance_mm = f64::INFINITY;

    while let Some(interval) = pending.pop() {
        if queries == query_budget {
            return Ok(RotationSweep::NotCertified { interval });
        }
        let midpoint = (interval.start + interval.end) * 0.5;
        let midpoint_pose = rigid_motion.position_at_time(midpoint);
        let midpoint_clearance_mm = surface_distance(fixed, moving, &midpoint_pose)?;
        queries += 1;

        let angular_width_radians =
            rotation.angle_degrees.to_radians() * (interval.end - interval.start);
        let maximum_deviation_mm =
            rotational_deviation_bound(maximum_radius_mm, angular_width_radians);
        let clearance_lower_bound_mm = midpoint_clearance_mm - maximum_deviation_mm;
        if clearance_is_certified(clearance_lower_bound_mm, rotation.target_clearance_mm) {
            minimum_clearance_mm = minimum_clearance_mm.min(clearance_lower_bound_mm);
            continue;
        }

        let interval_width_degrees = rotation.angle_degrees * (interval.end - interval.start);
        if let Some(reason) = observed_rotation_limit(
            midpoint_clearance_mm,
            rotation.target_clearance_mm,
            interval_width_degrees,
        ) {
            return Ok(RotationSweep::Blocked {
                interval: RotationInterval {
                    start: interval.start,
                    end: midpoint,
                },
                reason,
            });
        }

        pending.push(RotationInterval {
            start: midpoint,
            end: interval.end,
        });
        pending.push(RotationInterval {
            start: interval.start,
            end: midpoint,
        });
    }

    Ok(RotationSweep::Clear {
        minimum_clearance_mm,
    })
}

fn rotational_deviation_bound(maximum_radius_mm: f64, angular_width_radians: f64) -> f64 {
    let midpoint_to_endpoint_radians = angular_width_radians * 0.5;
    // Every moving point stays within this chord distance of its midpoint
    // pose, so the surface distance cannot decrease by more than this bound.
    if midpoint_to_endpoint_radians >= std::f64::consts::PI {
        2.0 * maximum_radius_mm
    } else {
        2.0 * maximum_radius_mm * (midpoint_to_endpoint_radians * 0.5).sin()
    }
}

fn clearance_is_certified(clearance_lower_bound_mm: f64, target_clearance_mm: f64) -> bool {
    clearance_lower_bound_mm > target_clearance_mm
}

fn observed_rotation_limit(
    midpoint_clearance_mm: f64,
    target_clearance_mm: f64,
    interval_width_degrees: f64,
) -> Option<MotionBlockReason> {
    if midpoint_clearance_mm <= target_clearance_mm && interval_width_degrees <= 0.1 {
        Some(if target_clearance_mm > 0.0 {
            MotionBlockReason::ClearanceThreshold
        } else {
            MotionBlockReason::Contact
        })
    } else {
        None
    }
}

fn rotational_rigid_motion(rotation: &ValidatedRotation) -> NonlinearRigidMotion {
    let angular_velocity = Vector::from_array(rotation.axis) * rotation.angle_degrees.to_radians();
    NonlinearRigidMotion::new(
        Pose::identity(),
        Vector::from_array(rotation.pivot_mm),
        Vector::ZERO,
        angular_velocity,
    )
}

fn translated_pose(direction: [f64; 3], distance_mm: f64) -> Pose {
    Pose::translation(
        direction[0] * distance_mm,
        direction[1] * distance_mm,
        direction[2] * distance_mm,
    )
}

fn surface_distance(
    fixed: &SolidPart,
    moving: &SolidPart,
    moving_pose: &Pose,
) -> Result<f64, GeometryError> {
    let clearance_mm = distance(
        &Pose::identity(),
        &fixed.trimesh,
        moving_pose,
        &moving.trimesh,
    )
    .map_err(|_| GeometryError::Kernel("surface-distance query is unsupported".to_string()))?;
    finite_nonnegative(clearance_mm, "surface distance")
}

fn finite_nonnegative(value: f64, field: &'static str) -> Result<f64, GeometryError> {
    if !value.is_finite() || value < 0.0 {
        return Err(GeometryError::Kernel(format!(
            "{field} is not finite and non-negative"
        )));
    }
    Ok(value)
}

fn validated_contact_distance(value: f64, maximum: f64) -> Result<f64, GeometryError> {
    let value = finite_nonnegative(value, "linear mesh sweep contact distance")?;
    if value > maximum {
        return Err(GeometryError::Kernel(
            "linear mesh sweep contact distance exceeds requested travel".to_string(),
        ));
    }
    Ok(value)
}

fn clear_motion(motion: ValidatedMotion, clearance_at_end_mm: f64) -> LinearMotionReport {
    LinearMotionReport {
        direction: motion.direction,
        travel_mm: motion.travel_mm,
        target_clearance_mm: motion.target_clearance_mm,
        can_translate_full_distance: true,
        retained: false,
        first_blocked_at_mm: None,
        block_reason: None,
        clearance_at_end_mm: Some(clearance_at_end_mm),
    }
}

fn blocked_motion(motion: ValidatedMotion, reason: MotionBlockReason) -> LinearMotionReport {
    LinearMotionReport {
        direction: motion.direction,
        travel_mm: motion.travel_mm,
        target_clearance_mm: motion.target_clearance_mm,
        can_translate_full_distance: false,
        retained: matches!(
            reason,
            MotionBlockReason::InitialInterference | MotionBlockReason::Contact
        ),
        first_blocked_at_mm: Some(0.0),
        block_reason: Some(reason),
        clearance_at_end_mm: None,
    }
}

fn blocked_rotation(
    rotation: ValidatedRotation,
    reason: MotionBlockReason,
    interval_degrees: [f64; 2],
) -> RotationalMotionReport {
    RotationalMotionReport {
        pivot_mm: rotation.pivot_mm,
        axis: rotation.axis,
        angle_degrees: rotation.angle_degrees,
        target_clearance_mm: rotation.target_clearance_mm,
        can_rotate_full_angle: false,
        retained: matches!(
            reason,
            MotionBlockReason::InitialInterference | MotionBlockReason::Contact
        ),
        first_limit_interval_degrees: Some(interval_degrees),
        block_reason: Some(reason),
        clearance_at_end_mm: None,
        minimum_certified_clearance_mm: None,
    }
}

fn invalid_part(part: &'static str, reason: String) -> GeometryError {
    GeometryError::InvalidAssemblyPart { part, reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(size: f64, offset: [f64; 3]) -> TriangleMesh {
        let [x, y, z] = offset;
        let vertices = vec![
            [x, y, z],
            [x + size, y, z],
            [x + size, y + size, z],
            [x, y + size, z],
            [x, y, z + size],
            [x + size, y, z + size],
            [x + size, y + size, z + size],
            [x, y + size, z + size],
        ];
        let triangles = vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ];
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    fn append_mesh(destination: &mut TriangleMesh, source: TriangleMesh) {
        let offset = destination.vertices.len() as u32;
        destination.vertices.extend(source.vertices);
        destination.triangles.extend(
            source
                .triangles
                .into_iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
    }

    fn quarter_turn(target_clearance_mm: Option<f64>) -> RotationalMotion {
        RotationalMotion {
            pivot_mm: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
            angle_degrees: 90.0,
            target_clearance_mm,
        }
    }

    #[test]
    fn rotational_sweep_certifies_clearance_over_the_complete_arc() {
        let report = analyze_assembly(
            &cube(1.0, [5.0, 5.0, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                rotation: Some(quarter_turn(Some(0.5))),
                ..AssemblyOptions::default()
            },
        )
        .expect("rotation report");

        let rotation = report.rotation.expect("rotation analysis");
        assert!(rotation.can_rotate_full_angle, "{rotation:?}");
        assert!(!rotation.retained);
        assert_eq!(rotation.block_reason, None);
        assert!(
            rotation
                .minimum_certified_clearance_mm
                .expect("clearance certificate")
                >= 0.5
        );
        assert!(rotation.clearance_at_end_mm.expect("end clearance") > 0.5);
    }

    #[test]
    fn rotational_sweep_refines_a_close_but_clear_arc() {
        let report = analyze_assembly(
            &cube(0.5, [1.1, 3.4, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                rotation: Some(quarter_turn(Some(0.2))),
                ..AssemblyOptions::default()
            },
        )
        .expect("rotation report");

        let rotation = report.rotation.expect("rotation analysis");
        assert!(rotation.can_rotate_full_angle, "{rotation:?}");
        assert!(
            rotation
                .minimum_certified_clearance_mm
                .expect("clearance certificate")
                > 0.2
        );
    }

    #[test]
    fn rotational_sweep_detects_contact_between_clear_endpoints() {
        let report = analyze_assembly(
            &cube(0.5, [1.1, 1.8, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                rotation: Some(quarter_turn(None)),
                ..AssemblyOptions::default()
            },
        )
        .expect("rotation report");

        assert_eq!(report.static_analysis.relation, AssemblyRelation::Separated);
        let rotation = report.rotation.expect("rotation analysis");
        assert!(!rotation.can_rotate_full_angle, "{rotation:?}");
        assert!(rotation.retained);
        assert_eq!(rotation.block_reason, Some(MotionBlockReason::Contact));
        let [start, end] = rotation
            .first_limit_interval_degrees
            .expect("contact interval");
        assert!(start > 10.0 && start <= end && end < 80.0, "{rotation:?}");
        assert!(end - start <= 0.1, "{rotation:?}");
    }

    #[test]
    fn rotational_clearance_limit_does_not_claim_physical_retention() {
        let report = analyze_assembly(
            &cube(0.5, [1.1, 1.8, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                rotation: Some(quarter_turn(Some(0.25))),
                ..AssemblyOptions::default()
            },
        )
        .expect("rotation report");

        let rotation = report.rotation.expect("rotation analysis");
        assert!(!rotation.can_rotate_full_angle, "{rotation:?}");
        assert!(!rotation.retained);
        assert_eq!(
            rotation.block_reason,
            Some(MotionBlockReason::ClearanceThreshold)
        );
    }

    #[test]
    fn rotational_contact_angle_is_translation_invariant() {
        let analyze = |offset: [f64; 3]| {
            analyze_assembly(
                &cube(0.5, [1.1 + offset[0], 1.8 + offset[1], offset[2]]),
                &cube(1.0, [2.0 + offset[0], offset[1], offset[2]]),
                AssemblyOptions {
                    rotation: Some(RotationalMotion {
                        pivot_mm: offset,
                        ..quarter_turn(None)
                    }),
                    ..AssemblyOptions::default()
                },
            )
            .expect("rotation report")
            .rotation
            .expect("rotation analysis")
            .first_limit_interval_degrees
            .expect("contact interval")
        };

        let origin = analyze([0.0, 0.0, 0.0]);
        let translated = analyze([10.0, -3.0, 2.0]);
        assert!((origin[0] - translated[0]).abs() < 1e-9);
        assert!((origin[1] - translated[1]).abs() < 1e-9);
    }

    #[test]
    fn rotational_clearance_certificate_is_translation_invariant() {
        let analyze = |offset: [f64; 3]| {
            analyze_assembly(
                &cube(1.0, [5.0 + offset[0], 5.0 + offset[1], offset[2]]),
                &cube(1.0, [2.0 + offset[0], offset[1], offset[2]]),
                AssemblyOptions {
                    rotation: Some(RotationalMotion {
                        pivot_mm: offset,
                        ..quarter_turn(Some(0.5))
                    }),
                    ..AssemblyOptions::default()
                },
            )
            .expect("rotation report")
            .rotation
            .expect("rotation analysis")
            .minimum_certified_clearance_mm
            .expect("clearance certificate")
        };

        let origin = analyze([0.0, 0.0, 0.0]);
        let translated = analyze([10.0, -3.0, 2.0]);
        assert!((origin - translated).abs() < 1e-9);
    }

    #[test]
    fn rotational_initial_limits_are_classified_before_the_sweep() {
        for (moving_x, target, reason, retained) in [
            (0.5, 0.0, MotionBlockReason::InitialInterference, true),
            (1.0, 0.0, MotionBlockReason::Contact, true),
            (
                1.25,
                0.5,
                MotionBlockReason::InsufficientInitialClearance,
                false,
            ),
            (1.5, 0.5, MotionBlockReason::ClearanceThreshold, false),
        ] {
            let report = analyze_assembly(
                &cube(1.0, [0.0, 0.0, 0.0]),
                &cube(1.0, [moving_x, 0.0, 0.0]),
                AssemblyOptions {
                    rotation: Some(quarter_turn(Some(target))),
                    ..AssemblyOptions::default()
                },
            )
            .expect("rotation report");

            let rotation = report.rotation.expect("rotation analysis");
            assert_eq!(rotation.block_reason, Some(reason));
            assert_eq!(rotation.retained, retained);
            assert_eq!(rotation.first_limit_interval_degrees, Some([0.0, 0.0]));
        }
    }

    #[test]
    fn rotational_clearance_budget_exhaustion_fails_closed() {
        let fixed = prepare_solid("fixed", &cube(1.0, [5.0, 5.0, 0.0])).expect("fixed");
        let moving = prepare_solid("moving", &cube(1.0, [2.0, 0.0, 0.0])).expect("moving");
        let rotation = ValidatedRotation {
            pivot_mm: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
            angle_degrees: 90.0,
            target_clearance_mm: 0.5,
        };
        let rigid_motion = rotational_rigid_motion(&rotation);
        let result = certify_rotation_clearance(&fixed, &moving, &rigid_motion, &rotation, 4.0, 0)
            .expect("bounded analysis");

        assert!(matches!(result, RotationSweep::NotCertified { .. }));

        let one_query =
            certify_rotation_clearance(&fixed, &moving, &rigid_motion, &rotation, 100.0, 1)
                .expect("one-query analysis");
        assert!(matches!(
            one_query,
            RotationSweep::NotCertified {
                interval: RotationInterval {
                    start: 0.0,
                    end: 0.5
                }
            }
        ));

        let static_analysis = static_analysis(&fixed, &moving, None).expect("static analysis");
        let report = rotation_analysis(&fixed, &moving, &static_analysis, rotation, 0)
            .expect("bounded rotation report");
        assert_eq!(
            report.block_reason,
            Some(MotionBlockReason::ClearanceNotCertified)
        );
        assert_eq!(report.first_limit_interval_degrees, Some([0.0, 90.0]));
    }

    #[test]
    fn rotational_certificate_math_enforces_its_geometric_boundaries() {
        let quarter_arc_bound = rotational_deviation_bound(2.0, std::f64::consts::PI);
        assert!((quarter_arc_bound - 2.0 * 2.0_f64.sqrt()).abs() < 1e-12);
        let larger_radius_bound = rotational_deviation_bound(3.0, std::f64::consts::PI);
        assert!((larger_radius_bound - 3.0 * 2.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(
            rotational_deviation_bound(2.0, 4.0 * std::f64::consts::PI),
            4.0
        );
        assert_eq!(
            rotational_deviation_bound(3.0, 4.0 * std::f64::consts::PI),
            6.0
        );
        assert!(!clearance_is_certified(0.5, 0.5));
        assert!(!clearance_is_certified(0.4, 0.5));
        assert!(clearance_is_certified(0.6, 0.5));
        assert_eq!(observed_rotation_limit(0.6, 0.5, 0.1), None);
        assert_eq!(
            observed_rotation_limit(0.5, 0.5, 0.1),
            Some(MotionBlockReason::ClearanceThreshold)
        );
        assert_eq!(observed_rotation_limit(0.0, 0.0, 0.2), None);
        assert_eq!(
            observed_rotation_limit(0.0, 0.0, 0.1),
            Some(MotionBlockReason::Contact)
        );

        let motion = rotational_rigid_motion(&ValidatedRotation {
            pivot_mm: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
            angle_degrees: 90.0,
            target_clearance_mm: 0.0,
        });
        let rotated = motion.position_at_time(1.0) * Vector::new(2.0, 0.0, 0.0);
        assert!(rotated.x.abs() < 1e-12);
        assert!((rotated.y - 2.0).abs() < 1e-12);
        assert!(rotated.z.abs() < 1e-12);
    }

    #[test]
    fn separated_parts_report_clearance_and_continuous_clearance_limit() {
        let report = analyze_assembly(
            &cube(2.0, [0.0, 0.0, 0.0]),
            &cube(2.0, [5.0, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(2.0),
                motion: Some(LinearMotion {
                    direction: [-1.0, 0.0, 0.0],
                    travel_mm: 5.0,
                    target_clearance_mm: Some(0.5),
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        assert_eq!(report.static_analysis.relation, AssemblyRelation::Separated);
        assert!((report.static_analysis.clearance_mm - 3.0).abs() < 1e-9);
        assert_eq!(report.static_analysis.meets_required_clearance, Some(true));
        let witnesses = report
            .static_analysis
            .closest_surface_points
            .expect("surface witnesses");
        assert!((witnesses.moving_mm[0] - witnesses.fixed_mm[0] - 3.0).abs() < 1e-9);
        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(!motion.retained);
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::ClearanceThreshold)
        );
        assert!((motion.first_blocked_at_mm.expect("clearance limit") - 2.5).abs() < 1e-9);
    }

    #[test]
    fn motion_starting_at_contact_blocks_at_zero() {
        let report = analyze_assembly(
            &cube(2.0, [0.0, 0.0, 0.0]),
            &cube(2.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(0.0),
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 3.0,
                    target_clearance_mm: None,
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        assert_eq!(report.static_analysis.relation, AssemblyRelation::Contact);
        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(motion.retained);
        assert_eq!(motion.first_blocked_at_mm, Some(0.0));
        assert_eq!(motion.block_reason, Some(MotionBlockReason::Contact));
    }

    #[test]
    fn motion_from_positive_clearance_detects_a_later_obstacle() {
        let mut fixed = cube(2.0, [0.0, 0.0, 0.0]);
        append_mesh(&mut fixed, cube(2.0, [5.0, 0.0, 0.0]));
        let report = analyze_assembly(
            &fixed,
            &cube(2.0, [2.1, 0.0, 0.0]),
            AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 3.0,
                    target_clearance_mm: None,
                }),
                ..AssemblyOptions::default()
            },
        )
        .expect("assembly report");

        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(motion.retained);
        assert_eq!(motion.block_reason, Some(MotionBlockReason::Contact));
        assert!((motion.first_blocked_at_mm.expect("contact") - 0.9).abs() < 1e-9);
    }

    #[test]
    fn overlap_reports_exact_interference_and_blocks_motion_at_start() {
        let report = analyze_assembly(
            &cube(2.0, [0.0, 0.0, 0.0]),
            &cube(2.0, [1.0, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(0.0),
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 3.0,
                    target_clearance_mm: None,
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        assert_eq!(
            report.static_analysis.relation,
            AssemblyRelation::Interfering
        );
        assert!((report.static_analysis.interference_volume_mm3 - 4.0).abs() < 1e-9);
        assert!((report.static_analysis.fixed_interference_fraction - 0.5).abs() < 1e-9);
        assert!((report.static_analysis.moving_interference_fraction - 0.5).abs() < 1e-9);
        assert_eq!(report.static_analysis.meets_required_clearance, Some(false));
        let motion = report.motion.expect("motion report");
        assert_eq!(motion.first_blocked_at_mm, Some(0.0));
        assert!(motion.retained);
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::InitialInterference)
        );
    }

    #[test]
    fn containment_is_interference_even_when_surfaces_have_a_gap() {
        let report = analyze_assembly(
            &cube(4.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [1.5, 1.5, 1.5]),
            AssemblyOptions::default(),
        )
        .expect("assembly report");

        assert_eq!(
            report.static_analysis.relation,
            AssemblyRelation::Interfering
        );
        assert_eq!(report.static_analysis.clearance_mm, 0.0);
        assert!((report.static_analysis.surface_gap_mm - 1.5).abs() < 1e-9);
        assert!((report.static_analysis.interference_volume_mm3 - 1.0).abs() < 1e-9);
        assert!((report.static_analysis.moving_interference_fraction - 1.0).abs() < 1e-9);
    }

    #[test]
    fn required_clearance_is_inherited_by_motion_and_checked_at_start() {
        let report = analyze_assembly(
            &cube(1.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [1.25, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(0.5),
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 1.0,
                    target_clearance_mm: None,
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        assert_eq!(report.static_analysis.meets_required_clearance, Some(false));
        let motion = report.motion.expect("motion report");
        assert_eq!(motion.target_clearance_mm, 0.5);
        assert!(!motion.retained);
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::InsufficientInitialClearance)
        );
    }

    #[test]
    fn motion_starting_on_the_exact_clearance_boundary_blocks_at_zero() {
        let report = analyze_assembly(
            &cube(1.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [1.5, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(0.5),
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 1.0,
                    target_clearance_mm: None,
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(!motion.retained);
        assert_eq!(motion.first_blocked_at_mm, Some(0.0));
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::ClearanceThreshold)
        );
    }

    #[test]
    fn tangent_motion_starting_on_the_clearance_boundary_blocks_at_zero() {
        let report = analyze_assembly(
            &cube(1.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [1.5, 0.0, 0.0]),
            AssemblyOptions {
                required_clearance_mm: Some(0.5),
                motion: Some(LinearMotion {
                    direction: [0.0, 1.0, 0.0],
                    travel_mm: 0.25,
                    target_clearance_mm: None,
                }),
                rotation: None,
            },
        )
        .expect("assembly report");

        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(!motion.retained);
        assert_eq!(motion.first_blocked_at_mm, Some(0.0));
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::ClearanceThreshold)
        );
    }

    #[test]
    fn motion_above_the_clearance_boundary_detects_a_later_clearance_limit() {
        let mut fixed = cube(1.0, [0.0, 0.0, 0.0]);
        append_mesh(&mut fixed, cube(1.0, [1.5, 2.0, 0.0]));
        let report = analyze_assembly(
            &fixed,
            &cube(1.0, [1.5001, 0.0, 0.0]),
            AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [0.0, 1.0, 0.0],
                    travel_mm: 1.0,
                    target_clearance_mm: Some(0.5),
                }),
                ..AssemblyOptions::default()
            },
        )
        .expect("assembly report");

        let motion = report.motion.expect("motion report");
        assert!(!motion.can_translate_full_distance);
        assert!(!motion.retained);
        assert_eq!(
            motion.block_reason,
            Some(MotionBlockReason::ClearanceThreshold)
        );
        assert!((motion.first_blocked_at_mm.expect("clearance limit") - 0.5).abs() < 1e-6);
    }

    #[test]
    fn motion_can_finish_on_the_exact_clearance_boundary() {
        let report = analyze_assembly(
            &cube(1.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [-1.0, 0.0, 0.0],
                    travel_mm: 0.5,
                    target_clearance_mm: Some(0.5),
                }),
                ..AssemblyOptions::default()
            },
        )
        .expect("assembly report");

        let motion = report.motion.expect("motion report");
        assert!(motion.can_translate_full_distance, "{motion:?}");
        assert!(!motion.retained);
        assert!((motion.clearance_at_end_mm.expect("end clearance") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn motion_translation_preserves_y_and_z_axes() {
        for (offset, direction) in [
            ([0.0, 2.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, 2.0], [0.0, 0.0, 1.0]),
        ] {
            let report = analyze_assembly(
                &cube(1.0, [0.0, 0.0, 0.0]),
                &cube(1.0, offset),
                AssemblyOptions {
                    motion: Some(LinearMotion {
                        direction,
                        travel_mm: 2.0,
                        target_clearance_mm: None,
                    }),
                    ..AssemblyOptions::default()
                },
            )
            .expect("axis motion report");

            let motion = report.motion.expect("motion report");
            assert!(motion.can_translate_full_distance);
            assert!((motion.clearance_at_end_mm.expect("end clearance") - 3.0).abs() < 1e-9);
        }
    }

    #[test]
    fn every_assembly_numeric_option_enforces_its_boundary() {
        for invalid in [-1.0, f64::INFINITY, f64::NAN] {
            let error = AssemblyOptions {
                required_clearance_mm: Some(invalid),
                motion: None,
                rotation: None,
            }
            .validate()
            .expect_err("invalid required clearance");
            assert_eq!(error.code(), "validation");
        }
        assert!(
            AssemblyOptions {
                rotation: Some(quarter_turn(Some(0.0))),
                ..AssemblyOptions::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            AssemblyOptions {
                required_clearance_mm: Some(0.0),
                motion: None,
                rotation: None,
            }
            .validate()
            .is_ok()
        );

        for invalid in [-1.0, 0.0, f64::INFINITY, f64::NAN] {
            let error = AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: invalid,
                    target_clearance_mm: None,
                }),
                ..AssemblyOptions::default()
            }
            .validate()
            .expect_err("invalid travel");
            assert_eq!(error.code(), "validation");
        }

        for invalid in [-1.0, f64::INFINITY, f64::NAN] {
            let error = AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 1.0,
                    target_clearance_mm: Some(invalid),
                }),
                ..AssemblyOptions::default()
            }
            .validate()
            .expect_err("invalid motion clearance");
            assert_eq!(error.code(), "validation");
        }
        assert!(
            AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [1.0, 0.0, 0.0],
                    travel_mm: 1.0,
                    target_clearance_mm: Some(0.0),
                }),
                ..AssemblyOptions::default()
            }
            .validate()
            .is_ok()
        );

        for invalid in [-1.0, 0.0, f64::INFINITY, f64::NAN] {
            let error = AssemblyOptions {
                rotation: Some(RotationalMotion {
                    angle_degrees: invalid,
                    ..quarter_turn(None)
                }),
                ..AssemblyOptions::default()
            }
            .validate()
            .expect_err("invalid angular travel");
            assert_eq!(error.code(), "validation");
        }
        for invalid in [-1.0, f64::INFINITY, f64::NAN] {
            let error = AssemblyOptions {
                rotation: Some(RotationalMotion {
                    target_clearance_mm: Some(invalid),
                    ..quarter_turn(None)
                }),
                ..AssemblyOptions::default()
            }
            .validate()
            .expect_err("invalid rotational clearance");
            assert_eq!(error.code(), "validation");
        }
        for rotation in [
            RotationalMotion {
                axis: [0.0; 3],
                ..quarter_turn(None)
            },
            RotationalMotion {
                pivot_mm: [f64::NAN, 0.0, 0.0],
                ..quarter_turn(None)
            },
        ] {
            let error = AssemblyOptions {
                rotation: Some(rotation),
                ..AssemblyOptions::default()
            }
            .validate()
            .expect_err("invalid rotational frame");
            assert_eq!(error.code(), "validation");
        }
    }

    #[test]
    fn kernel_numeric_contracts_reject_non_finite_and_out_of_range_results() {
        for invalid in [-1.0, f64::INFINITY, f64::NAN] {
            assert!(finite_nonnegative(invalid, "test value").is_err());
        }
        assert_eq!(finite_nonnegative(0.0, "test value").unwrap(), 0.0);

        assert_eq!(validated_contact_distance(2.0, 2.0).unwrap(), 2.0);
        assert_eq!(validated_contact_distance(1.0, 2.0).unwrap(), 1.0);
        for invalid in [-1.0, 3.0, f64::INFINITY, f64::NAN] {
            assert!(validated_contact_distance(invalid, 2.0).is_err());
        }
    }

    #[test]
    fn invalid_parts_and_motion_options_fail_with_stable_codes() {
        let open = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2]],
        };
        let error = analyze_assembly(
            &open,
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions::default(),
        )
        .expect_err("open fixed part rejected");
        assert_eq!(error.code(), "invalid_assembly_part");
        assert!(error.to_string().contains("fixed"));
        assert!(error.to_string().contains("open_mesh"));

        let error = analyze_assembly(
            &cube(1.0, [0.0, 0.0, 0.0]),
            &cube(1.0, [2.0, 0.0, 0.0]),
            AssemblyOptions {
                motion: Some(LinearMotion {
                    direction: [0.0; 3],
                    travel_mm: 1.0,
                    target_clearance_mm: None,
                }),
                ..AssemblyOptions::default()
            },
        )
        .expect_err("zero direction rejected");
        assert_eq!(error.code(), "validation");
        assert!(error.to_string().contains("motion.direction"));
    }
}
