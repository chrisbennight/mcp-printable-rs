//! Retained comparisons of completed slices, without another preparation lifecycle.
use super::{SliceHandle, SliceWorker, hash_file, slice_directory};
use crate::{
    error::ToolError,
    projects,
    provenance::{self, Kind, RecordRef},
};
use printable_workspace::{Snapshot, Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read},
    sync::Arc,
    time::{Duration, Instant},
};

const RECORD_BYTES: u64 = 4 * 1024 * 1024;
const INPUT_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    MinimumMaterialSupport,
    StrengthInUse,
    FinishNamedFaces,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub label: String,
    pub slice: SliceHandle,
    pub slice_provenance: RecordRef,
    pub rationale: String,
    #[serde(default)]
    pub reviews: Vec<RecordRef>,
    /// Declared calibration, conditioning and environment; not measurements.
    #[serde(default)]
    pub conditions: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareParams {
    pub project_id: String,
    pub output_dir: String,
    pub objective: Objective,
    /// Named requirements bound to the source; these are not mesh face selectors.
    #[serde(default)]
    pub critical_faces: Vec<String>,
    #[serde(default)]
    pub load_assumptions: Vec<String>,
    pub baseline: Candidate,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CandidateSummary {
    pub label: String,
    pub slice: SliceHandle,
    pub slice_provenance: RecordRef,
    pub source_sha256: String,
    pub settings_sha256: String,
    pub current_source: String,
    pub preflight_status: Value,
    pub final_layout_state: Value,
    pub compatibility: Vec<String>,
    pub estimates: Value,
    pub estimates_count: usize,
    pub estimates_inline_truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CompareResult {
    pub objective: Objective,
    pub baseline: CandidateSummary,
    pub candidates: Vec<CandidateSummary>,
    pub report_artifact: Value,
    pub report_sha256: String,
    pub provenance: RecordRef,
    pub physical_result: String,
    pub automatic_selection: String,
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

impl CompareParams {
    fn validate(&self) -> Result<(), ToolError> {
        if !(1..=8).contains(&self.candidates.len())
            || self.critical_faces.len() > 32
            || self.load_assumptions.len() > 32
            || self
                .critical_faces
                .iter()
                .chain(&self.load_assumptions)
                .any(|v| !text(v, 1000))
        {
            return Err(invalid(
                "comparison requires one to eight candidates and bounded named requirements",
            ));
        }
        let mut labels = BTreeSet::new();
        let mut handles = BTreeSet::new();
        for item in std::iter::once(&self.baseline).chain(&self.candidates) {
            if item.slice.project_id != self.project_id
                || !text(&item.label, 100)
                || !labels.insert(&item.label)
                || !handles.insert(&item.slice.output_dir)
                || !text(&item.rationale, 4000)
                || item.reviews.len() > 16
                || item.conditions.len() > 32
                || item
                    .conditions
                    .iter()
                    .any(|(key, value)| !text(key, 100) || !text(value, 1000))
            {
                return Err(invalid(
                    "comparison candidates require unique labels and slice handles in this project, rationale and bounded evidence",
                ));
            }
        }
        Ok(())
    }
}

struct Budget {
    remaining: u64,
    deadline: Instant,
}
impl Budget {
    fn record(
        &mut self,
        ws: &Workspace,
        reference: &RecordRef,
        kind: Kind,
        project: &str,
    ) -> Result<Value, ToolError> {
        self.check()?;
        self.remaining = self
            .remaining
            .checked_sub(RECORD_BYTES)
            .ok_or_else(|| invalid("comparison record byte budget exceeded"))?;
        let record = provenance::read_bounded(ws, reference, kind, project, RECORD_BYTES)?;
        self.check()?;
        Ok(record.data)
    }
    fn check(&self) -> Result<(), ToolError> {
        if Instant::now() >= self.deadline {
            Err(invalid("comparison processing deadline exceeded"))
        } else {
            Ok(())
        }
    }
    fn snapshot(&mut self, ws: &Workspace, path: &str) -> Result<Snapshot, ToolError> {
        self.check()?;
        if self.remaining == 0 {
            return Err(invalid("comparison input byte budget exceeded"));
        }
        let snapshot = ws.snapshot_artifact_bounded(path, self.remaining)?;
        self.remaining -= snapshot.meta().size_bytes;
        self.check()?;
        Ok(snapshot)
    }
    fn verify(
        &mut self,
        ws: &Workspace,
        path: &str,
        expected: &str,
    ) -> Result<Snapshot, ToolError> {
        let snapshot = self.snapshot(ws, path)?;
        if snapshot.meta().path != path || hash_file(snapshot.path())? != expected {
            return Err(invalid(
                "comparison evidence bytes differ from the retained identity",
            ));
        }
        self.check()?;
        Ok(snapshot)
    }
}

/// Native header strings retain their original units and ordered material slots.
/// Missing breakdowns are not inferred from toolpath lengths or material flags.
fn estimates(path: &std::path::Path, budget: &Budget) -> Result<Value, ToolError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let (mut time, mut grams) = (None, None);
    loop {
        budget.check()?;
        line.clear();
        let count = reader.by_ref().take(65537).read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > 65536 {
            return Err(invalid(
                "comparison toolpath line exceeds its processing bound",
            ));
        }
        for (marker, target) in [
            (
                b"; estimated printing time (normal mode) =".as_slice(),
                &mut time,
            ),
            (b"; filament used [g] =".as_slice(), &mut grams),
        ] {
            if let Some(value) = line.strip_prefix(marker) {
                let value = std::str::from_utf8(value)
                    .map_err(|_| invalid("native estimate is not UTF-8"))?
                    .trim();
                if value.len() > 256 {
                    return Err(invalid("native estimate exceeds its text bound"));
                }
                if target.as_ref().is_some_and(|old| old != value) {
                    return Err(invalid("conflicting native estimate headers"));
                }
                *target = Some(value.to_owned());
            }
        }
    }
    Ok(
        json!({"source":"verified_native_gcode_headers", "time_native":time,
        "filament_grams_by_slot_native":grams,"support_grams":null,"tower_grams":null,"flushing_grams":null,
        "breakdown_status":"not_available_from_supported_native_headers","physical_result":"not_observed"}),
    )
}

struct Loaded {
    summary: CandidateSummary,
    report: Value,
    data: Value,
}

fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    value[field]
        .as_str()
        .ok_or_else(|| invalid("retained slice omitted a required identity"))
}

fn load(
    ws: &Workspace,
    params: &CompareParams,
    item: &Candidate,
    budget: &mut Budget,
) -> Result<Loaded, ToolError> {
    budget.check()?;
    let data = budget.record(ws, &item.slice_provenance, Kind::Slice, &params.project_id)?;
    if data["slice"] != json!(item.slice) {
        return Err(invalid(
            "slice handle does not match its retained provenance",
        ));
    }
    let settings_sha256 = provenance::digest(&serde_json::to_vec(&data["settings"])?);
    if data["settings_sha256"] != settings_sha256 {
        return Err(invalid("retained slice settings identity does not match"));
    }
    let source_sha256 = required_text(&data, "source_sha256")?.to_owned();
    let output = slice_directory(ws, &params.project_id, &item.slice.output_dir)?;
    let request_source = required_text(&data["request"], "source")?;
    let extension = std::path::Path::new(request_source)
        .extension()
        .and_then(|v| v.to_str())
        .filter(|v| matches!(*v, "stl" | "3mf"))
        .ok_or_else(|| invalid("retained slice source type is unsupported"))?;
    let source = projects::resolve(ws, &params.project_id, request_source)?;
    if data["source"] != source {
        return Err(invalid(
            "retained slice source path does not match its request",
        ));
    }
    budget.verify(ws, &format!("{output}/source.{extension}"), &source_sha256)?;
    let current_source = match budget.snapshot(ws, &source) {
        Ok(snapshot) => {
            if hash_file(snapshot.path())? == source_sha256 {
                "matches_retained_source"
            } else {
                "changed_since_slice"
            }
        }
        Err(ToolError::Workspace(WsError::NotFound(_))) => "missing_since_slice",
        Err(error) => return Err(error),
    }
    .to_owned();
    let artifacts = data["artifacts"]
        .as_object()
        .filter(|a| a.len() <= 1024)
        .ok_or_else(|| invalid("retained slice omitted bounded artifacts"))?;
    if !artifacts.contains_key("model.gcode.3mf")
        || !artifacts.keys().any(|name| name.ends_with(".gcode"))
    {
        return Err(invalid(
            "comparison requires a native package and toolpaths from a completed slice",
        ));
    }
    let mut native_estimates = BTreeMap::new();
    for (name, artifact) in artifacts {
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err(invalid(
                "retained slice artifact name is not local to its output",
            ));
        }
        let path = format!("{output}/{name}");
        if artifact["artifact"]["path"] != path {
            return Err(invalid(
                "retained slice artifact path does not match its handle",
            ));
        }
        let snapshot = budget.verify(ws, &path, required_text(artifact, "sha256")?)?;
        if artifact["artifact"]["size_bytes"] != snapshot.meta().size_bytes {
            return Err(invalid("retained slice artifact size differs"));
        }
        if name.ends_with(".gcode") {
            native_estimates.insert(name.clone(), estimates(snapshot.path(), budget)?);
        }
    }
    for (field, filename) in [
        ("source_layout", "source-layout.json"),
        ("final_layout", "final-layout.json"),
    ] {
        let evidence = &data[field];
        if let Some(path) = evidence["report_artifact"]["path"].as_str() {
            if path != format!("{output}/{filename}") {
                return Err(invalid("layout evidence does not belong to its slice"));
            }
            budget.verify(ws, path, required_text(evidence, "report_sha256")?)?;
        }
    }
    let mut reviews = Vec::new();
    for reference in &item.reviews {
        budget.check()?;
        let review = budget.record(ws, reference, Kind::ToolpathReview, &params.project_id)?;
        if review["slice"] != json!(item.slice)
            || review["slice_provenance"] != json!(item.slice_provenance)
            || !artifacts.values().any(|a| {
                a["sha256"] == review["source_sha256"]
                    && a["artifact"]["path"] == review["toolpath"]
            })
        {
            return Err(invalid(
                "toolpath review is not bound to this exact slice and output",
            ));
        }
        let media = &review["image"];
        budget.verify(
            ws,
            required_text(&media["artifact"], "path")?,
            required_text(media, "sha256")?,
        )?;
        let mut evidence = json!({});
        for key in [
            "slice",
            "slice_provenance",
            "toolpath",
            "source_sha256",
            "image",
            "request",
            "kind",
            "projection",
            "units",
            "selected_layers",
            "layers",
            "segments",
            "features",
            "bounds_mm",
            "path_length_mm_by_feature",
            "legend_rgb",
            "estimates",
            "physical_result",
        ] {
            if let Some(value) = review.get(key) {
                evidence[key] = value.clone();
            }
        }
        if serde_json::to_vec(&evidence)?.len() > 64 * 1024 {
            return Err(invalid(
                "comparison review summary exceeds its byte budget; retain a narrower review",
            ));
        }
        reviews.push(json!({"record":reference,"evidence":evidence}));
    }
    let summary = CandidateSummary {
        label: item.label.clone(),
        slice: item.slice.clone(),
        slice_provenance: item.slice_provenance.clone(),
        source_sha256,
        settings_sha256,
        current_source,
        preflight_status: data["preflight"]["status"].clone(),
        final_layout_state: data["final_layout"]["state"].clone(),
        compatibility: Vec::new(),
        estimates: json!(native_estimates.iter().take(16).collect::<BTreeMap<_, _>>()),
        estimates_count: native_estimates.len(),
        estimates_inline_truncated: native_estimates.len() > 16,
    };
    let report = json!({"selection":item,"identity":summary,"source":source,"settings":data["settings"],
        "request":data["request"],"engine":data["engine"],"source_layout":data["source_layout"],"final_layout":data["final_layout"],
        "preflight":data["preflight"],"outputs":data["artifacts"],"reviews":reviews,"native_estimates":native_estimates,
        "settings_scope":"resolved_global_inputs; native per-object settings may override them",
        "conditions_scope":"caller_declared_assumptions; not physical verification"});
    Ok(Loaded {
        summary,
        report,
        data,
    })
}

