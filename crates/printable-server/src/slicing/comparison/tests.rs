use super::*;
use crate::projects::{CreateParams, ProjectRequest};

fn workspace(root: &std::path::Path) -> Workspace {
    let ws = Workspace::open(Some(root), None).unwrap();
    projects::dispatch(
        &ws,
        ProjectRequest::Create(CreateParams {
            project_id: "part".into(),
            name: "Part".into(),
            description: String::new(),
            adopt_existing: false,
        }),
    )
    .unwrap();
    ws.write_artifact("projects/part/source.stl", b"original design", false)
        .unwrap();
    ws
}

fn candidate(
    ws: &Workspace,
    name: &str,
    design: &[u8],
    process: Value,
    material: &str,
) -> Candidate {
    let output = format!("projects/part/{name}");
    ws.create_public_directory(&output).unwrap();
    ws.write_generated_bytes(&format!("{output}/source.stl"), design, false)
        .unwrap();
    let mut artifacts = json!({});
    for (filename,bytes) in [("model.gcode.3mf",b"native package fixture".as_slice()),("plate_1.gcode",
        b"; estimated printing time (normal mode) = 1m 2s\n; filament used [g] = 0, 2.4\nG90\nG1 X1\n".as_slice())] {
        let artifact=ws.write_generated_bytes(&format!("{output}/{filename}"),bytes,false).unwrap();
        artifacts[filename]=json!({"artifact":artifact,"sha256":provenance::digest(bytes)});
    }
    let settings = json!({"build_plate":"Textured PEI Plate","printer":{"name":"fixture", "nozzle_diameter":["0.4"]},
        "process":process,"filaments":[{"filament_type":material}],"filament_colours":["#ABCDEF"]});
    let slice = SliceHandle {
        project_id: "part".into(),
        output_dir: name.into(),
    };
    let record=provenance::store(ws,Kind::Slice,"part",json!({"slice":slice,
        "source":"projects/part/source.stl","source_sha256":provenance::digest(design),
        "request":{"source":"source.stl","plate":1,"layout":{"placements":[{"build_index":0,"transform_3mf":[1,0,0,0,1,0,0,0,1,3,4,0]}]}},
        "settings_sha256":provenance::digest(&serde_json::to_vec(&settings).unwrap()),"settings":settings,
        "engine":{"name":"OrcaSlicer","version":super::super::ENGINE_VERSION,"source_revision":super::super::ENGINE_SOURCE_REVISION},
        "artifacts":artifacts,"preflight":{"status":"unresolved"},"final_layout":{"state":"unmeasured"}})).unwrap();
    let image = ws
        .write_reserved_artifact(
            &format!(".printable/evidence/review_media/{name}.png"),
            b"image fixture",
            false,
        )
        .unwrap();
    let review=provenance::store(ws,Kind::ToolpathReview,"part",json!({"slice":slice,"slice_provenance":record,
        "source_sha256":artifacts["plate_1.gcode"]["sha256"],"toolpath":format!("{output}/plate_1.gcode"),
        "image":{"artifact":image,"sha256":provenance::digest(b"image fixture")},"layers":[1,2],"features":["Outer wall"]})).unwrap();
    Candidate {
        label: name.into(),
        slice,
        slice_provenance: record,
        rationale: "Retain a controlled baseline or motivated candidate".into(),
        reviews: vec![review],
        conditions: BTreeMap::from([(
            "material_conditioning".into(),
            "same declared conditioning; not measured".into(),
        )]),
    }
}

fn params(ws: &Workspace) -> CompareParams {
    CompareParams {
        project_id: "part".into(),
        output_dir: "comparison".into(),
        objective: Objective::StrengthInUse,
        critical_faces: vec!["Fitting rim must retain its dimensions".into()],
        load_assumptions: vec!["Bending across the long arm".into()],
        baseline: candidate(
            ws,
            "baseline",
            b"original design",
            json!({"layer_height":"0.2","wall_loops":"2"}),
            "PLA",
        ),
        candidates: vec![candidate(
            ws,
            "candidate",
            b"original design",
            json!({"layer_height":"0.2","wall_loops":"3"}),
            "PLA",
        )],
    }
}

#[test]
fn native_bambu_estimates_keep_normal_total_and_reject_ambiguous_headers() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let budget = Budget {
        remaining: 1024,
        deadline: Instant::now() + Duration::from_secs(30),
        cancellation: CancellationToken::new(),
    };
    let normal = "; model printing time: 1m 46s; total estimated time: 2m 16s\n";
    let additional = "; model printing time: 3m; total estimated time: 3m 30s\n";
    std::fs::write(
        file.path(),
        format!("{normal}{additional}; filament used [g] = 0.45, 0.00\n"),
    )
    .unwrap();
    let result = estimates(file.path(), &budget).unwrap();
    assert_eq!(result["time_native"], "2m 16s");
    assert_eq!(result["filament_grams_by_slot_native"], "0.45, 0.00");
    for headers in [
        format!("{normal}; estimated printing time (normal mode) = 1m\n"),
        format!("; estimated printing time (normal mode) = 1m\n{normal}"),
        format!("{normal}{additional}{additional}"),
        "; model printing time: ; total estimated time: 1m\n".to_owned(),
        "; estimated printing time (normal mode) = 1m\n; estimated printing time (normal mode) = 2m\n".to_owned(),
    ] {
        std::fs::write(file.path(), headers).unwrap();
        assert!(estimates(file.path(), &budget).is_err());
    }
}

