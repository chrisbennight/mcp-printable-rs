//! Delivery-policy checks over native CAD measurements; no modeling or I/O.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    Inspection,
    PrintablePart,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    #[default]
    Mm,
    Inch,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Requirements {
    pub policy: Policy,
    /// Required delivery units. Current CAD exports use millimetres.
    pub units: Units,
    /// Required axis-aligned dimensions in millimetres, in X/Y/Z order.
    pub dimensions_mm: Option<[f64; 3]>,
    pub dimension_tolerance_mm: f64,
    pub solid_count: Option<usize>,
}

impl Default for Requirements {
    fn default() -> Self {
        Self {
            policy: Policy::Inspection,
            units: Units::Mm,
            dimensions_mm: None,
            dimension_tolerance_mm: 0.05,
            solid_count: None,
        }
    }
}

impl Requirements {
    pub fn validate(&self) -> Result<(), ToolError> {
        if !self.dimension_tolerance_mm.is_finite()
            || self.dimension_tolerance_mm < 0.0
            || self.dimensions_mm.is_some_and(|dimensions| {
                dimensions
                    .iter()
                    .any(|value| !value.is_finite() || *value <= 0.0)
            })
            || self
                .solid_count
                .is_some_and(|count| !(1..=1024).contains(&count))
        {
            return Err(ToolError::Validation("CAD qualification requires positive finite dimensions, a finite nonnegative tolerance, and a solid count from 1 to 1024".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Passed,
    Failed,
    Unmeasured,
    PhysicalTestRequired,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Criterion {
    pub status: CriterionStatus,
    pub required: bool,
    /// JSON pointer relative to the native report, when measurement is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<&'static str>,
}

#[derive(Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Incomplete,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Qualification {
    pub scope: &'static str,
    pub policy: Policy,
    pub status: Status,
    pub requirements: Requirements,
    pub criteria: BTreeMap<&'static str, Criterion>,
}

fn criterion(
    value: Option<bool>,
    required: bool,
    evidence: &'static str,
    next_action: &'static str,
) -> Criterion {
    Criterion {
        status: match value {
            Some(true) => CriterionStatus::Passed,
            Some(false) => CriterionStatus::Failed,
            None => CriterionStatus::Unmeasured,
        },
        required,
        evidence: value.map(|_| evidence),
        next_action: (value != Some(true)).then_some(next_action),
    }
}

pub fn assess(report: &Value, requirements: &Requirements) -> Qualification {
    let printable = requirements.policy == Policy::PrintablePart;
    let dimensions = requirements.dimensions_mm.and_then(|expected| {
        let measured = report["bounds_mm"]["size"].as_array()?;
        if measured.len() != 3 {
            return None;
        }
        let values = measured
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<_>>>()?;
        Some(values.iter().zip(expected).all(|(actual, expected)| {
            actual.is_finite() && (*actual - expected).abs() <= requirements.dimension_tolerance_mm
        }))
    });
    let solids = report["solid_count"].as_u64();
    let meaningful = solids.and_then(|count| {
        if count == 0 {
            return Some(false);
        }
        let volumes = report["solid_volumes_mm3"].as_array()?;
        if u64::try_from(volumes.len()).ok()? != count {
            return None;
        }
        let volumes = volumes
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<_>>>()?;
        Some(
            volumes
                .iter()
                .all(|volume| volume.is_finite() && *volume > 0.0),
        )
    });
    let units = report["units"].as_str().map(|units| {
        units
            == match requirements.units {
                Units::Mm => "mm",
                Units::Inch => "inch",
            }
    });
    let criteria = BTreeMap::from([
        (
            "valid_geometry",
            criterion(
                report["valid"].as_bool(),
                printable,
                "/valid",
                "Inspect native diagnostics and repair invalid geometry.",
            ),
        ),
        (
            "meaningful_solids",
            criterion(
                meaningful,
                printable,
                "/solid_count",
                "Close surfaces and verify positive-volume solids before fabrication.",
            ),
        ),
        (
            "units",
            criterion(
                units,
                true,
                "/units",
                "Current CAD exports use millimetres; reconcile the delivery unit requirement.",
            ),
        ),
        (
            "dimensions",
            criterion(
                dimensions,
                printable || requirements.dimensions_mm.is_some(),
                "/bounds_mm/size",
                "Declare target dimensions in millimetres and revise the model or its parameters to match.",
            ),
        ),
        (
            "solid_count",
            criterion(
                requirements
                    .solid_count
                    .and_then(|expected| solids.map(|count| count == expected as u64)),
                requirements.solid_count.is_some(),
                "/solid_count",
                "Check assembly structure against the requested solid count.",
            ),
        ),
        (
            "physical_performance",
            Criterion {
                status: CriterionStatus::PhysicalTestRequired,
                required: false,
                evidence: None,
                next_action: Some(
                    "Verify physical fit, strength, material and print-process suitability separately.",
                ),
            },
        ),
    ]);
    let status = if criteria
        .values()
        .any(|criterion| criterion.required && criterion.status == CriterionStatus::Failed)
    {
        Status::Failed
    } else if criteria
        .values()
        .any(|criterion| criterion.required && criterion.status != CriterionStatus::Passed)
    {
        Status::Incomplete
    } else {
        Status::Passed
    };
    Qualification {
        scope: "cad_delivery",
        policy: requirements.policy,
        status,
        requirements: requirements.clone(),
        criteria,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report() -> Value {
        json!({"valid":true,"units":"mm","solid_count":2,"solid_volumes_mm3":[50,75],"bounds_mm":{"size":[42,20,30]}})
    }

    #[test]
    fn delivery_policies_distinguish_geometry_units_dimensions_and_multiple_parts() {
        let requirements = Requirements {
            policy: Policy::PrintablePart,
            dimensions_mm: Some([42.0, 20.0, 30.0]),
            solid_count: Some(2),
            ..Requirements::default()
        };
        assert_eq!(assess(&report(), &requirements).status, Status::Passed);
        for (field, value, criterion) in [
            ("valid", json!(false), "valid_geometry"),
            ("solid_count", json!(0), "meaningful_solids"),
            ("units", json!("inch"), "units"),
            ("bounds_mm", json!({"size":[1.6535,20,30]}), "dimensions"),
            ("solid_volumes_mm3", json!([50, -75]), "meaningful_solids"),
        ] {
            let mut report = report();
            report[field] = value;
            let result = assess(&report, &requirements);
            assert_eq!(result.status, Status::Failed);
            assert_eq!(result.criteria[criterion].status, CriterionStatus::Failed);
        }
        assert_eq!(
            assess(
                &report(),
                &Requirements {
                    policy: Policy::PrintablePart,
                    ..Requirements::default()
                }
            )
            .status,
            Status::Incomplete
        );
        let mut open = report();
        open["solid_count"] = json!(0);
        let inspection = assess(&open, &Requirements::default());
        assert_eq!(inspection.status, Status::Passed);
        assert_eq!(
            inspection.criteria["meaningful_solids"].status,
            CriterionStatus::Failed
        );
        assert!(!inspection.criteria["meaningful_solids"].required);
        assert_eq!(assess(&json!({}), &requirements).status, Status::Incomplete);
    }
}