fn compatibility(
    baseline: &Loaded,
    candidate: &Loaded,
    left: &Candidate,
    right: &Candidate,
) -> Vec<String> {
    let mut issues = Vec::new();
    for (a, b, reason) in [
        (
            &baseline.data["source_sha256"],
            &candidate.data["source_sha256"],
            "different_source_geometry_or_authored_settings",
        ),
        (
            &baseline.data["settings"]["printer"],
            &candidate.data["settings"]["printer"],
            "different_printer_or_nozzle",
        ),
        (
            &baseline.data["settings"]["filaments"],
            &candidate.data["settings"]["filaments"],
            "different_ordered_material_settings",
        ),
        (
            &baseline.data["settings"]["filament_colours"],
            &candidate.data["settings"]["filament_colours"],
            "different_material_slot_colours",
        ),
        (
            &baseline.data["settings"]["build_plate"],
            &candidate.data["settings"]["build_plate"],
            "different_build_surface",
        ),
        (
            &baseline.data["engine"],
            &candidate.data["engine"],
            "different_engine_identity",
        ),
        (
            &baseline.data["request"]["plate"],
            &candidate.data["request"]["plate"],
            "different_selected_plates",
        ),
    ] {
        if a.is_null() || b.is_null() {
            issues.push(format!("{reason}_identity_missing"));
        } else if a != b {
            issues.push(reason.to_owned());
        }
    }
    if left.conditions.is_empty() || right.conditions.is_empty() {
        issues.push("physical_conditions_unspecified".into());
    } else if left.conditions != right.conditions {
        issues.push("different_declared_physical_conditions".into());
    }
    if baseline.summary.current_source != "matches_retained_source"
        || candidate.summary.current_source != "matches_retained_source"
    {
        issues.push("current_authored_source_differs_from_historical_evidence".into());
    }
    for key in [
        "layer_height",
        "wall_loops",
        "top_shell_layers",
        "bottom_shell_layers",
        "top_shell_thickness",
        "bottom_shell_thickness",
        "line_width",
        "outer_wall_line_width",
        "inner_wall_line_width",
    ] {
        let a = &baseline.data["settings"]["process"][key];
        let b = &candidate.data["settings"]["process"][key];
        if a != b {
            issues.push(format!("shell_geometry_setting_changed:{key}"));
        }
    }
    if baseline.data["engine"]["source_revision"].is_null()
        || candidate.data["engine"]["source_revision"].is_null()
    {
        issues.push("engine_source_revision_unrecorded".into());
    }
    issues.push("physical_equivalence_not_verified".into());
    if candidate.data["preflight"]["status"] == "blocked" {
        issues.push("candidate_has_known_conflicts".into());
    } else if candidate.data["preflight"]["status"] != "modeled_checks_passed" {
        issues.push("candidate_clearance_scope_unresolved".into());
    }
    if right.reviews.is_empty() {
        issues.push("candidate_toolpath_reviews_missing".into());
    }
    issues
}

