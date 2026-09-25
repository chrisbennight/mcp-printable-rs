//! Typed workspace, Blender modeling/file, and backend-status tools.
//!
//! Each tool has a typed parameter struct whose JSON Schema is derived by
//! `schemars` (serde deserialization + typed bounds, rather than a hand-written
//! schema string). Public discovery uses [`workflows`]; [`TOOLS`] names internal
//! operations. Responses use compact JSON and structured MCP content.

mod native;
pub mod output;
pub mod workflows;
use native::NativeViewParams;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::f64::consts::TAU;
use std::io::{Cursor, Read};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine as _;
use image::imageops::{FilterType, resize};
use image::{ImageFormat, ImageReader, Limits, RgbImage};
use quick_xml::Reader as XmlReader;
use quick_xml::XmlVersion;
use quick_xml::encoding::Decoder as XmlDecoder;
use quick_xml::events::{BytesStart, Event as XmlEvent};
use rmcp::model::{JsonObject, ToolAnnotations};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use printable_blender::{BlenderClient, Deadline, Params};
use printable_geom::{
    AssemblyOptions, LinearMotion, RotationalMotion, ValidationOptions, analyze_stl,
};
use printable_imaging::{PanelLayout, Tile, TileLayout, constants, side_by_side, tile_images};
use printable_scad::{RunOutput, ScadPermit, ScadRunner};
use printable_workspace::{Snapshot, Workspace};

use crate::config::Settings;
use crate::error::ToolError;
use crate::file_transfer::PublishParams;
use crate::jobs::{
    JobRegistry, RenderJobArtifactsParams, RenderJobCancelParams, RenderJobListParams,
    RenderJobStatusParams, RenderJobSubmitParams,
};
use crate::upload::{CHUNK_MAX_DECODED, UploadRegistry, random_hex_id};

/// Upper bound on concurrent ordinary workspace operations across all MCP
/// sessions. Each read/write can hold a cap-sized (25 MiB) buffer on the
/// blocking pool while it runs.
const MAX_CONCURRENT_WORKSPACE_OPS: usize = 4;
static WORKSPACE_OPS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_WORKSPACE_OPS)));
// Decoding and resampling can temporarily retain input bytes, RGBA and RGB
// images, fitted tiles, and an output canvas. Serialize those operations
// process-wide instead of multiplying their bounded peak across MCP sessions.
static VISUAL_OPS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
// Validation retains one cap-sized STL and assembly analysis retains two while
// expanding vertices, topology, BVHs, and Manifold state. Serialize those
// bounded peaks so concurrent sessions queue instead of multiplying them.
static GEOMETRY_OPS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
pub(crate) const PREVIEW_INLINE_MAX_BYTES: u64 = 1024 * 1024;
// An 8 Mi-pixel RGB canvas occupies 24 MiB, leaving 1 MiB for PNG framing and
// compression overhead under the workspace's 25 MiB generated-artifact cap.
const MAX_COMPOSITE_PIXELS: u64 = 8 * 1024 * 1024;
const MAX_DECODED_INPUT_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODER_ALLOC_BYTES: u64 = 67_108_864;
const MAX_RENDER_VIEWS: usize = 36;
const MAX_RENDER_VIEW_PIXELS: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_PRODUCT_RENDER_PIXELS: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_PRODUCT_RENDER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PRODUCT_INSTANCES: u64 = 4096;
const MAX_PRODUCT_VERTICES: u64 = 1_000_000;
const MAX_PRODUCT_EDGES: u64 = 3_000_000;
const MAX_PRODUCT_FACES: u64 = 2_000_000;
const MAX_PRODUCT_LOOPS: u64 = 6_000_000;
const MAX_PRODUCT_ATTRIBUTE_VALUES: u64 = 16_000_000;
const MAX_PRODUCT_MATERIAL_SLOTS: u64 = 4096;
const MAX_REVIEW_SOURCE_PIXELS: u64 = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_VERTICES: u64 = 1_000_000;
const MAX_DIAGNOSTIC_EDGES: u64 = 3_000_000;
const MAX_DIAGNOSTIC_FACES: u64 = 2_000_000;
const MAX_DIAGNOSTIC_LOOPS: u64 = 6_000_000;
const MAX_DIAGNOSTIC_ATTRIBUTE_VALUES: u64 = 16_000_000;
// Bounds pass through f32 mesh storage, world transforms, and bisect interpolation.
const BLENDER_BOUNDS_TOLERANCE_ULPS: f64 = 64.0;
const MAX_SCAD_SOURCE_BYTES: usize = 1024 * 1024;

fn default_path() -> String {
    ".".to_string()
}
fn default_limit() -> usize {
    printable_workspace::MAX_LIST_LIMIT
}
fn default_scene_limit() -> u16 {
    100
}
fn default_execute_timeout_seconds() -> f64 {
    120.0
}
fn default_animation_frame_start() -> i32 {
    1
}
fn default_animation_frame_end() -> i32 {
    250
}
fn default_render_timeout_seconds() -> f64 {
    3600.0
}
fn default_render_dimension() -> u16 {
    512
}
fn default_product_render_width() -> u16 {
    1024
}
fn default_product_render_height() -> u16 {
    768
}
fn default_scad_view() -> String {
    "iso".to_string()
}
fn default_true() -> bool {
    true
}
fn default_gallery_columns() -> u8 {
    3
}
fn default_turntable_columns() -> u8 {
    4
}
fn default_turntable_frames() -> u8 {
    8
}
fn default_turntable_elevation() -> f64 {
    20.0
}
fn default_gallery_views() -> Vec<GalleryView> {
    vec![
        GalleryView::Front,
        GalleryView::Right,
        GalleryView::Back,
        GalleryView::Left,
        GalleryView::Top,
        GalleryView::Isometric,
    ]
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StorageQueryParams {}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StorageCleanupParams {
    /// Identifiers returned by cleanup_preview; retained artifact paths are not accepted.
    #[schemars(length(max = 1000))]
    ids: Vec<String>,
}

/// `printable_workspace_list` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListParams {
    /// Directory under the workspace to list (default `.`).
    #[serde(default = "default_path")]
    path: String,
    /// Maximum artifacts to return (`1..=1000`, also enforced by the workspace).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
}

/// `printable_workspace_read` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    /// Non-video artifact path under the workspace. MP4 paths are discoverable but not base64-transferable.
    path: String,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatParams {
    /// Existing supported artifact. Workspace-relative unless project_id is supplied.
    path: String,
    /// Resolve path relative to this existing project; no bytes are read or returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
}

/// `printable_workspace_write` parameters (single-shot, small artifacts).
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteParams {
    /// Destination path under the workspace.
    path: String,
    /// Artifact bytes, base64-encoded. Decoded size capped at 1 MiB; stream
    /// larger artifacts with `write_begin`/`write_chunk`/`write_commit`.
    data_base64: String,
    /// Replace an existing artifact at `path` (default: refuse).
    #[serde(default)]
    overwrite: bool,
}

/// `printable_workspace_write_begin` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteBeginParams {
    /// Destination path under the workspace, committed atomically at the end.
    path: String,
    /// Replace an existing artifact at `path` on commit (default: refuse).
    #[serde(default)]
    overwrite: bool,
}

/// `printable_workspace_write_chunk` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteChunkParams {
    /// Upload id from `write_begin`.
    upload_id: String,
    /// Next slice of artifact bytes, base64-encoded (decoded size capped at
    /// 1 MiB per chunk; total capped at 25 MiB).
    data_base64: String,
}

/// `printable_workspace_write_commit` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteCommitParams {
    /// Upload id from `write_begin`.
    upload_id: String,
}

/// `printable_status` parameters (none).
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusParams {}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneExpectation {
    /// Project identity included in observations of a bound scene.
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    /// Scene generation returned by the latest relevant Blender observation.
    #[schemars(length(min = 36, max = 36))]
    generation: String,
    /// Mutation revision within that generation.
    #[schemars(range(min = 0, max = 9007199254740991_u64))]
    revision: u64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneInfoParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Cursor into scene order, before filtering. Reuse next_offset with the same filters.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: u32,
    /// Maximum object summaries to return (default 100, maximum 1000).
    #[serde(default = "default_scene_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u16,
    /// Case-insensitive literal substring of the object name.
    #[serde(skip_serializing_if = "Option::is_none")]
    name_contains: Option<String>,
    /// Exact Blender type, such as MESH, CURVE, or EMPTY.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_type: Option<String>,
    /// Exact collection name; direct membership only, in the active scene.
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<String>,
    /// False returns only names and types. Defaults to true for existing callers.
    #[serde(skip_serializing_if = "Option::is_none")]
    include_transforms: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectDependenciesParams {
    project_id: String,
    /// Require the observed scene identity for stable dependency pagination.
    expected_scene: SceneExpectation,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: u32,
    #[serde(default = "default_inspection_limit")]
    #[schemars(range(min = 1, max = 100))]
    limit: u16,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ObjectSection {
    Summary,
    Materials,
    Modifiers,
    Hierarchy,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ObjectInfoParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Exact Blender object name.
    name: String,
    /// Defaults to summary. Other sections return paginated modeling structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    section: Option<ObjectSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 1000000))]
    offset: Option<u32>,
    /// Section page size (default 20, maximum 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u16>,
}

#[derive(Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NodeTreeKind {
    #[default]
    Material,
    Geometry,
}

#[derive(Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NodeTreeSection {
    #[default]
    Nodes,
    Links,
}

