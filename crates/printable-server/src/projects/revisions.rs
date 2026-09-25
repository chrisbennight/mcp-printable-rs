//! Immutable design snapshots with a guarded project revision head.

use super::{requirements::Manifest, resolve};
use crate::{error::ToolError, upload::random_hex_id};
use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_INPUT: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub id: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseParams {
    pub project_id: String,
    /// Null only when creating this project's first design revision.
    pub expected_parent: Option<Identity>,
    pub source: String,
    /// Digest of the source bytes the caller intends to revise.
    pub expected_source_sha256: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub manifest: Manifest,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    pub project_id: String,
    /// Omit to read the current revision head.
    pub revision: Option<Identity>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub path: String,
    pub snapshot: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub format_version: u32,
    pub project_id: String,
    pub id: String,
    pub parent: Option<Identity>,
    pub source: String,
    pub files: BTreeMap<String, Source>,
    pub manifest: Manifest,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Response {
    pub identity: Identity,
    pub revision: Revision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurements_directory: Option<String>,
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn validate_identity(identity: &Identity) -> Result<(), ToolError> {
    if identity.id.len() != 32
        || !identity.id.bytes().all(|c| c.is_ascii_hexdigit())
        || !is_digest(&identity.sha256)
    {
        return Err(invalid(
            "revision identity requires a retained ID and its SHA-256 digest",
        ));
    }
    Ok(())
}
fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}
fn directory(project: &str) -> String {
    format!(".printable/revisions/{project}")
}

fn head(workspace: &Workspace, project: &str) -> Result<Option<Identity>, ToolError> {
    match workspace.read_artifact(&format!("{}/head.json", directory(project))) {
        Ok((_, bytes)) => {
            let value: Identity = serde_json::from_slice(&bytes)?;
            validate_identity(&value)?;
            Ok(Some(value))
        }
        Err(WsError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn get(
    workspace: &Workspace,
    project: &str,
    identity: &Identity,
) -> Result<Revision, ToolError> {
    super::get(workspace, project)?;
    validate_identity(identity)?;
    let (_, bytes) = workspace.read_artifact(&format!(
        "{}/{}/revision.json",
        directory(project),
        identity.id
    ))?;
    if digest(&bytes) != identity.sha256 {
        return Err(invalid(
            "retained revision digest does not match its identity",
        ));
    }
    let revision: Revision = serde_json::from_slice(&bytes)?;
    if revision.format_version != 1 || revision.project_id != project || revision.id != identity.id
    {
        return Err(invalid(
            "retained revision has an unsupported version or inconsistent identity",
        ));
    }
    revision.manifest.validate()?;
    if !revision.files.contains_key(&revision.source) || revision.files.len() > 129 {
        return Err(invalid("retained revision has inconsistent source files"));
    }
    for (name, file) in &revision.files {
        if file.path != resolve(workspace, project, name)?
            || file.snapshot != format!("{}/{}/inputs/{name}", directory(project), identity.id)
            || !is_digest(&file.sha256)
            || file.size_bytes > MAX_INPUT
        {
            return Err(invalid(
                "retained revision has inconsistent source identity",
            ));
        }
    }
    Ok(revision)
}

pub fn read(workspace: &Workspace, params: GetParams) -> Result<Value, ToolError> {
    super::get(workspace, &params.project_id)?;
    let identity = match params.revision {
        Some(identity) => Some(identity),
        None => head(workspace, &params.project_id)?,
    }
    .ok_or_else(|| {
        invalid("project has no design revision; existing project files remain available")
    })?;
    let revision = get(workspace, &params.project_id, &identity)?;
    let measurements_directory = Some(format!(
        "{}/{}/measurements",
        directory(&params.project_id),
        identity.id
    ));
    Ok(serde_json::to_value(Response {
        identity,
        revision,
        measurements_directory,
    })?)
}

pub fn revise(workspace: &Workspace, params: ReviseParams) -> Result<Value, ToolError> {
    super::get(workspace, &params.project_id)?;
    params.manifest.validate()?;
    if params.inputs.len() > 128 || !is_digest(&params.expected_source_sha256) {
        return Err(invalid(
            "revision requires a source SHA-256 digest and at most 128 input files",
        ));
    }
    if let Some(parent) = &params.expected_parent {
        get(workspace, &params.project_id, parent)?;
    }
    let mut names = params.inputs;
    names.push(params.source.clone());
    names.sort();
    names.dedup();
    // Complete source reads before the cross-process update lock. Copying the
    // immutable inputs is part of the guarded commit; contenders return busy.
    let mut snapshots = BTreeMap::new();
    let mut total = 0u64;
    for name in names {
        let path = resolve(workspace, &params.project_id, &name)?;
        let snapshot = workspace.snapshot_artifact_bounded(&path, MAX_INPUT)?;
        total += snapshot.meta().size_bytes;
        if total > MAX_INPUT {
            return Err(invalid("revision input set exceeds 1 GiB"));
        }
        let sha256 = hash_file(snapshot.path())?;
        if name == params.source && sha256 != params.expected_source_sha256 {
            return Err(invalid(
                "source changed; inspect its current digest before revising",
            ));
        }
        snapshots.insert(name, (snapshot, sha256));
    }
    let id = random_hex_id()?;
    let root = format!("{}/{}", directory(&params.project_id), id);
    let _lock = workspace
        .try_lock_reserved(&format!("{}/update.lock", directory(&params.project_id)))
        .map_err(|e| match e {
            WsError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock => invalid(
                "project revision update is busy; inspect the current revision before retrying",
            ),
            other => other.into(),
        })?;
    if head(workspace, &params.project_id)? != params.expected_parent {
        return Err(invalid(
            "project revision changed; reopen the current revision before editing parameters",
        ));
    }
    for (snapshot, _) in snapshots.values() {
        workspace.verify_snapshot_source(snapshot)?;
    }
    let mut files = BTreeMap::new();
    for (name, (snapshot, sha256)) in snapshots {
        let retained = format!("{root}/inputs/{name}");
        workspace.commit_reserved_generated_artifact_bounded(
            &retained,
            snapshot.path(),
            false,
            MAX_INPUT,
        )?;
        files.insert(
            name,
            Source {
                path: snapshot.meta().path.clone(),
                snapshot: retained,
                sha256,
                size_bytes: snapshot.meta().size_bytes,
            },
        );
    }
    let revision = Revision {
        format_version: 1,
        project_id: params.project_id.clone(),
        id: id.clone(),
        parent: params.expected_parent,
        source: params.source,
        files,
        manifest: params.manifest,
    };
    let bytes = serde_json::to_vec(&revision)?;
    let identity = Identity {
        id,
        sha256: digest(&bytes),
    };
    workspace.write_reserved_artifact(&format!("{root}/revision.json"), &bytes, false)?;
    workspace.write_reserved_artifact(
        &format!("{}/head.json", directory(&params.project_id)),
        &serde_json::to_vec(&identity)?,
        true,
    )?;
    Ok(serde_json::to_value(Response {
        identity,
        revision,
        measurements_directory: None,
    })?)
}

pub fn hash_file(path: &std::path::Path) -> Result<String, ToolError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn setup() -> (tempfile::TempDir, Arc<Workspace>, Value) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        super::super::dispatch(
            &workspace,
            serde_json::from_value(
                json!({"action":"create","params":{"project_id":"housing","name":"Housing"}}),
            )
            .unwrap(),
        )
        .unwrap();
        let source = b"result = cq.Workplane().box(parameters['width'], 30, 5)";
        workspace
            .write_artifact("projects/housing/part.py", source, false)
            .unwrap();
        let params = json!({"project_id":"housing","expected_parent":null,"source":"part.py","expected_source_sha256":digest(source),"manifest":{
            "format_version":1,"units":"mm","parameters":{"width":{"value":40,"unit":"mm","minimum":30,"maximum":80,"description":"Enclosure width"}},
            "requirements":{"envelope":{"kind":"build_envelope","size_mm":[80,40,20]}},"assumptions":["FDM prototype; fit needs a physical sample"]}});
        (root, workspace, params)
    }

    #[test]
    fn revisions_survive_reopen_and_keep_original_bytes_after_path_replacement() {
        let (root, workspace, params) = setup();
        let result = revise(&workspace, serde_json::from_value(params.clone()).unwrap()).unwrap();
        let identity: Identity = serde_json::from_value(result["identity"].clone()).unwrap();
        workspace
            .write_artifact("projects/housing/part.py", b"changed", true)
            .unwrap();
        let reopened = Workspace::open(Some(root.path()), None).unwrap();
        let revision = get(&reopened, "housing", &identity).unwrap();
        let file = &revision.files["part.py"];
        assert_eq!(
            digest(&reopened.read_artifact(&file.snapshot).unwrap().1),
            file.sha256
        );
        assert_ne!(digest(b"changed"), file.sha256);
        let mut stale = params;
        stale["expected_parent"] = result["identity"].clone();
        assert!(revise(&reopened, serde_json::from_value(stale).unwrap()).is_err());
        assert_eq!(head(&reopened, "housing").unwrap(), Some(identity));
    }

    #[test]
    fn concurrent_parameter_edits_have_exactly_one_authoritative_successor() {
        let (_root, workspace, params) = setup();
        let first = revise(&workspace, serde_json::from_value(params.clone()).unwrap()).unwrap();
        let barrier = Arc::new(Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|i| {
                let workspace = Arc::clone(&workspace);
                let barrier = Arc::clone(&barrier);
                let mut p = params.clone();
                p["expected_parent"] = first["identity"].clone();
                p["manifest"]["parameters"]["width"]["value"] = json!(50 + i);
                std::thread::spawn(move || {
                    barrier.wait();
                    revise(&workspace, serde_json::from_value(p).unwrap())
                })
            })
            .collect();
        let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        let successor = read(
            &workspace,
            GetParams {
                project_id: "housing".into(),
                revision: None,
            },
        )
        .unwrap();
        assert_eq!(successor["revision"]["parent"], first["identity"]);
        assert_eq!(
            workspace
                .read_artifact("projects/housing/part.py")
                .unwrap()
                .1,
            b"result = cq.Workplane().box(parameters['width'], 30, 5)"
        );
    }

    #[test]
    fn invalid_manifest_or_source_identity_does_not_advance_revision_head() {
        let (_root, workspace, params) = setup();
        for (pointer, value) in [
            ("/manifest/parameters/width/value", json!(100)),
            ("/expected_source_sha256", json!("0".repeat(64))),
            ("/manifest/format_version", json!(2)),
        ] {
            let mut p = params.clone();
            *p.pointer_mut(pointer).unwrap() = value;
            assert!(revise(&workspace, serde_json::from_value(p).unwrap()).is_err());
            assert_eq!(head(&workspace, "housing").unwrap(), None);
        }
    }

    #[test]
    fn successful_revision_operations_match_full_and_selected_response_contracts() {
        let (_root, workspace, params) = setup();
        let revised = revise(&workspace, serde_json::from_value(params).unwrap()).unwrap();
        let reopened = read(
            &workspace,
            GetParams {
                project_id: "housing".into(),
                revision: None,
            },
        )
        .unwrap();
        let full = serde_json::to_value(crate::tools::output::schema("project")).unwrap();
        let full = jsonschema::validator_for(&full).unwrap();
        for (action, value) in [("revise", revised), ("revision", reopened)] {
            full.validate(&value).unwrap();
            let contract = crate::resources::contracts::read(&format!(
                "printable://contracts/project/{action}"
            ))
            .unwrap();
            let selected = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
            selected.validate(&value).unwrap();
            let mut invalid = value;
            invalid["revision"]["manifest"]["parameters"]["width"]["unit"] = json!("unknown_unit");
            assert!(!full.is_valid(&invalid));
            assert!(!selected.is_valid(&invalid));
        }
    }
}
