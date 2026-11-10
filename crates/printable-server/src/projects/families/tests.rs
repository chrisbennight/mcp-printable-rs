use super::*;
use serde_json::json;

pub(crate) fn fixture(workspace: &Workspace) -> Family {
    super::super::dispatch(
        workspace,
        serde_json::from_value(
            json!({"action":"create","params":{"project_id":"parts","name":"Parts"}}),
        )
        .unwrap(),
    )
    .unwrap();
    for (name, bytes) in [
        ("small.stl", b"small".as_slice()),
        ("large.stl", b"large".as_slice()),
        ("split-a.stl", b"a".as_slice()),
        ("split-b.stl", b"b".as_slice()),
    ] {
        workspace
            .write_artifact(&format!("projects/parts/{name}"), bytes, false)
            .unwrap();
    }
    serde_json::from_value(json!({"id":"brackets","name":"Mounting brackets","category":"Mounts",
        "axes":[{"id":"length","label":"Length","kind":"number","unit":"mm"},{"id":"pattern","label":"Mounting pattern","kind":"text"}],
        "variants":[{"id":"short","name":"Short two-hole bracket","attributes":{"length":20,"pattern":"two-hole"},
            "representations":[{"id":"whole","name":"Whole","files":[{"path":"small.stl","sha256":digest(b"small")}]}]},
            {"id":"long","name":"Long four-hole bracket","attributes":{"length":40,"pattern":"four-hole"},
            "representations":[{"id":"whole","name":"Whole","files":[{"path":"large.stl","sha256":digest(b"large")}]},
                {"id":"split","name":"Split print","files":[{"path":"split-a.stl","sha256":digest(b"a")},{"path":"split-b.stl","sha256":digest(b"b")}]}]}]})).unwrap()
}
fn params() -> SelectParams {
    SelectParams {
        project_id: "parts".into(),
        family_id: "brackets".into(),
        revision: None,
        attributes: BTreeMap::new(),
        variant_id: None,
        offset: 0,
        limit: 24,
    }
}
fn save(workspace: &Workspace, family: Family) -> Response {
    put(
        workspace,
        PutParams {
            project_id: "parts".into(),
            expected_revision: None,
            family,
        },
    )
    .unwrap()
}

#[test]
fn sparse_typed_choices_representations_and_exact_reopening() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let family = fixture(&workspace);
    let saved = save(&workspace, family);
    let mut query = params();
    query
        .attributes
        .insert("length".into(), Attribute::Number(20.0));
    let selection = select(&workspace, &query).unwrap();
    assert_eq!(selection.total, 1);
    assert_eq!(selection.selected.unwrap().id, "short");
    assert_eq!(
        selection.options[1].values,
        vec![Attribute::Text("two-hole".into())]
    );
    query
        .attributes
        .insert("pattern".into(), Attribute::Text("four-hole".into()));
    assert_eq!(select(&workspace, &query).unwrap().total, 0);
    drop(workspace);
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let mut query = params();
    query.revision = Some(saved.revision);
    query.variant_id = Some("long".into());
    let selected = select(&workspace, &query).unwrap().selected.unwrap();
    assert_eq!(selected.representations.len(), 2);
    assert_eq!(selected.representations[1].files.len(), 2);
}

#[test]
fn stale_busy_edits_and_source_changes_do_not_relabel_variants() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let family = fixture(&workspace);
    let saved = save(&workspace, family.clone());
    assert!(
        put(
            &workspace,
            PutParams {
                project_id: "parts".into(),
                expected_revision: None,
                family: family.clone()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("revision changed")
    );
    let lock = workspace
        .try_lock_reserved(".printable/families/parts/brackets/edit.lock")
        .unwrap();
    assert!(
        put(
            &workspace,
            PutParams {
                project_id: "parts".into(),
                expected_revision: Some(saved.revision.clone()),
                family: family.clone()
            }
        )
        .is_err()
    );
    drop(lock);
    let mut changed = family.clone();
    changed.description = "Updated notes".into();
    let second = put(
        &workspace,
        PutParams {
            project_id: "parts".into(),
            expected_revision: Some(saved.revision.clone()),
            family: changed,
        },
    )
    .unwrap();
    assert_ne!(saved.revision, second.revision);
    assert_eq!(
        get(
            &workspace,
            &GetParams {
                project_id: "parts".into(),
                family_id: "brackets".into(),
                revision: Some(saved.revision.clone())
            }
        )
        .unwrap()
        .record
        .family,
        family
    );
    workspace
        .write_artifact("projects/parts/small.stl", b"other", true)
        .unwrap();
    let mut query = params();
    query.variant_id = Some("short".into());
    query.revision = Some(saved.revision);
    assert!(
        select(&workspace, &query)
            .unwrap_err()
            .to_string()
            .contains("source content changed")
    );
}

#[test]
fn inventory_retains_duplicate_references_and_unclassified_files() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let mut family = fixture(&workspace);
    workspace
        .write_artifact("projects/parts/copy.stl", b"small", false)
        .unwrap();
    workspace
        .write_artifact("projects/parts/readme.json", b"{}", false)
        .unwrap();
    family.variants[1].representations[0].files.push(File {
        path: "copy.stl".into(),
        sha256: digest(b"small"),
    });
    save(&workspace, family);
    let listing = list(
        &workspace,
        &ListParams {
            project_id: "parts".into(),
            text: "40 four-hole".into(),
            offset: 0,
            limit: 24,
        },
    )
    .unwrap();
    assert_eq!(listing.total, 1);
    assert_eq!(listing.counts.variants, 2);
    assert_eq!(listing.counts.classified_files, 5);
    assert_eq!(listing.counts.distinct_classified_contents, 4);
    assert_eq!(listing.counts.repeated_contents, 1);
    assert_eq!(listing.counts.unclassified_files, 1);
    assert!(
        list(
            &workspace,
            &ListParams {
                project_id: "parts".into(),
                text: "missing".into(),
                offset: 0,
                limit: 24
            }
        )
        .unwrap()
        .families
        .is_empty()
    );
}

