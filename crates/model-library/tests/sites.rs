//! Site clients against local fake sites. No request leaves the loopback
//! interface.

use model_library::{
    Client, Credentials, FileKind, FileStream, LibraryError, SearchQuery, SearchSort, Site, Token,
};
use serde_json::json;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{body_partial_json, header, method, path, query_param},
};

fn client(server: &MockServer, credentials: Credentials) -> Client {
    Client::loopback(
        &Url::parse(&format!("{}/", server.uri())).unwrap(),
        credentials,
    )
    .unwrap()
}

fn token(value: &str) -> Option<Token> {
    Some(Token::new(value.into()).unwrap())
}

fn query(text: &str, page: u32, limit: u32, sort: SearchSort) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        page,
        limit,
        sort,
    }
}

async fn body(mut stream: FileStream) -> Result<Vec<u8>, LibraryError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.chunk().await? {
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn no_authorization(request: &Request) -> bool {
    !request.headers.contains_key("authorization")
}

#[tokio::test]
async fn printables_search_maps_pages_designers_licenses_and_images() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({
            "operationName": "SearchModels",
            "variables": {"query": "bowl", "limit": 2, "offset": 2, "ordering": "popular"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"result": {
            "totalCount": 5,
            "items": [
                {"id": "553277", "name": "Low Poly\nBowl", "slug": "low-poly-tray-bowl",
                 "likesCount": 7414, "downloadCount": 35013,
                 "user": {"handle": "Markus710", "publicUsername": "Markus"},
                 "image": {"filePath": "media/prints/553277/a.png"},
                 "license": {"name": "CC BY-NC"}},
                {"id": 1, "name": "Plain", "slug": null, "user": null, "image": null, "license": null}
            ]
        }}})))
        .expect(1)
        .mount(&server)
        .await;

    let page = client(&server, Credentials::default())
        .search(
            Site::Printables,
            &query(" bowl ", 2, 2, SearchSort::Popular),
        )
        .await
        .unwrap();
    assert_eq!(
        (page.total, page.next_page, page.query.as_str()),
        (Some(5), Some(3), "bowl")
    );
    let first = &page.models[0];
    assert_eq!(first.title, "Low Poly Bowl");
    assert_eq!(
        first.url,
        "https://www.printables.com/model/553277-low-poly-tray-bowl"
    );
    assert_eq!(first.designer.as_deref(), Some("Markus"));
    assert_eq!(first.license.as_deref(), Some("CC BY-NC"));
    assert_eq!(
        first.image_url.as_deref(),
        Some("https://media.printables.com/media/prints/553277/a.png")
    );
    assert_eq!((first.likes, first.downloads), (Some(7414), Some(35013)));
    assert_eq!(page.models[1].url, "https://www.printables.com/model/1");
}