fn default_inspection_limit() -> u16 {
    20
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NodeTreeInfoParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Exact material name or Geometry Nodes group name, according to kind.
    #[schemars(length(min = 1, max = 255))]
    name: String,
    #[serde(default)]
    kind: NodeTreeKind,
    /// Nodes give identity/type; links give socket identifiers. No recursive expansion.
    #[serde(default)]
    section: NodeTreeSection,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: u32,
    #[serde(default = "default_inspection_limit")]
    #[schemars(range(min = 1, max = 100))]
    limit: u16,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneClearParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenProjectSceneParams {
    #[serde(default = "default_project_scene_timeout")]
    #[schemars(range(min = 1, max = 1800))]
    timeout_seconds: u64,
    project_id: String,
    expected_scene: SceneExpectation,
    mode: ProjectSceneMode,
    /// Project-relative checkpoint, required only for checkpoint mode.
    checkpoint: Option<String>,
    /// Workspace-relative backup of the current scene before switching.
    save_current_to: Option<String>,
    #[serde(default)]
    discard_current: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProjectSceneMode {
    Empty,
    Checkpoint,
    Adopt,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AttachCadParams {
    #[serde(default = "default_project_scene_timeout")]
    #[schemars(range(min = 1, max = 1800))]
    timeout_seconds: u64,
    project_id: String,
    expected_scene: SceneExpectation,
    /// Project-relative GLB emitted by cad_build.
    path: String,
}

fn default_project_scene_timeout() -> u64 {
    600
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneCheckpointParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Destination `.blend` path under the confined workspace.
    path: String,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SceneRestoreParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Existing `.blend` checkpoint under the confined workspace.
    path: String,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ObjectRenameParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Exact current Blender object name.
    name: String,
    /// Desired unique Blender object name.
    new_name: String,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RigidRotationAnimateParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// One to 1000 unique Blender objects with no hierarchy, animation, constraints, or rigid-body state.
    #[schemars(length(min = 1, max = 1000))]
    objects: Vec<String>,
    /// Unique name for the Empty pivot controller created by this operation.
    controller_name: String,
    /// Rotation pivot in Blender world coordinates.
    pivot: [f64; 3],
    /// Finite non-zero right-hand-rule axis; Blender normalizes it before mutation.
    axis: [f64; 3],
    /// Positive angular travel in degrees.
    angle_degrees: f64,
    /// First keyed Blender timeline frame (default 1).
    #[serde(default = "default_animation_frame_start")]
    #[schemars(range(min = -1048574, max = 1048574))]
    frame_start: i32,
    /// Last keyed Blender timeline frame (default 250); must exceed frame_start.
    #[serde(default = "default_animation_frame_end")]
    #[schemars(range(min = -1048574, max = 1048574))]
    frame_end: i32,
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PrimitiveKind {
    Cube,
    Cylinder,
    UvSphere,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PrimitiveCreateParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Primitive family to create.
    primitive: PrimitiveKind,
    /// Optional unique object name.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Cube edge length (cube only; default 2.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<f64>,
    /// Radial segments (cylinder only; default 64, maximum 1024).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 3, max = 1024))]
    vertices: Option<u16>,
    /// Radius (cylinder or UV sphere; default 1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    radius: Option<f64>,
    /// Cylinder depth (default 2.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<f64>,
    /// Longitudinal segments (UV sphere only; default 64, maximum 1024).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 3, max = 1024))]
    segments: Option<u16>,
    /// Latitudinal rings (UV sphere only; default 32, maximum 512).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 3, max = 512))]
    ring_count: Option<u16>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum BooleanOperation {
    Union,
    Difference,
    Intersect,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BooleanApplyParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Mesh object that receives the boolean result.
    target: String,
    /// Distinct mesh object used as the boolean operand.
    operand: String,
    /// Exact boolean operation.
    operation: BooleanOperation,
    /// Optional unique name for the resulting target mesh.
    #[serde(skip_serializing_if = "Option::is_none")]
    result_name: Option<String>,
    /// Delete the operand after a successful apply (default false).
    #[serde(default)]
    delete_operand: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StlImportParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Existing `.stl` path under the confined shared workspace.
    path: String,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StlExportParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Destination `.stl` path under the confined shared workspace.
    path: String,
    /// Export only selected objects (default false).
    #[serde(default)]
    selected_only: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BlendSaveParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Destination `.blend` path under the confined shared workspace.
    path: String,
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum RenderEngine {
    Eevee,
    Cycles,
}

fn default_render_engine() -> RenderEngine {
    RenderEngine::Eevee
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderPreviewParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Destination `.png` path under the confined shared workspace.
    path: String,
    /// Output width in pixels (default 512, maximum 8192).
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    width: u16,
    /// Output height in pixels (default 512, maximum 8192).
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    height: u16,
    /// EEVEE for fast review or CYCLES for final-quality rendering.
    #[serde(default = "default_render_engine")]
    engine: RenderEngine,
    /// CYCLES sample count (default 128, maximum 4096); invalid for EEVEE.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4096))]
    samples: Option<u16>,
    /// Caller-selected positive, runtime-representable render budget in seconds.
    /// Defaults to one hour; there is no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
    /// Include the PNG as MCP image content when it is at most 1 MiB. The
    /// workspace artifact is always produced regardless of this setting.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProductPresentationProfile {
    Engineering,
    StudioNeutral,
    StudioDark,
}

impl ProductPresentationProfile {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Engineering => "engineering",
            Self::StudioNeutral => "studio_neutral",
            Self::StudioDark => "studio_dark",
        }
    }

    fn camera_type(self) -> &'static str {
        match self {
            Self::Engineering => "orthographic",
            Self::StudioNeutral | Self::StudioDark => "perspective",
        }
    }

    fn lens_mm(self) -> Option<f64> {
        match self {
            Self::Engineering => None,
            Self::StudioNeutral => Some(70.0),
            Self::StudioDark => Some(85.0),
        }
    }

    fn view_transform(self) -> &'static str {
        match self {
            Self::Engineering | Self::StudioNeutral => "Khronos PBR Neutral",
            Self::StudioDark => "AgX",
        }
    }

    fn world(self) -> ([f64; 3], f64) {
        match self {
            Self::Engineering => ([0.18, 0.18, 0.18], 0.8),
            Self::StudioNeutral => ([0.055, 0.055, 0.055], 0.65),
            Self::StudioDark => ([0.008, 0.015, 0.028], 0.45),
        }
    }

    pub(crate) fn ground_material(self) -> Option<([f64; 3], f64, f64)> {
        match self {
            Self::Engineering => None,
            Self::StudioNeutral => Some(([0.18, 0.18, 0.18], 0.0, 0.72)),
            Self::StudioDark => Some(([0.012, 0.022, 0.038], 0.0, 0.58)),
        }
    }

    pub(crate) fn default_shading(self) -> ProductSurfaceShading {
        match self {
            Self::Engineering => ProductSurfaceShading::Preserve,
            Self::StudioNeutral | Self::StudioDark => ProductSurfaceShading::SmoothByAngle,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProductPresentationView {
    /// Horizontal orbit angle in degrees from +X toward +Y (default 45).
    #[serde(default = "default_product_azimuth")]
    #[schemars(range(min = -360.0, max = 360.0))]
    pub(crate) azimuth_degrees: f64,
    /// Vertical camera angle in degrees above the XY plane (default 25).
    /// Engineering accepts -89 through 89; grounded studio profiles accept
    /// 0 through 89 so their opaque ground cannot hide the product.
    #[serde(default = "default_product_elevation")]
    #[schemars(range(min = -89.0, max = 89.0))]
    pub(crate) elevation_degrees: f64,
}

fn default_product_azimuth() -> f64 {
    45.0
}

fn default_product_elevation() -> f64 {
    25.0
}

impl Default for ProductPresentationView {
    fn default() -> Self {
        Self {
            azimuth_degrees: default_product_azimuth(),
            elevation_degrees: default_product_elevation(),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProductMaterialOverride {
    /// Selected object names that receive this presentation-only material.
    #[schemars(length(min = 1, max = 1000))]
    pub(crate) objects: Vec<String>,
    /// Display-referred sRGB color components from 0 through 1.
    pub(crate) base_color_srgb: [f64; 3],
    /// PBR metallic value from 0 through 1.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub(crate) metallic: f64,
    /// PBR roughness value from 0 through 1.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub(crate) roughness: f64,
}

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProductSurfaceShading {
    Preserve,
    SmoothByAngle,
}

impl ProductSurfaceShading {
    fn name(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::SmoothByAngle => "smooth_by_angle",
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProductPresentation {
    /// Deterministic engineering or product-studio setup.
    pub(crate) profile: ProductPresentationProfile,
    /// Camera orbit angles. Bounds framing always adds a fixed 15% margin.
    #[serde(default)]
    pub(crate) view: ProductPresentationView,
    /// Presentation-only material assignments. An object can appear once.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub(crate) materials: Vec<ProductMaterialOverride>,
    /// Optional override for the profile's preserve/smooth-by-angle choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) surface_shading: Option<ProductSurfaceShading>,
    /// Presentation-only exposure in stops; bounded to -10 through 10.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = -10, max = 10))]
    pub(crate) exposure_stops: Option<f64>,
    /// Scale all profile area lights and world illumination together (0 through 10).
    /// Defaults to 1. Zero disables this illumination, not emissive materials.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 10))]
    pub(crate) light_intensity_scale: Option<f64>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderProductParams {
    /// Reject stale scene state before rendering starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Destination `.png` path under the confined shared workspace.
    path: String,
    /// Unique renderable source object names. Every evaluated collection
    /// instance of a selected source object is included.
    #[schemars(length(min = 1, max = 1000))]
    objects: Vec<String>,
    /// Deterministic disposable product-presentation setup.
    presentation: ProductPresentation,
    /// Output width in pixels (default 1024, maximum 8192). Width multiplied
    /// by height may not exceed 16,777,216 pixels.
    #[serde(default = "default_product_render_width")]
    #[schemars(range(min = 1, max = 8192))]
    width: u16,
    /// Output height in pixels (default 768, maximum 8192). Width multiplied
    /// by height may not exceed 16,777,216 pixels.
    #[serde(default = "default_product_render_height")]
    #[schemars(range(min = 1, max = 8192))]
    height: u16,
    /// EEVEE for fast product review or CYCLES for final-quality rendering.
    #[serde(default = "default_render_engine")]
    engine: RenderEngine,
    /// CYCLES sample count (default 128, maximum 4096); invalid for EEVEE.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4096))]
    samples: Option<u16>,
    /// Caller-selected positive render budget in seconds (default one hour).
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
    /// Include the PNG as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
enum GalleryView {
    Front,
    Right,
    Back,
    Left,
    Top,
    Bottom,
    Isometric,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MultiViewRenderSettings {
    /// Reject stale scene state before rendering starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Per-view width in pixels (default 512, maximum 8192). Width × height
    /// must not exceed 8,388,608 pixels so every RGB8 source is compositable.
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    width: u16,
    /// Per-view height in pixels (default 512, maximum 8192). Width × height
    /// must not exceed 8,388,608 pixels so every RGB8 source is compositable.
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    height: u16,
    /// EEVEE for fast review or CYCLES for final-quality rendering.
    #[serde(default = "default_render_engine")]
    engine: RenderEngine,
    /// CYCLES sample count (default 128, maximum 4096); invalid for EEVEE.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 4096))]
    samples: Option<u16>,
    /// Caller-selected budget for the complete multi-view render in seconds.
    /// Defaults to one hour; there is no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderGalleryParams {
    /// Destination path for the labeled gallery PNG.
    path: String,
    /// Unique preset views to render. Defaults to front/right/back/left/top/isometric.
    #[serde(default = "default_gallery_views")]
    #[schemars(length(min = 1, max = 7))]
    views: Vec<GalleryView>,
    /// Tile columns in the composite (default 3, maximum 7).
    #[serde(default = "default_gallery_columns")]
    #[schemars(range(min = 1, max = 7))]
    columns: u8,
    /// Optional deterministic product presentation. When omitted, the existing
    /// engineering-review renderer and output remain unchanged.
    presentation: Option<ProductPresentation>,
    #[serde(flatten)]
    render: MultiViewRenderSettings,
    /// Include the composite as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderDimensionsParams {
    /// Destination path for the three-view labeled dimensions PNG.
    path: String,
    #[serde(flatten)]
    render: MultiViewRenderSettings,
    /// Include the composite as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

fn default_section_axis() -> SectionAxis {
    SectionAxis::Z
}

fn default_build_direction() -> [f64; 3] {
    [0.0, 0.0, 1.0]
}

fn default_isometric_direction() -> [f64; 3] {
    [1.0, -1.0, 1.0]
}

fn default_overhang_angle() -> f64 {
    45.0
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
enum SectionAxis {
    X,
    Y,
    Z,
}

impl SectionAxis {
    fn label(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::Y => "Y",
            Self::Z => "Z",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderCrossSectionParams {
    /// Destination path for the labeled cross-section PNG.
    path: String,
    /// Optional renderable object names. Omit to section the complete visible scene.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1000))]
    objects: Option<Vec<String>>,
    /// World axis normal to the cut plane (default Z).
    #[serde(default = "default_section_axis")]
    axis: SectionAxis,
    /// World-space cut position on the selected axis. Defaults to the selected bounds center.
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<f64>,
    /// Camera direction. Magnitude is normalized; defaults to the positive cut-plane axis.
    #[serde(skip_serializing_if = "Option::is_none")]
    view_direction: Option<[f64; 3]>,
    #[serde(flatten)]
    render: MultiViewRenderSettings,
    /// Include the labeled result as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderHeatmapParams {
    /// Destination path for the labeled printability heatmap PNG.
    path: String,
    /// Optional renderable object names. Omit to analyze the complete visible scene.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1000))]
    objects: Option<Vec<String>>,
    /// World-space build-up direction. Magnitude is normalized (default +Z).
    #[serde(default = "default_build_direction")]
    build_direction: [f64; 3],
    /// Downward overhang angle in degrees; faces strictly above it receive warning or severe coloring (default 45).
    #[serde(default = "default_overhang_angle")]
    #[schemars(range(min = 0.0, max = 90.0))]
    overhang_angle_degrees: f64,
    /// Camera direction. Magnitude is normalized (default isometric).
    #[serde(default = "default_isometric_direction")]
    view_direction: [f64; 3],
    #[serde(flatten)]
    render: MultiViewRenderSettings,
    /// Include the labeled result as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

/// `printable_validate_mesh` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ValidateMeshParams {
    /// Confined STL artifact to validate. STL coordinates are interpreted as millimetres.
    path: String,
    /// Build-up direction. Magnitude is normalized (default +Z).
    #[serde(default = "default_build_direction")]
    build_direction: [f64; 3],
    /// Downward overhang angle in degrees (default 45).
    #[serde(default = "default_overhang_angle")]
    #[schemars(range(min = 0.0, max = 90.0))]
    overhang_angle_degrees: f64,
    /// Optional material density in grams per cubic centimetre for mass estimation.
    density_g_cm3: Option<f64>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AssemblyMotionParams {
    /// Translation direction in the shared STL coordinate system. Magnitude is normalized.
    direction: [f64; 3],
    /// Positive translation distance to test in millimetres. The sweep is continuous, not sampled.
    travel_mm: f64,
    /// Optional non-negative clearance to preserve throughout this motion. Defaults to required_clearance_mm, then zero.
    target_clearance_mm: Option<f64>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AssemblyRotationParams {
    /// World-space pivot in the shared STL millimetre coordinate system.
    pivot_mm: [f64; 3],
    /// Right-hand-rule rotation axis. Magnitude is normalized.
    axis: [f64; 3],
    /// Positive angular travel to test in degrees.
    angle_degrees: f64,
    /// Optional non-negative clearance to preserve throughout this rotation. Defaults to required_clearance_mm, then zero.
    target_clearance_mm: Option<f64>,
}

/// `printable_analyze_assembly` parameters.
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnalyzeAssemblyParams {
    /// Confined watertight STL for the fixed assembly geometry.
    fixed_path: String,
    /// Confined watertight STL for the part being checked or moved.
    moving_path: String,
    /// Optional non-negative design clearance in millimetres for the static pass/fail result.
    required_clearance_mm: Option<f64>,
    /// Optional rigid linear sweep with distinct clearance-limit and physical-contact results.
    motion: Option<AssemblyMotionParams>,
    /// Optional rigid angular sweep. Passing requires a conservative certificate over the complete arc.
    rotation: Option<AssemblyRotationParams>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ScadDefineValue {
    Bool(bool),
    Number(f64),
    String(#[schemars(length(max = 4096))] String),
    NumberVector(#[schemars(length(max = 16))] Vec<f64>),
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
struct ScadDefinitions(BTreeMap<String, ScadDefineValue>);

impl schemars::JsonSchema for ScadDefinitions {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ScadDefinitions".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema =
            <BTreeMap<String, ScadDefineValue> as schemars::JsonSchema>::json_schema(generator);
        schema.insert(
            "maxProperties".to_string(),
            json!(printable_scad::MAX_DEFINITIONS),
        );
        schema
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProductKit {
    ProductV1,
}

#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProductManufacturingProfile {
    nozzle_diameter_mm: f64,
    layer_height_mm: f64,
    minimum_wall_mm: f64,
    moving_clearance_mm: f64,
    maximum_overhang_degrees: f64,
}

#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProductFormProfile {
    primary_radius_mm: f64,
    secondary_radius_mm: f64,
    edge_break_mm: f64,
    transition_length_mm: f64,
}

#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProductDesignProfile {
    kit: ProductKit,
    manufacturing: ProductManufacturingProfile,
    form: ProductFormProfile,
}

impl ProductDesignProfile {
    fn core(&self) -> printable_scad::ProductProfile {
        printable_scad::ProductProfile {
            manufacturing: printable_scad::ManufacturingProfile {
                nozzle_diameter_mm: self.manufacturing.nozzle_diameter_mm,
                layer_height_mm: self.manufacturing.layer_height_mm,
                minimum_wall_mm: self.manufacturing.minimum_wall_mm,
                moving_clearance_mm: self.manufacturing.moving_clearance_mm,
                maximum_overhang_degrees: self.manufacturing.maximum_overhang_degrees,
            },
            form: printable_scad::FormProfile {
                primary_radius_mm: self.form.primary_radius_mm,
                secondary_radius_mm: self.form.secondary_radius_mm,
                edge_break_mm: self.form.edge_break_mm,
                transition_length_mm: self.form.transition_length_mm,
            },
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScadCompileParams {
    /// Confined OpenSCAD source. Literal import()/surface() workspace paths are snapshotted.
    #[schemars(length(min = 1, max = 1048576))]
    source: String,
    /// Destination `.stl` path under the confined workspace.
    path: String,
    /// Typed OpenSCAD -D definitions. Names are unique ASCII identifiers; pbl_ is reserved.
    #[serde(default)]
    defines: ScadDefinitions,
    /// Optional product variant exposed to source as the reserved pbl_variant definition.
    #[schemars(length(max = 64))]
    variant: Option<String>,
    /// Explicit millimetre manufacturing/form profile and bundled generic product kit.
    design_profile: Option<ProductDesignProfile>,
    /// Replace an existing destination atomically (default false).
    #[serde(default)]
    overwrite: bool,
    /// Caller-selected positive subprocess budget in seconds. Defaults to one hour; no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScadRenderParams {
    /// Confined OpenSCAD source. Literal import()/surface() workspace paths are snapshotted.
    #[schemars(length(min = 1, max = 1048576))]
    source: String,
    /// Destination `.png` path under the confined workspace.
    path: String,
    /// Typed OpenSCAD -D definitions. Names are unique ASCII identifiers; pbl_ is reserved.
    #[serde(default)]
    defines: ScadDefinitions,
    /// Optional product variant exposed to source as the reserved pbl_variant definition.
    #[schemars(length(max = 64))]
    variant: Option<String>,
    /// Explicit millimetre manufacturing/form profile and bundled generic product kit.
    design_profile: Option<ProductDesignProfile>,
    /// Named camera: iso, front, back, right, left, top, or bottom.
    #[serde(default = "default_scad_view")]
    view: String,
    /// Square output size in pixels (default 512, maximum 8192).
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 8192))]
    size: u16,
    /// Use fast OpenCSG preview mode (default true); false requests a full render.
    #[serde(default = "default_true")]
    preview: bool,
    /// Replace an existing destination atomically (default false).
    #[serde(default)]
    overwrite: bool,
    /// Caller-selected positive subprocess budget in seconds. Defaults to one hour; no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
    /// Include the PNG as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScadCrossSectionParams {
    /// Confined OpenSCAD source. Literal import()/surface() workspace paths are snapshotted.
    #[schemars(length(min = 1, max = 1048576))]
    source: String,
    /// Destination `.svg` path under the confined workspace.
    path: String,
    /// Typed OpenSCAD -D definitions. Names are unique ASCII identifiers; pbl_ is reserved.
    #[serde(default)]
    defines: ScadDefinitions,
    /// Optional product variant exposed to source as the reserved pbl_variant definition.
    #[schemars(length(max = 64))]
    variant: Option<String>,
    /// Explicit millimetre manufacturing/form profile and bundled generic product kit.
    design_profile: Option<ProductDesignProfile>,
    /// Z plane in source millimetres to project as a 2D cut (default 0).
    #[serde(default)]
    z_mm: f64,
    /// Replace an existing destination atomically (default false).
    #[serde(default)]
    overwrite: bool,
    /// Caller-selected positive subprocess budget in seconds. Defaults to one hour; no configured maximum.
    #[serde(default = "default_render_timeout_seconds")]
    timeout_seconds: f64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderTurntableParams {
    /// Destination path for the labeled turntable contact-sheet PNG.
    path: String,
    /// Evenly spaced frames around the model (default 8, maximum 36).
    #[serde(default = "default_turntable_frames")]
    #[schemars(range(min = 3, max = 36))]
    frames: u8,
    /// Camera elevation in degrees from -89 through 89 (default 20).
    #[serde(default = "default_turntable_elevation")]
    elevation_degrees: f64,
    /// Orbit clockwise instead of counter-clockwise (default false).
    #[serde(default)]
    clockwise: bool,
    /// Tile columns in the contact sheet (default 4, maximum 12).
    #[serde(default = "default_turntable_columns")]
    #[schemars(range(min = 1, max = 12))]
    columns: u8,
    /// Optional deterministic product presentation. Orbit controls remain
    /// authoritative for each turntable view.
    presentation: Option<ProductPresentation>,
    #[serde(flatten)]
    render: MultiViewRenderSettings,
    /// Include the composite as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompareRendersParams {
    /// Existing PNG artifact shown in the left BEFORE panel.
    before_path: String,
    /// Existing PNG artifact shown in the right AFTER panel.
    after_path: String,
    /// Destination path for the labeled comparison PNG.
    path: String,
    /// Width of each output panel (default 512, maximum 4096).
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 4096))]
    panel_width: u16,
    /// Height of each output panel (default 512, maximum 4096).
    #[serde(default = "default_render_dimension")]
    #[schemars(range(min = 1, max = 4096))]
    panel_height: u16,
    /// Include the comparison as MCP image content when it is at most 1 MiB.
    #[serde(default = "default_true")]
    include_inline: bool,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BlenderExecuteParams {
    /// Reject stale scene state at Blender's serialized command boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    /// Optional current UI target; use inspect editing_state to discover editors.
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<EditorContextParams>,
    /// Explicit synchronous Python source executed on Blender's main thread.
    /// Set `result` to finite JSON data to return structured output. Spawned
    /// threads and processes must finish before the source returns.
    #[schemars(length(min = 1))]
    code: String,
    /// Caller-selected positive, runtime-representable synchronous execution
    /// budget in seconds. Defaults to 120 seconds; there is no configured
    /// maximum.
    #[serde(default = "default_execute_timeout_seconds")]
    timeout_seconds: f64,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditorContextParams {
    #[serde(default)]
    #[schemars(range(min = 0, max = 63))]
    window: u8,
    /// Blender area type, for example VIEW_3D, NODE_EDITOR, or IMAGE_EDITOR.
    #[schemars(length(min = 1, max = 64))]
    area_type: String,
    /// Index among areas of this type in the selected window.
    #[serde(default)]
    area_index: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    region_type: Option<String>,
    /// Reject unless the current mode matches; this does not change mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    expected_mode: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum EditingStateSection {
    Editors,
    Selection,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditingStateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_scene: Option<SceneExpectation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    section: Option<EditingStateSection>,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000000))]
    offset: u32,
    #[serde(default = "default_inspection_limit")]
    #[schemars(range(min = 1, max = 100))]
    limit: u16,
}

/// One tool's static metadata. `schema`/`annotations` are `fn` pointers so each
/// tool's typed param struct is monomorphized once.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Arc<JsonObject>,
    pub annotations: fn() -> ToolAnnotations,
}

/// Derive the input schema for a param struct in rmcp's tool convention
/// (draft 2020-12, title stripped). Param structs always have an object root.
fn schema_of<T: schemars::JsonSchema + std::any::Any>() -> Arc<JsonObject> {
    let schema = rmcp::handler::server::tool::schema_for_input::<T>()
        .expect("a tool parameter struct always has an object-rooted schema");
    let mut schema = Value::Object((*schema).clone());
    normalize_type_unions(&mut schema);
    Arc::new(
        schema
            .as_object()
            .expect("a normalized tool schema remains object-rooted")
            .clone(),
    )
}

/// Express legal array-valued JSON Schema type unions in the object form that
/// strict MCP clients consistently consume, without changing accepted values.
fn normalize_type_unions(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    let portable_types = object.get("type").and_then(|value| {
        let values = value.as_array()?;
        let mut types: Vec<String> = Vec::with_capacity(values.len());
        for value in values {
            let name = value.as_str()?;
            if !JSON_SCHEMA_TYPES.contains(&name) || types.iter().any(|item| item == name) {
                return None;
            }
            types.push(name.to_owned());
        }
        (!types.is_empty()).then_some(types)
    });
    if let Some(types) = portable_types {
        object.remove("type");
        let branches = Value::Array(
            types
                .into_iter()
                .map(|name| json!({"type": name}))
                .collect(),
        );
        if object.contains_key("anyOf") {
            object
                .entry("allOf")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("a generated allOf schema must be an array")
                .push(json!({"anyOf": branches}));
        } else {
            object.insert("anyOf".to_owned(), branches);
        }
    }

    for keyword in [
        "$defs",
        "definitions",
        "properties",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                normalize_type_unions(child);
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for child in children {
                normalize_type_unions(child);
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        if let Some(children) = items.as_array_mut() {
            for child in children {
                normalize_type_unions(child);
            }
        } else {
            normalize_type_unions(items);
        }
    }
    for keyword in [
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
        "additionalProperties",
        "unevaluatedProperties",
        "additionalItems",
        "unevaluatedItems",
    ] {
        if let Some(child @ Value::Object(_)) = object.get_mut(keyword) {
            normalize_type_unions(child);
        }
    }
}

const JSON_SCHEMA_TYPES: [&str; 7] = [
    "null", "boolean", "object", "array", "number", "string", "integer",
];

/// Read-only, idempotent, closed-world — the three read tools' annotations.
fn read_only_idempotent() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(false)
}

/// The write tool's annotations: mutating, non-idempotent, closed-world.
fn write_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(true)
        .idempotent(false)
        .open_world(false)
}

fn publish_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .idempotent(false)
        .open_world(false)
}

fn code_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(true)
        .idempotent(false)
        .open_world(true)
}

/// The tool catalog, in a stable order.
pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "printable_workspace_usage",
        description: "Report logical workspace usage by project and artifact class, live reservations, budget, and accounting limits.",
        schema: schema_of::<StorageQueryParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_workspace_cleanup_preview",
        description: "Preview abandoned managed temporary directories. Retained artifacts and active leases remain protected.",
        schema: schema_of::<StorageQueryParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_workspace_cleanup",
        description: "Remove selected abandoned temporary directories after rechecking live ownership. Reports protected, pending, or confirmed deleted outcomes.",
        schema: schema_of::<StorageCleanupParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_workspace_stat",
        description: "Inspect confined artifact metadata without reading bytes. Supports optional project-relative resolution. Describes a mutable filename, not immutable content identity.",
        schema: schema_of::<StatParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_workspace_list",
        description: "List supported image, model, video, and metadata artifacts under the confined workspace.",
        schema: schema_of::<ListParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_workspace_read",
        description: "Read a confined supported non-video artifact as base64, up to 25 MiB. MP4 video is path-addressable and discoverable through workspace/job tools at every size, but never returned as base64.",
        schema: schema_of::<ReadParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_workspace_publish",
        description: "Publish an immutable confined artifact snapshot through the governed MCP file-transfer handoff. Returns structured file metadata only; artifact bytes are streamed directly to the client and are never base64-encoded into tool content.",
        schema: schema_of::<PublishParams>,
        annotations: publish_annotations,
    },
    ToolDef {
        name: "printable_workspace_write",
        description: "Atomically write a small supported base64 artifact (up to 1 MiB decoded); stream larger artifacts with write_begin/write_chunk/write_commit. The .printable namespace is reserved for internal service state.",
        schema: schema_of::<WriteParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_workspace_write_begin",
        description: "Begin a chunked artifact upload outside the reserved .printable namespace; returns an upload_id for write_chunk/write_commit. Use for artifacts larger than a single 1 MiB write.",
        schema: schema_of::<WriteBeginParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_workspace_write_chunk",
        description: "Append one base64 chunk (up to 1 MiB decoded) to an open upload; returns the running byte total (capped at 25 MiB).",
        schema: schema_of::<WriteChunkParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_workspace_write_commit",
        description: "Atomically commit an open upload to its destination path, returning artifact metadata.",
        schema: schema_of::<WriteCommitParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_scene_get",
        description: "Find Blender objects with optional name, type, and collection filters. Set include_transforms=false for concise names/types. Follow next_offset even after an empty page; each call scans at most 10000 scene entries. Read printable://modeling/blender-v1 for modeling workflows.",
        schema: schema_of::<SceneInfoParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_project_dependencies",
        description: "Inspect registered external file dependencies of the bound Blender project, with engine and units metadata. Bounded pages omit absolute external paths. This is an inventory, not a portability or packed-content guarantee.",
        schema: schema_of::<ProjectDependenciesParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_project_export_blender",
        description: "Export selected native Blender project inputs and a packed editable entrypoint into a new ZIP, using a bounded isolated child without changing the live scene. Retains units, engine metadata, source hashes and explicit dependency limitations.",
        schema: schema_of::<crate::projects::NativeExportParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_object_get",
        description: "Inspect one Blender object: summary transforms/mesh counts, or bounded materials, modifiers, and hierarchy sections. Follow each section's next_offset for more details.",
        schema: schema_of::<ObjectInfoParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_node_tree_get",
        description: "Inspect a material or Geometry Nodes tree as paginated nodes or links with socket identifiers. Read-only topology, without mesh buffers, recursive groups, or arbitrary property dumps. Use targeted Blender Python for individual parameter values.",
        schema: schema_of::<NodeTreeInfoParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_scene_clear",
        description: "Remove every object from the current Blender scene while preserving data used by other scenes.",
        schema: schema_of::<SceneClearParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_scene_checkpoint",
        description: "Save a recoverable .blend checkpoint into the confined shared workspace.",
        schema: schema_of::<SceneCheckpointParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_scene_restore",
        description: "Replace the current scene from a confined .blend checkpoint with embedded scripts disabled.",
        schema: schema_of::<SceneRestoreParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_object_rename",
        description: "Rename one Blender object, refusing a name collision before mutation.",
        schema: schema_of::<ObjectRenameParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_rigid_rotation_animate",
        description: "Author a rigid pivot-axis rotation for one or more Blender objects with no existing parent or children, object animation data, constraints, rigid-body simulation, or rigid-body constraint. Creates a named Empty controller, preserves every target's world transform while parenting, and inserts linear axis-angle keyframes over the requested timeline range. Existing hierarchy, competing target motion, and controller collisions are rejected before mutation.",
        schema: schema_of::<RigidRotationAnimateParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_primitive_create",
        description: "Create a typed cube, cylinder, or UV sphere in the current Blender scene.",
        schema: schema_of::<PrimitiveCreateParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_boolean_apply",
        description: "Apply an exact union, difference, or intersection between two named mesh objects.",
        schema: schema_of::<BooleanApplyParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_stl_import",
        description: "Import an STL from the confined shared workspace through Blender's private input staging.",
        schema: schema_of::<StlImportParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_stl_export",
        description: "Export the Blender scene or selection as an STL artifact in the confined shared workspace.",
        schema: schema_of::<StlExportParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_validate_mesh",
        description: "Validate a confined STL for watertightness, winding, manifold solid geometry, connected components, bounds, volume, surface area, center of mass, build-plate contact, and overhang support. Returns actionable errors and warnings without modifying the artifact.",
        schema: schema_of::<ValidateMeshParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_analyze_assembly",
        description: "Analyze two confined watertight STL solids in a shared millimetre coordinate system. Reports surface gap separately from volumetric interference, evaluates an optional caller-selected design clearance, and continuously sweeps linear or rotational rigid motion. Rotation passes only when a conservative certificate covers the complete arc; bounded-work exhaustion is reported as clearance_not_certified. An exact-boundary start blocks conservatively at zero. A positive clearance threshold is distinguished from zero-clearance physical contact and does not claim retention.",
        schema: schema_of::<AnalyzeAssemblyParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_scad_compile",
        description: "Compile confined OpenSCAD source to an STL workspace artifact with optional typed definitions, a reserved product variant, and the generic product_v1 FDM design kit driven by an explicit millimetre profile. Literal imports are immutable snapshots. The response validates the final mesh using the profile's actual +Z overhang policy and reports measured versus not-certified manufacturing evidence honestly; the caller controls a positive subprocess budget with no configured maximum.",
        schema: schema_of::<ScadCompileParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_scad_render",
        description: "Render confined OpenSCAD source with optional typed definitions, a reserved product variant, and the generic product_v1 FDM design kit driven by an explicit millimetre profile from a named camera to a PNG workspace artifact. Literal imports are immutable snapshots; the caller controls a positive subprocess budget, and PNGs up to 1 MiB can be returned inline.",
        schema: schema_of::<ScadRenderParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_scad_cross_section",
        description: "Project a confined OpenSCAD model with optional typed definitions, a reserved product variant, and the generic product_v1 FDM design kit driven by an explicit millimetre profile at a caller-selected Z plane into an SVG workspace artifact. Parameterized calls compile the fully defined model before projection so source defaults retain the same -D override behavior as other workflows; calls without the optional parameterization retain direct projection. Literal imports are immutable snapshots, and parameterized calls share one positive caller budget and concurrency permit across both subprocesses.",
        schema: schema_of::<ScadCrossSectionParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_blend_save",
        description: "Save a copy of the current Blender scene as a .blend artifact in the confined shared workspace.",
        schema: schema_of::<BlendSaveParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_preview",
        description: "Render the current Blender scene from its active camera to a confined PNG artifact, creating a default isometric camera and light when missing. Use EEVEE for fast review or CYCLES for final quality. The caller selects a positive render budget with no configured maximum. PNGs up to 1 MiB can also be returned as inline MCP image content; larger renders remain available as workspace artifacts.",
        schema: schema_of::<RenderPreviewParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_product",
        description: "Render selected evaluated product geometry, including collection instances, through a disposable engineering, studio-neutral, or studio-dark presentation scene. Uses fixed 15% bounds framing, preserves source materials unless a presentation-only fallback or explicit override applies, reports exact camera/light/color/shading choices, and promotes the confined PNG only after cleanup and source-state verification. It never adds a render-only bevel or changes validated geometry.",
        schema: schema_of::<RenderProductParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_gallery",
        description: "Render selected model views in one caller-budgeted Blender operation and create a labeled gallery PNG plus individual confined artifacts. An optional product presentation applies the same engineering, studio-neutral, or studio-dark profile, material, shading, cleanup, and source-verification contract as printable_render_product; omission preserves the legacy orthographic review output. Publication starts after all views render and scene cleanup succeeds; a publication failure can leave earlier outputs, so inspect requested paths before retrying. The composite can be returned inline up to 1 MiB.",
        schema: schema_of::<RenderGalleryParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_dimensions",
        description: "Measure render-evaluated world bounds and render labeled front, right, and top orthographic views in one caller-budgeted Blender operation. The result preserves exact dimensions as metadata and persists the composite plus individual source views; the composite can be returned inline up to 1 MiB.",
        schema: schema_of::<RenderDimensionsParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_cross_section",
        description: "Render a non-destructive world-axis cutaway from evaluated visible mesh geometry, including collection instances. The cut plane defaults to the selected bounds center; callers can analyze named object subsets for large scenes. Convert non-mesh objects first, hide them, or select only mesh objects. The labeled result and its full-resolution source are confined artifacts, and the result can be returned inline up to 1 MiB.",
        schema: schema_of::<RenderCrossSectionParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_printability_heatmap",
        description: "Render evaluated visible mesh geometry with supported faces green, threshold overhangs amber, and severe overhangs red for a caller-selected build direction and angle. Returns exact face/area category metadata, supports named object subsets, persists the labeled result plus full-resolution source, and can return the result inline up to 1 MiB. Convert non-mesh objects first, hide them, or select only mesh objects.",
        schema: schema_of::<RenderHeatmapParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_turntable",
        description: "Render evenly spaced views around the model in one caller-budgeted Blender operation and create a labeled turntable contact sheet plus individual confined frames. An optional product presentation applies the same engineering, studio-neutral, or studio-dark profile, material, shading, cleanup, and source-verification contract as printable_render_product; omission preserves the legacy orthographic output. Grounded profiles require a non-negative elevation. The composite can be returned inline up to 1 MiB.",
        schema: schema_of::<RenderTurntableParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_job_submit",
        description: "Submit a durable still, turntable, timeline-animation, or mechanically certified rigid-rotation job from an immutable confined .blend checkpoint. An optional product presentation is persisted and replayed after restart. General animation preserves the authored camera unless auto_frame_sequence evaluates complete timeline bounds under a separate caller budget. Mechanical presentation begins only after the original checkpoint geometry passes complete-arc clearance, uses a conservative rotation envelope without a mesh ground plane, and produces zero frames when certification is blocked or inconclusive. Jobs persist resumable PNG frames, report progress, and encode MP4 artifacts without base64. Frame, sequence-framing, storage, video-size, and encoding budgets are caller-selected.",
        schema: schema_of::<RenderJobSubmitParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_render_job_status",
        description: "Inspect one durable render job's state, progress, current frame, recovery count, cancellation request, mechanical certificate when applicable, video artifact, and failure details.",
        schema: schema_of::<RenderJobStatusParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_render_job_list",
        description: "List retained durable render jobs newest first with optional state filtering and bounded pagination.",
        schema: schema_of::<RenderJobListParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_render_job_artifacts",
        description: "Discover a durable render job's resumable PNG frame sequence, mechanical analysis artifacts and certificate when applicable, and encoded MP4 artifact with bounded pagination.",
        schema: schema_of::<RenderJobArtifactsParams>,
        annotations: read_only_idempotent,
    },
    ToolDef {
        name: "printable_render_job_cancel",
        description: "Request cooperative cancellation of a queued or running durable render job. Queued work cancels immediately; active Blender work stops at the next frame boundary, and FFmpeg is terminated and reaped.",
        schema: schema_of::<RenderJobCancelParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_compare_renders",
        description: "Compose two existing confined PNG artifacts into a labeled BEFORE/AFTER review image without invoking Blender. The comparison is persisted and can be returned inline up to 1 MiB.",
        schema: schema_of::<CompareRendersParams>,
        annotations: write_annotations,
    },
    ToolDef {
        name: "printable_blender_execute",
        description: "Execute synchronous explicit Python on Blender's main thread inside the isolated Blender container. Set result to finite JSON data; Python, native, and inherited subprocess output plus the result are response-bounded. Spawned threads and processes must finish before the source returns. A timeout after request delivery has an unknown mutation outcome: never automatically retry; wait for healthy status, then inspect the scene or restore a checkpoint. Persistent unhealthy status requires operator recovery of the Blender container.",
        schema: schema_of::<BlenderExecuteParams>,
        annotations: code_annotations,
    },
    ToolDef {
        name: "printable_status",
        description: "Report server, Blender backend/device, OpenSCAD, durable-job queue/FFmpeg/recovery integrity, and workspace readiness without mutation.",
        schema: schema_of::<StatusParams>,
        annotations: read_only_idempotent,
    },
];

/// Look up a tool by name.
pub fn lookup(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

pub(crate) struct ToolOutput {
    pub(crate) value: Value,
    pub(crate) inline_png_base64: Option<String>,
}

impl ToolOutput {
    fn without_inline(value: Value) -> Self {
        Self {
            value,
            inline_png_base64: None,
        }
    }
}

/// Dispatch a tool call to its handler, returning the JSON response value or a
/// [`ToolError`]. Blocking workspace I/O (directory scans, up-to-cap reads, and
/// atomic writes with fsync) runs on Tokio's blocking pool via [`blocking`], so
/// concurrent sessions cannot occupy the async worker threads and stall
/// unrelated requests (including `/healthz`).
pub async fn dispatch(
    workspace: &Arc<Workspace>,
    uploads: &Arc<UploadRegistry>,
    blender: &BlenderClient,
    settings: &Settings,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let scad = Arc::new(ScadRunner::discover(
        settings.openscad_bin.clone(),
        settings.scad_concurrency,
    ));
    Ok(dispatch_with_content(
        workspace,
        uploads,
        blender,
        settings,
        (&scad, None),
        name,
        args,
    )
    .await?
    .value)
}

pub(crate) async fn dispatch_with_content(
    workspace: &Arc<Workspace>,
    uploads: &Arc<UploadRegistry>,
    blender: &BlenderClient,
    settings: &Settings,
    backends: (&Arc<ScadRunner>, Option<&Arc<JobRegistry>>),
    name: &str,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let (scad, jobs) = backends;
    match name {
        "printable_workspace_usage" => {
            let _: StorageQueryParams = de(args)?;
            let ws = Arc::clone(workspace);
            Ok(ToolOutput::without_inline(serde_json::to_value(
                blocking(move || ws.storage_usage()).await?,
            )?))
        }
        "printable_workspace_cleanup_preview" => {
            let _: StorageQueryParams = de(args)?;
            let ws = Arc::clone(workspace);
            let entries = blocking(move || ws.cleanup_preview()).await?;
            Ok(ToolOutput::without_inline(
                json!({"cleanup":entries,"retained_artifacts":"protected; only managed temporary directories are eligible"}),
            ))
        }
        "printable_workspace_cleanup" => {
            let p: StorageCleanupParams = de(args)?;
            let ws = Arc::clone(workspace);
            let entries = blocking(move || ws.cleanup_scratch(&p.ids)).await?;
            Ok(ToolOutput::without_inline(
                json!({"cleanup":entries,"retained_artifacts":"protected; only managed temporary directories are eligible"}),
            ))
        }
        "printable_native_view" => native::capture(Arc::clone(workspace), blender, de(args)?).await,
        "printable_render_product" => {
            let params: RenderProductParams = de(args)?;
            render_product(Arc::clone(workspace), blender, params).await
        }
        "printable_render_gallery" => {
            let params: RenderGalleryParams = de(args)?;
            render_gallery(Arc::clone(workspace), blender, params).await
        }
        "printable_render_dimensions" => {
            let params: RenderDimensionsParams = de(args)?;
            render_dimensions(Arc::clone(workspace), blender, params).await
        }
        "printable_render_cross_section" => {
            let params: RenderCrossSectionParams = de(args)?;
            render_cross_section(Arc::clone(workspace), blender, params).await
        }
        "printable_render_printability_heatmap" => {
            let params: RenderHeatmapParams = de(args)?;
            render_heatmap(Arc::clone(workspace), blender, params).await
        }
        "printable_render_turntable" => {
            let params: RenderTurntableParams = de(args)?;
            render_turntable(Arc::clone(workspace), blender, params).await
        }
        "printable_compare_renders" => {
            let params: CompareRendersParams = de(args)?;
            compare_renders(Arc::clone(workspace), params).await
        }
        "printable_scad_render" => {
            let params: ScadRenderParams = de(args)?;
            scad_render(Arc::clone(workspace), Arc::clone(scad), params).await
        }
        "printable_render_job_submit" => {
            let params: RenderJobSubmitParams = de(args)?;
            job_registry(jobs)?
                .submit(params)
                .await
                .map(ToolOutput::without_inline)
        }
        "printable_render_job_status" => {
            let params: RenderJobStatusParams = de(args)?;
            job_registry(jobs)?
                .status(params)
                .await
                .map(ToolOutput::without_inline)
        }
        "printable_render_job_list" => {
            let params: RenderJobListParams = de(args)?;
            job_registry(jobs)?
                .list(params)
                .await
                .map(ToolOutput::without_inline)
        }
        "printable_render_job_artifacts" => {
            let params: RenderJobArtifactsParams = de(args)?;
            job_registry(jobs)?
                .artifacts(params)
                .await
                .map(ToolOutput::without_inline)
        }
        "printable_render_job_cancel" => {
            let params: RenderJobCancelParams = de(args)?;
            job_registry(jobs)?
                .cancel(params)
                .await
                .map(ToolOutput::without_inline)
        }
        _ => dispatch_value(
            workspace,
            uploads,
            blender,
            settings,
            (scad, jobs),
            name,
            args,
        )
        .await
        .map(ToolOutput::without_inline),
    }
}

fn job_registry(jobs: Option<&Arc<JobRegistry>>) -> Result<&Arc<JobRegistry>, ToolError> {
    jobs.ok_or_else(|| ToolError::Job("durable render job registry is unavailable".to_string()))
}

async fn dispatch_value(
    workspace: &Arc<Workspace>,
    uploads: &Arc<UploadRegistry>,
    blender: &BlenderClient,
    settings: &Settings,
    backends: (&Arc<ScadRunner>, Option<&Arc<JobRegistry>>),
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let (scad, jobs) = backends;
    match name {
        "printable_workspace_stat" => {
            let p: StatParams = de(args)?;
            let path = match &p.project_id {
                Some(id) => crate::projects::resolve(workspace, id, &p.path)?,
                None => p.path.clone(),
            };
            let ws = Arc::clone(workspace);
            let meta = blocking(move || ws.stat_artifact(&path)).await?;
            let mut result = serde_json::to_value(meta)?;
            result["identity"] = json!("mutable_path");
            if let Some(id) = p.project_id {
                result["project_id"] = json!(id);
                result["project_path"] = json!(p.path);
            }
            Ok(result)
        }
        "printable_workspace_list" => {
            let p: ListParams = de(args)?;
            let ws = Arc::clone(workspace);
            let metas = blocking(move || ws.list_artifacts(&p.path, p.limit)).await?;
            Ok(serde_json::to_value(metas)?)
        }
        "printable_workspace_read" => {
            let p: ReadParams = de(args)?;
            read_artifact(Arc::clone(workspace), p.path).await
        }
        "printable_workspace_write" => {
            let p: WriteParams = de(args)?;
            write_artifact(Arc::clone(workspace), p).await
        }
        "printable_workspace_write_begin" => {
            let p: WriteBeginParams = de(args)?;
            let id = uploads.begin(workspace, p.path, p.overwrite).await?;
            Ok(json!({ "upload_id": id }))
        }
        "printable_workspace_write_chunk" => {
            let p: WriteChunkParams = de(args)?;
            let decoded = decode_bounded(&p.data_base64)?;
            let bytes_written = uploads.chunk(&p.upload_id, decoded).await?;
            Ok(json!({ "bytes_written": bytes_written }))
        }
        "printable_workspace_write_commit" => {
            let p: WriteCommitParams = de(args)?;
            let meta = uploads.commit(&p.upload_id, workspace).await?;
            Ok(serde_json::to_value(meta)?)
        }
        "printable_scene_get" => {
            let p: SceneInfoParams = de(args)?;
            validate_scene_page(&p)?;
            blender_command(blender, "get_scene_info", &p).await
        }
        "printable_project_dependencies" => {
            let p: ProjectDependenciesParams = de(args)?;
            validate_inspection_page(p.offset, p.limit)?;
            crate::projects::get(workspace, &p.project_id)?;
            blender_command(blender, "get_project_dependencies", &p).await
        }
        "printable_project_export_blender" => {
            let p: crate::projects::NativeExportParams = de(args)?;
            let path = p.validate(workspace)?;
            let result =
                blender_project_command(blender, "export_project_blender", &p, p.timeout_seconds)
                    .await?;
            if result["path"].as_str() != Some(&path)
                || result["project_id"].as_str() != Some(&p.project_id)
            {
                return Err(ToolError::Validation(
                    "native export returned an inconsistent project artifact".into(),
                ));
            }
            let artifact = workspace.stat_artifact(&path)?;
            Ok(
                json!({"artifact": artifact, "sha256": result["sha256"], "manifest": result["manifest"]}),
            )
        }
        "printable_object_get" => {
            let p: ObjectInfoParams = de(args)?;
            validate_inspection_page(p.offset.unwrap_or(0), p.limit.unwrap_or(20))?;
            blender_command(blender, "get_object_info", &p).await
        }
        "printable_node_tree_get" => {
            let p: NodeTreeInfoParams = de(args)?;
            validate_inspection_page(p.offset, p.limit)?;
            validate_inspection_name(&p.name)?;
            blender_command(blender, "get_node_tree_info", &p).await
        }
        "printable_editing_state_get" => {
            let p: EditingStateParams = de(args)?;
            validate_inspection_page(p.offset, p.limit)?;
            blender_command(blender, "get_editing_state", &p).await
        }
        "printable_scene_clear" => {
            let p: SceneClearParams = de(args)?;
            blender_command(blender, "clear_scene", &p).await
        }
        "printable_scene_open_project" => {
            let mut p: OpenProjectSceneParams = de(args)?;
            crate::projects::get(workspace, &p.project_id)?;
            if matches!(p.mode, ProjectSceneMode::Checkpoint) != p.checkpoint.is_some() {
                return Err(ToolError::Validation(
                    "checkpoint mode requires a checkpoint path; other modes do not accept one"
                        .into(),
                ));
            }
            if !matches!(p.mode, ProjectSceneMode::Adopt)
                && p.save_current_to.is_none()
                && !p.discard_current
            {
                return Err(ToolError::Validation(
                    "switching a scene requires save_current_to or explicit discard_current".into(),
                ));
            }
            if let Some(path) = &mut p.checkpoint {
                *path = crate::projects::resolve(workspace, &p.project_id, path)?;
                validate_blender_artifact(workspace, path, ".blend", true)?;
            }
            if let Some(path) = &p.save_current_to {
                validate_blender_artifact(workspace, path, ".blend", false)?;
                if let Some(current) = &p.expected_scene.project_id {
                    let root = crate::projects::get(workspace, current)?.root;
                    if !path.starts_with(&format!("{root}/")) {
                        return Err(ToolError::Validation(
                            "save_current_to must belong to the currently bound project".into(),
                        ));
                    }
                }
                if p.checkpoint.as_ref() == Some(path) {
                    return Err(ToolError::Validation(
                        "backup must not replace the checkpoint being restored".into(),
                    ));
                }
            }
            blender_project_command(blender, "open_project", &p, p.timeout_seconds).await
        }
        "printable_scene_attach_cad" => {
            let mut p: AttachCadParams = de(args)?;
            if p.expected_scene.project_id.as_deref() != Some(&p.project_id) {
                return Err(ToolError::Validation(
                    "CAD attachment requires the intended project's observed scene state".into(),
                ));
            }
            p.path = crate::projects::resolve(workspace, &p.project_id, &p.path)?;
            validate_blender_artifact(workspace, &p.path, ".glb", true)?;
            blender_project_command(blender, "attach_cad", &p, p.timeout_seconds).await
        }
        "printable_scene_checkpoint" => {
            let p: SceneCheckpointParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".blend", false)?;
            blender_command(blender, "save_blend", &p).await
        }
        "printable_scene_restore" => {
            let p: SceneRestoreParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".blend", true)?;
            blender_command(blender, "restore_checkpoint", &p).await
        }
        "printable_object_rename" => {
            let p: ObjectRenameParams = de(args)?;
            blender_command(blender, "rename_object", &p).await
        }
        "printable_rigid_rotation_animate" => {
            let p: RigidRotationAnimateParams = de(args)?;
            validate_rigid_rotation(&p)?;
            blender_command(blender, "animate_rotation", &p).await
        }
        "printable_primitive_create" => {
            let p: PrimitiveCreateParams = de(args)?;
            validate_primitive(&p)?;
            blender_command(blender, "create_primitive", &p).await
        }
        "printable_boolean_apply" => {
            let p: BooleanApplyParams = de(args)?;
            blender_command(blender, "boolean", &p).await
        }
        "printable_stl_import" => {
            let p: StlImportParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".stl", true)?;
            blender_command(blender, "import_stl", &p).await
        }
        "printable_stl_export" => {
            let p: StlExportParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".stl", false)?;
            blender_command(blender, "export_stl", &p).await
        }
        "printable_validate_mesh" => {
            let p: ValidateMeshParams = de(args)?;
            validate_mesh_artifact(Arc::clone(workspace), p).await
        }
        "printable_analyze_assembly" => {
            let p: AnalyzeAssemblyParams = de(args)?;
            analyze_assembly_artifacts(Arc::clone(workspace), p, settings).await
        }
        "printable_scad_compile" => {
            let p: ScadCompileParams = de(args)?;
            scad_compile(Arc::clone(workspace), Arc::clone(scad), p).await
        }
        "printable_scad_cross_section" => {
            let p: ScadCrossSectionParams = de(args)?;
            scad_cross_section(Arc::clone(workspace), Arc::clone(scad), p).await
        }
        "printable_blend_save" => {
            let p: BlendSaveParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".blend", false)?;
            blender_command(blender, "save_blend", &p).await
        }
        "printable_render_preview" => {
            let p: RenderPreviewParams = de(args)?;
            validate_blender_artifact(workspace, &p.path, ".png", false)?;
            let work_budget = validate_render_preview(&p)?;
            let value = serde_json::to_value(&p)?;
            let mut params = value.as_object().cloned().ok_or_else(|| {
                ToolError::Validation("Blender command parameters must be an object".to_string())
            })?;
            params.remove("include_inline");
            let mut result = blender
                .send_value_with_work_budget("render_still", params, work_budget)
                .await?;
            validate_render_response(&result, &p)?;
            let size_bytes = result["size_bytes"].as_u64().ok_or_else(|| {
                ToolError::Validation("Blender render response omitted size_bytes".to_string())
            })?;
            result["inline"] = inline_metadata(p.include_inline, size_bytes);
            Ok(result)
        }
        "printable_blender_execute" => {
            let p: BlenderExecuteParams = de(args)?;
            let work_budget = validate_blender_execute(&p)?;
            let value = serde_json::to_value(&p)?;
            let params = value.as_object().cloned().ok_or_else(|| {
                ToolError::Validation("Blender command parameters must be an object".to_string())
            })?;
            Ok(blender
                .send_value_with_work_budget("execute_code", params, work_budget)
                .await?)
        }
        "printable_status" => {
            // No parameters, but reject unknown fields (deny_unknown_fields).
            let _: StatusParams = de(args)?;
            status(workspace, blender, settings, scad, jobs).await
        }
        other => Err(ToolError::Validation(format!("unknown tool: {other}"))),
    }
}

async fn blender_project_command<T: serde::Serialize>(
    blender: &BlenderClient,
    command: &str,
    params: &T,
    timeout_seconds: u64,
) -> Result<Value, ToolError> {
    if !(1..=1800).contains(&timeout_seconds) {
        return Err(ToolError::Validation(
            "timeout_seconds must be between 1 and 1800".into(),
        ));
    }
    let Value::Object(params) = serde_json::to_value(params)? else {
        return Err(ToolError::Validation(
            "Blender command parameters must be an object".into(),
        ));
    };
    Ok(blender
        .send_value_with_work_budget(command, params, Duration::from_secs(timeout_seconds))
        .await?)
}

async fn blender_command<T: serde::Serialize>(
    blender: &BlenderClient,
    command: &str,
    params: &T,
) -> Result<Value, ToolError> {
    blender_command_with_deadline(blender, command, params, blender.default_deadline()).await
}

async fn blender_command_with_deadline<T: serde::Serialize>(
    blender: &BlenderClient,
    command: &str,
    params: &T,
    deadline: Deadline,
) -> Result<Value, ToolError> {
    let value = serde_json::to_value(params)?;
    let params = value.as_object().cloned().ok_or_else(|| {
        ToolError::Validation("Blender command parameters must be an object".to_string())
    })?;
    Ok(blender.send_value(command, params, deadline).await?)
}

fn validate_blender_artifact(
    workspace: &Workspace,
    path: &str,
    suffix: &str,
    must_exist: bool,
) -> Result<(), ToolError> {
    if !workspace.confined() {
        return Err(printable_workspace::WsError::Unconfined.into());
    }
    let matches_suffix = std::path::Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == &suffix[1..]);
    if !matches_suffix {
        return Err(ToolError::Validation(format!("path must end in {suffix}")));
    }
    if !must_exist {
        workspace.validate_public_mutation_path(path)?;
    }
    workspace.resolve(path, must_exist)?;
    Ok(())
}

fn validate_primitive(params: &PrimitiveCreateParams) -> Result<(), ToolError> {
    let invalid_field = match params.primitive {
        PrimitiveKind::Cube => [
            ("vertices", params.vertices.is_some()),
            ("radius", params.radius.is_some()),
            ("depth", params.depth.is_some()),
            ("segments", params.segments.is_some()),
            ("ring_count", params.ring_count.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name)),
        PrimitiveKind::Cylinder => [
            ("size", params.size.is_some()),
            ("segments", params.segments.is_some()),
            ("ring_count", params.ring_count.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name)),
        PrimitiveKind::UvSphere => [
            ("size", params.size.is_some()),
            ("vertices", params.vertices.is_some()),
            ("depth", params.depth.is_some()),
        ]
        .into_iter()
        .find_map(|(name, present)| present.then_some(name)),
    };
    if let Some(field) = invalid_field {
        return Err(ToolError::Validation(format!(
            "{field} is not valid for the selected primitive"
        )));
    }
    for (name, value) in [
        ("size", params.size),
        ("radius", params.radius),
        ("depth", params.depth),
    ] {
        if value.is_some_and(|number| !number.is_finite() || number <= 0.0) {
            return Err(ToolError::Validation(format!(
                "{name} must be a positive finite number"
            )));
        }
    }
    for (name, value, maximum) in [
        ("vertices", params.vertices, 1024),
        ("segments", params.segments, 1024),
        ("ring_count", params.ring_count, 512),
    ] {
        if value.is_some_and(|number| number < 3 || number > maximum) {
            return Err(ToolError::Validation(format!(
                "{name} must be an integer between 3 and {maximum}"
            )));
        }
    }
    Ok(())
}

fn validate_scene_page(params: &SceneInfoParams) -> Result<(), ToolError> {
    for name in [
        &params.name_contains,
        &params.object_type,
        &params.collection,
    ]
    .into_iter()
    .flatten()
    {
        validate_inspection_name(name)?;
    }
    if params.offset > 1_000_000 {
        return Err(ToolError::Validation(
            "offset must be an integer between 0 and 1000000".to_string(),
        ));
    }
    if !(1..=1000).contains(&params.limit) {
        return Err(ToolError::Validation(
            "limit must be an integer between 1 and 1000".to_string(),
        ));
    }
    Ok(())
}

fn validate_inspection_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ToolError::Validation(
            "inspection names must contain between 1 and 255 characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_inspection_page(offset: u32, limit: u16) -> Result<(), ToolError> {
    if offset > 1_000_000 || !(1..=100).contains(&limit) {
        return Err(ToolError::Validation(
            "inspection offset must be between 0 and 1000000 and limit between 1 and 100"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_rigid_rotation(params: &RigidRotationAnimateParams) -> Result<(), ToolError> {
    validate_object_subset(Some(&params.objects))?;
    if params.controller_name.is_empty() || params.controller_name.len() > 255 {
        return Err(ToolError::Validation(
            "controller_name must be a non-empty string".to_string(),
        ));
    }
    if params
        .objects
        .iter()
        .any(|name| name == &params.controller_name)
    {
        return Err(ToolError::Validation(
            "controller_name must differ from every target object".to_string(),
        ));
    }
    if params.pivot.iter().any(|component| !component.is_finite()) {
        return Err(ToolError::Validation(
            "pivot must contain three finite numbers".to_string(),
        ));
    }
    normalized_direction(params.axis, "axis")?;
    if !params.angle_degrees.is_finite() || params.angle_degrees <= 0.0 {
        return Err(ToolError::Validation(
            "angle_degrees must be a positive finite number".to_string(),
        ));
    }
    if !(-1_048_574..=1_048_574).contains(&params.frame_start)
        || !(-1_048_574..=1_048_574).contains(&params.frame_end)
    {
        return Err(ToolError::Validation(
            "frame_start and frame_end must be between -1048574 and 1048574".to_string(),
        ));
    }
    if params.frame_start >= params.frame_end {
        return Err(ToolError::Validation(
            "frame_start must be less than frame_end".to_string(),
        ));
    }
    Ok(())
}

fn validate_blender_execute(params: &BlenderExecuteParams) -> Result<Duration, ToolError> {
    if params.code.is_empty() {
        return Err(ToolError::Validation("code must be non-empty".to_string()));
    }
    validate_work_budget(params.timeout_seconds)
}

fn validate_render_preview(params: &RenderPreviewParams) -> Result<Duration, ToolError> {
    validate_render_settings(
        params.width,
        params.height,
        params.engine,
        params.samples,
        params.timeout_seconds,
    )
}

fn validate_render_product(params: &RenderProductParams) -> Result<Duration, ToolError> {
    validate_object_subset(Some(&params.objects))?;
    validate_product_presentation(&params.presentation, Some(&params.objects))?;
    if u64::from(params.width) * u64::from(params.height) > MAX_PRODUCT_RENDER_PIXELS {
        return Err(ToolError::Validation(format!(
            "product render exceeds the {MAX_PRODUCT_RENDER_PIXELS}-pixel output limit; reduce width or height"
        )));
    }
    validate_render_settings(
        params.width,
        params.height,
        params.engine,
        params.samples,
        params.timeout_seconds,
    )
}

pub(crate) fn validate_product_presentation(
    presentation: &ProductPresentation,
    selected_objects: Option<&[String]>,
) -> Result<(), ToolError> {
    for (name, value, minimum, maximum) in [
        ("exposure_stops", presentation.exposure_stops, -10.0, 10.0),
        (
            "light_intensity_scale",
            presentation.light_intensity_scale,
            0.0,
            10.0,
        ),
    ] {
        if value.is_some_and(|value| !value.is_finite() || !(minimum..=maximum).contains(&value)) {
            return Err(ToolError::Validation(format!(
                "{name} must be a finite number from {minimum} through {maximum}"
            )));
        }
    }
    if !presentation.view.azimuth_degrees.is_finite()
        || !(-360.0..=360.0).contains(&presentation.view.azimuth_degrees)
    {
        return Err(ToolError::Validation(
            "azimuth_degrees must be a finite number from -360 through 360".to_string(),
        ));
    }
    if !presentation.view.elevation_degrees.is_finite()
        || !(-89.0..=89.0).contains(&presentation.view.elevation_degrees)
    {
        return Err(ToolError::Validation(
            "elevation_degrees must be a finite number from -89 through 89".to_string(),
        ));
    }
    if presentation.profile.ground_material().is_some() && presentation.view.elevation_degrees < 0.0
    {
        return Err(ToolError::Validation(
            "studio presentation elevation_degrees must be from 0 through 89 so the ground cannot occlude the product".to_string(),
        ));
    }
    if presentation.materials.len() > 64 {
        return Err(ToolError::Validation(
            "presentation materials must contain at most 64 entries".to_string(),
        ));
    }
    let selected =
        selected_objects.map(|objects| objects.iter().map(String::as_str).collect::<HashSet<_>>());
    let mut assigned = HashSet::new();
    for material in &presentation.materials {
        validate_object_subset(Some(&material.objects))?;
        for object in &material.objects {
            if selected
                .as_ref()
                .is_some_and(|selected| !selected.contains(object.as_str()))
            {
                return Err(ToolError::Validation(format!(
                    "presentation material object is not selected: {object}"
                )));
            }
            if !assigned.insert(object.as_str()) {
                return Err(ToolError::Validation(format!(
                    "presentation material object is assigned more than once: {object}"
                )));
            }
        }
        if material
            .base_color_srgb
            .iter()
            .any(|component| !component.is_finite() || !(0.0..=1.0).contains(component))
        {
            return Err(ToolError::Validation(
                "base_color_srgb must contain three finite numbers from 0 through 1".to_string(),
            ));
        }
        for (name, value) in [
            ("metallic", material.metallic),
            ("roughness", material.roughness),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(ToolError::Validation(format!(
                    "{name} must be a finite number from 0 through 1"
                )));
            }
        }
    }
    Ok(())
}

fn validate_render_settings(
    width: u16,
    height: u16,
    engine: RenderEngine,
    samples: Option<u16>,
    timeout_seconds: f64,
) -> Result<Duration, ToolError> {
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err(ToolError::Validation(
            "width and height must be integers between 1 and 8192".to_string(),
        ));
    }
    match engine {
        RenderEngine::Eevee if samples.is_some() => {
            return Err(ToolError::Validation(
                "samples is only valid for CYCLES renders".to_string(),
            ));
        }
        RenderEngine::Cycles if samples.is_some_and(|samples| !(1..=4096).contains(&samples)) => {
            return Err(ToolError::Validation(
                "samples must be an integer between 1 and 4096".to_string(),
            ));
        }
        _ => {}
    }
    validate_work_budget(timeout_seconds)
}

#[derive(Clone, serde::Serialize)]
struct RenderViewRequest {
    path: String,
    label: String,
    direction: [f64; 3],
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct RenderBounds {
    minimum: [f64; 3],
    maximum: [f64; 3],
    dimensions: [f64; 3],
    center: [f64; 3],
    diagonal: f64,
    coordinate_space: String,
    unit: String,
}

impl GalleryView {
    fn label_and_direction(self) -> (&'static str, [f64; 3]) {
        match self {
            GalleryView::Front => ("FRONT", [0.0, -1.0, 0.0]),
            GalleryView::Right => ("RIGHT", [1.0, 0.0, 0.0]),
            GalleryView::Back => ("BACK", [0.0, 1.0, 0.0]),
            GalleryView::Left => ("LEFT", [-1.0, 0.0, 0.0]),
            GalleryView::Top => ("TOP", [0.0, 0.0, 1.0]),
            GalleryView::Bottom => ("BOTTOM", [0.0, 0.0, -1.0]),
            GalleryView::Isometric => ("ISOMETRIC", [1.0, -1.0, 1.0]),
        }
    }
}

async fn render_dimensions(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderDimensionsParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    let layout = fit_composite_layout(3, 3, params.render.width, params.render.height)?;
    let views = allocate_render_views(
        &workspace,
        [
            ("FRONT", [0.0, -1.0, 0.0]),
            ("RIGHT", [1.0, 0.0, 0.0]),
            ("TOP", [0.0, 0.0, 1.0]),
        ],
    )?;
    let mut rendered = render_view_artifacts(blender, &params.render, &views, None).await?;
    let bounds = validated_render_bounds(&rendered)?;
    let labels = [
        format!(
            "FRONT | X {} x Z {}",
            compact_measurement(bounds.dimensions[0]),
            compact_measurement(bounds.dimensions[2])
        ),
        format!(
            "RIGHT | Y {} x Z {}",
            compact_measurement(bounds.dimensions[1]),
            compact_measurement(bounds.dimensions[2])
        ),
        format!(
            "TOP | X {} x Y {}",
            compact_measurement(bounds.dimensions[0]),
            compact_measurement(bounds.dimensions[1])
        ),
    ];
    for (view, label) in rendered["views"]
        .as_array_mut()
        .ok_or_else(|| ToolError::Validation("Blender view response omitted views".to_string()))?
        .iter_mut()
        .zip(labels)
    {
        view["label"] = json!(label);
    }
    composite_render_views(
        workspace,
        params.path,
        layout,
        params.include_inline,
        "dimensions",
        rendered,
    )
    .await
}

fn compact_measurement(value: f64) -> String {
    let formatted = format!("{value:.3}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

async fn render_cross_section(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderCrossSectionParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    validate_object_subset(params.objects.as_deref())?;
    if params.position.is_some_and(|value| !value.is_finite()) {
        return Err(ToolError::Validation(
            "position must be a finite number".to_string(),
        ));
    }
    let view_direction = params
        .view_direction
        .map(|direction| normalized_direction(direction, "view_direction"))
        .transpose()?;
    let output_path = params.path.clone();
    let include_inline = params.include_inline;
    let axis = json!(params.axis.label());
    let mut backend = serde_json::to_value(&params)?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            ToolError::Validation("diagnostic parameters must be an object".to_string())
        })?;
    backend.remove("include_inline");
    if let Some(direction) = view_direction {
        backend.insert("view_direction".to_string(), json!(direction));
    }
    let source_path = allocate_diagnostic_source(&workspace)?;
    backend.insert("path".to_string(), json!(source_path.clone()));
    backend.insert("mode".to_string(), json!("cross_section"));
    let rendered = render_diagnostic_source(blender, &params.render, backend).await?;
    let source_bounds = validate_diagnostic_response(
        &rendered,
        "cross_section",
        &source_path,
        &params.render,
        params.objects.as_deref(),
    )?;
    if rendered["analysis"]["axis"] != axis {
        return Err(ToolError::Validation(
            "Blender diagnostic response axis did not match the request".to_string(),
        ));
    }
    let position = rendered["analysis"]["position"].as_f64().ok_or_else(|| {
        ToolError::Validation("Blender diagnostic response omitted cut position".to_string())
    })?;
    let expected_position = params
        .position
        .unwrap_or(source_bounds.center[params.axis.index()]);
    if !approximately_equal(expected_position, position) {
        return Err(ToolError::Validation(
            "Blender diagnostic response position did not match the request".to_string(),
        ));
    }
    let section_faces = rendered["analysis"]["section_faces"]
        .as_u64()
        .ok_or_else(|| {
            ToolError::Validation("Blender diagnostic response omitted section faces".to_string())
        })?;
    let label = format!(
        "SECTION {}={} | {} cap faces",
        params.axis.label(),
        compact_measurement(position),
        section_faces
    );
    composite_diagnostic(
        workspace,
        output_path,
        include_inline,
        "cross_section",
        label,
        rendered,
    )
    .await
}

async fn render_heatmap(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderHeatmapParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    validate_object_subset(params.objects.as_deref())?;
    let build_direction = normalized_direction(params.build_direction, "build_direction")?;
    let view_direction = normalized_direction(params.view_direction, "view_direction")?;
    if !params.overhang_angle_degrees.is_finite()
        || !(0.0..=90.0).contains(&params.overhang_angle_degrees)
    {
        return Err(ToolError::Validation(
            "overhang_angle_degrees must be a finite number from 0 through 90".to_string(),
        ));
    }
    let output_path = params.path.clone();
    let include_inline = params.include_inline;
    let mut backend = serde_json::to_value(&params)?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            ToolError::Validation("diagnostic parameters must be an object".to_string())
        })?;
    backend.remove("include_inline");
    backend.insert("build_direction".to_string(), json!(build_direction));
    backend.insert("view_direction".to_string(), json!(view_direction));
    let source_path = allocate_diagnostic_source(&workspace)?;
    backend.insert("path".to_string(), json!(source_path.clone()));
    backend.insert("mode".to_string(), json!("overhang"));
    let rendered = render_diagnostic_source(blender, &params.render, backend).await?;
    validate_diagnostic_response(
        &rendered,
        "overhang",
        &source_path,
        &params.render,
        params.objects.as_deref(),
    )?;
    let rendered_build_direction: [f64; 3] =
        serde_json::from_value(rendered["analysis"]["build_direction"].clone()).map_err(|_| {
            ToolError::Validation(
                "Blender heatmap response build direction was malformed".to_string(),
            )
        })?;
    if rendered_build_direction
        .iter()
        .zip(build_direction)
        .any(|(actual, expected)| !approximately_equal(*actual, expected))
        || rendered["analysis"]["overhang_angle_degrees"] != json!(params.overhang_angle_degrees)
    {
        return Err(ToolError::Validation(
            "Blender heatmap response parameters did not match the request".to_string(),
        ));
    }
    let warning_faces = rendered["analysis"]["categories"]["warning"]["faces"]
        .as_u64()
        .ok_or_else(|| {
            ToolError::Validation("Blender heatmap response omitted warning faces".to_string())
        })?;
    let severe_faces = rendered["analysis"]["categories"]["severe"]["faces"]
        .as_u64()
        .ok_or_else(|| {
            ToolError::Validation("Blender heatmap response omitted severe faces".to_string())
        })?;
    let label = format!(
        "OVERHANG >{} deg | {} warning + {} severe faces",
        compact_measurement(params.overhang_angle_degrees),
        warning_faces,
        severe_faces
    );
    composite_diagnostic(
        workspace,
        output_path,
        include_inline,
        "printability_heatmap",
        label,
        rendered,
    )
    .await
}

fn validate_object_subset(objects: Option<&[String]>) -> Result<(), ToolError> {
    let Some(objects) = objects else {
        return Ok(());
    };
    if objects.is_empty() || objects.len() > 1000 {
        return Err(ToolError::Validation(
            "objects must contain between 1 and 1000 unique names".to_string(),
        ));
    }
    let mut unique = HashSet::with_capacity(objects.len());
    for name in objects {
        if name.is_empty() || name.len() > 255 || !unique.insert(name) {
            return Err(ToolError::Validation(
                "objects must contain between 1 and 1000 unique names".to_string(),
            ));
        }
    }
    Ok(())
}

fn normalized_direction(direction: [f64; 3], name: &str) -> Result<[f64; 3], ToolError> {
    if direction.iter().any(|component| !component.is_finite()) {
        return Err(ToolError::Validation(format!(
            "{name} must contain three finite numbers with a finite non-zero magnitude"
        )));
    }
    let scale = direction
        .iter()
        .map(|component| component.abs())
        .fold(0.0_f64, f64::max);
    if scale == 0.0 {
        return Err(ToolError::Validation(format!(
            "{name} must contain three finite numbers with a finite non-zero magnitude"
        )));
    }
    let scaled = direction.map(|component| component / scale);
    let magnitude = scaled
        .into_iter()
        .fold(0.0_f64, |length, component| length.hypot(component));
    Ok(scaled.map(|component| component / magnitude))
}

fn allocate_diagnostic_source(workspace: &Workspace) -> Result<String, ToolError> {
    let path = format!("visual/diagnostics/{}/source.png", random_hex_id()?);
    validate_blender_artifact(workspace, &path, ".png", false)?;
    Ok(path)
}

async fn render_diagnostic_source(
    blender: &BlenderClient,
    settings: &MultiViewRenderSettings,
    params: serde_json::Map<String, Value>,
) -> Result<Value, ToolError> {
    validate_render_view_surface(1, settings.width, settings.height)?;
    let work_budget = validate_render_settings(
        settings.width,
        settings.height,
        settings.engine,
        settings.samples,
        settings.timeout_seconds,
    )?;
    Ok(blender
        .send_value_with_work_budget("render_diagnostic", params, work_budget)
        .await?)
}

async fn render_product(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderProductParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    let work_budget = validate_render_product(&params)?;
    let requested_profile = params.presentation.profile;
    let requested_azimuth = params.presentation.view.azimuth_degrees;
    let requested_elevation = params.presentation.view.elevation_degrees;
    let requested_shading = params
        .presentation
        .surface_shading
        .unwrap_or_else(|| requested_profile.default_shading());
    let include_inline = params.include_inline;
    let requested_path = params.path.clone();
    let requested_width = params.width;
    let requested_height = params.height;
    let requested_engine = params.engine;
    let requested_samples = params.samples;
    let requested_objects = params.objects.clone();
    let requested_materials = params.presentation.materials.clone();
    let value = serde_json::to_value(&params)?;
    let mut command_params = value.as_object().cloned().ok_or_else(|| {
        ToolError::Validation("Blender command parameters must be an object".to_string())
    })?;
    command_params.remove("include_inline");
    command_params.insert(
        "max_output_bytes".to_string(),
        json!(MAX_PRODUCT_RENDER_BYTES),
    );
    let rendered = blender
        .send_value_with_work_budget("render_product", command_params, work_budget)
        .await?;
    validate_product_render_response(
        &rendered,
        ProductRenderExpectation {
            path: &requested_path,
            width: requested_width,
            height: requested_height,
            engine: requested_engine,
            samples: requested_samples,
            objects: &requested_objects,
            profile: requested_profile,
            azimuth: requested_azimuth,
            elevation: requested_elevation,
            shading: requested_shading,
            materials: &requested_materials,
            exposure: params.presentation.exposure_stops.unwrap_or(0.0),
            light_intensity_scale: params.presentation.light_intensity_scale.unwrap_or(1.0),
        },
    )?;
    let bytes = verify_product_png_artifact(
        Arc::clone(&workspace),
        requested_path,
        &rendered,
        u64::from(requested_width),
        u64::from(requested_height),
    )
    .await?;
    let mut value = rendered;
    let (inline, inline_png_base64) = capture_inline_png(include_inline, &bytes);
    value["inline"] = inline;
    Ok(ToolOutput {
        value,
        inline_png_base64,
    })
}

struct ProductRenderExpectation<'a> {
    exposure: f64,
    light_intensity_scale: f64,
    path: &'a str,
    width: u16,
    height: u16,
    engine: RenderEngine,
    samples: Option<u16>,
    objects: &'a [String],
    profile: ProductPresentationProfile,
    azimuth: f64,
    elevation: f64,
    shading: ProductSurfaceShading,
    materials: &'a [ProductMaterialOverride],
}

fn validate_product_render_response(
    response: &Value,
    expected: ProductRenderExpectation<'_>,
) -> Result<(), ToolError> {
    let malformed = || ToolError::Validation("Blender product response is malformed".to_string());
    let response_object = response.as_object().ok_or_else(malformed)?;
    if response_object.get("path").and_then(Value::as_str) != Some(expected.path)
        || response_object.get("media_type").and_then(Value::as_str) != Some("image/png")
        || response_object.get("width").and_then(Value::as_u64) != Some(u64::from(expected.width))
        || response_object.get("height").and_then(Value::as_u64) != Some(u64::from(expected.height))
        || response_object.get("size_bytes").and_then(Value::as_u64) == Some(0)
        || response_object
            .get("size_bytes")
            .and_then(Value::as_u64)
            .is_none()
        || response_object
            .get("sha256")
            .and_then(Value::as_str)
            .is_none_or(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        || response_object
            .get("source_state_verified")
            .and_then(Value::as_bool)
            != Some(true)
        || response_object
            .get("cleanup_verified")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(malformed());
    }
    validate_render_backend(response_object, expected.engine, expected.samples)?;
    let mut expected_objects = expected.objects.to_vec();
    expected_objects.sort();
    if response_object.get("objects") != Some(&json!(expected_objects)) {
        return Err(malformed());
    }
    let render_bounds = validated_render_bounds(response)?;

    let presentation = response_object
        .get("presentation")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    if presentation.get("profile").and_then(Value::as_str) != Some(expected.profile.name()) {
        return Err(malformed());
    }
    let camera = presentation
        .get("camera")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    if camera.get("type").and_then(Value::as_str) != Some(expected.profile.camera_type())
        || camera.get("azimuth_degrees").and_then(Value::as_f64) != Some(expected.azimuth)
        || camera.get("elevation_degrees").and_then(Value::as_f64) != Some(expected.elevation)
        || !finite_triplet(camera.get("position"))
        || camera.get("target") != Some(&json!(render_bounds.center))
        || camera
            .get("clip_start")
            .and_then(Value::as_f64)
            .is_none_or(|value| !value.is_finite() || value <= 0.0)
        || camera
            .get("clip_end")
            .and_then(Value::as_f64)
            .is_none_or(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(malformed());
    }
    let camera_clip_start = camera["clip_start"].as_f64().ok_or_else(malformed)?;
    let camera_clip_end = camera["clip_end"].as_f64().ok_or_else(malformed)?;
    let camera_shape_valid = match expected.profile {
        ProductPresentationProfile::Engineering => {
            camera.get("lens_mm") == Some(&Value::Null)
                && camera
                    .get("ortho_scale")
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.is_finite() && value > 0.0)
        }
        ProductPresentationProfile::StudioNeutral | ProductPresentationProfile::StudioDark => {
            camera.get("lens_mm").and_then(Value::as_f64) == expected.profile.lens_mm()
                && camera.get("ortho_scale") == Some(&Value::Null)
                && camera
                    .get("sensor_width_mm")
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.is_finite() && value > 0.0)
        }
    };
    if !camera_shape_valid || camera_clip_end <= camera_clip_start {
        return Err(malformed());
    }
    let lighting = presentation
        .get("lighting")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let expected_light_count =
        if matches!(expected.profile, ProductPresentationProfile::Engineering) {
            2
        } else {
            3
        };
    let expected_roles = if matches!(expected.profile, ProductPresentationProfile::Engineering) {
        &["key", "fill"][..]
    } else {
        &["key", "fill", "rim"][..]
    };
    if lighting.len() != expected_light_count
        || lighting
            .iter()
            .zip(expected_roles)
            .any(|(light, expected_role)| {
                let Some(light) = light.as_object() else {
                    return true;
                };
                light.get("role").and_then(Value::as_str) != Some(*expected_role)
                    || light.get("type").and_then(Value::as_str) != Some("AREA")
                    || light.get("shape").and_then(Value::as_str) != Some("DISK")
                    || !finite_triplet(light.get("position"))
                    || light
                        .get("energy_watts")
                        .and_then(Value::as_f64)
                        .is_none_or(|value| {
                            !value.is_finite()
                                || if expected.light_intensity_scale == 0.0 {
                                    value != 0.0
                                } else {
                                    value <= 0.0
                                }
                        })
                    || light
                        .get("size")
                        .and_then(Value::as_f64)
                        .is_none_or(|value| !value.is_finite() || value <= 0.0)
            })
    {
        return Err(malformed());
    }
    let color = presentation
        .get("color_management")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    if color.get("display_device").and_then(Value::as_str) != Some("sRGB")
        || color.get("view_transform").and_then(Value::as_str)
            != Some(expected.profile.view_transform())
        || color.get("look").and_then(Value::as_str) != Some("None")
        || color
            .get("exposure")
            .and_then(Value::as_f64)
            .is_none_or(|value| !value.is_finite() || (value - expected.exposure).abs() > 1e-5)
        || presentation
            .get("light_intensity_scale")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            != expected.light_intensity_scale
        || color.get("gamma").and_then(Value::as_f64) != Some(1.0)
    {
        return Err(malformed());
    }
    let ground = presentation
        .get("ground")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let ground_valid =
        if let Some((base_color, metallic, roughness)) = expected.profile.ground_material() {
            ground.get("enabled").and_then(Value::as_bool) == Some(true)
                && ground.get("style").and_then(Value::as_str) == Some("seamless")
                && ground
                    .get("z")
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite)
                && ground
                    .get("size")
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.is_finite() && value > 0.0)
                && ground.get("base_color_srgb") == Some(&json!(base_color))
                && ground.get("metallic").and_then(Value::as_f64) == Some(metallic)
                && ground.get("roughness").and_then(Value::as_f64) == Some(roughness)
        } else {
            ground.len() == 1 && ground.get("enabled").and_then(Value::as_bool) == Some(false)
        };
    if !ground_valid {
        return Err(malformed());
    }
    if !product_materials_and_shading_match(presentation, expected.materials, expected.shading) {
        return Err(malformed());
    }
    let framing = presentation
        .get("framing")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    if framing.get("margin_percent").and_then(Value::as_f64) != Some(15.0)
        || framing.get("bounds") != response_object.get("bounds")
        || framing
            .get("instance_count")
            .and_then(Value::as_u64)
            .is_none_or(|count| count < expected.objects.len() as u64)
    {
        return Err(malformed());
    }
    let geometry = presentation
        .get("geometry")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let geometry_fields = [
        ("instances", MAX_PRODUCT_INSTANCES),
        ("unique_evaluated_meshes", MAX_PRODUCT_INSTANCES),
        ("vertices", MAX_PRODUCT_VERTICES),
        ("edges", MAX_PRODUCT_EDGES),
        ("faces", MAX_PRODUCT_FACES),
        ("loops", MAX_PRODUCT_LOOPS),
        ("attribute_values", MAX_PRODUCT_ATTRIBUTE_VALUES),
        ("material_slots", MAX_PRODUCT_MATERIAL_SLOTS),
    ];
    if geometry_fields.iter().any(|(field, maximum)| {
        geometry
            .get(*field)
            .and_then(Value::as_u64)
            .is_none_or(|count| count > *maximum)
    }) || geometry.get("instances") != framing.get("instance_count")
        || geometry.get("instances").and_then(Value::as_u64) == Some(0)
        || geometry
            .get("unique_evaluated_meshes")
            .and_then(Value::as_u64)
            == Some(0)
        || geometry.get("vertices").and_then(Value::as_u64) == Some(0)
    {
        return Err(malformed());
    }
    let world = presentation
        .get("world")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let (world_color, world_strength) = expected.profile.world();
    if world.get("base_color_srgb") != Some(&json!(world_color))
        || world.get("strength").and_then(Value::as_f64)
            != Some(world_strength * expected.light_intensity_scale)
    {
        return Err(malformed());
    }
    let materials = presentation
        .get("materials")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let preserved = validated_sorted_names(materials.get("preserved_objects"), &malformed)?;
    let fallback = materials
        .get("fallback")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let fallback_objects = validated_sorted_names(fallback.get("objects"), &malformed)?;
    if fallback.get("base_color_srgb") != Some(&json!([0.42, 0.45, 0.5]))
        || fallback.get("metallic").and_then(Value::as_f64) != Some(0.0)
        || fallback.get("roughness").and_then(Value::as_f64) != Some(0.5)
    {
        return Err(malformed());
    }
    let expected_set = expected_objects.iter().cloned().collect::<HashSet<_>>();
    let override_set = expected
        .materials
        .iter()
        .flat_map(|material| material.objects.iter().cloned())
        .collect::<HashSet<_>>();
    let preserved_set = preserved.into_iter().collect::<HashSet<_>>();
    let fallback_set = fallback_objects.into_iter().collect::<HashSet<_>>();
    if !preserved_set.is_disjoint(&override_set)
        || !fallback_set.is_disjoint(&override_set)
        || !preserved_set.is_subset(&expected_set)
        || !fallback_set.is_subset(&expected_set)
    {
        return Err(malformed());
    }
    let mut classified = preserved_set;
    classified.extend(fallback_set);
    for requested in expected.materials {
        classified.extend(requested.objects.iter().cloned());
    }
    if classified != expected_set {
        return Err(malformed());
    }
    Ok(())
}

pub(crate) fn product_controls_match(
    actual: &serde_json::Map<String, Value>,
    expected: &ProductPresentation,
) -> bool {
    expected.exposure_stops.is_none_or(|expected| {
        actual
            .get("color_management")
            .and_then(|color| color.get("exposure"))
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && (value - expected).abs() <= 1e-5)
    }) && expected.light_intensity_scale.is_none_or(|expected| {
        actual.get("light_intensity_scale").and_then(Value::as_f64) == Some(expected)
    })
}

pub(crate) fn product_materials_and_shading_match(
    presentation: &serde_json::Map<String, Value>,
    expected_materials: &[ProductMaterialOverride],
    expected_shading: ProductSurfaceShading,
) -> bool {
    let Some(shading) = presentation.get("shading").and_then(Value::as_object) else {
        return false;
    };
    let expected_angle = match expected_shading {
        ProductSurfaceShading::Preserve => Value::Null,
        ProductSurfaceShading::SmoothByAngle => json!(30.0),
    };
    if shading.get("mode").and_then(Value::as_str) != Some(expected_shading.name())
        || shading.get("angle_degrees") != Some(&expected_angle)
        || shading.get("presentation_only").and_then(Value::as_bool) != Some(true)
    {
        return false;
    }
    let Some(overrides) = presentation
        .get("materials")
        .and_then(Value::as_object)
        .and_then(|materials| materials.get("overrides"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if overrides.len() != expected_materials.len() {
        return false;
    }
    overrides
        .iter()
        .zip(expected_materials)
        .all(|(reported, requested)| {
            let Some(reported) = reported.as_object() else {
                return false;
            };
            let mut requested_objects = requested.objects.clone();
            requested_objects.sort();
            reported.get("objects") == Some(&json!(requested_objects))
                && reported.get("base_color_srgb") == Some(&json!(requested.base_color_srgb))
                && reported.get("metallic").and_then(Value::as_f64) == Some(requested.metallic)
                && reported.get("roughness").and_then(Value::as_f64) == Some(requested.roughness)
        })
}

fn finite_triplet(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|values| {
        values.len() == 3
            && values
                .iter()
                .all(|value| value.as_f64().is_some_and(f64::is_finite))
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(crate) async fn verify_product_png_artifact(
    workspace: Arc<Workspace>,
    path: String,
    rendered: &Value,
    expected_width: u64,
    expected_height: u64,
) -> Result<Vec<u8>, ToolError> {
    let reported_size = rendered["size_bytes"]
        .as_u64()
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            ToolError::Validation(
                "Blender product response omitted a positive size_bytes".to_string(),
            )
        })?;
    let reported_sha256 = rendered["sha256"]
        .as_str()
        .ok_or_else(|| {
            ToolError::Validation("Blender product response omitted sha256".to_string())
        })?
        .to_string();
    visual_blocking(move || {
        let snapshot = workspace.snapshot_artifact_bounded(&path, MAX_PRODUCT_RENDER_BYTES)?;
        let bytes = std::fs::read(snapshot.path())?;
        if bytes.len() as u64 != reported_size || sha256_hex(&bytes) != reported_sha256 {
            return Err(ToolError::Validation(
                "product render changed before artifact verification".to_string(),
            ));
        }
        validate_png(&bytes, expected_width, expected_height)?;
        Ok(bytes)
    })
    .await
}

fn validated_sorted_names(
    value: Option<&Value>,
    malformed: &impl Fn() -> ToolError,
) -> Result<Vec<String>, ToolError> {
    let names = value.and_then(Value::as_array).ok_or_else(malformed)?;
    let mut result = names
        .iter()
        .map(|name| {
            name.as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .ok_or_else(malformed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if result.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(malformed());
    }
    result.shrink_to_fit();
    Ok(result)
}

async fn render_gallery(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderGalleryParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    if params.views.len() > 7 {
        return Err(ToolError::Validation(
            "views must contain between 1 and 7 entries".to_string(),
        ));
    }
    if params.views.iter().copied().collect::<HashSet<_>>().len() != params.views.len() {
        return Err(ToolError::Validation(
            "gallery views must be unique".to_string(),
        ));
    }
    if !(1..=7).contains(&params.columns) {
        return Err(ToolError::Validation(
            "columns must be an integer between 1 and 7".to_string(),
        ));
    }
    let layout = fit_composite_layout(
        params.views.len(),
        params.columns,
        params.render.width,
        params.render.height,
    )?;
    let specs = params
        .views
        .iter()
        .map(|view| view.label_and_direction())
        .collect::<Vec<_>>();
    if let Some(presentation) = &params.presentation {
        validate_multiview_presentation(
            presentation,
            specs.iter().map(|(_label, direction)| *direction),
        )?;
    }
    let preset_views = serde_json::to_value(&params.views)?;
    let views = allocate_render_views(&workspace, specs)?;
    let rendered = render_view_artifacts(
        blender,
        &params.render,
        &views,
        params.presentation.as_ref(),
    )
    .await?;
    let mut output = composite_render_views(
        workspace,
        params.path,
        layout,
        params.include_inline,
        "gallery",
        rendered,
    )
    .await?;
    output.value["preset_views"] = preset_views;
    Ok(output)
}

async fn render_turntable(
    workspace: Arc<Workspace>,
    blender: &BlenderClient,
    params: RenderTurntableParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    if !(3..=MAX_RENDER_VIEWS as u8).contains(&params.frames) {
        return Err(ToolError::Validation(
            "frames must be an integer between 3 and 36".to_string(),
        ));
    }
    if !(1..=12).contains(&params.columns) {
        return Err(ToolError::Validation(
            "columns must be an integer between 1 and 12".to_string(),
        ));
    }
    if !params.elevation_degrees.is_finite() || !(-89.0..=89.0).contains(&params.elevation_degrees)
    {
        return Err(ToolError::Validation(
            "elevation_degrees must be a finite number from -89 through 89".to_string(),
        ));
    }
    let layout = fit_composite_layout(
        usize::from(params.frames),
        params.columns,
        params.render.width,
        params.render.height,
    )?;
    let elevation = params.elevation_degrees.to_radians();
    let specs = (0..params.frames)
        .map(|frame| {
            let fraction = f64::from(frame) / f64::from(params.frames);
            let base_angle = fraction * TAU;
            let angle = if params.clockwise {
                -base_angle
            } else {
                base_angle
            };
            let degrees = fraction * 360.0;
            let angle_label = if degrees.fract() == 0.0 {
                format!("{degrees:.0}°")
            } else {
                format!("{degrees:.1}°")
            };
            let label = if params.clockwise && frame > 0 {
                format!("{angle_label} CW")
            } else {
                angle_label
            };
            (
                label,
                [
                    angle.sin() * elevation.cos(),
                    -angle.cos() * elevation.cos(),
                    elevation.sin(),
                ],
            )
        })
        .collect::<Vec<_>>();
    if let Some(presentation) = &params.presentation {
        validate_multiview_presentation(
            presentation,
            specs.iter().map(|(_label, direction)| *direction),
        )?;
    }
    let views = allocate_render_views(
        &workspace,
        specs
            .iter()
            .map(|(label, direction)| (label.as_str(), *direction)),
    )?;
    let rendered = render_view_artifacts(
        blender,
        &params.render,
        &views,
        params.presentation.as_ref(),
    )
    .await?;
    let orbit = json!({
        "frames": params.frames,
        "elevation_degrees": params.elevation_degrees,
        "clockwise": params.clockwise,
    });
    let mut output = composite_render_views(
        workspace,
        params.path,
        layout,
        params.include_inline,
        "turntable",
        rendered,
    )
    .await?;
    output.value["orbit"] = orbit;
    Ok(output)
}

fn allocate_render_views<'a>(
    workspace: &Workspace,
    specs: impl IntoIterator<Item = (&'a str, [f64; 3])>,
) -> Result<Vec<RenderViewRequest>, ToolError> {
    let batch_id = random_hex_id()?;
    specs
        .into_iter()
        .enumerate()
        .map(|(index, (label, direction))| {
            let view = RenderViewRequest {
                path: format!("visual/renders/{batch_id}/{:02}.png", index + 1),
                label: label.to_string(),
                direction,
            };
            validate_blender_artifact(workspace, &view.path, ".png", false)?;
            Ok(view)
        })
        .collect()
}

fn fit_composite_layout(
    view_count: usize,
    requested_columns: u8,
    tile_width: u16,
    tile_height: u16,
) -> Result<TileLayout, ToolError> {
    if view_count == 0 || requested_columns == 0 || tile_width == 0 || tile_height == 0 {
        return Err(ToolError::Validation(
            "a visual composite requires views, columns, and non-zero tile dimensions".to_string(),
        ));
    }
    let columns = u32::from(requested_columns).min(view_count as u32);
    let rows = (view_count as u64).div_ceil(u64::from(columns));
    let source_width = u32::from(tile_width);
    let source_height = u32::from(tile_height);
    let width_is_longest = source_width >= source_height;
    let source_longest = source_width.max(source_height);
    let dimensions = |longest: u32| {
        if width_is_longest {
            (
                longest,
                (u64::from(source_height) * u64::from(longest) / u64::from(source_width)).max(1)
                    as u32,
            )
        } else {
            (
                (u64::from(source_width) * u64::from(longest) / u64::from(source_height)).max(1)
                    as u32,
                longest,
            )
        }
    };
    let fits = |longest: u32| {
        let (width, height) = dimensions(longest);
        u64::from(columns)
            * u64::from(width)
            * rows
            * (u64::from(height) + u64::from(constants::LABEL_BAR_HEIGHT))
            <= MAX_COMPOSITE_PIXELS
    };
    let longest = (1..=source_longest)
        .rev()
        .find(|candidate| fits(*candidate))
        .ok_or_else(|| {
            ToolError::Validation(
                "visual composite cannot fit within the generated-artifact limit".to_string(),
            )
        })?;
    let (tile_w, tile_h) = dimensions(longest);
    Ok(TileLayout {
        columns,
        tile_w,
        tile_h,
        label_height: constants::LABEL_BAR_HEIGHT,
    })
}

async fn render_view_artifacts(
    blender: &BlenderClient,
    settings: &MultiViewRenderSettings,
    views: &[RenderViewRequest],
    presentation: Option<&ProductPresentation>,
) -> Result<Value, ToolError> {
    validate_render_view_surface(views.len(), settings.width, settings.height)?;
    let work_budget = validate_render_settings(
        settings.width,
        settings.height,
        settings.engine,
        settings.samples,
        settings.timeout_seconds,
    )?;
    let mut params = serde_json::Map::new();
    params.insert("views".to_string(), serde_json::to_value(views)?);
    if let Some(expected) = &settings.expected_scene {
        params.insert(
            "expected_scene".to_string(),
            serde_json::to_value(expected)?,
        );
    }
    params.insert("width".to_string(), json!(settings.width));
    params.insert("height".to_string(), json!(settings.height));
    params.insert("engine".to_string(), serde_json::to_value(settings.engine)?);
    if let Some(samples) = settings.samples {
        params.insert("samples".to_string(), json!(samples));
    }
    params.insert(
        "timeout_seconds".to_string(),
        json!(settings.timeout_seconds),
    );
    if let Some(presentation) = presentation {
        params.insert(
            "presentation".to_string(),
            serde_json::to_value(presentation)?,
        );
    }
    let result = blender
        .send_value_with_work_budget("render_views", params, work_budget)
        .await?;
    validate_render_views_response(&result, views, settings, presentation)?;
    Ok(result)
}

fn validate_multiview_presentation(
    presentation: &ProductPresentation,
    directions: impl IntoIterator<Item = [f64; 3]>,
) -> Result<(), ToolError> {
    validate_product_presentation(presentation, None)?;
    if presentation.profile.ground_material().is_some()
        && directions.into_iter().any(|direction| direction[2] < 0.0)
    {
        return Err(ToolError::Validation(
            "grounded studio presentation cannot render a below-ground gallery or turntable view"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_render_view_surface(
    view_count: usize,
    width: u16,
    height: u16,
) -> Result<(), ToolError> {
    let view_pixels = u64::from(width) * u64::from(height);
    if view_pixels > MAX_REVIEW_SOURCE_PIXELS {
        return Err(ToolError::Validation(format!(
            "each review source must be at most {MAX_REVIEW_SOURCE_PIXELS} pixels so its RGB8 PNG remains compositable; use printable_render_preview for a larger single image"
        )));
    }
    let render_pixels = view_count as u64 * view_pixels;
    if render_pixels > MAX_RENDER_VIEW_PIXELS {
        return Err(ToolError::Validation(format!(
            "view renders exceed the {MAX_RENDER_VIEW_PIXELS}-pixel aggregate surface limit"
        )));
    }
    Ok(())
}

fn validate_render_views_response(
    response: &Value,
    requested: &[RenderViewRequest],
    settings: &MultiViewRenderSettings,
    presentation: Option<&ProductPresentation>,
) -> Result<(), ToolError> {
    let object = response.as_object().ok_or_else(|| {
        ToolError::Validation("Blender view response must be an object".to_string())
    })?;
    validate_render_backend(object, settings.engine, settings.samples)?;
    let rendered = object
        .get("views")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Validation("Blender view response omitted views".to_string()))?;
    if rendered.len() != requested.len() {
        return Err(ToolError::Validation(
            "Blender view response count did not match the request".to_string(),
        ));
    }
    let render_bounds = validated_render_bounds(response)?;
    for (actual, expected) in rendered.iter().zip(requested) {
        let view = actual.as_object().ok_or_else(|| {
            ToolError::Validation("Blender returned a malformed view artifact".to_string())
        })?;
        if view.get("path").and_then(Value::as_str) != Some(expected.path.as_str())
            || view.get("label").and_then(Value::as_str) != Some(expected.label.as_str())
            || view.get("media_type").and_then(Value::as_str) != Some("image/png")
            || view.get("width").and_then(Value::as_u64) != Some(u64::from(settings.width))
            || view.get("height").and_then(Value::as_u64) != Some(u64::from(settings.height))
            || view
                .get("size_bytes")
                .and_then(Value::as_u64)
                .is_none_or(|size| size == 0)
        {
            return Err(ToolError::Validation(
                "Blender view artifact metadata did not match the request".to_string(),
            ));
        }
    }
    if let Some(expected) = presentation {
        let actual = object
            .get("presentation")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                ToolError::Validation(
                    "Blender product view response omitted presentation metadata".to_string(),
                )
            })?;
        let presentation_views = actual
            .get("views")
            .and_then(Value::as_array)
            .filter(|views| views.len() == requested.len());
        if actual.get("profile").and_then(Value::as_str) != Some(expected.profile.name())
            || presentation_views.is_none()
        {
            return Err(ToolError::Validation(
                "Blender product view presentation did not match the request".to_string(),
            ));
        }
        for (actual_view, requested_view) in presentation_views
            .expect("presentation view count checked")
            .iter()
            .zip(requested)
        {
            validate_product_view_presentation(
                actual_view,
                requested_view,
                expected,
                &render_bounds,
                object.get("bounds"),
            )?;
        }
    } else if object.contains_key("presentation") {
        return Err(ToolError::Validation(
            "legacy Blender view response unexpectedly applied presentation".to_string(),
        ));
    }
    Ok(())
}

fn validate_product_view_presentation(
    actual: &Value,
    requested: &RenderViewRequest,
    expected: &ProductPresentation,
    render_bounds: &RenderBounds,
    render_bounds_value: Option<&Value>,
) -> Result<(), ToolError> {
    let malformed = || {
        ToolError::Validation(
            "Blender product view presentation did not match the request".to_string(),
        )
    };
    let actual = actual.as_object().ok_or_else(malformed)?;
    let expected_shading = expected
        .surface_shading
        .unwrap_or_else(|| expected.profile.default_shading());
    if actual.get("profile").and_then(Value::as_str) != Some(expected.profile.name())
        || actual.get("source_state_verified").and_then(Value::as_bool) != Some(true)
        || actual.get("cleanup_verified").and_then(Value::as_bool) != Some(true)
        || !product_materials_and_shading_match(actual, &expected.materials, expected_shading)
        || !product_controls_match(actual, expected)
    {
        return Err(malformed());
    }
    let direction = requested.direction;
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    let azimuth = direction[1].atan2(direction[0]).to_degrees();
    let elevation = (direction[2] / magnitude)
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees();
    let camera = actual
        .get("camera")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let observed_azimuth = camera
        .get("azimuth_degrees")
        .and_then(Value::as_f64)
        .ok_or_else(malformed)?;
    let observed_elevation = camera
        .get("elevation_degrees")
        .and_then(Value::as_f64)
        .ok_or_else(malformed)?;
    let clip_start = camera
        .get("clip_start")
        .and_then(Value::as_f64)
        .ok_or_else(malformed)?;
    let clip_end = camera
        .get("clip_end")
        .and_then(Value::as_f64)
        .ok_or_else(malformed)?;
    let camera_shape_valid = match expected.profile {
        ProductPresentationProfile::Engineering => {
            camera.get("lens_mm") == Some(&Value::Null)
                && camera
                    .get("ortho_scale")
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.is_finite() && value > 0.0)
        }
        ProductPresentationProfile::StudioNeutral | ProductPresentationProfile::StudioDark => {
            camera.get("lens_mm").and_then(Value::as_f64) == expected.profile.lens_mm()
                && camera.get("ortho_scale") == Some(&Value::Null)
                && camera
                    .get("sensor_width_mm")
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.is_finite() && value > 0.0)
        }
    };
    if camera.get("behavior").and_then(Value::as_str) != Some("profile")
        || camera.get("type").and_then(Value::as_str) != Some(expected.profile.camera_type())
        || !approximately_equal(observed_azimuth, azimuth)
        || !approximately_equal(observed_elevation, elevation)
        || !finite_triplet(camera.get("position"))
        || camera.get("target") != Some(&json!(render_bounds.center))
        || !clip_start.is_finite()
        || clip_start <= 0.0
        || !clip_end.is_finite()
        || clip_end <= clip_start
        || !camera_shape_valid
    {
        return Err(malformed());
    }
    let framing = actual
        .get("framing")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    if framing.get("margin_percent").and_then(Value::as_f64) != Some(15.0)
        || framing.get("bounds") != render_bounds_value
        || framing
            .get("instance_count")
            .and_then(Value::as_u64)
            .is_none_or(|count| count == 0)
    {
        return Err(malformed());
    }
    Ok(())
}

fn validated_render_bounds(response: &Value) -> Result<RenderBounds, ToolError> {
    validated_bounds(
        response.get("bounds"),
        "Blender view response bounds were malformed",
    )
}

fn validated_bounds(value: Option<&Value>, message: &str) -> Result<RenderBounds, ToolError> {
    let bounds: RenderBounds = serde_json::from_value(
        value
            .cloned()
            .ok_or_else(|| ToolError::Validation(message.to_string()))?,
    )
    .map_err(|_| ToolError::Validation(message.to_string()))?;
    if bounds.coordinate_space != "world"
        || bounds.unit != "blender_unit"
        || !bounds.diagonal.is_finite()
        || bounds.diagonal < 0.1
    {
        return Err(ToolError::Validation(message.to_string()));
    }
    for axis in 0..3 {
        let minimum = bounds.minimum[axis];
        let maximum = bounds.maximum[axis];
        let dimension = bounds.dimensions[axis];
        let center = bounds.center[axis];
        if !minimum.is_finite()
            || !maximum.is_finite()
            || !dimension.is_finite()
            || !center.is_finite()
            || maximum < minimum
            || !approximately_equal(dimension, maximum - minimum)
            || !approximately_equal(center, (minimum + maximum) * 0.5)
        {
            return Err(ToolError::Validation(message.to_string()));
        }
    }
    let expected_diagonal = bounds
        .dimensions
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt()
        .max(0.1);
    if !approximately_equal(bounds.diagonal, expected_diagonal) {
        return Err(ToolError::Validation(message.to_string()));
    }
    Ok(bounds)
}

fn approximately_equal(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * left.abs().max(right.abs()).max(1.0)
}

fn diagnostic_bounds_tolerance(source: &RenderBounds) -> f64 {
    let coordinate_scale = source
        .minimum
        .iter()
        .chain(source.maximum.iter())
        .map(|coordinate| coordinate.abs())
        .fold(source.diagonal.max(1.0), f64::max);
    f64::from(f32::EPSILON) * BLENDER_BOUNDS_TOLERANCE_ULPS * coordinate_scale
}

fn diagnostic_bounds_match(source: &RenderBounds, rendered: &RenderBounds) -> bool {
    let tolerance = diagnostic_bounds_tolerance(source);
    (0..3).all(|axis| {
        (source.minimum[axis] - rendered.minimum[axis]).abs() <= tolerance
            && (source.maximum[axis] - rendered.maximum[axis]).abs() <= tolerance
    })
}

fn cross_section_bounds_are_compatible(
    source: &RenderBounds,
    rendered: &RenderBounds,
    axis: usize,
    position: f64,
    section_faces: u64,
) -> bool {
    let tolerance = diagnostic_bounds_tolerance(source);
    let contained = (0..3).all(|index| {
        rendered.minimum[index] >= source.minimum[index] - tolerance
            && rendered.maximum[index] <= source.maximum[index] + tolerance
    });
    contained
        && rendered.maximum[axis] <= position + tolerance
        && (section_faces == 0 || (rendered.maximum[axis] - position).abs() <= tolerance)
}

fn validate_diagnostic_response(
    response: &Value,
    expected_mode: &str,
    expected_path: &str,
    settings: &MultiViewRenderSettings,
    expected_objects: Option<&[String]>,
) -> Result<RenderBounds, ToolError> {
    let malformed =
        || ToolError::Validation("Blender diagnostic response was malformed".to_string());
    let object = response.as_object().ok_or_else(malformed)?;
    validate_render_backend(object, settings.engine, settings.samples)?;
    if object.get("path").and_then(Value::as_str) != Some(expected_path)
        || object.get("media_type").and_then(Value::as_str) != Some("image/png")
        || object.get("width").and_then(Value::as_u64) != Some(u64::from(settings.width))
        || object.get("height").and_then(Value::as_u64) != Some(u64::from(settings.height))
        || object
            .get("size_bytes")
            .and_then(Value::as_u64)
            .is_none_or(|size| size == 0)
        || object.get("mode").and_then(Value::as_str) != Some(expected_mode)
    {
        return Err(malformed());
    }
    let expected_objects = match expected_objects {
        None => Value::Null,
        Some(names) => {
            let mut names = names.to_vec();
            names.sort();
            serde_json::to_value(names)?
        }
    };
    if object.get("objects") != Some(&expected_objects) {
        return Err(malformed());
    }
    let source_bounds = validated_bounds(
        object.get("source_bounds"),
        "Blender diagnostic response source bounds were malformed",
    )?;
    let rendered_bounds = validated_bounds(
        object.get("rendered_bounds"),
        "Blender diagnostic response rendered bounds were malformed",
    )?;
    let analysis = object
        .get("analysis")
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let source_instances = analysis
        .get("source_instances")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let vertices = analysis
        .get("evaluated_vertices")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let edges = analysis
        .get("evaluated_edges")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let faces = analysis
        .get("evaluated_faces")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let loops = analysis
        .get("evaluated_loops")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let attribute_values = analysis
        .get("copied_attribute_values")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    if source_instances == 0
        || vertices == 0
        || edges == 0
        || faces == 0
        || loops < faces
        || vertices > MAX_DIAGNOSTIC_VERTICES
        || edges > MAX_DIAGNOSTIC_EDGES
        || faces > MAX_DIAGNOSTIC_FACES
        || loops > MAX_DIAGNOSTIC_LOOPS
        || attribute_values > MAX_DIAGNOSTIC_ATTRIBUTE_VALUES
    {
        return Err(malformed());
    }
    match expected_mode {
        "cross_section" => {
            let axis = analysis
                .get("axis")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let axis_index = match axis {
                "X" => 0,
                "Y" => 1,
                "Z" => 2,
                _ => return Err(malformed()),
            };
            let position = analysis
                .get("position")
                .and_then(Value::as_f64)
                .ok_or_else(malformed)?;
            let section_faces = analysis
                .get("section_faces")
                .and_then(Value::as_u64)
                .ok_or_else(malformed)?;
            let section_area = analysis
                .get("section_area")
                .and_then(Value::as_f64)
                .ok_or_else(malformed)?;
            if !position.is_finite()
                || position <= source_bounds.minimum[axis_index]
                || position >= source_bounds.maximum[axis_index]
                || section_faces > faces
                || !section_area.is_finite()
                || section_area < 0.0
                || !cross_section_bounds_are_compatible(
                    &source_bounds,
                    &rendered_bounds,
                    axis_index,
                    position,
                    section_faces,
                )
            {
                return Err(malformed());
            }
        }
        "overhang" => {
            let build_direction: [f64; 3] = serde_json::from_value(
                analysis
                    .get("build_direction")
                    .cloned()
                    .ok_or_else(malformed)?,
            )
            .map_err(|_| malformed())?;
            normalized_direction(build_direction, "build_direction")?;
            let threshold = analysis
                .get("overhang_angle_degrees")
                .and_then(Value::as_f64)
                .ok_or_else(malformed)?;
            if !threshold.is_finite() || !(0.0..=90.0).contains(&threshold) {
                return Err(malformed());
            }
            let categories = analysis
                .get("categories")
                .and_then(Value::as_object)
                .ok_or_else(malformed)?;
            let mut categorized_faces = 0_u64;
            for category in ["supported", "warning", "severe"] {
                let category = categories
                    .get(category)
                    .and_then(Value::as_object)
                    .ok_or_else(malformed)?;
                categorized_faces = categorized_faces
                    .checked_add(
                        category
                            .get("faces")
                            .and_then(Value::as_u64)
                            .ok_or_else(malformed)?,
                    )
                    .ok_or_else(malformed)?;
                let area = category
                    .get("area")
                    .and_then(Value::as_f64)
                    .ok_or_else(malformed)?;
                if !area.is_finite() || area < 0.0 {
                    return Err(malformed());
                }
            }
            if categorized_faces != faces {
                return Err(malformed());
            }
            if !diagnostic_bounds_match(&source_bounds, &rendered_bounds) {
                return Err(malformed());
            }
        }
        _ => return Err(malformed()),
    }
    Ok(source_bounds)
}

async fn composite_render_views(
    workspace: Arc<Workspace>,
    output_path: String,
    layout: TileLayout,
    include_inline: bool,
    kind: &'static str,
    rendered: Value,
) -> Result<ToolOutput, ToolError> {
    let rendered_views = rendered["views"]
        .as_array()
        .ok_or_else(|| ToolError::Validation("Blender view response omitted views".to_string()))?
        .clone();
    let engine = rendered["engine"].clone();
    let render_device = rendered["render_device"].clone();
    let graphics_backend = rendered["graphics_backend"].clone();
    let samples = rendered["samples"].clone();
    let bounds = rendered["bounds"].clone();
    let presentation = rendered.get("presentation").cloned();
    let scene_state = rendered.get("scene_state").cloned();
    visual_blocking(move || {
        let mut tiles = Vec::with_capacity(rendered_views.len());
        for view in &rendered_views {
            let path = view["path"].as_str().ok_or_else(|| {
                ToolError::Validation("Blender view response omitted path".to_string())
            })?;
            let label = view["label"].as_str().ok_or_else(|| {
                ToolError::Validation("Blender view response omitted label".to_string())
            })?;
            let reported_size = view["size_bytes"].as_u64().ok_or_else(|| {
                ToolError::Validation("Blender view response omitted size_bytes".to_string())
            })?;
            let width = view["width"].as_u64().ok_or_else(|| {
                ToolError::Validation("Blender view response omitted width".to_string())
            })?;
            let height = view["height"].as_u64().ok_or_else(|| {
                ToolError::Validation("Blender view response omitted height".to_string())
            })?;
            let snapshot = workspace.snapshot_artifact(path)?;
            let bytes = std::fs::read(snapshot.path())?;
            if bytes.len() as u64 != reported_size {
                return Err(ToolError::Validation(
                    "rendered view changed before compositing".to_string(),
                ));
            }
            validate_png(&bytes, width, height)?;
            tiles.push(Tile {
                label: label.to_string(),
                image: decode_fitted_png(&bytes, layout.tile_w, layout.tile_h)?,
            });
        }
        let composite = tile_images(&tiles, &layout);
        let (width, height) = composite.dimensions();
        let bytes = encode_png(composite)?;
        let meta = workspace.write_artifact(&output_path, &bytes, true)?;
        let (inline, inline_png_base64) = capture_inline_png(include_inline, &bytes);
        let mut value = json!({
            "kind": kind,
            "path": meta.path,
            "size_bytes": meta.size_bytes,
            "media_type": meta.media_type,
            "width": width,
            "height": height,
            "layout": {
                "columns": layout.columns,
                "rows": (rendered_views.len() as u32).div_ceil(layout.columns),
                "tile_width": layout.tile_w,
                "tile_height": layout.tile_h,
                "label_height": layout.label_height,
            },
            "views": rendered_views,
            "engine": engine,
            "render_device": render_device,
            "graphics_backend": graphics_backend,
            "samples": samples,
            "bounds": bounds,
            "inline": inline,
        });
        if let Some(presentation) = presentation {
            value["presentation"] = presentation;
        }
        if let Some(scene_state) = scene_state {
            value["scene_state"] = scene_state;
        }
        Ok::<ToolOutput, ToolError>(ToolOutput {
            value,
            inline_png_base64,
        })
    })
    .await
}

async fn composite_diagnostic(
    workspace: Arc<Workspace>,
    output_path: String,
    include_inline: bool,
    kind: &'static str,
    label: String,
    rendered: Value,
) -> Result<ToolOutput, ToolError> {
    let source_path = rendered["path"]
        .as_str()
        .ok_or_else(|| {
            ToolError::Validation("Blender diagnostic response omitted path".to_string())
        })?
        .to_string();
    let source_size = rendered["size_bytes"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender diagnostic response omitted size_bytes".to_string())
    })?;
    let source_width = rendered["width"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender diagnostic response omitted width".to_string())
    })?;
    let source_height = rendered["height"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender diagnostic response omitted height".to_string())
    })?;
    let layout = fit_composite_layout(
        1,
        1,
        u16::try_from(source_width).map_err(|_| {
            ToolError::Validation("Blender diagnostic response width was invalid".to_string())
        })?,
        u16::try_from(source_height).map_err(|_| {
            ToolError::Validation("Blender diagnostic response height was invalid".to_string())
        })?,
    )?;
    let engine = rendered["engine"].clone();
    let render_device = rendered["render_device"].clone();
    let graphics_backend = rendered["graphics_backend"].clone();
    let samples = rendered["samples"].clone();
    let objects = rendered["objects"].clone();
    let source_bounds = rendered["source_bounds"].clone();
    let rendered_bounds = rendered["rendered_bounds"].clone();
    let analysis = rendered["analysis"].clone();
    let scene_state = rendered.get("scene_state").cloned();
    visual_blocking(move || {
        let snapshot = workspace.snapshot_artifact(&source_path)?;
        let bytes = std::fs::read(snapshot.path())?;
        if bytes.len() as u64 != source_size {
            return Err(ToolError::Validation(
                "diagnostic source changed before compositing".to_string(),
            ));
        }
        validate_png(&bytes, source_width, source_height)?;
        let tile = Tile {
            label,
            image: decode_fitted_png(&bytes, layout.tile_w, layout.tile_h)?,
        };
        let composite = tile_images(&[tile], &layout);
        let (width, height) = composite.dimensions();
        let bytes = encode_png(composite)?;
        let meta = workspace.write_artifact(&output_path, &bytes, true)?;
        let (inline, inline_png_base64) = capture_inline_png(include_inline, &bytes);
        Ok::<ToolOutput, ToolError>(ToolOutput {
            value: json!({
                "kind": kind,
                "path": meta.path,
                "size_bytes": meta.size_bytes,
                "media_type": meta.media_type,
                "width": width,
                "height": height,
                "source": {
                    "path": source_path,
                    "size_bytes": source_size,
                    "media_type": "image/png",
                    "width": source_width,
                    "height": source_height,
                },
                "objects": objects,
                "source_bounds": source_bounds,
                "rendered_bounds": rendered_bounds,
                "analysis": analysis,
                "scene_state": scene_state,
                "engine": engine,
                "render_device": render_device,
                "graphics_backend": graphics_backend,
                "samples": samples,
                "inline": inline,
            }),
            inline_png_base64,
        })
    })
    .await
}

async fn compare_renders(
    workspace: Arc<Workspace>,
    params: CompareRendersParams,
) -> Result<ToolOutput, ToolError> {
    validate_blender_artifact(&workspace, &params.before_path, ".png", true)?;
    validate_blender_artifact(&workspace, &params.after_path, ".png", true)?;
    validate_blender_artifact(&workspace, &params.path, ".png", false)?;
    validate_comparison_layout(params.panel_width, params.panel_height)?;
    visual_blocking(move || {
        let before = workspace.snapshot_artifact(&params.before_path)?;
        let after = workspace.snapshot_artifact(&params.after_path)?;
        let layout = PanelLayout {
            width: u32::from(params.panel_width),
            height: u32::from(params.panel_height),
            label_height: constants::LABEL_BAR_HEIGHT,
        };
        let before_image =
            decode_fitted_png(&std::fs::read(before.path())?, layout.width, layout.height)?;
        let after_image =
            decode_fitted_png(&std::fs::read(after.path())?, layout.width, layout.height)?;
        let composite = side_by_side(&before_image, &after_image, &layout);
        let (width, height) = composite.dimensions();
        let bytes = encode_png(composite)?;
        let meta = workspace.write_artifact(&params.path, &bytes, true)?;
        let (inline, inline_png_base64) = capture_inline_png(params.include_inline, &bytes);
        Ok::<ToolOutput, ToolError>(ToolOutput {
            value: json!({
                "kind": "before_after",
                "path": meta.path,
                "size_bytes": meta.size_bytes,
                "media_type": meta.media_type,
                "width": width,
                "height": height,
                "panels": {
                    "before": params.before_path,
                    "after": params.after_path,
                    "panel_width": params.panel_width,
                    "panel_height": params.panel_height,
                    "label_height": constants::LABEL_BAR_HEIGHT,
                },
                "inline": inline,
            }),
            inline_png_base64,
        })
    })
    .await
}

fn validate_comparison_layout(
    panel_width: u16,
    panel_height: u16,
) -> Result<(u32, u32), ToolError> {
    if !(1..=4096).contains(&panel_width) || !(1..=4096).contains(&panel_height) {
        return Err(ToolError::Validation(
            "panel_width and panel_height must be integers between 1 and 4096".to_string(),
        ));
    }
    let width = u64::from(panel_width) * 2;
    let height = u64::from(panel_height) + u64::from(constants::LABEL_BAR_HEIGHT);
    if width * height > MAX_COMPOSITE_PIXELS {
        return Err(ToolError::Validation(format!(
            "comparison exceeds the {MAX_COMPOSITE_PIXELS}-pixel decoded-memory limit; reduce panel dimensions"
        )));
    }
    Ok((
        u32::try_from(width)
            .map_err(|_| ToolError::Validation("comparison width is unsupported".to_string()))?,
        u32::try_from(height)
            .map_err(|_| ToolError::Validation("comparison height is unsupported".to_string()))?,
    ))
}

fn decode_png(bytes: &[u8]) -> Result<RgbImage, ToolError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODER_ALLOC_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    reader.limits(limits.clone());
    let dimensions = reader.into_dimensions().map_err(|_| {
        ToolError::Validation("input artifact is not a decodable PNG image".to_string())
    })?;
    validate_decoded_dimensions(dimensions.0, dimensions.1)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| {
        ToolError::Validation("input artifact is not a decodable PNG image".to_string())
    })?;
    let rgba = image.into_rgba8();
    Ok(RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y).0;
        image::Rgb(std::array::from_fn(|channel| {
            composite_channel(pixel[channel], constants::CANVAS_BG[channel], pixel[3])
        }))
    }))
}

