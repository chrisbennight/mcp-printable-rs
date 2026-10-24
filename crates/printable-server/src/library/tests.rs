use super::*;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

use crate::{config::Settings, mcp::PrintableServer};

fn settings(get: impl Fn(&str) -> Option<&'static str>) -> Result<Settings, SettingsError> {
    Settings::from_lookup(|key| match key {
        "PRINTABLE_MCP_BEARER" => Some("ab".repeat(32)),
        key => get(key).map(str::to_owned),
    })
}

#[test]
fn library_settings_enable_named_sites_and_require_thingiverse_tokens() {
    assert!(settings(|_| None).unwrap().library.is_none());
    assert!(
        settings(|key| (key == SITES_SETTING).then_some(" "))
            .unwrap()
            .library
            .is_none()
    );
    assert_eq!(
        settings(|key| (key == SITES_SETTING).then_some(" , ")).unwrap_err(),
        SettingsError::Library(SITES_SETTING)
    );
    assert_eq!(
        settings(|key| (key == SITES_SETTING).then_some("printables,cults3d")).unwrap_err(),
        SettingsError::Library(SITES_SETTING)
    );
    assert_eq!(
        settings(|key| (key == SITES_SETTING).then_some("thingiverse")).unwrap_err(),
        SettingsError::Library(THINGIVERSE_TOKEN_SETTING)
    );
    assert_eq!(
        settings(|key| match key {
            SITES_SETTING => Some("makerworld"),
            "PRINTABLE_MAKERWORLD_TOKEN" => Some("two words"),
            _ => None,
        })
        .unwrap_err(),
        SettingsError::Library("PRINTABLE_MAKERWORLD_TOKEN")
    );
    let configured = settings(|key| match key {
        SITES_SETTING => Some(" makerworld, printables,thingiverse "),
        THINGIVERSE_TOKEN_SETTING => Some("SENTINEL_THINGIVERSE"),
        _ => None,
    })
    .unwrap();
    let library = configured.library.as_ref().unwrap();
    assert_eq!(
        library.report(),
        json!({"configured": true, "sites": [
            {"site": "makerworld", "search": true, "download": false},
            {"site": "printables", "search": true, "download": true},
            {"site": "thingiverse", "search": true, "download": true},
        ], "makerworld_sign_in": {
            "account_configured": false, "automatic_codes": false, "persistent": false,
            "state": "signed_out"
        }})
    );
    assert!(!format!("{configured:?}").contains("SENTINEL"));
}

#[test]
fn site_file_names_become_safe_unique_workspace_names() {
    for (name, expected) in [
        ("Tray.STL", "Tray.stl"),
        ("../../etc/passwd.stl", "passwd.stl"),
        ("C:\\models\\Bowl v2 (final).3mf", "Bowl_v2_final.3mf"),
        ("plate.gcode.3mf", "plate.gcode.3mf"),
        (".hidden.stl", "hidden.stl"),
        ("???.step", "file.step"),
        ("Ünïcode bowl.obj", "n_code_bowl.obj"),
    ] {
        assert_eq!(local_name(name).unwrap(), expected, "{name}");
    }
    assert_eq!(
        local_name(&format!("{}.stl", "a".repeat(300)))
            .unwrap()
            .len(),
        104
    );
    for name in ["guide.pdf", "README", "archive.rar"] {
        assert!(
            matches!(local_name(name), Err(ToolError::Validation(_))),
            "{name}"
        );
    }
    let mut taken = BTreeSet::new();
    assert_eq!(unique_name("part.stl".into(), "1", &mut taken), "part.stl");
    assert_eq!(
        unique_name("part.stl".into(), "2", &mut taken),
        "part-2.stl"
    );
    assert_eq!(
        unique_name("part.stl".into(), "2", &mut taken),
        "part-2-2.stl"
    );
}

struct Fixture {
    server: PrintableServer,
    site: MockServer,
    files: MockServer,
    _workspace: tempfile::TempDir,
}

async fn fixture(sites: &[Site]) -> Fixture {
    fixture_with(sites, &[]).await
}

