//! Durable outcome records for prints that Printable sent to the printer backend.
//!
//! Printable records a submission when it stages or starts a print; the backend
//! owns what happens next. The follower polls each submitted print until the
//! backend reports a final state and then writes one record per print under
//! `.printable/prints/<project_id>/<print_id>.json`. A record is written once,
//! so it survives later loss of the backend's own history. A print whose final
//! state cannot be established is recorded with outcome `unknown`, never as a
//! success or failure.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

use bambuddy_api::{ApiError, BambuddyApi, jobs::QueueItem};
use printable_workspace::{Workspace, WsError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::PrinterService;
use crate::{
    config::SettingsError,
    error::ToolError,
    provenance::{self, RecordRef},
};

const LINKS: &str = ".printable/evidence/print_jobs";
const RECORDS: &str = ".printable/prints";
/// Backend states after which a print no longer changes.
const FINAL_STATES: [&str; 4] = ["completed", "failed", "cancelled", "skipped"];
const INTERVAL_KEY: &str = "PRINTABLE_PRINT_FOLLOW_INTERVAL_SECONDS";
const MAX_FOLLOW_KEY: &str = "PRINTABLE_PRINT_FOLLOW_MAX_HOURS";
const DEFAULT_INTERVAL_SECONDS: u64 = 60;
const DEFAULT_MAX_FOLLOW_HOURS: u64 = 168;
/// Submission records are small JSON documents; anything larger is refused.
const MAX_SUBMISSION_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowSettings {
    pub interval: Duration,
    /// How long after its submission a print is followed before its outcome
    /// is recorded as unknown.
    pub max_follow: Duration,
}

/// Read the follower settings. An interval of zero turns following off.
pub(super) fn settings(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<FollowSettings>, SettingsError> {
    let number =
        |key: &'static str, default: u64, range: std::ops::RangeInclusive<u64>| match get(key)
            .filter(|value| !value.trim().is_empty())
        {
            None => Ok(default),
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|value| range.contains(value))
                .ok_or(SettingsError::Printers(key)),
        };
    let interval = number(INTERVAL_KEY, DEFAULT_INTERVAL_SECONDS, 0..=3600)?;
    let hours = number(MAX_FOLLOW_KEY, DEFAULT_MAX_FOLLOW_HOURS, 1..=8760)?;
    Ok((interval > 0).then(|| FollowSettings {
        interval: Duration::from_secs(interval),
        max_follow: Duration::from_secs(hours * 3600),
    }))
}

pub fn record_path(project_id: &str, print_id: u64) -> String {
    format!("{RECORDS}/{project_id}/{print_id}.json")
}

/// A submitted print without a final record.
#[derive(Debug)]
struct Pending {
    print_id: u64,
    project_id: String,
    submissions: Vec<RecordRef>,
    submitted_unix_ms: u64,
    /// The queue item the backend returned when it accepted the submission.
    acknowledged: Option<QueueItem>,
}

fn unix_ms(modified_ns: i64) -> u64 {
    u64::try_from(modified_ns / 1_000_000).unwrap_or(0)
}

/// Read a retained submission directly. Submissions are small, immutable and
/// checked against their digest, so no snapshot is needed.
fn read_submission(workspace: &Workspace, reference: &RecordRef) -> Result<Value, ToolError> {
    let invalid = |message: &str| ToolError::Validation(format!("print submission {message}"));
    if !reference.path.starts_with(".printable/evidence/") {
        return Err(invalid("must be retained evidence"));
    }
    if workspace.stat_artifact(&reference.path)?.size_bytes > MAX_SUBMISSION_BYTES {
        return Err(invalid("is larger than a submission record can be"));
    }
    let (meta, bytes) = workspace.read_generated_bytes(&reference.path)?;
    if meta.path != reference.path || provenance::digest(&bytes) != reference.sha256 {
        return Err(invalid("does not match its digest"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Collect submitted prints that have no final record yet, skipping prints
/// already known to be recorded. Also returns prints found to be recorded.
fn pending(
    workspace: &Workspace,
    finished: &HashSet<u64>,
) -> Result<(Vec<Pending>, Vec<u64>), ToolError> {
    let links = match workspace.list_artifacts(LINKS, usize::MAX) {
        Ok(links) => links,
        Err(WsError::NotFound(_)) => return Ok((Vec::new(), Vec::new())),
        Err(error) => return Err(error.into()),
    };
    let mut grouped: BTreeMap<u64, Vec<(RecordRef, u64)>> = BTreeMap::new();
    for link in links {
        let Some(print_id) = link
            .path
            .strip_prefix(&format!("{LINKS}/"))
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(id, _)| id.parse::<u64>().ok())
        else {
            continue;
        };
        if finished.contains(&print_id) {
            continue;
        }
        let (_, bytes) = workspace.read_generated_bytes(&link.path)?;
        let reference: RecordRef = serde_json::from_slice(&bytes)?;
        grouped
            .entry(print_id)
            .or_default()
            .push((reference, unix_ms(link.modified_ns)));
    }
    let mut pending = Vec::new();
    let mut recorded = Vec::new();
    for (print_id, links) in grouped {
        let submitted_unix_ms = links.iter().map(|(_, ms)| *ms).min().unwrap_or(0);
        let submissions: Vec<RecordRef> = links.into_iter().map(|(link, _)| link).collect();
        let record = read_submission(workspace, &submissions[0])?;
        let Some(project_id) = record["project_id"].as_str().map(str::to_owned) else {
            continue;
        };
        match workspace.stat_artifact(&record_path(&project_id, print_id)) {
            Ok(_) => {
                recorded.push(print_id);
                continue;
            }
            Err(WsError::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        let acknowledged =
            serde_json::from_value(record["data"]["queue_acknowledgement"].clone()).ok();
        pending.push(Pending {
            print_id,
            project_id,
            submissions,
            submitted_unix_ms,
            acknowledged,
        });
    }
    Ok((pending, recorded))
}

fn record(
    pending: &Pending,
    item: Option<&QueueItem>,
    outcome: &str,
    reason: Option<&str>,
    printers: &BTreeMap<u64, (String, Option<String>)>,
    now_unix_ms: u64,
) -> Value {
    // Each setup detail comes from the latest observation, else from the queue
    // item the backend returned on acceptance; status and times are only ever
    // observed values.
    let ack = pending.acknowledged.as_ref();
    let printer_id = item
        .and_then(|item| item.printer_id)
        .or_else(|| ack.and_then(|item| item.printer_id));
    let known = printer_id.and_then(|id| printers.get(&id));
    json!({
        "format_version": 1,
        "print_id": pending.print_id,
        "project_id": pending.project_id,
        "outcome": outcome,
        "reason": reason,
        "backend_status": item.map(|item| item.status.as_str()),
        "printer": {
            "id": printer_id,
            "name": item.and_then(|item| item.printer_name.clone()).or_else(|| ack.and_then(|item| item.printer_name.clone())).or_else(|| known.map(|(name, _)| name.clone())),
            "model": known.and_then(|(_, model)| model.clone()),
        },
        "plate_id": item.and_then(|item| item.plate_id).or_else(|| ack.and_then(|item| item.plate_id)),
        "ams_mapping": item.and_then(|item| item.ams_mapping.clone()).or_else(|| ack.and_then(|item| item.ams_mapping.clone())),
        "filament_type": item.and_then(|item| item.filament_type.clone()).or_else(|| ack.and_then(|item| item.filament_type.clone())),
        "filament_color": item.and_then(|item| item.filament_color.clone()).or_else(|| ack.and_then(|item| item.filament_color.clone())),
        "started_at": item.and_then(|item| item.started_at.clone()),
        "completed_at": item.and_then(|item| item.completed_at.clone()),
        "error_message": item.and_then(|item| item.error_message.clone()),
        "library_file_id": item.and_then(|item| item.library_file_id).or_else(|| ack.and_then(|item| item.library_file_id)),
        "submitted_unix_ms": pending.submitted_unix_ms,
        "recorded_unix_ms": now_unix_ms,
        "submissions": pending.submissions,
    })
}

fn write(workspace: &Workspace, pending: &Pending, value: &Value) -> Result<(), ToolError> {
    match workspace.write_reserved_artifact(
        &record_path(&pending.project_id, pending.print_id),
        &serde_json::to_vec(value)?,
        false,
    ) {
        Ok(_) | Err(WsError::AlreadyExists(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// What one pass over the submitted prints did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    pub followed: usize,
    pub recorded: usize,
    /// Prints whose backend read failed transiently; they are retried later.
    pub deferred: usize,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ToolError::Validation("print follower task did not complete".into()))?
}

/// Read every unfinished print once and record those that have finished.
pub async fn pass(
    service: &PrinterService,
    workspace: &Arc<Workspace>,
    settings: FollowSettings,
    now_unix_ms: u64,
    finished: &mut HashSet<u64>,
) -> Result<PassReport, ToolError> {
    let scan = Arc::clone(workspace);
    let known = finished.clone();
    let (pending, recorded) = blocking(move || pending(&scan, &known)).await?;
    finished.extend(recorded);
    let mut report = PassReport {
        followed: pending.len(),
        ..PassReport::default()
    };
    if pending.is_empty() {
        return Ok(report);
    }
    // Printer models are optional detail: a failed list leaves them out.
    let printers: BTreeMap<u64, (String, Option<String>)> = service
        .read
        .printers()
        .await
        .map(|printers| {
            printers
                .into_iter()
                .map(|printer| (printer.id, (printer.name, printer.model)))
                .collect()
        })
        .unwrap_or_default();
    let max_follow_ms = u64::try_from(settings.max_follow.as_millis()).unwrap_or(u64::MAX);
    for print in pending {
        let expired = now_unix_ms.saturating_sub(print.submitted_unix_ms) > max_follow_ms;
        let value = match service.read.print_job(print.print_id).await {
            Ok(item) if FINAL_STATES.contains(&item.status.as_str()) => {
                let outcome = item.status.clone();
                record(&print, Some(&item), &outcome, None, &printers, now_unix_ms)
            }
            Ok(item) if expired => record(
                &print,
                Some(&item),
                "unknown",
                Some("the print did not reach a final state within the follow time"),
                &printers,
                now_unix_ms,
            ),
            Ok(_) => continue,
            Err(ApiError::NotFound) => record(
                &print,
                None,
                "unknown",
                Some("the printer backend no longer has this print"),
                &printers,
                now_unix_ms,
            ),
            Err(_) if expired => record(
                &print,
                None,
                "unknown",
                Some("the printer backend could not be read before the follow time ended"),
                &printers,
                now_unix_ms,
            ),
            Err(error) => {
                tracing::warn!(
                    print_id = print.print_id,
                    error = %error,
                    "could not read print state; retrying on the next pass"
                );
                report.deferred += 1;
                continue;
            }
        };
        let target = Arc::clone(workspace);
        let print_id = print.print_id;
        blocking(move || write(&target, &print, &value)).await?;
        finished.insert(print_id);
        report.recorded += 1;
    }
    Ok(report)
}

/// Follow submitted prints until the server shuts down.
pub fn spawn(service: Arc<PrinterService>, workspace: Arc<Workspace>, cancel: CancellationToken) {
    let Some(settings) = service.follow else {
        return;
    };
    tokio::spawn(async move {
        let mut finished = HashSet::new();
        let mut interval = tokio::time::interval(settings.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            match pass(
                &service,
                &workspace,
                settings,
                crate::printers::observations::now_ms(),
                &mut finished,
            )
            .await
            {
                Ok(report) if report.recorded > 0 => {
                    tracing::info!(recorded = report.recorded, "recorded print outcomes");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(code = error.code(), "print follower pass failed");
                }
            }
        }
    });
}

/// The recorded outcomes of one project's prints, ordered by print id.
pub fn project_prints(workspace: &Workspace, project_id: &str) -> Result<Vec<Value>, ToolError> {
    crate::projects::get(workspace, project_id)?;
    let files = match workspace.list_artifacts(&format!("{RECORDS}/{project_id}"), usize::MAX) {
        Ok(files) => files,
        Err(WsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut prints = files
        .iter()
        .map(|file| {
            let (_, bytes) = workspace.read_generated_bytes(&file.path)?;
            Ok(serde_json::from_slice::<Value>(&bytes)?)
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    prints.sort_by_key(|print| print["print_id"].as_u64());
    Ok(prints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::Kind;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const HOUR_MS: u64 = 3_600_000;

    fn service(server: &MockServer) -> Arc<PrinterService> {
        super::super::configure(&|key| match key {
            "PRINTABLE_BAMBUDDY_URL" => Some(server.uri()),
            "PRINTABLE_BAMBUDDY_READ_KEY" | "PRINTABLE_BAMBUDDY_CONTROL_KEY" => {
                Some("fixture".into())
            }
            _ => None,
        })
        .unwrap()
        .unwrap()
    }

    fn settings() -> FollowSettings {
        FollowSettings {
            interval: Duration::from_secs(60),
            max_follow: Duration::from_secs(48 * 3600),
        }
    }

    fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        crate::projects::dispatch(
            &workspace,
            crate::projects::ProjectRequest::Create(crate::projects::CreateParams {
                project_id: "desk".into(),
                name: "Desk".into(),
                description: String::new(),
                adopt_existing: false,
            }),
        )
        .unwrap();
        (root, Arc::new(workspace))
    }

    /// Store an accepted submission and its link, as staging a print does.
    fn submit(workspace: &Workspace, print_id: u64) -> RecordRef {
        let reference = provenance::store(
            workspace,
            Kind::Submission,
            "desk",
            json!({"action":"stage","outcome":"accepted","print_id":print_id,
                "queue_acknowledgement":{"id":print_id,"printer_id":1,"manual_start":true,
                    "status":"pending","plate_id":3,"ams_mapping":[1]}}),
        )
        .unwrap();
        workspace
            .write_reserved_artifact(
                &format!("{LINKS}/{print_id}/{}.json", reference.sha256),
                &serde_json::to_vec(&reference).unwrap(),
                false,
            )
            .unwrap();
        reference
    }

    async fn job(server: &MockServer, id: u64, status: u16, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/queue/{id}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn printers(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/printers/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id":1,"name":"Workshop X1C","model":"X1 Carbon","is_active":true}
            ])))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn finished_prints_are_recorded_once_with_printer_and_plate_details() {
        let backend = MockServer::start().await;
        printers(&backend).await;
        job(&backend, 7, 200, json!({"id":7,"printer_id":1,"manual_start":false,"status":"completed","plate_id":2,
            "ams_mapping":[0,-1],"filament_type":"PLA","filament_color":"#FF8800",
            "started_at":"2026-11-01T10:00:00Z","completed_at":"2026-11-01T12:30:00Z","library_file_id":91})).await;
        job(
            &backend,
            8,
            200,
            json!({"id":8,"printer_id":1,"manual_start":false,"status":"failed","error_message":"Spaghetti detected"}),
        )
        .await;
        let (_root, workspace) = workspace();
        let submission = submit(&workspace, 7);
        submit(&workspace, 8);
        let service = service(&backend);
        let now = crate::printers::observations::now_ms();
        let report = pass(&service, &workspace, settings(), now, &mut HashSet::new())
            .await
            .unwrap();
        assert_eq!(
            report,
            PassReport {
                followed: 2,
                recorded: 2,
                deferred: 0
            }
        );
        let prints = project_prints(&workspace, "desk").unwrap();
        assert_eq!(prints.len(), 2);
        assert_eq!(prints[0]["outcome"], "completed");
        assert_eq!(
            prints[0]["printer"],
            json!({"id":1,"name":"Workshop X1C","model":"X1 Carbon"})
        );
        assert_eq!(prints[0]["plate_id"], 2);
        assert_eq!(prints[0]["ams_mapping"], json!([0, -1]));
        assert_eq!(prints[0]["submissions"], json!([submission]));
        assert_eq!(prints[1]["outcome"], "failed");
        assert_eq!(prints[1]["error_message"], "Spaghetti detected");
        let again = pass(&service, &workspace, settings(), now, &mut HashSet::new())
            .await
            .unwrap();
        assert_eq!(again, PassReport::default());
    }

    #[tokio::test]
    async fn unfinished_prints_wait_and_become_unknown_only_after_the_follow_time() {
        let backend = MockServer::start().await;
        printers(&backend).await;
        job(
            &backend,
            9,
            200,
            json!({"id":9,"printer_id":1,"manual_start":false,"status":"printing"}),
        )
        .await;
        let (_root, workspace) = workspace();
        submit(&workspace, 9);
        let service = service(&backend);
        let now = crate::printers::observations::now_ms();
        let waiting = pass(&service, &workspace, settings(), now, &mut HashSet::new())
            .await
            .unwrap();
        assert_eq!((waiting.followed, waiting.recorded), (1, 0));
        assert!(project_prints(&workspace, "desk").unwrap().is_empty());
        let later = now + 49 * HOUR_MS;
        let expired = pass(&service, &workspace, settings(), later, &mut HashSet::new())
            .await
            .unwrap();
        assert_eq!(expired.recorded, 1);
        let print = &project_prints(&workspace, "desk").unwrap()[0];
        assert_eq!(print["outcome"], "unknown");
        assert_eq!(print["backend_status"], "printing");
    }

    #[tokio::test]
    async fn missing_prints_are_unknown_and_unreachable_backends_are_retried() {
        let backend = MockServer::start().await;
        printers(&backend).await;
        job(&backend, 10, 404, json!({"detail":"Not found"})).await;
        job(&backend, 11, 503, json!({"detail":"Unavailable"})).await;
        let (_root, workspace) = workspace();
        submit(&workspace, 10);
        submit(&workspace, 11);
        let service = service(&backend);
        let now = crate::printers::observations::now_ms();
        let report = pass(&service, &workspace, settings(), now, &mut HashSet::new())
            .await
            .unwrap();
        assert_eq!(
            report,
            PassReport {
                followed: 2,
                recorded: 1,
                deferred: 1
            }
        );
        let prints = project_prints(&workspace, "desk").unwrap();
        assert_eq!(prints.len(), 1);
        assert_eq!(
            (
                prints[0]["print_id"].as_u64(),
                prints[0]["outcome"].as_str()
            ),
            (Some(10), Some("unknown"))
        );
        assert_eq!(prints[0]["backend_status"], Value::Null);
        assert_eq!(prints[0]["plate_id"], 3);
        assert_eq!(prints[0]["ams_mapping"], json!([1]));
        assert_eq!(prints[0]["printer"]["name"], "Workshop X1C");
        let expired = now + 49 * HOUR_MS;
        let report = pass(
            &service,
            &workspace,
            settings(),
            expired,
            &mut HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            PassReport {
                followed: 1,
                recorded: 1,
                deferred: 0
            }
        );
        let prints = project_prints(&workspace, "desk").unwrap();
        assert_eq!(
            (
                prints[1]["print_id"].as_u64(),
                prints[1]["outcome"].as_str()
            ),
            (Some(11), Some("unknown"))
        );
    }

    #[test]
    fn settings_default_disable_and_reject_out_of_range_values() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(
            settings_from(&lookup(&[])).unwrap(),
            Some(FollowSettings {
                interval: Duration::from_secs(60),
                max_follow: Duration::from_secs(168 * 3600)
            })
        );
        assert_eq!(
            settings_from(&lookup(&[(INTERVAL_KEY, "0")])).unwrap(),
            None
        );
        for (key, value) in [
            (INTERVAL_KEY, "3601"),
            (INTERVAL_KEY, "soon"),
            (MAX_FOLLOW_KEY, "0"),
        ] {
            let pairs: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(key, value)].into_boxed_slice());
            assert!(settings_from(&lookup(pairs)).is_err());
        }
    }

    fn settings_from(
        get: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<FollowSettings>, SettingsError> {
        super::settings(get)
    }
}