fn decode_fitted_png(bytes: &[u8], width: u32, height: u32) -> Result<RgbImage, ToolError> {
    let decoded = decode_png(bytes)?;
    Ok(resize(&decoded, width, height, FilterType::Lanczos3))
}

fn validate_decoded_dimensions(width: u32, height: u32) -> Result<(), ToolError> {
    if u64::from(width)
        .checked_mul(u64::from(height))
        .is_none_or(|pixels| pixels > MAX_DECODED_INPUT_PIXELS)
    {
        return Err(ToolError::Validation(format!(
            "input image exceeds the {MAX_DECODED_INPUT_PIXELS}-pixel decoded-memory limit"
        )));
    }
    Ok(())
}

fn composite_channel(foreground: u8, background: u8, alpha: u8) -> u8 {
    let alpha = u16::from(alpha);
    let inverse = 255 - alpha;
    let foreground = u16::from(foreground) * alpha;
    let background = u16::from(background) * inverse;
    ((foreground + background + 127) / 255) as u8
}

fn encode_png(image: RgbImage) -> Result<Vec<u8>, ToolError> {
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|error| ToolError::Io(std::io::Error::other(error.to_string())))?;
    Ok(output.into_inner())
}

fn inline_metadata(requested: bool, size_bytes: u64) -> Value {
    if !requested {
        json!({
            "requested": false,
            "included": false,
            "max_bytes": PREVIEW_INLINE_MAX_BYTES,
            "reason": "disabled by caller",
        })
    } else if size_bytes > PREVIEW_INLINE_MAX_BYTES {
        json!({
            "requested": true,
            "included": false,
            "max_bytes": PREVIEW_INLINE_MAX_BYTES,
            "reason": "artifact exceeds inline transport limit",
        })
    } else {
        json!({
            "requested": true,
            "included": false,
            "max_bytes": PREVIEW_INLINE_MAX_BYTES,
        })
    }
}