/// A server whose library reaches local fake sites, with MakerWorld sign-in
/// settings from `account`.
async fn fixture_with(sites: &[Site], account: &[(&str, String)]) -> Fixture {
    let site = MockServer::start().await;
    let files = MockServer::start().await;
    let mut settings = settings(|_| None).unwrap();
    let client = Client::loopback(
        &url::Url::parse(&format!("{}/", site.uri())).unwrap(),
        Credentials::default(),
    )
    .unwrap();
    let makerworld = account::MakerWorldAccount::configure(&|key| {
        account
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    settings.library = Some(Arc::new(LibraryService::new(
        client,
        sites.iter().copied().collect(),
        makerworld,
    )));
    let workspace = tempfile::tempdir().unwrap();
    let server = PrintableServer::new(
        Arc::new(Workspace::open(Some(workspace.path()), None).unwrap()),
        Arc::new(printable_blender::BlenderClient::new(
            "127.0.0.1",
            9,
            Default::default(),
        )),
        Arc::new(settings),
    );
    let created = call(
        &server,
        "project",
        json!({"action": "create", "params": {"project_id": "bowls", "name": "Bowls"}}),
    )
    .await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    Fixture {
        server,
        site,
        files,
        _workspace: workspace,
    }
}

async fn call(server: &PrintableServer, tool: &str, arguments: Value) -> CallToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("tool arguments are an object");
    };
    server
        .invoke_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .expect("advertised tool")
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured result")
}

async fn mount_printables_model(fixture: &Fixture, second_file_status: u16) {
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({"operationName": "ModelFiles"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"print": {
                "id": "553277", "name": "Low Poly Tray", "slug": "low-poly-tray",
                "user": {"publicUsername": "Markus"},
                "license": {"name": "Creative Commons — Attribution", "disallowRemixing": false},
                "stls": [
                    {"id": "1", "name": "Tray.STL", "fileSize": 11},
                    {"id": "2", "name": "sub/Tray.stl", "fileSize": 5}
                ],
                "otherFiles": [{"id": "3", "name": "guide.pdf"}]
            }}})),
        )
        .mount(&fixture.site)
        .await;
    for (id, status) in [("1", 200), ("2", second_file_status)] {
        Mock::given(method("POST"))
            .and(path("/printables/graphql/"))
            .and(body_partial_json(json!({
                "operationName": "GetDownloadLink",
                "variables": {"files": [{"fileType": "stl", "ids": [id]}]}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"getDownloadLink": {
                "ok": true, "output": {"files": [{"id": id, "link": format!("{}/files/{id}.stl", fixture.files.uri())}]}
            }}})))
            .mount(&fixture.site)
            .await;
        let response = if status == 200 {
            ResponseTemplate::new(200).set_body_bytes(format!("solid {id}\n").into_bytes())
        } else {
            ResponseTemplate::new(status)
        };
        Mock::given(method("GET"))
            .and(path(format!("/files/{id}.stl")))
            .respond_with(response)
            .mount(&fixture.files)
            .await;
    }
}