#[test]
fn malformed_combinations_paths_and_file_identities_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let family = fixture(&workspace);
    let mut cases = vec![];
    let mut f = family.clone();
    f.variants[1].attributes = f.variants[0].attributes.clone();
    cases.push(f);
    let mut f = family.clone();
    f.variants[0]
        .attributes
        .insert("length".into(), Attribute::Text("20".into()));
    cases.push(f);
    let mut f = family.clone();
    f.variants[0].representations[0].files[0].path = "../outside.stl".into();
    cases.push(f);
    let mut f = family.clone();
    f.id = "../bad".into();
    cases.push(f);
    let mut f = family.clone();
    f.variants[0].representations[0].files[0].sha256 = digest(b"wrong");
    cases.push(f);
    for family in cases {
        assert!(
            put(
                &workspace,
                PutParams {
                    project_id: "parts".into(),
                    expected_revision: None,
                    family
                }
            )
            .is_err()
        );
    }
    std::os::unix::fs::symlink(
        root.path().join("projects/parts/small.stl"),
        root.path().join("projects/parts/link.stl"),
    )
    .unwrap();
    let mut f = family;
    f.variants[0].representations[0].files[0].path = "link.stl".into();
    assert!(
        put(
            &workspace,
            PutParams {
                project_id: "parts".into(),
                expected_revision: None,
                family: f
            }
        )
        .is_err()
    );
}

#[test]
fn unrelated_boolean_and_numeric_families_use_the_same_model() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    fixture(&workspace);
    let family:Family=serde_json::from_value(json!({"id":"enclosures","name":"Board enclosures",
        "axes":[{"id":"board","label":"Board","kind":"text"},{"id":"vented","label":"Vented lid","kind":"boolean"}],
        "variants":[{"id":"board-a-solid","name":"Board A solid","attributes":{"board":"A","vented":false},"representations":[{"id":"stl","name":"STL","files":[{"path":"small.stl","sha256":digest(b"small")}]}]}]})).unwrap();
    save(&workspace, family);
    let mut query = params();
    query.family_id = "enclosures".into();
    query
        .attributes
        .insert("vented".into(), Attribute::Boolean(true));
    assert_eq!(select(&workspace, &query).unwrap().total, 0);
}

#[test]
fn public_workflow_contracts_describe_actual_family_results() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(Some(root.path()), None).unwrap();
    let family = fixture(&workspace);
    for request in [
        json!({"action":"family_put","params":{"project_id":"parts","expected_revision":null,"family":family}}),
        json!({"action":"family","params":{"project_id":"parts","family_id":"brackets"}}),
        json!({"action":"families","params":{"project_id":"parts"}}),
        json!({"action":"family_select","params":{"project_id":"parts","family_id":"brackets","variant_id":"long"}}),
    ] {
        let contract = crate::resources::contracts::read(
            &format!(
                "printable://contracts/project/{}",
                request["action"].as_str().unwrap()
            ),
            crate::tools::workflows::Availability::default(),
        )
        .unwrap();
        assert_eq!(contract["output_schema_scope"], "action");
        assert!(
            jsonschema::validator_for(&contract["inputSchema"])
                .unwrap()
                .is_valid(&request)
        );
        let result =
            super::super::dispatch(&workspace, serde_json::from_value(request).unwrap()).unwrap();
        assert!(
            jsonschema::validator_for(&contract["outputSchema"])
                .unwrap()
                .is_valid(&result),
            "{result}"
        );
    }
}
