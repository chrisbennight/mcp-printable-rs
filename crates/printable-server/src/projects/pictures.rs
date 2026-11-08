//! Pictures generated for projects that have no cover.
//!
//! A project's generated pictures live in `.printable/pictures/<project_id>/`:
//! each picture as a PNG named by its SHA-256 digest, and `pictures.json`,
//! which records what the pictures show. They are kept apart from the project
//! record and the project directory, so making them changes neither the
//! record's `record_version` nor the project's files.
//!
//! The plate picture is OrcaSlicer's view of the project's model. It is
//! copied from the newest sliced package or from a 3MF project file when one
//! carries a plate thumbnail, and otherwise exported by the slicer worker
//! from the newest model without slicing it.

use printable_workspace::{ArtifactMeta, Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::cover;
use crate::{
    error::ToolError,
    provenance,
    slicing::{self, package},
};

const FORMAT_VERSION: u32 = 1;
const RECORD_FILE: &str = "pictures.json";
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_ERROR_CHARS: usize = 512;
/// Largest model the slicer is asked to preview.
pub(crate) const MAX_PREVIEW_MODEL_BYTES: u64 = 256 * 1024 * 1024;
/// Largest 3MF package copied to read its plate thumbnail.
const MAX_PACKAGE_BYTES: u64 = 1024 * 1024 * 1024;
/// The wait before a failed picture is tried again; it doubles with each
/// further failure, up to a day.
const FIRST_RETRY_MS: u64 = 5 * 60 * 1000;
const MAX_RETRY_MS: u64 = 24 * 60 * 60 * 1000;

/// Reserved directory of a project's generated pictures.
pub(crate) fn directory(project_id: &str) -> String {
    format!(".printable/pictures/{project_id}")
}

fn record_path(project_id: &str) -> String {
    format!("{}/{RECORD_FILE}", directory(project_id))
}

/// A picture generated from a project's files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Picture {
    /// Reserved workspace path of the stored PNG.
    pub path: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    /// Workspace path of the file the picture was made from.
    pub source: String,
    pub origin: Origin,
    pub created_unix_ms: u64,
}

/// How a generated picture was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Copied from the first plate thumbnail of a sliced package.
    SliceThumbnail,
    /// Copied from the first plate thumbnail stored in a 3MF project file.
    PackageThumbnail,
    /// OrcaSlicer's plate picture of a model, exported without slicing it.
    SlicerPreview,
}

impl Picture {
    fn belongs_to(&self, project_id: &str) -> bool {
        self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && self.path == format!("{}/{}.png", directory(project_id), self.sha256)
    }
}

/// A picture that could not be made, and when to try again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Failure {
    pub attempts: u32,
    pub next_attempt_unix_ms: u64,
    pub error: String,
}

/// What `pictures.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub format_version: u32,
    /// A digest of the files the plate picture was chosen from; a change to
    /// any of them makes the picture again.
    pub candidates: String,
    pub plate: Option<Picture>,
    pub plate_failure: Option<Failure>,
}

impl Record {
    /// The plate picture, when it is stored where this project's pictures
    /// belong.
    pub(crate) fn plate(&self, project_id: &str) -> Option<&Picture> {
        self.plate
            .as_ref()
            .filter(|picture| picture.belongs_to(project_id))
    }
}

/// Read a project's record. The pictures are derived data, so a record that
/// is missing or cannot be read counts as absent, and a picture file that is
/// gone or cannot be read counts as no picture: the picture is made again and
/// the record replaced, and the pass reports a failure to write it. A
/// project's card never fails because of its generated pictures.
pub(crate) fn read(workspace: &Workspace, project_id: &str) -> Option<Record> {
    let path = record_path(project_id);
    if workspace.stat_artifact(&path).ok()?.size_bytes > MAX_RECORD_BYTES {
        return None;
    }
    let (_, bytes) = workspace.read_artifact(&path).ok()?;
    let mut record = serde_json::from_slice::<Record>(&bytes)
        .ok()
        .filter(|record| record.format_version == FORMAT_VERSION)?;
    if record
        .plate
        .as_ref()
        .is_some_and(|plate| workspace.stat_artifact(&plate.path).is_err())
    {
        record.plate = None;
    }
    Some(record)
}