#[tokio::test]
async fn library_is_advertised_only_when_a_site_is_enabled() {
    let directory = tempfile::tempdir().unwrap();
    let server = PrintableServer::new(
        Arc::new(Workspace::open(Some(directory.path()), None).unwrap()),
        Arc::new(printable_blender::BlenderClient::new(
            "127.0.0.1",
            9,
            Default::default(),
        )),
        Arc::new(settings(|_| None).unwrap()),
    );
    assert!(
        !server
            .list_tools_payload()
            .tools
            .iter()
            .any(|tool| tool.name == "library")
    );
    let arguments = json!({"action": "search", "params": {"site": "printables", "query": "bowl"}});
    let Value::Object(arguments) = arguments else {
        unreachable!()
    };
    assert!(
        server
            .invoke_tool(CallToolRequestParams::new("library").with_arguments(arguments))
            .await
            .is_err()
    );

    let fixture = fixture(&[Site::Printables]).await;
    let tool = fixture
        .server
        .list_tools_payload()
        .tools
        .into_iter()
        .find(|tool| tool.name == "library")
        .expect("library advertised");
    let annotations = tool.annotations.expect("annotations");
    assert_eq!(annotations.open_world_hint, Some(true));
    let refused = call(
        &fixture.server,
        "library",
        json!({"action": "search", "params": {"site": "makerworld", "query": "bowl"}}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(structured(&refused)["error"]["code"], "validation");
    assert!(fixture.site.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn download_saves_hashed_files_and_a_source_record() {
    let fixture = fixture(&[Site::Printables]).await;
    mount_printables_model(&fixture, 200).await;
    let request = json!({"action": "download", "params": {
        "site": "printables",
        "model": "https://www.printables.com/model/553277-low-poly-tray",
        "files": ["stl-1", "stl-2", "stl-1"],
        "project_id": "bowls"
    }});
    let result = call(&fixture.server, "library", request.clone()).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let value = structured(&result);
    let files = value["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        files[0]["artifact"]["path"],
        "projects/bowls/library/printables-553277/Tray.stl"
    );
    assert_eq!(
        files[1]["artifact"]["path"],
        "projects/bowls/library/printables-553277/Tray-stl-2.stl"
    );
    let expected: String = Sha256::digest(b"solid 1\n")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(files[0]["sha256"], expected);
    assert_eq!(value["license"]["name"], "Creative Commons — Attribution");

    let read = call(
        &fixture.server,
        "artifact",
        json!({"action": "read", "params": {"path": value["manifest"]["path"]}}),
    )
    .await;
    let encoded = structured(&read)["data_base64"].as_str().unwrap();
    use base64::Engine as _;
    let manifest: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["site"], "printables");
    assert_eq!(
        manifest["url"],
        "https://www.printables.com/model/553277-low-poly-tray"
    );
    assert_eq!(manifest["designer"], "Markus");
    assert_eq!(manifest["files"][0]["sha256"], expected);
    assert_eq!(manifest["files"][1]["name"], "sub/Tray.stl");

    // Existing files are refused before anything is downloaded again.
    let before = fixture.files.received_requests().await.unwrap().len();
    let repeated = call(&fixture.server, "library", request.clone()).await;
    assert_eq!(repeated.is_error, Some(true));
    assert_eq!(structured(&repeated)["error"]["code"], "validation");
    assert_eq!(
        fixture.files.received_requests().await.unwrap().len(),
        before
    );

    let mut replace = request;
    replace["params"]["overwrite"] = json!(true);
    let replaced = call(&fixture.server, "library", replace).await;
    assert_ne!(replaced.is_error, Some(true), "{replaced:?}");
}

#[tokio::test]
async fn a_failed_transfer_leaves_the_project_unchanged() {
    let fixture = fixture(&[Site::Printables]).await;
    mount_printables_model(&fixture, 503).await;
    let failed = call(
        &fixture.server,
        "library",
        json!({"action": "download", "params": {
            "site": "printables", "model": "553277",
            "files": ["stl-1", "stl-2"], "project_id": "bowls"
        }}),
    )
    .await;
    assert_eq!(failed.is_error, Some(true));
    assert_eq!(structured(&failed)["error"]["code"], "library_unavailable");
    let listed = call(
        &fixture.server,
        "project",
        json!({"action": "files", "params": {"project_id": "bowls"}}),
    )
    .await;
    let entries = structured(&listed)["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| !entry["path"].as_str().unwrap().contains("library/")),
        "{entries:?}"
    );
}

#[tokio::test]
async fn unsupported_and_unknown_files_are_refused_before_any_transfer() {
    let fixture = fixture(&[Site::Printables]).await;
    mount_printables_model(&fixture, 200).await;
    for (files, message) in [
        (json!(["other-3"]), "unsupported type .pdf"),
        (json!(["stl-9"]), "lists no file stl-9"),
    ] {
        let refused = call(
            &fixture.server,
            "library",
            json!({"action": "download", "params": {
                "site": "printables", "model": "553277", "files": files, "project_id": "bowls"
            }}),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        let error = &structured(&refused)["error"];
        assert_eq!(error["code"], "validation");
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
    }
    let missing_project = call(
        &fixture.server,
        "library",
        json!({"action": "download", "params": {
            "site": "printables", "model": "553277", "files": ["stl-1"], "project_id": "absent"
        }}),
    )
    .await;
    assert_eq!(missing_project.is_error, Some(true));
    assert!(fixture.files.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn makerworld_downloads_without_a_token_fail_before_contacting_the_site() {
    let fixture = fixture(&[Site::MakerWorld]).await;
    let refused = call(
        &fixture.server,
        "library",
        json!({"action": "download", "params": {
            "site": "makerworld", "model": "1514079", "files": ["1"], "project_id": "bowls"
        }}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(
        structured(&refused)["error"]["code"],
        "library_credentials_required"
    );
    assert!(fixture.site.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_commit_failure_after_saving_names_the_saved_files() {
    let fixture = fixture(&[Site::Printables]).await;
    mount_printables_model(&fixture, 200).await;
    std::fs::create_dir_all(
        fixture
            ._workspace
            .path()
            .join("projects/bowls/library/printables-553277/Tray-stl-2.stl"),
    )
    .unwrap();
    let failed = call(
        &fixture.server,
        "library",
        json!({"action": "download", "params": {
            "site": "printables", "model": "553277",
            "files": ["stl-1", "stl-2"], "project_id": "bowls"
        }}),
    )
    .await;
    assert_eq!(failed.is_error, Some(true));
    let error = &structured(&failed)["error"];
    assert_eq!(error["code"], "library_incomplete");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("projects/bowls/library/printables-553277/Tray.stl"),
        "{error}"
    );
}

#[tokio::test]
async fn a_site_file_named_like_the_record_does_not_replace_it() {
    let fixture = fixture(&[Site::Printables]).await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({"operationName": "ModelFiles"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"print": {
                "id": "7", "name": "Settings", "otherFiles": [{"id": "9", "name": "source.json"}]
            }}})),
        )
        .mount(&fixture.site)
        .await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(
            json!({"operationName": "GetDownloadLink"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"getDownloadLink": {
                "ok": true, "output": {"link": format!("{}/files/source.json", fixture.files.uri())}
            }}}),
        ))
        .mount(&fixture.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/source.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"site\": \"forged\"}"))
        .mount(&fixture.files)
        .await;
    let result = call(
        &fixture.server,
        "library",
        json!({"action": "download", "params": {
            "site": "printables", "model": "7", "files": ["other-9"], "project_id": "bowls"
        }}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let value = structured(&result);
    assert_eq!(
        value["files"][0]["artifact"]["path"],
        "projects/bowls/library/printables-7/source-other-9.json"
    );
    assert_eq!(
        value["manifest"]["path"],
        "projects/bowls/library/printables-7/source.json"
    );
}

const ACCOUNT: &str = "PRINTABLE_BAMBU_ACCOUNT";
const PASSWORD: &str = "PRINTABLE_BAMBU_PASSWORD";
const LOGIN: &str = "/makerworld/v1/user-service/user/login";
const EMAIL_CODE: &str = "/makerworld/v1/user-service/user/sendemail/code";
/// RFC 6238's test key, "12345678901234567890", in base32.
const AUTHENTICATOR: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

fn account_settings(extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        (ACCOUNT, "maker@example.com".to_owned()),
        (PASSWORD, "correct horse".to_owned()),
    ];
    settings.extend_from_slice(extra);
    settings
}

/// A MakerWorld model whose one print profile downloads with `token`.
async fn mount_makerworld_model(fixture: &Fixture, token: &str) {
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "modelId": "US2b", "title": "Bowl",
            "instances": [{"profileId": 77, "title": "Default"}]
        })))
        .mount(&fixture.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/iot-service/api/user/profile/77"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {token}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": format!("{}/files/bowl.3mf", fixture.files.uri())
        })))
        .mount(&fixture.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/bowl.3mf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04bowl".to_vec()))
        .mount(&fixture.files)
        .await;
}

