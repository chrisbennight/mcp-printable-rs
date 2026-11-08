//! Workflow requests share the existing typed operation parameters and handlers.

use super::*;

/// A workflow request's schema before [`compact_schema`] removes annotations.
fn full_workflow_schema<T: schemars::JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .expect("a derived workflow schema serializes");
    let object = schema
        .as_object_mut()
        .expect("a derived schema is an object");
    object.remove("title");
    object.insert("type".to_string(), json!("object"));
    normalize_type_unions(&mut schema);
    schema
}

fn workflow_schema_of<T: schemars::JsonSchema>() -> Arc<JsonObject> {
    let mut schema = full_workflow_schema::<T>();
    compact_schema(&mut schema);
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
    Dependencies(ProjectDependenciesParams) => "printable_project_dependencies",
    Object(ObjectInfoParams) => "printable_object_get",
    NodeTree(NodeTreeInfoParams) => "printable_node_tree_get",
    EditingState(EditingStateParams) => "printable_editing_state_get",
});

workflow!(HeadRequest {
    Models(HeadModelsParams) => "printable_flame_models",
    Create(HeadCreateParams) => "printable_flame_create",
    Update(HeadUpdateParams) => "printable_flame_update",
    // Boxed: its many print options make it far larger than the others.
    PreparePrint(Box<HeadPreparePrintParams>) => "printable_flame_prepare_print",
    Export(HeadExportParams) => "printable_flame_export",
    Reconstruct(crate::recon::ReconRequest) => "printable_flame_reconstruct",
    // Boxed: it holds both a reconstruction and a print request.
    PhotoToBust(Box<HeadPhotoToBustParams>) => "printable_flame_photo_to_bust",
});

workflow!(PhotoRequest {
    Depth(crate::recon::DepthRequest) => "printable_photo_depth",
    Object(crate::recon::ObjectRequest) => "printable_photo_object",
    Status(crate::recon::PhotoJobRequest) => "printable_photo_job_status",
    List(crate::recon::PhotoJobListRequest) => "printable_photo_job_list",
    Cancel(crate::recon::PhotoJobRequest) => "printable_photo_job_cancel",
});

/// Actions that need the reconstruction worker.
const RECONSTRUCTION_ACTIONS: [&str; 2] = ["reconstruct", "photo_to_bust"];

/// The `head` input schema without the actions that need the reconstruction
/// worker, for servers whose worker is absent or not ready.
fn head_schema() -> Arc<JsonObject> {
    without_actions(
        &workflow_schema_of::<HeadRequest>(),
        &RECONSTRUCTION_ACTIONS,
    )
}

/// The `photo` input schema with depth and retained-job actions.
fn photo_depth_schema() -> Arc<JsonObject> {
    without_actions(&workflow_schema_of::<PhotoRequest>(), &["object"])
}

/// The `photo` input schema with object generation and retained-job actions.
fn photo_object_schema() -> Arc<JsonObject> {
    without_actions(&workflow_schema_of::<PhotoRequest>(), &["depth"])
}

fn photo_jobs_schema() -> Arc<JsonObject> {
    without_actions(&workflow_schema_of::<PhotoRequest>(), &["depth", "object"])
}

/// A workflow input schema without the `removed` actions.
fn without_actions(schema: &JsonObject, removed: &[&str]) -> Arc<JsonObject> {
    let mut schema = schema.clone();
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(variants)) = schema.get_mut(key) {
            variants.retain(|variant| {
                let action = &variant["properties"]["action"];
                !removed
                    .iter()
                    .any(|name| action["const"] == *name || action["enum"] == json!([name]))
            });
        }
    }
    // Drop the definitions that only the removed actions used.
    if let Some(Value::Object(definitions)) = schema.remove("$defs") {
        let mut reachable = std::collections::HashSet::new();
        let mut pending = definition_refs(&Value::Object(schema.clone()));
        while let Some(name) = pending.pop() {
            if let Some(definition) = definitions.get(&name)
                && reachable.insert(name)
            {
                pending.extend(definition_refs(definition));
            }
        }
        let kept: JsonObject = definitions
            .into_iter()
            .filter(|(name, _)| reachable.contains(name))
            .collect();
        schema.insert("$defs".into(), Value::Object(kept));
    }
    Arc::new(schema)
}