#[test]
fn comparison_retains_baseline_transforms_reviews_unknowns_and_restart_readback() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path());
    let params = params(&ws);
    // Mutable presentation files cannot redefine sealed candidate evidence.
    ws.write_generated_bytes("projects/part/baseline/state.json", b"{}", false)
        .unwrap();
    ws.write_generated_bytes("projects/part/baseline/settings.json", b"{}", false)
        .unwrap();
    let original = ws
        .snapshot_artifact("projects/part/baseline/plate_1.gcode")
        .unwrap();
    let original_hash = hash_file(original.path()).unwrap();
    let result = compare(&ws, params, CancellationToken::new()).unwrap();
    for uri in [
        "printable://contracts/slice",
        "printable://contracts/slice/compare",
    ] {
        let contract = crate::resources::contracts::read(
            uri,
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        assert!(
            jsonschema::validator_for(&contract["outputSchema"])
                .unwrap()
                .is_valid(&serde_json::to_value(&result).unwrap()),
            "{uri} rejects a successful comparison"
        );
    }
    assert_eq!(result.automatic_selection, "not_performed");
    let estimate = &result.baseline.estimates["plate_1.gcode"];
    assert_eq!(estimate["time_native"], "1m 2s");
    assert_eq!(estimate["filament_grams_by_slot_native"], "0, 2.4");
    for key in ["support_grams", "tower_grams", "flushing_grams"] {
        assert!(estimate[key].is_null());
    }
    assert!(
        result.candidates[0]
            .compatibility
            .iter()
            .any(|v| v == "shell_geometry_setting_changed:wall_loops")
    );
    let path = result.report_artifact["path"].as_str().unwrap();
    let bytes = std::fs::read(ws.snapshot_artifact(path).unwrap().path()).unwrap();
    assert_eq!(provenance::digest(&bytes), result.report_sha256);
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        report["baseline"]["request"]["layout"]["placements"][0]["transform_3mf"][9],
        3
    );
    assert_eq!(
        report["candidates"][0]["reviews"][0]["evidence"]["features"],
        json!(["Outer wall"])
    );
    assert_eq!(report["physical_result"], "not_observed");
    assert!(report.get("strength_score").is_none());
    assert_eq!(
        hash_file(
            ws.snapshot_artifact("projects/part/baseline/plate_1.gcode")
                .unwrap()
                .path()
        )
        .unwrap(),
        original_hash
    );
    drop(ws);
    let restarted = Workspace::open(Some(root.path()), None).unwrap();
    let retained = provenance::read(
        &restarted,
        &result.provenance,
        Kind::PreparationComparison,
        "part",
    )
    .unwrap();
    assert_eq!(retained.data["report_sha256"], provenance::digest(&bytes));
    assert_eq!(
        std::fs::read(restarted.snapshot_artifact(path).unwrap().path()).unwrap(),
        bytes
    );
}

#[test]
fn changed_current_source_preserves_historical_identity_and_flags_applicability() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path());
    let params = params(&ws);
    ws.write_artifact("projects/part/source.stl", b"edited design", true)
        .unwrap();
    let result = compare(&ws, params, CancellationToken::new()).unwrap();
    assert_eq!(result.baseline.current_source, "changed_since_slice");
    assert_eq!(
        result.baseline.source_sha256,
        provenance::digest(b"original design")
    );
    assert!(
        result.candidates[0]
            .compatibility
            .iter()
            .any(|v| v == "current_authored_source_differs_from_historical_evidence")
    );
}

#[test]
fn incompatible_material_source_and_conditions_are_retained_without_a_winner() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path());
    let mut params = params(&ws);
    params.candidates[0] = candidate(
        &ws,
        "different",
        b"different source",
        json!({"layer_height":"0.3"}),
        "PETG",
    );
    params.candidates[0].conditions.clear();
    let result = compare(&ws, params, CancellationToken::new()).unwrap();
    for reason in [
        "different_source_geometry_or_authored_settings",
        "different_ordered_material_settings",
        "physical_conditions_unspecified",
    ] {
        assert!(
            result.candidates[0]
                .compatibility
                .iter()
                .any(|v| v == reason)
        );
    }
    assert_eq!(result.automatic_selection, "not_performed");
}

#[test]
fn tampered_output_or_mismatched_review_fails_before_allocating_output() {
    for tamper in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(root.path());
        let mut params = params(&ws);
        if tamper {
            ws.write_generated_bytes("projects/part/candidate/plate_1.gcode", b"changed", true)
                .unwrap();
        } else {
            params.candidates[0].reviews = params.baseline.reviews.clone();
        }
        assert!(compare(&ws, params, CancellationToken::new()).is_err());
        assert!(!root.path().join("projects/part/comparison").exists());
    }
}

#[test]
fn invalid_candidates_and_changed_review_media_fail_before_allocating_output() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path());
    let mut params = params(&ws);
    params.candidates[0].label = params.baseline.label.clone();
    assert!(params.validate().is_err());
    params.candidates[0].label = "candidate".into();
    ws.write_reserved_artifact(
        ".printable/evidence/review_media/candidate.png",
        b"changed media",
        true,
    )
    .unwrap();
    assert!(compare(&ws, params, CancellationToken::new()).is_err());
    assert!(!root.path().join("projects/part/comparison").exists());
}

#[test]
fn cancelled_comparison_keeps_its_inputs_and_does_not_publish() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path());
    let params = params(&ws);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        compare(&ws, params, cancellation)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(!root.path().join("projects/part/comparison").exists());
    assert!(
        ws.snapshot_artifact("projects/part/baseline/plate_1.gcode")
            .is_ok()
    );
}