async fn password_login(fixture: &Fixture, response: Value, times: u64) {
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(body_partial_json(
            json!({"account": "maker@example.com", "password": "correct horse"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(times)
        .mount(&fixture.site)
        .await;
}

async fn emailed_code(fixture: &Fixture, code: &str, token: &str, emails: u64) {
    Mock::given(method("POST"))
        .and(path(EMAIL_CODE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(emails)
        .mount(&fixture.site)
        .await;
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(body_partial_json(
            json!({"account": "maker@example.com", "code": code}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"accessToken": token, "expiresIn": 7_776_000})),
        )
        .expect(1)
        .mount(&fixture.site)
        .await;
}

fn download_bowl() -> Value {
    json!({"action": "download", "params": {
        "site": "makerworld", "model": "42", "files": ["77"], "project_id": "bowls"
    }})
}

fn sign_in(code: Option<&str>) -> Value {
    let mut params = json!({"site": "makerworld"});
    if let Some(code) = code {
        params["code"] = json!(code);
    }
    json!({"action": "sign_in", "params": params})
}

#[tokio::test]
async fn an_emailed_code_is_asked_for_once_then_downloads_proceed() {
    let fixture = fixture_with(&[Site::MakerWorld], &account_settings(&[])).await;
    password_login(&fixture, json!({"loginType": "verifyCode"}), 1).await;
    emailed_code(&fixture, "123456", "AAB_session", 1).await;
    mount_makerworld_model(&fixture, "AAB_session").await;

    for _ in 0..2 {
        let refused = call(&fixture.server, "library", download_bowl()).await;
        assert_eq!(refused.is_error, Some(true));
        let error = &structured(&refused)["error"];
        assert_eq!(error["code"], "library_sign_in_required");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("emailed a sign-in code"), "{message}");
        assert!(message.contains(r#""site": "makerworld""#), "{message}");
    }
    let status = call(&fixture.server, "status", json!({"detail": true})).await;
    assert_eq!(
        structured(&status)["library"]["makerworld_sign_in"]["state"],
        "code_required"
    );

    let signed_in = call(&fixture.server, "library", sign_in(Some("123456"))).await;
    assert_ne!(signed_in.is_error, Some(true), "{signed_in:?}");
    assert_eq!(structured(&signed_in)["state"], "signed_in");
    let downloaded = call(&fixture.server, "library", download_bowl()).await;
    assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
    assert_eq!(
        structured(&downloaded)["files"][0]["artifact"]["path"],
        "projects/bowls/library/makerworld-42/Default.3mf"
    );
}

#[tokio::test]
async fn a_configured_authenticator_secret_signs_in_without_the_user() {
    let fixture = fixture_with(
        &[Site::MakerWorld],
        &account_settings(&[(
            "PRINTABLE_BAMBU_AUTHENTICATOR_SECRET",
            AUTHENTICATOR.to_lowercase(),
        )]),
    )
    .await;
    password_login(&fixture, json!({"loginType": "tfa", "tfaKey": "key-1"}), 1).await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(ResponseTemplate::new(204).insert_header("set-cookie", "bbl_csrf_token=c1"))
        .mount(&fixture.site)
        .await;
    let secret = model_library::AuthenticatorSecret::new(AUTHENTICATOR).unwrap();
    Mock::given(method("POST"))
        .and(path("/bambu-web/api/sign-in/tfa"))
        .and(move |request: &wiremock::Request| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            [now - 30, now, now + 30]
                .iter()
                .any(|time| body["tfaCode"] == secret.code_at(*time))
        })
        .respond_with(
            ResponseTemplate::new(200).insert_header("set-cookie", "token=AAB_auto; Path=/"),
        )
        .expect(1)
        .mount(&fixture.site)
        .await;
    mount_makerworld_model(&fixture, "AAB_auto").await;

    let downloaded = call(&fixture.server, "library", download_bowl()).await;
    assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
}

#[tokio::test]
async fn an_authenticator_prompt_falls_back_to_email_when_the_website_refuses() {
    let fixture = fixture_with(&[Site::MakerWorld], &account_settings(&[])).await;
    password_login(&fixture, json!({"loginType": "tfa", "tfaKey": "key-1"}), 1).await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("cf-mitigated", "challenge")
                .set_body_string("Just a moment..."),
        )
        .mount(&fixture.site)
        .await;
    emailed_code(&fixture, "246810", "AAB_fallback", 1).await;
    mount_makerworld_model(&fixture, "AAB_fallback").await;

    let refused = call(&fixture.server, "library", download_bowl()).await;
    let message = structured(&refused)["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("authenticator app"), "{message}");
    let fallback = call(&fixture.server, "library", sign_in(Some("135790"))).await;
    assert_ne!(fallback.is_error, Some(true), "{fallback:?}");
    let value = structured(&fallback);
    assert_eq!(value["state"], "code_required");
    assert_eq!(value["code_source"], "email");
    assert!(
        value["instructions"]
            .as_str()
            .unwrap()
            .contains("refused the authenticator step")
    );
    let signed_in = call(&fixture.server, "library", sign_in(Some("246810"))).await;
    assert_eq!(structured(&signed_in)["state"], "signed_in");
    let downloaded = call(&fixture.server, "library", download_bowl()).await;
    assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
}

#[tokio::test]
async fn a_rejected_token_is_replaced_by_signing_in_again() {
    let fixture = fixture_with(
        &[Site::MakerWorld],
        &account_settings(&[("PRINTABLE_MAKERWORLD_TOKEN", "AAB_old".to_owned())]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/iot-service/api/user/profile/77"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer AAB_old",
        ))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"code": 4, "error": "Please login."})),
        )
        .expect(1)
        .mount(&fixture.site)
        .await;
    password_login(
        &fixture,
        json!({"accessToken": "AAB_new", "expiresIn": 7_776_000}),
        1,
    )
    .await;
    mount_makerworld_model(&fixture, "AAB_new").await;

    let downloaded = call(&fixture.server, "library", download_bowl()).await;
    assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
}

#[tokio::test]
async fn a_signed_in_session_survives_a_restart_in_the_state_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let state = tempfile::tempdir().unwrap();
    let directory = state.path().to_str().unwrap().to_owned();
    let settings = account_settings(&[("PRINTABLE_LIBRARY_STATE_DIR", directory.clone())]);
    let fixture = fixture_with(&[Site::MakerWorld], &settings).await;
    password_login(
        &fixture,
        json!({"accessToken": "AAB_stored", "expiresIn": 7_776_000}),
        1,
    )
    .await;
    let signed_in = call(&fixture.server, "library", sign_in(None)).await;
    assert_eq!(
        structured(&signed_in)["state"],
        "signed_in",
        "{signed_in:?}"
    );

    let file = state.path().join("makerworld-session.json");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let lookup = |settings: Vec<(&'static str, String)>| {
        move |key: &str| {
            settings
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        }
    };
    let restarted = account::MakerWorldAccount::configure(&lookup(settings.clone())).unwrap();
    assert_eq!(
        restarted.initial_token().unwrap().expose_secret(),
        "AAB_stored"
    );
    let mut other = settings;
    other[0].1 = "someone@example.com".into();
    let other = account::MakerWorldAccount::configure(&lookup(other)).unwrap();
    assert!(other.initial_token().is_none());
}

