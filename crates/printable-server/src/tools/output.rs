use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::compact_schema;

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// A design revision's identity.
fn revision_identity() -> Value {
    object(
        json!({"id": {"type": "string"}, "sha256": {"type": "string", "pattern": "^[a-f0-9]{64}$"}}),
        &["id", "sha256"],
    )
}

/// A revision's declared requirements, each with its status and the
/// evidence it was decided from.
fn declared_requirements() -> Value {
    object(
        json!({
            "scope": {"const": "declared_requirements"},
            "status": {"enum": ["passed", "failed", "incomplete"]},
            "criteria": {"type": "object", "additionalProperties": object(json!({
                "status": {"enum": ["passed", "failed", "unmeasured", "physical_test_required"]},
                "evidence": {"type": "string"}
            }), &["status"])}
        }),
        &["scope", "status", "criteria"],
    )
}

fn artifact() -> Value {
    object(
        json!({
            "path": {"type": "string"},
            "size_bytes": {"type": "integer", "minimum": 0},
            "media_type": {"type": "string"},
            "modified_ns": {"type": "integer"},
            "identity": {"const": "mutable_path", "description": "Metadata observation, not an immutable byte snapshot."},
            "project_id": {"type": "string"},
            "project_path": {"type": "string"}
        }),
        &["path", "size_bytes", "media_type"],
    )
}

fn photo_job() -> Value {
    let result = object(
        json!({
            "artifact":artifact(), "image_sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},
            "model":{"enum":["pixal3d","trellis2"]},
            "triangles":{"type":"integer","minimum":1},
            "generated_triangles":{"type":"integer","minimum":1},
            "peak_ram_bytes":{"type":"integer","minimum":0},
            "peak_gpu_memory_bytes":{"type":"integer","minimum":0},
            "seconds":{"type":"number","minimum":0}
        }),
        &["artifact", "image_sha256"],
    );
    let mut job = object(
        json!({
            "job_id":{"type":"string","pattern":"^[0-9a-f]{32}$"},
            "state":{"enum":["queued","running","succeeded","failed","cancelled","interrupted","outcome_unknown"]},
            "phase":{"enum":["queued","preparing_models","waiting_for_gpu","foreground","generating","delivering","publishing","finished"]},
            "model":{"enum":["pixal3d","trellis2"]}, "mesh_path":{"type":"string"},
            "created_unix_ms":{"type":"integer","minimum":0}, "updated_unix_ms":{"type":"integer","minimum":0},
            "cancellation_requested":{"type":"boolean"}, "cancellable":{"type":"boolean"},
            "result":{"anyOf":[result,{"type":"null"}]},
            "failure":{"anyOf":[object(json!({"code":{"type":"string"},"message":{"type":"string"}}), &["code","message"]),{"type":"null"}]},
            "request":{"type":"object"}, "execution":{"type":"object"},
            "image_sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}
        }),
        &[
            "job_id",
            "state",
            "phase",
            "model",
            "mesh_path",
            "created_unix_ms",
            "updated_unix_ms",
            "cancellation_requested",
            "cancellable",
            "result",
            "failure",
        ],
    );
    job["allOf"] = json!([{
        "if":{"properties":{"state":{"const":"succeeded"}},"required":["state"]},
        "then":{"properties":{"result":{"type":"object"}}}
    }]);
    job
}

fn publication_errors() -> Value {
    json!({"type":"array","items":object(json!({"artifact":{"type":"string"},"code":{"type":"string"},"message":{"type":"string"}}), &["artifact","code","message"])})
}