#[tokio::test]
async fn printables_files_download_through_a_link_without_credentials() {
    let site = MockServer::start().await;
    let files = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({"operationName": "ModelFiles", "variables": {"id": "553277"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"print": {
            "id": "553277", "name": "Low Poly Tray", "slug": "low-poly-tray-bowl",
            "summary": "A  tray.", "user": {"handle": "Markus710"},
            "license": {"name": "Creative Commons — Attribution — Noncommercial",
                        "abbreviation": "CC-BY-NC", "disallowRemixing": false},
            "stls": [{"id": "2351722", "name": "Tray.stl", "fileSize": 384084, "folder": "parts", "note": ""}],
            "gcodes": [{"id": 9, "name": "Tray_0.4n.gcode", "fileSize": "1000"}],
            "slas": [],
            "otherFiles": [{"id": "5", "name": "guide.pdf"}]
        }}})))
        .mount(&site)
        .await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({
            "operationName": "GetDownloadLink",
            "variables": {"printId": "553277", "source": "model_detail",
                          "files": [{"fileType": "stl", "ids": ["2351722"]}]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"getDownloadLink": {
            "ok": true, "errors": [],
            "output": {"link": null, "files": [{"id": "2351722", "link": format!("{}/media/Tray.stl?sig=a%2Fb", files.uri())}]}
        }}})))
        .expect(1)
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/Tray.stl"))
        .and(query_param("sig", "a/b"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"solid tray".to_vec()))
        .expect(1)
        .mount(&files)
        .await;

    let client = client(&site, Credentials::default());
    let model = client
        .model(
            Site::Printables,
            "https://www.printables.com/model/553277-low-poly-tray-bowl",
        )
        .await
        .unwrap();
    assert_eq!(model.designer.as_deref(), Some("Markus710"));
    assert_eq!(model.summary.as_deref(), Some("A tray."));
    let license = model.license.as_ref().unwrap();
    assert_eq!(license.remixing_allowed, Some(true));
    let ids: Vec<_> = model
        .files
        .iter()
        .map(|file| (file.file_id.as_str(), file.kind))
        .collect();
    assert_eq!(
        ids,
        [
            ("stl-2351722", FileKind::Model),
            ("gcode-9", FileKind::Sliced),
            ("other-5", FileKind::Other)
        ]
    );
    assert_eq!(model.files[0].note.as_deref(), Some("parts"));
    assert_eq!(model.files[1].size_bytes, Some(1000));
    let stream = client.open_file(&model, "stl-2351722", 1024).await.unwrap();
    assert_eq!(body(stream).await.unwrap(), b"solid tray");
    assert!(matches!(
        client.open_file(&model, "stl-404", 1024).await,
        Err(LibraryError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn printables_refused_links_report_the_site_reason() {
    let site = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({"operationName": "ModelFiles"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"print": {
                "id": "7", "name": "Paid", "stls": [{"id": "1", "name": "a.stl"}]
            }}})),
        )
        .mount(&site)
        .await;
    Mock::given(method("POST"))
        .and(path("/printables/graphql/"))
        .and(body_partial_json(json!({"operationName": "GetDownloadLink"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"getDownloadLink": {
            "ok": false, "errors": [{"field": "files", "messages": ["Purchase required"]}], "output": null
        }}})))
        .mount(&site)
        .await;
    let client = client(&site, Credentials::default());
    let model = client.model(Site::Printables, "7").await.unwrap();
    assert_eq!(
        client.open_file(&model, "stl-1", 1024).await.unwrap_err(),
        LibraryError::Forbidden {
            site: Site::Printables,
            reason: "Purchase required".into()
        }
    );

    let missing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"print": null}})))
        .mount(&missing)
        .await;
    assert_eq!(
        self::client(&missing, Credentials::default())
            .model(Site::Printables, "8")
            .await
            .unwrap_err(),
        LibraryError::NotFound {
            site: Site::Printables
        }
    );
}

#[tokio::test]
async fn makerworld_search_and_profiles_need_no_token_but_downloads_do() {
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/search-service/select/design2"))
        .and(query_param("keyword", "mesh bowl"))
        .and(query_param("limit", "3"))
        .and(query_param("offset", "0"))
        .and(query_param("orderBy", "hotScore"))
        .and(no_authorization)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total": 1,
            "hits": [{"id": 1514079, "slug": "mesh-bowl", "title": "Mesh Bowl", "cover": "https://makerworld.bblmw.com/c.jpg",
                      "license": "BY-SA", "likeCount": 4696, "downloadCount": 8676,
                      "designCreator": {"name": "KL Worx"}}]
        })))
        .expect(1)
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/1514079"))
        .and(no_authorization)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1514079, "modelId": "US9b4978f0afd3e0", "slug": "mesh-bowl", "title": "Mesh Bowl",
            "summary": "<p>Print <b>vase</b> mode</p><script>x()</script>", "license": "BY-SA",
            "coverUrl": "https://makerworld.bblmw.com/c.jpg", "designCreator": {"name": "KL Worx"},
            "instances": [
                {"id": 1, "profileId": 1580000, "title": "0.2mm layer",
                 "extention": {"modelInfo": {"compatibility": {"devProductName": "A1"}}}},
                {"id": 2, "profileId": "1580001", "title": null}
            ]
        })))
        .mount(&site)
        .await;

    let anonymous = client(&site, Credentials::default());
    let page = anonymous
        .search(
            Site::MakerWorld,
            &query("mesh bowl", 1, 3, SearchSort::Popular),
        )
        .await
        .unwrap();
    assert_eq!(page.next_page, None);
    assert_eq!(
        page.models[0].url,
        "https://makerworld.com/en/models/1514079-mesh-bowl"
    );
    assert_eq!(page.models[0].designer.as_deref(), Some("KL Worx"));
    assert!(matches!(
        anonymous
            .search(Site::MakerWorld, &query("bowl", 1, 3, SearchSort::Newest))
            .await,
        Err(LibraryError::InvalidRequest(_))
    ));

    let model = anonymous
        .model(
            Site::MakerWorld,
            "https://makerworld.com/en/models/1514079-mesh-bowl#profileId-1580000",
        )
        .await
        .unwrap();
    assert_eq!(model.summary.as_deref(), Some("Print vase mode"));
    assert_eq!(model.files[0].name, "0.2mm layer.3mf");
    assert_eq!(model.files[0].kind, FileKind::PrintProfile);
    assert_eq!(model.files[0].note.as_deref(), Some("print profile for A1"));
    assert_eq!(model.files[1].name, "profile-1580001.3mf");
    assert_eq!(
        anonymous
            .open_file(&model, "1580000", 1024)
            .await
            .unwrap_err(),
        LibraryError::CredentialsRequired {
            site: Site::MakerWorld
        }
    );
    let rendered = serde_json::to_string(&model).unwrap();
    assert!(!rendered.contains("US9b4978f0afd3e0"), "{rendered}");
}