#[tokio::test]
async fn sign_in_codes_need_a_waiting_sign_in_and_other_sites_do_not_sign_in() {
    let fixture = fixture_with(
        &[Site::MakerWorld, Site::Printables],
        &account_settings(&[]),
    )
    .await;
    let early = call(&fixture.server, "library", sign_in(Some("123456"))).await;
    assert_eq!(structured(&early)["error"]["code"], "validation");
    let other = call(
        &fixture.server,
        "library",
        json!({"action": "sign_in", "params": {"site": "printables"}}),
    )
    .await;
    assert_eq!(structured(&other)["error"]["code"], "validation");
    assert!(fixture.site.received_requests().await.unwrap().is_empty());
}

#[test]
fn sign_in_settings_must_be_complete_and_valid() {
    let configure = |pairs: &[(&str, &str)]| {
        account::MakerWorldAccount::configure(&|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
        .map(|_| ())
    };
    assert!(configure(&[]).is_ok());
    assert_eq!(
        configure(&[(ACCOUNT, "maker@example.com")]).unwrap_err(),
        SettingsError::Library(PASSWORD)
    );
    assert_eq!(
        configure(&[(PASSWORD, "pw")]).unwrap_err(),
        SettingsError::Library(ACCOUNT)
    );
    assert_eq!(
        configure(&[(ACCOUNT, "+15551234567"), (PASSWORD, "pw")]).unwrap_err(),
        SettingsError::Library(ACCOUNT)
    );
    assert_eq!(
        configure(&[("PRINTABLE_BAMBU_AUTHENTICATOR_SECRET", AUTHENTICATOR)]).unwrap_err(),
        SettingsError::Library(ACCOUNT)
    );
    assert_eq!(
        configure(&[
            (ACCOUNT, "maker@example.com"),
            (PASSWORD, "pw"),
            ("PRINTABLE_BAMBU_AUTHENTICATOR_SECRET", "short")
        ])
        .unwrap_err(),
        SettingsError::Library("PRINTABLE_BAMBU_AUTHENTICATOR_SECRET")
    );
    for directory in ["relative/state", "/nonexistent/printable-state"] {
        assert_eq!(
            configure(&[("PRINTABLE_LIBRARY_STATE_DIR", directory)]).unwrap_err(),
            SettingsError::Library("PRINTABLE_LIBRARY_STATE_DIR")
        );
    }
}

#[tokio::test]
async fn early_renewal_that_needs_the_user_is_not_retried_while_the_token_works() {
    use base64::Engine as _;

    let tomorrow = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 24 * 60 * 60;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{tomorrow}}}"#));
    let expiring = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.c2ln");
    let fixture = fixture_with(
        &[Site::MakerWorld],
        &account_settings(&[
            ("PRINTABLE_MAKERWORLD_TOKEN", expiring.clone()),
            ("PRINTABLE_BAMBU_AUTHENTICATOR_SECRET", AUTHENTICATOR.into()),
        ]),
    )
    .await;
    password_login(&fixture, json!({"loginType": "tfa", "tfaKey": "key-1"}), 1).await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(ResponseTemplate::new(403).insert_header("cf-mitigated", "challenge"))
        .mount(&fixture.site)
        .await;
    Mock::given(method("POST"))
        .and(path(EMAIL_CODE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&fixture.site)
        .await;
    mount_makerworld_model(&fixture, &expiring).await;

    for overwrite in [false, true] {
        let mut request = download_bowl();
        request["params"]["overwrite"] = json!(overwrite);
        let downloaded = call(&fixture.server, "library", request).await;
        assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
    }
    // The code Bambu Lab emailed during renewal can still be used.
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(body_partial_json(json!({"code": "777888"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"accessToken": "AAB_renewed", "expiresIn": 7_776_000})),
        )
        .expect(1)
        .mount(&fixture.site)
        .await;
    let renewed = call(&fixture.server, "library", sign_in(Some("777888"))).await;
    assert_eq!(structured(&renewed)["state"], "signed_in", "{renewed:?}");
}

fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    }
}

fn token_expiring_at(exp: u64) -> String {
    use base64::Engine as _;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    format!("eyJhbGciOiJSUzI1NiJ9.{payload}.c2ln")
}

#[tokio::test]
async fn invalid_downloads_are_refused_before_any_sign_in() {
    let fixture = fixture_with(&[Site::MakerWorld], &account_settings(&[])).await;
    mount_makerworld_model(&fixture, "unused").await;
    let mut request = download_bowl();
    request["params"]["files"] = json!(["9999"]);
    let refused = call(&fixture.server, "library", request).await;
    assert_eq!(structured(&refused)["error"]["code"], "validation");
    let requests = fixture.site.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().contains("user-service")),
        "signed in for an invalid request"
    );
}

#[tokio::test]
async fn a_late_rejection_does_not_discard_a_newer_session() {
    let fixture = fixture_with(&[], &[]).await;
    password_login(&fixture, json!({"accessToken": "AAB_new"}), 1).await;
    let client = Client::loopback(
        &url::Url::parse(&format!("{}/", fixture.site.uri())).unwrap(),
        Credentials::default(),
    )
    .unwrap();
    let account = account::MakerWorldAccount::configure(&lookup(account_settings(&[(
        "PRINTABLE_MAKERWORLD_TOKEN",
        "AAB_old".to_owned(),
    )])))
    .unwrap();
    let stale = account.ensure(&client).await.unwrap();
    let signed_in = account.sign_in(&client, None).await.unwrap();
    assert_eq!(signed_in["state"], "signed_in");
    account.invalidate(&client, stale).await;
    assert_eq!(account.initial_token().unwrap().expose_secret(), "AAB_new");
}