/// One way to make a project's plate picture; a plan tries its steps in
/// order until one succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Copy the first plate thumbnail stored in a 3MF package.
    Thumbnail { package: String, origin: Origin },
    /// Ask the slicer to export a plate preview of a model.
    Preview {
        /// Workspace path of the model.
        model: String,
        /// The model's path within the project directory.
        relative: String,
    },
}

/// What a project's plate picture needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Need {
    /// Nothing: the project has a cover or no model, or its picture shows
    /// its current files.
    Nothing,
    /// The picture is not current, and a failure waits for its retry time.
    Retry,
    /// Make the picture now.
    Make(Box<Plan>),
}

/// How to make one project's plate picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub project_id: String,
    pub candidates: String,
    pub steps: Vec<Step>,
    /// The record this plan replaces.
    pub prior: Option<Record>,
}

/// The newest of `files` that `wanted` selects, by modification time and
/// then path.
fn newest<'a>(
    files: &[(&'a str, &'a ArtifactMeta)],
    wanted: impl Fn(&str, &ArtifactMeta) -> bool,
) -> Option<(&'a str, &'a ArtifactMeta)> {
    files
        .iter()
        .copied()
        .filter(|(relative, meta)| wanted(relative, meta))
        .max_by(|(a, a_meta), (b, b_meta)| {
            a_meta
                .modified_ns
                .cmp(&b_meta.modified_ns)
                .then_with(|| b.cmp(a))
        })
}

/// Decide what a project's plate picture needs. `files` are the project's
/// files with their paths within the project directory. There is nothing to
/// do when the project has a cover or no model, or when its picture shows its
/// current files. A recorded failure is retried once its wait has passed,
/// even while an earlier picture is kept.
pub(crate) fn plan(
    project_id: &str,
    files: &[(&str, &ArtifactMeta)],
    has_cover: bool,
    record: Option<&Record>,
    previews: bool,
    now_unix_ms: u64,
) -> Need {
    if has_cover {
        return Need::Nothing;
    }
    let lower = |relative: &str| relative.to_ascii_lowercase();
    let sliced = |relative: &str, _: &ArtifactMeta| lower(relative).ends_with(".gcode.3mf");
    let project_3mf = |relative: &str, _: &ArtifactMeta| {
        let name = lower(relative);
        name.ends_with(".3mf") && !name.ends_with(".gcode.3mf")
    };
    let model = |relative: &str, meta: &ArtifactMeta| {
        let name = lower(relative);
        meta.size_bytes <= MAX_PREVIEW_MODEL_BYTES
            && (name.ends_with(".stl")
                || name.ends_with(".obj")
                || (name.ends_with(".3mf") && !name.ends_with(".gcode.3mf")))
    };
    let mut steps = Vec::new();
    let mut digest = Sha256::new();
    digest.update(FORMAT_VERSION.to_le_bytes());
    digest.update([u8::from(previews)]);
    let mut note = |kind: &str, relative: &str, meta: &ArtifactMeta| {
        for part in [kind.as_bytes(), relative.as_bytes()] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
        digest.update(meta.size_bytes.to_le_bytes());
        digest.update(meta.modified_ns.to_le_bytes());
    };
    for (selected, origin) in [
        (newest(files, sliced), Origin::SliceThumbnail),
        (newest(files, project_3mf), Origin::PackageThumbnail),
    ] {
        if let Some((relative, meta)) = selected {
            note("thumbnail", relative, meta);
            steps.push(Step::Thumbnail {
                package: meta.path.clone(),
                origin,
            });
        }
    }
    if previews && let Some((relative, meta)) = newest(files, model) {
        note("preview", relative, meta);
        steps.push(Step::Preview {
            model: meta.path.clone(),
            relative: relative.to_owned(),
        });
    }
    if steps.is_empty() {
        return Need::Nothing;
    }
    let candidates: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if let Some(record) = record.filter(|record| record.candidates == candidates) {
        match &record.plate_failure {
            Some(failure) if failure.next_attempt_unix_ms > now_unix_ms => return Need::Retry,
            Some(_) => {}
            None if record.plate.is_some() => return Need::Nothing,
            None => {}
        }
    }
    Need::Make(Box::new(Plan {
        project_id: project_id.to_owned(),
        candidates,
        steps,
        prior: record.cloned(),
    }))
}