#[tokio::test]
async fn makerworld_signed_links_keep_their_exact_query_and_never_receive_the_token() {
    let site = MockServer::start().await;
    let storage = MockServer::start().await;
    let signed_query = "X-Amz-Credential=AKIA%2F20261024%2Fus-west-2%2Fs3%2Faws4_request&X-Amz-Signature=ab%2Bcd%3D&x=a+b";
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "modelId": "US2bb73b106683e5", "title": "Part",
            "instances": [{"profileId": 77, "title": "Default"}]
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/iot-service/api/user/profile/77"))
        .and(query_param("model_id", "US2bb73b106683e5"))
        .and(header("authorization", "Bearer AAB_fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": format!("{}/bucket/part.3mf?{signed_query}", storage.uri()), "name": "part.3mf"
        })))
        .expect(1)
        .mount(&site)
        .await;
    let expected = signed_query.to_owned();
    Mock::given(method("GET"))
        .and(path("/bucket/part.3mf"))
        .and(no_authorization)
        .and(move |request: &Request| request.url.query() == Some(expected.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04".to_vec()))
        .expect(1)
        .mount(&storage)
        .await;

    let client = client(
        &site,
        Credentials {
            makerworld: token("AAB_fixture"),
            thingiverse: None,
        },
    );
    let model = client.model(Site::MakerWorld, "42").await.unwrap();
    let stream = client.open_file(&model, "77", 1024).await.unwrap();
    assert_eq!(body(stream).await.unwrap(), b"PK\x03\x04");
}

#[tokio::test]
async fn makerworld_refusals_are_classified() {
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/1"))
        .respond_with(
            ResponseTemplate::new(418)
                .set_body_json(json!({"captchaId": "c", "error": "We need to confirm"})),
        )
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 2, "modelId": "US1", "title": "Points", "instances": [{"profileId": 3}]
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/iot-service/api/user/profile/3"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"error": "Redeem points to download"})),
        )
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/4"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&site)
        .await;
    let client = client(
        &site,
        Credentials {
            makerworld: token("AAB_fixture"),
            thingiverse: None,
        },
    );
    assert_eq!(
        client.model(Site::MakerWorld, "1").await.unwrap_err(),
        LibraryError::Challenged {
            site: Site::MakerWorld
        }
    );
    let model = client.model(Site::MakerWorld, "2").await.unwrap();
    assert_eq!(
        client.open_file(&model, "3", 1024).await.unwrap_err(),
        LibraryError::Forbidden {
            site: Site::MakerWorld,
            reason: "Redeem points to download".into()
        }
    );
    assert_eq!(
        client.model(Site::MakerWorld, "4").await.unwrap_err(),
        LibraryError::RateLimited {
            site: Site::MakerWorld
        }
    );
}

