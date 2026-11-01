//! Focused native CAD inspection without delivery exports.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{BuildParams, angular_tolerance, linear_tolerance, qualification, timeout_seconds};
use crate::{error::ToolError, projects};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    pub project_id: String,
    /// Project-relative STEP file or Python script assigning a native CAD result.
    pub source: Option<String>,
    /// Reopen an immutable native result instead of executing or translating source.
    pub model: Option<ModelReference>,
    /// New project-relative directory retaining this inspection and its inputs.
    pub output_dir: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub revision: Option<projects::revisions::Identity>,
    #[serde(default = "timeout_seconds")]
    pub timeout_seconds: u64,
    /// Return admission immediately; status and cancel use the usual build handle.
    #[serde(default = "background")]
    pub background: bool,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default)]
    pub frame: Frame,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "page_size")]
    pub limit: usize,
    /// Retain native geometry for follow-up questions. Defaults to true.
    #[serde(default = "background")]
    pub retain_model: bool,
    #[serde(default)]
    pub questions: Vec<Question>,
    /// Optional selected deliverables, sharing the same loaded geometry.
    #[serde(default)]
    pub exports: Vec<Format>,
    #[serde(default = "linear_tolerance")]
    pub linear_tolerance_mm: f64,
    #[serde(default = "angular_tolerance")]
    pub angular_tolerance_rad: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelReference {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Step,
    Stl,
    Glb,
    Svg,
}

