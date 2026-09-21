//! Explicit, bounded project file bundles; no directory-wide collection.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
};

use printable_workspace::{ArtifactMeta, Workspace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::error::ToolError;

const MAX_FILES: usize = 256;
const MAX_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = MAX_SOURCE_BYTES + 4 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportFilesParams {
    pub project_id: String,
    /// Explicit project-relative files. Nothing else is collected automatically.
    pub files: Vec<String>,
    /// New project-relative .zip artifact; an existing destination is never replaced.
    pub output_path: String,
}

#[derive(Debug, Serialize)]
pub struct ExportFilesResult {
    pub artifact: ArtifactMeta,
    pub sha256: String,
    pub manifest: BundleManifest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BundleManifest {
    pub format_version: u32,
    pub project_id: String,
    pub scope: String,
    /// Selected files have not been inspected for native engine dependencies.
    pub dependencies_inspected: bool,
    pub files: Vec<BundleFile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BundleFile {
    pub path: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub sha256: String,
}

fn validate_selection_path(path: &str) -> Result<(), ToolError> {
    if path.len() > 1024
        || path.chars().any(char::is_control)
        || path.split('/').any(|component| {
            let component = component.to_ascii_lowercase();
            component.starts_with('.')
                || matches!(
                    component.as_str(),
                    "secrets" | "credentials" | "secrets.json" | "credentials.json"
                )
        })
    {
        return Err(ToolError::Validation(
            "bundle paths must be at most 1024 bytes and exclude hidden files, credentials, secrets, and control characters".into(),
        ));
    }
    Ok(())
}

fn hash_file(path: &std::path::Path) -> Result<String, ToolError> {
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let size = input.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn export_files(
    workspace: &Workspace,
    params: ExportFilesParams,
) -> Result<ExportFilesResult, ToolError> {
    if params.files.is_empty() || params.files.len() > MAX_FILES {
        return Err(ToolError::Validation(
            "select 1–256 project files for export".into(),
        ));
    }
    validate_selection_path(&params.output_path)?;
    if !params.output_path.ends_with(".zip") {
        return Err(ToolError::Validation(
            "bundle output_path must end in .zip".into(),
        ));
    }
    let output_path = super::resolve(workspace, &params.project_id, &params.output_path)?;
    let mut selected = BTreeSet::new();
    // Validate the complete selection before creating temporary snapshots.
    for path in &params.files {
        validate_selection_path(path)?;
        super::resolve(workspace, &params.project_id, path)?;
        if path == &params.output_path || !selected.insert(path) {
            return Err(ToolError::Validation(
                "bundle selection must be unique and must not include its output_path".into(),
            ));
        }
    }
    let mut snapshots = Vec::with_capacity(selected.len());
    let mut remaining = MAX_SOURCE_BYTES;
    for path in selected {
        let source = super::resolve(workspace, &params.project_id, path)?;
        let snapshot = workspace.snapshot_artifact_bounded(&source, remaining.max(1))?;
        remaining = remaining
            .checked_sub(snapshot.meta().size_bytes)
            .ok_or_else(|| {
                ToolError::Validation(
                    "selected project files exceed the 1 GiB export budget".into(),
                )
            })?;
        snapshots.push((path, snapshot));
    }
    // All sources must still identify the versions collected for this bundle.
    // Archive construction subsequently reads only the immutable copies.
    for (_, snapshot) in &snapshots {
        workspace.verify_snapshot_source(snapshot)?;
    }
    let mut manifest = BundleManifest {
        format_version: 1,
        project_id: params.project_id,
        scope: "selected_files".into(),
        dependencies_inspected: false,
        files: Vec::with_capacity(snapshots.len()),
    };
    let temporary = tempfile::tempdir()?;
    let archive_path = temporary.path().join("bundle.zip");
    let mut archive = ZipWriter::new(File::create(&archive_path)?);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);
    for (path, snapshot) in snapshots {
        let entry = BundleFile {
            path: format!("files/{path}"),
            size_bytes: snapshot.meta().size_bytes,
            media_type: snapshot.meta().media_type.into(),
            sha256: hash_file(snapshot.path())?,
        };
        archive
            .start_file(&entry.path, options)
            .map_err(archive_error)?;
        std::io::copy(&mut File::open(snapshot.path())?, &mut archive)?;
        manifest.files.push(entry);
    }
    archive
        .start_file("manifest.json", options)
        .map_err(archive_error)?;
    archive.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    archive.finish().map_err(archive_error)?.sync_all()?;
    let sha256 = hash_file(&archive_path)?;
    let artifact = workspace.commit_generated_artifact_bounded(
        &output_path,
        &archive_path,
        false,
        MAX_ARCHIVE_BYTES,
    )?;
    Ok(ExportFilesResult {
        artifact,
        sha256,
        manifest,
    })
}

fn archive_error(error: zip::result::ZipError) -> ToolError {
    ToolError::Io(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &std::path::Path) -> Workspace {
        let workspace = Workspace::open(Some(root), None).unwrap();
        super::super::create(
            &workspace,
            super::super::CreateParams {
                project_id: "model".into(),
                name: "Example".into(),
                description: String::new(),
                adopt_existing: false,
            },
        )
        .unwrap();
        workspace
    }

    fn request(files: &[&str], output: &str) -> ExportFilesParams {
        ExportFilesParams {
            project_id: "model".into(),
            files: files.iter().map(|path| (*path).into()).collect(),
            output_path: output.into(),
        }
    }

    #[test]
    fn selected_bundle_retains_paths_exact_bytes_and_hashes_without_other_files() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let sources: [(&str, &[u8]); 3] = [
            ("source/model.scad", b"cube([1, 2, 3]);"),
            ("settings/parameters.json", b"{\"units\":\"mm\"}"),
            ("textures/color.png", b"texture fixture"),
        ];
        for (path, bytes) in sources {
            workspace
                .write_artifact(&format!("projects/model/{path}"), bytes, false)
                .unwrap();
        }
        workspace
            .write_artifact("projects/model/unselected.json", b"{}", false)
            .unwrap();
        workspace
            .write_artifact("projects/unrelated/part.stl", b"other project", false)
            .unwrap();
        let selected = sources.map(|(path, _)| path);
        let result = export_files(&workspace, request(&selected, "exports/model.zip")).unwrap();
        assert_eq!(result.artifact.media_type, "application/zip");
        assert_eq!(result.manifest.scope, "selected_files");
        assert!(!result.manifest.dependencies_inspected);
        let schema =
            serde_json::Value::Object(crate::tools::output::schema("project").as_ref().clone());
        let validator = jsonschema::validator_for(&schema).unwrap();
        let mut response = serde_json::to_value(&result).unwrap();
        assert!(validator.is_valid(&response));
        response["manifest"]["files"][0]["sha256"] = serde_json::json!("invalid");
        assert!(!validator.is_valid(&response));
        assert_eq!(
            result.sha256,
            hash_file(&root.path().join(&result.artifact.path)).unwrap()
        );
        let mut zip =
            zip::ZipArchive::new(File::open(root.path().join(result.artifact.path)).unwrap())
                .unwrap();
        assert_eq!(zip.len(), sources.len() + 1);
        for (path, bytes) in sources {
            let mut actual = Vec::new();
            zip.by_name(&format!("files/{path}"))
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
            assert_eq!(actual, bytes);
            let entry = result
                .manifest
                .files
                .iter()
                .find(|entry| entry.path == format!("files/{path}"))
                .unwrap();
            assert_eq!(entry.size_bytes, bytes.len() as u64);
            let expected: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(entry.sha256, expected);
        }
        let stored: BundleManifest =
            serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(stored.project_id, result.manifest.project_id);
        assert_eq!(stored.files.len(), sources.len());
        // A repeated explicit export neither overwrites nor changes the first artifact.
        assert_eq!(
            export_files(&workspace, request(&selected, "exports/model.zip"))
                .unwrap_err()
                .code(),
            "already_exists"
        );
    }

    #[test]
    fn selection_validation_rejects_escape_hidden_sensitive_duplicate_and_self_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        workspace
            .write_artifact("projects/model/model.stl", b"fixture", false)
            .unwrap();
        for files in [
            vec![],
            vec!["../unrelated/model.stl"],
            vec!["/model.stl"],
            vec![".printable/state.json"],
            vec!["settings/credentials.json"],
            vec!["model.stl", "model.stl"],
            vec!["exports/model.zip"],
            vec!["model.stl", "missing.stl"],
            vec!["model.stl\\bad"],
        ] {
            assert!(
                export_files(&workspace, request(&files, "exports/model.zip")).is_err(),
                "{files:?}"
            );
            assert!(
                !root
                    .path()
                    .join("projects/model/exports/model.zip")
                    .exists()
            );
        }
        std::os::unix::fs::symlink("model.stl", root.path().join("projects/model/link.stl"))
            .unwrap();
        assert_eq!(
            export_files(&workspace, request(&["link.stl"], "exports/model.zip"))
                .unwrap_err()
                .code(),
            "symlink_refused"
        );
        assert!(export_files(&workspace, request(&["model.stl"], "../outside.zip")).is_err());
    }
}
