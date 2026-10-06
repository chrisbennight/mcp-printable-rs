//! Durable project identities and paths shared by the modeling backends.

use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ToolError;

mod export;
pub mod requirements;
pub mod revisions;
pub use export::NativeExportParams;

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

pub fn get(workspace: &Workspace, id: &str) -> Result<Project, ToolError> {
    validate_id(id)?;
    let (_, bytes) = workspace.read_generated_bytes(&manifest_path(id))?;
    let project: Project = serde_json::from_slice(&bytes)?;
    if project.format_version != 1 || project.project_id != id || project.root != project_root(id) {
        return Err(ToolError::Validation(
            "project metadata has an unsupported version or inconsistent identity".into(),
        ));
    }
    Ok(project)
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
    let project = Project {
        format_version: 1,
        root: project_root(&params.project_id),
        project_id: params.project_id,
        name: params.name,
        description: params.description,
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
        Ok(existing) if &existing == requested => Ok(Some(existing)),
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
