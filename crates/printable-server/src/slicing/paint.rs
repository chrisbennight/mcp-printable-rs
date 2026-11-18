//! Read existing Orca paint from a private snapshot of a project 3MF.

use super::processing::{ProcessingLimit, copy_and_hash};
use super::{ProcessGroup, SliceWorker, invalid, slice_error};
use crate::{error::ToolError, projects, worker_health::probe_native};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::sink,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

const TIME_LIMIT: Duration = Duration::from_secs(60);
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REPORT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INLINE_VOLUMES: usize = 32;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectPaintParams {
    pub project_id: String,
    /// Project-relative Orca 3MF.
    pub source: String,
}

#[derive(Serialize, JsonSchema)]
pub struct PaintInspection {
    source: String,
    source_sha256: String,
    summary: PaintSummary,
    categories: PaintCategories,
    painted_volumes: Vec<PaintedVolume>,
    painted_volumes_truncated: bool,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct PaintSummary {
    objects: u64,
    volumes: u64,
    volumes_with_paint: u64,
    /// Sum of facet assignments across paint layers.
    painted_facets_total: u64,
}

#[derive(Default, Serialize, JsonSchema)]
struct PaintCategories {
    supports: bool,
    seam: bool,
    material: bool,
    fuzzy_skin: bool,
}

#[derive(Serialize, JsonSchema)]
struct PaintedVolume {
    /// One-based Orca object index.
    object: u64,
    /// One-based volume index within the object.
    volume: u64,
    supports: SupportOrSeam,
    seam: SupportOrSeam,
    material: Vec<MaterialPaint>,
    fuzzy_skin_facets: u64,
}

#[derive(Serialize, JsonSchema)]
struct SupportOrSeam {
    enforcer_facets: u64,
    blocker_facets: u64,
}

#[derive(Serialize, JsonSchema)]
struct MaterialPaint {
    /// One-based filament index.
    filament: u32,
    facets: u64,
}

#[derive(Deserialize)]
struct NativeInspection {
    frame: String,
    summary: PaintSummary,
    objects: Vec<NativeObject>,
}

#[derive(Deserialize)]
struct NativeObject {
    index: u64,
    volumes: Vec<NativeVolume>,
}

#[derive(Deserialize)]
struct NativeVolume {
    index: u64,
    painted_facets_total: u64,
    paints: NativePaint,
}

#[derive(Deserialize)]
struct NativePaint {
    supports: NativeLayer,
    seam: NativeLayer,
    mmu_segmentation: NativeLayer,
    fuzzy_skin: NativeLayer,
}

#[derive(Deserialize)]
struct NativeLayer {
    #[serde(default)]
    states: Vec<NativeAssignment>,
}

#[derive(Deserialize)]
struct NativeAssignment {
    state: String,
    facets: u64,
}

impl NativeLayer {
    fn facets(&self, state: &str) -> u64 {
        self.states
            .iter()
            .find(|entry| entry.state == state)
            .map_or(0, |entry| entry.facets)
    }

    fn support_or_seam(&self) -> SupportOrSeam {
        SupportOrSeam {
            enforcer_facets: self.facets("ENFORCER"),
            blocker_facets: self.facets("BLOCKER"),
        }
    }
}

impl NativeInspection {
    fn summarize(
        self,
        source: String,
        source_sha256: String,
    ) -> Result<PaintInspection, ToolError> {
        if self.frame != "mesh_local" {
            return Err(slice_error(
                "native paint inspection returned an unexpected frame",
            ));
        }
        let mut painted_volumes = Vec::new();
        let mut categories = PaintCategories::default();
        let mut painted_volumes_truncated = false;
        let volumes = self
            .objects
            .into_iter()
            .flat_map(|object| {
                object
                    .volumes
                    .into_iter()
                    .map(move |volume| (object.index, volume))
            })
            .filter(|(_, volume)| volume.painted_facets_total > 0);
        for (object, volume) in volumes {
            let paints = volume.paints;
            categories.supports |= !paints.supports.states.is_empty();
            categories.seam |= !paints.seam.states.is_empty();
            categories.material |= !paints.mmu_segmentation.states.is_empty();
            categories.fuzzy_skin |= !paints.fuzzy_skin.states.is_empty();
            if painted_volumes.len() == MAX_INLINE_VOLUMES {
                painted_volumes_truncated = true;
                continue;
            }
            let index = |value: u64| {
                value
                    .checked_add(1)
                    .ok_or_else(|| slice_error("native paint inspection returned an invalid index"))
            };
            let material = paints
                .mmu_segmentation
                .states
                .into_iter()
                .map(|entry| {
                    let filament = entry
                        .state
                        .strip_prefix("extruder_")
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|value| (1..=32).contains(value))
                        .ok_or_else(|| {
                            slice_error(
                                "native paint inspection returned an invalid filament index",
                            )
                        })?;
                    Ok(MaterialPaint {
                        filament,
                        facets: entry.facets,
                    })
                })
                .collect::<Result<_, ToolError>>()?;
            painted_volumes.push(PaintedVolume {
                object: index(object)?,
                volume: index(volume.index)?,
                supports: paints.supports.support_or_seam(),
                seam: paints.seam.support_or_seam(),
                material,
                fuzzy_skin_facets: paints.fuzzy_skin.facets("FUZZY_SKIN"),
            });
        }
        Ok(PaintInspection {
            source,
            source_sha256,
            summary: self.summary,
            categories,
            painted_volumes,
            painted_volumes_truncated,
        })
    }
}