impl Format {
    pub(super) fn filename(&self) -> &'static str {
        match self {
            Self::Step => "selected.step",
            Self::Stl => "selected.stl",
            Self::Glb => "selected.glb",
            Self::Svg => "selected.svg",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Question {
    /// Selected native planar and cylindrical faces; indices apply only to this model.
    Features {
        id: String,
        component_id: Option<String>,
        selection: Option<Selection>,
        #[serde(default)]
        offset: usize,
        #[serde(default = "page_size")]
        limit: usize,
    },
    Distance {
        id: String,
        first: Target,
        second: Target,
    },
    Alignment {
        id: String,
        first: Target,
        second: Target,
    },
    Section {
        id: String,
        component_id: Option<String>,
        selection: Option<Selection>,
        origin_mm: [f64; 3],
        normal: [f64; 3],
    },
    Validity {
        id: String,
        component_id: Option<String>,
        selection: Option<Selection>,
    },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub component_id: Option<String>,
    /// Resolve one occurrence by selectors; omitted identity/selection uses the outer selection.
    pub selection: Option<Selection>,
    /// Native face index returned by features; omitted means the complete component.
    pub face_index: Option<usize>,
}

fn background() -> bool {
    true
}
fn page_size() -> usize {
    20
}

impl InspectParams {
    pub(super) fn build_params(&self) -> BuildParams {
        BuildParams {
            project_id: self.project_id.clone(),
            source: self
                .model
                .as_ref()
                .map(|model| model.path.clone())
                .or_else(|| self.source.clone())
                .unwrap_or_default(),
            output_dir: self.output_dir.clone(),
            parameters: self.parameters.clone(),
            inputs: self.inputs.clone(),
            linear_tolerance_mm: self.linear_tolerance_mm,
            angular_tolerance_rad: self.angular_tolerance_rad,
            timeout_seconds: self.timeout_seconds,
            revision: self.revision.clone(),
            background: self.background,
            qualification: qualification::Requirements::default(),
        }
    }

    pub(super) fn validate(&self) -> Result<(), ToolError> {
        if self.source.is_some() == self.model.is_some()
            || self.model.as_ref().is_some_and(|model| {
                model.sha256.len() != 64
                    || !model.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    || !self.inputs.is_empty()
                    || !self.parameters.is_empty()
                    || self.revision.is_some()
            })
        {
            return Err(ToolError::Validation("Select exactly one CAD source or retained model; a model reference includes its SHA-256 and cannot override inputs or parameters".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for question in &self.questions {
            let valid_target = match question {
                Question::Features {
                    component_id,
                    selection,
                    ..
                }
                | Question::Section {
                    component_id,
                    selection,
                    ..
                }
                | Question::Validity {
                    component_id,
                    selection,
                    ..
                } => target_valid(component_id.as_deref(), selection.as_ref()),
                Question::Distance { first, second, .. }
                | Question::Alignment { first, second, .. } => {
                    target_valid(first.component_id.as_deref(), first.selection.as_ref())
                        && target_valid(second.component_id.as_deref(), second.selection.as_ref())
                }
            };
            if !valid_target {
                return Err(ToolError::Validation("CAD measurement targets need one nonempty identity or valid selection; omit both to use the outer selection".into()));
            }
            let id = match question {
                Question::Features { id, limit, .. } => {
                    if *limit == 0 {
                        return Err(ToolError::Validation(
                            "CAD feature page size must be positive".into(),
                        ));
                    }
                    id
                }
                Question::Section {
                    id,
                    origin_mm,
                    normal,
                    ..
                } => {
                    if origin_mm.iter().chain(normal).any(|v| !v.is_finite())
                        || normal.iter().all(|v| *v == 0.0)
                    {
                        return Err(ToolError::Validation(
                            "CAD section needs a finite origin and nonzero finite normal".into(),
                        ));
                    }
                    id
                }
                Question::Distance { id, .. }
                | Question::Alignment { id, .. }
                | Question::Validity { id, .. } => id,
            };
            if id.is_empty() || !ids.insert(id) {
                return Err(ToolError::Validation(
                    "CAD question identifiers must be nonempty and unique".into(),
                ));
            }
        }
        if self.limit == 0 || !self.selection.valid() {
            return Err(ToolError::Validation(
                "CAD inspection needs a positive page size, nonempty selectors, and finite ordered region bounds".into(),
            ));
        }
        Ok(())
    }
}

fn target_valid(identity: Option<&str>, selection: Option<&Selection>) -> bool {
    !(identity.is_some() && selection.is_some())
        && identity.is_none_or(|id| !id.is_empty())
        && selection.is_none_or(Selection::valid)
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// Exact occurrence paths returned by a previous inspection of this source.
    #[serde(default)]
    pub component_ids: Vec<String>,
    /// Case-insensitive substring of occurrence, product, or assembly path names.
    pub name_contains: Option<String>,
    /// Select native component bounds intersecting this assembly-coordinate region.
    pub region_mm: Option<Region>,
}

impl Selection {
    fn valid(&self) -> bool {
        !self.component_ids.iter().any(|id| id.is_empty())
            && !self
                .name_contains
                .as_ref()
                .is_some_and(|name| name.trim().is_empty())
            && !self.region_mm.as_ref().is_some_and(|region| {
                (0..3).any(|axis| {
                    !region.min[axis].is_finite()
                        || !region.max[axis].is_finite()
                        || region.min[axis] > region.max[axis]
                })
            })
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Frame {
    #[default]
    Assembly,
    Component,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
    pub size: [f64; 3],
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Component {
    pub id: String,
    pub parent_id: Option<String>,
    pub source_name: String,
    pub occurrence_name: Option<String>,
    pub product_name: Option<String>,
    /// Placement from component coordinates into assembly coordinates, in mm.
    pub transform: [[f64; 4]; 3],
    pub bounds_mm: Bounds,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Report {
    pub cadquery_version: String,
    pub units: String,
    pub frame: Frame,
    pub bounds_method: String,
    pub component_count: usize,
    pub matched_count: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub components: Vec<Component>,
    /// Requested occurrence identifiers absent from the imported source.
    pub missing_component_ids: Vec<String>,
    #[serde(default)]
    pub answers: Vec<Answer>,
    #[serde(default)]
    pub exports: Vec<ExportOutcome>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Answer {
    pub id: String,
    pub kind: QuestionKind,
    pub status: AnswerStatus,
    pub frame: Frame,
    pub method: String,
    pub component_ids: Vec<String>,
    /// Kind-specific native evidence; never manufacturing qualification.
    pub evidence: Option<Evidence>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    Measured,
    Unavailable,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    Features,
    Distance,
    Alignment,
    Section,
    Validity,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Evidence {
    Features {
        features: Vec<Feature>,
        matched_count: usize,
        next_offset: Option<usize>,
    },
    Distance {
        distance_mm: f64,
        interpretation: String,
    },
    Alignment {
        angle_degrees: f64,
        first: Feature,
        second: Feature,
        interpretation: String,
    },
    Section {
        edge_count: usize,
        length_mm: f64,
        bounds_mm: Option<Bounds>,
        origin_mm: [f64; 3],
        normal: [f64; 3],
    },
    Validity {
        valid: bool,
        solid_count: usize,
        interpretation: String,
    },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Feature {
    pub face_index: usize,
    pub kind: FeatureKind,
    pub origin_mm: [f64; 3],
    pub direction: [f64; 3],
    pub area_mm2: f64,
    pub radius_mm: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    Plane,
    Cylinder,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExportOutcome {
    pub format: Format,
    pub status: ExportStatus,
    pub filename: Option<String>,
    pub error: Option<String>,
    pub component_count: usize,
    /// Complete selected identifiers in the retained inventory, using a JSON pointer.
    pub component_scope: String,
    pub frame: Frame,
    pub units: String,
    pub representation: String,
    pub linear_tolerance_mm: Option<f64>,
    pub angular_tolerance_rad: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportStatus {
    Completed,
    Failed,
    PublicationUnconfirmed,
}
