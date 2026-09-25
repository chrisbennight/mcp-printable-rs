//! Retained printer evidence never upgrades an acknowledgement to execution.
use crate::{
    error::ToolError,
    provenance::{self, Kind, RecordRef},
};
use bambuddy_api::jobs::QueueItem;
use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Source {
    pub project_id: String,
    pub source: String,
    pub delivery: Option<RecordRef>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Execution {
    #[serde(rename = "not_observed")]
    Unobserved,
    #[serde(rename = "printing_observed")]
    Printing,
    #[serde(rename = "completion_observed")]
    Completed,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Evidence {
    pub record: Option<RecordRef>,
    pub import_receipt: Option<RecordRef>,
    pub execution: Execution,
    /// No supported backend API verifies the digest of bytes stored or executed.
    pub backend_digest_verification: &'static str,
}

pub(super) fn source(workspace: &Workspace, id: Option<u64>) -> Result<Option<Source>, ToolError> {
    let Some(id) = id else { return Ok(None) };
    match workspace.read_artifact(&format!(".printable/bambuddy-library/{id}.json")) {
        Ok((_, bytes)) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(WsError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) struct Pending {
    source: Source,
    reference: RecordRef,
    action: &'static str,
}

/// Persist the attempt before the external mutation. A disconnected or failed
/// request leaves an unknown outcome that can be inspected without replaying it.
pub(super) fn begin(
    workspace: &Workspace,
    library_id: Option<u64>,
    action: &'static str,
    request: Value,
) -> Result<Option<Pending>, ToolError> {
    let Some(source) = source(workspace, library_id)? else {
        return Ok(None);
    };
    let Some(delivery) = &source.delivery else {
        return Ok(None);
    };
    let imported = provenance::read(workspace, delivery, Kind::ImportReceipt, &source.project_id)?;
    if imported.data["library_file_id"] != json!(library_id) {
        return Err(ToolError::Validation(
            "import receipt does not identify this library file".into(),
        ));
    }
    let reference = provenance::store(
        workspace,
        Kind::Submission,
        &source.project_id,
        json!({"attempt_id":crate::upload::random_hex_id()?,"action":action,"outcome":"unknown",
            "import_receipt":delivery,"request":request,"execution":"not_observed",
            "backend_digest_verification":"unverified"}),
    )?;
    Ok(Some(Pending {
        source,
        reference,
        action,
    }))
}

pub(super) fn accepted(
    workspace: &Workspace,
    pending: Option<Pending>,
    job: &QueueItem,
) -> Result<Evidence, ToolError> {
    let mut evidence = Evidence {
        record: None,
        import_receipt: None,
        execution: Execution::Unobserved,
        backend_digest_verification: "unverified",
    };
    if let Some(pending) = pending {
        let reference = provenance::store(
            workspace,
            Kind::Submission,
            &pending.source.project_id,
            json!({"intent":pending.reference,"action":pending.action,"outcome":"accepted",
                "print_id":job.id,"import_receipt":pending.source.delivery,"queue_acknowledgement":job,
                "execution":"not_observed","backend_digest_verification":"unverified"}),
        )?;
        workspace.write_reserved_artifact(
            &format!(
                ".printable/evidence/print_jobs/{}/{}.json",
                job.id, reference.sha256
            ),
            &serde_json::to_vec(&reference)?,
            false,
        )?;
        evidence.record = Some(reference);
        evidence.import_receipt = pending.source.delivery;
    }
    Ok(evidence)
}

pub(super) fn observe(workspace: &Workspace, job: &QueueItem) -> Result<Evidence, ToolError> {
    let execution = match job.status.as_str() {
        "printing" => Execution::Printing,
        "completed" => Execution::Completed,
        _ => Execution::Unobserved,
    };
    let mut evidence = Evidence {
        record: None,
        import_receipt: None,
        execution,
        backend_digest_verification: "unverified",
    };
    let Some(source) = source(workspace, job.library_file_id)? else {
        return Ok(evidence);
    };
    let Some(delivery) = &source.delivery else {
        return Ok(evidence);
    };
    provenance::read(workspace, delivery, Kind::ImportReceipt, &source.project_id)?;
    let files = match workspace
        .list_artifacts(&format!(".printable/evidence/print_jobs/{}", job.id), 101)
    {
        Ok(files) => files,
        Err(WsError::NotFound(_)) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let complete = files.len() < 101;
    let mut submissions = Vec::new();
    for file in files.into_iter().take(100) {
        let (_, bytes) = workspace.read_artifact(&file.path)?;
        let reference: RecordRef = serde_json::from_slice(&bytes)?;
        let receipt =
            provenance::read(workspace, &reference, Kind::Submission, &source.project_id)?;
        if receipt.data["print_id"] != job.id || receipt.data["import_receipt"] != json!(delivery) {
            return Err(ToolError::Validation(
                "print submission association does not match the observed job".into(),
            ));
        }
        submissions.push(reference);
    }
    evidence.record = Some(provenance::store(
        workspace,
        Kind::Observation,
        &source.project_id,
        json!({"print_id":job.id,"import_receipt":delivery,"submissions":submissions,
            "submission_list_complete":complete,"queue_observation":job,"execution":evidence.execution,
            "backend_digest_verification":"unverified","physical_qualification":"unverified"}),
    )?);
    evidence.import_receipt = source.delivery;
    Ok(evidence)
}