fn capture_inline_png(requested: bool, bytes: &[u8]) -> (Value, Option<String>) {
    let mut metadata = inline_metadata(requested, bytes.len() as u64);
    if !requested || bytes.len() as u64 > PREVIEW_INLINE_MAX_BYTES {
        return (metadata, None);
    }
    metadata["included"] = json!(true);
    (
        metadata,
        Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
    )
}

fn validate_work_budget(timeout_seconds: f64) -> Result<Duration, ToolError> {
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err(ToolError::Validation(
            "timeout_seconds must be a positive finite number".to_string(),
        ));
    }
    Duration::try_from_secs_f64(timeout_seconds).map_err(|_| {
        ToolError::Validation("timeout_seconds exceeds the supported duration range".to_string())
    })
}

fn validate_render_response(
    response: &Value,
    params: &RenderPreviewParams,
) -> Result<(), ToolError> {
    let object = response.as_object().ok_or_else(|| {
        ToolError::Validation("Blender render response must be an object".to_string())
    })?;
    if object.get("path").and_then(Value::as_str) != Some(params.path.as_str()) {
        return Err(ToolError::Validation(
            "Blender render response path did not match the request".to_string(),
        ));
    }
    if object.get("media_type").and_then(Value::as_str) != Some("image/png") {
        return Err(ToolError::Validation(
            "Blender render response must describe an image/png artifact".to_string(),
        ));
    }
    validate_render_backend(object, params.engine, params.samples)?;
    if object.get("size_bytes").and_then(Value::as_u64) == Some(0)
        || object.get("size_bytes").and_then(Value::as_u64).is_none()
    {
        return Err(ToolError::Validation(
            "Blender render response must include a positive size_bytes".to_string(),
        ));
    }
    if object.get("width").and_then(Value::as_u64) != Some(u64::from(params.width))
        || object.get("height").and_then(Value::as_u64) != Some(u64::from(params.height))
    {
        return Err(ToolError::Validation(
            "Blender render response dimensions did not match the request".to_string(),
        ));
    }
    Ok(())
}