impl SliceWorker {
    pub(super) async fn inspect_paint(
        &self,
        params: InspectPaintParams,
    ) -> Result<Value, ToolError> {
        if !Path::new(&params.source)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("3mf"))
        {
            return Err(invalid("paint inspection requires a project 3MF"));
        }
        let source = projects::resolve(&self.workspace, &params.project_id, &params.source)?;
        let limit = ProcessingLimit {
            deadline: Instant::now() + TIME_LIMIT,
            cancellation: CancellationToken::new(),
            operation: "native Orca paint inspection",
        };
        let _cancel_on_drop = limit.cancellation.clone().drop_guard();
        let inspection =
            tokio::time::timeout(TIME_LIMIT, self.inspect_paint_snapshot(source, limit))
                .await
                .map_err(|_| slice_error("native Orca paint inspection exceeded its deadline"))??;
        Ok(serde_json::to_value(inspection)?)
    }

    async fn inspect_paint_snapshot(
        &self,
        source: String,
        limit: ProcessingLimit,
    ) -> Result<PaintInspection, ToolError> {
        let admission = self
            .admit("slicer is busy; retry paint inspection later")
            .await?;
        let mut help = Command::new(&self.binary);
        help.arg("--help");
        if !probe_native(help)
            .await
            .is_some_and(|text| text.contains("--inspect-paint"))
        {
            return Err(slice_error(
                "the installed OrcaSlicer does not support paint inspection",
            ));
        }
        limit.check()?;

        let workspace = Arc::clone(&self.workspace);
        let snapshot_source = source.clone();
        let copy_limit = limit.clone();
        let (snapshot, source_hash, admission) = tokio::task::spawn_blocking(move || {
            copy_limit.check()?;
            let snapshot =
                workspace.snapshot_artifact_bounded(&snapshot_source, MAX_SOURCE_BYTES)?;
            let mut input = std::fs::File::open(snapshot.path())?;
            let hash = copy_and_hash(&mut input, &mut sink(), &copy_limit)?;
            Ok::<_, ToolError>((snapshot, hash, admission))
        })
        .await
        .map_err(|_| slice_error("paint inspection input snapshot task failed"))??;
        limit.check()?;

        let local_source = snapshot.path();
        let working_directory = local_source.parent().expect("confined snapshot directory");
        let mut command = Command::new(&self.binary);
        command
            .arg("--inspect-paint")
            .arg(local_source)
            .current_dir(working_directory)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", working_directory)
            .env("OMP_NUM_THREADS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        snapshot
            .child_lease()?
            .retain_for_command(command.as_std_mut())?;
        let mut child = command.spawn()?;
        let group = ProcessGroup(child.id().expect("spawned paint inspection process"));
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("piped paint inspection stdout")
            .take(MAX_REPORT_BYTES + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() as u64 > MAX_REPORT_BYTES {
            return Err(slice_error(
                "native paint inspection report exceeds its size limit",
            ));
        }
        let status = child.wait().await?;
        drop(group);
        if !status.success() {
            return Err(slice_error(&format!(
                "OrcaSlicer could not inspect 3MF paint ({status})"
            )));
        }
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            let mut input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(snapshot.path())?;
            if !input.metadata()?.is_file() {
                return Err(slice_error(
                    "native paint inspection changed its input snapshot",
                ));
            }
            if copy_and_hash(&mut input, &mut sink(), &limit)? != source_hash {
                return Err(slice_error(
                    "native paint inspection changed its input snapshot",
                ));
            }
            let native: NativeInspection = serde_json::from_slice(&output)
                .map_err(|_| slice_error("OrcaSlicer returned an invalid paint report"))?;
            limit.check()?;
            native.summarize(source, source_hash)
        })
        .await
        .map_err(|_| slice_error("native paint inspection validation task failed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provenance, slicing::tests::fixture, tools::output};
    use serde_json::json;

    #[test]
    fn project_categories_include_paint_beyond_inline_volume_limit() {
        let volumes: Vec<_> = (0..=MAX_INLINE_VOLUMES)
            .map(|index| {
                let (supports, fuzzy_skin) = if index == MAX_INLINE_VOLUMES {
                    (
                        json!({}),
                        json!({"states":[{"state":"FUZZY_SKIN","facets":1}]}),
                    )
                } else {
                    (
                        json!({"states":[{"state":"ENFORCER","facets":1}]}),
                        json!({}),
                    )
                };
                json!({"index": index, "painted_facets_total":1, "paints":{
                    "supports":supports,"seam":{},"mmu_segmentation":{},"fuzzy_skin":fuzzy_skin
                }})
            })
            .collect();
        let count = volumes.len();
        let native: NativeInspection = serde_json::from_value(json!({
            "frame":"mesh_local","summary":{"objects":1,"volumes":count,
                "volumes_with_paint":count,"painted_facets_total":count},
            "objects":[{"index":0,"volumes":volumes}]
        }))
        .unwrap();
        let result = native
            .summarize("part.3mf".into(), "fixture-digest".into())
            .unwrap();
        assert_eq!(result.painted_volumes.len(), MAX_INLINE_VOLUMES);
        assert!(result.painted_volumes_truncated);
        assert!(result.categories.supports);
        assert!(result.categories.fuzzy_skin);
    }

    #[tokio::test]
    async fn worker_projects_paint_assignments_and_preserves_input() {
        let directory = tempfile::tempdir().unwrap();
        let worker = fixture(directory.path(), false);
        for (name, bytes) in [
            ("painted.3mf", b"painted".as_slice()),
            ("plain.3mf", b"plain".as_slice()),
            ("invalid.3mf", b"invalid".as_slice()),
        ] {
            worker
                .workspace
                .write_artifact(&format!("projects/part/{name}"), bytes, false)
                .unwrap();
        }
        std::fs::write(
            &worker.binary,
            r#"#!/usr/bin/python3
import json, pathlib, sys
if sys.argv[1:] == ["--help"]:
    print("--inspect-paint")
    sys.exit(0)
assert sys.argv[1] == "--inspect-paint"
source = pathlib.Path(sys.argv[2])
assert source.parent == pathlib.Path.cwd()
content = source.read_bytes()
if content == b"invalid":
    sys.exit(1)
painted = int(content == b"painted")
def layer(state, facets):
    return {"states":[{"state":state,"facets":facets}]} if painted else {"empty":True}
print(json.dumps({"frame":"mesh_local","sources":[str(source)],"summary":{
    "objects":1,"volumes":1,"volumes_with_paint":painted,"painted_facets_total":5*painted
}, "objects":[{"index":0,"volumes":[{"index":0,"painted_facets_total":5*painted,"paints":{
    "supports":layer("ENFORCER",1),"seam":layer("BLOCKER",1),
    "mmu_segmentation":layer("extruder_2",2),"fuzzy_skin":layer("FUZZY_SKIN",1)
}}]}]}))
"#,
        )
        .unwrap();
        let request = |source: &str| InspectPaintParams {
            project_id: "part".into(),
            source: source.into(),
        };
        let painted = worker.inspect_paint(request("painted.3mf")).await.unwrap();
        assert_eq!(painted["source_sha256"], provenance::digest(b"painted"));
        assert_eq!(painted["summary"]["volumes_with_paint"], 1);
        assert_eq!(
            painted["painted_volumes"][0],
            json!({"object":1,"volume":1,"supports":{"enforcer_facets":1,"blocker_facets":0},
                "seam":{"enforcer_facets":0,"blocker_facets":1},
                "material":[{"filament":2,"facets":2}],"fuzzy_skin_facets":1})
        );
        assert!(!painted.to_string().contains("/payload/"));
        assert_eq!(
            painted["categories"],
            json!({"supports":true,"seam":true,"material":true,"fuzzy_skin":true})
        );
        let schema = output::schema("slice");
        assert!(jsonschema::is_valid(&json!(schema.as_ref()), &painted));
        let schema = output::action_schema("slice", "inspect_paint").unwrap();
        assert!(jsonschema::is_valid(&schema, &painted));

        let plain = worker.inspect_paint(request("plain.3mf")).await.unwrap();
        assert_eq!(plain["summary"]["volumes_with_paint"], 0);
        assert_eq!(plain["painted_volumes"], json!([]));
        assert!(worker.inspect_paint(request("invalid.3mf")).await.is_err());
        let (_, bytes) = worker
            .workspace
            .read_artifact("projects/part/painted.3mf")
            .unwrap();
        assert_eq!(bytes, b"painted");
    }
}
