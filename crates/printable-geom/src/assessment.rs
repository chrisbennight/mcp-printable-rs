//! Scoped manufacturing evidence; a geometric measurement is not a physical test.

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Passed,
    Failed,
    Unmeasured,
    PhysicalTestRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentStatus {
    Failed,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssessmentCriterion {
    pub status: CriterionStatus,
    /// JSON pointer relative to the containing validation report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeshAssessment {
    pub scope: &'static str,
    pub status: AssessmentStatus,
    pub criteria: BTreeMap<&'static str, AssessmentCriterion>,
}

impl MeshAssessment {
    pub(crate) fn new(
        solid_geometry: bool,
        finite_dimensions: bool,
        requires_support: bool,
    ) -> Self {
        use CriterionStatus::*;
        let checked = |passed, evidence| AssessmentCriterion {
            status: if passed { Passed } else { Failed },
            evidence: Some(evidence),
        };
        Self {
            scope: "mesh_geometry",
            status: if solid_geometry && finite_dimensions && !requires_support {
                AssessmentStatus::Incomplete
            } else {
                AssessmentStatus::Failed
            },
            criteria: BTreeMap::from([
                ("solid_topology", checked(solid_geometry, "/topology")),
                ("finite_dimensions", checked(finite_dimensions, "/bounds")),
                (
                    "support_free_orientation",
                    checked(!requires_support, "/overhang"),
                ),
                (
                    "wall_thickness",
                    AssessmentCriterion {
                        status: Unmeasured,
                        evidence: None,
                    },
                ),
                (
                    "dimensional_requirements",
                    AssessmentCriterion {
                        status: Unmeasured,
                        evidence: None,
                    },
                ),
                (
                    "build_envelope",
                    AssessmentCriterion {
                        status: Unmeasured,
                        evidence: None,
                    },
                ),
                (
                    "material_process",
                    AssessmentCriterion {
                        status: Unmeasured,
                        evidence: None,
                    },
                ),
                (
                    "physical_performance",
                    AssessmentCriterion {
                        status: PhysicalTestRequired,
                        evidence: None,
                    },
                ),
            ]),
        }
    }
}
