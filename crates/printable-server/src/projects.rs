//! Durable project identities and paths shared by the modeling backends.

use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ToolError;

pub mod cover;
mod export;
pub mod requirements;
pub mod revisions;
pub use export::NativeExportParams;

/// Format written for new and updated project records. Version 1 records stay
/// readable unchanged until an explicit update rewrites them.
const FORMAT_VERSION: u32 = 2;
const MAX_TAGS: usize = 32;
const MAX_TAG_CHARS: usize = 64;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateParams {
    /// Stable project identifier: lowercase letters, digits, underscores, or hyphens.
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Deliberately use files already present under the project directory.
    #[serde(default)]
    pub adopt_existing: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectParams {
    pub project_id: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathParams {
    pub project_id: String,
    /// Artifact path relative to this project's files, without parent traversal.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesParams {
    pub project_id: String,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateParams {
    pub project_id: String,
    /// The `record_version` from the record being edited. The edit is refused
    /// when the stored record has changed since it was read.
    pub expected_record_version: u64,
    /// Replacement description.
    #[serde(default)]
    pub description: Option<String>,
    /// Replacement tag list. Tags are trimmed, 1 to 64 characters, without
    /// control characters; at most 32 remain after merging case-insensitive
    /// duplicates.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProjectRequest {
    Revise(revisions::ReviseParams),
    Revision(revisions::GetParams),
    Create(CreateParams),
    Get(ProjectParams),
    List(ListParams),
    Resolve(PathParams),
    Files(FilesParams),
    Update(UpdateParams),
    Cover(cover::CoverParams),
    ExportFiles(export::ExportFilesParams),
    ExportBlender(NativeExportParams),
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub format_version: u32,
    pub project_id: String,
    pub name: String,
    pub description: String,
    /// Workspace-relative directory shared by Blender, OpenSCAD, and CAD sources.
    pub root: String,
    /// Increases with every accepted update. Version 1 records report 0.
    #[serde(default)]
    pub record_version: u64,
    /// Creation time in Unix milliseconds; absent from version 1 records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_unix_ms: Option<u64>,
    /// True when the creation time was estimated from the version 1 record's
    /// file time during its first update.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created_estimated: bool,
    /// Time of the latest accepted change in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<cover::Cover>,
}

impl Project {
    fn valid_for(&self, id: &str) -> bool {
        let identity = self.project_id == id && self.root == project_root(id);
        let fields = match self.format_version {
            1 => {
                self.record_version == 0
                    && self.created_unix_ms.is_none()
                    && !self.created_estimated
                    && self.updated_unix_ms.is_none()
                    && self.tags.is_empty()
                    && self.cover.is_none()
            }
            FORMAT_VERSION => {
                self.record_version > 0
                    && self.created_unix_ms.is_some()
                    && self.updated_unix_ms.is_some()
                    && normalize_tags(self.tags.clone()).is_ok_and(|tags| tags == self.tags)
                    && self.cover.as_ref().is_none_or(|cover| cover.belongs_to(id))
            }
            _ => false,
        };
        identity && fields
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, ToolError> {
    let mut normalized: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty()
            || tag.chars().count() > MAX_TAG_CHARS
            || tag.chars().any(char::is_control)
        {
            return Err(ToolError::Validation(format!(
                "tags must be 1 to {MAX_TAG_CHARS} characters without control characters"
            )));
        }
        if !normalized
            .iter()
            .any(|existing| existing.to_lowercase() == tag.to_lowercase())
        {
            normalized.push(tag.to_owned());
        }
    }
    if normalized.len() > MAX_TAGS {
        return Err(ToolError::Validation(format!(
            "a project can have at most {MAX_TAGS} tags"
        )));
    }
    Ok(normalized)
}

fn validate_id(id: &str) -> Result<(), ToolError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
    {
        return Err(ToolError::Validation(
            "project_id must contain lowercase letters, digits, underscores, or hyphens".into(),
        ));
    }
    Ok(())
}

fn manifest_path(id: &str) -> String {
    format!(".printable/projects/{id}.json")
}
fn project_root(id: &str) -> String {
    format!("projects/{id}")
}

/// Read a project. Version 1 records report the record file's time as their
/// creation and update times, marked as estimated; the stored file is unchanged.
pub fn get(workspace: &Workspace, id: &str) -> Result<Project, ToolError> {
    let (mut project, modified_ms) = read_record(workspace, id)?;
    if project.format_version == 1 {
        project.created_unix_ms = Some(modified_ms);
        project.updated_unix_ms = Some(modified_ms);
        project.created_estimated = true;
    }
    Ok(project)
}

/// Read a project record with the file's modification time in Unix milliseconds.
fn read_record(workspace: &Workspace, id: &str) -> Result<(Project, u64), ToolError> {
    validate_id(id)?;
    let (meta, bytes) = workspace.read_generated_bytes(&manifest_path(id))?;
    let project: Project = serde_json::from_slice(&bytes)?;
    if !project.valid_for(id) {
        return Err(ToolError::Validation(
            "project metadata has an unsupported version or inconsistent identity".into(),
        ));
    }
    let modified_ms = u64::try_from(meta.modified_ns / 1_000_000).unwrap_or(0);
    Ok((project, modified_ms))
}

/// Apply one edit to the stored record under the project's update lock.
///
/// The lock serialises cooperating server processes; the version check
/// refuses edits prepared from a record that has since changed. A version 1
/// record becomes version 2 here, with its creation time estimated from the
/// record file's modification time.
pub(crate) fn modify(
    workspace: &Workspace,
    id: &str,
    expected_record_version: u64,
    edit: impl FnOnce(&mut Project) -> Result<(), ToolError>,
) -> Result<Project, ToolError> {
    validate_id(id)?;
    let _lock = workspace
        .try_lock_reserved(&format!(".printable/projects/{id}.lock"))
        .map_err(|error| match error {
            WsError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock => {
                ToolError::Validation(
                    "project record update is busy; read the project again before retrying".into(),
                )
            }
            other => other.into(),
        })?;
    let (mut project, modified_ms) = read_record(workspace, id)?;
    if project.record_version != expected_record_version {
        return Err(ToolError::Validation(format!(
            "project record changed: expected record_version {expected_record_version}, found {}; read the project again before editing",
            project.record_version
        )));
    }
    if project.format_version == 1 {
        project.format_version = FORMAT_VERSION;
        project.created_unix_ms = Some(modified_ms);
        project.created_estimated = true;
    }
    edit(&mut project)?;
    project.record_version += 1;
    project.updated_unix_ms = Some(unix_ms().max(project.created_unix_ms.unwrap_or(0)));
    workspace.write_reserved_artifact(&manifest_path(id), &serde_json::to_vec(&project)?, true)?;
    Ok(project)
}

fn update(workspace: &Workspace, params: UpdateParams) -> Result<Project, ToolError> {
    if params.description.is_none() && params.tags.is_none() {
        return Err(ToolError::Validation(
            "project update requires description or tags".into(),
        ));
    }
    let tags = params.tags.map(normalize_tags).transpose()?;
    modify(
        workspace,
        &params.project_id,
        params.expected_record_version,
        |project| {
            if let Some(description) = params.description {
                project.description = description;
            }
            if let Some(tags) = tags {
                project.tags = tags;
            }
            Ok(())
        },
    )
}

pub fn resolve(workspace: &Workspace, id: &str, path: &str) -> Result<String, ToolError> {
    let project = get(workspace, id)?;
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ToolError::Validation(
            "project artifact path must be relative without empty, dot, or parent components"
                .into(),
        ));
    }
    let joined = format!("{}/{path}", project.root);
    workspace.validate_public_mutation_path(&joined)?;
    Ok(joined)
}

fn create(workspace: &Workspace, params: CreateParams) -> Result<Project, ToolError> {
    validate_id(&params.project_id)?;
    if params.name.trim().is_empty() || params.name.chars().any(char::is_control) {
        return Err(ToolError::Validation(
            "project requires a nonempty name without control characters".into(),
        ));
    }
    let now = unix_ms();
    let project = Project {
        format_version: FORMAT_VERSION,
        root: project_root(&params.project_id),
        project_id: params.project_id,
        name: params.name,
        description: params.description,
        record_version: 1,
        created_unix_ms: Some(now),
        created_estimated: false,
        updated_unix_ms: Some(now),
        tags: Vec::new(),
        cover: None,
    };
    if let Some(existing) = existing_project(workspace, &project)? {
        return Ok(existing);
    }
    if !workspace.create_public_directory(&project.root)? && !params.adopt_existing {
        return existing_project(workspace, &project)?.ok_or_else(|| ToolError::Validation(
            "project directory already exists; set adopt_existing to use its files, or choose another project_id".into(),
        ));
    }
    let path = manifest_path(&project.project_id);
    match workspace.write_reserved_artifact(&path, &serde_json::to_vec(&project)?, false) {
        Ok(_) => Ok(project),
        Err(WsError::AlreadyExists(_)) => existing_project(workspace, &project)?.ok_or_else(|| {
            ToolError::Validation(
                "project metadata changed during creation; inspect before retrying".into(),
            )
        }),
        Err(error) => Err(error.into()),
    }
}

fn existing_project(
    workspace: &Workspace,
    requested: &Project,
) -> Result<Option<Project>, ToolError> {
    match get(workspace, &requested.project_id) {
        Ok(existing)
            if existing.name == requested.name
                && existing.description == requested.description =>
        {
            Ok(Some(existing))
        }
        Ok(_) => Err(ToolError::Validation("project_id already exists with different metadata; use project.get or choose another identifier".into())),
        Err(ToolError::Workspace(WsError::NotFound(_))) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn dispatch(workspace: &Workspace, request: ProjectRequest) -> Result<Value, ToolError> {
    match request {
        ProjectRequest::Revise(params) => revisions::revise(workspace, params),
        ProjectRequest::Revision(params) => revisions::read(workspace, params),
        ProjectRequest::ExportBlender(_) => Err(ToolError::Validation(
            "native export requires the Blender command dispatcher".into(),
        )),
        ProjectRequest::Cover(_) => Err(ToolError::Validation(
            "cover selection requires the OpenSCAD renderer".into(),
        )),
        ProjectRequest::Update(params) => Ok(serde_json::to_value(update(workspace, params)?)?),
        ProjectRequest::ExportFiles(params) => Ok(serde_json::to_value(export::export_files(
            workspace, params,
        )?)?),
        ProjectRequest::Create(params) => Ok(serde_json::to_value(create(workspace, params)?)?),
        ProjectRequest::Get(params) => {
            Ok(serde_json::to_value(get(workspace, &params.project_id)?)?)
        }
        ProjectRequest::Resolve(params) => Ok(
            json!({"project_id":params.project_id,"path":resolve(workspace, &params.project_id, &params.path)?}),
        ),
        ProjectRequest::List(params) => {
            if params.limit == 0 {
                return Err(WsError::InvalidLimit.into());
            }
            let files = match workspace.list_artifacts(".printable/projects", params.limit) {
                Ok(files) => files,
                Err(WsError::NotFound(_)) => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            let limit_reached = files.len() == params.limit;
            let mut projects = Vec::with_capacity(files.len());
            for file in files {
                let id = file
                    .path
                    .strip_prefix(".printable/projects/")
                    .and_then(|name| name.strip_suffix(".json"))
                    .ok_or_else(|| {
                        ToolError::Validation("unexpected project metadata path".into())
                    })?;
                projects.push(get(workspace, id)?);
            }
            Ok(
                json!({"requested_limit":params.limit,"returned":projects.len(),"projects":projects,"limit_reached":limit_reached}),
            )
        }
        ProjectRequest::Files(params) => {
            if params.limit == 0 {
                return Err(WsError::InvalidLimit.into());
            }
            let project = get(workspace, &params.project_id)?;
            let entries = match workspace.list_artifacts(&project.root, params.limit) {
                Ok(entries) => entries,
                Err(WsError::NotFound(_)) => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            Ok(
                json!({"project_id":project.project_id,"requested_limit":params.limit,"returned":entries.len(),"limit_reached":entries.len() == params.limit,"entries":entries}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_pages_report_counts_for_defaults_and_large_requests() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        for index in 0..1101 {
            create(
                &workspace,
                CreateParams {
                    project_id: format!("project-{index}"),
                    name: format!("Project {index}"),
                    description: String::new(),
                    adopt_existing: false,
                },
            )
            .unwrap();
            workspace
                .write_artifact(
                    &format!("projects/project-0/file-{index}.stl"),
                    b"fixture",
                    false,
                )
                .unwrap();
        }
        for (requested, returned) in [(100, 100), (1500, 1101), (usize::MAX, 1101)] {
            for (request, collection) in [
                (
                    ProjectRequest::List(ListParams { limit: requested }),
                    "projects",
                ),
                (
                    ProjectRequest::Files(FilesParams {
                        project_id: "project-0".into(),
                        limit: requested,
                    }),
                    "entries",
                ),
            ] {
                let output = dispatch(&workspace, request).unwrap();
                assert_eq!(output["requested_limit"], requested);
                assert_eq!(output["returned"], returned);
                assert_eq!(output[collection].as_array().unwrap().len(), returned);
                assert_eq!(output["limit_reached"], returned == requested);
            }
        }
        assert_eq!(
            serde_json::from_value::<ListParams>(json!({}))
                .unwrap()
                .limit,
            100
        );
        assert_eq!(
            serde_json::from_value::<FilesParams>(json!({"project_id":"project-0"}))
                .unwrap()
                .limit,
            100
        );
        for request in [
            ProjectRequest::List(ListParams { limit: 0 }),
            ProjectRequest::Files(FilesParams {
                project_id: "project-0".into(),
                limit: 0,
            }),
        ] {
            assert_eq!(
                dispatch(&workspace, request).unwrap_err().code(),
                "invalid_limit"
            );
        }
    }

    #[test]
    fn long_project_names_and_descriptions_reach_filesystem_storage() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let params = CreateParams {
            project_id: "p".repeat(65),
            name: "n".repeat(257),
            description: "d".repeat(4097),
            adopt_existing: false,
        };
        let created = create(&workspace, params.clone()).unwrap();
        assert_eq!(get(&workspace, &params.project_id).unwrap(), created);
    }

    #[test]
    fn projects_survive_reopen_and_resolve_distinct_backend_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        for id in ["sensor", "housing"] {
            let params = CreateParams {
                project_id: id.into(),
                name: id.into(),
                description: String::new(),
                adopt_existing: false,
            };
            let project = create(&workspace, params.clone()).unwrap();
            assert_eq!(create(&workspace, params).unwrap(), project);
            let path = resolve(&workspace, id, "source/design.scad").unwrap();
            workspace
                .write_artifact(&path, id.as_bytes(), false)
                .unwrap();
        }
        drop(workspace);
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        assert_eq!(get(&workspace, "sensor").unwrap().root, "projects/sensor");
        assert_eq!(
            workspace
                .read_artifact(&resolve(&workspace, "housing", "source/design.scad").unwrap())
                .unwrap()
                .1,
            b"housing"
        );
        let files = dispatch(
            &workspace,
            ProjectRequest::Files(FilesParams {
                project_id: "sensor".into(),
                limit: 100,
            }),
        )
        .unwrap();
        assert_eq!(
            files["entries"][0]["path"],
            "projects/sensor/source/design.scad"
        );
        assert_eq!(dispatch(&workspace,ProjectRequest::List(ListParams {limit:100})).unwrap()["projects"].as_array().unwrap().len(),2);
        for path in [
            "../housing/model.stl",
            "/model.stl",
            "nested/../../model.stl",
            "a\\b.stl",
        ] {
            assert!(resolve(&workspace, "sensor", path).is_err());
        }
        assert!(get(&workspace, "../sensor").is_err());
        assert!(
            workspace
                .write_artifact(".printable/projects/sensor.json", b"{}", true)
                .is_err()
        );
        assert!(
            create(
                &workspace,
                CreateParams {
                    project_id: "sensor".into(),
                    name: "different".into(),
                    description: String::new(),
                    adopt_existing: false,
                }
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    #[test]
    fn legacy_files_require_explicit_adoption_and_remain_intact() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let path = "projects/sensor/source/design.scad";
        workspace.write_artifact(path, b"cube(10);", false).unwrap();
        let mut params = CreateParams {
            project_id: "sensor".into(),
            name: "Sensor".into(),
            description: String::new(),
            adopt_existing: false,
        };
        assert!(create(&workspace, params.clone()).is_err());
        assert!(get(&workspace, "sensor").is_err());
        assert_eq!(workspace.read_artifact(path).unwrap().1, b"cube(10);");
        params.adopt_existing = true;
        let project = create(&workspace, params.clone()).unwrap();
        params.adopt_existing = false;
        assert_eq!(create(&workspace, params).unwrap(), project);
        assert_eq!(workspace.read_artifact(path).unwrap().1, b"cube(10);");
        assert!(
            !workspace
                .create_public_directory("projects/sensor")
                .unwrap()
        );
        assert!(
            workspace
                .create_public_directory(".printable/other")
                .is_err()
        );
        std::os::unix::fs::symlink(root.path(), root.path().join("alias")).unwrap();
        assert!(workspace.create_public_directory("alias").is_err());
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;

    fn legacy(workspace: &Workspace) {
        workspace
            .write_reserved_artifact(
                ".printable/projects/bracket.json",
                br#"{"format_version":1,"project_id":"bracket","name":"Bracket","description":"Old","root":"projects/bracket"}"#,
                false,
            )
            .unwrap();
    }

    fn update(version: u64, description: Option<&str>, tags: Option<&[&str]>) -> ProjectRequest {
        ProjectRequest::Update(UpdateParams {
            project_id: "bracket".into(),
            expected_record_version: version,
            description: description.map(str::to_owned),
            tags: tags.map(|tags| tags.iter().map(|tag| (*tag).to_owned()).collect()),
        })
    }

    #[test]
    fn version_1_records_report_estimates_and_upgrade_on_their_first_edit() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        legacy(&workspace);
        let read = get(&workspace, "bracket").unwrap();
        assert_eq!((read.format_version, read.record_version), (1, 0));
        assert!(read.created_estimated && read.created_unix_ms.is_some());
        let stored = std::fs::read(root.path().join(".printable/projects/bracket.json")).unwrap();
        assert!(!String::from_utf8(stored).unwrap().contains("created"));
        let edited = dispatch(
            &workspace,
            update(0, None, Some(&[" desk ", "Desk", "storage"])),
        )
        .unwrap();
        assert_eq!(edited["format_version"], 2);
        assert_eq!(edited["record_version"], 1);
        assert_eq!(edited["created_estimated"], true);
        assert_eq!(edited["created_unix_ms"], json!(read.created_unix_ms));
        assert_eq!(edited["tags"], json!(["desk", "storage"]));
        assert_eq!(edited["description"], "Old");
        assert_eq!(
            serde_json::to_value(get(&workspace, "bracket").unwrap()).unwrap(),
            edited
        );
    }

    #[test]
    fn stale_busy_empty_and_invalid_edits_are_refused_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        legacy(&workspace);
        dispatch(&workspace, update(0, Some("New"), None)).unwrap();
        let before = get(&workspace, "bracket").unwrap();
        let stale = dispatch(&workspace, update(0, Some("Other"), None)).unwrap_err();
        assert!(stale.to_string().contains("record changed"), "{stale}");
        let held = workspace
            .try_lock_reserved(".printable/projects/bracket.lock")
            .unwrap();
        let busy = dispatch(&workspace, update(1, Some("Other"), None)).unwrap_err();
        assert!(busy.to_string().contains("busy"), "{busy}");
        drop(held);
        for request in [
            update(1, None, None),
            update(1, None, Some(&[""])),
            update(1, None, Some(&["bad\ntag"])),
            update(1, None, Some(&[&"x".repeat(65)])),
        ] {
            assert!(dispatch(&workspace, request).is_err());
        }
        let too_many: Vec<String> = (0..33).map(|index| format!("tag-{index}")).collect();
        let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
        assert!(dispatch(&workspace, update(1, None, Some(&too_many))).is_err());
        assert_eq!(get(&workspace, "bracket").unwrap(), before);
    }

    #[test]
    fn new_projects_start_at_version_2_and_repeat_creation_returns_them() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let params = CreateParams {
            project_id: "desk".into(),
            name: "Desk".into(),
            description: "Organizer".into(),
            adopt_existing: false,
        };
        let created = create(&workspace, params.clone()).unwrap();
        assert_eq!((created.format_version, created.record_version), (2, 1));
        assert!(!created.created_estimated);
        assert_eq!(created.created_unix_ms, created.updated_unix_ms);
        assert_eq!(create(&workspace, params).unwrap(), created);
    }
}