/// The names of the `$defs` entries a schema refers to.
fn definition_refs(value: &Value) -> Vec<String> {
    match value {
        Value::Object(object) => object
            .iter()
            .flat_map(|(key, value)| match (key.as_str(), value) {
                ("$ref", Value::String(target)) => target
                    .strip_prefix("#/$defs/")
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
                _ => definition_refs(value),
            })
            .collect(),
        Value::Array(items) => items.iter().flat_map(definition_refs).collect(),
        _ => Vec::new(),
    }
}

workflow!(EditRequest {
    Primitive(PrimitiveCreateParams) => "printable_primitive_create",
    Boolean(BooleanApplyParams) => "printable_boolean_apply",
    Rename(ObjectRenameParams) => "printable_object_rename",
    RigidRotation(RigidRotationAnimateParams) => "printable_rigid_rotation_animate",
});

workflow!(SceneRequest {
    OpenProject(OpenProjectSceneParams) => "printable_scene_open_project",
    AttachCad(AttachCadParams) => "printable_scene_attach_cad",
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
    Usage(StorageQueryParams) => "printable_workspace_usage",
    CleanupPreview(StorageQueryParams) => "printable_workspace_cleanup_preview",
    Cleanup(StorageCleanupParams) => "printable_workspace_cleanup",
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
        HEAD => {
            // Which fields a request names is lost once it is parsed.
            if arguments["action"] == "photo_to_bust" {
                head::refuse_reconstructed(&arguments["params"])?;
            }
            de::<HeadRequest>(arguments)?.resolve()
        }
        PHOTO => de::<PhotoRequest>(arguments)?.resolve(),
        "scene" => de::<SceneRequest>(arguments)?.resolve(),
        "scad_build" => de::<ScadRequest>(arguments)?.resolve(),
        "cad_build" => ResolvedCall::new(
            "printable_cad_build",
            de::<crate::cad::CadRequest>(arguments)?,
        ),
        "view" => de::<ViewRequest>(arguments)?.resolve(),
        "slice" => ResolvedCall::new(
            "printable_slice",
            de::<crate::slicing::SliceRequest>(arguments)?,
        ),
        "render" => de::<RenderRequest>(arguments)?.resolve(),
        "job" => de::<JobRequest>(arguments)?.resolve(),
        "artifact" => de::<ArtifactRequest>(arguments)?.resolve(),
        "project" => match de::<crate::projects::ProjectRequest>(arguments)? {
            crate::projects::ProjectRequest::ExportBlender(params) => {
                ResolvedCall::new("printable_project_export_blender", params)
            }
            crate::projects::ProjectRequest::Cover(params) => {
                ResolvedCall::new("printable_project_cover", params)
            }
            crate::projects::ProjectRequest::Reindex(params) => {
                ResolvedCall::new("printable_project_reindex", params)
            }
            request => ResolvedCall::new("printable_project", request),
        },
        CATALOG => match de::<crate::projects::CatalogRequest>(arguments)? {
            crate::projects::CatalogRequest::Search(params) => {
                ResolvedCall::new("printable_project_search", params)
            }
            crate::projects::CatalogRequest::Card(params) => {
                ResolvedCall::new("printable_project_card", params)
            }
            crate::projects::CatalogRequest::Status(params) => {
                ResolvedCall::new("printable_project_index_status", params)
            }
        },
        LIBRARY => ResolvedCall::new(
            "printable_library",
            de::<crate::library::LibraryRequest>(arguments)?,
        ),
        "printer" => ResolvedCall::new(
            "printable_printer",
            de::<crate::printers::PrinterRequest>(arguments)?,
        ),
        "skill" => ResolvedCall::new(
            "printable_skill",
            de::<crate::resources::skills::SkillRequest>(arguments)?,
        ),
        "print" => ResolvedCall::new(
            "printable_print",
            de::<crate::printers::PrintRequest>(arguments)?,
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

/// The workflow tool that exists only while FLAME head models are mounted.
pub const HEAD: &str = "head";

/// The workflow tool that exists only while the photograph worker can
/// estimate depth, generate objects or inspect retained photograph jobs.
pub const PHOTO: &str = "photo";

/// The workflow tool that exists only while public model sites are enabled.
pub const LIBRARY: &str = "library";

/// The read-only workflow tool for the user's own project library.
pub const CATALOG: &str = "catalog";

/// Optional capabilities that change what this server advertises.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Availability {
    /// A FLAME model is mounted.
    pub head: bool,
    /// The reconstruction worker is ready and a FLAME 2020 model is mounted.
    pub reconstruct: bool,
    /// The photograph worker reports its depth model present.
    pub depth: bool,
    /// The photograph worker can serve at least one object generator.
    pub object: bool,
    /// Retained photograph jobs exist independently of current model mounts.
    pub photo_jobs: bool,
    /// At least one public model site is enabled.
    pub library: bool,
}

/// Tools this server advertises and accepts. `head` is included only when a
/// FLAME model is mounted, and offers `reconstruct` only when that can be
/// served, `photo` when generation or retained jobs can be served,
/// offering only the actions that can be served, and `library` only when a
/// model site is enabled, so clients never see a capability that cannot be
/// served.
pub fn advertised(available: Availability) -> impl Iterator<Item = &'static ToolDef> {
    TOOLS
        .iter()
        .filter(move |tool| tool.name != HEAD || available.head)
        .filter(move |tool| {
            tool.name != PHOTO || available.depth || available.object || available.photo_jobs
        })
        .filter(move |tool| tool.name != LIBRARY || available.library)
        .map(move |tool| match tool.name {
            HEAD if available.reconstruct => &HEAD_WITH_RECONSTRUCTION,
            PHOTO if !available.object && !available.depth => &PHOTO_JOBS,
            PHOTO if !available.object => &PHOTO_DEPTH,
            PHOTO if !available.depth => &PHOTO_OBJECT,
            _ => tool,
        })
}

// The parts of the `photo` description, as macros so `concat!` can join them.
macro_rules! photo_prefix {
    () => {
        "Estimate geometry from a workspace photograph on the photograph worker's GPU, one job at a time."
    };
}
macro_rules! photo_depth_text {
    () => {
        "depth writes a 16-bit relative depth PNG, and optionally a normal map, for reliefs and molds; calibrate heights before building geometry, as the image-to-mold skill describes."
    };
}
macro_rules! photo_object_text {
    () => {
        "object generates one object's shape with Pixal3D or TRELLIS.2 as an STL in millimetres, Z up; hidden sides are invented, so validate and repair it before slicing. A background or TRELLIS.2 request returns a job_id: poll it with status, find jobs with list and stop one with cancel."
    };
}

static PHOTO_DEPTH: ToolDef = ToolDef {
    name: PHOTO,
    description: concat!(
        photo_prefix!(),
        " ",
        photo_depth_text!(),
        " status, list and cancel inspect and stop retained jobs."
    ),
    schema: photo_depth_schema,
    annotations: write_annotations,
};

static PHOTO_OBJECT: ToolDef = ToolDef {
    name: PHOTO,
    description: concat!(photo_prefix!(), " ", photo_object_text!()),
    schema: photo_object_schema,
    annotations: write_annotations,
};

static PHOTO_JOBS: ToolDef = ToolDef {
    name: PHOTO,
    description: "Inspect retained photograph jobs with status and list, or request cancellation with cancel. Interrupted jobs are never replayed; inspect the output path before submitting again.",
    schema: photo_jobs_schema,
    annotations: write_annotations,
};

// The parts of the `head` description.
macro_rules! head_text {
    () => {
        "Generate parametric human heads in millimetres from mounted FLAME models. models lists variants; create and update evaluate a head in Blender from coefficients and joint rotations; export writes an OBJ; prepare_print writes a watertight bust STL, optionally tuned for an FDM or resin printer."
    };
}
macro_rules! head_coefficients {
    () => {
        "Coefficients are standard deviations within plus or minus 5."
    };
}

const HEAD_DESCRIPTION: &str = concat!(head_text!(), " ", head_coefficients!());

static HEAD_WITH_RECONSTRUCTION: ToolDef = ToolDef {
    name: HEAD,
    description: concat!(
        head_text!(),
        " reconstruct estimates identity, expression and surface detail from 1 to 8 photographs of one person, and photo_to_bust also prepares the bust in one step. ",
        head_coefficients!()
    ),
    schema: workflow_schema_of::<HeadRequest>,
    annotations: write_annotations,
};

pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "skill",
        description: "List bundled client-run skills or get one's complete instructions by exact name. inspect-printer-camera guides vision review of printer camera snapshots; image-to-mold guides relief depth maps, mold geometry, validation and slicing. Loading instructions does no work and grants no authority.",
        schema: workflow_schema_of::<crate::resources::skills::SkillRequest>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "slice",
        description: "Slice a project STL or 3MF: discover printer, process and filament profiles and settings, prepare a slice on an explicit build plate, inspect or cancel it, and review actual toolpath layers. Preparing never starts a print; use print for that. Inspect retained state before retrying an uncertain preparation.",
        schema: workflow_schema_of::<crate::slicing::SliceRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printer",
        description: "Observe printers: list them, read status, materials and history, request a telemetry refresh, or save a camera snapshot to a project. A refresh does not prove fresh telemetry or readiness. Use print to stage or control jobs, and read printable://printing/workflow-v1 before physical printing.",
        schema: workflow_schema_of::<crate::printers::PrinterRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "print",
        description: "Import, review and stage printer-ready files, read print records and history, and explicitly start, update, pause, resume, stop, cancel or clear the plate. Start, resume and clear_plate can cause physical motion; after an unknown outcome, inspect rather than retry. Read printable://printing/workflow-v1 first; use slice to make printer-ready files.",
        schema: workflow_schema_of::<crate::printers::PrintRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "status",
        description: "Report backend, workspace and durable-job readiness without changing anything. Set detail for command inventory, device details and runtime configuration.",
        schema: schema_of::<StatusRequest>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "inspect",
        description: "Inspect the live Blender scene in bounded pages: objects, one object's sections, node trees, editing state and project dependencies; follow next_offset. Use view for images and blender_execute for other queries. Read printable://modeling/blender-v1 for modeling guidance.",
        schema: workflow_schema_of::<InspectRequest>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "edit",
        description: "Create a primitive, apply an exact boolean, rename an object, or author a rigid rotation in Blender. Rotation authoring does not certify clearance; use analyze_assembly for that, and blender_execute for general modeling.",
        schema: workflow_schema_of::<EditRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "blender_execute",
        description: "Run synchronous Blender Python for organic or procedural modeling, materials and animation; set result to bounded JSON. Checkpoint with scene first. After a timeout the mutation outcome is unknown: never retry automatically; wait for a healthy status, then inspect or restore.",
        schema: schema_of::<BlenderExecuteParams>,
        annotations: code_annotations,
    },
    ToolDef {
        name: "scene",
        description: "Manage the live Blender scene: open a project, attach CAD GLB assemblies, clear, checkpoint, restore, and import or export STL. Switching projects needs the current scene state and an explicit backup or discard. Restore disables embedded scripts.",
        schema: workflow_schema_of::<SceneRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "scad_build",
        description: "Build confined OpenSCAD source into a validated STL mesh, a PNG image or an SVG section, with typed definitions and an optional product profile. Read printable://design/product-v1 for modules and evidence limits. Use cad_build for CadQuery or STEP.",
        schema: workflow_schema_of::<ScadRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "cad_build",
        description: "Build CadQuery Python or import STEP into a project, then inspect components, features, distances and sections, or export STEP, STL, GLB or SVG by the returned model identity. Inspect and export run in the background: check or stop them with status or cancel by project_id and output_dir, and give each build a new output_dir. Geometric evidence does not qualify physical fit, and nothing here changes Blender or prints.",
        schema: workflow_schema_of::<crate::cad::CadRequest>,
        annotations: code_annotations,
    },
    ToolDef {
        name: "view",
        description: "Look at the live Blender scene: capture a native viewport or editor, or render dimensions, a section or cutaway, or an overhang heatmap. Images are artifacts with optional bounded inline copies. Use render for presentation images.",
        schema: workflow_schema_of::<ViewRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "render",
        description: "Render the scene, a product view, a gallery or a turntable contact sheet as artifacts, with optional bounded inline images. Reuse presentation settings for matched views and read printable://render/product-v1. Use job for long renders and video.",
        schema: workflow_schema_of::<RenderRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "compare_renders",
        description: "Build a before/after comparison of existing PNG renders. Render both from the same view first; comparison does not align cameras or invoke Blender.",
        schema: schema_of::<CompareRendersParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "validate_mesh",
        description: "Validate an immutable STL snapshot: solid topology, bounds, mass properties, bed contact and overhangs, with optional wall thickness, dimensions, draft and reference comparison. Read assessment for failed and unmeasured criteria. It does not repair geometry or establish physical suitability.",
        schema: schema_of::<ValidateMeshParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "analyze_assembly",
        description: "Analyze two watertight STL solids in millimetres for interference, surface gap and optional continuous rigid motion. Uncertified rotation fails closed; clearance does not imply retention or press-fit behaviour.",
        schema: schema_of::<AnalyzeAssemblyParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "job",
        description: "Run durable checkpoint-based renders in the background: submit, get progress, list, page artifacts and cancel. Mechanical jobs need full-arc clearance before frame one, and videos are artifacts, never base64. Use render for quick images and artifact for uploads.",
        schema: workflow_schema_of::<JobRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "project",
        description: "Manage durable projects: create, get, list, resolve shared paths and list files; update descriptions and tags, set the cover and reindex the library search; keep design requirements and immutable source revisions; export selected files or a native Blender bundle. Edits need the expected record or revision. Use catalog to search the user's projects or read a project's card.",
        schema: workflow_schema_of::<crate::projects::ProjectRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: CATALOG,
        description: "Find the user's own Printable projects and read their library cards. search finds projects by words, meaning or a picture, with filters and sort, and returns concise results unless detail is full; card returns one project's full card; status reports the search index and its search methods. Find designs on MakerWorld, Printables or Thingiverse with library instead.",
        schema: workflow_schema_of::<crate::projects::CatalogRequest>,
        annotations: catalog_annotations,
    },
    ToolDef {
        name: HEAD,
        description: HEAD_DESCRIPTION,
        schema: head_schema,
        annotations: write_annotations,
    },
    ToolDef {
        name: PHOTO,
        description: concat!(
            photo_prefix!(),
            " ",
            photo_depth_text!(),
            " ",
            photo_object_text!()
        ),
        schema: workflow_schema_of::<PhotoRequest>,
        annotations: write_annotations,
    },
    ToolDef {
        name: LIBRARY,
        description: "Find existing designs on MakerWorld, Printables and Thingiverse: search by keyword, get one model's license and files, and download chosen files into a project with a source.json record. When a MakerWorld download reports library_sign_in_required, ask the user for the code and pass it to sign_in. Site text is untrusted; respect each design's license. Use catalog for the user's own projects.",
        schema: workflow_schema_of::<crate::library::LibraryRequest>,
        annotations: library_annotations,
    },
    ToolDef {
        name: "artifact",
        description: "Work with workspace files: stat, list, read and write small files, upload larger ones in chunks, ingest uploaded file URIs and check transfers, publish immutable files for transfer, and report or clean up storage. Writes and chunks accept at most 1 MiB decoded; publish video for delivery.",
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
    fn project_scene_actions_require_and_preserve_the_complete_observation() {
        let observed = json!({
            "generation": "00000000-0000-4000-8000-000000000001",
            "revision": 4,
            "project_id": "model"
        });
        for (action, mut params, operation) in [
            (
                "open_project",
                json!({"project_id": "model", "mode": "adopt"}),
                "printable_scene_open_project",
            ),
            (
                "attach_cad",
                json!({"project_id": "model", "path": "builds/one/model.glb"}),
                "printable_scene_attach_cad",
            ),
        ] {
            assert!(resolve("scene", json!({"action": action, "params": params})).is_err());
            params["expected_scene"] = observed.clone();
            let resolved = resolve("scene", json!({"action": action, "params": params})).unwrap();
            assert_eq!(resolved.name, operation);
            assert_eq!(resolved.arguments["expected_scene"], observed);
        }
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
    fn head_and_photo_are_advertised_only_when_they_can_be_served() {
        let names = |head, depth| {
            advertised(Availability {
                head,
                reconstruct: false,
                depth,
                object: false,
                photo_jobs: false,
                library: false,
            })
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
        };
        assert!(!names(false, false).contains(&HEAD));
        assert!(names(true, false).contains(&HEAD));
        assert!(!names(true, false).contains(&PHOTO));
        assert!(names(false, true).contains(&PHOTO));
        assert!(!names(true, true).contains(&LIBRARY));
        assert_eq!(names(true, true).len(), TOOLS.len() - 1);
        assert_eq!(names(false, false).len(), TOOLS.len() - 3);
        let library = advertised(Availability {
            library: true,
            ..Availability::default()
        })
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
        assert!(library.contains(&LIBRARY));
        assert_eq!(library.len(), TOOLS.len() - 2);
    }

    #[test]
    fn photo_offers_only_the_actions_its_models_serve() {
        let photo = |depth, object| {
            advertised(Availability {
                depth,
                object,
                ..Availability::default()
            })
            .find(|tool| tool.name == PHOTO)
        };
        assert!(photo(false, false).is_none());
        for (depth, object) in [(true, false), (false, true), (true, true)] {
            let tool = photo(depth, object).unwrap();
            let schema = serde_json::to_string(&*(tool.schema)()).unwrap();
            assert_eq!(schema.contains("depth_path"), depth);
            assert_eq!(schema.contains("mesh_path"), object);
            assert_eq!(tool.description.contains("depth writes"), depth);
            assert_eq!(tool.description.contains("object generates"), object);
        }
    }

    #[test]
    fn retained_photo_jobs_remain_discoverable_without_models() {
        let tool = advertised(Availability {
            photo_jobs: true,
            ..Availability::default()
        })
        .find(|tool| tool.name == PHOTO)
        .unwrap();
        let schema = serde_json::to_string(&*(tool.schema)()).unwrap();
        assert!(!schema.contains("depth_path"));
        assert!(!schema.contains("mesh_path"));
        for (action, operation, params) in [
            (
                "status",
                "printable_photo_job_status",
                json!({"job_id":"0123456789abcdef0123456789abcdef"}),
            ),
            ("list", "printable_photo_job_list", json!({"limit":10})),
            (
                "cancel",
                "printable_photo_job_cancel",
                json!({"job_id":"0123456789abcdef0123456789abcdef"}),
            ),
        ] {
            assert!(schema.contains(action));
            let call = resolve(PHOTO, json!({"action":action,"params":params})).unwrap();
            assert_eq!(call.name, operation);
        }
    }

    #[test]
    fn photo_object_resolves_to_the_object_operation() {
        let call = resolve(
            PHOTO,
            json!({"action": "object", "params": {"image": "figure.jpg", "mesh_path": "figure.stl", "resolution": 512}}),
        )
        .unwrap();
        assert_eq!(call.name, "printable_photo_object");
        assert!(
            resolve(
                PHOTO,
                json!({"action": "object", "params": {"image": "figure.jpg", "mesh_path": "figure.stl", "texture": true}}),
            )
            .is_err()
        );
    }

    #[test]
    fn photo_depth_resolves_to_the_depth_operation() {
        let call = resolve(
            PHOTO,
            json!({"action": "depth", "params": {"image": "photo.jpg", "depth_path": "depth.png"}}),
        )
        .unwrap();
        assert_eq!(call.name, "printable_photo_depth");
        assert!(
            resolve(
                PHOTO,
                json!({"action": "depth", "params": {"image": "photo.jpg", "depth_path": "depth.png", "scale": 2}}),
            )
            .is_err()
        );
    }

    #[test]
    fn reconstruct_is_advertised_only_when_it_can_be_served() {
        let head_schema = |reconstruct| {
            let tool = advertised(Availability {
                head: true,
                reconstruct,
                depth: false,
                object: false,
                photo_jobs: false,
                library: false,
            })
            .find(|tool| tool.name == HEAD)
            .unwrap();
            serde_json::to_string(&*(tool.schema)()).unwrap()
        };
        assert!(!head_schema(false).contains("reconstruct"));
        assert!(!head_schema(false).contains("photo_to_bust"));
        assert!(head_schema(true).contains("reconstruct"));
        assert!(head_schema(true).contains("photo_to_bust"));
        assert!(head_schema(false).contains("prepare_print"));
    }

    #[test]
    fn head_actions_resolve_to_flame_operations() {
        for (action, operation) in [
            ("models", "printable_flame_models"),
            ("create", "printable_flame_create"),
            ("update", "printable_flame_update"),
            ("prepare_print", "printable_flame_prepare_print"),
            ("export", "printable_flame_export"),
            ("reconstruct", "printable_flame_reconstruct"),
            ("photo_to_bust", "printable_flame_photo_to_bust"),
        ] {
            let params = match action {
                "update" => json!({"object": "Head"}),
                "prepare_print" => json!({"path": "bust.stl"}),
                "export" => json!({"path": "head.obj"}),
                "reconstruct" => json!({"images": ["face.jpg"], "displacement_path": "detail.png"}),
                "photo_to_bust" => json!({
                    "reconstruct": {"images": ["face.jpg"], "displacement_path": "detail.png"},
                    "print": {"path": "bust.stl"}
                }),
                _ => json!({}),
            };
            let call = resolve(HEAD, json!({"action": action, "params": params})).unwrap();
            assert_eq!(call.name, operation);
        }
        assert!(resolve(HEAD, json!({"action": "create", "params": {"scale": 2}})).is_err());
    }

    #[test]
    fn public_workflow_catalog_preserves_the_agreed_product_boundaries() {
        assert_eq!(
            TOOLS.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            [
                "skill",
                "slice",
                "printer",
                "print",
                "status",
                "inspect",
                "edit",
                "blender_execute",
                "scene",
                "scad_build",
                "cad_build",
                "view",
                "render",
                "compare_renders",
                "validate_mesh",
                "analyze_assembly",
                "job",
                "project",
                "catalog",
                "head",
                "photo",
                "library",
                "artifact",
            ],
        );
        for name in [
            "skill",
            "status",
            "inspect",
            "validate_mesh",
            "analyze_assembly",
            "catalog",
        ] {
            assert_eq!(
                (lookup(name).expect("read tool").annotations)().read_only_hint,
                Some(true)
            );
        }
        for name in [
            "scene", "edit", "job", "artifact", "printer", "print", "project",
        ] {
            let annotations = (lookup(name).expect("mutating tool").annotations)();
            assert_eq!(annotations.read_only_hint, Some(false));
            assert_eq!(annotations.destructive_hint, Some(true));
        }
    }

    #[test]
    fn catalog_is_read_only_and_may_reach_configured_search_services() {
        let catalog = advertised(Availability::default())
            .find(|tool| tool.name == CATALOG)
            .expect("the catalog is always advertised");
        let annotations = (catalog.annotations)();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(true));
        for words in ["own Printable projects", "MakerWorld", "library"] {
            assert!(catalog.description.contains(words), "{words}");
        }
        let schema = serde_json::to_string(&*(catalog.schema)()).unwrap();
        for action in ["search", "card", "status"] {
            assert!(schema.contains(&format!("\"{action}\"")), "{action}");
        }
        for moved in [
            "index_status",
            "index_rebuild",
            "index_retry",
            "\"card\"",
            "\"search\"",
        ] {
            let project = serde_json::to_string(&*(lookup("project").unwrap().schema)()).unwrap();
            assert!(!project.contains(moved), "{moved}");
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
                "head",
                json!({"action": "create", "params": {"identity": [0.5], "pose": {"jaw": [0.2, 0.0, 0.0]}}}),
            ),
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

    /// The keys `compact_schema` removed from `full`, as (key, value) pairs.
    /// Fails when anything else differs.
    fn removed(full: &Value, compact: &Value, found: &mut Vec<(String, Value)>) {
        match (full, compact) {
            (Value::Object(full), Value::Object(compact)) => {
                assert!(
                    compact.keys().all(|key| full.contains_key(key)),
                    "compaction added a key"
                );
                for (key, value) in full {
                    match compact.get(key) {
                        Some(kept) => removed(value, kept, found),
                        None => found.push((key.clone(), value.clone())),
                    }
                }
            }
            (Value::Array(full), Value::Array(compact)) => {
                assert_eq!(full.len(), compact.len());
                for (full, compact) in full.iter().zip(compact) {
                    removed(full, compact, found);
                }
            }
            (full, compact) => assert_eq!(full, compact),
        }
    }

    #[test]
    fn compaction_removes_only_annotations_validators_ignore() {
        let schemas = [
            full_workflow_schema::<HeadRequest>(),
            full_workflow_schema::<JobRequest>(),
            full_workflow_schema::<RenderRequest>(),
            full_workflow_schema::<ViewRequest>(),
            full_workflow_schema::<crate::projects::ProjectRequest>(),
            full_workflow_schema::<crate::projects::CatalogRequest>(),
            full_workflow_schema::<crate::printers::PrintRequest>(),
            full_workflow_schema::<crate::cad::CadRequest>(),
        ];
        let mut found = Vec::new();
        for full in schemas {
            let mut compact = full.clone();
            compact_schema(&mut compact);
            removed(&full, &compact, &mut found);
            for kept in ["title", "$schema"] {
                assert!(
                    !serde_json::to_string(&compact)
                        .unwrap()
                        .contains(&format!("\"{kept}\":\""))
                );
            }
        }
        assert!(!found.is_empty());
        for (key, value) in found {
            match key.as_str() {
                "title" | "$schema" => {}
                "format" => assert!(
                    value
                        .as_str()
                        .is_some_and(|format| NUMERIC_FORMATS.contains(&format)),
                    "{value}"
                ),
                "default" => assert!(
                    value.is_null()
                        || value == json!("")
                        || value == json!([])
                        || value == json!({}),
                    "{value}"
                ),
                other => panic!("compaction removed {other}: {value}"),
            }
        }
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
