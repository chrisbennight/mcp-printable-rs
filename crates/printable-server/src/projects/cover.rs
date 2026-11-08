//! Cover pictures shown on project library cards.
//!
//! Covers are copied into `.printable/covers/<project_id>/` under their SHA-256
//! digest, so a card keeps its picture when the project file it came from is
//! replaced or deleted. Stored covers are never rewritten; replacing a cover
//! records a new file.

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};
use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Project, get, modify, resolve};
use crate::{error::ToolError, provenance, slicing::package};

const MAX_COVER_BYTES: u64 = package::MAX_THUMBNAIL_BYTES;
const MAX_COVER_SIDE: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
/// Square size of a cover rendered from a model.
pub const MODEL_VIEW_SIZE: u32 = 512;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cover {
    /// Reserved workspace path of the stored picture.
    pub path: String,
    pub sha256: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub origin: CoverOrigin,
}

/// Where a stored cover came from.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoverOrigin {
    /// A picture file in the project.
    Image { path: String },
    /// A plate thumbnail stored in a sliced 3MF package.
    SliceThumbnail { package: String, plate: u32 },
    /// An isometric OpenSCAD view of a model file.
    ModelView { model: String },
}

impl Cover {
    pub(super) fn belongs_to(&self, project_id: &str) -> bool {
        let digest = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let expected = extension(&self.media_type)
            .map(|ext| format!(".printable/covers/{project_id}/{}.{ext}", self.sha256));
        digest && expected.as_deref() == Some(self.path.as_str())
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoverParams {
    pub project_id: String,
    /// The `record_version` from the record being edited. The change is
    /// refused when the stored record has changed since it was read.
    pub expected_record_version: u64,
    pub source: CoverSource,
    /// Caller-selected positive work budget in seconds for rendering a model
    /// view. Defaults to one hour.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: f64,
}

fn default_timeout_seconds() -> f64 {
    3600.0
}

fn first_plate() -> u32 {
    1
}

/// Paths are relative to the project directory.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CoverSource {
    /// The first plate thumbnail of the newest sliced 3MF package in the
    /// project; otherwise an isometric view of its newest STL model.
    Automatic,
    /// Remove the cover.
    None,
    /// A PNG, JPEG or WebP picture in the project.
    Image { path: String },
    /// A plate thumbnail from a sliced 3MF package in the project.
    SliceThumbnail {
        package: String,
        #[serde(default = "first_plate")]
        plate: u32,
    },
    /// An isometric OpenSCAD view of an STL model in the project.
    ModelView { model: String },
}

/// Cover bytes ready to store, with their origin.
pub struct Picture {
    bytes: Vec<u8>,
    origin: CoverOrigin,
}

impl Picture {
    pub fn rendered(bytes: Vec<u8>, model: String) -> Self {
        Self {
            bytes,
            origin: CoverOrigin::ModelView { model },
        }
    }
}

/// What a cover request needs before it can be committed.
pub enum Plan {
    Remove,
    Store(Picture),
    /// Render this workspace STL path with OpenSCAD, then store the result.
    RenderModel {
        model: String,
    },
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Validation(message.into())
}

fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// OpenSCAD source that imports one workspace STL. The path is quoted in an
/// OpenSCAD string, so only paths without quotes or backslashes qualify.
pub fn model_view_source(model: &str) -> String {
    format!("import(\"{model}\");")
}

fn require_model(path: &str) -> Result<(), ToolError> {
    if !path.ends_with(".stl") || path.contains(['"', '\\']) || path.chars().any(char::is_control) {
        return Err(invalid(
            "a model view needs an STL path without quotes, backslashes or control characters",
        ));
    }
    Ok(())
}

fn read_picture(workspace: &Workspace, path: &str) -> Result<Vec<u8>, ToolError> {
    if !["png", "jpg", "jpeg", "webp"]
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
    {
        return Err(invalid("a cover image must be a PNG, JPEG or WebP file"));
    }
    let snapshot = workspace.snapshot_artifact_bounded(path, MAX_COVER_BYTES)?;
    Ok(std::fs::read(snapshot.path())?)
}

fn read_thumbnail(workspace: &Workspace, path: &str, plate: u32) -> Result<Vec<u8>, ToolError> {
    if !path.ends_with(".3mf") {
        return Err(invalid("a slice thumbnail must come from a 3MF package"));
    }
    let snapshot = workspace.snapshot_artifact(path)?;
    package::plate_thumbnail(snapshot.path(), plate)
}

fn automatic(workspace: &Workspace, project: &Project) -> Result<Plan, ToolError> {
    let mut entries = match workspace.list_artifacts(&project.root, usize::MAX) {
        Ok(entries) => entries,
        Err(WsError::NotFound(_)) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    entries.sort_by(|a, b| {
        b.modified_ns
            .cmp(&a.modified_ns)
            .then_with(|| a.path.cmp(&b.path))
    });
    for entry in entries
        .iter()
        .filter(|entry| entry.path.ends_with(".gcode.3mf"))
    {
        let snapshot = workspace.snapshot_artifact(&entry.path)?;
        let Ok(facts) = package::facts(snapshot.path()) else {
            continue;
        };
        if let Some(&plate) = facts.thumbnails.first() {
            return Ok(Plan::Store(Picture {
                bytes: package::plate_thumbnail(snapshot.path(), plate)?,
                origin: CoverOrigin::SliceThumbnail {
                    package: entry.path.clone(),
                    plate,
                },
            }));
        }
    }
    entries
        .iter()
        .find(|entry| require_model(&entry.path).is_ok())
        .map(|entry| Plan::RenderModel {
            model: entry.path.clone(),
        })
        .ok_or_else(|| {
            invalid("the project has no sliced 3MF thumbnail or STL model to make a cover from")
        })
}

/// Validate the request and gather the picture, deferring only model renders.
pub fn plan(workspace: &Workspace, params: &CoverParams) -> Result<Plan, ToolError> {
    let project = get(workspace, &params.project_id)?;
    if project.record_version != params.expected_record_version {
        return Err(invalid(format!(
            "project record changed: expected record_version {}, found {}; read the project again before editing",
            params.expected_record_version, project.record_version
        )));
    }
    let id = &params.project_id;
    Ok(match &params.source {
        CoverSource::None => Plan::Remove,
        CoverSource::Automatic => automatic(workspace, &project)?,
        CoverSource::Image { path } => {
            let path = resolve(workspace, id, path)?;
            Plan::Store(Picture {
                bytes: read_picture(workspace, &path)?,
                origin: CoverOrigin::Image { path },
            })
        }
        CoverSource::SliceThumbnail { package, plate } => {
            let package = resolve(workspace, id, package)?;
            Plan::Store(Picture {
                bytes: read_thumbnail(workspace, &package, *plate)?,
                origin: CoverOrigin::SliceThumbnail {
                    package,
                    plate: *plate,
                },
            })
        }
        CoverSource::ModelView { model } => {
            let model = resolve(workspace, id, model)?;
            require_model(&model)?;
            Plan::RenderModel { model }
        }
    })
}

/// Decode the picture under size limits and report its type and dimensions.
fn inspect(bytes: &[u8]) -> Result<(&'static str, u32, u32), ToolError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| invalid("cover picture cannot be read"))?;
    let media_type = match reader.format() {
        Some(ImageFormat::Png) => "image/png",
        Some(ImageFormat::Jpeg) => "image/jpeg",
        Some(ImageFormat::WebP) => "image/webp",
        _ => return Err(invalid("a cover picture must be PNG, JPEG or WebP")),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_COVER_SIDE);
    limits.max_image_height = Some(MAX_COVER_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| invalid(format!("cover picture cannot be decoded: {error}")))?;
    Ok((media_type, image.width(), image.height()))
}

fn store(workspace: &Workspace, project_id: &str, picture: Picture) -> Result<Cover, ToolError> {
    if picture.bytes.len() as u64 > MAX_COVER_BYTES {
        return Err(invalid(format!(
            "a cover picture must be at most {MAX_COVER_BYTES} bytes"
        )));
    }
    let (media_type, width, height) = inspect(&picture.bytes)?;
    let sha256 = provenance::digest(&picture.bytes);
    let ext = extension(media_type).expect("inspected cover types have extensions");
    let path = format!(".printable/covers/{project_id}/{sha256}.{ext}");
    match workspace.write_reserved_artifact(&path, &picture.bytes, false) {
        Ok(_) | Err(WsError::AlreadyExists(_)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(Cover {
        path,
        sha256,
        media_type: media_type.into(),
        width,
        height,
        origin: picture.origin,
    })
}

/// Store the picture (or none) and record it on the project.
pub fn commit(
    workspace: &Workspace,
    params: &CoverParams,
    picture: Option<Picture>,
) -> Result<Project, ToolError> {
    let cover = picture
        .map(|picture| store(workspace, &params.project_id, picture))
        .transpose()?;
    modify(
        workspace,
        &params.project_id,
        params.expected_record_version,
        |project| {
            project.cover = cover;
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{CreateParams, ProjectRequest, dispatch};
    use crate::slicing::package::fixture;
    use serde_json::json;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        dispatch(
            &workspace,
            ProjectRequest::Create(CreateParams {
                project_id: "desk".into(),
                name: "Desk organizer".into(),
                description: String::new(),
                adopt_existing: false,
            }),
        )
        .unwrap();
        (root, workspace)
    }

    fn params(version: u64, source: CoverSource) -> CoverParams {
        CoverParams {
            project_id: "desk".into(),
            expected_record_version: version,
            source,
            timeout_seconds: 60.0,
        }
    }

    fn apply(workspace: &Workspace, params: &CoverParams) -> Result<Project, ToolError> {
        match plan(workspace, params)? {
            Plan::Remove => commit(workspace, params, None),
            Plan::Store(picture) => commit(workspace, params, Some(picture)),
            Plan::RenderModel { model } => commit(
                workspace,
                params,
                Some(Picture::rendered(fixture::png(8, 8), model)),
            ),
        }
    }

    #[test]
    fn chosen_images_are_copied_and_survive_source_replacement() {
        let (root, workspace) = workspace();
        let picture = fixture::png(6, 4);
        workspace
            .write_artifact("projects/desk/photos/finished.png", &picture, false)
            .unwrap();
        let source = CoverSource::Image {
            path: "photos/finished.png".into(),
        };
        let project = apply(&workspace, &params(1, source.clone())).unwrap();
        let cover = project.cover.clone().unwrap();
        assert_eq!((cover.width, cover.height), (6, 4));
        assert_eq!(cover.media_type, "image/png");
        assert!(cover.belongs_to("desk"));
        assert_eq!(project.record_version, 2);
        workspace
            .write_artifact("projects/desk/photos/finished.png", b"replaced", true)
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join(&cover.path)).unwrap(),
            picture
        );
        assert_eq!(crate::projects::get(&workspace, "desk").unwrap(), project);
        let stale = apply(&workspace, &params(1, source))
            .unwrap_err()
            .to_string();
        assert!(stale.contains("record changed"), "{stale}");
        let removed = apply(&workspace, &params(2, CoverSource::None)).unwrap();
        assert_eq!(removed.cover, None);
    }

    #[test]
    fn invalid_pictures_and_paths_are_refused() {
        let (_root, workspace) = workspace();
        workspace
            .write_artifact("projects/desk/notes.png", b"not a picture", false)
            .unwrap();
        workspace
            .write_artifact("projects/desk/model.obj", b"o cube", false)
            .unwrap();
        for source in [
            CoverSource::Image {
                path: "notes.png".into(),
            },
            CoverSource::Image {
                path: "model.obj".into(),
            },
            CoverSource::Image {
                path: "../other/cover.png".into(),
            },
            CoverSource::ModelView {
                model: "model.obj".into(),
            },
        ] {
            assert!(apply(&workspace, &params(1, source)).is_err());
        }
        assert_eq!(
            crate::projects::get(&workspace, "desk").unwrap().cover,
            None
        );
    }

    #[test]
    fn automatic_prefers_the_newest_sliced_thumbnail_then_a_model_view() {
        let (_root, workspace) = workspace();
        assert!(apply(&workspace, &params(1, CoverSource::Automatic)).is_err());
        workspace
            .write_artifact("projects/desk/desk.stl", b"solid desk", false)
            .unwrap();
        let rendered = apply(&workspace, &params(1, CoverSource::Automatic)).unwrap();
        assert_eq!(
            rendered.cover.unwrap().origin,
            CoverOrigin::ModelView {
                model: "projects/desk/desk.stl".into()
            }
        );
        let thumbnail = fixture::png(3, 3);
        let package = fixture::package(&[
            ("Metadata/plate_2.gcode", b"G28"),
            ("Metadata/plate_2.png", &thumbnail),
        ]);
        workspace
            .write_artifact("projects/desk/slice-1/model.gcode.3mf", &package, false)
            .unwrap();
        let sliced = apply(&workspace, &params(2, CoverSource::Automatic)).unwrap();
        assert_eq!(
            sliced.cover.unwrap().origin,
            CoverOrigin::SliceThumbnail {
                package: "projects/desk/slice-1/model.gcode.3mf".into(),
                plate: 2
            }
        );
    }

    #[test]
    fn cover_requests_use_the_documented_wire_shape() {
        let request: ProjectRequest = serde_json::from_value(json!({"action":"cover","params":{
            "project_id":"desk","expected_record_version":3,
            "source":{"slice_thumbnail":{"package":"slice-1/model.gcode.3mf"}}}}))
        .unwrap();
        let ProjectRequest::Cover(params) = request else {
            panic!("cover request")
        };
        assert!(matches!(
            params.source,
            CoverSource::SliceThumbnail { plate: 1, .. }
        ));
        assert!(
            serde_json::from_value::<CoverSource>(json!("automatic")).is_ok()
                && serde_json::from_value::<CoverSource>(json!("none")).is_ok()
        );
    }
}
