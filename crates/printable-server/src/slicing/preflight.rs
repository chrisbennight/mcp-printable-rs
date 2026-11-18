//! Retained collision evidence from the native output, independent of slice exit status.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use printable_geom::print_collision::{
    self, CheckState, ClearanceBox, MAX_PRINT_COORDINATE_MM, PlateGeometry,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{invalid, review};
use crate::error::ToolError;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineEnvelope {
    pub component: String,
    /// Occupied bounds relative to the active nozzle's G-code reference point.
    pub minimum_mm: [f64; 3],
    pub maximum_mm: [f64; 3],
    /// Manufacturer data, measured geometry or retained artifact supplying the bounds.
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitialPosition {
    /// Known nozzle position in the input G-code frame before its first command.
    pub position_mm: [f64; 3],
    /// Evidence establishing that starting position; never infer it from zeroes.
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreflightOptions {
    pub initial_position: Option<InitialPosition>,
    /// Machine components, including the gantry where applicable. Empty is unmeasured.
    #[serde(default)]
    pub machine_envelopes: Vec<MachineEnvelope>,
    /// Processing budget for all selected plates after native slicing.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Inline state is a bounded index into the complete hash-bound artifact.
pub(super) fn compact(mut report: Value) -> Value {
    if let Some(plates) = report["plates"].as_object_mut() {
        let total = plates.len();
        let names = plates.keys().skip(32).cloned().collect::<Vec<_>>();
        for name in names {
            plates.remove(&name);
        }
        for plate in plates.values_mut() {
            for (object, key) in [
                ("", "findings"),
                ("deposited_footprints", "items"),
                ("deposited_footprints", "separations"),
            ] {
                let value = if object.is_empty() {
                    &mut *plate
                } else {
                    &mut plate[object]
                };
                if let Some(items) = value[key].as_array_mut() {
                    let count = items.len();
                    items.truncate(32);
                    value[format!("{key}_count")] = json!(count);
                    value[format!("{key}_inline_truncated")] = json!(count > 32);
                }
            }
        }
        report["plates_count"] = json!(total);
        report["plates_inline_truncated"] = json!(total > 32);
    }
    report
}

impl Default for PreflightOptions {
    fn default() -> Self {
        Self {
            initial_position: None,
            machine_envelopes: Vec::new(),
            timeout_seconds: default_timeout(),
        }
    }
}

impl PreflightOptions {
    pub fn validate(&self) -> Result<(), ToolError> {
        if self.timeout_seconds == 0
            || Instant::now()
                .checked_add(Duration::from_secs(self.timeout_seconds))
                .is_none()
        {
            return Err(invalid(
                "collision processing budget must be positive and runtime-representable",
            ));
        }
        if self.initial_position.as_ref().is_some_and(|position| {
            position.source.trim().is_empty()
                || position
                    .position_mm
                    .iter()
                    .any(|value| !value.is_finite() || value.abs() > MAX_PRINT_COORDINATE_MM)
        }) {
            return Err(invalid(
                "initial nozzle position requires a source and finite coordinates",
            ));
        }
        for envelope in &self.machine_envelopes {
            if envelope.component.trim().is_empty()
                || envelope.component.len() > 256
                || envelope.source.trim().is_empty()
                || (0..3).any(|i| {
                    !envelope.minimum_mm[i].is_finite()
                        || !envelope.maximum_mm[i].is_finite()
                        || envelope.minimum_mm[i].abs() > MAX_PRINT_COORDINATE_MM
                        || envelope.maximum_mm[i].abs() > MAX_PRINT_COORDINATE_MM
                        || envelope.minimum_mm[i] >= envelope.maximum_mm[i]
                })
            {
                return Err(invalid(
                    "machine envelopes require a component, source and finite ordered bounds",
                ));
            }
        }
        Ok(())
    }

    pub fn deadline(&self) -> Option<Instant> {
        Instant::now().checked_add(Duration::from_secs(self.timeout_seconds))
    }
}

fn points(value: &Value) -> Option<Vec<[f64; 2]>> {
    value
        .as_array()?
        .iter()
        .map(|point| {
            let (x, y) = point.as_str()?.split_once('x')?;
            let point = [x.parse::<f64>().ok()?, y.parse::<f64>().ok()?];
            point
                .iter()
                .all(|v| v.is_finite() && v.abs() <= MAX_PRINT_COORDINATE_MM)
                .then_some(point)
        })
        .collect()
}

fn plate(printer: &Value) -> (Option<PlateGeometry>, bool) {
    let Some(boundary_mm) = points(&printer["printable_area"]).filter(|points| points.len() >= 3)
    else {
        return (None, false);
    };
    let (exclusions_mm, exclusions_known) = match points(&printer["bed_exclude_area"]) {
        Some(points) if points.is_empty() || points.as_slice() == [[0., 0.]] => (Vec::new(), true),
        Some(points) if points.len() >= 3 => (vec![points], true),
        _ => (Vec::new(), false),
    };
    (
        Some(PlateGeometry {
            boundary_mm,
            exclusions_mm,
        }),
        exclusions_known,
    )
}

/// A failed analysis is explicit evidence of missing clearance, while prepared
/// native artifacts remain available for correction and further inspection.
pub fn analyze(
    source: &Path,
    printer: &Value,
    options: &PreflightOptions,
    deadline: Option<Instant>,
    cancel: &CancellationToken,
) -> Value {
    let result = deadline
        .ok_or_else(|| invalid("collision processing budget is no longer runtime-representable"))
        .and_then(|deadline| analyze_inner(source, printer, options, deadline, cancel));
    match result {
        Ok(value) => value,
        Err(error) => json!({
            "status":"unresolved",
            "checks":{"bed":"unmeasured","tower_interference":"unmeasured","toolhead":"unmeasured"},
            "processing_complete":false, "coverage_complete":false, "findings":[],
            "unresolved":[{"code":error.code(),"message":error.to_string()}],
            "physical_result":"not_observed",
        }),
    }
}

fn analyze_inner(
    source: &Path,
    printer: &Value,
    options: &PreflightOptions,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Value, ToolError> {
    let parsed = review::parse_for_preflight(
        source,
        deadline,
        cancel,
        options
            .initial_position
            .as_ref()
            .map(|position| position.position_mm),
    )?;
    let boxes = options
        .machine_envelopes
        .iter()
        .map(|envelope| ClearanceBox {
            component: envelope.component.clone(),
            minimum_mm: envelope.minimum_mm,
            maximum_mm: envelope.maximum_mm,
        })
        .collect::<Vec<_>>();
    let (plate, planar_exclusions_known) = plate(printer);
    let volumes_configured = printer["bed_exclusion_volumes_configured"].as_bool() == Some(true);
    let bed_exclusions_known = planar_exclusions_known && !volumes_configured;
    let mut report =
        print_collision::check_print_paths(&parsed.moves, plate.as_ref(), &boxes, || {
            Instant::now() < deadline && !cancel.is_cancelled()
        })
        .map_err(|error| invalid(&error.to_string()))?;
    let mut unresolved = parsed.unresolved.clone();
    if plate.is_none() {
        unresolved.insert("bed_boundary_not_available".into());
    }
    if !planar_exclusions_known {
        unresolved.insert("bed_exclusions_not_verified".into());
    }
    if volumes_configured {
        unresolved.insert("bed_exclusion_volumes_unmeasured".into());
    }
    if !bed_exclusions_known && report.bed != CheckState::Failed {
        report.bed = CheckState::Unmeasured;
    }
    if !parsed.complete || !parsed.unresolved.is_empty() || parsed.moves.is_empty() {
        let unsupported = parsed.unresolved.iter().any(|reason| {
            reason.starts_with("unsupported_")
                || reason == "arc_clearance_not_checked"
                || reason.contains("inch G-code")
                || reason.contains("non-XY arc planes")
        });
        for state in [
            &mut report.bed,
            &mut report.tower_interference,
            &mut report.toolhead,
        ] {
            if *state != CheckState::Failed {
                *state = if unsupported && *state == CheckState::Passed {
                    CheckState::Unsupported
                } else {
                    CheckState::Unmeasured
                };
            }
        }
    }
    let status = if !report.findings.is_empty() {
        "blocked"
    } else if [report.bed, report.tower_interference, report.toolhead]
        .iter()
        .any(|state| matches!(state, CheckState::Unmeasured | CheckState::Unsupported))
    {
        "unresolved"
    } else {
        "modeled_checks_passed"
    };
    let coverage_complete = parsed.complete
        && unresolved.is_empty()
        && report.complete
        && !report.missing_deposition_dimensions
        && [report.bed, report.tower_interference, report.toolhead]
            .iter()
            .all(|state| matches!(state, CheckState::Passed | CheckState::Failed));
    let (footprints, footprints_complete) =
        print_collision::deposited_footprints(&parsed.moves, || {
            Instant::now() < deadline && !cancel.is_cancelled()
        })
        .map_err(|error| invalid(&error.to_string()))?;
    let mut separations = Vec::new();
    let mut separations_complete = footprints_complete;
    'separations: for (a_index, a) in footprints.iter().enumerate() {
        for (b_index, b) in footprints.iter().enumerate().skip(a_index + 1) {
            if a.role == b.role && a.object == b.object {
                continue;
            }
            if separations.len() >= 4096 || Instant::now() >= deadline || cancel.is_cancelled() {
                separations_complete = false;
                break 'separations;
            }
            separations.push(json!({"footprints":[a_index,b_index],
                "xy_gap_lower_bound_mm":print_collision::footprint_gap_lower_bound(a,b),
                "overlapping_bounds_establish_contact":false}));
        }
    }
    Ok(json!({
        "status":status, "coordinate_frame":"final_gcode", "units":"mm",
        "checks":{"bed":report.bed,"tower_interference":report.tower_interference,"toolhead":report.toolhead},
        "processing_complete":parsed.complete && report.complete,
        "coverage_complete":coverage_complete,
        "findings":report.findings,"findings_truncated":report.findings_truncated, "unresolved":unresolved,
        "missing_deposition_dimensions":report.missing_deposition_dimensions,
        "machine_envelopes":options.machine_envelopes,
        "initial_position":options.initial_position,
        "bed_geometry":plate,
        "bed_exclusions_known":bed_exclusions_known,
        "deposited_footprints":{"coordinate_frame":"final_gcode","units":"mm",
            "representation":"conservative_axis_aligned_bounds_of_rectangular_deposition",
            "complete":footprints_complete && parsed.complete && parsed.unresolved.is_empty(),"items":footprints,
            "separations_complete":separations_complete,"separations":separations,
            "limits":["Includes support, brim and tower paths only when explicit motion and deposition dimensions are available.",
                "Bounds may include empty space; zero lower-bound separation does not prove contact.",
                "Native object labels are retained; missing ownership is not assigned by proximity."]},
        "scope":"explicit linear motion and conservative rectangular line envelopes in native execution order",
        "limits":["firmware macro motion, deformation, buildup and post-slice modifications are not simulated",
            "part-to-part source geometry and rounded deposition shape are not established by this check"],
        "physical_result":"not_observed",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn analyze(
        source: &Path,
        printer: &Value,
        options: &PreflightOptions,
        deadline: Option<Instant>,
    ) -> Value {
        super::analyze(
            source,
            printer,
            options,
            deadline,
            &CancellationToken::new(),
        )
    }

    fn source(text: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file
    }

    fn known_start() -> PreflightOptions {
        PreflightOptions {
            initial_position: Some(InitialPosition {
                position_mm: [0.; 3],
                source: "synthetic program start".into(),
            }),
            ..PreflightOptions::default()
        }
    }

    #[test]
    fn startup_deposition_is_an_obstacle_for_later_layer_motion() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z1.2\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; CHANGE_LAYER\nG1 X15 Y5 Z0.2\nG1 Y15\n",
        );
        let mut options = known_start();
        options.machine_envelopes.push(MachineEnvelope {
            component: "shroud".into(),
            minimum_mm: [-0.1, -0.1, 0.5],
            maximum_mm: [0.1, 0.1, 1.5],
            source: "synthetic component".into(),
        });
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["checks"]["toolhead"], "failed");
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["kind"] == "toolhead_interference"
                    && finding["other_source_line"] == 7)
        );
    }

    #[test]
    fn unclassified_startup_priming_and_stationary_extrusion_remain_unresolved() {
        let options = known_start();
        for (text, reason) in [
            (
                "G90\nM83\nG1 X10 Y10 Z0.2\nG1 X20 E1\n; CHANGE_LAYER\n",
                "custom_or_unclassified_motion_not_checked",
            ),
            (
                "M83\nG1 E1\n; CHANGE_LAYER\n",
                "stationary_extrusion_not_checked",
            ),
        ] {
            let gcode = source(text);
            let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
            assert_eq!(report["coverage_complete"], false);
            assert!(
                report["unresolved"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == reason)
            );
        }
    }

    #[test]
    fn a_missing_initial_position_is_not_inferred_from_zero_coordinates() {
        let gcode = source(
            "G90\nM83\nG1 X0 Y0 Z0\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["coverage_complete"], false);
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "motion_without_known_xyz_origin")
        );
        let mut options = known_start();
        options.initial_position.as_mut().unwrap().source.clear();
        assert!(options.validate().is_err());
        options.initial_position.as_mut().unwrap().source = "synthetic".into();
        options.initial_position.as_mut().unwrap().position_mm[0] = f64::NAN;
        assert!(options.validate().is_err());
    }

    #[test]
    fn absolute_extrusion_and_tool_selection_do_not_invent_machine_state() {
        let options = known_start();
        let printer = json!({"printable_area":["0x0","25x0","25x25","0x25"],"bed_exclude_area":[]});
        let body = "G90\nM82\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n";
        let gcode = source(body);
        let report = analyze(gcode.path(), &printer, &options, options.deadline());
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "absolute_extrusion_origin_unknown")
        );
        let gcode = source(&format!("G92 E0\n{body}"));
        let report = analyze(gcode.path(), &printer, &options, options.deadline());
        assert_eq!(report["unresolved"], json!([]));
        let gcode = source(&format!("G92 E0\n{body}T1\nG1 X10 Y10 Z0.2 E0\n"));
        let report = analyze(gcode.path(), &printer, &options, options.deadline());
        assert_eq!(report["coverage_complete"], false);
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "unsupported_firmware_toolchange_motion")
        );
    }

    #[test]
    fn native_success_does_not_prevent_a_positive_tower_collision_finding() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["checks"]["tower_interference"], "failed");
        assert_eq!(report["findings"][0]["kind"], "tower_interference");
        assert!(report["findings"][0]["source_line"].as_u64().is_some());
    }

    #[test]
    fn unsupported_later_motion_retains_an_earlier_collision() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\nG20\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["processing_complete"], false);
        assert_eq!(report["findings"][0]["kind"], "tower_interference");
        assert!(!report["unresolved"].as_array().unwrap().is_empty());
    }

    #[test]
    fn missing_or_invalid_bed_metadata_and_machine_geometry_remain_unmeasured() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n",
        );
        let options = PreflightOptions::default();
        for printer in [
            json!({}),
            json!({"printable_area":["0x0","25x0","25x25","0x25"],"bed_exclude_area":["bad"]}),
        ] {
            let report = analyze(gcode.path(), &printer, &options, options.deadline());
            assert_eq!(report["status"], "unresolved");
            assert_eq!(report["checks"]["bed"], "unmeasured");
            assert_eq!(report["checks"]["toolhead"], "unmeasured");
        }
    }

    #[test]
    fn native_empty_exclusion_and_unknown_exclusions_preserve_known_boundary_findings() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X30 E1\n",
        );
        let options = known_start();
        for (excluded, known) in [
            (json!(["0x0"]), true),
            (json!(["0.000x-0.0"]), true),
            (json!(["bad"]), false),
        ] {
            let printer =
                json!({"printable_area":["0x0","25x0","25x25","0x25"],"bed_exclude_area":excluded});
            let report = analyze(gcode.path(), &printer, &options, options.deadline());
            assert_eq!(report["checks"]["bed"], "failed");
            assert_eq!(report["status"], "blocked");
            assert_eq!(report["bed_exclusions_known"], known);
            assert!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| finding["kind"] == "outside_bed")
            );
            if !known {
                assert_eq!(report["coverage_complete"], false);
            }
        }
        let inside = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n",
        );
        let report = analyze(
            inside.path(),
            &json!({"printable_area":["0x0","25x0","25x25","0x25"],"bed_exclude_area":["0x0"]}),
            &options,
            options.deadline(),
        );
        assert_eq!(report["checks"]["bed"], "passed");
        let with_volume = json!({"printable_area":["0x0","25x0","25x25","0x25"],
            "bed_exclude_area":["0x0"],"bed_exclusion_volumes_configured":true});
        let report = analyze(inside.path(), &with_volume, &options, options.deadline());
        assert_eq!(report["checks"]["bed"], "unmeasured");
        assert_eq!(report["bed_exclusions_known"], false);
        assert_eq!(report["coverage_complete"], false);
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .contains(&json!("bed_exclusion_volumes_unmeasured"))
        );
        let report = analyze(gcode.path(), &with_volume, &options, options.deadline());
        assert_eq!(report["checks"]["bed"], "failed");
        assert_eq!(report["status"], "blocked");
    }

    #[test]
    fn invalid_machine_geometry_is_rejected_before_native_preparation() {
        let mut options = PreflightOptions::default();
        options.machine_envelopes.push(MachineEnvelope {
            component: "shroud".into(),
            source: "manufacturer envelope".into(),
            minimum_mm: [0.; 3],
            maximum_mm: [1., 1., 0.],
        });
        assert!(options.validate().is_err());
        options.machine_envelopes[0].maximum_mm = [1., 1., 1e308];
        assert!(options.validate().is_err());
        options.machine_envelopes.clear();
        options.timeout_seconds = 0;
        assert!(options.validate().is_err());
    }

    #[test]
    fn setup_motion_can_be_reanchored_without_inventing_a_printing_origin() {
        let gcode = source(
            "G29\nG90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["checks"]["tower_interference"], "failed");
    }

    #[test]
    fn conditional_firmware_paths_are_not_treated_as_unconditional_deposition() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nM622 J1\nG1 X20 E1\nM623\nG1 X15 Y5 Z0.2\n; FEATURE: Prime tower\nG1 Y15 E1\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "unresolved");
        assert!(report["findings"].as_array().unwrap().is_empty());
        assert_eq!(report["checks"]["tower_interference"], "unsupported");
    }

    #[test]
    fn conditional_extrusion_cannot_fabricate_deposition_after_xyz_reanchoring() {
        let body = "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 E-1\nM622 J1\nG1 E1\nM623\nG1 X10 Y10 Z0.2\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5 Z0.2\nG1 Y15 E1\n";
        let gcode = source(body);
        let options = known_start();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "unresolved");
        assert_eq!(report["checks"]["tower_interference"], "unsupported");
        assert!(report["findings"].as_array().unwrap().is_empty());

        let earlier_collision = "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\n";
        let gcode = source(&format!("{earlier_collision}{body}"));
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["coverage_complete"], false);
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["kind"] == "tower_interference"
                    && finding["source_line"].as_u64().unwrap() < 13)
        );
    }

    #[test]
    fn canceled_or_expired_analysis_does_not_clear_empty_coverage() {
        let gcode = source("G90\nM83\nG1 X10 Y10 Z0.2\n");
        let options = PreflightOptions::default();
        let token = CancellationToken::new();
        token.cancel();
        let canceled = super::analyze(
            gcode.path(),
            &json!({}),
            &options,
            options.deadline(),
            &token,
        );
        assert_eq!(canceled["status"], "unresolved");
        assert_eq!(canceled["processing_complete"], false);
        assert_eq!(canceled["coverage_complete"], false);
        let expired = analyze(gcode.path(), &json!({}), &options, Some(Instant::now()));
        assert_eq!(expired["status"], "unresolved");
        assert_eq!(expired["coverage_complete"], false);
        let unrepresentable = analyze(gcode.path(), &json!({}), &options, None);
        assert_eq!(unrepresentable["status"], "unresolved");
        assert_eq!(unrepresentable["coverage_complete"], false);
    }

    #[test]
    fn a_move_that_only_restores_retracted_filament_is_not_an_obstacle() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 E-1\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\n",
        );
        let options = known_start();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["checks"]["tower_interference"], "passed");
        assert!(report["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn unknown_absolute_origin_cannot_create_an_obstacle_from_unknown_retraction_debt() {
        let options = known_start();
        let printer = json!({"printable_area":["0x0","50x0","50x50","0x50"],"bed_exclude_area":[]});
        for reset in ["", "G92 E0\nM83\n"] {
            let gcode = source(&format!(
                "G90\nM82\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 E1\n{reset}G1 X20 E2\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E3\n"
            ));
            let report = analyze(gcode.path(), &printer, &options, options.deadline());
            assert_eq!(report["status"], "unresolved");
            assert!(report["findings"].as_array().unwrap().is_empty());
            assert!(
                report["unresolved"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r == "absolute_extrusion_origin_unknown")
            );
            assert_eq!(report["coverage_complete"], false);
        }
    }

    #[test]
    fn inline_evidence_is_bounded_without_hiding_aggregate_failure_or_analysis_truncation() {
        let plate = json!({"status":"blocked","findings":(0..100).map(|i|json!({"source_line":i})).collect::<Vec<_>>(),
            "findings_truncated":true,"deposited_footprints":{"items":vec![json!({});100],"separations":vec![json!({});100]}});
        let plates = (0..40)
            .map(|i| (format!("plate_{i}.gcode"), plate.clone()))
            .collect::<serde_json::Map<_, _>>();
        let compact = compact(json!({"status":"blocked","plates":plates}));
        assert_eq!(compact["status"], "blocked");
        assert_eq!(compact["plates_count"], 40);
        assert_eq!(compact["plates_inline_truncated"], true);
        assert_eq!(compact["plates"].as_object().unwrap().len(), 32);
        for plate in compact["plates"].as_object().unwrap().values() {
            assert_eq!(plate["findings"].as_array().unwrap().len(), 32);
            assert_eq!(plate["findings_count"], 100);
            assert_eq!(plate["findings_inline_truncated"], true);
            assert_eq!(plate["findings_truncated"], true);
            assert_eq!(plate["deposited_footprints"]["items_count"], 100);
            assert_eq!(plate["deposited_footprints"]["separations_count"], 100);
        }
    }

    #[test]
    fn unsupported_firmware_commands_cannot_leave_a_false_clearance_pass() {
        let baseline = "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n";
        let printer = json!({"printable_area":["0x0","50x0","50x50","0x50"],"bed_exclude_area":[]});
        let mut options = known_start();
        options.machine_envelopes.push(MachineEnvelope {
            component: "shroud".into(),
            minimum_mm: [-1., -1., 1.],
            maximum_mm: [1., 1., 4.],
            source: "synthetic geometry".into(),
        });
        let original = source(baseline);
        assert_eq!(
            analyze(original.path(), &printer, &options, options.deadline())["status"],
            "modeled_checks_passed"
        );
        for command in [
            "SET_GCODE_OFFSET X=100 MOVE=0",
            "VENDOR_POSITION_MACRO",
            "M206 X100",
            "M218 X100",
            "M98 P\"motion.g\"",
            "M99",
            "M9999 P1",
        ] {
            for text in [
                format!("{command}\n{baseline}"),
                format!("{baseline}{command}\nG1 X25 E1\n"),
            ] {
                let gcode = source(&text);
                let report = analyze(gcode.path(), &printer, &options, options.deadline());
                assert_eq!(report["status"], "unresolved", "{command}: {report}");
                assert_eq!(report["coverage_complete"], false, "{command}: {report}");
                assert!(!report["unresolved"].as_array().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn standard_temperature_and_fan_commands_preserve_explicit_linear_motion() {
        let gcode = source(
            "M104 S200\nG90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nM109 S200\nM106 S255\nG1 X20 E1\nM107\n",
        );
        let options = known_start();
        let printer = json!({"printable_area":["0x0","25x0","25x25","0x25"],"bed_exclude_area":[]});
        let report = analyze(gcode.path(), &printer, &options, options.deadline());
        assert_eq!(report["checks"]["tower_interference"], "passed");
        assert_eq!(report["unresolved"], json!([]));
    }

    #[test]
    fn a_later_named_macro_does_not_discard_an_established_collision() {
        let gcode = source(
            "G90\nM83\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n; FEATURE: Prime tower\nG1 X15 Y5\nG1 Y15 E1\nSET_GCODE_OFFSET X=100 MOVE=0\nG1 X25 E1\n",
        );
        let options = PreflightOptions::default();
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["checks"]["tower_interference"], "failed");
        assert!(!report["unresolved"].as_array().unwrap().is_empty());
    }
}
