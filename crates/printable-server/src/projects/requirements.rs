//! Declared design requirements and the limits of native geometric evidence.

use crate::error::ToolError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Mm,
    Inch,
    Degrees,
    Scalar,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub value: f64,
    pub unit: Unit,
    pub minimum: f64,
    pub maximum: f64,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Requirement {
    Dimensions {
        size_mm: [f64; 3],
        tolerance_mm: f64,
    },
    /// Vertical cylindrical inner walls at these XY positions, in model coordinates.
    /// This measurement does not establish thread, through-hole, or fastener fit.
    HolePattern {
        centers_mm: Vec<[f64; 2]>,
        radius_mm: f64,
        tolerance_mm: f64,
    },
    BuildEnvelope {
        size_mm: [f64; 3],
    },
    /// Retained but unmeasured by the CAD report's current geometry checks.
    Clearance {
        minimum_mm: f64,
        description: String,
    },
    PhysicalTest {
        description: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    /// Modeling coordinates must be explicitly declared in millimetres.
    pub units: LengthUnit,
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
    #[serde(default)]
    pub requirements: BTreeMap<String, Requirement>,
    /// Descriptive notes only; never executed or interpreted as tool instructions.
    #[serde(default)]
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LengthUnit {
    Mm,
}

fn positive(v: f64) -> bool {
    v.is_finite() && v > 0.0
}
fn tolerance(v: f64) -> bool {
    v.is_finite() && (0.0..=10.0).contains(&v)
}
fn description(v: &str) -> bool {
    !v.trim().is_empty() && v.len() <= 1024
}

#[derive(Deserialize)]
struct Hole {
    center_mm: [f64; 2],
    radius_mm: f64,
}

fn match_holes(expected: &[[f64; 2]], radius: f64, tolerance: f64, measured: &[Hole]) -> bool {
    let candidates: Vec<Vec<usize>> = expected
        .iter()
        .map(|center| {
            measured
                .iter()
                .enumerate()
                .filter(|(_, hole)| {
                    center
                        .iter()
                        .zip(hole.center_mm)
                        .all(|(a, b)| (a - b).abs() <= tolerance)
                        && (radius - hole.radius_mm).abs() <= tolerance
                })
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    fn assign(
        row: usize,
        edges: &[Vec<usize>],
        owners: &mut [Option<usize>],
        visited: &mut [bool],
    ) -> bool {
        for &column in &edges[row] {
            if visited[column] {
                continue;
            }
            visited[column] = true;
            if owners[column].is_none_or(|previous| assign(previous, edges, owners, visited)) {
                owners[column] = Some(row);
                return true;
            }
        }
        false
    }
    let mut owners = vec![None; measured.len()];
    (0..expected.len()).all(|row| {
        assign(
            row,
            &candidates,
            &mut owners,
            &mut vec![false; measured.len()],
        )
    })
}
pub(super) fn name(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ToolError> {
        let valid = self.format_version == 1
            && self.parameters.len() <= 32
            && self.requirements.len() <= 64
            && self.assumptions.len() <= 32
            && self.assumptions.iter().all(|s| description(s))
            && self.parameters.iter().all(|(key, p)| {
                name(key)
                    && description(&p.description)
                    && [p.value, p.minimum, p.maximum]
                        .iter()
                        .all(|n| n.is_finite())
                    && p.minimum <= p.value
                    && p.value <= p.maximum
            })
            && self.requirements.iter().all(|(key, requirement)| {
                name(key)
                    && match requirement {
                        Requirement::Dimensions {
                            size_mm,
                            tolerance_mm,
                        } => size_mm.iter().all(|v| positive(*v)) && tolerance(*tolerance_mm),
                        Requirement::HolePattern {
                            centers_mm,
                            radius_mm,
                            tolerance_mm,
                        } => {
                            !centers_mm.is_empty()
                                && centers_mm.len() <= 64
                                && positive(*radius_mm)
                                && tolerance(*tolerance_mm)
                                && centers_mm.iter().flatten().all(|n| n.is_finite())
                                && centers_mm
                                    .iter()
                                    .enumerate()
                                    .all(|(i, p)| !centers_mm[..i].contains(p))
                        }
                        Requirement::BuildEnvelope { size_mm } => {
                            size_mm.iter().all(|v| positive(*v))
                        }
                        Requirement::Clearance {
                            minimum_mm,
                            description: text,
                        } => positive(*minimum_mm) && description(text),
                        Requirement::PhysicalTest { description: text } => description(text),
                    }
            });
        if !valid || serde_json::to_vec(self)?.len() > 64 * 1024 {
            return Err(ToolError::Validation("manifest requires version 1, bounded parameter ranges and descriptions, finite millimetre requirements, and bounded notes".into()));
        }
        Ok(())
    }

    /// Values remain in their declared parameter units; no hidden conversion.
    pub fn values(&self) -> BTreeMap<String, Value> {
        self.parameters
            .iter()
            .map(|(key, p)| (key.clone(), json!(p.value)))
            .collect()
    }

    pub fn assess(&self, report: &Value) -> Value {
        let dimensions: Option<[f64; 3]> =
            serde_json::from_value(report["bounds_mm"]["size"].clone()).ok();
        let dimensions = dimensions.filter(|v| v.iter().all(|n| positive(*n)));
        let known_mm = report["units"] == "mm";
        let holes: Option<Vec<Hole>> =
            serde_json::from_value(report["vertical_holes"]["holes"].clone()).ok();
        let holes = holes.filter(|holes| {
            report["vertical_holes"]["status"] == "measured"
                && report["valid"] == true
                && known_mm
                && holes.len() <= 4096
                && holes
                    .iter()
                    .all(|h| positive(h.radius_mm) && h.center_mm.iter().all(|n| n.is_finite()))
        });
        let checked = |pass: bool, evidence: &str| json!({"status":if pass {"passed"} else {"failed"}, "evidence":evidence});
        let unmeasured = || json!({"status":"unmeasured"});
        let criteria: BTreeMap<_, _> = self
            .requirements
            .iter()
            .map(|(id, r)| {
                let result = match r {
                    Requirement::Dimensions {
                        size_mm,
                        tolerance_mm,
                    } => dimensions
                        .filter(|_| known_mm)
                        .map(|d| {
                            checked(
                                d.iter()
                                    .zip(size_mm)
                                    .all(|(a, b)| (a - b).abs() <= *tolerance_mm),
                                "/bounds_mm/size",
                            )
                        })
                        .unwrap_or_else(unmeasured),
                    Requirement::BuildEnvelope { size_mm } => dimensions
                        .filter(|_| known_mm)
                        .map(|d| {
                            checked(
                                d.iter().zip(size_mm).all(|(a, b)| a <= b),
                                "/bounds_mm/size",
                            )
                        })
                        .unwrap_or_else(unmeasured),
                    Requirement::HolePattern {
                        centers_mm,
                        radius_mm,
                        tolerance_mm,
                    } => holes
                        .as_ref()
                        .map(|holes| {
                            let fits = match_holes(centers_mm, *radius_mm, *tolerance_mm, holes);
                            checked(fits, "/vertical_holes")
                        })
                        .unwrap_or_else(unmeasured),
                    Requirement::Clearance { .. } => unmeasured(),
                    Requirement::PhysicalTest { .. } => json!({"status":"physical_test_required"}),
                };
                (id, result)
            })
            .collect();
        let status = if criteria.values().any(|v| v["status"] == "failed") {
            "failed"
        } else if criteria.is_empty() || criteria.values().any(|v| v["status"] != "passed") {
            "incomplete"
        } else {
            "passed"
        };
        json!({"scope":"declared_requirements", "status":status, "criteria":criteria})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hole_assignment_handles_overlapping_tolerances_without_reusing_a_hole() {
        let holes = vec![
            Hole {
                center_mm: [0.0, 0.0],
                radius_mm: 1.0,
            },
            Hole {
                center_mm: [0.1, 0.0],
                radius_mm: 1.0,
            },
        ];
        assert!(match_holes(&[[0.05, 0.0], [-0.05, 0.0]], 1.0, 0.06, &holes));
        assert!(!match_holes(
            &[[0.0, 0.0], [0.01, 0.0]],
            1.0,
            0.06,
            &holes[..1]
        ));
    }
    #[test]
    fn wider_part_keeps_hole_grid_but_a_moved_hole_fails_despite_valid_solids() {
        let manifest:Manifest=serde_json::from_value(json!({"format_version":1,"units":"mm","requirements":{
            "mounts":{"kind":"hole_pattern","centers_mm":[[-12,-8],[-12,8],[12,-8],[12,8]],"radius_mm":2,"tolerance_mm":0.05},
            "envelope":{"kind":"build_envelope","size_mm":[80,40,20]},
            "fit":{"kind":"clearance","minimum_mm":0.3,"description":"lid clearance"},
            "load":{"kind":"physical_test","description":"test mounting load"}}})).unwrap();
        manifest.validate().unwrap();
        let holes: Vec<_> = [[-12, -8], [-12, 8], [12, -8], [12, 8]]
            .into_iter()
            .map(|p| json!({"center_mm":p,"radius_mm":2}))
            .collect();
        let mut report = json!({"units":"mm","valid":true,"bounds_mm":{"size":[40,30,5]},"vertical_holes":{"status":"measured","holes":holes}});
        for width in [40, 60] {
            report["bounds_mm"]["size"][0] = json!(width);
            let assessment = manifest.assess(&report);
            assert_eq!(assessment["criteria"]["mounts"]["status"], "passed");
            assert_eq!(assessment["criteria"]["fit"]["status"], "unmeasured");
            assert_eq!(
                assessment["criteria"]["load"]["status"],
                "physical_test_required"
            );
            assert_eq!(assessment["status"], "incomplete");
        }
        report["vertical_holes"]["holes"][0]["center_mm"][0] = json!(-10);
        assert_eq!(
            manifest.assess(&report)["criteria"]["mounts"]["status"],
            "failed"
        );
        report["vertical_holes"]["status"] = json!("unmeasured");
        assert_eq!(
            manifest.assess(&report)["criteria"]["mounts"]["status"],
            "unmeasured"
        );
    }
}
