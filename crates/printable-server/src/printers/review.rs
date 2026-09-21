use super::{
    PrinterService,
    materials::{Material, Unavailable},
    observations::{Activity, DIAGNOSTICS_NOTE, activity, now_ms, state_note},
};
use crate::error::ToolError;
use bambuddy_api::{BambuddyApi, job_types::FilamentRequirement};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewSource {
    Library { id: u64 },
    Archive { id: u64 },
    Queue { id: u64 },
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialRequirement {
    pub slot_id: u32,
    pub material_type: Option<String>,
    pub filament_id: Option<String>,
    pub product_name: Option<String>,
    pub brand: Option<String>,
    /// Explicit inventory subtype, such as transparent, matte, or reinforced.
    pub subtype: Option<String>,
    pub color: Option<String>,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewParams {
    pub source: ReviewSource,
    pub printer_id: Option<u64>,
    pub plate: Option<u16>,
    pub ams_mapping: Option<Vec<i32>>,
    pub use_ams: Option<bool>,
    pub bed_levelling: Option<bool>,
    pub flow_calibration: Option<bool>,
    pub vibration_calibration: Option<bool>,
    pub timelapse: Option<bool>,
    #[serde(default)]
    pub options: bambuddy_api::job_options::JobOptions,
    #[serde(default)]
    pub requirements: Vec<MaterialRequirement>,
}
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchResult {
    Match,
    Mismatch,
    Unknown,
}
#[derive(Debug, Serialize, JsonSchema)]
pub struct Finding {
    pub check: String,
    pub result: MatchResult,
    pub expected: Option<Value>,
    pub observed: Option<Value>,
    pub explanation: String,
}
#[derive(Debug, Serialize, JsonSchema)]
pub struct MaterialReview {
    pub slot_id: u32,
    pub mapping_id: Option<i32>,
    pub loaded: Option<Material>,
    pub slice_requirement: Option<FilamentRequirement>,
    pub findings: Vec<Finding>,
}
#[derive(Debug, Serialize, JsonSchema)]
pub struct PrintReview {
    pub source: ReviewSource,
    pub printer_id: u64,
    pub plate: u16,
    pub ams_mapping: Vec<i32>,
    pub use_ams: Option<bool>,
    pub findings: Vec<Finding>,
    pub materials: Vec<MaterialReview>,
    pub faults: Vec<bambuddy_api::PrinterFault>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics_note: Option<String>,
    pub effective_options: Option<EffectiveOptions>,
    pub fetched_at_unix_ms: u64,
    pub unavailable_sources: BTreeMap<String, Option<Unavailable>>,
    pub starts_printing: bool,
    pub reservation: bool,
    pub dispatch_note: String,
}
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct EffectiveOptions {
    pub bed_levelling: Option<bool>,
    pub flow_calibration: Option<bool>,
    pub vibration_calibration: Option<bool>,
    pub layer_inspect: Option<bool>,
    pub timelapse: Option<bool>,
    pub nozzle_offset_calibration: Option<bool>,
    pub preheat_override: Option<String>,
    pub preheat_chamber_target_override: Option<i64>,
    pub scheduled_time: Option<String>,
    pub require_previous_success: Option<bool>,
    pub manual_start: bool,
    pub skip_filament_check: Option<bool>,
    pub auto_off_after: Option<bool>,
}
fn compare(check: &str, expected: Option<&str>, observed: Option<&str>) -> Finding {
    let expected = expected.map(str::trim).filter(|s| !s.is_empty());
    let observed = observed.map(str::trim).filter(|s| !s.is_empty());
    Finding {
        check: check.into(),
        result: match (expected, observed) {
            (Some(a), Some(b)) if a.trim().eq_ignore_ascii_case(b.trim()) => MatchResult::Match,
            (Some(_), Some(_)) => MatchResult::Mismatch,
            _ => MatchResult::Unknown,
        },
        expected: expected.map(|s| json!(s)),
        observed: observed.map(|s| json!(s)),
        explanation: "Comparison uses reported metadata; missing values are unknown".into(),
    }
}
fn evidence(check: &str, result: MatchResult, observed: Value, explanation: &str) -> Finding {
    Finding {
        check: check.into(),
        result,
        expected: None,
        observed: Some(observed),
        explanation: explanation.into(),
    }
}
pub(super) fn canon(value: &str, models: &BTreeMap<String, String>) -> String {
    models
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(value))
        .map(|(_, v)| v.as_str())
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}
fn material_findings(request: &MaterialRequirement, material: Option<&Material>) -> Vec<Finding> {
    let detail = material.and_then(|m| m.detail.as_ref());
    let spool = detail
        .and_then(|d| d.inventory.as_ref())
        .and_then(|a| a.spool.as_ref());
    let mut findings = Vec::new();
    for (name, wanted, actual) in [
        (
            "material_type",
            request.material_type.as_deref(),
            material.and_then(|m| m.material_type.as_deref()),
        ),
        (
            "filament_id",
            request.filament_id.as_deref(),
            material.and_then(|m| m.filament_id.as_deref()),
        ),
        (
            "brand",
            request.brand.as_deref(),
            spool.and_then(|s| s.brand.as_deref()),
        ),
        (
            "subtype",
            request.subtype.as_deref(),
            spool.and_then(|s| s.subtype.as_deref()),
        ),
        (
            "color",
            request.color.as_deref(),
            material.and_then(|m| m.color.as_deref()),
        ),
    ] {
        if wanted.is_some() {
            let mut finding = compare(name, wanted, actual);
            if matches!(name, "brand" | "subtype")
                && material.is_some_and(|m| !m.conflicts.is_empty())
            {
                finding.result = MatchResult::Unknown;
                finding.explanation =
                    "Inventory identity is unresolved because product sources disagree".into();
            }
            findings.push(finding);
        }
    }
    if let Some(wanted) = &request.product_name {
        let names = [
            material.and_then(|m| m.product_name.as_deref()),
            detail.and_then(|d| d.catalog_name.as_deref()),
            detail
                .and_then(|d| d.preset.as_ref())
                .and_then(|p| p.preset_name.as_deref()),
            detail.and_then(|d| d.reported.tray_sub_brands.as_deref()),
            spool.and_then(|s| s.slicer_filament_name.as_deref()),
        ];
        let found = names
            .into_iter()
            .flatten()
            .find(|name| name.trim().eq_ignore_ascii_case(wanted.trim()));
        let mut finding = compare(
            "product_name",
            Some(wanted),
            found.or_else(|| material.and_then(|m| m.product_name.as_deref())),
        );
        if material.is_some_and(|m| !m.conflicts.is_empty()) {
            finding.result = MatchResult::Unknown;
            finding.explanation = "Product sources disagree; resolve the identity conflict".into();
        }
        findings.push(finding);
    }
    if material.is_some_and(|m| !m.conflicts.is_empty()) {
        findings.push(evidence(
            "identity_conflict",
            MatchResult::Unknown,
            json!(material.map(|m| &m.conflicts)),
            "Resolve contradictory material metadata before relying on exact identity",
        ));
    }
    findings
}

impl PrinterService {
    pub(super) async fn review(&self, p: ReviewParams) -> Result<Value, ToolError> {
        if p.plate == Some(0) || p.requirements.iter().any(|r| r.slot_id == 0) {
            return Err(ToolError::Validation(
                "plate and material slot IDs must be positive".into(),
            ));
        }
        if p.ams_mapping
            .as_ref()
            .is_some_and(|m| m.iter().any(|v| !(-1..=255).contains(v)))
        {
            return Err(ToolError::Validation("invalid material mapping ID".into()));
        }
        let (source_id, archive, job) = match &p.source {
            ReviewSource::Library { id } => (*id, false, None),
            ReviewSource::Archive { id } => (*id, true, None),
            ReviewSource::Queue { id } => {
                super::validate_id(*id)?;
                let job = self.read.print_job(*id).await?;
                let (id, archive) = job
                    .library_file_id
                    .map(|id| (id, false))
                    .or(job.archive_id.map(|id| (id, true)))
                    .ok_or_else(|| ToolError::Validation("queue item has no source".into()))?;
                (id, archive, Some(job))
            }
        };
        super::validate_id(source_id)?;
        let printer_id = p
            .printer_id
            .or(job.as_ref().and_then(|j| j.printer_id))
            .ok_or_else(|| ToolError::Validation("review requires a target printer".into()))?;
        super::validate_id(printer_id)?;
        let plate = p
            .plate
            .or(job.as_ref().and_then(|j| j.plate_id))
            .unwrap_or(1);
        let mapping = p
            .ams_mapping
            .clone()
            .or(job.as_ref().and_then(|j| j.ams_mapping.clone()))
            .unwrap_or_default();
        let use_ams = p.use_ams.or(job.as_ref().and_then(|j| j.use_ams));
        let (model, nozzle, bed_type, file_type) = if archive {
            let a = self.read.archive(source_id).await?;
            (a.sliced_for_model, a.nozzle_diameter, a.bed_type, None)
        } else {
            let f = self.read.library_file(source_id).await?;
            let meta = f.metadata.unwrap_or_default();
            (
                meta.sliced_for_model.or(f.sliced_for_model),
                meta.nozzle_diameter,
                meta.bed_type,
                Some(f.file_type),
            )
        };
        let (requirements, printers, models, plates) = tokio::join!(
            self.read.filament_requirements(source_id, archive, plate),
            self.read.printers(),
            self.read.printer_models(),
            self.read.plates(source_id, archive)
        );
        let plates = super::materials::Observation::from_result(plates);
        let selected_bed = plates
            .data
            .as_ref()
            .and_then(|p| p.plates.iter().find(|p| p.index == plate))
            .and_then(|p| p.bed_type.as_ref());
        let bed_type = selected_bed.cloned().or(bed_type);
        let bed_note = if selected_bed.is_some() {
            "Selected plate requirement; the API does not verify the physical plate installed"
        } else {
            "File-level metadata fallback; the selected plate's requirement and physical plate are unverified"
        };
        let printers = printers?;
        let target = printers
            .iter()
            .find(|p| p.id == printer_id)
            .ok_or_else(|| ToolError::Validation("printer not found".into()))?;
        let loaded = self.loaded_materials(printer_id, true).await?;
        let models = models.unwrap_or_default();
        let mut findings = vec![compare(
            "printer_model",
            model.as_deref().map(|s| canon(s, &models)).as_deref(),
            target
                .model
                .as_deref()
                .map(|s| canon(s, &models))
                .as_deref(),
        )];
        findings.push(compare(
            "source_format",
            Some("gcode.3mf"),
            file_type.as_deref(),
        ));
        findings.push(evidence(
            "selected_plate",
            match &plates.data {
                Some(p) if p.plates.iter().any(|p| p.index == plate) => MatchResult::Match,
                Some(_) => MatchResult::Mismatch,
                None => MatchResult::Unknown,
            },
            json!({"selected":plate,"available":plates.data.as_ref().map(|p|p.plates.iter().map(|p|p.index).collect::<Vec<_>>())}),
            "Selection compared with the source's reported plate list",
        ));
        findings.push(evidence(
            "printer_enabled",
            if target.is_active {
                MatchResult::Match
            } else {
                MatchResult::Mismatch
            },
            json!(target.is_active),
            "Bambuddy printer registration",
        ));
        findings.push(evidence(
            "connected",
            if loaded.status.connected {
                MatchResult::Match
            } else {
                MatchResult::Unknown
            },
            json!(loaded.status.connected),
            "Disconnected observations may be stale",
        ));
        findings.push(evidence(
            "printer_state",
            match activity(&loaded.status) {
                Activity::Idle => MatchResult::Match,
                Activity::Active => MatchResult::Mismatch,
                Activity::Unknown => MatchResult::Unknown,
            },
            json!({"state":loaded.status.state,"current_print":loaded.status.current_print}),
            state_note(&loaded.status),
        ));
        findings.push(evidence(
            "plate_clear",
            if loaded.status.awaiting_plate_clear {
                MatchResult::Mismatch
            } else {
                MatchResult::Unknown
            },
            json!(loaded.status.awaiting_plate_clear),
            "No pending acknowledgement is not physical proof of a clear plate",
        ));
        findings.push(evidence(
            "bed_type",
            MatchResult::Unknown,
            json!(bed_type),
            bed_note,
        ));
        if let Some(diameter) = nozzle {
            let actual = loaded
                .status
                .nozzles
                .iter()
                .map(|n| {
                    n.nozzle_diameter
                        .as_ref()
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|v| v.is_finite() && *v > 0.0)
                })
                .collect::<Vec<_>>();
            findings.push(evidence(
                "nozzle_diameter",
                if actual.is_empty() || actual.iter().any(Option::is_none) {
                    MatchResult::Unknown
                } else if actual
                    .iter()
                    .flatten()
                    .all(|n| (*n - diameter).abs() < 0.0001)
                {
                    MatchResult::Match
                } else if actual
                    .iter()
                    .flatten()
                    .all(|n| (*n - diameter).abs() >= 0.0001)
                {
                    MatchResult::Mismatch
                } else {
                    MatchResult::Unknown
                },
                json!({"required":diameter,"installed":actual}),
                "Mixed nozzle diameters require routing-specific confirmation",
            ));
        } else {
            findings.push(evidence(
                "nozzle_diameter",
                MatchResult::Unknown,
                json!({"required":null,"installed":loaded.status.nozzles.iter().map(|n| &n.nozzle_diameter).collect::<Vec<_>>()}),
                "The source does not report its nozzle diameter requirement",
            ));
        }
        let mut sources = loaded.sources;
        sources.insert("plates".into(), plates.unavailable);
        let requirements = super::materials::Observation::from_result(requirements);
        sources.insert("slice_requirements".into(), requirements.unavailable);
        let slice: Vec<_> = requirements
            .data
            .map(|r| r.filaments)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.used_in_plate != Some(false))
            .collect();
        if slice.is_empty() {
            findings.push(evidence(
                "slice_requirements",
                MatchResult::Unknown,
                Value::Null,
                "No per-plate requirements were available; material sufficiency is unknown",
            ));
        }
        let slots = slice
            .iter()
            .map(|r| r.slot_id)
            .chain(p.requirements.iter().map(|r| r.slot_id))
            .collect::<std::collections::BTreeSet<_>>();
        if use_ams == Some(false)
            && slots.iter().any(|slot| {
                mapping
                    .get(slot.saturating_sub(1) as usize)
                    .is_some_and(|id| (0..254).contains(id))
            })
        {
            findings.push(evidence("material_routing", MatchResult::Mismatch, json!(mapping),
                "AMS is disabled; retained AMS locations do not describe the external material in use"));
        }
        let mut required_by_mapping = BTreeMap::<i32, Option<f64>>::new();
        for req in &slice {
            if req.used_in_plate != Some(false)
                && let Some(id) = mapping.get(req.slot_id.saturating_sub(1) as usize)
            {
                let total = required_by_mapping.entry(*id).or_insert(Some(0.0));
                *total = total
                    .zip(req.used_grams.filter(|g| g.is_finite() && *g >= 0.0))
                    .map(|(total, grams)| total + grams)
                    .filter(|g| g.is_finite());
            }
        }
        let materials=slots.into_iter().map(|slot|{
            let mapped=mapping.get(slot.saturating_sub(1) as usize).copied()
                .filter(|id| use_ams != Some(false) || *id >= 254);
            let material=mapped.and_then(|m|loaded.materials.iter().find(|x|x.mapping_id.and_then(|id|i32::try_from(id).ok())==Some(m))).cloned();
            let req=slice.iter().find(|r|r.slot_id==slot).cloned();
            let mut findings=p.requirements.iter().filter(|r|r.slot_id==slot).flat_map(|r|material_findings(r,material.as_ref())).collect::<Vec<_>>();
            if let Some(r)=&req {
                findings.push(compare("slice_color",r.color.as_deref(),material.as_ref().and_then(|m|m.color.as_deref())));
                if let Some(polymer)=&r.material_type{findings.push(compare("slice_material",Some(polymer),material.as_ref().and_then(|m|m.material_type.as_deref())));}
                if let Some(filament)=r.tray_info_idx.as_ref().filter(|s|!s.is_empty()){findings.push(compare("slice_filament_id",Some(filament),material.as_ref().and_then(|m|m.filament_id.as_deref())));}
            }
            let grams=material.as_ref().and_then(|m|m.detail.as_ref()).and_then(|d|d.remaining_grams);
            let needed=mapped.and_then(|m|required_by_mapping.get(&m).copied().flatten());
            let stock=match(needed,grams){(Some(n),Some(g)) if g>=n=>MatchResult::Match,(Some(_),Some(_)) if loaded.status.ams_filament_backup!=Some(true)=>MatchResult::Mismatch,_=>MatchResult::Unknown};
            findings.push(evidence("material_quantity",stock,json!({"required_grams_at_location":needed,"remaining_grams":grams}),"Checks the selected location; firmware backup pools are not certified by this review"));
            MaterialReview{slot_id:slot,mapping_id:mapped,loaded:material,slice_requirement:req,findings}
        }).collect();
        let mut options = job
            .map(|j| EffectiveOptions {
                bed_levelling: j.bed_levelling,
                flow_calibration: j.flow_cali,
                vibration_calibration: j.vibration_cali,
                layer_inspect: j.layer_inspect,
                timelapse: j.timelapse,
                nozzle_offset_calibration: j.nozzle_offset_cali,
                preheat_override: j.preheat_override,
                preheat_chamber_target_override: j.preheat_chamber_target_override,
                scheduled_time: j.scheduled_time,
                require_previous_success: j.require_previous_success,
                manual_start: j.manual_start,
                skip_filament_check: j.skip_filament_check,
                auto_off_after: j.auto_off_after,
            })
            .unwrap_or(EffectiveOptions {
                bed_levelling: Some(true),
                flow_calibration: Some(true),
                vibration_calibration: Some(true),
                timelapse: Some(false),
                manual_start: true,
                ..Default::default()
            });
        options.bed_levelling = p.bed_levelling.or(options.bed_levelling);
        options.flow_calibration = p.flow_calibration.or(options.flow_calibration);
        options.vibration_calibration = p.vibration_calibration.or(options.vibration_calibration);
        options.timelapse = p.timelapse.or(options.timelapse);
        options.layer_inspect = p.options.layer_inspect.or(options.layer_inspect);
        options.nozzle_offset_calibration = p
            .options
            .nozzle_offset_cali
            .or(options.nozzle_offset_calibration);
        options.preheat_override = p
            .options
            .preheat_override
            .map(|v| {
                match v {
                    bambuddy_api::job_options::Preheat::Inherit => "inherit",
                    bambuddy_api::job_options::Preheat::On => "on",
                    bambuddy_api::job_options::Preheat::Off => "off",
                }
                .to_owned()
            })
            .or(options.preheat_override);
        options.preheat_chamber_target_override = p
            .options
            .preheat_chamber_target_override
            .map(i64::from)
            .or(options.preheat_chamber_target_override);
        if let Some(schedule) = p.options.scheduled_time {
            options.scheduled_time = schedule;
        }
        options.require_previous_success = p
            .options
            .require_previous_success
            .or(options.require_previous_success);
        let diagnostics_note =
            (!loaded.status.hms_errors.is_empty()).then(|| DIAGNOSTICS_NOTE.into());
        Ok(serde_json::to_value(PrintReview{source:p.source,printer_id,plate,ams_mapping:mapping,use_ams,findings,materials,faults:loaded.status.hms_errors,diagnostics_note,effective_options:Some(options),
            fetched_at_unix_ms:now_ms(),unavailable_sources:sources,starts_printing:false,reservation:false,
            dispatch_note:"Review is an observation, not a reservation. Bambuddy rechecks supported conditions at dispatch; absent stock data does not establish sufficiency.".into()})?)
    }
}