#[tokio::test]
async fn thingiverse_sends_its_token_only_to_the_api_origin() {
    let site = MockServer::start().await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/thingiverse/search/raspberry%20pi%2Fcase"))
        .and(query_param("type", "things"))
        .and(query_param("per_page", "30"))
        .and(query_param("page", "2"))
        .and(query_param("sort", "newest"))
        .and(header("authorization", "Bearer tv_fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total": 31,
            "hits": [{"id": 763622, "name": "Case", "public_url": "https://www.thingiverse.com/thing:763622",
                      "thumbnail": "https://cdn.thingiverse.com/t.jpg", "creator": {"name": "maker"}, "like_count": 3}]
        })))
        .expect(1)
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/thingiverse/things/763622"))
        .and(header("authorization", "Bearer tv_fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 763622, "name": "Case", "public_url": "https://evil.example/thing",
            "license": "Creative Commons - Attribution", "allows_derivatives": true,
            "description": "Fits &lt;b&gt;Pi 4&lt;/b&gt;"
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/thingiverse/things/763622/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "case.STL", "size": 12, "download_url": format!("{}/thingiverse/files/1/download", site.uri()), "direct_url": null},
            {"id": 2, "name": "notes.pdf", "size": "3", "download_url": null, "direct_url": format!("{}/assets/notes.pdf", cdn.uri())}
        ])))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/thingiverse/files/1/download"))
        .and(header("authorization", "Bearer tv_fixture"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/assets/case.stl", cdn.uri())),
        )
        .expect(1)
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/case.stl"))
        .and(no_authorization)
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"solid case\n".to_vec()))
        .expect(1)
        .mount(&cdn)
        .await;

    let client = client(
        &site,
        Credentials {
            makerworld: None,
            thingiverse: token("tv_fixture"),
        },
    );
    let page = client
        .search(
            Site::Thingiverse,
            &query("raspberry pi/case", 2, 50, SearchSort::Newest),
        )
        .await
        .unwrap();
    assert_eq!((page.limit, page.next_page), (30, None));
    let model = client
        .model(Site::Thingiverse, "thing:763622")
        .await
        .unwrap();
    assert_eq!(model.url, "https://www.thingiverse.com/thing:763622");
    assert_eq!(model.summary.as_deref(), Some("Fits <b>Pi 4</b>"));
    assert_eq!(model.license.as_ref().unwrap().remixing_allowed, Some(true));
    assert_eq!(model.files[0].kind, FileKind::Model);
    assert_eq!(model.files[1].size_bytes, Some(3));
    let stream = client.open_file(&model, "1", 1024).await.unwrap();
    assert_eq!(body(stream).await.unwrap(), b"solid case\n");

    let anonymous = self::client(&site, Credentials::default());
    assert_eq!(
        anonymous.model(Site::Thingiverse, "1").await.unwrap_err(),
        LibraryError::CredentialsRequired {
            site: Site::Thingiverse
        }
    );
}

#[tokio::test]
async fn thingiverse_accepts_a_bare_result_list() {
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/thingiverse/search/vase"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "1", "name": "Vase"}, {"id": "2", "name": "Vase 2"}
        ])))
        .mount(&site)
        .await;
    let page = client(
        &site,
        Credentials {
            makerworld: None,
            thingiverse: token("tv"),
        },
    )
    .search(
        Site::Thingiverse,
        &query("vase", 1, 2, SearchSort::Relevance),
    )
    .await
    .unwrap();
    assert_eq!(
        (page.total, page.next_page, page.models.len()),
        (None, Some(2), 2)
    );
}

#[tokio::test]
async fn downloads_refuse_foreign_hosts_redirect_loops_and_oversized_bodies() {
    let site = MockServer::start().await;
    let thingiverse = || Credentials {
        makerworld: None,
        thingiverse: token("tv"),
    };
    Mock::given(method("GET"))
        .and(path("/thingiverse/things/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1, "name": "x"})))
        .mount(&site)
        .await;
    let file = |id: u32, location: String| json!({"id": id, "name": format!("{id}.stl"), "direct_url": location});
    Mock::given(method("GET"))
        .and(path("/thingiverse/things/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            file(1, format!("{}/foreign", site.uri())),
            file(2, format!("{}/loop", site.uri())),
            file(3, format!("{}/large", site.uri())),
        ])))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/foreign"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/x.stl"),
        )
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .expect(model_library::MAXIMUM_REDIRECTS as u64 + 1)
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/large"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 2048]))
        .mount(&site)
        .await;
    let client = client(&site, thingiverse());
    let model = client.model(Site::Thingiverse, "1").await.unwrap();
    assert_eq!(
        client.open_file(&model, "1", 1024).await.unwrap_err(),
        LibraryError::DisallowedHost {
            site: Site::Thingiverse
        }
    );
    assert_eq!(
        client.open_file(&model, "2", 1024).await.unwrap_err(),
        LibraryError::IncompatibleResponse {
            site: Site::Thingiverse
        }
    );
    assert_eq!(
        client.open_file(&model, "3", 1024).await.unwrap_err(),
        LibraryError::FileTooLarge { limit: 1024 }
    );
    let stream = client.open_file(&model, "3", 4096).await.unwrap();
    assert_eq!(stream.declared_size(), Some(2048));
    assert_eq!(body(stream).await.unwrap().len(), 2048);
}