fn validate_render_backend(
    object: &serde_json::Map<String, Value>,
    requested_engine: RenderEngine,
    requested_samples: Option<u16>,
) -> Result<(), ToolError> {
    let engine = object.get("engine").and_then(Value::as_str);
    let render_device = object.get("render_device").and_then(Value::as_str);
    let valid_backend = match requested_engine {
        RenderEngine::Eevee => {
            matches!(engine, Some("BLENDER_EEVEE" | "BLENDER_EEVEE_NEXT"))
                && render_device == Some("GRAPHICS")
        }
        RenderEngine::Cycles => {
            engine == Some("CYCLES") && matches!(render_device, Some("CPU" | "OPTIX"))
        }
    };
    if !valid_backend {
        return Err(ToolError::Validation(
            "Blender render response engine or device did not match the request".to_string(),
        ));
    }
    let expected_samples = match requested_engine {
        RenderEngine::Eevee => Value::Null,
        RenderEngine::Cycles => json!(requested_samples.unwrap_or(128)),
    };
    if object.get("samples") != Some(&expected_samples) {
        return Err(ToolError::Validation(
            "Blender render response samples did not match the request".to_string(),
        ));
    }
    let graphics_backend_valid = match object.get("graphics_backend") {
        Some(Value::Null) => true,
        Some(Value::Object(graphics)) => {
            ["backend", "device_type", "renderer", "vendor", "version"]
                .into_iter()
                .all(|field| graphics.get(field).and_then(Value::as_str).is_some())
        }
        _ => false,
    };
    if !graphics_backend_valid {
        return Err(ToolError::Validation(
            "Blender render response omitted graphics backend metadata".to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn prepare_inline_png_content(
    workspace: Arc<Workspace>,
    mut value: Value,
) -> Result<(Value, Option<String>), ToolError> {
    let requested = value["inline"]["requested"].as_bool().unwrap_or(false);
    let reported_size = value["size_bytes"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender render response omitted size_bytes".to_string())
    })?;
    if !requested || reported_size > PREVIEW_INLINE_MAX_BYTES {
        return Ok((value, None));
    }
    let path = value["path"]
        .as_str()
        .ok_or_else(|| ToolError::Validation("Blender render response omitted path".to_string()))?
        .to_string();
    let expected_width = value["width"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender render response omitted width".to_string())
    })?;
    let expected_height = value["height"].as_u64().ok_or_else(|| {
        ToolError::Validation("Blender render response omitted height".to_string())
    })?;
    let (meta, bytes) = blocking(move || workspace.read_artifact(&path)).await?;
    if meta.size_bytes != reported_size {
        return Err(ToolError::Validation(
            "render artifact changed before inline transfer".to_string(),
        ));
    }
    validate_png(&bytes, expected_width, expected_height)?;
    value["inline"]["included"] = json!(true);
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok((value, Some(data_base64)))
}

fn validate_png(bytes: &[u8], expected_width: u64, expected_height: u64) -> Result<(), ToolError> {
    validate_complete_png_container(bytes)?;
    let expected_width = u32::try_from(expected_width).map_err(|_| {
        ToolError::Validation("render artifact dimensions are unsupported".to_string())
    })?;
    let expected_height = u32::try_from(expected_height).map_err(|_| {
        ToolError::Validation("render artifact dimensions are unsupported".to_string())
    })?;
    if expected_width == 0
        || expected_height == 0
        || expected_width > 8192
        || expected_height > 8192
    {
        return Err(ToolError::Validation(
            "render artifact dimensions are unsupported".to_string(),
        ));
    }
    let pixel_bytes = u64::from(expected_width)
        .checked_mul(u64::from(expected_height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            ToolError::Validation("render artifact dimensions are unsupported".to_string())
        })?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(expected_width);
    limits.max_image_height = Some(expected_height);
    limits.max_alloc = Some(pixel_bytes.saturating_add(16 * 1024 * 1024));
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| {
        ToolError::Validation("render artifact is not a complete PNG image".to_string())
    })?;
    if image.width() != expected_width || image.height() != expected_height {
        return Err(ToolError::Validation(
            "render artifact dimensions do not match the request".to_string(),
        ));
    }
    Ok(())
}

fn validate_complete_png_container(bytes: &[u8]) -> Result<(), ToolError> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(ToolError::Validation(
            "render artifact is not a complete PNG image".to_string(),
        ));
    }
    let mut offset = PNG_SIGNATURE.len();
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or_else(|| {
            ToolError::Validation("render artifact is not a complete PNG image".to_string())
        })?;
        let header = bytes.get(offset..header_end).ok_or_else(|| {
            ToolError::Validation("render artifact is not a complete PNG image".to_string())
        })?;
        let data_len = usize::try_from(u32::from_be_bytes(
            header[..4].try_into().expect("four-byte PNG chunk length"),
        ))
        .expect("u32 PNG chunk length fits usize on supported targets");
        let chunk_end = header_end
            .checked_add(data_len)
            .and_then(|end| end.checked_add(4))
            .ok_or_else(|| {
                ToolError::Validation("render artifact is not a complete PNG image".to_string())
            })?;
        if chunk_end > bytes.len() {
            return Err(ToolError::Validation(
                "render artifact is not a complete PNG image".to_string(),
            ));
        }
        if &header[4..8] == b"IEND" {
            return if data_len == 0 && chunk_end == bytes.len() {
                Ok(())
            } else {
                Err(ToolError::Validation(
                    "render artifact is not a complete PNG image".to_string(),
                ))
            };
        }
        offset = chunk_end;
    }
    Err(ToolError::Validation(
        "render artifact is not a complete PNG image".to_string(),
    ))
}