fn compare(ws: &Workspace, params: CompareParams) -> Result<CompareResult, ToolError> {
    params.validate()?;
    let output = slice_directory(ws, &params.project_id, &params.output_dir)?;
    let mut budget = Budget {
        remaining: INPUT_BYTES,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    let mut baseline = load(ws, &params, &params.baseline, &mut budget)?;
    let mut candidates = Vec::new();
    let mut reports = Vec::new();
    for item in &params.candidates {
        let mut candidate = load(ws, &params, item, &mut budget)?;
        candidate.summary.compatibility =
            compatibility(&baseline, &candidate, &params.baseline, item);
        candidate.report["identity"] = json!(candidate.summary);
        reports.push(candidate.report);
        candidates.push(candidate.summary);
    }
    baseline.summary.compatibility =
        vec!["reference_baseline; physical_result_not_verified".into()];
    baseline.report["identity"] = json!(baseline.summary);
    let report = json!({"format_version":1,"project_id":params.project_id,"objective":params.objective,
        "critical_faces":params.critical_faces,"load_assumptions":params.load_assumptions,
        "baseline":baseline.report,"candidates":reports,"automatic_selection":"not_performed",
        "strength_evidence":"stated_design_rationale_only; no strength_score or physical result inferred",
        "physical_result":"not_observed","missing_values":"unknown; never zero"});
    let bytes = serde_json::to_vec(&report)?;
    if bytes.len() > 48 * 1024 * 1024 {
        return Err(invalid("comparison report exceeds its byte budget"));
    }
    budget.check()?;
    if !ws.create_public_directory(&output)? {
        return Err(invalid(
            "comparison output directory exists; retain it and choose a new directory",
        ));
    }
    let report_artifact =
        ws.write_generated_bytes(&format!("{output}/comparison.json"), &bytes, false)?;
    let report_sha256 = provenance::digest(&bytes);
    let reference = provenance::store(
        ws,
        Kind::PreparationComparison,
        &params.project_id,
        json!({"report_artifact":report_artifact,"report_sha256":report_sha256,"request":params}),
    )?;
    Ok(CompareResult {
        objective: params.objective,
        baseline: baseline.summary,
        candidates,
        report_artifact: json!(report_artifact),
        report_sha256,
        provenance: reference,
        physical_result: "not_observed".into(),
        automatic_selection: "not_performed".into(),
    })
}

impl SliceWorker {
    pub(super) async fn compare(&self, params: CompareParams) -> Result<Value, ToolError> {
        params.validate()?;
        let admission = self
            .admit("slicer is busy; compare retained candidates after the active operation")
            .await?;
        let ws = Arc::clone(&self.workspace);
        let response = tokio::task::spawn_blocking(move || {
            let _admission = admission;
            compare(&ws, params)
        })
        .await
        .map_err(|_| invalid("comparison task failed"))??;
        Ok(serde_json::to_value(response)?)
    }
}
