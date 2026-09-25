//! Immutable links between design evidence, slices, reviews and printer records.
use crate::error::ToolError;
use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    /// Retained metadata path, not a mutable public report filename.
    pub path: String,
    #[schemars(regex(pattern = "^[a-f0-9]{64}$"))]
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Slice,
    ToolpathReview,
    ImportIntent,
    ImportReceipt,
    Submission,
    Observation,
}

impl Kind {
    fn directory(self) -> &'static str {
        match self {
            Self::Slice => "slice",
            Self::ToolpathReview => "toolpath_review",
            Self::ImportIntent => "import_intent",
            Self::ImportReceipt => "import_receipt",
            Self::Submission => "submission",
            Self::Observation => "observation",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    format_version: u32,
    kind: Kind,
    pub project_id: String,
    pub data: Value,
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}

/// Content-addressed records deduplicate repeated observations without replacing
/// prior evidence. No timestamp is added implicitly: callers choose what was
/// actually observed and which external attempt the record describes.
pub fn store(
    workspace: &Workspace,
    kind: Kind,
    project_id: &str,
    data: Value,
) -> Result<RecordRef, ToolError> {
    let bytes = serde_json::to_vec(&Record {
        format_version: 1,
        kind,
        project_id: project_id.into(),
        data,
    })?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("provenance metadata exceeds its size limit"));
    }
    let sha256 = digest(&bytes);
    let reference = RecordRef {
        path: format!(".printable/evidence/{}/{sha256}.json", kind.directory()),
        sha256,
    };
    match workspace.write_reserved_artifact(&reference.path, &bytes, false) {
        Ok(_) => {}
        Err(WsError::AlreadyExists(_)) => {
            read_value(workspace, &reference)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(reference)
}

/// Existing CAD measurement references use their own record format; their
/// digest is verified before a caller interprets the available measurements.
pub fn read_value(workspace: &Workspace, reference: &RecordRef) -> Result<Value, ToolError> {
    if reference.sha256.len() != 64
        || !reference
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("provenance requires a lowercase SHA-256 digest"));
    }
    let snapshot = workspace.snapshot_artifact_bounded(&reference.path, MAX_RECORD_BYTES)?;
    if snapshot.meta().path != reference.path || !snapshot.meta().path.starts_with(".printable/") {
        return Err(invalid(
            "provenance must reference canonical retained metadata",
        ));
    }
    let bytes = std::fs::read(snapshot.path())?;
    if digest(&bytes) != reference.sha256 {
        return Err(invalid(
            "retained provenance bytes do not match their digest",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn read(
    workspace: &Workspace,
    reference: &RecordRef,
    kind: Kind,
    project_id: &str,
) -> Result<Record, ToolError> {
    let record: Record = serde_json::from_value(read_value(workspace, reference)?)?;
    if record.format_version != 1 || record.kind != kind || record.project_id != project_id {
        return Err(invalid(
            "provenance record kind, version, or project does not match this operation",
        ));
    }
    Ok(record)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportEvidence {
    pub slice: Option<RecordRef>,
    pub toolpath_review: Option<RecordRef>,
}

pub fn design_evidence(
    workspace: &Workspace,
    reference: Option<&RecordRef>,
    source: &str,
    sha256: &str,
) -> Result<Value, ToolError> {
    let Some(reference) = reference else {
        return Ok(json!({"status":"unverified"}));
    };
    let measurement = read_value(workspace, reference)?;
    let matched = measurement["artifacts"]
        .as_array()
        .is_some_and(|artifacts| {
            artifacts.iter().any(|artifact| {
                artifact["sha256"] == sha256 && artifact["artifact"]["path"] == source
            })
        });
    if !matched {
        return Err(invalid(
            "CAD measurement does not identify the selected source bytes",
        ));
    }
    Ok(
        json!({"status":"matched_cad_artifact","record":reference,"revision":measurement["revision"],
        "qualification":measurement.get("qualification").cloned().unwrap_or(json!({"status":"unverified"})),
        "requirements":measurement.get("requirements").cloned().unwrap_or(json!({"status":"unverified"})),
        "engine":{"name":"CadQuery","version":measurement["report"]["cadquery_version"]}}),
    )
}

/// Compare the upload snapshot with a completed slice and its selected review.
/// Human approval, backend byte verification and physical fitness are separate
/// evidence; a generated toolpath image cannot establish any of them.
pub fn verify_import(
    workspace: &Workspace,
    project_id: &str,
    sha256: &str,
    size: u64,
    evidence: &ImportEvidence,
) -> Result<Value, ToolError> {
    let Some(slice_ref) = &evidence.slice else {
        if evidence.toolpath_review.is_some() {
            return Err(invalid(
                "toolpath review requires its retained slice reference",
            ));
        }
        return Ok(
            json!({"slice":"unverified","toolpath_review":"unverified","design":"unverified"}),
        );
    };
    let slice = read(workspace, slice_ref, Kind::Slice, project_id)?;
    let artifacts = slice.data["artifacts"]
        .as_object()
        .ok_or_else(|| invalid("retained slice has no artifact identities"))?;
    if !artifacts.iter().any(|(name, artifact)| {
        name.ends_with(".gcode.3mf")
            && artifact["sha256"] == sha256
            && artifact["artifact"]["size_bytes"] == size
    }) {
        return Err(invalid(
            "upload bytes do not match a printer artifact in the retained slice",
        ));
    }
    if let Some(review_ref) = &evidence.toolpath_review {
        let review = read(workspace, review_ref, Kind::ToolpathReview, project_id)?;
        if review.data["slice_provenance"] != serde_json::to_value(slice_ref)? {
            return Err(invalid(
                "toolpath review belongs to different source or slice settings",
            ));
        }
        if !artifacts.values().any(|artifact| {
            artifact["sha256"] == review.data["source_sha256"]
                && artifact["artifact"]["path"] == review.data["toolpath"]
        }) {
            return Err(invalid(
                "reviewed toolpath is not an artifact of this slice",
            ));
        }
    }
    Ok(
        json!({"slice":"matched_local_bytes","toolpath_review":if evidence.toolpath_review.is_some(){"matched_slice"}else{"unverified"},"design":slice.data["design_evidence"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cad_measurement_links_only_the_identified_output_and_preserves_scope() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        let measurement = json!({"revision":{"id":"retained_revision","sha256":digest(b"revision")},
            "qualification":{"scope":"cad_delivery","status":"incomplete"},
            "requirements":{"scope":"declared_requirements","status":"incomplete"},
            "report":{"cadquery_version":"fixture"},
            "artifacts":[{"artifact":{"path":"projects/p/build/model.stl"},"sha256":digest(b"model")} ]});
        let bytes = serde_json::to_vec(&measurement).unwrap();
        let reference = RecordRef {
            path: ".printable/revisions/p/measurements/fixture.json".into(),
            sha256: digest(&bytes),
        };
        ws.write_reserved_artifact(&reference.path, &bytes, false)
            .unwrap();
        let evidence = design_evidence(
            &ws,
            Some(&reference),
            "projects/p/build/model.stl",
            &digest(b"model"),
        )
        .unwrap();
        assert_eq!(evidence["revision"], measurement["revision"]);
        assert_eq!(evidence["qualification"]["status"], "incomplete");
        assert_eq!(evidence["engine"]["version"], "fixture");
        assert!(
            design_evidence(
                &ws,
                Some(&reference),
                "projects/p/build/model.stl",
                &digest(b"changed")
            )
            .is_err()
        );
        assert!(
            design_evidence(
                &ws,
                Some(&reference),
                "projects/other/build/model.stl",
                &digest(b"model")
            )
            .is_err()
        );
    }

    #[test]
    fn upload_identity_and_review_applicability_survive_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        let original = b"printer bytes";
        let artifacts = json!({"model.gcode.3mf":{"sha256":digest(original),"artifact":{"path":"projects/p/slice/model.gcode.3mf","size_bytes":original.len()}},"plate_1.gcode":{"sha256":digest(b"gcode"),"artifact":{"path":"projects/p/slice/plate_1.gcode","size_bytes":5}}});
        let slice = store(&ws, Kind::Slice, "p", json!({"artifacts":artifacts,"source_sha256":digest(b"design"),"settings":{"layer_height":0.2}})).unwrap();
        let review = store(&ws, Kind::ToolpathReview, "p", json!({"slice_provenance":slice,"source_sha256":digest(b"gcode"),"toolpath":"projects/p/slice/plate_1.gcode"})).unwrap();
        let mut evidence = ImportEvidence {
            slice: Some(slice.clone()),
            toolpath_review: Some(review),
        };
        assert!(
            verify_import(
                &ws,
                "p",
                &digest(original),
                original.len() as u64,
                &evidence
            )
            .is_ok()
        );
        assert!(verify_import(&ws, "p", &digest(b"replacement"), 11, &evidence).is_err());
        for (source, height) in [(b"design".as_slice(), 0.3), (b"new design".as_slice(), 0.2)] {
            evidence.slice = Some(store(&ws, Kind::Slice, "p", json!({"artifacts":artifacts,"source_sha256":digest(source),"settings":{"layer_height":height}})).unwrap());
            assert!(
                verify_import(
                    &ws,
                    "p",
                    &digest(original),
                    original.len() as u64,
                    &evidence
                )
                .is_err()
            );
        }
        assert!(read(&ws, &slice, Kind::Slice, "another_project").is_err());
        ws.write_reserved_artifact(&slice.path, b"changed", true)
            .unwrap();
        assert!(read(&ws, &slice, Kind::Slice, "p").is_err());
    }

    #[test]
    fn repeated_observations_reuse_immutable_records_and_legacy_links_stay_unknown() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path()), None).unwrap();
        let a = store(
            &ws,
            Kind::Observation,
            "p",
            json!({"backend_status":"pending"}),
        )
        .unwrap();
        let b = store(
            &ws,
            Kind::Observation,
            "p",
            json!({"backend_status":"pending"}),
        )
        .unwrap();
        assert_eq!(a, b);
        let legacy =
            verify_import(&ws, "p", &digest(b"file"), 4, &ImportEvidence::default()).unwrap();
        assert_eq!(legacy["slice"], "unverified");
    }
}
