use std::sync::Arc;

use serde_json::{Map, Value, json};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
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

fn scene_state() -> Value {
    object(
        json!({
            "generation": {"type": "string", "format": "uuid"},
            "revision": {"type": "integer", "minimum": 0, "maximum": 9007199254740991_u64}
        }),
        &["generation", "revision"],
    )
}

pub fn schema(name: &str) -> Arc<Map<String, Value>> {
    let value = match name {
        "printer" | "print" => crate::printers::output_schema(name),
        "status" => object(
            json!({
                "server_version": {"type": "string"}, "transport": {"const": "streamable-http"},
                "blender": {"type": "object", "properties": {
                    "available": {"type": "boolean"},
                    "scene_state": {"anyOf": [scene_state(), {"type": "null"}]},
                    "native_observation": {"type": "object"}
                }, "required": ["available"]},
                "openscad": {"type": "object"}, "workspace": {"type": "object"},
                "render_jobs": {"type": "object"},
                "printers": {"type": "object", "properties": {"configured": {"type": "boolean"}}, "required": ["configured"]}
            }),
            &[
                "server_version",
                "transport",
                "blender",
                "openscad",
                "workspace",
                "render_jobs",
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
        "cad_build" => object(
            json!({
                "build_directory": {"type": "string"},
                "report": {"type": "object"},
                "artifacts": {"type": "array", "items": object(json!({
                    "artifact": artifact(), "sha256": {"type": "string", "pattern": "^[a-f0-9]{64}$"}
                }), &["artifact", "sha256"])}
            }),
            &["build_directory", "report", "artifacts"],
        ),
        "scad_build" => object(
            json!({
                "artifact": artifact(), "diagnostics": {"type": "object"},
                "validation": {"type": "object"}, "manufacturing_evidence": {"type": "object"}
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
                "artifact": artifact(), "units": {"const": "millimetres"}, "report": {"type": "object"}
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
        "project" => json!({"type":"object","anyOf":[
            serde_json::to_value(schemars::schema_for!(crate::projects::Project)).expect("project schema serializes"),
            object(json!({"projects":{"type":"array","items":{"type":"object"}},"limit_reached":{"type":"boolean"}}), &["projects","limit_reached"]),
            object(json!({"project_id":{"type":"string"},"path":{"type":"string"}}), &["project_id","path"]),
            object(json!({"project_id":{"type":"string"},"entries":{"type":"array","items":artifact()},"limit_reached":{"type":"boolean"}}), &["project_id","entries","limit_reached"])
        ]}),
        "artifact" => json!({"type": "object", "anyOf": [
            artifact(),
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