#[tokio::test]
async fn a_refused_authenticator_secret_falls_back_to_asking_the_user() {
    let fixture = fixture_with(
        &[Site::MakerWorld],
        &account_settings(&[("PRINTABLE_BAMBU_AUTHENTICATOR_SECRET", AUTHENTICATOR.into())]),
    )
    .await;
    password_login(&fixture, json!({"loginType": "tfa", "tfaKey": "key-1"}), 1).await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(ResponseTemplate::new(204).insert_header("set-cookie", "bbl_csrf_token=c1"))
        .mount(&fixture.site)
        .await;
    Mock::given(method("POST"))
        .and(path("/bambu-web/api/sign-in/tfa"))
        .and(body_partial_json(json!({"tfaCode": "111222"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"accessToken": "AAB_user", "expiresIn": 7_776_000})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&fixture.site)
        .await;
    Mock::given(method("POST"))
        .and(path("/bambu-web/api/sign-in/tfa"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "Invalid code"})))
        .with_priority(2)
        .expect(1)
        .mount(&fixture.site)
        .await;
    mount_makerworld_model(&fixture, "AAB_user").await;

    let refused = call(&fixture.server, "library", download_bowl()).await;
    let error = &structured(&refused)["error"];
    assert_eq!(error["code"], "library_sign_in_required");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("authenticator app")
    );
    let status = call(&fixture.server, "status", json!({"detail": true})).await;
    assert_eq!(
        structured(&status)["library"]["makerworld_sign_in"]["authenticator_refused"],
        true
    );
    let signed_in = call(&fixture.server, "library", sign_in(Some("111222"))).await;
    assert_eq!(
        structured(&signed_in)["state"],
        "signed_in",
        "{signed_in:?}"
    );
    let downloaded = call(&fixture.server, "library", download_bowl()).await;
    assert_ne!(downloaded.is_error, Some(true), "{downloaded:?}");
}

