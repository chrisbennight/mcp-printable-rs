use super::*;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn get(server: &MockServer, url: &str, value: Value) {
    Mock::given(method("GET"))
        .and(path(url))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}
fn service(server: &MockServer) -> Arc<PrinterService> {
    configure(&|key| match key {
        "PRINTABLE_BAMBUDDY_URL" => Some(server.uri()),
        "PRINTABLE_BAMBUDDY_READ_KEY" | "PRINTABLE_BAMBUDDY_CONTROL_KEY" => Some("fixture".into()),
        _ => None,
    })
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn mcp_dispatch_preserves_typed_results_and_uncertain_control_without_replay() {
    use crate::{config::Settings, mcp::PrintableServer};
    use rmcp::model::CallToolRequestParams;

    let backend = MockServer::start().await;
    get(
        &backend,
        "/api/v1/printers/",
        json!([{
            "id": 1, "name": "Fixture", "is_active": true,
            "access_code": "NOT_FOR_OUTPUT"
        }]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/printers/1/print/pause"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&backend)
        .await;
    let settings = Settings::from_lookup(|key| match key {
        "PRINTABLE_MCP_BEARER" => Some("ab".repeat(32)),
        "PRINTABLE_BAMBUDDY_URL" => Some(backend.uri()),
        "PRINTABLE_BAMBUDDY_READ_KEY" | "PRINTABLE_BAMBUDDY_CONTROL_KEY" => Some("fixture".into()),
        _ => None,
    })
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let workspace = printable_workspace::Workspace::open(Some(directory.path()), None).unwrap();
    let server = PrintableServer::new(
        Arc::new(workspace),
        Arc::new(printable_blender::BlenderClient::new(
            "127.0.0.1",
            9,
            Default::default(),
        )),
        Arc::new(settings),
    );
    let request = |name: &str, value: Value| {
        CallToolRequestParams::new(name.to_owned())
            .with_arguments(value.as_object().unwrap().clone())
    };
    let result = server
        .invoke_tool(request("printer", json!({"action":"list","params":{}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    let result = result.structured_content.unwrap();
    validate("printer", &result);
    assert_eq!(result["printers"][0]["id"], 1);
    assert!(!result.to_string().contains("NOT_FOR_OUTPUT"));
    let result = server
        .invoke_tool(request(
            "print",
            json!({"action":"pause","params":{"printer_id":1}}),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.unwrap()["error"]["code"],
        "printer_outcome_unknown"
    );
    let before = backend.received_requests().await.unwrap().len();
    let result = server
        .invoke_tool(request(
            "print",
            json!({"action":"pause","params":{"printer_id":0}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        result.structured_content.unwrap()["error"]["code"],
        "validation"
    );
    assert_eq!(backend.received_requests().await.unwrap().len(), before);
    backend.verify().await;
}

#[test]
fn optional_configuration_does_not_require_a_printer_for_modeling() {
    assert!(configure(&|_| None).unwrap().is_none());
    let error =
        configure(&|key| (key == "PRINTABLE_BAMBUDDY_URL").then(|| "http://localhost:8000".into()))
            .unwrap_err();
    assert_eq!(
        error,
        SettingsError::Printers("PRINTABLE_BAMBUDDY_READ_KEY")
    );
}

#[tokio::test]
async fn import_snapshots_project_bytes_and_uploads_once_on_a_current_thread_runtime() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/library/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 91, "filename": "part.gcode.3mf", "file_type": "gcode.3mf"
        })))
        .expect(1)
        .mount(&backend)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(directory.path()), None).unwrap());
    crate::projects::dispatch(
        &ws,
        serde_json::from_value(json!({
            "action":"create", "params":{"project_id":"import-fixture","name":"Import fixture"}
        }))
        .unwrap(),
    )
    .unwrap();
    ws.write_artifact(
        "projects/import-fixture/part.gcode.3mf",
        b"fixture-print-bytes",
        false,
    )
    .unwrap();
    let result = service(&backend)
        .control(
            serde_json::from_value(json!({
                "action":"import", "params":{"project_id":"import-fixture","path":"part.gcode.3mf"}
            }))
            .unwrap(),
            &ws,
        )
        .await
        .unwrap();
    assert_eq!(result["starts_printing"], false);
    assert_eq!(result["library_file"]["id"], 91);
    let schema = super::action_output_schema("print", "import").unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&result)
    );
    let requests = backend.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .body
            .windows(b"fixture-print-bytes".len())
            .any(|bytes| bytes == b"fixture-print-bytes")
    );
    let (_, association) = ws
        .read_artifact(".printable/bambuddy-library/91.json")
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&association).unwrap()["project_id"],
        "import-fixture"
    );
    backend.verify().await;
}

