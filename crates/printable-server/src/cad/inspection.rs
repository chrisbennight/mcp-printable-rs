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
    pub source: String,
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
            source: self.source.clone(),
            output_dir: self.output_dir.clone(),
            parameters: self.parameters.clone(),
            inputs: self.inputs.clone(),
            linear_tolerance_mm: linear_tolerance(),
            angular_tolerance_rad: angular_tolerance(),
            timeout_seconds: self.timeout_seconds,
            revision: self.revision.clone(),
            background: self.background,
            qualification: qualification::Requirements::default(),
        }
    }

    pub(super) fn validate(&self) -> Result<(), ToolError> {
        if self.limit == 0
            || self.selection.component_ids.iter().any(|id| id.is_empty())
            || self
                .selection
                .name_contains
                .as_ref()
                .is_some_and(|name| name.trim().is_empty())
            || self.selection.region_mm.as_ref().is_some_and(|region| {
                (0..3).any(|axis| {
                    !region.min[axis].is_finite()
                        || !region.max[axis].is_finite()
                        || region.min[axis] > region.max[axis]
                })
            })
        {
            return Err(ToolError::Validation(
                "CAD inspection needs a positive page size, nonempty selectors, and finite ordered region bounds".into(),
            ));
        }
        Ok(())
    }
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
}