/// Deserialize a tool's arguments into its typed parameters.
fn de<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, ToolError> {
    serde_json::from_value(v).map_err(|e| ToolError::Validation(e.to_string()))
}

/// Run a blocking workspace operation on Tokio's blocking pool, flattening the
/// join error and the operation error into a single [`ToolError`].
pub(crate) async fn blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
    E: Into<ToolError> + Send + 'static,
{
    // Move an OWNED permit into the blocking job so it is released only when the
    // job finishes, not when this future is dropped. spawn_blocking detaches: a
    // cancelled request (client disconnect) frees a future-held permit while its
    // filesystem work runs on, which would let more than
    // MAX_CONCURRENT_WORKSPACE_OPS run at once. The semaphore is never closed, so
    // acquire only fails while the process is tearing down.
    let permit = Arc::clone(&WORKSPACE_OPS)
        .acquire_owned()
        .await
        .map_err(|_| ToolError::Validation("server is shutting down".to_string()))?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    {
        Ok(result) => result.map_err(Into::into),
        Err(join) => Err(ToolError::Validation(format!(
            "workspace task failed: {join}"
        ))),
    }
}

async fn visual_blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
    E: Into<ToolError> + Send + 'static,
{
    let visual_permit = Arc::clone(&VISUAL_OPS)
        .acquire_owned()
        .await
        .map_err(|_| ToolError::Validation("server is shutting down".to_string()))?;
    blocking(move || {
        let _visual_permit = visual_permit;
        f()
    })
    .await
}

pub(crate) async fn geometry_blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
    E: Into<ToolError> + Send + 'static,
{
    let geometry_permit = Arc::clone(&GEOMETRY_OPS)
        .acquire_owned()
        .await
        .map_err(|_| ToolError::Validation("server is shutting down".to_string()))?;
    blocking(move || {
        let _geometry_permit = geometry_permit;
        f()
    })
    .await
}

async fn validate_mesh_artifact(
    workspace: Arc<Workspace>,
    params: ValidateMeshParams,
) -> Result<Value, ToolError> {
    validate_blender_artifact(&workspace, &params.path, ".stl", true)?;
    geometry_blocking(move || {
        let (artifact, bytes) = workspace.read_artifact(&params.path)?;
        let report = analyze_stl(
            &bytes,
            ValidationOptions {
                build_direction: params.build_direction,
                overhang_angle_degrees: params.overhang_angle_degrees,
                density_g_cm3: params.density_g_cm3,
            },
        )?;
        Ok::<_, ToolError>(json!({
            "artifact": artifact,
            "units": "millimetres",
            "report": report,
        }))
    })
    .await
}

async fn analyze_assembly_artifacts(
    workspace: Arc<Workspace>,
    params: AnalyzeAssemblyParams,
    settings: &Settings,
) -> Result<Value, ToolError> {
    validate_blender_artifact(&workspace, &params.fixed_path, ".stl", true)?;
    validate_blender_artifact(&workspace, &params.moving_path, ".stl", true)?;
    let options = AssemblyOptions {
        required_clearance_mm: params.required_clearance_mm,
        motion: params.motion.map(|motion| LinearMotion {
            direction: motion.direction,
            travel_mm: motion.travel_mm,
            target_clearance_mm: motion.target_clearance_mm,
        }),
        rotation: params.rotation.map(|rotation| RotationalMotion {
            pivot_mm: rotation.pivot_mm,
            axis: rotation.axis,
            angle_degrees: rotation.angle_degrees,
            target_clearance_mm: rotation.target_clearance_mm,
        }),
    };
    options.validate()?;
    let worker_bin = geometry_worker_path(settings)?;
    let worker_memory_bytes = settings.geometry_worker_memory_bytes;
    geometry_blocking(move || {
        let (fixed_artifact, fixed_bytes) = workspace.read_artifact(&params.fixed_path)?;
        let (moving_artifact, moving_bytes) = workspace.read_artifact(&params.moving_path)?;
        let report = run_geometry_worker(
            &worker_bin,
            worker_memory_bytes,
            &fixed_bytes,
            &moving_bytes,
            options,
        )?;
        Ok::<_, ToolError>(json!({
            "fixed_artifact": fixed_artifact,
            "moving_artifact": moving_artifact,
            "units": "millimetres",
            "report": report,
        }))
    })
    .await
}

fn geometry_worker_path(settings: &Settings) -> Result<PathBuf, ToolError> {
    geometry_worker_path_from_override(settings.geometry_worker_bin.as_deref())
}

pub(crate) fn geometry_worker_path_from_override(
    override_path: Option<&std::path::Path>,
) -> Result<PathBuf, ToolError> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let executable = std::env::current_exe().map_err(|_| {
        geometry_worker_error(
            "geometry_worker_unavailable",
            "server executable path is unavailable",
        )
    })?;
    let directory = executable.parent().ok_or_else(|| {
        geometry_worker_error(
            "geometry_worker_unavailable",
            "server installation directory is unavailable",
        )
    })?;
    Ok(directory.join("printable-geometry-worker"))
}

pub(crate) fn run_geometry_worker(
    worker_bin: &std::path::Path,
    memory_bytes: u64,
    fixed: &[u8],
    moving: &[u8],
    options: AssemblyOptions,
) -> Result<Value, ToolError> {
    let staging = tempfile::Builder::new()
        .prefix("printable-assembly-")
        .tempdir()?;
    let fixed_path = staging.path().join("fixed.stl");
    let moving_path = staging.path().join("moving.stl");
    std::fs::write(&fixed_path, fixed)?;
    std::fs::write(&moving_path, moving)?;
    run_geometry_worker_files(worker_bin, memory_bytes, &fixed_path, &moving_path, options)
}

pub(crate) fn run_geometry_worker_files(
    worker_bin: &std::path::Path,
    memory_bytes: u64,
    fixed_path: &std::path::Path,
    moving_path: &std::path::Path,
    options: AssemblyOptions,
) -> Result<Value, ToolError> {
    let staging = tempfile::Builder::new()
        .prefix("printable-assembly-options-")
        .tempdir()?;
    let options_path = staging.path().join("options.json");
    std::fs::write(&options_path, serde_json::to_vec(&options)?)?;

    let mut command = Command::new(worker_bin);
    command.args([fixed_path, moving_path, &options_path]);
    set_worker_memory_limit(&mut command, memory_bytes)?;
    let output = command.output().map_err(|_| {
        geometry_worker_error(
            "geometry_worker_unavailable",
            "could not start the isolated assembly geometry worker",
        )
    })?;
    parse_geometry_worker_output(output.status.success(), &output.stdout)
}

fn parse_geometry_worker_output(status_success: bool, stdout: &[u8]) -> Result<Value, ToolError> {
    let envelope = serde_json::from_slice::<Value>(stdout).map_err(|_| {
        if status_success {
            geometry_worker_error(
                "geometry_worker_protocol",
                "assembly geometry worker returned an invalid response",
            )
        } else {
            geometry_worker_error(
                "geometry_resource_limit",
                "assembly geometry worker exceeded its memory budget or terminated unexpectedly",
            )
        }
    })?;
    if let Some(error) = envelope.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .map(worker_error_code)
            .unwrap_or("geometry");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("assembly geometry worker failed")
            .to_string();
        return Err(geometry_worker_error(code, message));
    }
    if !status_success {
        return Err(geometry_worker_error(
            "geometry_resource_limit",
            "assembly geometry worker exceeded its memory budget or terminated unexpectedly",
        ));
    }
    envelope.get("report").cloned().ok_or_else(|| {
        geometry_worker_error(
            "geometry_worker_protocol",
            "assembly geometry worker response omitted its report",
        )
    })
}

