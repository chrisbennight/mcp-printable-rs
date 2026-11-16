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

fn polygon(value: &Value) -> Option<Vec<[f64; 2]>> {
    let points = value
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
        .collect::<Option<Vec<_>>>()?;
    (points.len() >= 3).then_some(points)
}

fn plate(printer: &Value) -> Option<PlateGeometry> {
    let boundary_mm = polygon(&printer["printable_area"])?;
    let exclusions = &printer["bed_exclude_area"];
    let exclusions_mm = if exclusions.as_array().is_some_and(Vec::is_empty) {
        Vec::new()
    } else {
        vec![polygon(exclusions)?]
    };
    Some(PlateGeometry {
        boundary_mm,
        exclusions_mm,
    })
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
    let plate = plate(printer);
    let mut report =
        print_collision::check_print_paths(&parsed.moves, plate.as_ref(), &boxes, || {
            Instant::now() < deadline && !cancel.is_cancelled()
        })
        .map_err(|error| invalid(&error.to_string()))?;
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
    let coverage_complete = [report.bed, report.tower_interference, report.toolhead]
        .iter()
        .all(|state| matches!(state, CheckState::Passed | CheckState::Failed));
    Ok(json!({
        "status":status, "coordinate_frame":"final_gcode", "units":"mm",
        "checks":{"bed":report.bed,"tower_interference":report.tower_interference,"toolhead":report.toolhead},
        "processing_complete":parsed.complete && report.complete,
        "coverage_complete":coverage_complete,
        "findings":report.findings, "unresolved":parsed.unresolved,
        "missing_deposition_dimensions":report.missing_deposition_dimensions,
        "machine_envelopes":options.machine_envelopes,
        "initial_position":options.initial_position,
        "bed_geometry":plate,
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
        let body = "G90\nM82\nG1 X10 Y10 Z0.2\n; CHANGE_LAYER\n; LAYER_HEIGHT: 0.2\n; LINE_WIDTH: 0.4\n; FEATURE: Outer wall\nG1 X20 E1\n";
        let gcode = source(body);
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "absolute_extrusion_origin_unknown")
        );
        let gcode = source(&format!("G92 E0\n{body}"));
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
        assert_eq!(report["unresolved"], json!([]));
        let gcode = source(&format!("G92 E0\n{body}T1\nG1 X10 Y10 Z0.2 E0\n"));
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
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
        let report = analyze(gcode.path(), &json!({}), &options, options.deadline());
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
