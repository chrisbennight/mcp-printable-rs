use super::*;
use crate::projects::{CreateParams, ProjectRequest};
use std::os::unix::fs::PermissionsExt;

fn fixture(root: &Path, delay: bool) -> Arc<SliceWorker> {
    let workspace = Arc::new(Workspace::open(Some(root), None).unwrap());
    projects::dispatch(
        &workspace,
        ProjectRequest::Create(CreateParams {
            project_id: "part".into(),
            name: "Part".into(),
            description: String::new(),
            adopt_existing: false,
        }),
    )
    .unwrap();
    workspace
        .write_artifact(
            "projects/part/source.stl",
            b"retained original model",
            false,
        )
        .unwrap();
    let profiles = root.join("profiles");
    for (category, name, fields) in [
        (
            "machine",
            "printer",
            json!({"machine_start_gcode":"G28","nozzle_diameter":["0.4"]}),
        ),
        (
            "process",
            "process",
            json!({"compatible_printers":["printer"],"layer_height":"0.2"}),
        ),
        (
            "filament",
            "filament",
            json!({"compatible_printers":["printer"],"filament_type":["PLA"],"textured_plate_temp":["65"],"textured_plate_temp_initial_layer":["65"],"hot_plate_temp":["55"],"hot_plate_temp_initial_layer":["55"]}),
        ),
    ] {
        std::fs::create_dir_all(profiles.join(category)).unwrap();
        let mut profile = fields;
        profile["type"] = json!(category);
        profile["name"] = json!(name);
        profile["instantiation"] = json!("true");
        std::fs::write(
            profiles.join(category).join("profile.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(
        profiles.join("filament/filaments_color_codes.json"),
        br#"{"data":[],"total":0}"#,
    )
    .unwrap();
    let binary = root.join("fake-slicer");
    let script = if delay {
        "#!/usr/bin/python3\nimport sys,time\nwith open(sys.argv[sys.argv.index('--pipe')+1], 'w') as progress:\n progress.write('{\"message\":\"Generating supports\",\"total_percent\":25,\"plate_percent\":30,\"plate_index\":1,\"plate_count\":1}\\n')\n progress.flush()\n time.sleep(60)\n"
    } else {
        "#!/usr/bin/python3\nimport sys,pathlib\np=pathlib.Path(sys.argv[sys.argv.index('--outputdir')+1])\n(p/'model.gcode.3mf').write_bytes(b'fake3mf')\n(p/'plate_1.gcode').write_text('M83\\n; CHANGE_LAYER\\n; FEATURE: Outer wall\\nG1 X10 E1\\n')\n"
    };
    std::fs::write(&binary, script).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    Arc::new(SliceWorker::new(
        workspace,
        Profiles::load(&profiles).unwrap(),
        binary,
    ))
}

fn params() -> PrepareParams {
    serde_json::from_value(json!({"project_id":"part","source":"source.stl","output_dir":"slice","build_plate":"Textured PEI Plate",
        "printer":{"name":"printer"},"process":{"name":"process"},"filaments":[{"name":"filament"}]})).unwrap()
}

#[tokio::test]
async fn readiness_verifies_the_native_interface_profiles_and_busy_state() {
    use crate::worker_health::CapabilityState;
    let directory = tempfile::tempdir().unwrap();
    let worker = fixture(directory.path(), false);
    std::fs::write(
        &worker.binary,
        "#!/bin/sh\nprintf 'OrcaSlicer-2.4.2:\\n--slice --export-3mf\\n'\n",
    )
    .unwrap();
    let startup = worker.probe_engine().await;
    assert_eq!(startup.state, CapabilityState::Ready);
    assert_eq!(startup.profile_counts.unwrap().printer, 1);
    let permit = worker.admission.acquire().await.unwrap();
    assert_eq!(worker.readiness(&startup).state, CapabilityState::Busy);
    drop(permit);
    assert_eq!(worker.readiness(&startup).state, CapabilityState::Ready);
    for script in [
        "#!/bin/sh\nprintf 'different interface\\n'\n",
        "#!/bin/sh\nprintf 'OrcaSlicer-2.4.20:\\n--slice --export-3mf\\n'\n",
    ] {
        std::fs::write(&worker.binary, script).unwrap();
        assert_eq!(
            worker.probe_engine().await.state,
            CapabilityState::Incompatible
        );
    }
}
fn handle() -> SliceHandle {
    SliceHandle {
        project_id: "part".into(),
        output_dir: "slice".into(),
    }
}

async fn terminal(worker: &Arc<SliceWorker>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = worker.status(handle(), false).await.unwrap();
            if state["status"] != "running" {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn completion_retains_source_and_rejects_duplicate_or_changed_review() {
    let directory = tempfile::tempdir().unwrap();
    let worker = fixture(directory.path(), false);
    let accepted = worker.prepare(params()).await.unwrap();
    assert_eq!(accepted["status"], "running");
    assert_eq!(accepted["setup"]["build_plate"], "Textured PEI Plate");
    assert_eq!(
        accepted["setup"]["filaments"][0]["bed_temperature_initial_layer"],
        json!(["65"])
    );
    let completed = terminal(&worker).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    assert!(worker.prepare(params()).await.is_err());
    let (_, source) = worker
        .workspace
        .read_artifact("projects/part/slice/source.stl")
        .unwrap();
    assert_eq!(source, b"retained original model");
    let request = || review::ReviewParams {
        slice: handle(),
        toolpath: "plate_1.gcode".into(),
        first_layer: 1,
        last_layer: 1,
        features: vec![],
        material: None,
        include_travel: false,
        size: 128,
    };
    let reviewed = worker.review(request()).await.unwrap();
    assert!(reviewed["segments"].as_u64().unwrap() > 0);
    let (_, png) = worker
        .workspace
        .read_artifact(reviewed["image"]["path"].as_str().unwrap())
        .unwrap();
    let image = image::load_from_memory(&png).unwrap().to_rgb8();
    let background = image.get_pixel(0, 0);
    assert!(image.pixels().any(|pixel| pixel != background));
    worker
        .workspace
        .write_artifact("projects/part/slice/plate_1.gcode", b"changed", true)
        .unwrap();
    assert!(worker.review(request()).await.is_err());
    let restarted = Arc::new(SliceWorker::new(
        Arc::clone(&worker.workspace),
        Profiles::load(&directory.path().join("profiles")).unwrap(),
        directory.path().join("fake-slicer"),
    ));
    assert_eq!(
        restarted.status(handle(), false).await.unwrap()["status"],
        "completed"
    );
}

#[test]
fn settings_discovery_and_surface_selection_use_profile_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let worker = fixture(directory.path(), false);
    let mut request = params();
    request.build_plate = setup::BuildPlate::HighTemp;
    request.filaments[0]
        .overrides
        .insert("hot_plate_temp_initial_layer".into(), json!(["60"]));
    let query = profiles::SettingsQuery {
        category: Category::Filament,
        profile: request.filaments[0].clone(),
        query: "hot_plate_temp".into(),
        offset: 0,
        limit: 1,
    };
    let discovered = worker.profiles.settings(query).unwrap();
    assert_eq!(discovered["total"], 2);
    assert_eq!(discovered["next_offset"], 1);
    assert_eq!(discovered["settings"][0]["key"], "hot_plate_temp");
    assert_eq!(discovered["settings"][0]["overrideable"], true);
    let filament = worker
        .profiles
        .resolve(Category::Filament, &request.filaments[0])
        .unwrap();
    let summary = setup::summary(&request, &json!({}), &json!({}), &[filament]);
    assert_eq!(summary["project_plate"], 1);
    assert_eq!(
        summary["filaments"][0]["bed_temperature_initial_layer"],
        json!(["60"])
    );
}

#[tokio::test]
async fn cancellation_releases_admission_and_unknown_running_state_is_interrupted() {
    let directory = tempfile::tempdir().unwrap();
    let worker = fixture(directory.path(), true);
    worker.prepare(params()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = worker.status(handle(), false).await.unwrap();
            if state["progress"]["total_percent"] == 25.0 {
                assert_eq!(state["status"], "running");
                assert_eq!(state["progress"]["message"], "Generating supports");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut next = params();
    next.output_dir = "second".into();
    assert!(worker.prepare(next).await.is_err());
    worker
        .workspace
        .write_artifact(
            "projects/part/slice/state.json",
            b"edited invalid state",
            true,
        )
        .unwrap();
    assert_eq!(
        worker.status(handle(), false).await.unwrap()["status"],
        "running"
    );
    assert_eq!(
        worker.status(handle(), true).await.unwrap()["cancel_requested"],
        true
    );
    assert_eq!(terminal(&worker).await["status"], "cancelled");
    assert_eq!(worker.admission.available_permits(), 1);
    let interrupted = json!({"id":"orphan","status":"running","slice":handle()});
    worker
        .workspace
        .write_artifact(
            "projects/part/slice/state.json",
            &serde_json::to_vec(&interrupted).unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(
        worker.status(handle(), false).await.unwrap()["status"],
        "interrupted"
    );
}