#[cfg(target_os = "linux")]
fn set_worker_memory_limit(command: &mut Command, memory_bytes: u64) -> Result<(), ToolError> {
    let limit = libc::rlim_t::try_from(memory_bytes).map_err(|_| {
        geometry_worker_error(
            "geometry_resource_limit",
            "worker memory budget exceeds the platform range",
        )
    })?;
    // The limit is installed after fork and before exec, so only the disposable
    // geometry child receives the address-space restriction.
    unsafe {
        command.pre_exec(move || {
            let resource_limit = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &resource_limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_worker_memory_limit(_command: &mut Command, _memory_bytes: u64) -> Result<(), ToolError> {
    Ok(())
}

fn worker_error_code(code: &str) -> &'static str {
    match code {
        "empty_mesh" => "empty_mesh",
        "invalid_stl" => "invalid_stl",
        "non_finite_geometry" => "non_finite_geometry",
        "invalid_mesh_index" => "invalid_mesh_index",
        "mesh_too_large" => "mesh_too_large",
        "validation" => "validation",
        "invalid_assembly_part" => "invalid_assembly_part",
        "geometry_worker_io" => "geometry_worker_io",
        _ => "geometry",
    }
}

fn geometry_worker_error(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError::GeometryWorker {
        code,
        message: message.into(),
    }
}

struct PreparedScadJob {
    _staging: printable_workspace::ManagedScratch,
    _snapshots: Vec<Snapshot>,
    source_path: PathBuf,
    output_path: PathBuf,
    cross_section: Option<PreparedScadCrossSection>,
}

struct PreparedScadCrossSection {
    model_path: PathBuf,
    projection_source_path: PathBuf,
}

fn validate_scad_source(source: &str) -> Result<(), ToolError> {
    if source.is_empty() {
        return Err(ToolError::Validation(
            "source must not be empty".to_string(),
        ));
    }
    if source.len() > MAX_SCAD_SOURCE_BYTES {
        return Err(ToolError::Validation(format!(
            "source exceeds {MAX_SCAD_SOURCE_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_scad_destination(
    workspace: &Workspace,
    path: &str,
    suffix: &str,
    overwrite: bool,
) -> Result<(), ToolError> {
    validate_blender_artifact(workspace, path, suffix, false)?;
    match workspace.resolve(path, true) {
        Ok(_) if !overwrite => {
            Err(printable_workspace::WsError::AlreadyExists(path.to_string()).into())
        }
        Ok(existing) => {
            if !std::fs::symlink_metadata(existing)?.file_type().is_file() {
                return Err(printable_workspace::WsError::NotRegularFile.into());
            }
            Ok(())
        }
        Err(printable_workspace::WsError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn prepare_scad_job(
    workspace: Arc<Workspace>,
    runner: Arc<ScadRunner>,
    source: String,
    output_suffix: &'static str,
    cross_section_z_mm: Option<f64>,
    materialize_cross_section: bool,
    product_v1: bool,
) -> Result<(ScadPermit, Arc<PreparedScadJob>), ToolError> {
    let permit = runner.acquire().await?;
    blocking(move || {
        let mut snapshots = Vec::new();
        let mut snapshot_paths: HashMap<String, String> = HashMap::new();
        if product_v1 {
            printable_scad::validate_product_v1_caller(&source)?;
        }
        let confined = printable_scad::confine_source(&source, |path| {
            if let Some(existing) = snapshot_paths.get(path) {
                return Ok(existing.clone());
            }
            let snapshot = workspace
                .snapshot_artifact(path)
                .map_err(|error| printable_scad::GateError::Snapshot(error.to_string()))?;
            let snapshot_path = snapshot.path().to_string_lossy().into_owned();
            snapshot_paths.insert(path.to_string(), snapshot_path.clone());
            snapshots.push(snapshot);
            Ok(snapshot_path)
        })?;
        let staging = workspace.scratch(
            2 * MAX_PRODUCT_RENDER_BYTES + 4 * MAX_SCAD_SOURCE_BYTES as u64,
            "openscad",
        )?;
        let confined_source = if product_v1 {
            std::fs::write(
                staging.path().join(printable_scad::PRODUCT_V1_CALLER_FILE),
                confined.as_bytes(),
            )?;
            std::fs::write(
                staging.path().join(printable_scad::PRODUCT_V1_KIT_FILE),
                printable_scad::PRODUCT_V1_SOURCE.as_bytes(),
            )?;
            printable_scad::product_v1_wrapper()
        } else {
            confined
        };
        let executable_source = match (cross_section_z_mm, materialize_cross_section) {
            (Some(z_mm), false) => printable_scad::cross_section_source(&confined_source, z_mm),
            _ => confined_source,
        };
        let source_path = staging.path().join("source.scad");
        let output_path = staging.path().join(format!("output{output_suffix}"));
        std::fs::write(&source_path, executable_source.as_bytes())?;
        let cross_section = materialize_cross_section
            .then_some(cross_section_z_mm)
            .flatten()
            .map(|z_mm| {
                let model_path = staging.path().join("model.stl");
                let projection_source_path = staging.path().join("projection.scad");
                std::fs::write(
                    &projection_source_path,
                    printable_scad::cross_section_source("import(\"model.stl\");", z_mm),
                )?;
                Ok::<_, std::io::Error>(PreparedScadCrossSection {
                    model_path,
                    projection_source_path,
                })
            })
            .transpose()?;
        let job = Arc::new(PreparedScadJob {
            _staging: staging,
            _snapshots: snapshots,
            source_path,
            output_path,
            cross_section,
        });
        Ok::<_, ToolError>((permit.retain(Arc::clone(&job)), job))
    })
    .await
}

fn scad_diagnostics(output: &RunOutput) -> Value {
    json!({
        "stdout": output.stdout,
        "stderr": output.stderr,
        "stdout_truncated": output.stdout_truncated,
        "stderr_truncated": output.stderr_truncated,
    })
}

fn generated_file_size(path: &std::path::Path) -> Result<usize, ToolError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        ToolError::Validation("OpenSCAD completed without producing an artifact".to_string())
    })?;
    if !metadata.file_type().is_file() {
        return Err(ToolError::Validation(
            "OpenSCAD output is not a regular file".to_string(),
        ));
    }
    if metadata.len() == 0 {
        return Err(ToolError::Validation(
            "OpenSCAD produced an empty artifact".to_string(),
        ));
    }
    if metadata.len() > printable_workspace::MAX_TRANSFER_BYTES {
        return Err(printable_workspace::WsError::WriteTooLarge.into());
    }
    usize::try_from(metadata.len()).map_err(|_| {
        ToolError::Validation("OpenSCAD artifact size is not representable".to_string())
    })
}

fn read_generated_bounded(path: &std::path::Path) -> Result<Vec<u8>, ToolError> {
    let expected_len = generated_file_size(path)?;
    let mut file = std::fs::File::open(path)?;
    let mut bytes = vec![0; expected_len];
    file.read_exact(&mut bytes)?;
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(printable_workspace::WsError::WriteTooLarge.into());
    }
    Ok(bytes)
}

fn validate_scad_svg(bytes: &[u8]) -> Result<(), ToolError> {
    std::str::from_utf8(bytes).map_err(|_| {
        ToolError::Validation("OpenSCAD cross-section output is not UTF-8 SVG".to_string())
    })?;
    let mut reader = XmlReader::from_reader(bytes);
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut saw_declaration = false;
    let mut saw_doctype = false;
    loop {
        let event = reader.read_event().map_err(|_| invalid_scad_svg())?;
        match event {
            XmlEvent::Start(element) => {
                validate_scad_svg_element(&element, reader.decoder())?;
                if depth == 0 {
                    validate_scad_svg_root(&element, saw_root)?;
                    saw_root = true;
                }
                depth = depth.checked_add(1).ok_or_else(invalid_scad_svg)?;
            }
            XmlEvent::Empty(element) => {
                validate_scad_svg_element(&element, reader.decoder())?;
                if depth == 0 {
                    validate_scad_svg_root(&element, saw_root)?;
                    saw_root = true;
                }
            }
            XmlEvent::End(_) => {
                depth = depth.checked_sub(1).ok_or_else(invalid_scad_svg)?;
            }
            XmlEvent::Text(text) if depth == 0 => {
                let text = text.decode().map_err(|_| invalid_scad_svg())?;
                if !text.trim().is_empty() {
                    return Err(invalid_scad_svg());
                }
            }
            XmlEvent::CData(_) if depth == 0 => return Err(invalid_scad_svg()),
            XmlEvent::GeneralRef(reference) => {
                let reference: &[u8] = reference.as_ref();
                if depth == 0 || !matches!(reference, b"amp" | b"lt" | b"gt" | b"apos" | b"quot") {
                    return Err(invalid_scad_svg());
                }
            }
            XmlEvent::Decl(_) => {
                if saw_declaration || saw_doctype || saw_root || depth != 0 {
                    return Err(invalid_scad_svg());
                }
                saw_declaration = true;
            }
            XmlEvent::DocType(_) => {
                if saw_doctype || saw_root || depth != 0 {
                    return Err(invalid_scad_svg());
                }
                saw_doctype = true;
            }
            XmlEvent::Eof => {
                return if saw_root && depth == 0 {
                    Ok(())
                } else {
                    Err(invalid_scad_svg())
                };
            }
            _ => {}
        }
    }
}

fn validate_scad_svg_element(
    element: &BytesStart<'_>,
    decoder: XmlDecoder,
) -> Result<(), ToolError> {
    for attribute in element.attributes().with_checks(true) {
        attribute
            .map_err(|_| invalid_scad_svg())?
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
            .map_err(|_| invalid_scad_svg())?;
    }
    Ok(())
}

fn validate_scad_svg_root(element: &BytesStart<'_>, saw_root: bool) -> Result<(), ToolError> {
    if saw_root || element.name().local_name().as_ref() != b"svg" {
        return Err(invalid_scad_svg());
    }
    Ok(())
}

fn invalid_scad_svg() -> ToolError {
    ToolError::Validation(
        "OpenSCAD cross-section output is not a complete SVG document".to_string(),
    )
}

fn validate_scad_render_size(size: u16) -> Result<(), ToolError> {
    if size == 0 || size > 8192 {
        return Err(ToolError::Validation(
            "size must be an integer between 1 and 8192".to_string(),
        ));
    }
    Ok(())
}

fn serialize_scad_definitions(
    definitions: &ScadDefinitions,
    variant: Option<&str>,
    profile: Option<&printable_scad::ProductProfile>,
) -> Result<printable_scad::SerializedDefinitions, ToolError> {
    let definitions = definitions
        .0
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ScadDefineValue::Bool(value) => printable_scad::DefineValue::Bool(*value),
                ScadDefineValue::Number(value) => printable_scad::DefineValue::Number(*value),
                ScadDefineValue::String(value) => {
                    printable_scad::DefineValue::String(value.clone())
                }
                ScadDefineValue::NumberVector(value) => {
                    printable_scad::DefineValue::NumberVector(value.clone())
                }
            };
            (name.clone(), value)
        })
        .collect();
    printable_scad::serialize_product_definitions(&definitions, variant, profile)
        .map_err(|error| ToolError::Validation(error.to_string()))
}

fn scad_definition_metadata(definitions: &printable_scad::SerializedDefinitions) -> Value {
    json!({
        "count": definitions.names().len(),
        "names": definitions.names(),
        "variant_applied": definitions.variant_applied(),
    })
}

fn bind_scad_variant(
    source: String,
    definitions: &printable_scad::SerializedDefinitions,
) -> String {
    // OpenSCAD's -D override needs a source binding for indirect references.
    // Values stay in typed argv entries; only a fixed declaration is added.
    if definitions.variant_applied() {
        format!("pbl_variant = undef;\n{source}")
    } else {
        source
    }
}

fn insert_design_profile(result: &mut Value, profile: Option<&ProductDesignProfile>) {
    if let Some(profile) = profile {
        result["design_profile"] =
            serde_json::to_value(profile).expect("product profile is JSON-serializable");
    }
}

fn insert_manufacturing_evidence(result: &mut Value, profile: &ProductDesignProfile) {
    result["manufacturing_evidence"] = json!({
        "units": "millimeters",
        "build_direction": [0.0, 0.0, 1.0],
        "measured": ["topology", "bounds", "build_plate_contact", "overhang"],
        "kit_local_wall_assertions": {
            "status": "enforced_when_used",
            "minimum_wall_mm": profile.manufacturing.minimum_wall_mm,
        },
        "global_minimum_wall": {
            "status": "not_certified",
        },
        "moving_clearance": {
            "status": "not_run",
            "requested_mm": profile.manufacturing.moving_clearance_mm,
        },
    });
}

async fn scad_compile(
    workspace: Arc<Workspace>,
    runner: Arc<ScadRunner>,
    params: ScadCompileParams,
) -> Result<Value, ToolError> {
    validate_scad_destination(&workspace, &params.path, ".stl", params.overwrite)?;
    validate_scad_source(&params.source)?;
    let budget = validate_work_budget(params.timeout_seconds)?;
    let product_profile = params
        .design_profile
        .as_ref()
        .map(ProductDesignProfile::core);
    let definitions = serialize_scad_definitions(
        &params.defines,
        params.variant.as_deref(),
        product_profile.as_ref(),
    )?;
    let validation_options = ValidationOptions {
        build_direction: [0.0, 0.0, 1.0],
        overhang_angle_degrees: product_profile.as_ref().map_or(
            ValidationOptions::default().overhang_angle_degrees,
            |profile| profile.manufacturing.maximum_overhang_degrees,
        ),
        density_g_cm3: None,
    };
    let definition_metadata = scad_definition_metadata(&definitions);
    let source = bind_scad_variant(params.source, &definitions);
    let (permit, job) = prepare_scad_job(
        Arc::clone(&workspace),
        runner,
        source,
        ".stl",
        None,
        false,
        product_profile.is_some(),
    )
    .await?;
    let args = printable_scad::compile_args(
        &job.output_path.to_string_lossy(),
        &job.source_path.to_string_lossy(),
        definitions.argv(),
    );
    let (permit, output) = permit
        .run(args, budget, printable_workspace::MAX_TRANSFER_BYTES)
        .await?;
    geometry_blocking(move || {
        let _permit = permit;
        let bytes = read_generated_bounded(&job.output_path)?;
        let validation = analyze_stl(&bytes, validation_options)?;
        let artifact = workspace.write_artifact(&params.path, &bytes, params.overwrite)?;
        let mut result = json!({
            "artifact": artifact,
            "validation": validation,
            "definitions": definition_metadata,
            "diagnostics": scad_diagnostics(&output),
        });
        insert_design_profile(&mut result, params.design_profile.as_ref());
        if let Some(profile) = params.design_profile.as_ref() {
            insert_manufacturing_evidence(&mut result, profile);
        }
        Ok::<_, ToolError>(result)
    })
    .await
}

async fn scad_render(
    workspace: Arc<Workspace>,
    runner: Arc<ScadRunner>,
    params: ScadRenderParams,
) -> Result<ToolOutput, ToolError> {
    validate_scad_destination(&workspace, &params.path, ".png", params.overwrite)?;
    validate_scad_source(&params.source)?;
    validate_scad_render_size(params.size)?;
    if printable_scad::camera(&params.view).is_none() {
        return Err(ToolError::Validation(
            "view must be one of iso, front, back, right, left, top, bottom".to_string(),
        ));
    }
    let budget = validate_work_budget(params.timeout_seconds)?;
    let product_profile = params
        .design_profile
        .as_ref()
        .map(ProductDesignProfile::core);
    let definitions = serialize_scad_definitions(
        &params.defines,
        params.variant.as_deref(),
        product_profile.as_ref(),
    )?;
    let definition_metadata = scad_definition_metadata(&definitions);
    let source = bind_scad_variant(params.source, &definitions);
    let (permit, job) = prepare_scad_job(
        Arc::clone(&workspace),
        runner,
        source,
        ".png",
        None,
        false,
        product_profile.is_some(),
    )
    .await?;
    let args = printable_scad::render_args(
        &job.output_path.to_string_lossy(),
        &job.source_path.to_string_lossy(),
        &params.view,
        u32::from(params.size),
        params.preview,
        definitions.argv(),
    )
    .ok_or_else(|| ToolError::Validation("unknown OpenSCAD view".to_string()))?;
    let (permit, output) = permit
        .run(args, budget, printable_workspace::MAX_TRANSFER_BYTES)
        .await?;
    visual_blocking(move || {
        let _permit = permit;
        let bytes = read_generated_bounded(&job.output_path)?;
        validate_png(&bytes, u64::from(params.size), u64::from(params.size))?;
        let artifact = workspace.write_artifact(&params.path, &bytes, params.overwrite)?;
        let (inline, inline_png_base64) = capture_inline_png(params.include_inline, &bytes);
        Ok::<_, ToolError>(ToolOutput {
            value: {
                let mut result = json!({
                "artifact": artifact,
                "view": params.view,
                "size": params.size,
                "preview": params.preview,
                "inline": inline,
                "definitions": definition_metadata,
                "diagnostics": scad_diagnostics(&output),
                });
                insert_design_profile(&mut result, params.design_profile.as_ref());
                result
            },
            inline_png_base64,
        })
    })
    .await
}

async fn scad_cross_section(
    workspace: Arc<Workspace>,
    runner: Arc<ScadRunner>,
    params: ScadCrossSectionParams,
) -> Result<Value, ToolError> {
    validate_scad_destination(&workspace, &params.path, ".svg", params.overwrite)?;
    validate_scad_source(&params.source)?;
    if !params.z_mm.is_finite() {
        return Err(ToolError::Validation(
            "z_mm must be a finite number".to_string(),
        ));
    }
    let budget = validate_work_budget(params.timeout_seconds)?;
    let product_profile = params
        .design_profile
        .as_ref()
        .map(ProductDesignProfile::core);
    let definitions = serialize_scad_definitions(
        &params.defines,
        params.variant.as_deref(),
        product_profile.as_ref(),
    )?;
    let definition_metadata = scad_definition_metadata(&definitions);
    let materialize_cross_section = !definitions.argv().is_empty();
    let source = bind_scad_variant(params.source, &definitions);
    let (permit, job) = prepare_scad_job(
        Arc::clone(&workspace),
        runner,
        source,
        ".svg",
        Some(params.z_mm),
        materialize_cross_section,
        product_profile.is_some(),
    )
    .await?;
    let (permit, model_output, projection_output) =
        if let Some(cross_section) = job.cross_section.as_ref() {
            let deadline = tokio::time::Instant::now()
                .checked_add(budget)
                .ok_or_else(|| {
                    ToolError::Validation(
                        "timeout_seconds exceeds the supported monotonic clock range".to_string(),
                    )
                })?;
            let model_args = printable_scad::compile_args(
                &cross_section.model_path.to_string_lossy(),
                &job.source_path.to_string_lossy(),
                definitions.argv(),
            );
            let (permit, model_output) = permit
                .run(model_args, budget, printable_workspace::MAX_TRANSFER_BYTES)
                .await?;
            let model_path = cross_section.model_path.clone();
            blocking(move || generated_file_size(&model_path)).await?;
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(printable_scad::ScadError::Timeout)?;
            let projection_args = printable_scad::compile_args(
                &job.output_path.to_string_lossy(),
                &cross_section.projection_source_path.to_string_lossy(),
                &[],
            );
            let (permit, projection_output) = permit
                .run(
                    projection_args,
                    remaining,
                    printable_workspace::MAX_TRANSFER_BYTES,
                )
                .await?;
            (permit, model_output, Some(projection_output))
        } else {
            let args = printable_scad::compile_args(
                &job.output_path.to_string_lossy(),
                &job.source_path.to_string_lossy(),
                definitions.argv(),
            );
            let (permit, output) = permit
                .run(args, budget, printable_workspace::MAX_TRANSFER_BYTES)
                .await?;
            (permit, output, None)
        };
    blocking(move || {
        let _permit = permit;
        let bytes = read_generated_bounded(&job.output_path)?;
        validate_scad_svg(&bytes)?;
        let artifact = workspace.write_artifact(&params.path, &bytes, params.overwrite)?;
        let mut result = json!({
            "artifact": artifact,
            "z_mm": params.z_mm,
            "definitions": definition_metadata,
            "diagnostics": scad_diagnostics(&model_output),
        });
        if let Some(projection_output) = projection_output.as_ref() {
            result["projection_diagnostics"] = scad_diagnostics(projection_output);
        }
        insert_design_profile(&mut result, params.design_profile.as_ref());
        Ok::<_, ToolError>(result)
    })
    .await
}

async fn read_artifact(workspace: Arc<Workspace>, path: String) -> Result<Value, ToolError> {
    // Read + base64-encode (up to the transfer cap) off the async workers.
    let (meta, data_base64) = blocking(move || {
        let (meta, bytes) = workspace.read_artifact(&path)?;
        Ok::<_, ToolError>((
            meta,
            base64::engine::general_purpose::STANDARD.encode(&bytes),
        ))
    })
    .await?;
    // The four metadata fields, then the base64 body (base64 is the tool layer's
    // concern; the workspace crate is byte-oriented).
    let mut obj = serde_json::to_value(&meta)?;
    obj["data_base64"] = json!(data_base64);
    Ok(obj)
}

async fn write_artifact(workspace: Arc<Workspace>, p: WriteParams) -> Result<Value, ToolError> {
    // Single-shot writes are capped at one chunk (1 MiB decoded). rmcp buffers
    // the whole base64 body per in-flight request, so bounding it keeps a burst
    // of concurrent writes from retaining an unbounded aggregate; larger
    // artifacts stream through write_begin/write_chunk/write_commit.
    let bytes = decode_bounded(&p.data_base64)?;
    // Write + fsync off the async workers.
    let meta = blocking(move || {
        workspace
            .write_artifact(&p.path, &bytes, p.overwrite)
            .map_err(ToolError::from)
    })
    .await?;
    Ok(serde_json::to_value(meta)?)
}

/// Decode a base64 request body, rejecting anything whose *decoded* size would
/// exceed the per-request chunk cap. Bounds the base64 length first so an
/// over-cap payload is refused before the buffer is materialized (base64 packs
/// 3 bytes into 4 chars), then verifies the decoded length exactly.
fn decode_bounded(data_base64: &str) -> Result<Vec<u8>, ToolError> {
    let max_encoded = (CHUNK_MAX_DECODED as u64).div_ceil(3) * 4;
    if data_base64.len() as u64 > max_encoded {
        return Err(ToolError::PayloadTooLarge(CHUNK_MAX_DECODED));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.as_bytes())
        .map_err(|_| ToolError::InvalidBase64)?;
    if bytes.len() > CHUNK_MAX_DECODED {
        return Err(ToolError::PayloadTooLarge(CHUNK_MAX_DECODED));
    }
    Ok(bytes)
}

/// Probe backend readiness without mutating anything: a 5-second Blender
/// liveness check, an OpenSCAD binary probe, and workspace status. Never fails
/// as a whole — a downed backend is reported, not raised.
async fn status(
    workspace: &Workspace,
    blender: &BlenderClient,
    settings: &Settings,
    scad: &ScadRunner,
    jobs: Option<&Arc<JobRegistry>>,
) -> Result<Value, ToolError> {
    let mut blender_json = json!({
        "available": false,
        "host": settings.blender_host,
        "port": settings.blender_port,
    });
    match blender
        .send_value(
            "bridge_status",
            Params::new(),
            Deadline::new(Duration::from_secs(5)),
        )
        .await
    {
        Ok(backend) => {
            blender_json["available"] = json!(true);
            blender_json["version"] = backend
                .get("blender_version")
                .cloned()
                .unwrap_or(Value::Null);
            blender_json["addon_version"] = json!(blender.addon_version());
            blender_json["render_device"] =
                backend.get("render_device").cloned().unwrap_or(Value::Null);
            blender_json["cycles_devices"] = backend
                .get("cycles_devices")
                .cloned()
                .unwrap_or(Value::Null);
            blender_json["commands"] = backend.get("commands").cloned().unwrap_or(Value::Null);
            blender_json["native_observation"] = backend
                .get("native_observation")
                .cloned()
                .unwrap_or(Value::Null);
            blender_json["scene_state"] =
                backend.get("scene_state").cloned().unwrap_or(Value::Null);
            blender_json["execution_limits"] = backend
                .get("execution_limits")
                .cloned()
                .unwrap_or(Value::Null);
        }
        Err(e) => {
            blender_json["error"] = json!(e.to_string());
        }
    }
    if let Some(warning) = blender.pop_version_warning() {
        blender_json["compatibility_warning"] = json!(warning);
    }

    let openscad = scad.binary();
    let ws = workspace.status();
    let render_jobs = match jobs {
        Some(registry) => registry.health().await,
        None => json!({
            "queue_depth": settings.render_job_queue_depth,
            "queued": 0,
            "running": 0,
            "encoder": {
                "available": false,
                "binary": settings.ffmpeg_bin,
                "error": "durable render job registry is unavailable",
            },
        }),
    };

    // The workspace status struct uses `root`/`blender_root`; the reported keys
    // are `workspace_root`/`blender_workspace_root`, so map by hand.
    Ok(json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "transport": "streamable-http",
        "http": {
            "host": settings.http_host,
            "port": settings.http_port,
            "path": "/mcp",
        },
        "blender": blender_json,
        "openscad": {
            "available": openscad.is_some(),
            "binary": openscad.map(|p| p.display().to_string()),
            "concurrency": settings.scad_concurrency,
        },
        "render_jobs": render_jobs,
        "cad": {"configured": settings.cad_endpoint.is_some()},
        "slicer": {"configured": settings.slicer_endpoint.is_some()},
        "printers": {"configured": settings.printers.is_some()},
        "workspace": {
            "confined": ws.confined,
            "workspace_root": ws.root,
            "blender_workspace_root": ws.blender_root,
            "max_transfer_bytes": ws.max_transfer_bytes,
            "max_list_scan_entries": ws.max_list_scan_entries,
        },
    }))
}

#[cfg(test)]
mod schema_portability_tests {
    use std::collections::HashSet;

    use serde_json::{Map, Value, json};

    use super::{
        JSON_SCHEMA_TYPES, PrimitiveCreateParams, TOOLS, normalize_type_unions, schema_of,
    };

    const BOOLEAN_SCHEMA_KEYWORDS: [&str; 4] = [
        "additionalProperties",
        "unevaluatedProperties",
        "additionalItems",
        "unevaluatedItems",
    ];
    const CONSTRAINING_KEYWORDS: [&str; 43] = [
        "type",
        "enum",
        "const",
        "multipleOf",
        "maximum",
        "exclusiveMaximum",
        "minimum",
        "exclusiveMinimum",
        "maxLength",
        "minLength",
        "pattern",
        "format",
        "contentMediaType",
        "contentEncoding",
        "contentSchema",
        "maxItems",
        "minItems",
        "uniqueItems",
        "maxContains",
        "minContains",
        "maxProperties",
        "minProperties",
        "required",
        "dependentRequired",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "items",
        "prefixItems",
        "contains",
        "additionalItems",
        "unevaluatedItems",
        "properties",
        "patternProperties",
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "dependentSchemas",
        "dependencies",
        "$ref",
        "$dynamicRef",
        "$recursiveRef",
    ];

    fn for_each_subschema(node: &Map<String, Value>, mut visit: impl FnMut(&Value, &str)) {
        for keyword in [
            "properties",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
            "$defs",
            "definitions",
        ] {
            if let Some(children) = node.get(keyword).and_then(Value::as_object) {
                for child in children.values() {
                    visit(child, keyword);
                }
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(children) = node.get(keyword).and_then(Value::as_array) {
                for child in children {
                    visit(child, keyword);
                }
            }
        }
        for keyword in [
            "items",
            "contains",
            "not",
            "propertyNames",
            "if",
            "then",
            "else",
            "additionalProperties",
            "unevaluatedProperties",
            "additionalItems",
            "unevaluatedItems",
            "contentSchema",
        ] {
            let Some(child) = node.get(keyword) else {
                continue;
            };
            if keyword == "items"
                && let Some(children) = child.as_array()
            {
                for child in children {
                    visit(child, keyword);
                }
                continue;
            }
            visit(child, keyword);
        }
    }

    fn schema_declares_id(schema: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let Some(node) = schema.as_object() else {
            return false;
        };
        if node
            .get("$id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            return true;
        }
        let mut found = false;
        for_each_subschema(node, |child, _| {
            found |= schema_declares_id(child, depth + 1);
        });
        found
    }

    fn inspector_findings(schema: &Value) -> Vec<&'static str> {
        fn walk(
            schema: &Value,
            parent_keyword: Option<&str>,
            depth: usize,
            has_embedded_ids: bool,
            findings: &mut Vec<&'static str>,
        ) {
            if depth > 64 {
                return;
            }
            if schema.is_boolean() {
                if parent_keyword.is_none_or(|keyword| !BOOLEAN_SCHEMA_KEYWORDS.contains(&keyword))
                {
                    findings.push("boolean-schema");
                }
                return;
            }
            let Some(node) = schema.as_object() else {
                return;
            };
            if node
                .get("type")
                .and_then(Value::as_array)
                .is_some_and(|types| {
                    !types.is_empty()
                        && types.iter().all(|item| {
                            item.as_str()
                                .is_some_and(|name| JSON_SCHEMA_TYPES.contains(&name))
                        })
                        && types
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<HashSet<_>>()
                            .len()
                            == types.len()
                })
            {
                findings.push("type-union");
            }
            if !has_embedded_ids
                && node
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|reference| !reference.is_empty() && !reference.starts_with('#'))
            {
                findings.push("remote-ref");
            }
            let constrains = node
                .keys()
                .any(|keyword| CONSTRAINING_KEYWORDS.contains(&keyword.as_str()))
                || (node.contains_key("if")
                    && (node.contains_key("then") || node.contains_key("else")));
            if !constrains && parent_keyword != Some("not") {
                findings.push("untyped-schema");
            }
            for_each_subschema(node, |child, keyword| {
                walk(child, Some(keyword), depth + 1, has_embedded_ids, findings);
            });
        }

        let has_embedded_ids = schema_declares_id(schema, 0);
        let mut findings = Vec::new();
        walk(schema, None, 0, has_embedded_ids, &mut findings);
        findings
    }

    fn validates(schema: &Value, instance: &Value) -> bool {
        jsonschema::validator_for(schema)
            .expect("published schema compiles")
            .is_valid(instance)
    }

    #[test]
    fn catalog_schemas_pass_mcp_inspector_portability_rules() {
        let findings = TOOLS
            .iter()
            .flat_map(|tool| {
                let schema = Value::Object((*(tool.schema)()).clone());
                inspector_findings(&schema)
                    .into_iter()
                    .map(move |rule| format!("{}.inputSchema: {rule}", tool.name))
            })
            .collect::<Vec<_>>();
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn normalized_unions_preserve_runtime_values_and_nullable_fields() {
        let original = json!({
            "type": ["string", "null"],
            "anyOf": [{"maxLength": 3}]
        });
        let mut normalized = original.clone();
        normalize_type_unions(&mut normalized);
        for instance in [json!(null), json!("ok"), json!("long"), json!(7), json!({})] {
            assert_eq!(
                validates(&original, &instance),
                validates(&normalized, &instance),
                "normalization changed the value domain for {instance}"
            );
        }
        assert!(normalized.get("type").is_none());
        assert!(normalized["allOf"][0]["anyOf"].is_array());

        let primitive = Value::Object((*schema_of::<PrimitiveCreateParams>()).clone());
        assert!(validates(&primitive, &json!({"primitive": "cube"})));
        assert!(validates(
            &primitive,
            &json!({"primitive": "cube", "name": null})
        ));
        assert!(!validates(
            &primitive,
            &json!({"primitive": "cube", "name": 7})
        ));
    }
}

#[cfg(test)]
mod visual_contract_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(unix)]
    #[test]
    fn geometry_worker_file_entrypoint_does_not_buffer_large_inputs() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("tempdir");
        let fixed = directory.path().join("fixed.stl");
        let moving = directory.path().join("moving.stl");
        for path in [&fixed, &moving] {
            std::fs::File::create(path)
                .expect("create sparse geometry input")
                .set_len(1024 * 1024 * 1024)
                .expect("size sparse geometry input");
        }
        let worker = directory.path().join("fake-worker");
        std::fs::write(
            &worker,
            "#!/bin/sh\nprintf '%s' '{\"report\":{\"path_inputs\":true}}'\n",
        )
        .expect("write fake worker");
        let mut permissions = std::fs::metadata(&worker)
            .expect("fake worker metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&worker, permissions).expect("make fake worker executable");

        let report = run_geometry_worker_files(
            &worker,
            1024 * 1024 * 1024,
            &fixed,
            &moving,
            AssemblyOptions::default(),
        )
        .expect("path-based worker does not read inputs in the server");
        assert_eq!(report["path_inputs"], json!(true));
    }

    #[test]
    fn scad_source_accepts_the_exact_cap_and_rejects_the_next_byte() {
        assert!(validate_scad_source(&" ".repeat(1024 * 1024)).is_ok());
        let error = validate_scad_source(&" ".repeat(1024 * 1024 + 1))
            .expect_err("source over cap rejected");
        assert_eq!(error.code(), "validation");
    }

    #[test]
    fn generated_scad_output_accepts_the_exact_cap_and_rejects_other_shapes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let exact = directory.path().join("exact-output");
        std::fs::File::create(&exact)
            .expect("create exact output")
            .set_len(25 * 1024 * 1024)
            .expect("size exact output");
        assert_eq!(
            read_generated_bounded(&exact)
                .expect("exact-cap output accepted")
                .len(),
            25 * 1024 * 1024
        );

        let oversized = directory.path().join("oversized-output");
        std::fs::File::create(&oversized)
            .expect("create oversized output")
            .set_len(25 * 1024 * 1024 + 1)
            .expect("size oversized output");
        assert_eq!(
            read_generated_bounded(&oversized)
                .expect_err("oversized output rejected")
                .code(),
            "write_too_large"
        );

        let empty = directory.path().join("empty-output");
        std::fs::File::create(&empty).expect("create empty output");
        assert_eq!(
            read_generated_bounded(&empty)
                .expect_err("empty output rejected")
                .code(),
            "validation"
        );
        assert_eq!(
            read_generated_bounded(directory.path())
                .expect_err("directory output rejected")
                .code(),
            "validation"
        );
    }

    #[test]
    fn scad_svg_requires_one_complete_well_formed_svg_document() {
        assert!(
            validate_scad_svg(
                br#"<?xml version="1.0"?><!DOCTYPE svg><svg xmlns="http://www.w3.org/2000/svg"><text>one &amp; <![CDATA[two]]></text><path d="M0 0 &amp; 1"/></svg>"#
            )
            .is_ok()
        );
        assert!(validate_scad_svg(b" \n<svg/>\n ").is_ok());
        for invalid in [
            b"<svg><path/>".as_slice(),
            b"<path/></svg>".as_slice(),
            b"not svg".as_slice(),
            b"\xff<svg></svg>".as_slice(),
            b"<svg><path></svg>".as_slice(),
            b"<svg bad='unterminated></svg>".as_slice(),
            b"<svg duplicate='one' duplicate='two'/>".as_slice(),
            b"<svg></svg><svg></svg>".as_slice(),
            b"<svg>&external;</svg>".as_slice(),
            b"outside<svg/>".as_slice(),
            b"<svg/>outside".as_slice(),
            b"<![CDATA[outside]]><svg/>".as_slice(),
            b"<svg/><![CDATA[outside]]>".as_slice(),
            b"<?xml version='1.0'?><?xml version='1.0'?><svg/>".as_slice(),
            b"<!DOCTYPE svg><!DOCTYPE svg><svg/>".as_slice(),
            b"<!DOCTYPE svg><?xml version='1.0'?><svg/>".as_slice(),
            b"<svg><?xml version='1.0'?></svg>".as_slice(),
            b"<svg/><?xml version='1.0'?>".as_slice(),
            b"<svg><!DOCTYPE svg></svg>".as_slice(),
            b"<svg/><!DOCTYPE svg>".as_slice(),
        ] {
            assert!(validate_scad_svg(invalid).is_err());
        }
    }

    #[test]
    fn render_png_requires_a_complete_decodable_image_with_matching_dimensions() {
        let bytes = encode_png(RgbImage::new(2, 3)).expect("encode test PNG");
        assert!(validate_png(&bytes, 2, 3).is_ok());
        assert!(validate_png(&bytes, 3, 3).is_err());
        assert!(validate_png(&bytes, 2, 2).is_err());
        assert!(validate_png(&bytes[..bytes.len() - 4], 2, 3).is_err());
        for (width, height) in [(0, 3), (2, 0), (8193, 1), (1, 8193)] {
            assert!(validate_png(&bytes, width, height).is_err());
        }
        let max_width = encode_png(RgbImage::new(8192, 1)).expect("encode maximum-width PNG");
        assert!(validate_png(&max_width, 8192, 1).is_ok());
        let max_height = encode_png(RgbImage::new(1, 8192)).expect("encode maximum-height PNG");
        assert!(validate_png(&max_height, 1, 8192).is_ok());
        let high_resolution =
            encode_png(RgbImage::new(3000, 3000)).expect("encode large valid PNG");
        assert!(validate_png(&high_resolution, 3000, 3000).is_ok());
        let over_width = encode_png(RgbImage::new(8193, 1)).expect("encode over-width PNG");
        assert!(validate_png(&over_width, 8193, 1).is_err());
        let over_height = encode_png(RgbImage::new(1, 8193)).expect("encode over-height PNG");
        assert!(validate_png(&over_height, 1, 8193).is_err());
    }

    #[tokio::test]
    async fn product_artifact_verification_rejects_each_integrity_drift() {
        let directory = tempfile::tempdir().expect("workspace");
        let workspace =
            Arc::new(Workspace::open(Some(directory.path()), None).expect("confined workspace"));
        let path = "product-frame.png";
        let bytes = encode_png(RgbImage::new(2, 3)).expect("encode product frame");
        workspace
            .write_artifact(path, &bytes, false)
            .expect("write product frame");
        let valid = json!({
            "size_bytes": bytes.len(),
            "sha256": sha256_hex(&bytes)
        });
        assert_eq!(
            verify_product_png_artifact(Arc::clone(&workspace), path.to_string(), &valid, 2, 3)
                .await
                .expect("valid product frame"),
            bytes
        );

        let mut zero = valid.clone();
        zero["size_bytes"] = json!(0);
        assert!(
            verify_product_png_artifact(Arc::clone(&workspace), path.to_string(), &zero, 2, 3)
                .await
                .expect_err("zero byte report rejected")
                .to_string()
                .contains("positive size_bytes")
        );

        for (field, replacement) in [
            ("size", json!(bytes.len() + 1)),
            ("digest", json!("0".repeat(64))),
        ] {
            let mut mismatched = valid.clone();
            match field {
                "size" => mismatched["size_bytes"] = replacement,
                "digest" => mismatched["sha256"] = replacement,
                _ => unreachable!(),
            }
            assert!(
                verify_product_png_artifact(
                    Arc::clone(&workspace),
                    path.to_string(),
                    &mismatched,
                    2,
                    3
                )
                .await
                .is_err(),
                "{field} drift must fail before product progress"
            );
        }
    }

    #[test]
    fn png_container_requires_one_empty_terminal_iend_chunk() {
        let png = encode_png(RgbImage::new(2, 3)).expect("encode test PNG");
        let mut trailing = png.clone();
        trailing.push(0);
        assert!(validate_complete_png_container(&trailing).is_err());

        let iend_offset = png.len() - 12;
        let mut nonempty_iend = png[..iend_offset].to_vec();
        nonempty_iend.extend_from_slice(&1_u32.to_be_bytes());
        nonempty_iend.extend_from_slice(b"IEND");
        nonempty_iend.push(0);
        nonempty_iend.extend_from_slice(&[0; 4]);
        assert!(validate_complete_png_container(&nonempty_iend).is_err());
    }

    #[test]
    fn scad_render_size_accepts_both_endpoints() {
        assert!(validate_scad_render_size(1).is_ok());
        assert!(validate_scad_render_size(8192).is_ok());
        assert!(validate_scad_render_size(0).is_err());
        assert!(validate_scad_render_size(8193).is_err());
    }

    #[test]
    fn geometry_worker_preserves_every_public_domain_error_code() {
        for code in [
            "empty_mesh",
            "invalid_stl",
            "non_finite_geometry",
            "invalid_mesh_index",
            "mesh_too_large",
            "validation",
            "invalid_assembly_part",
            "geometry_worker_io",
        ] {
            assert_eq!(worker_error_code(code), code);
        }
        assert_eq!(worker_error_code("unexpected"), "geometry");
    }

    #[test]
    fn geometry_worker_output_distinguishes_protocol_domain_and_resource_failures() {
        let report = parse_geometry_worker_output(true, br#"{"report":{"relation":"clear"}}"#)
            .expect("valid report");
        assert_eq!(report["relation"], json!("clear"));

        let protocol = parse_geometry_worker_output(true, b"not json").unwrap_err();
        assert_eq!(protocol.code(), "geometry_worker_protocol");
        let omitted = parse_geometry_worker_output(true, br#"{}"#).unwrap_err();
        assert_eq!(omitted.code(), "geometry_worker_protocol");

        let domain = parse_geometry_worker_output(
            false,
            br#"{"error":{"code":"invalid_assembly_part","message":"moving is open"}}"#,
        )
        .unwrap_err();
        assert_eq!(domain.code(), "invalid_assembly_part");
        assert!(domain.to_string().contains("moving is open"));

        for stdout in [b"not json".as_slice(), br#"{"report":{}}"#] {
            let resource = parse_geometry_worker_output(false, stdout).unwrap_err();
            assert_eq!(resource.code(), "geometry_resource_limit");
        }
    }

    fn diagnostic_contract_response(counts: [u64; 6]) -> Value {
        let [
            source_instances,
            vertices,
            edges,
            faces,
            loops,
            attribute_values,
        ] = counts;
        let bounds = json!({
            "minimum": [-1.0, -1.0, -1.0],
            "maximum": [1.0, 1.0, 1.0],
            "dimensions": [2.0, 2.0, 2.0],
            "center": [0.0, 0.0, 0.0],
            "diagonal": 12.0_f64.sqrt(),
            "coordinate_space": "world",
            "unit": "blender_unit",
        });
        json!({
            "path": "diagnostic.png",
            "size_bytes": 1,
            "media_type": "image/png",
            "width": 1,
            "height": 1,
            "engine": "BLENDER_EEVEE_NEXT",
            "render_device": "GRAPHICS",
            "graphics_backend": null,
            "samples": null,
            "mode": "overhang",
            "objects": null,
            "source_bounds": bounds,
            "rendered_bounds": bounds,
            "analysis": {
                "source_instances": source_instances,
                "evaluated_vertices": vertices,
                "evaluated_edges": edges,
                "evaluated_faces": faces,
                "evaluated_loops": loops,
                "copied_attribute_values": attribute_values,
                "build_direction": [0.0, 0.0, 1.0],
                "overhang_angle_degrees": 45.0,
                "categories": {
                    "supported": {"faces": faces, "area": 1.0},
                    "warning": {"faces": 0, "area": 0.0},
                    "severe": {"faces": 0, "area": 0.0},
                },
            },
        })
    }

    fn validate_diagnostic_counts(counts: [u64; 6]) -> Result<RenderBounds, ToolError> {
        validate_diagnostic_response(
            &diagnostic_contract_response(counts),
            "overhang",
            "diagnostic.png",
            &MultiViewRenderSettings {
                expected_scene: None,
                width: 1,
                height: 1,
                engine: RenderEngine::Eevee,
                samples: None,
                timeout_seconds: 1.0,
            },
            None,
        )
    }

    #[test]
    fn diagnostic_topology_accepts_exact_boundaries_and_rejects_each_violation() {
        for counts in [
            [1, 1, 1, 1, 3, 0],
            [1, 1, 1, 1, 1, 0],
            [1, MAX_DIAGNOSTIC_VERTICES, 1, 1, 3, 0],
            [1, 1, MAX_DIAGNOSTIC_EDGES, 1, 3, 0],
            [1, 1, 1, MAX_DIAGNOSTIC_FACES, MAX_DIAGNOSTIC_FACES, 0],
            [1, 1, 1, 1, MAX_DIAGNOSTIC_LOOPS, 0],
            [1, 1, 1, 1, 3, MAX_DIAGNOSTIC_ATTRIBUTE_VALUES],
        ] {
            assert!(validate_diagnostic_counts(counts).is_ok(), "{counts:?}");
        }

        for counts in [
            [0, 1, 1, 1, 3, 0],
            [1, 0, 1, 1, 3, 0],
            [1, 1, 0, 1, 3, 0],
            [1, 1, 1, 0, 3, 0],
            [1, 1, 1, 2, 1, 0],
            [1, MAX_DIAGNOSTIC_VERTICES + 1, 1, 1, 3, 0],
            [1, 1, MAX_DIAGNOSTIC_EDGES + 1, 1, 3, 0],
            [
                1,
                1,
                1,
                MAX_DIAGNOSTIC_FACES + 1,
                MAX_DIAGNOSTIC_FACES + 1,
                0,
            ],
            [1, 1, 1, 1, MAX_DIAGNOSTIC_LOOPS + 1, 0],
            [1, 1, 1, 1, 3, MAX_DIAGNOSTIC_ATTRIBUTE_VALUES + 1],
        ] {
            assert!(validate_diagnostic_counts(counts).is_err(), "{counts:?}");
        }
    }

    #[test]
    fn every_gallery_preset_has_the_exact_label_and_direction() {
        let expected = [
            (GalleryView::Front, "FRONT", [0.0, -1.0, 0.0]),
            (GalleryView::Right, "RIGHT", [1.0, 0.0, 0.0]),
            (GalleryView::Back, "BACK", [0.0, 1.0, 0.0]),
            (GalleryView::Left, "LEFT", [-1.0, 0.0, 0.0]),
            (GalleryView::Top, "TOP", [0.0, 0.0, 1.0]),
            (GalleryView::Bottom, "BOTTOM", [0.0, 0.0, -1.0]),
            (GalleryView::Isometric, "ISOMETRIC", [1.0, -1.0, 1.0]),
        ];
        for (view, label, direction) in expected {
            assert_eq!(view.label_and_direction(), (label, direction));
        }
    }

    #[test]
    fn multiview_presentation_keeps_grounded_cameras_above_the_floor() {
        let engineering: ProductPresentation = serde_json::from_value(json!({
            "profile": "engineering"
        }))
        .expect("engineering presentation");
        validate_multiview_presentation(&engineering, [[0.0, 0.0, -1.0]])
            .expect("engineering views have no studio floor");

        let studio: ProductPresentation = serde_json::from_value(json!({
            "profile": "studio_dark"
        }))
        .expect("studio presentation");
        assert!(validate_multiview_presentation(&studio, [[0.0, 0.0, -1.0]]).is_err());
        validate_multiview_presentation(&studio, [[0.0, 0.0, 0.0]])
            .expect("a level studio view stays above the floor");
        validate_multiview_presentation(&studio, [[0.0, 0.0, 1.0]])
            .expect("an elevated studio view stays above the floor");
    }

    #[test]
    fn presented_multiview_validates_every_view_presentation_attestation() {
        let presentation: ProductPresentation = serde_json::from_value(json!({
            "profile": "studio_dark",
            "materials": [{
                "objects": ["Body"],
                "base_color_srgb": [0.7, 0.2, 0.1],
                "metallic": 0.1,
                "roughness": 0.4
            }]
        }))
        .expect("product presentation");
        let settings: MultiViewRenderSettings = serde_json::from_value(json!({
            "width": 32,
            "height": 24,
            "engine": "EEVEE",
            "timeout_seconds": 5.0
        }))
        .expect("render settings");
        let requested = vec![RenderViewRequest {
            path: "view.png".to_string(),
            label: "FRONT".to_string(),
            direction: [0.0, -1.0, 0.0],
        }];
        let bounds = json!({
            "minimum": [-1.0, -1.0, -1.0],
            "maximum": [1.0, 1.0, 1.0],
            "dimensions": [2.0, 2.0, 2.0],
            "center": [0.0, 0.0, 0.0],
            "diagonal": 12.0_f64.sqrt(),
            "coordinate_space": "world",
            "unit": "blender_unit"
        });
        let response = json!({
            "views": [{
                "path": "view.png",
                "label": "FRONT",
                "size_bytes": 100,
                "media_type": "image/png",
                "width": 32,
                "height": 24
            }],
            "engine": "BLENDER_EEVEE_NEXT",
            "render_device": "GRAPHICS",
            "graphics_backend": null,
            "samples": null,
            "bounds": bounds,
            "presentation": {
                "profile": "studio_dark",
                "views": [{
                    "profile": "studio_dark",
                    "camera": {
                        "type": "perspective",
                        "behavior": "profile",
                        "azimuth_degrees": -90.0,
                        "elevation_degrees": 0.0,
                        "position": [5.0, -5.0, 3.0],
                        "target": [0.0, 0.0, 0.0],
                        "lens_mm": 85.0,
                        "ortho_scale": null,
                        "sensor_width_mm": 36.0,
                        "clip_start": 0.01,
                        "clip_end": 100.0
                    },
                    "materials": {
                        "overrides": [{
                            "objects": ["Body"],
                            "base_color_srgb": [0.7, 0.2, 0.1],
                            "metallic": 0.1,
                            "roughness": 0.4
                        }]
                    },
                    "shading": {
                        "mode": "smooth_by_angle",
                        "angle_degrees": 30.0,
                        "presentation_only": true
                    },
                    "framing": {
                        "margin_percent": 15.0,
                        "bounds": bounds,
                        "instance_count": 1
                    },
                    "source_state_verified": true,
                    "cleanup_verified": true
                }]
            }
        });
        validate_render_views_response(&response, &requested, &settings, Some(&presentation))
            .expect("complete per-view presentation accepted");

        for (field, replacement) in [
            ("camera", json!(45.0)),
            ("material", json!(0.6)),
            ("shading", json!("preserve")),
            ("framing", json!(14.0)),
            ("source", json!(false)),
            ("cleanup", json!(false)),
        ] {
            let mut mismatched = response.clone();
            let view = &mut mismatched["presentation"]["views"][0];
            match field {
                "camera" => view["camera"]["azimuth_degrees"] = replacement,
                "material" => view["materials"]["overrides"][0]["roughness"] = replacement,
                "shading" => view["shading"]["mode"] = replacement,
                "framing" => view["framing"]["margin_percent"] = replacement,
                "source" => view["source_state_verified"] = replacement,
                "cleanup" => view["cleanup_verified"] = replacement,
                _ => unreachable!(),
            }
            assert!(
                validate_render_views_response(
                    &mismatched,
                    &requested,
                    &settings,
                    Some(&presentation)
                )
                .is_err(),
                "{field} drift must fail before gallery success"
            );
        }
    }

    #[test]
    fn gallery_layout_rejects_zero_axes_and_preserves_label_bar_geometry() {
        assert!(fit_composite_layout(0, 1, 8, 8).is_err());
        assert!(fit_composite_layout(1, 0, 8, 8).is_err());
        assert!(fit_composite_layout(1, 1, 0, 8).is_err());
        assert!(fit_composite_layout(1, 1, 8, 0).is_err());
        let layout = fit_composite_layout(4, 3, 8, 8).unwrap();
        assert_eq!(
            (
                layout.columns,
                layout.tile_w,
                layout.tile_h,
                layout.label_height
            ),
            (3, 8, 8, 28)
        );
        let portrait = fit_composite_layout(1, 1, 200, 400).unwrap();
        assert_eq!((portrait.tile_w, portrait.tile_h), (200, 400));
    }

    #[test]
    fn gallery_layout_fits_the_maximum_default_turntable_without_losing_sources() {
        let layout = fit_composite_layout(36, 4, 512, 512).unwrap();
        assert_eq!((layout.tile_w, layout.tile_h), (468, 468));
        let pixels = u64::from(layout.columns)
            * u64::from(layout.tile_w)
            * 9
            * u64::from(layout.tile_h + layout.label_height);
        assert!(pixels <= MAX_COMPOSITE_PIXELS);
        let next_pixels = u64::from(layout.columns)
            * u64::from(layout.tile_w + 1)
            * 9
            * u64::from(layout.tile_h + 1 + layout.label_height);
        assert!(next_pixels > MAX_COMPOSITE_PIXELS);
    }

    #[test]
    fn gallery_layout_fits_maximum_frame_portrait_and_landscape_sets() {
        for (source_width, source_height) in [(1024, 2048), (2048, 1024)] {
            let layout = fit_composite_layout(36, 4, source_width, source_height).unwrap();
            let rows = 9;
            let pixels = u64::from(layout.columns)
                * u64::from(layout.tile_w)
                * rows
                * u64::from(layout.tile_h + layout.label_height);
            assert!(pixels <= MAX_COMPOSITE_PIXELS);
            assert!(
                (u64::from(layout.tile_w) * u64::from(source_height))
                    .abs_diff(u64::from(layout.tile_h) * u64::from(source_width))
                    < u64::from(source_width.max(source_height))
            );
            let (next_width, next_height) = if source_width > source_height {
                (layout.tile_w + 1, layout.tile_w.div_ceil(2))
            } else {
                (layout.tile_h.div_ceil(2), layout.tile_h + 1)
            };
            let next_pixels = u64::from(layout.columns)
                * u64::from(next_width)
                * rows
                * u64::from(next_height + layout.label_height);
            assert!(next_pixels > MAX_COMPOSITE_PIXELS);
        }
    }

    #[test]
    fn review_source_and_batch_surfaces_accept_only_the_exact_boundaries() {
        assert!(validate_render_view_surface(1, 8192, 1024).is_ok());
        assert!(validate_render_view_surface(1, 8192, 1025).is_err());
        assert!(validate_render_view_surface(8, 8192, 1024).is_ok());
        assert!(validate_render_view_surface(9, 8192, 1024).is_err());
    }

    #[test]
    fn comparison_layout_validates_each_axis_and_the_exact_memory_boundary() {
        for (width, height) in [(0, 1), (1, 0), (4097, 1), (1, 4097)] {
            assert!(validate_comparison_layout(width, height).is_err());
        }
        assert_eq!(validate_comparison_layout(4096, 996).unwrap(), (8192, 1024));
        assert!(validate_comparison_layout(4096, 997).is_err());
    }

    #[test]
    fn decoded_image_dimensions_accept_the_exact_pixel_boundary_only() {
        assert!(validate_decoded_dimensions(4096, 4096).is_ok());
        assert!(validate_decoded_dimensions(4096, 4097).is_err());
    }

    #[test]
    fn fitted_decode_releases_source_resolution_before_tile_retention() {
        let source = RgbImage::from_pixel(64, 32, image::Rgb([10, 20, 30]));
        let bytes = encode_png(source).unwrap();
        let fitted = decode_fitted_png(&bytes, 8, 4).unwrap();
        assert_eq!(fitted.dimensions(), (8, 4));
    }

    #[tokio::test]
    async fn completed_visual_output_owns_its_inline_image() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(temporary.path()), None).unwrap());
        let before = encode_png(RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0]))).unwrap();
        let after = encode_png(RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 255]))).unwrap();
        workspace
            .write_artifact("before.png", &before, false)
            .unwrap();
        workspace
            .write_artifact("after.png", &after, false)
            .unwrap();

        let output = compare_renders(
            Arc::clone(&workspace),
            CompareRendersParams {
                before_path: "before.png".to_string(),
                after_path: "after.png".to_string(),
                path: "comparison.png".to_string(),
                panel_width: 16,
                panel_height: 12,
                include_inline: true,
            },
        )
        .await
        .unwrap();
        let captured = base64::engine::general_purpose::STANDARD
            .decode(output.inline_png_base64.unwrap())
            .unwrap();

        let replacement =
            encode_png(RgbImage::from_pixel(32, 40, image::Rgb([0, 255, 0]))).unwrap();
        workspace
            .write_artifact("comparison.png", &replacement, true)
            .unwrap();

        assert_ne!(captured, replacement);
        let captured = image::load_from_memory(&captured).unwrap().into_rgb8();
        assert_eq!(*captured.get_pixel(8, 34), image::Rgb([255, 0, 0]));
        assert_eq!(*captured.get_pixel(24, 34), image::Rgb([0, 0, 255]));
    }

    #[test]
    fn inline_capture_respects_caller_choice_and_exact_size_boundary() {
        let small = [1, 2, 3];
        let (disabled, disabled_content) = capture_inline_png(false, &small);
        assert_eq!(disabled["requested"], json!(false));
        assert_eq!(disabled["included"], json!(false));
        assert!(disabled_content.is_none());

        let exact = vec![7; PREVIEW_INLINE_MAX_BYTES as usize];
        let (included, included_content) = capture_inline_png(true, &exact);
        assert_eq!(included["included"], json!(true));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(included_content.unwrap())
                .unwrap(),
            exact
        );

        let over_limit = vec![9; PREVIEW_INLINE_MAX_BYTES as usize + 1];
        let (excluded, excluded_content) = capture_inline_png(true, &over_limit);
        assert_eq!(excluded["included"], json!(false));
        assert_eq!(
            excluded["reason"],
            json!("artifact exceeds inline transport limit")
        );
        assert!(excluded_content.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visual_operations_hold_one_process_wide_memory_permit() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(visual_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok::<_, ToolError>(())
        }));
        tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
            .await
            .unwrap();
        assert_eq!(VISUAL_OPS.available_permits(), 0);

        let second_entered = Arc::new(AtomicBool::new(false));
        let second_flag = Arc::clone(&second_entered);
        let second = tokio::spawn(visual_blocking(move || {
            second_flag.store(true, Ordering::SeqCst);
            Ok::<_, ToolError>(())
        }));
        tokio::task::yield_now().await;
        assert!(!second_entered.load(Ordering::SeqCst));

        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(second_entered.load(Ordering::SeqCst));
    }

    #[test]
    fn alpha_compositing_covers_transparent_opaque_and_partial_pixels() {
        assert_eq!(composite_channel(200, 40, 0), 40);
        assert_eq!(composite_channel(200, 40, 255), 200);
        assert_eq!(composite_channel(200, 40, 128), 120);
    }
}
