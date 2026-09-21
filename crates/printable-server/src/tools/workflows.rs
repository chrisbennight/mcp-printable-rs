//! Workflow requests share the existing typed operation parameters and handlers.

use super::*;

fn workflow_schema_of<T: schemars::JsonSchema>() -> Arc<JsonObject> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .expect("a derived workflow schema serializes");
    let object = schema
        .as_object_mut()
        .expect("a derived schema is an object");
    object.remove("title");
    object.insert("type".to_string(), json!("object"));
    normalize_type_unions(&mut schema);
    Arc::new(
        schema
            .as_object()
            .expect("normalized schema object")
            .clone(),
    )
}

pub struct ResolvedCall {
    pub name: &'static str,
    pub arguments: Value,
    pub response: ResponseProjection,
}

#[derive(Clone, Copy, Default)]
pub enum ResponseProjection {
    #[default]
    Complete,
    JobProgress,
    Readiness,
}

impl ResponseProjection {
    pub fn apply(self, mut value: Value) -> Value {
        match self {
            Self::Complete => {}
            Self::JobProgress => {
                if let Some(object) = value.as_object_mut() {
                    object.retain(|key, _| {
                        matches!(
                            key.as_str(),
                            "job_id"
                                | "kind"
                                | "state"
                                | "progress"
                                | "updated_unix_ms"
                                | "cancellation_requested"
                                | "recovery_count"
                                | "failure"
                                | "video_artifact"
                                | "execution"
                        )
                    });
                }
            }
            Self::Readiness => {
                if let Some(object) = value.as_object_mut() {
                    object.remove("http");
                    for (section, fields) in [
                        (
                            "blender",
                            &[
                                "host",
                                "port",
                                "commands",
                                "cycles_devices",
                                "execution_limits",
                            ][..],
                        ),
                        (
                            "workspace",
                            &["workspace_root", "blender_workspace_root"][..],
                        ),
                        ("openscad", &["binary"][..]),
                    ] {
                        if let Some(details) =
                            object.get_mut(section).and_then(Value::as_object_mut)
                        {
                            for field in fields {
                                details.remove(*field);
                            }
                        }
                    }
                }
            }
        }
        value
    }
}

impl ResolvedCall {
    fn new<T: serde::Serialize>(name: &'static str, params: T) -> Result<Self, ToolError> {
        Ok(Self {
            name,
            arguments: serde_json::to_value(params)?,
            response: ResponseProjection::Complete,
        })
    }
}

macro_rules! workflow {
    ($name:ident { $($variant:ident($params:ty) => $operation:literal),+ $(,)? }) => {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        #[serde(tag = "action", content = "params", rename_all = "snake_case", deny_unknown_fields)]
        enum $name {
            $($variant($params)),+
        }

        impl $name {
            fn resolve(self) -> Result<ResolvedCall, ToolError> {
                match self {
                    $(Self::$variant(params) => ResolvedCall::new($operation, params)),+
                }
            }
        }
    };
}

workflow!(InspectRequest {
    Scene(SceneInfoParams) => "printable_scene_get",
    Object(ObjectInfoParams) => "printable_object_get",
    NodeTree(NodeTreeInfoParams) => "printable_node_tree_get",
    EditingState(EditingStateParams) => "printable_editing_state_get",
});

workflow!(EditRequest {
    Primitive(PrimitiveCreateParams) => "printable_primitive_create",
    Boolean(BooleanApplyParams) => "printable_boolean_apply",
    Rename(ObjectRenameParams) => "printable_object_rename",
    RigidRotation(RigidRotationAnimateParams) => "printable_rigid_rotation_animate",
});

workflow!(SceneRequest {
    Clear(SceneClearParams) => "printable_scene_clear",
    Checkpoint(SceneCheckpointParams) => "printable_scene_checkpoint",
    Restore(SceneRestoreParams) => "printable_scene_restore",
    Import(StlImportParams) => "printable_stl_import",
    Export(StlExportParams) => "printable_stl_export",
});

workflow!(ScadRequest {
    Mesh(ScadCompileParams) => "printable_scad_compile",
    Image(ScadRenderParams) => "printable_scad_render",
    Section(ScadCrossSectionParams) => "printable_scad_cross_section",
});

workflow!(ViewRequest {
    Native(NativeViewParams) => "printable_native_view",
    Dimensions(RenderDimensionsParams) => "printable_render_dimensions",
    Section(RenderCrossSectionParams) => "printable_render_cross_section",
    Overhangs(RenderHeatmapParams) => "printable_render_printability_heatmap",
});

