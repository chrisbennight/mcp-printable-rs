//! Collision checks on planned deposition and translated machine envelopes.
//!
//! Coordinates are millimetres in the final G-code frame. Line envelopes are
//! conservative rectangular prisms, including an end margin of half a line
//! width. They describe planned occupancy, not a measured physical print.

use std::collections::{BTreeMap, BTreeSet};

use parry3d_f64::{bounding_volume::Aabb, math::Vector, partitioning::Bvh};
use serde::{Deserialize, Serialize};

use crate::GeometryError;

const CONTACT_TOLERANCE_MM: f64 = 1e-6;
pub const MAX_COLLISION_FINDINGS: usize = 512;
/// Shared with G-code review; keeps orientation predicates and swept bounds in
/// the supported numeric coordinate range.
pub const MAX_PRINT_COORDINATE_MM: f64 = 1e7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRole {
    Part,
    Support,
    Brim,
    PrimeTower,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedMove {
    pub from_mm: [f64; 3],
    pub to_mm: [f64; 3],
    pub source_line: u64,
    pub layer: u32,
    pub object: Option<String>,
    pub role: PathRole,
    pub material: usize,
    pub travel: bool,
    pub width_mm: Option<f64>,
    pub height_mm: Option<f64>,
}

/// A machine component's occupied box relative to the nozzle reference point.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClearanceBox {
    pub component: String,
    pub minimum_mm: [f64; 3],
    pub maximum_mm: [f64; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlateGeometry {
    pub boundary_mm: Vec<[f64; 2]>,
    pub exclusions_mm: Vec<Vec<[f64; 2]>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Passed,
    Failed,
    Unmeasured,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionKind {
    OutsideBed,
    BedExclusion,
    TowerInterference,
    ToolheadInterference,
}

#[derive(Clone, Debug, Serialize)]
pub struct CollisionFinding {
    pub kind: CollisionKind,
    pub source_line: u64,
    pub layer: u32,
    pub object: Option<String>,
    pub role: PathRole,
    pub material: usize,
    pub position_mm: [f64; 3],
    pub other_source_line: Option<u64>,
    pub other_object: Option<String>,
    pub other_role: Option<PathRole>,
    pub component: Option<String>,
    pub evidence: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CollisionReport {
    pub bed: CheckState,
    pub tower_interference: CheckState,
    pub toolhead: CheckState,
    pub complete: bool,
    pub findings_truncated: bool,
    pub missing_deposition_dimensions: bool,
    /// One finding per kind/object pair/component. Unlabeled moves retain their
    /// source-line identity so different obstacles are not silently combined.
    pub findings: Vec<CollisionFinding>,
}

#[derive(Debug, Serialize)]
pub struct DepositedFootprint {
    pub object: Option<String>,
    pub role: PathRole,
    pub material: usize,
    pub minimum_mm: [f64; 3],
    pub maximum_mm: [f64; 3],
    pub paths: usize,
    pub first_source_line: u64,
    pub last_source_line: u64,
}

/// Bounding footprints include planned line width, height and end margins.
/// They conservatively include empty space and never establish a collision.
pub fn deposited_footprints(
    paths: &[PlannedMove],
    mut continue_work: impl FnMut() -> bool,
) -> Result<(Vec<DepositedFootprint>, bool), GeometryError> {
    if !validate(paths, None, &[], &mut continue_work)? {
        return Ok((Vec::new(), false));
    }
    let mut groups = BTreeMap::<(Option<String>, PathRole, usize), DepositedFootprint>::new();
    let mut complete = true;
    for path in paths {
        if !continue_work() {
            complete = false;
            break;
        }
        if path.travel {
            continue;
        }
        let Some(prism) = Prism::deposition(path) else {
            complete = false;
            continue;
        };
        let key = (path.object.clone(), path.role, path.material);
        if groups.len() >= 1024 && !groups.contains_key(&key) {
            complete = false;
            continue;
        }
        let bounds = prism.bounds();
        let value = groups.entry(key).or_insert_with(|| DepositedFootprint {
            object: path.object.clone(),
            role: path.role,
            material: path.material,
            minimum_mm: [f64::INFINITY; 3],
            maximum_mm: [f64::NEG_INFINITY; 3],
            paths: 0,
            first_source_line: path.source_line,
            last_source_line: path.source_line,
        });
        for axis in 0..3 {
            value.minimum_mm[axis] = value.minimum_mm[axis].min(bounds.mins[axis]);
            value.maximum_mm[axis] = value.maximum_mm[axis].max(bounds.maxs[axis]);
        }
        value.paths += 1;
        value.last_source_line = path.source_line;
    }
    Ok((groups.into_values().collect(), complete))
}

/// Distance between conservative XY bounds is a lower bound on separation.
/// Zero means the bounds overlap or touch; it does not establish contact.
pub fn footprint_gap_lower_bound(a: &DepositedFootprint, b: &DepositedFootprint) -> f64 {
    let gap = |axis: usize| {
        (a.minimum_mm[axis] - b.maximum_mm[axis])
            .max(b.minimum_mm[axis] - a.maximum_mm[axis])
            .max(0.)
    };
    gap(0).hypot(gap(1))
}

struct Prism {
    center: [f64; 3],
    axes: [[f64; 3]; 3],
    half: [f64; 3],
}

impl Prism {
    fn deposition(path: &PlannedMove) -> Option<Self> {
        if path.travel || path.role == PathRole::Other {
            return None;
        }
        let (width, height) = (path.width_mm?, path.height_mm?);
        if (path.from_mm[2] - path.to_mm[2]).abs() > CONTACT_TOLERANCE_MM {
            return None;
        }
        let dx = path.to_mm[0] - path.from_mm[0];
        let dy = path.to_mm[1] - path.from_mm[1];
        let length = dx.hypot(dy);
        if length <= CONTACT_TOLERANCE_MM {
            return None;
        }
        Some(Self {
            center: [
                (path.from_mm[0] + path.to_mm[0]) / 2.,
                (path.from_mm[1] + path.to_mm[1]) / 2.,
                path.to_mm[2] - height / 2.,
            ],
            axes: [
                [dx / length, dy / length, 0.],
                [-dy / length, dx / length, 0.],
                [0., 0., 1.],
            ],
            half: [(length + width) / 2., width / 2., height / 2.],
        })
    }

    fn machine(envelope: &ClearanceBox, nozzle: [f64; 3]) -> Self {
        Self {
            center: std::array::from_fn(|i| {
                nozzle[i] + (envelope.minimum_mm[i] + envelope.maximum_mm[i]) / 2.
            }),
            axes: [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]],
            half: std::array::from_fn(|i| (envelope.maximum_mm[i] - envelope.minimum_mm[i]) / 2.),
        }
    }

    fn radius(&self, axis: [f64; 3]) -> f64 {
        (0..3)
            .map(|i| self.half[i] * dot(self.axes[i], axis).abs())
            .sum()
    }

    fn bounds(&self) -> Aabb {
        let radius: [f64; 3] = std::array::from_fn(|i| {
            self.radius(std::array::from_fn(|j| if i == j { 1. } else { 0. }))
        });
        Aabb::new(
            Vector::from_array(std::array::from_fn(|i| self.center[i] - radius[i])),
            Vector::from_array(std::array::from_fn(|i| self.center[i] + radius[i])),
        )
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}

/// Continuous separating-axis test for translating, upright rectangular prisms.
/// Their axes remain fixed, so horizontal edge normals and Z are complete.
fn first_interference(moving: &Prism, velocity: [f64; 3], fixed: &Prism) -> Option<f64> {
    let mut interval = [0_f64, 1_f64];
    let difference = std::array::from_fn(|i| moving.center[i] - fixed.center[i]);
    for axis in moving.axes.into_iter().chain(fixed.axes) {
        let radius = moving.radius(axis) + fixed.radius(axis) - CONTACT_TOLERANCE_MM;
        let position = dot(difference, axis);
        let speed = dot(velocity, axis);
        if speed.abs() < f64::EPSILON {
            if position.abs() >= radius {
                return None;
            }
        } else {
            let a = (-radius - position) / speed;
            let b = (radius - position) / speed;
            interval[0] = interval[0].max(a.min(b));
            interval[1] = interval[1].min(a.max(b));
            if interval[0] >= interval[1] {
                return None;
            }
        }
    }
    Some(interval[0])
}

fn validate(
    paths: &[PlannedMove],
    plate: Option<&PlateGeometry>,
    boxes: &[ClearanceBox],
    continue_work: &mut impl FnMut() -> bool,
) -> Result<bool, GeometryError> {
    if !continue_work() {
        return Ok(false);
    }
    let invalid = |message: &str| GeometryError::InvalidOptions(message.into());
    if paths.len() > u32::MAX as usize {
        return Err(invalid(
            "toolpath exceeds the spatial index's integer range",
        ));
    }
    for path in paths {
        if !continue_work() {
            return Ok(false);
        }
        if path
            .from_mm
            .iter()
            .chain(&path.to_mm)
            .any(|v| !v.is_finite() || v.abs() > MAX_PRINT_COORDINATE_MM)
            || path
                .object
                .as_ref()
                .is_some_and(|object| object.len() > 256)
            || [path.width_mm, path.height_mm]
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || v <= 0. || v > MAX_PRINT_COORDINATE_MM)
        {
            return Err(invalid(
                "toolpath coordinates must be finite and deposition dimensions positive",
            ));
        }
    }
    for envelope in boxes {
        if !continue_work() {
            return Ok(false);
        }
        if envelope.component.is_empty()
            || envelope.component.len() > 256
            || (0..3).any(|i| {
                !envelope.minimum_mm[i].is_finite()
                    || !envelope.maximum_mm[i].is_finite()
                    || envelope.minimum_mm[i].abs() > MAX_PRINT_COORDINATE_MM
                    || envelope.maximum_mm[i].abs() > MAX_PRINT_COORDINATE_MM
                    || envelope.minimum_mm[i] >= envelope.maximum_mm[i]
            })
        {
            return Err(invalid(
                "machine envelopes require a component name and finite ordered bounds",
            ));
        }
    }
    if let Some(plate) = plate {
        for polygon in std::iter::once(&plate.boundary_mm).chain(&plate.exclusions_mm) {
            if !continue_work() {
                return Ok(false);
            }
            if polygon.len() < 3 {
                return Err(invalid(
                    "bed polygons require at least three finite vertices",
                ));
            }
            for value in polygon.iter().flatten() {
                if !continue_work() {
                    return Ok(false);
                }
                if !value.is_finite() || value.abs() > MAX_PRINT_COORDINATE_MM {
                    return Err(invalid(
                        "bed polygon coordinates exceed the supported numeric range",
                    ));
                }
            }
        }
    }
    Ok(true)
}

fn interrupted_report() -> CollisionReport {
    CollisionReport {
        bed: CheckState::Unmeasured,
        tower_interference: CheckState::Unmeasured,
        toolhead: CheckState::Unmeasured,
        complete: false,
        findings_truncated: false,
        missing_deposition_dimensions: false,
        findings: Vec::new(),
    }
}

/// Analyze explicit machine motion against material deposited by that time.
/// The caller supplies a cancellation/deadline predicate. An interrupted check
/// retains established findings and marks unfinished checks unmeasured.
pub fn check_print_paths(
    paths: &[PlannedMove],
    plate: Option<&PlateGeometry>,
    boxes: &[ClearanceBox],
    mut continue_work: impl FnMut() -> bool,
) -> Result<CollisionReport, GeometryError> {
    if !validate(paths, plate, boxes, &mut continue_work)? {
        return Ok(interrupted_report());
    }
    let mut deposits: Vec<Option<Prism>> = Vec::new();
    let mut tree = Bvh::new();
    let mut report = CollisionReport {
        bed: if plate.is_some() {
            CheckState::Passed
        } else {
            CheckState::Unmeasured
        },
        tower_interference: CheckState::Passed,
        toolhead: if boxes.is_empty() {
            CheckState::Unmeasured
        } else {
            CheckState::Passed
        },
        complete: true,
        findings_truncated: false,
        missing_deposition_dimensions: false,
        findings: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    'paths: for (index, path) in paths.iter().enumerate() {
        if report.findings_truncated || !continue_work() {
            report.complete = false;
            break;
        }
        let current = Prism::deposition(path);
        if !path.travel && path.role != PathRole::Other && current.is_none() {
            report.missing_deposition_dimensions = true;
        }
        if let (Some(prism), Some(plate)) = (&current, plate) {
            let corners = footprint(prism);
            if let Some(point) = outside_witness(&plate.boundary_mm, &corners) {
                add_finding(
                    &mut report,
                    &mut seen,
                    path,
                    None,
                    None,
                    CollisionKind::OutsideBed,
                    [point[0], point[1], path.to_mm[2]],
                );
            }
            if let Some(point) = plate
                .exclusions_mm
                .iter()
                .find_map(|polygon| overlap_witness(polygon, &corners))
            {
                add_finding(
                    &mut report,
                    &mut seen,
                    path,
                    None,
                    None,
                    CollisionKind::BedExclusion,
                    [point[0], point[1], path.to_mm[2]],
                );
            }
        }
        if let Some(current) = &current {
            for candidate in tree.intersect_aabb(&current.bounds()) {
                if report.findings_truncated || !continue_work() {
                    report.complete = false;
                    break 'paths;
                }
                let other_index = candidate as usize;
                let other = &paths[other_index];
                let tower_pair = (path.role == PathRole::PrimeTower
                    && matches!(other.role, PathRole::Part | PathRole::Support))
                    || (other.role == PathRole::PrimeTower
                        && matches!(path.role, PathRole::Part | PathRole::Support));
                if tower_pair
                    && first_interference(
                        current,
                        [0.; 3],
                        deposits[other_index].as_ref().expect("indexed deposition"),
                    )
                    .is_some()
                {
                    let fixed = deposits[other_index].as_ref().expect("indexed deposition");
                    let fixed_footprint = footprint(fixed);
                    let current_footprint = footprint(current);
                    let Some(point) = overlap_witness(&fixed_footprint, &current_footprint)
                        .or_else(|| {
                            current_footprint
                                .into_iter()
                                .find(|p| inside(&fixed_footprint, *p, true))
                        })
                    else {
                        report.complete = false;
                        break 'paths;
                    };
                    let bottom =
                        (current.center[2] - current.half[2]).max(fixed.center[2] - fixed.half[2]);
                    let top =
                        (current.center[2] + current.half[2]).min(fixed.center[2] + fixed.half[2]);
                    add_finding(
                        &mut report,
                        &mut seen,
                        path,
                        Some(other),
                        None,
                        CollisionKind::TowerInterference,
                        [point[0], point[1], (bottom + top) / 2.],
                    );
                }
            }
        }
        for envelope in boxes {
            if report.findings_truncated || !continue_work() {
                report.complete = false;
                break 'paths;
            }
            let machine = Prism::machine(envelope, path.from_mm);
            let end = Prism::machine(envelope, path.to_mm);
            let first = machine.bounds();
            let last = end.bounds();
            let swept = Aabb::new(first.mins.min(last.mins), first.maxs.max(last.maxs));
            let velocity = std::array::from_fn(|i| path.to_mm[i] - path.from_mm[i]);
            // In the translating nozzle frame, a straight extrusion's deposited
            // prefix grows monotonically backwards. Its final occupancy therefore
            // detects every self-interference during this move without treating
            // future material as an obstacle earlier in the sweep.
            if current
                .as_ref()
                .is_some_and(|current| first_interference(&end, [0.; 3], current).is_some())
            {
                add_finding(
                    &mut report,
                    &mut seen,
                    path,
                    Some(path),
                    Some(&envelope.component),
                    CollisionKind::ToolheadInterference,
                    path.to_mm,
                );
            }
            for candidate in tree.intersect_aabb(&swept) {
                if report.findings_truncated || !continue_work() {
                    report.complete = false;
                    break 'paths;
                }
                let other_index = candidate as usize;
                if let Some(fraction) = first_interference(
                    &machine,
                    velocity,
                    deposits[other_index].as_ref().expect("indexed deposition"),
                ) {
                    let position =
                        std::array::from_fn(|i| path.from_mm[i] + velocity[i] * fraction);
                    add_finding(
                        &mut report,
                        &mut seen,
                        path,
                        Some(&paths[other_index]),
                        Some(&envelope.component),
                        CollisionKind::ToolheadInterference,
                        position,
                    );
                }
            }
        }
        if report.findings_truncated || !continue_work() {
            report.complete = false;
            break;
        }
        if let Some(prism) = &current {
            tree.insert(prism.bounds(), index as u32);
        }
        deposits.push(current);
        if report.findings_truncated || !continue_work() {
            report.complete = false;
            break;
        }
    }
    let incomplete = !report.complete || report.missing_deposition_dimensions;
    report.bed = state(
        report.bed,
        incomplete,
        report.findings.iter().any(|f| {
            matches!(
                f.kind,
                CollisionKind::OutsideBed | CollisionKind::BedExclusion
            )
        }),
    );
    report.tower_interference = state(
        report.tower_interference,
        incomplete,
        report
            .findings
            .iter()
            .any(|f| f.kind == CollisionKind::TowerInterference),
    );
    report.toolhead = state(
        report.toolhead,
        incomplete,
        report
            .findings
            .iter()
            .any(|f| f.kind == CollisionKind::ToolheadInterference),
    );
    Ok(report)
}

fn state(previous: CheckState, incomplete: bool, failed: bool) -> CheckState {
    if failed {
        CheckState::Failed
    } else if incomplete {
        CheckState::Unmeasured
    } else {
        previous
    }
}

type FindingKey = (
    CollisionKind,
    Option<String>,
    Option<u64>,
    PathRole,
    Option<String>,
    Option<u64>,
    Option<PathRole>,
    Option<String>,
);

fn add_finding(
    report: &mut CollisionReport,
    seen: &mut BTreeSet<FindingKey>,
    path: &PlannedMove,
    other: Option<&PlannedMove>,
    component: Option<&str>,
    kind: CollisionKind,
    position: [f64; 3],
) {
    if report.findings.len() >= MAX_COLLISION_FINDINGS {
        report.findings_truncated = true;
        report.complete = false;
        return;
    }
    let other_object = other.and_then(|p| p.object.clone());
    let key = (
        kind,
        path.object.clone(),
        path.object.is_none().then_some(path.source_line),
        path.role,
        other_object.clone(),
        other
            .filter(|path| path.object.is_none())
            .map(|path| path.source_line),
        other.map(|p| p.role),
        component.map(str::to_owned),
    );
    if !seen.insert(key) {
        return;
    }
    report.findings.push(CollisionFinding {
        kind,
        source_line: path.source_line,
        layer: path.layer,
        object: path.object.clone(),
        role: path.role,
        material: path.material,
        position_mm: position,
        other_source_line: other.map(|p| p.source_line),
        other_object,
        other_role: other.map(|p| p.role),
        component: component.map(str::to_owned),
        evidence: "planned_rectangular_line_envelopes",
    });
}

fn footprint(prism: &Prism) -> [[f64; 2]; 4] {
    [[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]].map(|sign| {
        std::array::from_fn(|i| {
            prism.center[i]
                + sign[0] * prism.half[0] * prism.axes[0][i]
                + sign[1] * prism.half[1] * prism.axes[1][i]
        })
    })
}

fn side(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> f64 {
    robust::orient2d(
        robust::Coord { x: a[0], y: a[1] },
        robust::Coord { x: b[0], y: b[1] },
        robust::Coord { x: p[0], y: p[1] },
    )
}

fn edges(polygon: &[[f64; 2]]) -> impl Iterator<Item = ([f64; 2], [f64; 2])> + '_ {
    polygon
        .iter()
        .copied()
        .zip(polygon.iter().copied().cycle().skip(1))
        .take(polygon.len())
}

fn inside(polygon: &[[f64; 2]], point: [f64; 2], boundary: bool) -> bool {
    let mut winding = 0_i64;
    for (a, b) in edges(polygon) {
        let orientation = side(a, b, point);
        if orientation == 0.
            && (0..2).all(|i| point[i] >= a[i].min(b[i]) && point[i] <= a[i].max(b[i]))
        {
            return boundary;
        }
        if a[1] <= point[1] && point[1] < b[1] && orientation > 0. {
            winding += 1;
        }
        if b[1] <= point[1] && point[1] < a[1] && orientation < 0. {
            winding -= 1;
        }
    }
    winding != 0
}

fn crosses(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let first = side(a, b, c);
    let second = side(a, b, d);
    let third = side(c, d, a);
    let fourth = side(c, d, b);
    ((first < 0. && second > 0.) || (first > 0. && second < 0.))
        && ((third < 0. && fourth > 0.) || (third > 0. && fourth < 0.))
}

fn crossing_witness(polygon: &[[f64; 2]], rectangle: &[[f64; 2]; 4]) -> Option<[f64; 2]> {
    for (a, b) in edges(rectangle) {
        for (c, d) in edges(polygon) {
            if crosses(a, b, c, d) {
                let start = side(c, d, a);
                let end = side(c, d, b);
                let fraction = start / (start - end);
                return Some(std::array::from_fn(|i| a[i] + fraction * (b[i] - a[i])));
            }
        }
    }
    None
}

fn outside_witness(polygon: &[[f64; 2]], rectangle: &[[f64; 2]; 4]) -> Option<[f64; 2]> {
    rectangle
        .iter()
        .copied()
        .find(|p| !inside(polygon, *p, true))
        .or_else(|| crossing_witness(polygon, rectangle))
        .or_else(|| {
            edges(rectangle)
                .map(|(a, b)| [(a[0] + b[0]) / 2., (a[1] + b[1]) / 2.])
                .find(|p| !inside(polygon, *p, true))
        })
        .or_else(|| {
            edges(polygon)
                .map(|(a, b)| [(a[0] + b[0]) / 2., (a[1] + b[1]) / 2.])
                .find(|p| inside(rectangle, *p, false))
        })
        .or_else(|| {
            polygon
                .iter()
                .copied()
                .find(|p| inside(rectangle, *p, false))
        })
}

fn overlap_witness(polygon: &[[f64; 2]], rectangle: &[[f64; 2]; 4]) -> Option<[f64; 2]> {
    rectangle
        .iter()
        .copied()
        .find(|p| inside(polygon, *p, false))
        .or_else(|| {
            polygon
                .iter()
                .copied()
                .find(|p| inside(rectangle, *p, false))
        })
        .or_else(|| crossing_witness(polygon, rectangle))
        .or_else(|| {
            edges(rectangle)
                .map(|(a, b)| [(a[0] + b[0]) / 2., (a[1] + b[1]) / 2.])
                .find(|p| inside(polygon, *p, false))
        })
        .or_else(|| {
            edges(polygon)
                .map(|(a, b)| [(a[0] + b[0]) / 2., (a[1] + b[1]) / 2.])
                .find(|p| inside(rectangle, *p, false))
        })
        .or_else(|| {
            let center = rectangle.iter().fold([0.; 2], |mut sum, p| {
                for i in 0..2 {
                    sum[i] += p[i] / 4.;
                }
                sum
            });
            inside(polygon, center, false).then_some(center)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(from: [f64; 3], to: [f64; 3], role: PathRole, index: u64) -> PlannedMove {
        PlannedMove {
            from_mm: from,
            to_mm: to,
            source_line: index,
            layer: 1,
            object: None,
            role,
            material: 0,
            travel: false,
            width_mm: Some(0.4),
            height_mm: Some(0.2),
        }
    }
    fn head() -> ClearanceBox {
        ClearanceBox {
            component: "shroud".into(),
            minimum_mm: [-1., -1., 1.],
            maximum_mm: [1., 1., 4.],
        }
    }

    #[test]
    fn dense_unlabeled_collisions_keep_bounded_positive_evidence() {
        let mut paths = Vec::new();
        for index in 0..2000 {
            paths.push(line([0., 0., 0.2], [10., 0., 0.2], PathRole::Part, index));
        }
        let envelope = ClearanceBox {
            component: "synthetic obstruction".into(),
            minimum_mm: [-20., -20., -1.],
            maximum_mm: [20., 20., 1.],
        };
        let report = check_print_paths(&paths, None, &[envelope], || true).unwrap();
        assert_eq!(report.findings.len(), MAX_COLLISION_FINDINGS);
        assert!(report.findings_truncated);
        assert!(!report.complete);
        assert_eq!(report.toolhead, CheckState::Failed);
        assert_eq!(report.bed, CheckState::Unmeasured);
        assert_eq!(report.findings[0].source_line, 0);
    }

    #[test]
    fn footprints_include_generated_width_height_and_margins_without_claiming_contact() {
        let paths = [
            line([10., 10., 0.2], [20., 10., 0.2], PathRole::Part, 1),
            line([10., 13., 0.2], [20., 13., 0.2], PathRole::Brim, 2),
        ];
        let (footprints, complete) = deposited_footprints(&paths, || true).unwrap();
        assert!(complete);
        assert_eq!(footprints.len(), 2);
        assert_eq!(footprints[0].minimum_mm, [9.8, 9.8, 0.]);
        assert_eq!(footprints[0].maximum_mm, [20.2, 10.2, 0.2]);
        assert!((footprint_gap_lower_bound(&footprints[0], &footprints[1]) - 2.6).abs() < 1e-9);
        assert!(!deposited_footprints(&paths, || false).unwrap().1);
        let mut invalid = paths;
        invalid[0].width_mm = Some(f64::NAN);
        assert!(deposited_footprints(&invalid, || true).is_err());
    }

    #[test]
    fn tower_crossing_part_is_flagged_but_intended_contacts_are_not() {
        let mut paths = vec![
            line([10., 10., 0.2], [20., 10., 0.2], PathRole::Part, 10),
            line([15., 5., 0.2], [15., 15., 0.2], PathRole::PrimeTower, 20),
        ];
        let report = check_print_paths(&paths, None, &[], || true).unwrap();
        assert_eq!(report.tower_interference, CheckState::Failed);
        assert_eq!(report.findings[0].other_source_line, Some(10));
        for role in [PathRole::Part, PathRole::Support, PathRole::Brim] {
            paths[1].role = role;
            assert!(
                check_print_paths(&paths, None, &[], || true)
                    .unwrap()
                    .findings
                    .is_empty()
            );
        }
    }

    #[test]
    fn head_interference_with_current_extrusion_does_not_depend_on_segmentation() {
        for (end, middle, min, max) in [
            (
                [20., 10., 0.2],
                [15., 10., 0.2],
                [-2., -0.1, -0.1],
                [-1., 0.1, 1.],
            ),
            (
                [0., 10., 0.2],
                [5., 10., 0.2],
                [1., -0.1, -0.1],
                [2., 0.1, 1.],
            ),
            (
                [20., 20., 0.2],
                [15., 15., 0.2],
                [-2., -2., -0.1],
                [-1., -1., 1.],
            ),
        ] {
            let start = [10., 10., 0.2];
            let envelope = ClearanceBox {
                component: "trailing component".into(),
                minimum_mm: min,
                maximum_mm: max,
            };
            for paths in [
                vec![line(start, end, PathRole::Part, 1)],
                vec![
                    line(start, middle, PathRole::Part, 1),
                    line(middle, end, PathRole::Part, 2),
                ],
            ] {
                let report =
                    check_print_paths(&paths, None, std::slice::from_ref(&envelope), || true)
                        .unwrap();
                assert_eq!(report.toolhead, CheckState::Failed);
                assert!(
                    report
                        .findings
                        .iter()
                        .any(|finding| finding.kind == CollisionKind::ToolheadInterference)
                );
            }
        }
    }

    #[test]
    fn current_extrusion_does_not_collide_with_ahead_or_touching_components() {
        let paths = [line([10., 10., 0.2], [20., 10., 0.2], PathRole::Part, 1)];
        for (min, max) in [
            ([1., -0.1, -0.1], [2., 0.1, 1.]),
            ([-2., -0.1, 0.], [-1., 0.1, 1.]),
        ] {
            let envelope = ClearanceBox {
                component: "component".into(),
                minimum_mm: min,
                maximum_mm: max,
            };
            let report = check_print_paths(&paths, None, &[envelope], || true).unwrap();
            assert_eq!(report.toolhead, CheckState::Passed);
            assert!(report.findings.is_empty());
        }
    }

    #[test]
    fn one_move_reports_each_intersected_object_pair() {
        let mut first = line([10., 0., 0.2], [10., 5., 0.2], PathRole::Part, 1);
        first.object = Some("part-a".into());
        let mut second = line([20., 0., 0.2], [20., 5., 0.2], PathRole::Part, 2);
        second.object = Some("part-b".into());
        let mut moving = line([0., 2., 0.1], [30., 2., 0.1], PathRole::Part, 3);
        moving.object = Some("moving".into());
        moving.travel = true;
        let envelope = ClearanceBox {
            component: "shroud".into(),
            minimum_mm: [-0.1, -0.1, 0.05],
            maximum_mm: [0.1, 0.1, 0.2],
        };
        let report =
            check_print_paths(&[first, second, moving], None, &[envelope], || true).unwrap();
        let affected: BTreeSet<_> = report
            .findings
            .iter()
            .filter(|finding| finding.source_line == 3)
            .map(|finding| (finding.other_object.as_deref(), finding.other_source_line))
            .collect();
        assert_eq!(
            affected,
            BTreeSet::from([(Some("part-a"), Some(1)), (Some("part-b"), Some(2))])
        );
    }

    #[test]
    fn unlabeled_obstacles_retain_distinct_source_paths_and_locations() {
        let first = line([10., 0., 0.2], [10., 5., 0.2], PathRole::Part, 1);
        let second = line([20., 0., 0.2], [20., 5., 0.2], PathRole::Part, 2);
        let mut moving = line([0., 2., 0.1], [30., 2., 0.1], PathRole::Part, 3);
        moving.travel = true;
        let envelope = ClearanceBox {
            component: "shroud".into(),
            minimum_mm: [-0.1, -0.1, 0.05],
            maximum_mm: [0.1, 0.1, 0.2],
        };
        let report =
            check_print_paths(&[first, second, moving], None, &[envelope], || true).unwrap();
        let affected: Vec<_> = report
            .findings
            .iter()
            .filter(|finding| finding.source_line == 3)
            .collect();
        assert_eq!(affected.len(), 2);
        assert_eq!(
            affected
                .iter()
                .map(|finding| finding.other_source_line)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([Some(1), Some(2)])
        );
        assert!(
            affected
                .iter()
                .all(|finding| finding.object.is_none() && finding.other_object.is_none())
        );
        assert_ne!(affected[0].position_mm, affected[1].position_mm);
    }

    #[test]
    fn shared_xy_bounds_without_intersection_do_not_produce_a_collision() {
        let paths = vec![
            line([0., 0., 1.], [10., 10., 1.], PathRole::Part, 1),
            line([0., 2., 1.], [8., 10., 1.], PathRole::PrimeTower, 2),
        ];
        assert!(
            check_print_paths(&paths, None, &[], || true)
                .unwrap()
                .findings
                .is_empty()
        );
    }

    #[test]
    fn aligned_overlapping_lines_have_an_intersection_witness() {
        let paths = vec![
            line([10., 10., 1.], [22., 10., 1.], PathRole::Part, 1),
            line([20., 10., 1.], [30., 10., 1.], PathRole::PrimeTower, 2),
        ];
        let report = check_print_paths(&paths, None, &[], || true).unwrap();
        assert_eq!(report.tower_interference, CheckState::Failed);
        assert!(report.complete);
        assert!((19.8..=22.2).contains(&report.findings[0].position_mm[0]));
    }

    #[test]
    fn a_clear_endpoint_does_not_hide_an_intermediate_toolhead_collision() {
        let mut travel = line([0., 5., 0.2], [20., 5., 0.2], PathRole::PrimeTower, 2);
        travel.travel = true;
        let paths = vec![
            line([10., 0., 2.], [10., 10., 2.], PathRole::Part, 1),
            travel,
        ];
        let report = check_print_paths(&paths, None, &[head()], || true).unwrap();
        assert_eq!(report.toolhead, CheckState::Failed);
        let hit = report
            .findings
            .iter()
            .find(|f| f.kind == CollisionKind::ToolheadInterference)
            .unwrap();
        assert!(hit.position_mm[0] > 8. && hit.position_mm[0] < 10.);
    }

    #[test]
    fn sparse_tower_descent_and_print_order_use_previously_printed_geometry() {
        let mut descent = line([10., 5., 20.], [10., 5., 0.2], PathRole::PrimeTower, 2);
        descent.travel = true;
        let obstacle = line([10., 0., 5.], [10., 10., 5.], PathRole::Part, 1);
        let blocked = check_print_paths(
            &[obstacle.clone(), descent.clone()],
            None,
            &[head()],
            || true,
        )
        .unwrap();
        assert_eq!(blocked.toolhead, CheckState::Failed);
        let clear = check_print_paths(&[descent, obstacle], None, &[head()], || true).unwrap();
        assert_eq!(clear.toolhead, CheckState::Passed);
    }

    #[test]
    fn layer_contact_and_a_high_enough_hop_do_not_claim_interference() {
        let previous = line([10., 0., 0.2], [10., 10., 0.2], PathRole::Part, 1);
        let next = line([0., 5., 0.2], [20., 5., 0.2], PathRole::Part, 2);
        assert_eq!(
            check_print_paths(&[previous, next], None, &[head()], || true)
                .unwrap()
                .toolhead,
            CheckState::Passed
        );
    }

    #[test]
    fn generated_width_is_checked_at_bed_edges_and_exclusions() {
        let plate = PlateGeometry {
            boundary_mm: vec![[0., 0.], [20., 0.], [20., 20.], [0., 20.]],
            exclusions_mm: vec![vec![[8., 0.], [12., 0.], [12., 4.], [8., 4.]]],
        };
        let edge = line([0.1, 8., 0.2], [0.1, 12., 0.2], PathRole::Brim, 1);
        let exclusion = line([5., 2., 0.2], [15., 2., 0.2], PathRole::PrimeTower, 2);
        let report = check_print_paths(&[edge, exclusion], Some(&plate), &[], || true).unwrap();
        assert_eq!(report.bed, CheckState::Failed);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == CollisionKind::OutsideBed)
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == CollisionKind::BedExclusion)
        );
        let excluded = report
            .findings
            .iter()
            .find(|f| f.kind == CollisionKind::BedExclusion)
            .unwrap();
        assert!((8. ..=12.).contains(&excluded.position_mm[0]));
    }

    #[test]
    fn missing_machine_geometry_is_unmeasured_and_unknown_width_prevents_a_pass() {
        let mut path = line([1., 1., 1.], [2., 1., 1.], PathRole::Part, 1);
        path.width_mm = None;
        let report = check_print_paths(&[path], None, &[], || true).unwrap();
        assert_eq!(report.toolhead, CheckState::Unmeasured);
        assert_eq!(report.tower_interference, CheckState::Unmeasured);
        assert!(report.missing_deposition_dimensions);
    }

    #[test]
    fn an_interrupted_analysis_retains_confirmed_findings() {
        let paths = vec![
            line([10., 10., 1.], [20., 10., 1.], PathRole::Part, 1),
            line([15., 5., 1.], [15., 15., 1.], PathRole::PrimeTower, 2),
            line([30., 30., 1.], [40., 30., 1.], PathRole::Part, 3),
        ];
        let mut full_calls = 0;
        assert!(
            check_print_paths(&paths, None, &[], || {
                full_calls += 1;
                true
            })
            .unwrap()
            .complete
        );
        let mut established = false;
        for budget in 0..full_calls {
            let mut calls = 0;
            let report = check_print_paths(&paths, None, &[], || {
                calls += 1;
                calls <= budget
            })
            .unwrap();
            assert!(!report.complete);
            if !report.findings.is_empty() {
                established = true;
            }
            if established {
                assert_eq!(report.tower_interference, CheckState::Failed);
                assert!(!report.findings.is_empty());
            }
        }
        assert!(established);
    }

    #[test]
    fn an_exhausted_budget_stops_before_validating_or_indexing_the_prefix() {
        let paths = [line([f64::NAN, 0., 1.], [0., 10., 1.], PathRole::Part, 1)];
        let report = check_print_paths(&paths, None, &[], || false).unwrap();
        assert!(!report.complete);
        assert_eq!(report.bed, CheckState::Unmeasured);
        assert_eq!(report.tower_interference, CheckState::Unmeasured);
        assert_eq!(report.toolhead, CheckState::Unmeasured);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn finite_values_that_exceed_the_numeric_frame_are_rejected() {
        let paths = [line([1e308, 0., 1.], [1e308, 10., 1.], PathRole::Part, 1)];
        assert!(check_print_paths(&paths, None, &[], || true).is_err());
        let mut envelope = head();
        envelope.maximum_mm[2] = 1e308;
        assert!(check_print_paths(&[], None, &[envelope], || true).is_err());
    }

    #[test]
    fn clear_paths_pass_and_crossing_a_concave_bed_notch_is_flagged() {
        let plate = PlateGeometry {
            boundary_mm: vec![
                [0., 0.],
                [20., 0.],
                [20., 5.],
                [5., 5.],
                [5., 20.],
                [0., 20.],
            ],
            exclusions_mm: Vec::new(),
        };
        let clear = line([1., 10., 0.2], [3., 10., 0.2], PathRole::Part, 1);
        let report = check_print_paths(&[clear], Some(&plate), &[], || true).unwrap();
        assert_eq!(report.bed, CheckState::Passed);
        assert!(report.findings.is_empty());
        let crossing = line([2., 10., 0.2], [10., 2., 0.2], PathRole::Part, 1);
        let report = check_print_paths(&[crossing], Some(&plate), &[], || true).unwrap();
        assert_eq!(report.bed, CheckState::Failed);
        assert_eq!(report.findings[0].kind, CollisionKind::OutsideBed);
    }

    #[test]
    fn aligned_exclusion_overlap_is_reported_but_boundary_contact_is_allowed() {
        let plate = PlateGeometry {
            boundary_mm: vec![[0., 0.], [40., 0.], [40., 40.], [0., 40.]],
            exclusions_mm: vec![vec![[10., 9.8], [22., 9.8], [22., 10.2], [10., 10.2]]],
        };
        let overlapping = line([20., 10., 0.2], [30., 10., 0.2], PathRole::PrimeTower, 1);
        assert_eq!(
            check_print_paths(&[overlapping], Some(&plate), &[], || true)
                .unwrap()
                .bed,
            CheckState::Failed
        );
        let touching = line([20., 10.4, 0.2], [30., 10.4, 0.2], PathRole::PrimeTower, 1);
        assert_eq!(
            check_print_paths(&[touching], Some(&plate), &[], || true)
                .unwrap()
                .bed,
            CheckState::Passed
        );
    }
}
