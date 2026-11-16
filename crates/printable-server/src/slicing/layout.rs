//! Source layout evidence. Exact intersections run only in the disposable
//! geometry process; a source report does not certify generated toolpaths.

use std::{
    collections::BTreeSet,
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use printable_geom::{
    AssemblyOptions, AssemblyRelation, analyze_assembly,
    limit::WorkLimit,
    plate::{canonical_triangles, model_footprint},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ToolError;

use super::model;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub source: std::path::PathBuf,
    pub options: InspectOptions,
    pub placement: Option<PreparedLayout>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedLayout {
    pub controls: super::placement::LayoutControls,
    pub destination: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    pub project_id: String,
    /// Authored project-relative 3MF; output toolpaths are a separate check.
    pub source: String,
    /// New retained directory for the source identity and complete report.
    pub output_dir: String,
    pub expected_source_sha256: Option<String>,
    #[serde(default)]
    pub options: InspectOptions,
}

impl super::SliceWorker {
    pub(super) async fn inspect_layout(&self, params: InspectParams) -> Result<Value, ToolError> {
        params.options.validate()?;
        if Path::new(&params.source)
            .extension()
            .and_then(|value| value.to_str())
            != Some("3mf")
        {
            return Err(ToolError::Validation(
                "source layout inspection requires a 3MF project".into(),
            ));
        }
        let budget = Duration::from_secs(params.options.timeout_seconds + 5);
        let limit = super::processing::ProcessingLimit {
            deadline: Instant::now() + budget,
            cancellation: tokio_util::sync::CancellationToken::new(),
            operation: "source layout inspection",
        };
        let _cancel_on_drop = limit.cancellation.clone().drop_guard();
        let (report, output, admission, staging) = tokio::time::timeout(budget, async {
            let source = crate::projects::resolve(&self.workspace, &params.project_id, &params.source)?;
            let output = super::slice_directory(&self.workspace, &params.project_id, &params.output_dir)?;
            let admission = Arc::new(self.admit("slicer is busy; inspect the active preparation before retrying").await?);
            let workspace = Arc::clone(&self.workspace);
            let input = source.clone();
            let copy_limit = limit.clone();
            let copy_admission = Arc::clone(&admission);
            let (staging, source_sha256) = tokio::task::spawn_blocking(move || -> Result<_,ToolError> {
                let _admission = copy_admission;
                copy_limit.check()?;
                let size = workspace.stat_artifact(&input)?.size_bytes;
                let staging = Arc::new(workspace.scratch(size,"source-layout")?);
                let mut destination = std::fs::File::create(staging.path().join("source.3mf"))?;
                let (_, hash) = workspace.with_artifact_reader(&input, |reader| {
                    let mut bounded = reader.take(size.saturating_add(1));
                    super::processing::copy_and_hash(&mut bounded,&mut destination,&copy_limit)
                })?;
                copy_limit.check()?;
                if destination.metadata()?.len() != size {
                    return Err(ToolError::Validation("source changed during layout inspection".into()));
                }
                Ok((staging,hash))
            }).await.map_err(|_|ToolError::Validation("layout source snapshot task failed".into()))??;
            limit.check()?;
            if params.expected_source_sha256.as_ref().is_some_and(|expected|expected != &source_sha256) {
                return Err(ToolError::Validation("source changed since layout selection; inspect the current source before applying placements".into()));
            }
            let mut report = run_worker(self,&staging.path().join("source.3mf"),staging.path(),&params.options,None).await?;
            limit.check()?;
            report["source"] = json!({"path":source,"sha256":source_sha256});
            report["engine"] = json!({"name":"OrcaSlicer","version":super::ENGINE_VERSION,
                "source_revision":super::ENGINE_SOURCE_REVISION,"runtime_source_verified":false});
            report["options"] = serde_json::to_value(&params.options)?;
            Ok::<_,ToolError>((report,output,admission,staging))
        }).await.map_err(|_|ToolError::Validation("source layout inspection exceeded its processing deadline".into()))??;
        let workspace = Arc::clone(&self.workspace);
        tokio::task::spawn_blocking(move || {
            let _owners = (admission, staging);
            publish_inspection(&workspace, &output, report, &limit)
        })
        .await
        .map_err(|_| ToolError::Validation("layout report publication task failed".into()))?
    }
}

fn publish_inspection(
    workspace: &printable_workspace::Workspace,
    output: &str,
    report: Value,
    limit: &super::processing::ProcessingLimit,
) -> Result<Value, ToolError> {
    let report_bytes = serde_json::to_vec(&report)?;
    let report_sha256 = crate::provenance::digest(&report_bytes);
    let instances = report["instances"]
        .as_array()
        .ok_or_else(|| ToolError::Validation("layout report omitted instances".into()))?;
    let findings = report["findings"]
        .as_array()
        .ok_or_else(|| ToolError::Validation("layout report omitted findings".into()))?;
    limit.check()?;
    // Once publication begins, finish retaining the complete report.
    if !workspace.create_public_directory(output)? {
        return Err(ToolError::Validation(
            "layout output directory exists; inspect the retained report or choose a new directory"
                .into(),
        ));
    }
    let artifact = workspace.write_generated_bytes(
        &format!("{output}/source-layout.json"),
        &report_bytes,
        false,
    )?;
    let source_sha256 = &report["source"]["sha256"];
    let compact_instances = instances.iter().take(32).map(|instance|json!({
            "build_index":instance["build_index"],"copy":instance["copy"],"resource":instance["resource"],
            "native_plate":instance["native_plate"],"native_identify_id":instance["native_identify_id"],
            "build_transform_3mf":instance["build_transform_3mf"],"resource_transform_3mf":instance["resource_transform_3mf"],
            "bounds":instance["footprint"]["bounds"]})).collect::<Vec<_>>();
    Ok(
        json!({"source_sha256":source_sha256,"state":report["state"],
            "instances":compact_instances,"instances_count":instances.len(),"instances_truncated":instances.len()>32,
            "findings":findings.iter().take(32).collect::<Vec<_>>(),"findings_count":findings.len(),"findings_truncated":findings.len()>32,
            "unresolved":report["unresolved"],"report_artifact":artifact,
            "report_sha256":report_sha256,
            "generated_toolpath_clearance":"unmeasured"}),
    )
}

/// The child writes bulky evidence to a private file. Its stdout is only the
/// existing small geometry-worker protocol, and dropping the timed-out child
/// kills it even if the native kernel has not returned to a budget check.
pub(super) async fn run_worker(
    worker: &super::SliceWorker,
    source: &Path,
    staging: &Path,
    options: &InspectOptions,
    controls: Option<&super::placement::LayoutControls>,
) -> Result<Value, ToolError> {
    use tokio::io::AsyncReadExt;
    options.validate()?;
    let request_path = staging.join("layout-worker-request.json");
    let report_path = staging.join("source-layout.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec(&WorkerRequest {
            source: source.to_owned(),
            options: options.clone(),
            placement: controls.map(|controls| PreparedLayout {
                controls: controls.clone(),
                destination: staging.join("prepared.3mf"),
            }),
        })?,
    )?;
    let binary =
        crate::tools::geometry_worker_path_from_override(worker.geometry_worker_bin.as_deref())?;
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--inspect-layout")
        .arg(&request_path)
        .arg(&report_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::tools::set_worker_memory_limit(
        command.as_std_mut(),
        worker.geometry_worker_memory_bytes,
    )?;
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped worker stdout");
    let work = async {
        let mut bytes = Vec::new();
        stdout.take(64 * 1024 + 1).read_to_end(&mut bytes).await?;
        if bytes.len() > 64 * 1024 {
            return Err(ToolError::Validation(
                "geometry worker protocol exceeded its byte budget".into(),
            ));
        }
        let status = child.wait().await?;
        let response = crate::tools::parse_geometry_worker_output(status.success(), &bytes)?;
        if response["report_written"] != true {
            return Err(ToolError::Validation(
                "geometry worker did not confirm its layout report".into(),
            ));
        }
        let mut report = tokio::fs::File::open(&report_path).await?;
        let mut bytes = Vec::new();
        (&mut report)
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(ToolError::Validation(
                "layout report exceeded its byte budget".into(),
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    };
    tokio::time::timeout(Duration::from_secs(options.timeout_seconds + 5), work)
        .await
        .map_err(|_| {
            ToolError::Validation(
                "layout geometry process exceeded its time budget; clearance remains unmeasured"
                    .into(),
            )
        })?
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntendedInterface {
    /// Zero-based authored build indices, reported by source inspection.
    pub build_indices: [usize; 2],
    /// Design evidence explaining a deliberate fused or material interface.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InspectOptions {
    pub max_xml_bytes: u64,
    pub max_mesh_instances: usize,
    pub max_pairs: usize,
    pub timeout_seconds: u64,
    /// Requested solid-to-solid gap; generated support/brim gaps are separate.
    pub minimum_gap_mm: f64,
    pub intended_interfaces: Vec<IntendedInterface>,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            max_xml_bytes: 64 * 1024 * 1024,
            max_mesh_instances: 512,
            max_pairs: 4096,
            timeout_seconds: 30,
            minimum_gap_mm: 0.,
            intended_interfaces: Vec::new(),
        }
    }
}

impl InspectOptions {
    pub fn validate(&self) -> Result<(), ToolError> {
        if !(1..=256 * 1024 * 1024).contains(&self.max_xml_bytes)
            || !(1..=2048).contains(&self.max_mesh_instances)
            || !(1..=100_000).contains(&self.max_pairs)
            || !(1..=300).contains(&self.timeout_seconds)
            || !self.minimum_gap_mm.is_finite()
            || !(0. ..=1000.).contains(&self.minimum_gap_mm)
            || self.intended_interfaces.len() > 1024
        {
            return Err(ToolError::Validation("layout inspection requires bounded positive budgets and a finite gap from zero to 1000 mm".into()));
        }
        let mut pairs = BTreeSet::new();
        for interface in &self.intended_interfaces {
            let [a, b] = interface.build_indices;
            if a == b
                || interface.reason.trim().is_empty()
                || interface.reason.len() > 2000
                || !pairs.insert((a.min(b), a.max(b)))
            {
                return Err(ToolError::Validation("intended interfaces require distinct build indices, unique pairs and design evidence".into()));
            }
        }
        Ok(())
    }
}

/// Entry point for the memory-limited geometry executable. Keep this function
/// out of the server and slicer event loops: native solid operations allocate.
pub fn inspect_source(path: &Path, options: &InspectOptions) -> Result<Value, ToolError> {
    options.validate()?;
    let limit = WorkLimit::new(Duration::from_secs(options.timeout_seconds), || false);
    limit.apply(|| {
        let archive = model::read_archive(path, options.max_xml_bytes, || limit.check().is_ok())?;
        let meshes = archive.instances(options.max_mesh_instances)?;
        let mut unresolved = archive.unresolved.clone();
        let build = &archive.documents[&archive.primary_model].build;
        let plate_for = |index: usize| {
            let item = &build[index];
            archive.native.plates.iter().find(|plate| plate.instances.iter().any(|instance|
                instance.object_id == Some(item.component.object_id) && instance.instance_id == Some(item.copy)))
                .and_then(|plate| plate.plate)
        };
        let source_geometry_supported = !archive.unresolved.iter().any(|reason|
            matches!(reason.as_str(),"required_model_extensions_not_verified"|"geometry_extension_not_verified"|"mixed_mesh_and_component_object_not_verified"));
        let semantics_supported = |index: usize| {
            source_geometry_supported && archive.native.objects.get(&build[index].component.object_id).is_none_or(|object|
                object.parts.iter().all(|part| part.subtype.as_deref().is_none_or(|kind| kind == "normal_part")
                    && !part.settings.contains_key("part_type") && !part.settings.contains_key("volume_type")))
        };
        for interface in &options.intended_interfaces {
            if interface.build_indices.iter().any(|index| *index >= build.len() || !build[*index].printable) {
                return Err(ToolError::Validation("intended interface refers to a missing or nonprintable build instance".into()));
            }
        }
        let mut instances = Vec::new();
        for mesh in &meshes {
            limit.check()?;
            let item = &build[mesh.build_index];
            let identify_id = archive.native.plates.iter().flat_map(|plate|&plate.instances)
                .find(|instance|instance.object_id==Some(item.component.object_id) && instance.instance_id==Some(item.copy))
                .and_then(|instance|instance.identify_id).filter(|id|*id>0);
            let local = &archive.documents[&mesh.resource.model].objects[&mesh.resource.object_id].mesh;
            let signature = |mesh| -> Result<String,ToolError> {
                Ok(crate::provenance::digest(&serde_json::to_vec(&canonical_triangles(mesh)?)?))
            };
            instances.push(json!({"build_index":mesh.build_index,"copy":mesh.copy,
                "build_object_id":build[mesh.build_index].component.object_id,
                "native_plate":plate_for(mesh.build_index),
                "native_identify_id":identify_id,
                "resource":mesh.resource,"component_path":mesh.component_path,
                "object_type":mesh.object_type,"build_transform_3mf":item.component.transform,"resource_transform_3mf":mesh.transform,
                "local_geometry_sha256":signature(local)?,"world_geometry_sha256":signature(&mesh.mesh)?,
                "geometry_coordinate_grid_mm":0.0001,"triangles":mesh.mesh.triangles.len(),
                "property_groups":archive.documents[&mesh.resource.model].objects[&mesh.resource.object_id].property_groups,
                "property_indices":archive.documents[&mesh.resource.model].objects[&mesh.resource.object_id].property_indices,
                "declared_object_property_defaults":{
                    "pid":archive.documents[&mesh.resource.model].objects[&mesh.resource.object_id].default_property_group,
                    "pindex":archive.documents[&mesh.resource.model].objects[&mesh.resource.object_id].default_property_index},
                "footprint":model_footprint(&mesh.mesh)?}));
            if mesh.object_type != "model" || !semantics_supported(mesh.build_index) {
                unresolved.insert("source_geometry_or_native_resource_semantics_not_verified".into());
            }
        }
        let mut pairs = Vec::new();
        let mut findings = Vec::new();
        let mut count = 0;
        let mut different_plate_pairs = 0usize;
        'pairs: for (a_index, a) in meshes.iter().enumerate() {
            for (b_index, b) in meshes.iter().enumerate().skip(a_index + 1) {
                if limit.check().is_err() {
                    unresolved.insert("source_geometry_budget_exhausted".into());
                    break 'pairs;
                }
                if a.build_index == b.build_index { continue; }
                if let (Some(a),Some(b)) = (plate_for(a.build_index),plate_for(b.build_index))
                    && a != b {
                    different_plate_pairs += 1;
                    continue;
                }
                if count >= options.max_pairs {
                    unresolved.insert("source_pair_budget_exhausted".into());
                    break 'pairs;
                }
                count += 1;
                let ids = [a.build_index, b.build_index];
                let intended = options.intended_interfaces.iter().find(|interface| {
                    let [x, y] = interface.build_indices;
                    (x == ids[0] && y == ids[1]) || (x == ids[1] && y == ids[0])
                });
                if a.object_type != "model" || b.object_type != "model"
                    || !semantics_supported(a.build_index) || !semantics_supported(b.build_index) {
                    pairs.push(json!({"instances":[a_index,b_index],"build_indices":ids,
                        "state":"unsupported","reason":"source_geometry_or_native_resource_semantics_not_verified"}));
                    continue;
                }
                match analyze_assembly(&a.mesh, &b.mesh, AssemblyOptions {
                    required_clearance_mm: Some(options.minimum_gap_mm), ..Default::default()
                }) {
                    Ok(report) => {
                        let analysis = report.static_analysis;
                        let collision = analysis.relation == AssemblyRelation::Interfering;
                        let gap_failed = analysis.clearance_mm < options.minimum_gap_mm;
                        let accepted = intended.is_some();
                        let failed = (collision || gap_failed) && !accepted;
                        pairs.push(json!({"instances":[a_index,b_index],"build_indices":ids,
                            "state":if failed {"failed"} else {"passed"},
                            "solid_analysis":analysis,"intended_interface":intended}));
                        if failed {
                            let a_bounds = &instances[a_index]["footprint"]["bounds"];
                            let b_bounds = &instances[b_index]["footprint"]["bounds"];
                            findings.push(json!({"kind":if collision {"source_part_interference"} else {"source_gap_below_requirement"},
                                "instances":[a_index,b_index],"build_indices":ids,
                                "evidence":{"pair_index":pairs.len()-1,"resources":[&a.resource,&b.resource],
                                    "bounds_mm":[a_bounds,b_bounds],"relation":analysis.relation,
                                    "interference_volume_mm3":analysis.interference_volume_mm3,
                                    "closest_surface_points":analysis.closest_surface_points},
                                "instruction":"Correct the affected instance placement or establish an intended design interface, then inspect and slice the revision again."}));
                        }
                    }
                    Err(printable_geom::GeometryError::Cancelled | printable_geom::GeometryError::TimeLimit { .. }) => {
                        unresolved.insert("source_geometry_budget_exhausted".into());
                        break 'pairs;
                    }
                    Err(error) => {
                        unresolved.insert("source_pair_geometry_not_verified".into());
                        pairs.push(json!({"instances":[a_index,b_index],"build_indices":ids,
                            "state":"unmeasured","error":{"code":error.code(),"message":error.to_string()}}));
                    }
                }
            }
        }
        Ok(json!({"format_version":1,"scope":"authored_source_solids",
            "coordinate_frame":"primary_3mf_document","units":"millimetres",
            "primary_model":archive.primary_model,"xml_bytes_read":archive.xml_bytes_read,
            "native_metadata":archive.native,"different_plate_pairs_not_compared":different_plate_pairs,
            "instances":instances,"pairs":pairs,"findings":findings,"unresolved":unresolved,
            "state":if !findings.is_empty() {"failed"} else if !unresolved.is_empty() {"unmeasured"} else {"passed"},
            "limits":["Source hulls are conservative outlines; solid analysis establishes interference.",
                "Components within one authored build item may form an intended assembly.",
                "Generated support, brim, prime tower and moving hardware require final toolpath checks."]}))
    })
}

/// Compare retained authored instances with the exported native model. Stable
/// native identity and oriented geometry are required; matching bounds alone
/// cannot establish preservation. Unknown normalization remains unmeasured.
pub(super) fn compare_final(source: &Value, exported: &Value, automatic_transforms: bool) -> Value {
    use std::collections::BTreeMap;
    fn roots(report: &Value) -> (BTreeMap<u64, Vec<&Value>>, bool) {
        let Some(instances) = report["instances"].as_array() else {
            return (BTreeMap::new(), false);
        };
        let mut complete = !instances.is_empty();
        let mut builds = BTreeMap::<usize, Vec<&Value>>::new();
        for instance in instances {
            if let Some(build) = instance["build_index"]
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
            {
                builds.entry(build).or_default().push(instance);
            } else {
                complete = false;
            }
        }
        let mut roots = BTreeMap::new();
        for (build, instances) in builds {
            if let Some(identity) = root_identity(report, build) {
                roots.insert(identity, instances);
            } else {
                complete = false;
            }
        }
        (roots, complete)
    }
    fn signatures(instances: &[&Value], field: &str) -> Option<Vec<String>> {
        let mut result = instances
            .iter()
            .map(|i| i[field].as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        result.sort();
        Some(result)
    }
    fn transforms(instances: &[&Value]) -> Option<Vec<Vec<i64>>> {
        let mut result = instances
            .iter()
            .map(|i| {
                i["resource_transform_3mf"]
                    .as_array()?
                    .iter()
                    .map(|n| {
                        n.as_f64()
                            .filter(|v| v.is_finite())
                            .map(|v| (v * 1_000_000.).round() as i64)
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()?;
        result.sort();
        Some(result)
    }
    let compatible_frame = source["coordinate_frame"] == "primary_3mf_document"
        && exported["coordinate_frame"] == source["coordinate_frame"]
        && source["units"] == "millimetres"
        && exported["units"] == source["units"];
    let mut unresolved = BTreeSet::<String>::new();
    let mut findings = Vec::new();
    let mut instances = Vec::new();
    let mut requested_changes = Vec::new();
    let (before, before_complete) = roots(source);
    let (after, after_complete) = roots(exported);
    if !before_complete || !after_complete {
        unresolved.insert("native_identity_or_coordinate_mapping_not_verified".into());
    }
    match compatible_frame {
        true => {
            if before.keys().ne(after.keys()) {
                unresolved.insert("exported_instance_set_changed_or_missing".into());
            }
            for (identity, expected) in &before {
                let Some(actual) = after.get(identity) else {
                    continue;
                };
                let local = signatures(expected, "local_geometry_sha256");
                let local_matches =
                    local.is_some() && local == signatures(actual, "local_geometry_sha256");
                let world = signatures(expected, "world_geometry_sha256");
                let actual_world = signatures(actual, "world_geometry_sha256");
                let world_matches = world.is_some() && world == actual_world;
                let world_changed = world
                    .as_ref()
                    .zip(actual_world.as_ref())
                    .is_some_and(|(a, b)| a != b);
                let pose = transforms(expected);
                let actual_pose = transforms(actual);
                let pose_matches = pose.is_some() && pose == actual_pose;
                let pose_changed = pose
                    .as_ref()
                    .zip(actual_pose.as_ref())
                    .is_some_and(|(a, b)| a != b);
                let plates = |instances: &[&Value]| -> Option<BTreeSet<u64>> {
                    instances
                        .iter()
                        .map(|i| i["native_plate"].as_u64().filter(|p| *p > 0))
                        .collect()
                };
                let expected_plates = plates(expected);
                let actual_plates = plates(actual);
                let plates_known = expected_plates.as_ref().is_some_and(|p| p.len() == 1)
                    && actual_plates.as_ref().is_some_and(|p| p.len() == 1);
                let plate_matches = plates_known && expected_plates == actual_plates;
                let lock = source["native_metadata"]["plates"]
                    .as_array()
                    .and_then(|plates| {
                        plates
                            .iter()
                            .find(|p| p["plate"] == expected[0]["native_plate"])
                    })
                    .and_then(|p| p["locked"].as_bool());
                let preserve_transform = !automatic_transforms || lock == Some(true);
                let state = if local_matches
                    && (pose_changed || world_changed || (plates_known && !plate_matches))
                    && preserve_transform
                {
                    findings.push(json!({"kind":"native_instance_placement_changed","native_identify_id":identity,
                        "expected_build_indices":expected.iter().map(|i|&i["build_index"]).collect::<Vec<_>>(),
                        "actual_build_indices":actual.iter().map(|i|&i["build_index"]).collect::<Vec<_>>(),
                        "expected_transforms":expected.iter().map(|i|&i["resource_transform_3mf"]).collect::<Vec<_>>(),
                        "actual_transforms":actual.iter().map(|i|&i["resource_transform_3mf"]).collect::<Vec<_>>(),
                        "instruction":"Inspect the retained source and exported layout; correct or re-author the affected placement and slice again."}));
                    "failed"
                } else if local_matches && world_matches && pose_matches && plate_matches {
                    "passed"
                } else if local_matches
                    && automatic_transforms
                    && lock == Some(false)
                    && plates_known
                    && pose.is_some()
                    && world.is_some()
                    && transforms(actual).is_some()
                    && signatures(actual, "world_geometry_sha256").is_some()
                {
                    requested_changes.push(json!({"native_identify_id":identity,
                        "source_transforms":expected.iter().map(|i|&i["resource_transform_3mf"]).collect::<Vec<_>>(),
                        "exported_transforms":actual.iter().map(|i|&i["resource_transform_3mf"]).collect::<Vec<_>>()}));
                    "passed"
                } else {
                    unresolved.insert("native_geometry_normalization_not_verified".into());
                    "unmeasured"
                };
                instances.push(json!({"native_identify_id":identity,"state":state,
                    "local_geometry_matches":local_matches,"world_geometry_matches":world_matches,
                    "transform_matches":pose_matches,"plate_matches":plate_matches}));
            }
        }
        false => {
            unresolved.insert("native_identity_or_coordinate_mapping_not_verified".into());
        }
    }
    for report in [source, exported] {
        if report["unresolved"]
            .as_array()
            .is_none_or(|items| !items.is_empty())
        {
            unresolved.insert("source_or_exported_geometry_coverage_incomplete".into());
        }
    }
    json!({"state":if !findings.is_empty(){"failed"}else if unresolved.is_empty(){"passed"}else{"unmeasured"},
        "scope":"native_instance_geometry_and_placement","coordinate_grid_mm":0.0001,
        "automatic_transforms_requested":automatic_transforms,"requested_transform_changes":requested_changes,
        "transform_coefficient_grid":0.000001,"instances":instances,"findings":findings,"unresolved":unresolved,
        "authored_feature_preservation":"unverified_after_native_export",
        "material_assignment_preservation":"unverified_after_native_export",
        "limits":["Painted masks, opaque metadata and stable face identity are not established by geometry signatures.",
            "Source and private prepared archives remain retained; inspect native assignment evidence before relying on local material settings."]})
}

fn root_identity(report: &Value, build: usize) -> Option<u64> {
    let instances = report["instances"].as_array()?;
    let ids = instances
        .iter()
        .filter(|i| i["build_index"].as_u64() == Some(build as u64))
        .map(|i| i["native_identify_id"].as_u64().filter(|id| *id > 0))
        .collect::<Option<BTreeSet<_>>>()?;
    if ids.len() != 1 {
        return None;
    }
    let id = *ids.first()?;
    (!instances.iter().any(|i| {
        i["native_identify_id"].as_u64() == Some(id)
            && i["build_index"].as_u64() != Some(build as u64)
    }))
    .then_some(id)
}

/// Carry declared source interfaces through stable native identities. Build
/// indices may be renumbered by export, so copying the source indices is unsafe.
pub(super) fn apply_final_interfaces(
    source: &Value,
    exported: &mut Value,
    controls: Option<&super::placement::LayoutControls>,
) {
    use std::collections::BTreeMap;
    let Some(controls) = controls.filter(|c| !c.intended_interfaces.is_empty()) else {
        return;
    };
    let mapped = controls
        .intended_interfaces
        .iter()
        .map(|interface| {
            root_identity(source, interface.build_indices[0])
                .zip(root_identity(source, interface.build_indices[1]))
                .filter(|(a, b)| a != b)
                .map(|ids| (ids, interface))
        })
        .collect::<Vec<_>>();
    let source_mapping_incomplete = mapped.iter().any(Option::is_none);
    let identities = exported["instances"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            i["build_index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
        })
        .map(|build| (build, root_identity(exported, build)))
        .collect::<BTreeMap<_, _>>();
    let Some(pairs) = exported["pairs"].as_array_mut() else {
        return;
    };
    let mut accepted = BTreeSet::new();
    let mut unverified = BTreeSet::new();
    for (index, pair) in pairs.iter_mut().enumerate() {
        let pair_ids = pair["build_indices"]
            .as_array()
            .filter(|ids| ids.len() == 2)
            .and_then(|ids| {
                Some((
                    usize::try_from(ids[0].as_u64()?).ok()?,
                    usize::try_from(ids[1].as_u64()?).ok()?,
                ))
            })
            .and_then(|(a, b)| {
                Some((
                    identities.get(&a).copied().flatten()?,
                    identities.get(&b).copied().flatten()?,
                ))
            });
        let interface = pair_ids.and_then(|(a, b)| {
            mapped
                .iter()
                .flatten()
                .find(|((x, y), _)| (*x == a && *y == b) || (*x == b && *y == a))
        });
        if let Some((ids, interface)) = interface {
            pair["intended_interface"] = json!({"source_build_indices":interface.build_indices,
                "native_identify_ids":[ids.0,ids.1],"reason":interface.reason});
            if pair["solid_analysis"].is_object() {
                pair["state"] = json!("passed");
                accepted.insert(index);
            }
        } else if pair_ids.is_none() {
            pair["intended_interface_status"] = json!("native_mapping_not_verified");
            unverified.insert(index);
        }
    }
    if let Some(findings) = exported["findings"].as_array_mut() {
        findings.retain(|finding| {
            finding["evidence"]["pair_index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .is_none_or(|i| !accepted.contains(&i))
        });
    }
    if source_mapping_incomplete || !unverified.is_empty() {
        exported["unresolved"]
            .as_array_mut()
            .expect("layout unresolved reasons")
            .push(json!("intended_interface_native_mapping_not_verified"));
    }
    exported["state"] = json!(if exported["findings"]
        .as_array()
        .is_some_and(|f| !f.is_empty())
    {
        "failed"
    } else if exported["unresolved"]
        .as_array()
        .is_none_or(|r| !r.is_empty())
    {
        "unmeasured"
    } else {
        "passed"
    });
}

/// Native coordinate settings can disprove preservation, but their presence
/// alone cannot prove the generated tower's geometric position.
pub(super) fn check_requested_towers(
    preservation: &mut Value,
    exported: &Value,
    controls: Option<&super::placement::LayoutControls>,
) {
    let Some(controls) = controls.filter(|c| !c.tower_positions.is_empty()) else {
        return;
    };
    let settings = &exported["native_metadata"]["project_settings"];
    let mut checks = Vec::new();
    for requested in &controls.tower_positions {
        let plate_known = exported["native_metadata"]["plates"]
            .as_array()
            .is_some_and(|plates| {
                plates
                    .iter()
                    .any(|p| p["plate"].as_u64() == Some(u64::from(requested.plate)))
            });
        let coordinate = |key: &str| {
            settings[key]
                .as_array()?
                .get(usize::from(requested.plate) - 1)?
                .as_str()?
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
        };
        let actual = if plate_known {
            coordinate("wipe_tower_x")
                .zip(coordinate("wipe_tower_y"))
                .map(|(x, y)| [x, y])
        } else {
            None
        };
        let mismatch = actual.is_some_and(|actual| {
            (0..2).any(|axis| (actual[axis] - requested.position_mm[axis]).abs() > 1e-6)
        });
        if mismatch {
            preservation["findings"].as_array_mut().expect("comparison findings").push(json!({
                "kind":"native_tower_position_setting_changed","plate":requested.plate,
                "expected_bed_local_xy_mm":requested.position_mm,"actual_bed_local_xy_mm":actual,
                "evidence":"final_native_project_coordinate_vectors",
                "instruction":"Inspect the exported plate and tower paths; correct the placement and slice a new revision."}));
            preservation["state"] = json!("failed");
        }
        checks.push(json!({"plate":requested.plate,"requested_bed_local_xy_mm":requested.position_mm,
            "native_reported_bed_local_xy_mm":actual,"native_setting_state":if mismatch{"failed"}else if actual.is_some(){"passed"}else{"unmeasured"},
            "generated_position_state":"unmeasured"}));
    }
    preservation["requested_towers"] = json!(checks);
    preservation["unresolved"]
        .as_array_mut()
        .expect("comparison unresolved reasons")
        .push(json!(
            "requested_tower_position_geometric_effect_not_verified"
        ));
    if preservation["state"] == "passed" {
        preservation["state"] = json!("unmeasured");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cubes(offsets: &[f64], component_pair: bool) -> tempfile::NamedTempFile {
        cubes_with_reflection(offsets, component_pair, false)
    }

    fn cubes_with_reflection(
        offsets: &[f64],
        component_pair: bool,
        reflect_first: bool,
    ) -> tempfile::NamedTempFile {
        let vertices = [
            [0., 0., 0.],
            [2., 0., 0.],
            [2., 2., 0.],
            [0., 2., 0.],
            [0., 0., 2.],
            [2., 0., 2.],
            [2., 2., 2.],
            [0., 2., 2.],
        ];
        let triangles = [
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ];
        let mut geometry = String::from("<object id='1' pid='4' pindex='0'><mesh><vertices>");
        for [x, y, z] in vertices {
            geometry.push_str(&format!("<vertex x='{x}' y='{y}' z='{z}'/>"));
        }
        geometry.push_str("</vertices><triangles>");
        for [a, b, c] in triangles {
            geometry.push_str(&format!(
                "<triangle v1='{a}' v2='{b}' v3='{c}' pid='4' p1='1'/>"
            ));
        }
        geometry.push_str("</triangles></mesh></object>");
        let mut build = String::new();
        if component_pair {
            geometry.push_str("<object id='2'><components><component objectid='1'/><component objectid='1' transform='1 0 0 0 1 0 0 0 1 1 0 0'/></components></object>");
            build.push_str("<item objectid='2'/>");
        } else {
            for (index, x) in offsets.iter().enumerate() {
                let reflected = reflect_first && index == 0;
                let scale = if reflected { -1 } else { 1 };
                let translation = x + if reflected { 2. } else { 0. };
                build.push_str(&format!(
                    "<item objectid='1' transform='{scale} 0 0 0 1 0 0 0 1 {translation} 0 0'/>"
                ));
            }
        }
        let model = format!(
            "<model xmlns='http://schemas.microsoft.com/3dmanufacturing/core/2015/02' unit='millimeter'><resources>{geometry}</resources><build>{build}</build></model>"
        );
        let relationships = b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='model' Type='http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel' Target='/3D/3dmodel.model'/></Relationships>";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&super::super::package::fixture::package(&[
            ("_rels/.rels", relationships),
            ("3D/3dmodel.model", model.as_bytes()),
            ("Metadata/unknown.config", b"preserve authored data"),
        ]))
        .unwrap();
        file
    }

    #[test]
    fn overlapping_source_instances_produce_positive_solid_findings() {
        let source = cubes(&[0., 1., 8.], false);
        let report = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        assert_eq!(report["state"], "failed");
        assert_eq!(report["findings"].as_array().unwrap().len(), 1);
        assert_eq!(report["findings"][0]["kind"], "source_part_interference");
        assert_eq!(report["findings"][0]["build_indices"], json!([0, 1]));
        assert!(
            (report["findings"][0]["evidence"]["interference_volume_mm3"]
                .as_f64()
                .unwrap()
                - 4.)
                .abs()
                < 1e-6
        );
        assert_eq!(report["instances"][1]["resource"]["object_id"], 1);
        assert_eq!(report["instances"][1]["copy"], 1);
    }

    #[test]
    fn reflected_source_solids_retain_positive_interference_and_property_defaults() {
        let source = cubes_with_reflection(&[0., 1., 8.], false, true);
        let report = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        assert_eq!(report["state"], "failed");
        assert_eq!(report["findings"].as_array().unwrap().len(), 1);
        assert_eq!(report["findings"][0]["kind"], "source_part_interference");
        assert_eq!(report["findings"][0]["build_indices"], json!([0, 1]));
        assert!(
            (report["findings"][0]["evidence"]["interference_volume_mm3"]
                .as_f64()
                .unwrap()
                - 4.)
                .abs()
                < 1e-6
        );
        assert_eq!(
            report["instances"][0]["declared_object_property_defaults"],
            json!({"pid":4,"pindex":0})
        );
        assert_eq!(report["instances"][0]["property_groups"], json!([4]));
        assert_eq!(report["instances"][0]["property_indices"], json!([0, 1]));
    }

    #[test]
    fn unsupported_required_geometry_cannot_establish_definite_solid_interference() {
        use std::io::Read;
        let source = cubes(&[0., 1.], false);
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(std::fs::read(source.path()).unwrap()))
                .unwrap();
        let mut relationships = Vec::new();
        archive
            .by_name("_rels/.rels")
            .unwrap()
            .read_to_end(&mut relationships)
            .unwrap();
        let mut model = String::new();
        archive
            .by_name("3D/3dmodel.model")
            .unwrap()
            .read_to_string(&mut model)
            .unwrap();
        let model = model.replace(
            "<model xmlns=",
            "<model xmlns:future='urn:unverified-geometry' requiredextensions='future' xmlns=",
        );
        std::fs::write(
            source.path(),
            super::super::package::fixture::package(&[
                ("_rels/.rels", &relationships),
                ("3D/3dmodel.model", model.as_bytes()),
            ]),
        )
        .unwrap();
        let report = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        assert_eq!(report["state"], "unmeasured");
        assert!(report["findings"].as_array().unwrap().is_empty());
        assert_eq!(report["pairs"][0]["state"], "unsupported");
        assert!(
            report["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "required_model_extensions_not_verified")
        );
    }

    #[test]
    fn gaps_and_intended_interfaces_keep_their_distinct_meaning() {
        let source = cubes(&[0., 2.5], false);
        let options = InspectOptions {
            minimum_gap_mm: 1.,
            ..Default::default()
        };
        let report = inspect_source(source.path(), &options).unwrap();
        assert_eq!(
            report["findings"][0]["kind"],
            "source_gap_below_requirement"
        );
        assert_eq!(report["pairs"][0]["solid_analysis"]["clearance_mm"], 0.5);
        let source = cubes(&[0., 1.], false);
        let options = InspectOptions {
            intended_interfaces: vec![IntendedInterface {
                build_indices: [0, 1],
                reason: "Deliberate fused design interface".into(),
            }],
            ..Default::default()
        };
        let report = inspect_source(source.path(), &options).unwrap();
        assert_eq!(report["state"], "passed");
        assert_eq!(
            report["pairs"][0]["solid_analysis"]["relation"],
            "interfering"
        );
        assert!(report["pairs"][0]["intended_interface"]["reason"].is_string());
        let source = cubes(&[], true);
        let report = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        assert!(report["pairs"].as_array().unwrap().is_empty());
        assert!(report["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn exhausted_pairs_keep_positive_findings_and_unresolved_coverage() {
        let source = cubes(&[0., 1., 2.], false);
        let options = InspectOptions {
            max_pairs: 1,
            ..Default::default()
        };
        let report = inspect_source(source.path(), &options).unwrap();
        assert_eq!(report["state"], "failed");
        assert_eq!(report["findings"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["unresolved"],
            json!(["source_pair_budget_exhausted"])
        );
        assert!(
            InspectOptions {
                minimum_gap_mm: f64::NAN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn final_verification_requires_identity_geometry_and_pose() {
        let source = cubes(&[0., 8.], false);
        let mut before = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in before["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
            instance["native_plate"] = json!(1);
        }
        assert_eq!(compare_final(&before, &before, false)["state"], "passed");
        let mut after = before.clone();
        after["instances"][0]["resource_transform_3mf"][9] = json!(5.);
        // Geometry/bounds left equal deliberately: a symmetric model is not
        // evidence that a changed requested orientation was preserved.
        let comparison = compare_final(&before, &after, false);
        assert_eq!(comparison["state"], "failed");
        assert_eq!(comparison["findings"][0]["native_identify_id"], 100);
        after = before.clone();
        after["instances"][0]["native_identify_id"] = Value::Null;
        assert_eq!(compare_final(&before, &after, false)["state"], "unmeasured");
        after = before.clone();
        after["instances"][0]["local_geometry_sha256"] = json!("normalized geometry");
        assert_eq!(compare_final(&before, &after, false)["state"], "unmeasured");
        after = before.clone();
        after["instances"][1]["native_identify_id"] = json!(100);
        assert_eq!(compare_final(&before, &after, false)["state"], "unmeasured");
        after = before.clone();
        after["instances"][0]["native_plate"] = Value::Null;
        assert_eq!(compare_final(&before, &after, false)["state"], "unmeasured");
        after = before.clone();
        after["instances"][0]["resource_transform_3mf"] = Value::Null;
        after["instances"][0]["world_geometry_sha256"] = Value::Null;
        assert_eq!(compare_final(&before, &after, false)["state"], "unmeasured");
    }

    #[test]
    fn incomplete_identity_mapping_retains_known_locked_placement_violations() {
        let source = cubes(&[0., 8., 16.], false);
        let mut before = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in before["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
            instance["native_plate"] = json!(1);
        }
        before["native_metadata"]["plates"] = json!([{"plate":1,"locked":true}]);
        for ambiguous in [false, true] {
            let mut after = before.clone();
            after["instances"][0]["resource_transform_3mf"][9] = json!(5.);
            if ambiguous {
                after["instances"][1]["native_identify_id"] = json!(102);
            } else {
                after["instances"][1]["native_identify_id"] = Value::Null;
            }
            let comparison = compare_final(&before, &after, true);
            assert_eq!(comparison["state"], "failed");
            assert_eq!(comparison["findings"][0]["native_identify_id"], 100);
            assert!(
                comparison["unresolved"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("native_identity_or_coordinate_mapping_not_verified"))
            );
        }
    }

    #[test]
    fn requested_automatic_moves_are_observations_and_locked_placements_remain_required() {
        let source = cubes(&[0., 8.], false);
        let mut before = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in before["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
            instance["native_plate"] = json!(1);
        }
        before["native_metadata"]["plates"] = json!([{"plate":1,"locked":false}]);
        let moved = cubes(&[0., 12.], false);
        let mut after = inspect_source(moved.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in after["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
            instance["native_plate"] = json!(1);
        }
        let result = compare_final(&before, &after, true);
        assert_eq!(result["state"], "passed");
        assert!(result["findings"].as_array().unwrap().is_empty());
        assert_eq!(
            result["requested_transform_changes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(compare_final(&before, &after, false)["state"], "failed");
        before["native_metadata"]["plates"][0]["locked"] = json!(true);
        assert_eq!(compare_final(&before, &after, true)["state"], "failed");
        before["native_metadata"]["plates"][0]["locked"] = Value::Null;
        assert_eq!(compare_final(&before, &after, true)["state"], "unmeasured");
    }

    #[test]
    fn final_interfaces_follow_native_identity_instead_of_reusing_build_numbers() {
        let source = cubes(&[0., 1., 1.5], false);
        let mut before = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in before["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
        }
        let controls = super::super::placement::LayoutControls {
            source_sha256: "a".repeat(64),
            placements: vec![],
            tower_positions: vec![],
            minimum_gap_mm: None,
            intended_interfaces: vec![IntendedInterface {
                build_indices: [0, 1],
                reason: "Declared fused material interface".into(),
            }],
        };
        let mut after = before.clone();
        for instance in after["instances"].as_array_mut().unwrap() {
            let build = instance["build_index"].as_u64().unwrap();
            instance["build_index"] = json!(10 + build);
        }
        for pair in after["pairs"].as_array_mut().unwrap() {
            for build in pair["build_indices"].as_array_mut().unwrap() {
                *build = json!(10 + build.as_u64().unwrap());
            }
        }
        for finding in after["findings"].as_array_mut().unwrap() {
            for build in finding["build_indices"].as_array_mut().unwrap() {
                *build = json!(10 + build.as_u64().unwrap());
            }
        }
        apply_final_interfaces(&before, &mut after, Some(&controls));
        assert_eq!(after["state"], "failed");
        assert_eq!(after["pairs"][0]["state"], "passed");
        assert_eq!(
            after["pairs"][0]["intended_interface"]["native_identify_ids"],
            json!([100, 101])
        );
        assert_eq!(after["findings"].as_array().unwrap().len(), 2);
        let mut unknown = before.clone();
        unknown["instances"][0]["native_identify_id"] = Value::Null;
        let mut after = before.clone();
        apply_final_interfaces(&unknown, &mut after, Some(&controls));
        assert_eq!(after["state"], "failed");
        assert_eq!(after["findings"].as_array().unwrap().len(), 3);
        assert!(
            after["unresolved"]
                .as_array()
                .unwrap()
                .contains(&json!("intended_interface_native_mapping_not_verified"))
        );
    }

    #[test]
    fn unmapped_interface_does_not_hide_unrelated_known_final_collision() {
        let source = cubes(&[0., 10., 20., 20.5], false);
        let mut before = inspect_source(source.path(), &InspectOptions::default()).unwrap();
        for (index, instance) in before["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            instance["native_identify_id"] = json!(100 + index);
        }
        let mut after = before.clone();
        before["instances"][0]["native_identify_id"] = Value::Null;
        let controls = super::super::placement::LayoutControls {
            source_sha256: "a".repeat(64),
            placements: vec![],
            tower_positions: vec![],
            minimum_gap_mm: None,
            intended_interfaces: vec![IntendedInterface {
                build_indices: [0, 1],
                reason: "Declared design interface".into(),
            }],
        };
        apply_final_interfaces(&before, &mut after, Some(&controls));
        assert_eq!(after["state"], "failed");
        assert!(
            after["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["build_indices"] == json!([2, 3]))
        );
        assert!(
            after["pairs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pair| pair["build_indices"] == json!([2, 3]) && pair["state"] == "failed")
        );
        assert!(!after["unresolved"].as_array().unwrap().is_empty());
        after["instances"][2]["native_identify_id"] = Value::Null;
        apply_final_interfaces(&before, &mut after, Some(&controls));
        assert_eq!(after["state"], "failed");
        assert!(
            after["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["build_indices"] == json!([2, 3]))
        );
        assert!(
            after["pairs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pair| pair["build_indices"] == json!([2, 3])
                    && pair["intended_interface_status"] == "native_mapping_not_verified")
        );
    }

    #[test]
    fn requested_tower_settings_do_not_substitute_for_generated_position_evidence() {
        let controls = super::super::placement::LayoutControls {
            source_sha256: "a".repeat(64),
            placements: vec![],
            tower_positions: vec![super::super::placement::TowerPosition {
                plate: 2,
                position_mm: [20., 30.],
            }],
            minimum_gap_mm: None,
            intended_interfaces: Vec::new(),
        };
        let exported = json!({"native_metadata":{"plates":[{"plate":2}],"project_settings":{
            "wipe_tower_x":["10","20"],"wipe_tower_y":["15","30"]}}});
        let baseline = json!({"state":"passed","findings":[],"unresolved":[]});
        let mut result = baseline.clone();
        check_requested_towers(&mut result, &exported, Some(&controls));
        assert_eq!(result["state"], "unmeasured");
        assert_eq!(
            result["requested_towers"][0]["native_setting_state"],
            "passed"
        );
        assert_eq!(
            result["requested_towers"][0]["generated_position_state"],
            "unmeasured"
        );
        let mut changed = exported.clone();
        changed["native_metadata"]["project_settings"]["wipe_tower_y"][1] = json!("31");
        result = baseline.clone();
        check_requested_towers(&mut result, &changed, Some(&controls));
        assert_eq!(result["state"], "failed");
        assert_eq!(result["findings"][0]["plate"], 2);
        result = baseline;
        check_requested_towers(&mut result, &Value::Null, Some(&controls));
        assert_eq!(result["state"], "unmeasured");
        assert!(result["findings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn public_cancellation_stops_inspection_without_publishing_and_releases_admission() {
        use axum::{Json, Router, routing::post};
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("layout-child");
        std::fs::write(&binary,b"#!/usr/bin/python3\nimport os,pathlib,time\npathlib.Path(__file__).with_name('layout-started').write_text(str(os.getpid()))\ntime.sleep(60)\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut worker = super::super::tests::fixture(root.path(), false);
        Arc::get_mut(&mut worker).unwrap().geometry_worker_bin = Some(binary);
        let source = cubes(&[0., 3.], false);
        worker
            .workspace
            .write_artifact(
                "projects/part/layout.3mf",
                &std::fs::read(source.path()).unwrap(),
                false,
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let routed = Arc::clone(&worker);
        let app = Router::new().route(
            "/slice",
            post(move |Json(request): Json<super::super::SliceRequest>| {
                let worker = Arc::clone(&routed);
                async move {
                    Json(
                        worker
                            .dispatch(request)
                            .await
                            .unwrap_or_else(|error| json!({"error":error.to_string()})),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let token = cancellation.clone();
        let params = InspectParams {
            project_id: "part".into(),
            source: "layout.3mf".into(),
            output_dir: "inspection".into(),
            expected_source_sha256: None,
            options: InspectOptions::default(),
        };
        let call = tokio::spawn(crate::tools::REQUEST_CANCELLATION.scope(token, async move {
            super::super::forward(
                Some(&endpoint),
                super::super::SliceRequest::InspectLayout(params),
            )
            .await
        }));
        let started = root.path().join("layout-started");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("inspection must reach the isolated geometry child");
        let pid: i32 = std::fs::read_to_string(started).unwrap().parse().unwrap();
        cancellation.cancel();
        assert!(
            call.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(pid, 0) } == 0 || worker.admission.available_permits() != 1 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelled inspection must stop its child and release admission promptly");
        assert!(!root.path().join("projects/part/inspection").exists());
        server.abort();
    }

    #[tokio::test]
    async fn uncooperative_layout_child_is_stopped_at_the_parent_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("layout-child");
        std::fs::write(&binary,b"#!/usr/bin/python3\nimport os,pathlib,sys,time\npathlib.Path(sys.argv[3]).with_suffix('.pid').write_text(str(os.getpid()))\ntime.sleep(60)\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut worker = super::super::tests::fixture(directory.path(), false);
        std::sync::Arc::get_mut(&mut worker)
            .unwrap()
            .geometry_worker_bin = Some(binary);
        let staging = directory.path().join("layout-staging");
        std::fs::create_dir(&staging).unwrap();
        let options = InspectOptions {
            timeout_seconds: 1,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let error = run_worker(
            &worker,
            &directory.path().join("unused.3mf"),
            &staging,
            &options,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeded its time budget"));
        assert!(started.elapsed() < Duration::from_secs(12));
        let pid = std::fs::read_to_string(staging.join("source-layout.pid")).unwrap();
        let mut alive = true;
        for _ in 0..20 {
            alive = std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "timed-out layout child remained alive");
    }
}