/// The first plate thumbnail stored in a workspace 3MF package.
pub(crate) fn package_thumbnail(
    workspace: &Workspace,
    package: &str,
) -> Result<Vec<u8>, ToolError> {
    let snapshot = workspace.snapshot_artifact_bounded(package, MAX_PACKAGE_BYTES)?;
    let plate = package::facts(snapshot.path())?
        .thumbnails
        .into_iter()
        .min()
        .ok_or_else(|| ToolError::Validation("the 3MF has no plate thumbnail".into()))?;
    package::plate_thumbnail(snapshot.path(), plate)
}

/// Store a generated PNG under its digest and describe it.
pub(crate) fn store(
    workspace: &Workspace,
    project_id: &str,
    bytes: &[u8],
    source: &str,
    origin: Origin,
    now_unix_ms: u64,
) -> Result<Picture, ToolError> {
    if bytes.len() as u64 > cover::MAX_COVER_BYTES {
        return Err(ToolError::Validation(
            "the generated picture is larger than a picture may be".into(),
        ));
    }
    let (media_type, image) = cover::decode(bytes)?;
    if media_type != "image/png" {
        return Err(ToolError::Validation(
            "a generated picture must be a PNG".into(),
        ));
    }
    let sha256 = provenance::digest(bytes);
    let path = format!("{}/{sha256}.png", directory(project_id));
    match workspace.write_reserved_artifact(&path, bytes, false) {
        Ok(_) | Err(WsError::AlreadyExists(_)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(Picture {
        path,
        sha256,
        width: image.width(),
        height: image.height(),
        source: source.to_owned(),
        origin,
        created_unix_ms: now_unix_ms,
    })
}

/// Record a made picture. The picture it replaces, if any, is removed.
pub(crate) fn record_made(
    workspace: &Workspace,
    plan: &Plan,
    picture: Picture,
) -> Result<(), ToolError> {
    let replaced = plan
        .prior
        .as_ref()
        .and_then(|prior| prior.plate(&plan.project_id))
        .filter(|prior| prior.path != picture.path)
        .map(|prior| prior.path.clone());
    write(
        workspace,
        &plan.project_id,
        &Record {
            format_version: FORMAT_VERSION,
            candidates: plan.candidates.clone(),
            plate: Some(picture),
            plate_failure: None,
        },
    )?;
    if let Some(path) = replaced {
        workspace.remove_reserved_artifact(&path)?;
    }
    Ok(())
}

/// Record that no step made a picture. An earlier picture is kept until a
/// new one replaces it.
pub(crate) fn record_failed(
    workspace: &Workspace,
    plan: &Plan,
    error: &str,
    now_unix_ms: u64,
) -> Result<(), ToolError> {
    let prior = plan.prior.as_ref();
    let attempts = prior
        .filter(|prior| prior.candidates == plan.candidates)
        .and_then(|prior| prior.plate_failure.as_ref())
        .map_or(0, |failure| failure.attempts)
        .saturating_add(1);
    let wait = FIRST_RETRY_MS
        .saturating_mul(1 << (attempts - 1).min(20))
        .min(MAX_RETRY_MS);
    write(
        workspace,
        &plan.project_id,
        &Record {
            format_version: FORMAT_VERSION,
            candidates: plan.candidates.clone(),
            plate: prior
                .and_then(|prior| prior.plate(&plan.project_id))
                .cloned(),
            plate_failure: Some(Failure {
                attempts,
                next_attempt_unix_ms: now_unix_ms.saturating_add(wait),
                error: error.chars().take(MAX_ERROR_CHARS).collect(),
            }),
        },
    )
}

fn write(workspace: &Workspace, project_id: &str, record: &Record) -> Result<(), ToolError> {
    workspace.write_reserved_artifact(
        &record_path(project_id),
        &serde_json::to_vec(record)?,
        true,
    )?;
    Ok(())
}

/// The slicer request for a preview step.
pub(crate) fn preview_request(project_id: &str, relative: &str) -> slicing::PreviewParams {
    slicing::PreviewParams {
        project_id: project_id.to_owned(),
        source: relative.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(relative: &str, modified_ns: i64) -> ArtifactMeta {
        ArtifactMeta {
            path: format!("projects/part/{relative}"),
            size_bytes: 100,
            media_type: "application/octet-stream",
            modified_ns,
        }
    }

    fn plan_for(files: &[ArtifactMeta], record: Option<&Record>, previews: bool) -> Need {
        let listed: Vec<_> = files
            .iter()
            .map(|meta| (meta.path.strip_prefix("projects/part/").unwrap(), meta))
            .collect();
        plan("part", &listed, false, record, previews, 1_000)
    }

    fn make(need: Need) -> Plan {
        match need {
            Need::Make(plan) => *plan,
            other => panic!("expected a plan, found {other:?}"),
        }
    }

    #[test]
    fn plans_prefer_slices_then_project_packages_then_the_newest_model() {
        let files = [
            meta("old.stl", 1),
            meta("new.stl", 5),
            meta("design.3mf", 3),
            meta("slice-1/model.gcode.3mf", 2),
            meta("notes.txt", 9),
        ];
        let planned = make(plan_for(&files, None, true));
        assert_eq!(
            planned.steps,
            [
                Step::Thumbnail {
                    package: "projects/part/slice-1/model.gcode.3mf".into(),
                    origin: Origin::SliceThumbnail,
                },
                Step::Thumbnail {
                    package: "projects/part/design.3mf".into(),
                    origin: Origin::PackageThumbnail,
                },
                Step::Preview {
                    model: "projects/part/new.stl".into(),
                    relative: "new.stl".into(),
                },
            ]
        );
        // Without a slicer, only stored thumbnails are tried.
        assert_eq!(make(plan_for(&files, None, false)).steps.len(), 2);
        // Nothing to picture, or a cover, means nothing to do.
        assert_eq!(plan_for(&[meta("notes.txt", 1)], None, true), Need::Nothing);
        let listed = [("new.stl", &files[1])];
        assert_eq!(
            plan("part", &listed, true, None, true, 1_000),
            Need::Nothing
        );
    }

    #[test]
    fn a_made_picture_stands_until_its_files_change_and_failures_wait() {
        let files = [meta("part.stl", 1)];
        let first = make(plan_for(&files, None, true));
        let picture = Picture {
            path: format!("{}/{}.png", directory("part"), "a".repeat(64)),
            sha256: "a".repeat(64),
            width: 8,
            height: 8,
            source: "projects/part/part.stl".into(),
            origin: Origin::SlicerPreview,
            created_unix_ms: 1,
        };
        let made = Record {
            format_version: FORMAT_VERSION,
            candidates: first.candidates.clone(),
            plate: Some(picture.clone()),
            plate_failure: None,
        };
        assert_eq!(made.plate("part"), Some(&picture));
        assert_eq!(made.plate("other"), None);
        assert_eq!(plan_for(&files, Some(&made), true), Need::Nothing);
        // A changed model is pictured again.
        let changed = make(plan_for(&[meta("part.stl", 2)], Some(&made), true));
        assert_ne!(changed.candidates, first.candidates);
        // A failure waits for its retry time.
        let waiting = Record {
            plate: None,
            plate_failure: Some(Failure {
                attempts: 1,
                next_attempt_unix_ms: 5_000,
                error: "refused".into(),
            }),
            ..made.clone()
        };
        assert_eq!(plan_for(&files, Some(&waiting), true), Need::Retry);
        let listed = [("part.stl", &files[0])];
        make(plan("part", &listed, false, Some(&waiting), true, 5_000));
        // A failed refresh keeps the earlier picture and is still retried.
        let refreshing = Record {
            plate: Some(picture.clone()),
            ..waiting.clone()
        };
        assert_eq!(plan_for(&files, Some(&refreshing), true), Need::Retry);
        make(plan("part", &listed, false, Some(&refreshing), true, 5_000));
    }

    #[test]
    fn records_keep_the_picture_and_back_off_after_failures() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let mut png = Vec::new();
        image::RgbImage::from_pixel(8, 6, image::Rgb([200, 40, 40]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let planned = make(plan_for(&[meta("part.stl", 1)], None, true));
        let picture = store(
            &workspace,
            "part",
            &png,
            "projects/part/part.stl",
            Origin::SlicerPreview,
            7,
        )
        .unwrap();
        assert_eq!((picture.width, picture.height), (8, 6));
        assert!(
            store(
                &workspace,
                "part",
                b"not a picture",
                "x",
                Origin::SlicerPreview,
                7
            )
            .is_err()
        );
        record_made(&workspace, &planned, picture.clone()).unwrap();
        let made = read(&workspace, "part").unwrap();
        assert_eq!(made.plate("part"), Some(&picture));

        let mut retry = planned.clone();
        retry.prior = Some(made);
        record_failed(&workspace, &retry, "refused", 1_000).unwrap();
        let first = read(&workspace, "part").unwrap();
        assert_eq!(first.plate("part"), Some(&picture), "the picture is kept");
        let failure = first.plate_failure.clone().unwrap();
        assert_eq!(
            (failure.attempts, failure.next_attempt_unix_ms),
            (1, 1_000 + FIRST_RETRY_MS)
        );
        retry.prior = Some(first);
        record_failed(&workspace, &retry, "refused", 1_000).unwrap();
        let second = read(&workspace, "part").unwrap().plate_failure.unwrap();
        assert_eq!(
            (second.attempts, second.next_attempt_unix_ms),
            (2, 1_000 + 2 * FIRST_RETRY_MS)
        );

        // A picture whose file is gone is made again.
        workspace.remove_reserved_artifact(&picture.path).unwrap();
        let gone = read(&workspace, "part").unwrap();
        assert_eq!(gone.plate, None);
        assert!(gone.plate_failure.is_some());

        // An unreadable record is treated as absent.
        workspace
            .write_reserved_artifact(&record_path("part"), b"{not json", true)
            .unwrap();
        assert_eq!(read(&workspace, "part"), None);
    }

    #[test]
    fn package_thumbnails_come_from_the_first_plate_that_has_one() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let package = crate::slicing::package::fixture::package(&[
            ("Metadata/plate_2.png", b"second"),
            ("Metadata/plate_3.png", b"third"),
        ]);
        workspace
            .write_artifact("projects/part/design.3mf", &package, false)
            .unwrap();
        assert_eq!(
            package_thumbnail(&workspace, "projects/part/design.3mf").unwrap(),
            b"second"
        );
        let bare = crate::slicing::package::fixture::package(&[("3D/3dmodel.model", b"<model/>")]);
        workspace
            .write_artifact("projects/part/bare.3mf", &bare, false)
            .unwrap();
        assert!(package_thumbnail(&workspace, "projects/part/bare.3mf").is_err());
    }
}