workflow!(RenderRequest {
    Scene(RenderPreviewParams) => "printable_render_preview",
    Product(RenderProductParams) => "printable_render_product",
    Gallery(RenderGalleryParams) => "printable_render_gallery",
    Turntable(RenderTurntableParams) => "printable_render_turntable",
});

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobGetRequest {
    job_id: String,
    /// Include the complete retained specification, source identity, and certification evidence.
    #[serde(default)]
    detail: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    /// Include backend command inventory, device details, and runtime configuration metadata.
    #[serde(default)]
    detail: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum JobRequest {
    Submit(Box<RenderJobSubmitParams>),
    Get(JobGetRequest),
    List(RenderJobListParams),
    Artifacts(RenderJobArtifactsParams),
    Cancel(RenderJobCancelParams),
}

impl JobRequest {
    fn resolve(self) -> Result<ResolvedCall, ToolError> {
        match self {
            Self::Submit(params) => ResolvedCall::new("printable_render_job_submit", params),
            Self::Get(params) => Ok(ResolvedCall {
                name: "printable_render_job_status",
                arguments: json!({"job_id": params.job_id}),
                response: if params.detail {
                    ResponseProjection::Complete
                } else {
                    ResponseProjection::JobProgress
                },
            }),
            Self::List(params) => ResolvedCall::new("printable_render_job_list", params),
            Self::Artifacts(params) => ResolvedCall::new("printable_render_job_artifacts", params),
            Self::Cancel(params) => ResolvedCall::new("printable_render_job_cancel", params),
        }
    }
}

workflow!(ArtifactRequest {
    Stat(StatParams) => "printable_workspace_stat",
    List(ListParams) => "printable_workspace_list",
    Read(ReadParams) => "printable_workspace_read",
    Write(WriteParams) => "printable_workspace_write",
    Publish(PublishParams) => "printable_workspace_publish",
    Ingest(crate::file_ingest::IngestParams) => "printable_workspace_ingest",
    TransferStatus(crate::file_ingest::TransferStatusParams) => "printable_workspace_transfer_status",
    UploadBegin(WriteBeginParams) => "printable_workspace_write_begin",
    UploadChunk(WriteChunkParams) => "printable_workspace_write_chunk",
    UploadCommit(WriteCommitParams) => "printable_workspace_write_commit",
});

pub fn resolve(name: &str, arguments: Value) -> Result<ResolvedCall, ToolError> {
    match name {
        "inspect" => de::<InspectRequest>(arguments)?.resolve(),
        "edit" => de::<EditRequest>(arguments)?.resolve(),
        "scene" => de::<SceneRequest>(arguments)?.resolve(),
        "scad_build" => de::<ScadRequest>(arguments)?.resolve(),
        "view" => de::<ViewRequest>(arguments)?.resolve(),
        "render" => de::<RenderRequest>(arguments)?.resolve(),
        "job" => de::<JobRequest>(arguments)?.resolve(),
        "artifact" => de::<ArtifactRequest>(arguments)?.resolve(),
        "project" => ResolvedCall::new(
            "printable_project",
            de::<crate::projects::ProjectRequest>(arguments)?,
        ),
        "status" => {
            let request = de::<StatusRequest>(arguments)?;
            Ok(ResolvedCall {
                name: "printable_status",
                arguments: json!({}),
                response: if request.detail {
                    ResponseProjection::Complete
                } else {
                    ResponseProjection::Readiness
                },
            })
        }
        "blender_execute" => ResolvedCall::new(
            "printable_blender_execute",
            de::<BlenderExecuteParams>(arguments)?,
        ),
        "compare_renders" => ResolvedCall::new(
            "printable_compare_renders",
            de::<CompareRendersParams>(arguments)?,
        ),
        "validate_mesh" => ResolvedCall::new(
            "printable_validate_mesh",
            de::<ValidateMeshParams>(arguments)?,
        ),
        "analyze_assembly" => ResolvedCall::new(
            "printable_analyze_assembly",
            de::<AnalyzeAssemblyParams>(arguments)?,
        ),
        _ => Err(ToolError::Validation("unknown workflow tool".to_string())),
    }
}

pub fn lookup(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|tool| tool.name == name)
}

pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "status",
        description: "Report backend, workspace, and durable-job readiness without mutation.",
        schema: schema_of::<StatusRequest>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "inspect",
        description: "Inspect objects, modeling structure, node topology, or editing state and available editors. Select bounded parameters; follow next_offset. Read printable://modeling/blender-v1 for modeling guidance.",
        schema: workflow_schema_of::<InspectRequest>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "edit",
        description: "Create a primitive, apply an exact boolean, rename an object, or author rigid rotation. Rotation authoring does not certify clearance. Use blender_execute for general modeling.",
        schema: workflow_schema_of::<EditRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "blender_execute",
        description: "Run explicit synchronous Blender Python. Set result to finite JSON; output is bounded. Checkpoint first. A timeout after delivery has unknown mutation outcome: never retry automatically; wait for healthy status, then inspect or restore. Persistent unhealthy state requires operator recovery.",
        schema: schema_of::<BlenderExecuteParams>,
        annotations: code_annotations,
    },
    ToolDef {
        name: "scene",
        description: "Clear, checkpoint/save, restore, import STL, or export STL in the confined workspace. Restore replaces live state with embedded scripts disabled. Artifact transfer is separate.",
        schema: workflow_schema_of::<SceneRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "scad_build",
        description: "Build confined OpenSCAD source as validated STL, PNG, or SVG section with typed definitions and an optional explicit product profile. Read printable://design/product-v1 for modules and manufacturing evidence limits.",
        schema: workflow_schema_of::<ScadRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "view",
        description: "Observe the native viewport or editor, or inspect dimensioned geometry, sections, and overhangs. Native captures identify method, scene/view state, redraw and convergence. Images and metadata remain artifacts; inline images are optional and bounded.",
        schema: workflow_schema_of::<ViewRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "render",
        description: "Render an authored scene, product presentation, gallery, or turntable contact sheet. Product presentation preserves source geometry and verifies cleanup. Images remain artifacts; optional inline images are bounded. Read printable://render/product-v1 for presentation semantics.",
        schema: workflow_schema_of::<RenderRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "compare_renders",
        description: "Compare two existing PNG artifacts in a labeled before/after image without invoking Blender.",
        schema: schema_of::<CompareRendersParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "validate_mesh",
        description: "Validate an immutable STL snapshot for solid topology, bounds, mass properties, bed contact, and overhangs. Does not repair geometry or certify arbitrary global wall thickness.",
        schema: schema_of::<ValidateMeshParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "analyze_assembly",
        description: "Analyze two watertight STL solids in millimetres for interference, surface gap, and optional continuous rigid motion. Uncertified rotation fails closed; clearance does not imply physical retention or press-fit behavior.",
        schema: schema_of::<AnalyzeAssemblyParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "job",
        description: "Submit, inspect, list, or cancel durable checkpoint-based rendering. Query progress and paginated artifacts by job_id. Mechanical jobs require complete-arc clearance before frame one. Cancellation is cooperative; videos are artifacts, never base64. This tool does not upload files.",
        schema: workflow_schema_of::<JobRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "project",
        description: "Create and discover durable projects, list their shared files, and resolve project-relative artifact paths for existing backend tools. Each request identifies its project. This does not switch the live Blender scene.",
        schema: workflow_schema_of::<crate::projects::ProjectRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "artifact",
        description: "Stat/list/read/write workspace files, publish immutable files through governed raw-byte transfer, ingest gateway files using a file URI, check transfer_status using the private URI, or upload chunks using upload_id. Stat returns metadata without bytes and supports project-relative paths. Writes/chunks accept at most 1 MiB decoded; use publication for video delivery. This tool does not run rendering jobs.",
        schema: workflow_schema_of::<ArtifactRequest>,
        annotations: write_annotations,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_polling_retains_actionable_progress_without_repeating_specifications() {
        let record = json!({
            "job_id": "example", "kind": "mechanical_rotation", "state": "failed",
            "progress": {"completed_frames": 0, "total_frames": 200},
            "failure": {"code": "clearance_not_certified", "message": "motion blocked"},
            "execution": {"mode": "isolated_worker", "blender_finished": true},
            "spec": {"objects": ["fixed", "moving"], "frames": 200},
            "source_snapshot": {"sha256": "a".repeat(64), "size_bytes": 4096},
            "mechanical_analysis": {"report": {"can_rotate_full_angle": false}}
        });
        let request = json!({"action": "get", "params": {"job_id": "example"}});
        let call = resolve("job", request).unwrap();
        assert_eq!(call.arguments, json!({"job_id": "example"}));
        let progress = call.response.apply(record.clone());
        for key in [
            "job_id",
            "kind",
            "state",
            "progress",
            "failure",
            "execution",
        ] {
            assert_eq!(progress[key], record[key]);
        }
        assert!(progress.get("spec").is_none());
        assert!(progress.get("mechanical_analysis").is_none());
        let detailed = resolve(
            "job",
            json!({"action": "get", "params": {
                "job_id": "example", "detail": true
            }}),
        )
        .unwrap();
        assert_eq!(detailed.response.apply(record.clone()), record);
    }

    #[test]
    fn compact_readiness_keeps_capability_state_and_recovery_failures() {
        let status = json!({
            "blender": {"available": true, "commands": ["execute_code"],
                "scene_state": {"generation": "scene", "revision": 7},
                "native_observation": {"available": true}, "host": "internal"},
            "render_jobs": {"recovery_integrity": {"status": "blocked", "issues": ["repair history"]}},
            "workspace": {"confined": true, "workspace_root": "/workspace"},
            "http": {"host": "internal"}
        });
        let compact = resolve("status", json!({}))
            .unwrap()
            .response
            .apply(status.clone());
        assert_eq!(
            compact["blender"]["scene_state"],
            status["blender"]["scene_state"]
        );
        assert_eq!(
            compact["blender"]["native_observation"],
            status["blender"]["native_observation"]
        );
        assert_eq!(compact["render_jobs"], status["render_jobs"]);
        assert!(compact["blender"].get("commands").is_none());
        assert!(compact.get("http").is_none());
        assert_eq!(
            resolve("status", json!({"detail": true}))
                .unwrap()
                .response
                .apply(status.clone()),
            status
        );
    }

    #[test]
    fn public_workflow_catalog_preserves_the_agreed_product_boundaries() {
        assert_eq!(
            TOOLS.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            [
                "status",
                "inspect",
                "edit",
                "blender_execute",
                "scene",
                "scad_build",
                "view",
                "render",
                "compare_renders",
                "validate_mesh",
                "analyze_assembly",
                "job",
                "project",
                "artifact",
            ],
        );
        for name in ["status", "inspect", "validate_mesh", "analyze_assembly"] {
            assert_eq!(
                (lookup(name).expect("read tool").annotations)().read_only_hint,
                Some(true)
            );
        }
        for name in ["scene", "edit", "job", "artifact"] {
            let annotations = (lookup(name).expect("mutating tool").annotations)();
            assert_eq!(annotations.read_only_hint, Some(false));
            assert_eq!(annotations.destructive_hint, Some(true));
        }
    }

    #[test]
    fn workflow_schemas_validate_typed_requests_and_resolve_local_references() {
        let examples = [
            (
                "inspect",
                json!({"action": "scene", "params": {"include_transforms": false}}),
            ),
            (
                "edit",
                json!({"action": "rename", "params": {"name": "Cube", "new_name": "Part"}}),
            ),
            (
                "scene",
                json!({"action": "checkpoint", "params": {"path": "part.blend"}}),
            ),
            (
                "scad_build",
                json!({"action": "mesh", "params": {"source": "cube(1);", "path": "part.stl"}}),
            ),
            ("job", json!({"action": "list", "params": {"limit": 10}})),
            (
                "artifact",
                json!({"action": "upload_chunk", "params": {"upload_id": "test", "data_base64": "YQ=="}}),
            ),
        ];
        for tool in TOOLS {
            let schema = Value::Object((tool.schema)().as_ref().clone());
            let validator = jsonschema::validator_for(&schema).expect("valid workflow schema");
            if let Some((_, request)) = examples.iter().find(|(name, _)| *name == tool.name) {
                assert!(
                    validator.is_valid(request),
                    "{}: {:?}",
                    tool.name,
                    validator.iter_errors(request).collect::<Vec<_>>()
                );
                resolve(tool.name, request.clone()).expect("schema example also deserializes");
            }
        }
    }

    #[test]
    fn lifecycle_actions_cannot_cross_job_and_transfer_boundaries() {
        let upload = json!({"action": "upload_begin", "params": {"path": "part.stl"}});
        assert_eq!(
            resolve("artifact", upload.clone())
                .expect("upload request")
                .name,
            "printable_workspace_write_begin"
        );
        assert!(resolve("job", upload).is_err());
        let cancel = json!({"action": "cancel", "params": {"job_id": "job"}});
        assert_eq!(
            resolve("job", cancel.clone()).expect("cancel request").name,
            "printable_render_job_cancel"
        );
        assert!(resolve("artifact", cancel).is_err());
        assert!(
            resolve(
                "scene",
                json!({"action": "clear", "params": {}, "path": "unrelated.blend"})
            )
            .is_err()
        );
        assert!(
            resolve(
                "artifact",
                json!({"action": "publish", "params": {"path": "part.stl", "job_id": "job"}})
            )
            .is_err()
        );
    }

    #[test]
    fn routing_retains_typed_source_values_and_explicit_options() {
        let call = resolve(
            "inspect",
            json!({"action": "scene", "params": {
                "name_contains": "part", "include_transforms": false, "offset": 2, "limit": 3
            }}),
        )
        .expect("inspection request");
        assert_eq!(call.name, "printable_scene_get");
        assert_eq!(call.arguments["include_transforms"], false);
        assert_eq!(call.arguments["offset"], 2);
        assert_eq!(call.arguments["limit"], 3);
        assert_eq!(call.arguments["name_contains"], "part");
        let source = "result = {'note': 'literal $() and backticks'}";
        let call = resolve("blender_execute", json!({"code": source})).expect("code request");
        assert_eq!(call.arguments["code"], source);
        assert_eq!(call.arguments["timeout_seconds"], 120.0);
    }
}