fn cad_inspection_completed() -> (Value, Map<String, Value>) {
    let mut report = serde_json::to_value(schemars::schema_for!(crate::cad::inspection::Report))
        .expect("CAD inspection schema serializes");
    let definitions = report
        .as_object_mut()
        .expect("report schema")
        .remove("$defs")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    (
        object(
            json!({
                "completion":{"enum":["completed","partial"]}, "scope":{"const":"cad_inspection"},
                "inspection":report, "build_directory":{"type":"string"},
                "model":object(json!({"path":{"type":"string"},"sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}}), &["path","sha256"]),
                "publication_errors":publication_errors(),
                "artifacts":{"type":"array","items":object(json!({
                    "artifact":artifact(), "sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}
                }), &["artifact","sha256"])}
            }),
            &[
                "completion",
                "scope",
                "inspection",
                "build_directory",
                "artifacts",
            ],
        ),
        definitions,
    )
}

/// Selected inspection results exclude unrelated delivery qualification contracts.
pub(crate) fn cad_action_schema(action: &str) -> Option<Value> {
    if !matches!(action, "inspect" | "export") {
        return None;
    }
    let (completed, definitions) = cad_inspection_completed();
    let job = object(
        json!({
            "build":object(json!({"project_id":{"type":"string"},"output_dir":{"type":"string"}}), &["project_id","output_dir"]),
            "status":{"enum":["admitted","running","completed","failed","cancelled","interrupted"]},
            "phase":{"enum":["input_snapshot","native_execution","terminal"]},
            "cancel_requested":{"type":"boolean"},"progress":{"type":"null"},
            "admitted_at_unix_ms":{"type":"integer"},"native_started_at_unix_ms":{"type":"integer"},
            "finished_at_unix_ms":{"type":"integer"},"execution_timeout_seconds":{"type":"integer"},
            "error":{"type":"string"},"result":completed,"publication_errors":publication_errors()
        }),
        &["build", "status", "phase"],
    );
    Some(with_errors(
        "cad_build",
        json!({"type":"object", "anyOf":[completed,job], "$defs":definitions}),
    ))
}

fn scene_state() -> Value {
    object(
        json!({
            "project_id": {"type": "string"},
            "generation": {"type": "string", "format": "uuid"},
            "revision": {"type": "integer", "minimum": 0, "maximum": 9007199254740991_u64}
        }),
        &["generation", "revision"],
    )
}

fn worker_readiness() -> Value {
    object(
        json!({
            "protocol_version": {"const": 1},
            "configured": {"type": "boolean"},
            "state": {"enum": ["not_configured", "ready", "busy", "unavailable", "incompatible"]},
            "engine": {"enum": ["CadQuery", "OrcaSlicer"]},
            "version": {"type": ["string", "null"]},
            "profile_counts": {"anyOf": [
                {"type": "null"},
                object(json!({"printer": {"type":"integer", "minimum": 0}, "process": {"type":"integer", "minimum": 0}, "filament": {"type":"integer", "minimum": 0}}), &["printer", "process", "filament"])
            ]}
        }),
        &[
            "protocol_version",
            "configured",
            "state",
            "engine",
            "version",
            "profile_counts",
        ],
    )
}

fn mesh_report() -> Value {
    let criterion = object(
        json!({
            "status": {"enum": ["passed", "failed", "unmeasured", "physical_test_required"]},
            "evidence": {"type": "string", "description": "JSON pointer relative to this validation report."}
        }),
        &["status"],
    );
    let criteria = [
        "solid_topology",
        "finite_dimensions",
        "support_free_orientation",
        "wall_thickness",
        "dimensional_requirements",
        "build_envelope",
        "material_process",
        "physical_performance",
    ];
    let properties: Map<String, Value> = criteria
        .iter()
        .map(|name| ((*name).to_owned(), criterion.clone()))
        .collect();
    let number = json!({"type": "number"});
    let point = json!({"type": "array", "items": number, "minItems": 3, "maxItems": 3});
    let thin_region = object(
        json!({
            "area_mm2": number,
            "minimum_thickness_mm": number,
            "thinnest_point_mm": point,
            "bounds": object(
                json!({"minimum_mm": point, "maximum_mm": point, "dimensions_mm": point}),
                &["minimum_mm", "maximum_mm", "dimensions_mm"]
            )
        }),
        &[
            "area_mm2",
            "minimum_thickness_mm",
            "thinnest_point_mm",
            "bounds",
        ],
    );
    let wall_thickness = object(
        json!({
            "minimum_wall_mm": number,
            "cone_half_angle_degrees": number,
            "rays_per_sample": {"type": "integer", "minimum": 1},
            "max_exit_deviation_degrees": number,
            "max_exit_obliquity_degrees": number,
            "sample_spacing_mm": number,
            "samples": {"type": "integer", "minimum": 0},
            "unmeasured_samples": {"type": "integer", "minimum": 0},
            "measured_area_share": number,
            "thickness_mm": object(
                json!({"minimum": number, "p5": number, "p50": number, "p95": number, "maximum": number}),
                &["minimum", "p5", "p50", "p95", "maximum"]
            ),
            "thin_area_mm2": number,
            "thin_region_count": {"type": "integer", "minimum": 0},
            "thin_regions": {"type": "array", "items": thin_region}
        }),
        &[
            "minimum_wall_mm",
            "sample_spacing_mm",
            "samples",
            "unmeasured_samples",
            "measured_area_share",
            "thin_area_mm2",
            "thin_region_count",
            "thin_regions",
        ],
    );
    let bounds = object(
        json!({"minimum_mm": point, "maximum_mm": point, "dimensions_mm": point}),
        &["minimum_mm", "maximum_mm", "dimensions_mm"],
    );
    let feature = object(
        json!({
            "name": {"type": "string"},
            "shape": {"enum": ["plane", "cylinder", "sphere"]},
            "seed_distance_mm": number,
            "unmeasured_reason": {"type": "string"},
            "fit": object(
                json!({
                    "faces": {"type": "integer", "minimum": 0},
                    "area_mm2": number,
                    "points": {"type": "integer", "minimum": 0},
                    "excluded_points": {"type": "integer", "minimum": 0},
                    "sigma_mm": number,
                    "deviation_mm": {"type": "array", "items": number, "minItems": 2, "maxItems": 2},
                    "form_error_mm": number,
                    "region": bounds,
                    "plane": object(json!({"point_mm": point, "normal": point}), &["point_mm", "normal"]),
                    "cylinder": object(
                        json!({
                            "axis_point_mm": point,
                            "axis_direction": point,
                            "radius_mm": number,
                            "diameter_mm": number,
                            "length_mm": number,
                            "arc_degrees": number
                        }),
                        &["axis_point_mm", "axis_direction", "radius_mm", "diameter_mm", "length_mm", "arc_degrees"]
                    ),
                    "sphere": object(
                        json!({"centre_mm": point, "radius_mm": number, "diameter_mm": number}),
                        &["centre_mm", "radius_mm", "diameter_mm"]
                    )
                }),
                &["faces", "area_mm2", "points", "excluded_points", "sigma_mm", "deviation_mm", "form_error_mm", "region"]
            )
        }),
        &["name", "shape"],
    );
    let requirement = object(
        json!({
            "measure": {"enum": ["diameter", "distance", "angle"]},
            "features": {"type": "array", "items": {"type": "string"}},
            "nominal": number,
            "tolerance": number,
            "status": {"enum": ["passed", "failed", "unmeasured"]},
            "measured": number,
            "range": {"type": "array", "items": number, "minItems": 2, "maxItems": 2},
            "nominal_far_from_measured": {"type": "boolean"}
        }),
        &["measure", "features", "nominal", "tolerance", "status"],
    );
    let deviation_region = object(
        json!({
            "area_mm2": number,
            "extreme_mm": number,
            "extreme_point_mm": point,
            "centre_mm": point,
            "bounds": bounds
        }),
        &[
            "area_mm2",
            "extreme_mm",
            "extreme_point_mm",
            "centre_mm",
            "bounds",
        ],
    );
    let volume_region = object(
        json!({
            "material": {"enum": ["extra", "missing"]},
            "volume_mm3": number,
            "centre_mm": point,
            "bounds": bounds
        }),
        &["material", "volume_mm3", "centre_mm", "bounds"],
    );
    let volume = object(
        json!({
            "cell_mm": number,
            "cells": {"type": "integer", "minimum": 1},
            "reference_volume_mm3": number,
            "extra_volume_mm3": number,
            "missing_volume_mm3": number,
            "region_count": {"type": "integer", "minimum": 0},
            "regions": {"type": "array", "items": volume_region}
        }),
        &[
            "cell_mm",
            "cells",
            "reference_volume_mm3",
            "extra_volume_mm3",
            "missing_volume_mm3",
            "region_count",
            "regions",
        ],
    );
    let comparison = object(
        json!({
            "alignment": object(
                json!({
                    "method": {"enum": ["none", "best_fit"]},
                    "rotation": {"type": "array", "items": point, "minItems": 3, "maxItems": 3},
                    "translation_mm": point,
                    "rotation_degrees": number,
                    "fit_points": {"type": "integer", "minimum": 0},
                    "matched_points": {"type": "integer", "minimum": 0},
                    "rms_mm": number,
                    "start_pairs_rms_mm": number
                }),
                &["method", "rotation", "translation_mm", "rotation_degrees"]
            ),
            "tolerance_mm": number,
            "max_distance_mm": number,
            "sample_spacing_mm": number,
            "samples": {"type": "integer", "minimum": 0},
            "unmeasured_samples": {"type": "integer", "minimum": 0},
            "measured_area_share": number,
            "deviation_mm": object(
                json!({"minimum": number, "p5": number, "p50": number, "p95": number, "maximum": number, "mean": number, "rms": number}),
                &["minimum", "p5", "p50", "p95", "maximum", "mean", "rms"]
            ),
            "within_tolerance_share": number,
            "region_count": {"type": "integer", "minimum": 0},
            "regions": {"type": "array", "items": deviation_region},
            "volume": volume
        }),
        &[
            "alignment",
            "tolerance_mm",
            "max_distance_mm",
            "sample_spacing_mm",
            "samples",
            "unmeasured_samples",
            "measured_area_share",
            "within_tolerance_share",
            "region_count",
            "regions",
        ],
    );
    let dimensions = object(
        json!({
            "features": {"type": "array", "items": feature},
            "requirements": {"type": "array", "items": requirement}
        }),
        &["features", "requirements"],
    );
    let faces = object(
        json!({"faces": {"type": "integer", "minimum": 0}, "area_mm2": number}),
        &["faces", "area_mm2"],
    );
    let patch = object(
        json!({
            "area_mm2": number,
            "faces": {"type": "integer", "minimum": 1},
            "minimum_draft_degrees": number,
            "mean_draft_degrees": number,
            "centre_mm": point,
            "bounds": object(
                json!({"minimum_mm": point, "maximum_mm": point, "dimensions_mm": point}),
                &["minimum_mm", "maximum_mm", "dimensions_mm"]
            )
        }),
        &[
            "area_mm2",
            "faces",
            "minimum_draft_degrees",
            "mean_draft_degrees",
            "centre_mm",
            "bounds",
        ],
    );
    let draft = object(
        json!({
            "pull_direction": point,
            "minimum_draft_degrees": number,
            "parting_offset_mm": number,
            "base": faces,
            "released": faces,
            "shallow": faces,
            "undercut": faces,
            "shallow_patch_count": {"type": "integer", "minimum": 0},
            "shallow_patches": {"type": "array", "items": patch},
            "undercut_patch_count": {"type": "integer", "minimum": 0},
            "undercut_patches": {"type": "array", "items": patch}
        }),
        &[
            "pull_direction",
            "minimum_draft_degrees",
            "base",
            "released",
            "shallow",
            "undercut",
            "shallow_patch_count",
            "shallow_patches",
            "undercut_patch_count",
            "undercut_patches",
        ],
    );
    object(
        json!({
            "wall_thickness": wall_thickness,
            "dimensions": dimensions,
            "comparison": comparison,
            "draft": draft,
            "solid_geometry": {"type": "boolean"},
            "printable": {"type": "boolean", "deprecated": true,
                "description": "Compatibility alias for solid_geometry only. Does not establish manufacturing qualification."},
            "assessment": object(json!({
                "scope": {"const": "mesh_geometry"},
                "status": {"enum": ["failed", "incomplete"]},
                "criteria": object(Value::Object(properties), &criteria)
            }), &["scope", "status", "criteria"])
        }),
        &["solid_geometry", "printable", "assessment"],
    )
}

fn selected_bundle() -> Value {
    let digest = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    let file = object(
        json!({
            "path":{"type":"string"},
            "size_bytes":{"type":"integer","minimum":0},
            "media_type":{"type":"string"},
            "sha256":digest
        }),
        &["path", "size_bytes", "media_type", "sha256"],
    );
    let manifest = object(
        json!({
            "format_version":{"const":1},
            "project_id":{"type":"string"},
            "scope":{"const":"selected_files"},
            "dependencies_inspected":{"const":false},
            "files":{"type":"array","items":file,"minItems":1}
        }),
        &[
            "format_version",
            "project_id",
            "scope",
            "dependencies_inspected",
            "files",
        ],
    );
    object(
        json!({"artifact":artifact(),"sha256":digest,"manifest":manifest}),
        &["artifact", "sha256", "manifest"],
    )
}

fn native_bundle() -> Value {
    let digest = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    let file = object(
        json!({"path":{"type":"string"},"size_bytes":{"type":"integer","minimum":0},"sha256":digest}),
        &["path", "size_bytes", "sha256"],
    );
    let preparation = object(
        json!({
            "engine":object(json!({"name":{"const":"blender"},"version":{"type":"string"}}), &["name","version"]),
            "entrypoint":{"type":"string"},
            "prepared_libraries":{"type":"integer","minimum":0},
            "registered_external_files":{"const":0},
            "units":object(json!({"system":{"type":"string"},"length_unit":{"type":"string"},"scale_length":{"type":"number","exclusiveMinimum":0}}), &["system","length_unit","scale_length"]),
            "limitations":{"type":"array","items":{"type":"string"},"minItems":1}
        }),
        &[
            "engine",
            "entrypoint",
            "prepared_libraries",
            "registered_external_files",
            "units",
            "limitations",
        ],
    );
    let manifest = object(
        json!({
            "format_version":{"const":1},"project_id":{"type":"string"},
            "scope":{"const":"native_blender"},"entrypoint":{"type":"string"},
            "files":{"type":"array","items":file,"minItems":2},
            "preparation":preparation
        }),
        &[
            "format_version",
            "project_id",
            "scope",
            "entrypoint",
            "files",
            "preparation",
        ],
    );
    object(
        json!({"artifact":artifact(),"sha256":digest,"manifest":manifest}),
        &["artifact", "sha256", "manifest"],
    )
}

/// A typed result's schema with the shared execution-failure contract.
fn typed<T: schemars::JsonSchema>(name: &str) -> Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(T)).expect("a result schema serializes");
    schema["type"] = json!("object");
    with_errors(name, schema)
}

/// One action's own output contract, when it has one. Other actions are
/// described by their tool's [`schema`].
pub fn action_schema(name: &str, action: &str) -> Option<Value> {
    use crate::projects::{Project, card::Card, catalog::SearchView, index};
    let mut schema = match (name, action) {
        ("printer" | "print", _) => crate::printers::action_output_schema(name, action)?,
        ("cad_build", _) => cad_action_schema(action)?,
        ("catalog", "search") => typed::<SearchView>(name),
        ("catalog", "card") => typed::<Card>(name),
        ("catalog", "status") => typed::<index::IndexStatus>(name),
        ("project", "update" | "cover") => typed::<Project>(name),
        ("project", "reindex") => typed::<index::PassReport>(name),
        _ => return None,
    };
    compact_schema(&mut schema);
    Some(schema)
}

/// Add the shared execution-failure contract without changing success shapes.
/// Definitions stay at the document root so existing local references resolve.
pub(crate) fn with_errors(name: &str, mut success: Value) -> Value {
    let mut code = json!({"type": "string", "minLength": 1});
    if matches!(name, "printer" | "print") {
        // Known printer rejections must use the existing typed details schema.
        code["not"] = json!({"const": "printer_rejected"});
    }
    let failure = object(
        json!({
            "tool": {"const": name},
            "error": object(json!({
                "code": code,
                "message": {"type": "string"},
                "scene_state": scene_state()
            }), &["code", "message"])
        }),
        &["tool", "error"],
    );
    let root = success
        .as_object_mut()
        .expect("success schema is an object");
    let definitions = root.remove("$defs");
    let dialect = root.remove("$schema");
    let mut result = json!({"type": "object", "anyOf": [success, failure]});
    if let Some(definitions) = definitions {
        result["$defs"] = definitions;
    }
    if let Some(dialect) = dialect {
        result["$schema"] = dialect;
    }
    result
}

pub fn schema(name: &str) -> Arc<Map<String, Value>> {
    let value = match name {
        "skill" => {
            let mut schema =
                serde_json::to_value(schemars::schema_for!(crate::resources::skills::SkillResult))
                    .expect("skill output schema serializes");
            schema["type"] = json!("object");
            schema
        }
        "slice" => json!({"type":"object","anyOf":[
            {"required":["profile","settings","build_plates","total","next_offset"],"properties":{"profile":{"type":"string"},"settings":{"type":"array"},"build_plates":{"type":"array"},"total":{"type":"integer"}}},
            {"required":["kind","source_sha256","image","metadata_path"],"properties":{"kind":{"const":"actual_toolpath"},"source_sha256":{"type":"string"},"image":{"type":"object"},"metadata_path":{"type":"string"},"provenance":schemars::schema_for!(crate::provenance::RecordRef)}},
            {"required":["profiles","total","next_offset"],"properties":{"profiles":{"type":"array"},"total":{"type":"integer"}}},
            {"required":["id","slice","status"],"properties":{"id":{"type":"string"},"slice":{"type":"object"},"status":{"enum":["running","completed","failed","cancelled","interrupted"]},"provenance":schemars::schema_for!(crate::provenance::RecordRef),
                "finished_unix_ms":{"type":"integer","minimum":0},"summary":{"type":"object"}}}
        ]}),
        "printer" | "print" => crate::printers::output_schema(name),
        "status" => object(
            json!({
                "server_version": {"type": "string"}, "transport": {"const": "streamable-http"},
                "blender": {"type": "object", "properties": {
                    "available": {"type": "boolean"},
                    "state": {"enum": ["ready", "busy", "unavailable"]},
                    "scene_state": {"anyOf": [scene_state(), {"type": "null"}]},
                    "native_observation": {"type": "object"}
                }, "required": ["available"]},
                "openscad": {"type": "object"}, "workspace": {"type": "object"},
                "render_jobs": {"type": "object"},
                "cad": worker_readiness(), "slicer": worker_readiness(),
                "printers": {"type": "object", "properties": {"configured": {"type": "boolean"}}, "required": ["configured"]}
            }),
            &[
                "server_version",
                "transport",
                "blender",
                "openscad",
                "workspace",
                "render_jobs",
                "cad",
                "slicer",
            ],
        ),
        "inspect" => object(
            json!({
                "scene_state": scene_state(), "name": {"type": "string"},
                "section": {"type": "string"}, "items": {"type": "array"},
                "objects": {"type": "array"}, "total": {"type": "integer", "minimum": 0},
                "next_offset": {"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}]},
                "mode": {"type": "string"}
            }),
            &["scene_state"],
        ),
        "head" => json!({"type": "object", "anyOf": [
            {"required": ["reconstruct", "print"], "properties": {
                "reconstruct": {"type": "object", "required": ["variant", "identity", "expression", "pose", "displacement"]},
                "print": {"type": "object", "required": ["artifact", "bust", "validation"]},
                "blender": {"type": "object", "required": ["scene_state", "name", "head"]}
            }},
            {"required": ["models", "skipped"], "properties": {
                "models": {"type": "array"}, "skipped": {"type": "array"}
            }},
            {"required": ["scene_state", "name", "head"], "properties": {
                "scene_state": scene_state(), "name": {"type": "string"},
                "vertices": {"type": "integer"}, "polygons": {"type": "integer"},
                "head": {"type": "object", "required": ["variant", "units", "joints"], "properties": {
                    "variant": {"type": "string"}, "units": {"const": "millimetres"},
                    "joints": {"type": "object"}
                }}
            }},
            {"required": ["artifact", "units", "variant", "vertices", "triangles", "texture_coordinates", "head"], "properties": {
                "artifact": artifact(), "units": {"const": "millimetres"},
                "variant": {"type": "string"},
                "vertices": {"type": "integer", "minimum": 1},
                "triangles": {"type": "integer", "minimum": 1},
                "texture_coordinates": {"type": "boolean"},
                "head": {"type": "object"}
            }},
            {"required": ["artifact", "units", "variant", "bust", "validation"], "properties": {
                "artifact": artifact(), "units": {"const": "millimetres"},
                "variant": {"type": "string"}, "bust": {"type": "object"},
                "validation": mesh_report(),
                "printer": {"type": "object", "required": ["process", "layer_height_mm", "maximum_overhang_degrees"]},
                "warnings": {"type": "array", "items": {"type": "string"}}
            }},
            {"required": ["variant", "identity", "expression", "pose", "displacement"], "properties": {
                "variant": {"type": "string"},
                "identity": {"type": "array", "items": {"type": "number"}},
                "expression": {"type": "array", "items": {"type": "number"}},
                "pose": {"type": "object", "required": ["jaw"]},
                "displacement": {"type": "object", "required": ["artifact", "offset_mm", "scale_mm"], "properties": {
                    "artifact": artifact(), "offset_mm": {"type": "number"},
                    "scale_mm": {"type": "number"}
                }},
                "identity_agreement": {"type": "array", "items": {"type": "number"}},
                "clamped_coefficients": {"type": "integer", "minimum": 0},
                "texture": {"type": "object", "required": ["artifact", "width", "height", "seen"], "properties": {
                    "artifact": artifact(), "width": {"type": "integer", "minimum": 2},
                    "height": {"type": "integer", "minimum": 2},
                    "seen": {"type": "number", "minimum": 0, "maximum": 1}
                }}
            }}
        ]}),
        "edit" | "scene" => object(
            json!({
                "scene_state": scene_state(), "name": {"type": "string"},
                "path": {"type": "string"}
            }),
            &["scene_state"],
        ),
        "blender_execute" => object(
            json!({
                "scene_state": scene_state(), "result": {},
                "stdout": {"type": "string"}, "stderr": {"type": "string"},
                "stdout_truncated": {"type": "boolean"}, "stderr_truncated": {"type": "boolean"},
                "elapsed_ms": {"type": "integer", "minimum": 0},
                "context_before": {"type": "object"}, "context_after": {"type": "object"}
            }),
            &[
                "scene_state",
                "result",
                "stdout",
                "stderr",
                "stdout_truncated",
                "stderr_truncated",
                "elapsed_ms",
            ],
        ),
        "cad_build" => {
            let mut qualification = serde_json::to_value(schemars::schema_for!(
                crate::cad::qualification::Qualification
            ))
            .expect("CAD qualification schema serializes");
            let mut definitions = qualification
                .as_object_mut()
                .expect("schema object")
                .remove("$defs");
            let (inspection, inspection_definitions) = cad_inspection_completed();
            definitions
                .get_or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("qualification definitions")
                .extend(inspection_definitions);
            let completed = object(
                json!({
                    "completion": {"const":"completed"},
                    "qualification": qualification,
                    "build_directory": {"type": "string"},
                "revision": revision_identity(),
                "measurement": object(json!({"path":{"type":"string"},"sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}}), &["path","sha256"]),
                "requirements": declared_requirements(),
                    "report": {"type": "object"},
                    "artifacts": {"type": "array", "items": object(json!({
                        "artifact": artifact(), "sha256": {"type": "string", "pattern": "^[a-f0-9]{64}$"}
                    }), &["artifact", "sha256"])}
                }),
                &[
                    "completion",
                    "qualification",
                    "build_directory",
                    "report",
                    "artifacts",
                ],
            );
            let mut historical = completed.clone();
            historical["required"] = json!(["build_directory", "report", "artifacts"]);
            historical["not"] =
                json!({"anyOf":[{"required":["completion"]},{"required":["qualification"]}]});
            let mut result = json!({"type":"object","anyOf":[completed, inspection, object(json!({
                "build":object(json!({"project_id":{"type":"string"},"output_dir":{"type":"string"}}), &["project_id","output_dir"]),
                "status":{"enum":["admitted","running","completed","failed","cancelled","interrupted"]},
                "phase":{"enum":["input_snapshot","native_execution","terminal"]},
                "cancel_requested":{"type":"boolean"},"progress":{"type":"null"},
                "admitted_at_unix_ms":{"type":"integer"},"native_started_at_unix_ms":{"type":"integer"},"finished_at_unix_ms":{"type":"integer"},
                "execution_timeout_seconds":{"type":"integer"},"error":{"type":"string"},"result":{"anyOf":[completed,historical,inspection]},
                "native_stage":{"type":"string"},
                "progress_error":object(json!({"code":{"type":"string"},"message":{"type":"string"}}), &["code","message"]),
                "publication_errors":publication_errors(),
                "available_artifacts":{"type":"array","items":object(json!({"artifact":artifact(),"sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}}), &["artifact","sha256"])}
            }), &["build","status","phase"])]});
            if let Some(definitions) = definitions {
                result["$defs"] = definitions;
            }
            result
        }
        "scad_build" => object(
            json!({
                "artifact": artifact(), "diagnostics": {"type": "object"},
                "validation": mesh_report(), "manufacturing_evidence": {"type": "object"}
            }),
            &["artifact", "diagnostics"],
        ),
        "view" | "render" | "compare_renders" => object(
            json!({
                "path": {"type": "string"}, "size_bytes": {"type": "integer", "minimum": 1},
                "media_type": {"const": "image/png"},
                "width": {"type": "integer", "minimum": 1}, "height": {"type": "integer", "minimum": 1},
                "scene_state": scene_state(), "source": {"type": "object"},
                "views": {"type": "array"}, "analysis": {"type": "object"},
                "observation": {"type": "object"}, "presentation": {"type": "object"}
            }),
            &["path", "size_bytes", "media_type", "width", "height"],
        ),
        "validate_mesh" => object(
            json!({
                "artifact": artifact(), "reference_artifact": artifact(),
                "units": {"const": "millimetres"}, "report": mesh_report(),
                "revision": revision_identity(), "requirements": declared_requirements(),
                "requirement_evidence": {"type": "object"}
            }),
            &["artifact", "units", "report"],
        ),
        "analyze_assembly" => object(
            json!({
                "fixed_artifact": artifact(), "moving_artifact": artifact(),
                "units": {"const": "millimetres"}, "report": {"type": "object"}
            }),
            &["fixed_artifact", "moving_artifact", "units", "report"],
        ),
        "job" => json!({"type": "object", "anyOf": [
            object(json!({
                "job_id": {"type": "string"},
                "state": {"enum": ["queued", "running", "succeeded", "failed", "cancelled"]},
                "progress": {"type": "object"},
                "failure": {"anyOf": [{"type": "object"}, {"type": "null"}]},
                "execution": {"type": "object"}, "spec": {"type": "object"},
                "mechanical_analysis": {"anyOf": [{"type": "object"}, {"type": "null"}]}
            }), &["job_id", "state"]),
            object(json!({"jobs": {"type": "array"}, "next_offset": {
                "anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}]}
            }), &["jobs", "next_offset"])
        ]}),
        "catalog" => {
            let mut generator = schemars::SchemaGenerator::default();
            let search = generator.subschema_for::<crate::projects::catalog::SearchView>();
            let card = generator.subschema_for::<crate::projects::card::Card>();
            let status = generator.subschema_for::<crate::projects::index::IndexStatus>();
            json!({"type":"object","anyOf":[search, card, status],
                "$defs":generator.take_definitions(true)})
        }
        "project" => {
            let mut generator = schemars::SchemaGenerator::default();
            let revision = generator.subschema_for::<crate::projects::revisions::Response>();
            let project = generator.subschema_for::<crate::projects::Project>();
            let pass = generator.subschema_for::<crate::projects::index::PassReport>();
            json!({"type":"object","anyOf":[
            revision,
            project,
            pass,
            object(json!({"projects":{"type":"array","items":{"type":"object"}},"limit_reached":{"type":"boolean"}}), &["projects","limit_reached"]),
            object(json!({"project_id":{"type":"string"},"path":{"type":"string"}}), &["project_id","path"]),
            object(json!({"project_id":{"type":"string"},"entries":{"type":"array","items":artifact()},"limit_reached":{"type":"boolean"}}), &["project_id","entries","limit_reached"]),
            selected_bundle(), native_bundle()
        ],"$defs":generator.take_definitions(true)})
        }
        "library" => {
            let mut generator = schemars::SchemaGenerator::default();
            let page = generator.subschema_for::<model_library::SearchPage>();
            let detail = generator.subschema_for::<model_library::ModelDetail>();
            let digest = json!({"type": "string", "pattern": "^[0-9a-f]{64}$"});
            let saved = object(
                json!({
                    "file_id": {"type": "string"}, "name": {"type": "string"},
                    "artifact": artifact(), "sha256": digest
                }),
                &["file_id", "name", "artifact", "sha256"],
            );
            let download = object(
                json!({
                    "site": {"type": "string"}, "model_id": {"type": "string"},
                    "title": {"type": "string"}, "url": {"type": "string"},
                    "designer": {"type": ["string", "null"]},
                    "license": {"anyOf": [{"type": "object"}, {"type": "null"}]},
                    "files": {"type": "array", "items": saved, "minItems": 1},
                    "manifest": artifact()
                }),
                &["site", "model_id", "title", "url", "files", "manifest"],
            );
            let sign_in = object(
                json!({
                    "site": {"const": "makerworld"},
                    "state": {"enum": ["signed_in", "code_required"]},
                    "expires_unix_ms": {"type": ["integer", "null"], "minimum": 0},
                    "code_source": {"enum": ["email", "authenticator"]},
                    "instructions": {"type": "string"}
                }),
                &["site", "state"],
            );
            json!({"type": "object", "anyOf": [page, detail, download, sign_in],
                "$defs": generator.take_definitions(true)})
        }
        "photo" => json!({"type": "object", "anyOf": [
            object(
                json!({
                    "width": {"type": "integer", "minimum": 1},
                    "height": {"type": "integer", "minimum": 1},
                    "depth": {"type": "object", "required": ["artifact", "near", "far"], "properties": {
                        "artifact": artifact(), "near": {"type": "number"}, "far": {"type": "number"}
                    }},
                    "normals": {"type": "object", "required": ["artifact", "encoding", "frame"], "properties": {
                        "artifact": artifact(), "encoding": {"type": "string"}, "frame": {"type": "string"}
                    }},
                    "device": {"type": "string"},
                    "seconds": {"type": "number", "minimum": 0},
                    "peak_gpu_memory_bytes": {"type": "integer", "minimum": 0},
                    "image_sha256": {"type": "string"}
                }),
                &["width", "height", "depth", "device", "image_sha256"],
            ),
            object(
                json!({
                    "artifact": artifact(),
                    "triangles": {"type": "integer", "minimum": 1},
                    "generated_triangles": {"type": "integer", "minimum": 1},
                    "size_mm": {"type": "array", "items": {"type": "number", "minimum": 0}, "minItems": 3, "maxItems": 3},
                    "resolution": {"type": "integer", "minimum": 1},
                    "steps": object(json!({"structure": {"type": "integer"}, "shape": {"type": "integer"}}), &["structure", "shape"]),
                    "tokens": object(json!({"structure": {"type": "integer"}, "shape": {"type": "integer"}}), &["structure", "shape"]),
                    "seed": {"type": "integer", "minimum": 0},
                    "fov_degrees": {"type": "number"},
                    "fov_source": {"enum": ["request", "photograph", "assumed"]},
                    "background": {"enum": ["removed", "transparency"]},
                    "device": {"type": "string"},
                    "seconds": {"type": "number", "minimum": 0},
                    "stages": {"type": "array", "items": object(json!({
                        "stage": {"type": "string"},
                        "seconds": {"type": "number", "minimum": 0},
                        "peak_gpu_memory_bytes": {"type": "integer", "minimum": 0}
                    }), &["stage", "seconds"])},
                    "peak_gpu_memory_bytes": {"type": "integer", "minimum": 0},
                    "image_sha256": {"type": "string"}
                }),
                &["artifact", "triangles", "generated_triangles", "size_mm", "resolution", "steps", "tokens", "seed", "fov_degrees", "fov_source", "background", "device", "image_sha256"],
            ),
            photo_job(),
            object(json!({
                "jobs":{"type":"array","items":photo_job()},
                "requested_limit":{"type":"integer","minimum":1},
                "returned":{"type":"integer","minimum":0},
                "next_offset":{"type":["integer","null"],"minimum":0}
            }), &["jobs","requested_limit","returned","next_offset"])
        ]}),
        "artifact" => json!({"type": "object", "anyOf": [
            artifact(),
            object(json!({"format_version":{"const":1},"logical_bytes":{"type":"integer"},"reserved_remaining_bytes":{"type":"integer"},"charged_bytes":{"type":"integer"},"complete":{"type":"boolean"},"by_project":{"type":"object"},"by_class":{"type":"object"},"accounting":{"type":"string"}}), &["format_version","logical_bytes","reserved_remaining_bytes","charged_bytes","complete","by_project","by_class","accounting"]),
            object(json!({"cleanup":{"type":"array","items":object(json!({"id":{"type":"string"},"bytes":{"type":["integer","null"]},"state":{"enum":["protected","pending","deleted"]},"reason":{"type":"string"}}), &["id","bytes","state","reason"])},"retained_artifacts":{"type":"string"}}), &["cleanup","retained_artifacts"]),
            object(json!({"state": {"enum": ["prepared", "receiving", "ready", "failed", "commit_uncertain", "committed"]}}), &["state"]),
            object(json!({"entries": {"type": "array", "items": artifact()}}), &["entries"]),
            object(json!({"upload_id": {"type": "string"}}), &["upload_id"]),
            object(json!({"bytes_written": {"type": "integer", "minimum": 0}}), &["bytes_written"]),
            object(json!({"file": object(json!({
                "uri": {"type": "string"}, "name": {"type": "string"},
                "mimeType": {"type": "string"}, "size": {"type": "integer", "minimum": 0},
                "digest": object(json!({"algorithm": {"const": "sha-256"},
                    "value": {"type": "string", "pattern": "^[A-Za-z0-9_-]{43}$"}}), &["algorithm", "value"])
            }), &["uri", "name", "mimeType", "size", "digest"])}), &["file"])
        ]}),
        _ => unreachable!("output schema requested for an unknown workflow"),
    };
    let mut value = if matches!(name, "printer" | "print") {
        value
    } else {
        with_errors(name, value)
    };
    compact_schema(&mut value);
    Arc::new(
        value
            .as_object()
            .expect("output schema is an object")
            .clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_cover_every_workflow_and_reject_broken_evidence_shapes() {
        for tool in crate::tools::workflows::TOOLS {
            let definition = Value::Object(schema(tool.name).as_ref().clone());
            assert_eq!(
                definition["type"], "object",
                "{} output schema must declare an object root for MCP clients",
                tool.name
            );
            let validator = jsonschema::validator_for(&definition).unwrap();
            assert!(
                !validator.is_valid(&json!({})),
                "{} accepts missing evidence",
                tool.name
            );
            assert!(
                !validator.is_valid(&json!([])),
                "{} accepts a non-object",
                tool.name
            );
        }
        let definition = Value::Object(schema("inspect").as_ref().clone());
        let validator = jsonschema::validator_for(&definition).unwrap();
        let mut observation = json!({"scene_state": {
            "generation": "12345678-1234-4234-8234-123456789abc", "revision": 7
        }, "items": [], "next_offset": null});
        assert!(validator.is_valid(&observation));
        observation["scene_state"]["revision"] = json!(-1);
        assert!(!validator.is_valid(&observation));
        let definition = Value::Object(schema("view").as_ref().clone());
        let validator = jsonschema::validator_for(&definition).unwrap();
        let mut image = json!({"path": "view.png", "size_bytes": 100,
            "media_type": "image/png", "width": 640, "height": 480});
        assert!(validator.is_valid(&image));
        image["width"] = json!(0);
        assert!(!validator.is_valid(&image));
    }
}