#[tokio::test]
async fn sign_in_without_a_code_returns_the_waiting_request_instead_of_emailing_again() {
    let fixture = fixture_with(&[Site::MakerWorld], &account_settings(&[])).await;
    password_login(&fixture, json!({"loginType": "verifyCode"}), 1).await;
    emailed_code(&fixture, "123456", "AAB_session", 1).await;
    for _ in 0..2 {
        let waiting = call(&fixture.server, "library", sign_in(None)).await;
        assert_eq!(structured(&waiting)["state"], "code_required");
        assert_eq!(structured(&waiting)["code_source"], "email");
    }
    let signed_in = call(&fixture.server, "library", sign_in(Some("123456"))).await;
    assert_eq!(structured(&signed_in)["state"], "signed_in");
}

#[tokio::test]
async fn an_expired_configured_token_without_an_account_is_reported_as_expired() {
    let fixture = fixture_with(
        &[Site::MakerWorld],
        &[(
            "PRINTABLE_MAKERWORLD_TOKEN",
            token_expiring_at(1_000_000_000),
        )],
    )
    .await;
    mount_makerworld_model(&fixture, "unused").await;
    let refused = call(&fixture.server, "library", download_bowl()).await;
    assert_eq!(
        structured(&refused)["error"]["code"],
        "library_authentication"
    );
}

#[test]
fn a_newly_configured_token_takes_precedence_over_an_older_stored_session() {
    let state = tempfile::tempdir().unwrap();
    let directory = state.path().to_str().unwrap().to_owned();
    let seed: String = Sha256::digest(b"AAB_seed")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let stored = serde_json::json!({
        "format_version": 1, "account": "maker@example.com", "token": "AAB_stored",
        "expires_at_unix": null, "seed_sha256": seed
    });
    std::fs::write(
        state.path().join("makerworld-session.json"),
        serde_json::to_vec(&stored).unwrap(),
    )
    .unwrap();
    let configured = |token: Option<&str>| {
        let mut settings = account_settings(&[("PRINTABLE_LIBRARY_STATE_DIR", directory.clone())]);
        if let Some(token) = token {
            settings.push(("PRINTABLE_MAKERWORLD_TOKEN", token.to_owned()));
        }
        account::MakerWorldAccount::configure(&lookup(settings))
            .unwrap()
            .initial_token()
            .unwrap()
            .expose_secret()
            .to_owned()
    };
    assert_eq!(configured(None), "AAB_stored");
    assert_eq!(configured(Some("AAB_seed")), "AAB_stored");
    assert_eq!(configured(Some("AAB_pasted")), "AAB_pasted");
}

#[test]
fn an_unwritable_state_directory_is_refused_at_startup() {
    use std::os::unix::fs::PermissionsExt as _;

    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = account::MakerWorldAccount::configure(&lookup(vec![(
        "PRINTABLE_LIBRARY_STATE_DIR",
        state.path().to_str().unwrap().to_owned(),
    )]));
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    if nix_is_root() {
        return;
    }
    assert_eq!(
        result.map(|_| ()).unwrap_err(),
        SettingsError::Library("PRINTABLE_LIBRARY_STATE_DIR")
    );
}

/// Root ignores directory permissions, so the unwritable case cannot be
/// observed there.
fn nix_is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}