fn validate(name: &str, value: &Value) {
    let schema = output_schema(name);
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn unavailable_model_aliases_do_not_certify_incompatibility() {
    use super::review::{MatchResult, model_finding};
    let aliases = std::collections::BTreeMap::from([("Bambu Lab P1S".into(), "P1S".into())]);
    for (source, target, models, expected) in [
        (
            Some("Bambu Lab P1S"),
            Some("P1S"),
            None,
            MatchResult::Unknown,
        ),
        (Some(" P1S "), Some("p1s"), None, MatchResult::Match),
        (None, Some("P1S"), None, MatchResult::Unknown),
        (
            Some("Bambu Lab P1S"),
            Some("P1S"),
            Some(&aliases),
            MatchResult::Match,
        ),
        (
            Some("Bambu Lab P1S"),
            Some("A1"),
            Some(&aliases),
            MatchResult::Mismatch,
        ),
    ] {
        assert_eq!(model_finding(source, target, models).result, expected);
    }
}
fn status() -> Value {
    json!({"id":1,"name":"Workshop","connected":true,"state":"IDLE","tray_now":0,
    "ams":[{"id":0,"humidity":3,"temp":29.1,"tray":[{"id":0,"tray_type":"PLA","tray_sub_brands":"PLA Basic","tray_info_idx":"GFA00","tray_color":"FFFFFFFF","remain":-1,"state":11}]},
    {"id":128,"is_ams_ht":true,"tray":[{"id":0,"tray_type":"PETG","tray_sub_brands":"Transparent PETG","tray_info_idx":"custom-clear","tray_color":"FFFFFFFF","remain":50,"state":11}]}],
    "vt_tray":[{"id":254,"tray_type":"PC","remain":-1}],
    "hms_errors":[{"code":"0x200a0","severity":6,"attr":123,"module":2,"full_code":"0000007B000200A0","actions":[]}],
    "nozzles":[{"nozzle_diameter":"0.4","nozzle_type":"hardened_steel"}],"firmware_version":"fixture-1"})
}
#[tokio::test]
async fn printer_preserves_material_locations_faults_and_selectable_detail() {
    let server = MockServer::start().await;
    get(&server, "/api/v1/printers/1/status", status()).await;
    get(
        &server,
        "/api/v1/cloud/builtin-filaments",
        json!([{"filament_id":"GFA00","name":"Bambu PLA Basic"}]),
    )
    .await;
    let service = service(&server);
    let temp = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
    let summary = service
        .observe(
            serde_json::from_value(json!({"action":"status","params":{"printer_id":1}})).unwrap(),
            &ws,
        )
        .await
        .unwrap();
    validate("printer", &summary);
    assert!(summary.get("details").is_none());
    assert_eq!(summary["nozzles"][0]["diameter"], "0.4");
    assert_eq!(summary["nozzles"][0]["type"], "hardened_steel");
    assert_eq!(summary["materials"][0]["product_name"], "PLA Basic");
    assert_eq!(summary["materials"][0]["product_source"], "device");
    assert!(summary["materials"][0]["remaining_percent"].is_null());
    assert_eq!(summary["materials"][1]["mapping_id"], 128);
    assert_eq!(summary["materials"][2]["mapping_id"], 254);
    assert_eq!(summary["faults"][0]["attr"], 123);
    assert!(summary["faults"][0]["severity_name"].is_null());
    assert_eq!(summary["faults"][0]["meaning_available"], false);
    let detailed=service.observe(serde_json::from_value(json!({"action":"status","params":{"printer_id":1,"sections":["materials","hardware"]}})).unwrap(),&ws).await.unwrap();
    validate("printer", &detailed);
    assert_eq!(detailed["details"]["firmware_version"], "fixture-1");
    assert_eq!(detailed["materials"][0]["detail"]["reported"]["remain"], -1);
    assert_eq!(
        detailed["materials"][0]["detail"]["catalog_name"],
        "Bambu PLA Basic"
    );
    assert!(detailed["details"].get("temperatures").is_none());
}
#[tokio::test]
async fn review_distinguishes_exact_variants_and_explicit_transparency_without_mutation() {
    let server = MockServer::start().await;
    let mut hardware = status();
    hardware["state"] = json!("RUNNING");
    hardware["current_print"] = json!("existing-job");
    hardware["nozzles"] = json!([{"nozzle_diameter":"0.6"},{"nozzle_diameter":null}]);
    get(&server, "/api/v1/printers/1/status", hardware).await;
    get(
        &server,
        "/api/v1/printers/",
        json!([{"id":1,"name":"Workshop","model":"P1S","is_active":true}]),
    )
    .await;
    get(
        &server,
        "/api/v1/slicer/printer-models",
        json!({"Bambu Lab P1S":"P1S"}),
    )
    .await;
    get(&server,"/api/v1/library/files/9",json!({"id":9,"filename":"test.gcode.3mf","file_type":"gcode.3mf","metadata":{"sliced_for_model":"Bambu Lab P1S","nozzle_diameter":0.4}})).await;
    get(&server,"/api/v1/library/files/9/filament-requirements",json!({"plate_id":1,"filaments":[{"slot_id":1,"type":"PLA","color":"000000FF","used_grams":10},{"slot_id":2,"type":"PETG","color":"ffffffff","used_grams":20}]})).await;
    get(&server,"/api/v1/inventory/assignments",json!([{"id":1,"ams_id":128,"tray_id":0,"spool_id":8,"fingerprint_type":"PETG","fingerprint_color":"FFFFFFFF","spool":{"id":8,"material":"PETG","subtype":"transparent","brand":"Example"}}])).await;
    get(
        &server,
        "/api/v1/printers/1/inventory-remain",
        json!({"inventory_remain_g":{"128":100}}),
    )
    .await;
    get(
        &server,
        "/api/v1/printers/1/slot-presets",
        json!({"0":{"ams_id":0,"tray_id":0,"preset_id":"basic","preset_name":"Bambu PLA Basic"}}),
    )
    .await;
    get(
        &server,
        "/api/v1/library/files/9/plates",
        json!({"plates":[{"index":1,"bed_type":"textured_plate"}]}),
    )
    .await;
    let service = service(&server);
    let temp = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
    let review=service.control(serde_json::from_value(json!({"action":"review","params":{"source":{"kind":"library","id":9},"printer_id":1,"plate":1,"ams_mapping":[0,128],"flow_calibration":false,"options":{"preheat_override":"off"},"requirements":[{"slot_id":1,"product_name":"PLA Matte"},{"slot_id":1,"product_name":"Bambu PLA Basic"},{"slot_id":2,"subtype":"transparent","brand":"Example"}]}})).unwrap(),&ws).await.unwrap();
    validate("print", &review);
    let contract = crate::resources::contracts::read("printable://contracts/print/review").unwrap();
    assert_eq!(contract["output_schema_scope"], "action");
    let selected = &contract["outputSchema"];
    assert!(
        jsonschema::validator_for(selected)
            .unwrap()
            .is_valid(&review)
    );
    assert!(selected["$defs"].get("Control").is_none());
    assert!(selected["$defs"].get("QueueItem").is_none());
    for (index, expected) in [(0, "mismatch"), (1, "match")] {
        let finding = review["materials"][index]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["check"] == "slice_color")
            .unwrap();
        assert_eq!(finding["result"], expected);
    }
    assert!(
        review["materials"][0]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "product_name" && f["result"] == "match")
    );
    let schema = super::schema::output("print");
    assert!(
        schema["$defs"]["PrintStatus"]["properties"]
            .get("compatibility")
            .is_some()
    );
    assert!(
        schema["$defs"]["Control"]["properties"]
            .get("operation")
            .is_some()
    );
    assert!(
        schema["$defs"]["Control"]["properties"]
            .get("outcome")
            .is_some()
    );
    assert_eq!(review["findings"][0]["result"], "match");
    assert_eq!(review["materials"][0]["findings"][0]["result"], "mismatch");
    assert!(
        review["materials"][1]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "subtype" && f["result"] == "match")
    );
    assert_eq!(review["starts_printing"], false);
    assert_eq!(review["reservation"], false);
    assert!(
        review["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "printer_state"
                && f["result"] == "mismatch"
                && f["observed"]["current_print"] == "existing-job")
    );
    assert!(
        review["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "bed_type" && f["observed"] == "textured_plate")
    );
    assert_eq!(review["effective_options"]["flow_calibration"], false);
    assert_eq!(review["effective_options"]["preheat_override"], "off");
    let nozzle = review["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "nozzle_diameter")
        .unwrap();
    assert_eq!(nozzle["result"], "unknown");
    assert_eq!(nozzle["observed"]["installed"], json!([0.6, null]));
    let missing = service.control(serde_json::from_value(json!({"action":"review","params":{"source":{"kind":"library","id":9},"printer_id":1,"plate":2}})).unwrap(), &ws).await.unwrap();
    assert!(
        missing["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "selected_plate" && f["result"] == "mismatch")
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}
#[tokio::test]
async fn enrichment_is_not_attached_to_a_changed_spool() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/api/v1/printers/1/status"))
        .respond_with(move |_: &wiremock::Request| {
            let mut value = status();
            if count.fetch_add(1, Ordering::SeqCst) > 0 {
                value["ams"][0]["tray"][0]["tray_info_idx"] = json!("GFA01");
                value["ams"][0]["tray"][0]["tray_sub_brands"] = json!("PLA Matte");
            }
            ResponseTemplate::new(200).set_body_json(value)
        })
        .mount(&server)
        .await;
    get(
        &server,
        "/api/v1/printers/1/slot-presets",
        json!({"0":{"ams_id":0,"tray_id":0,"preset_id":"preset-basic","preset_name":"PLA Basic"}}),
    )
    .await;
    let loaded = service(&server).loaded_materials(1, true).await.unwrap();
    assert_eq!(
        loaded.materials[0].product_name.as_deref(),
        Some("PLA Matte")
    );
    assert!(
        loaded.materials[0]
            .detail
            .as_ref()
            .unwrap()
            .preset
            .is_none()
    );
    assert!(loaded.sources["enrichment"].is_some());
}
#[tokio::test]
async fn pending_edits_remain_staged_and_return_partial_counts() {
    for value in [
        json!({}),
        json!({"scheduled_time":null}),
        json!({"scheduled_time":"2026-09-20T12:00:00Z"}),
    ] {
        let options: bambuddy_api::job_options::JobOptions =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(options).unwrap(), value);
    }
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/queue/bulk"))
        .and(wiremock::matchers::body_partial_json(
            json!({"item_ids":[8,9],"manual_start":true,"scheduled_time":null}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"updated_count":1,"skipped_count":1,"message":"one job no longer pending"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let service = service(&server);
    let temp = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
    let result=service.control(serde_json::from_value(json!({"action":"update","params":{"target":{"kind":"queue","print_ids":[8,9]},"patch":{"scheduled_time":null}}})).unwrap(),&ws).await.unwrap();
    validate("print", &result);
    assert_eq!(result["update"]["skipped_count"], 1);
    assert_eq!(result["may_start_printing"], false);
    server.verify().await;
}

#[tokio::test]
async fn start_refuses_a_job_changed_during_inspection() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/api/v1/queue/7"))
        .respond_with(move |_: &wiremock::Request| {
            let mapping = if count.fetch_add(1, Ordering::SeqCst) == 0 { 0 } else { 1 };
            ResponseTemplate::new(200).set_body_json(json!({"id":7,"printer_id":1,"library_file_id":9,"status":"pending","manual_start":true,"ams_mapping":[mapping]}))
        })
        .mount(&server)
        .await;
    get(&server, "/api/v1/printers/1/status", status()).await;
    get(
        &server,
        "/api/v1/printers/",
        json!([{"id":1,"name":"Workshop","model":"P1S","is_active":true}]),
    )
    .await;
    get(&server, "/api/v1/library/files/9", json!({"id":9,"filename":"test.gcode.3mf","file_type":"gcode.3mf","sliced_for_model":"P1S"})).await;
    let temp = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
    let result = service(&server)
        .control(
            serde_json::from_value(json!({"action":"start","params":{"print_id":7}})).unwrap(),
            &ws,
        )
        .await;
    assert!(
        matches!(result, Err(crate::error::ToolError::Validation(message)) if message.contains("changed during inspection"))
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}

#[tokio::test]
async fn print_records_preserve_options_requirements_and_run_identity() {
    let server = MockServer::start().await;
    let file = json!({"id":9,"filename":"test.gcode.3mf","file_type":"gcode.3mf","tags":[],"metadata":{"bed_type":"Textured PEI","nozzle_diameter":0.4}});
    get(&server, "/api/v1/library/files", json!([file.clone()])).await;
    get(&server, "/api/v1/library/files/9", file).await;
    get(&server,"/api/v1/queue/7",json!({"id":7,"library_file_id":9,"status":"pending","manual_start":true,"flow_cali":false,"layer_inspect":true,"preheat_override":"off","plate_id":1,"ams_mapping":[0]})).await;
    get(
        &server,
        "/api/v1/library/files/9/plates",
        json!({"plates":[{"index":1,"name":"Plate 1","filament_used_grams":25}]}),
    )
    .await;
    get(
        &server,
        "/api/v1/library/files/9/filament-requirements",
        json!({"plate_id":1,"filaments":[{"slot_id":1,"type":"PLA","used_grams":25}]}),
    )
    .await;
    get(
        &server,
        "/api/v1/archives/8",
        json!({"id":8,"filename":"test.gcode.3mf","status":"completed","run_count":2}),
    )
    .await;
    get(&server,"/api/v1/archives/8/runs",json!({"items":[{"id":10,"archive_id":8,"printer_id":1,"status":"completed"},{"id":11,"archive_id":8,"printer_id":2,"status":"failed","failure_reason":"user stopped"}],"total":2})).await;
    let service = service(&server);
    let temp = tempfile::tempdir().unwrap();
    let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
    for detail in [false, true] {
        let list = service.control(serde_json::from_value(json!({"action":"list","params":{"collection":"library","detail":detail,"limit":1}})).unwrap(),&ws).await.unwrap();
        validate("print", &list);
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
    }
    let queue = service
        .control(
            serde_json::from_value(
                json!({"action":"status","params":{"print_id":7,"detail":true,"plate":1}}),
            )
            .unwrap(),
            &ws,
        )
        .await
        .unwrap();
    validate("print", &queue);
    assert_eq!(queue["print"]["flow_cali"], false);
    assert_eq!(queue["print"]["layer_inspect"], true);
    assert_eq!(
        queue["requirements"]["data"]["filaments"][0]["used_grams"],
        25.0
    );
    let archive = service
        .control(
            serde_json::from_value(
                json!({"action":"status","params":{"target":{"kind":"archive","id":8}}}),
            )
            .unwrap(),
            &ws,
        )
        .await
        .unwrap();
    validate("print", &archive);
    let runs = service
        .control(
            serde_json::from_value(json!({"action":"history","params":{"archive_id":8,"limit":1}}))
                .unwrap(),
            &ws,
        )
        .await
        .unwrap();
    validate("print", &runs);
    assert_eq!(runs["items"][0]["id"], 10);
    assert_eq!(runs["items"][0]["archive_id"], 8);
    assert_eq!(runs["next_offset"], 1);
    let filtered = service
        .control(
            serde_json::from_value(
                json!({"action":"history","params":{"archive_id":8,"printer_id":2,"limit":1}}),
            )
            .unwrap(),
            &ws,
        )
        .await
        .unwrap();
    assert_eq!(filtered["items"][0]["id"], 11);
    assert_eq!(filtered["total"], 1);
    assert!(filtered["next_offset"].is_null());
}

#[tokio::test]
async fn review_never_certifies_conflicting_identity_or_incomplete_shared_spool_demand() {
    for case in [
        "product_conflict",
        "variant_suffix",
        "assignment_conflict",
        "spool_material_conflict",
        "unknown_demand",
        "catalog_conflict",
        "ams_disabled",
        "unsupported_format",
    ] {
        let server = MockServer::start().await;
        let mut observation = status();
        if case == "catalog_conflict" {
            observation["connected"] = json!(false);
            observation["last_ams_update"] = json!(123.0);
            observation["ams"][0]["tray"][0]["tray_sub_brands"] = Value::Null;
        }
        get(&server, "/api/v1/printers/1/status", observation).await;
        get(
            &server,
            "/api/v1/cloud/builtin-filaments",
            json!([{"filament_id":"GFA00","name":"Bambu PLA Basic"}]),
        )
        .await;
        get(
            &server,
            "/api/v1/printers/",
            json!([{"id":1,"name":"Workshop","model":"P1S","is_active":true}]),
        )
        .await;
        get(
            &server,
            "/api/v1/library/files/9",
            json!({"id":9,"filename":"test.gcode.3mf","file_type":if case == "unsupported_format" {"stl"} else {"gcode.3mf"}}),
        )
        .await;
        let estimate = if case == "unknown_demand" {
            Value::Null
        } else {
            json!(1)
        };
        get(&server,"/api/v1/library/files/9/filament-requirements",json!({"filaments":[{"slot_id":1,"type":"PLA","used_grams":10},{"slot_id":2,"type":"PLA","used_grams":estimate},{"slot_id":3,"type":"PETG","used_in_plate":false}]})).await;
        let product = if matches!(
            case,
            "product_conflict" | "catalog_conflict" | "variant_suffix"
        ) {
            if case == "variant_suffix" {
                "PLA Basic-CF"
            } else {
                "PLA Matte"
            }
        } else {
            "PLA Basic"
        };
        let fingerprint = if case == "assignment_conflict" {
            "PETG"
        } else {
            "PLA"
        };
        get(&server,"/api/v1/inventory/assignments",json!([{"id":1,"ams_id":0,"tray_id":0,"spool_id":8,"fingerprint_type":fingerprint,"spool":{"id":8,"slicer_filament_name":product,"subtype":"matte","material":if case == "spool_material_conflict" {"PETG"} else {"PLA"}}}])).await;
        get(
            &server,
            "/api/v1/printers/1/inventory-remain",
            json!({"inventory_remain_g":{"0":15}}),
        )
        .await;
        let service = service(&server);
        let temp = tempfile::tempdir().unwrap();
        let ws = Arc::new(printable_workspace::Workspace::open(Some(temp.path()), None).unwrap());
        let review = service.control(serde_json::from_value(json!({"action":"review","params":{"source":{"kind":"library","id":9},"printer_id":1,"ams_mapping":[0,0],"use_ams":(case != "ams_disabled"),"requirements":[{"slot_id":1,"product_name":product,"subtype":"matte"}]}})).unwrap(),&ws).await.unwrap();
        assert_eq!(review["materials"].as_array().unwrap().len(), 2);
        assert!(
            review["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["check"] == "nozzle_diameter"
                    && f["result"] == "unknown"
                    && f["observed"]["required"].is_null())
        );
        for material in review["materials"].as_array().unwrap() {
            let quantity = material["findings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["check"] == "material_quantity")
                .unwrap();
            assert_eq!(
                quantity["result"],
                if case == "unsupported_format" {
                    "match"
                } else {
                    "unknown"
                },
                "{case}"
            );
        }
        if matches!(
            case,
            "product_conflict" | "catalog_conflict" | "variant_suffix"
        ) {
            assert!(
                review["materials"][0]["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|f| f["check"] == "subtype" || f["check"] == "product_name")
                    .all(|f| f["result"] == "unknown")
            );
            let summary = service
                .observe(
                    serde_json::from_value(json!({"action":"status","params":{"printer_id":1}}))
                        .unwrap(),
                    &ws,
                )
                .await
                .unwrap();
            assert!(
                !summary["materials"][0]["conflicts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(summary["materials"][0].get("detail").is_none());
            if case == "catalog_conflict" {
                let materials = service.observe(serde_json::from_value(json!({"action":"materials","params":{"scope":"loaded","printer_id":1}})).unwrap(), &ws).await.unwrap();
                validate("printer", &materials);
                assert_eq!(materials["connected"], false);
                assert_eq!(materials["last_ams_update"], 123.0);
            }
        }
        if case == "unsupported_format" {
            assert!(
                review["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["check"] == "source_format" && f["result"] == "mismatch")
            );
        }
        if matches!(case, "assignment_conflict" | "spool_material_conflict") {
            assert!(review["materials"][0]["loaded"]["detail"]["inventory"].is_null());
            assert_eq!(
                review["materials"][0]["loaded"]["detail"]["conflicting_inventory"]["spool_id"],
                8
            );
            assert!(review["materials"][0]["loaded"]["detail"]["remaining_grams"].is_null());
        }
    }
}