#[tokio::test]
async fn oversized_api_responses_are_refused() {
    let site = MockServer::start().await;
    let padding = "x".repeat(8 * 1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1, "title": padding})))
        .mount(&site)
        .await;
    assert_eq!(
        client(&site, Credentials::default())
            .model(Site::MakerWorld, "1")
            .await
            .unwrap_err(),
        LibraryError::ResponseTooLarge {
            site: Site::MakerWorld
        }
    );
}

#[tokio::test]
async fn invalid_queries_fail_before_any_request() {
    let site = MockServer::start().await;
    let client = client(&site, Credentials::default());
    for (text, page, limit) in [
        (" ", 1, 10),
        ("bowl", 0, 10),
        ("bowl", 1, 0),
        ("bowl", 1, 51),
    ] {
        assert!(matches!(
            client
                .search(
                    Site::Printables,
                    &query(text, page, limit, SearchSort::Relevance)
                )
                .await,
            Err(LibraryError::InvalidRequest(_))
        ));
    }
    let long: String = "a".repeat(201);
    assert!(
        client
            .search(
                Site::Printables,
                &query(&long, 1, 10, SearchSort::Relevance)
            )
            .await
            .is_err()
    );
    assert!(site.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_file_host_cannot_redirect_the_token_back_to_the_api() {
    let site = MockServer::start().await;
    let storage = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/design-service/design/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "modelId": "US2b", "title": "Part", "instances": [{"profileId": 77}]
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/iot-service/api/user/profile/77"))
        .and(header("authorization", "Bearer AAB_fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": format!("{}/bucket/part.3mf", storage.uri())
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/bucket/part.3mf"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}/makerworld/v1/user-service/my/devices", site.uri()),
        ))
        .mount(&storage)
        .await;
    Mock::given(method("GET"))
        .and(path("/makerworld/v1/user-service/my/devices"))
        .and(no_authorization)
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&site)
        .await;
    let client = client(
        &site,
        Credentials {
            makerworld: token("AAB_fixture"),
            thingiverse: None,
        },
    );
    let model = client.model(Site::MakerWorld, "42").await.unwrap();
    assert!(matches!(
        client.open_file(&model, "77", 1024).await,
        Err(LibraryError::Forbidden { .. })
    ));
}

#[tokio::test]
async fn printables_field_errors_are_refusals_and_missing_data_is_incompatible() {
    let site = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"operationName": "ModelFiles", "variables": {"id": "1"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"print": null},
            "errors": [{"message": "Club membership required", "path": ["print"]}]
        })))
        .mount(&site)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"operationName": "ModelFiles", "variables": {"id": "2"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{"message": "Cannot query field \"summary\""}]
        })))
        .mount(&site)
        .await;
    let client = client(&site, Credentials::default());
    assert_eq!(
        client.model(Site::Printables, "1").await.unwrap_err(),
        LibraryError::Forbidden {
            site: Site::Printables,
            reason: "Club membership required".into()
        }
    );
    assert_eq!(
        client.model(Site::Printables, "2").await.unwrap_err(),
        LibraryError::IncompatibleResponse {
            site: Site::Printables
        }
    );
}

#[tokio::test]
async fn the_last_allowed_page_offers_no_next_page() {
    let site = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"result": {
                "totalCount": 1_000_000, "items": [{"id": "1", "name": "Bowl"}]
            }}})),
        )
        .mount(&site)
        .await;
    let page = client(&site, Credentials::default())
        .search(
            Site::Printables,
            &query("bowl", 1000, 1, SearchSort::Relevance),
        )
        .await
        .unwrap();
    assert_eq!(page.next_page, None);
}
