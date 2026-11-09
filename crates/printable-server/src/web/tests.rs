use std::collections::HashMap;

use axum::http::Request as HttpRequest;
use serde_json::json;
use tower::ServiceExt as _;

use super::*;
use crate::projects::{
    card::tests::{create, print, slice, tag},
    cover::{CoverParams, CoverSource, Plan},
    index::PassKind,
};
use crate::slicing::package::fixture;

const USER_HEADER: &str = "x-signed-in-user";
const PROXY: &str = "10.0.0.2:41000";

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

fn enabled(overrides: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut pairs = vec![
        (LISTEN_KEY, "0.0.0.0:8080"),
        (PROXIES_KEY, "10.0.0.2, 127.0.0.1"),
        (USER_HEADER_KEY, "X-Signed-In-User"),
        (USERS_KEY, "alice, bob"),
    ];
    for (key, value) in overrides {
        pairs.retain(|(existing, _)| existing != key);
        pairs.push((*key, *value));
    }
    pairs
}

fn settings() -> WebSettings {
    configure(&lookup(&enabled(&[]))).unwrap().unwrap()
}

#[test]
fn settings_are_off_by_default_and_complete_when_on() {
    assert!(configure(&lookup(&[])).unwrap().is_none());
    assert!(configure(&lookup(&[(LISTEN_KEY, "  ")])).unwrap().is_none());
    let web = settings();
    assert_eq!(web.listen, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
    assert_eq!(web.user_header.as_str(), USER_HEADER);
    assert_eq!(web.users, ["alice", "bob"]);
    for key in [PROXIES_KEY, USER_HEADER_KEY, USERS_KEY] {
        let mut pairs = enabled(&[]);
        pairs.retain(|(existing, _)| *existing != key);
        assert_eq!(
            configure(&lookup(&pairs)).unwrap_err(),
            SettingsError::Web(key)
        );
        assert_eq!(
            configure(&lookup(&enabled(&[(key, " , ")]))).unwrap_err(),
            SettingsError::Web(key)
        );
    }
}

#[test]
fn malformed_settings_are_refused_without_echoing_them() {
    let cases = [
        (LISTEN_KEY, "8080"),
        (LISTEN_KEY, "localhost:8080"),
        (LISTEN_KEY, "0.0.0.0:0"),
        (PROXIES_KEY, "proxy.internal"),
        (PROXIES_KEY, "10.0.0.0/33"),
        (PROXIES_KEY, "10.0.0.1/24"),
        (PROXIES_KEY, "10.0.0.0/0"),
        (PROXIES_KEY, "::/0"),
        (PROXIES_KEY, "10.0.0.0/"),
        (PROXIES_KEY, "10.0.0.0/+8"),
        (PROXIES_KEY, "::ffff:10.0.0.2"),
        (PROXIES_KEY, "10.0.0.2, 300.0.0.1"),
        (USER_HEADER_KEY, "X Signed In"),
        (USERS_KEY, "alice, b\u{7}ob"),
    ];
    for (key, value) in cases {
        let error = configure(&lookup(&enabled(&[(key, value)]))).unwrap_err();
        assert_eq!(error, SettingsError::Web(key), "{key}={value}");
        assert!(!error.to_string().contains(value));
    }
}

#[test]
fn trusted_ranges_match_addresses_in_either_form() {
    let web = configure(&lookup(&enabled(&[(
        PROXIES_KEY,
        "10.0.0.2, 172.18.0.0/16, fd00::/8",
    )])))
    .unwrap()
    .unwrap();
    for (address, trusted) in [
        ("10.0.0.2", true),
        ("10.0.0.3", false),
        ("172.18.5.4", true),
        ("172.19.0.1", false),
        ("::ffff:172.18.0.9", true),
        ("::ffff:10.0.0.3", false),
        ("fd12::1", true),
        ("fe80::1", false),
    ] {
        assert_eq!(web.trusts(address.parse().unwrap()), trusted, "{address}");
    }
}

/// A workspace with an indexed library and the web router over it.
struct Library {
    root: tempfile::TempDir,
    workspace: Arc<Workspace>,
    index: Arc<ProjectIndex>,
    router: Router,
}

impl Library {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(Some(root.path()), None).unwrap());
        let index = Arc::new(ProjectIndex::default());
        let router = router(settings(), Arc::clone(&workspace), Arc::clone(&index));
        let library = Self {
            root,
            workspace,
            index,
            router,
        };
        library.fill();
        library
            .index
            .pass(&library.workspace, PassKind::Rebuild)
            .unwrap();
        library
    }

    fn fill(&self) {
        let workspace = &*self.workspace;
        create(workspace, "desk", "Desk organizer", "Three drawers");
        tag(workspace, "desk", &["storage"]);
        let version = projects::get(workspace, "desk").unwrap().record_version;
        workspace
            .write_artifact("projects/desk/photos/cover.png", &fixture::png(6, 4), false)
            .unwrap();
        let params = CoverParams {
            project_id: "desk".into(),
            expected_record_version: version,
            source: CoverSource::Image {
                path: "photos/cover.png".into(),
            },
            timeout_seconds: 60.0,
        };
        let Plan::Store(picture) = cover::plan(workspace, &params).unwrap() else {
            panic!("an image cover is stored directly")
        };
        cover::commit(workspace, &params, Some(picture)).unwrap();
        // Larger than one streamed piece, so the download spans several.
        let model: Vec<u8> = (0..300_000_u32).map(|n| (n % 251) as u8).collect();
        workspace
            .write_artifact("projects/desk/parts/drawer.stl", &model, false)
            .unwrap();
        slice(workspace, "desk", "slice-1", 5_000);
        print(workspace, "desk", 1, "completed", 10_000);

        create(
            workspace,
            "evil",
            "<script>alert(\"name\")</script> & 'quoted'",
            "\"><img src=x onerror=alert(1)> <b>bold</b>",
        );
        tag(workspace, "evil", &["<i>tag</i>", "a\"b'c"]);
        workspace
            .write_artifact(
                "projects/evil/<img src=x onerror=alert(2)>.stl",
                b"solid",
                false,
            )
            .unwrap();
        for (directory, address, title) in [
            ("bad", "javascript:alert(3)", "<script>title</script>"),
            (
                "good",
                "https://printables.example/model/1",
                "Plain \"title\"",
            ),
        ] {
            workspace
                .write_artifact(
                    &format!("projects/evil/{directory}/source.json"),
                    &serde_json::to_vec(&json!({
                        "site": "printables", "model_id": "1", "url": address,
                        "title": title, "designer": "\"><svg onload=alert(4)>",
                        "license": {"name": "<u>CC</u>"}, "retrieved_unix_ms": 1_000,
                    }))
                    .unwrap(),
                    false,
                )
                .unwrap();
        }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        peer: Option<&str>,
        users: &[&str],
        headers: &[(HeaderName, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = HttpRequest::builder().method(method).uri(uri);
        for user in users {
            builder = builder.header(USER_HEADER, *user);
        }
        for (name, value) in headers {
            builder = builder.header(name, *value);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        self.send(Method::GET, uri, Some(PROXY), &["alice"], &[])
            .await
    }

    async fn page(&self, uri: &str) -> String {
        let (status, headers, body) = self.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        String::from_utf8(body).unwrap()
    }
}

fn assert_secure(headers: &HeaderMap) {
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        "default-src 'self'; script-src 'none'; img-src 'self'; style-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(headers.contains_key(header::CACHE_CONTROL));
}

/// No scripts, and nothing loaded from another site: only anchors may name
/// an external address, and they must not pass on the page.
fn assert_self_contained(page: &str) {
    assert!(!page.contains("<script"), "a script element");
    for tag in page.split('<').skip(1) {
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        let external =
            tag.contains("=\"http:") || tag.contains("=\"https:") || tag.contains("=\"//");
        if external {
            assert!(
                tag.starts_with("a ") && tag.contains("rel=\"noopener noreferrer\""),
                "external address outside a plain link: <{tag}>"
            );
        }
        assert!(
            !tag.contains("javascript:") && !tag.contains(" on"),
            "active content: <{tag}>"
        );
    }
}

#[tokio::test]
async fn untrusted_peers_are_refused_before_anything_else() {
    let library = Library::new();
    for peer in [None, Some("192.0.2.7:5000"), Some("[2001:db8::1]:5000")] {
        for method in [Method::GET, Method::POST] {
            let (status, headers, body) = library.send(method, "/", peer, &["alice"], &[]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{peer:?}");
            assert_secure(&headers);
            assert!(!String::from_utf8(body).unwrap().contains("Desk"));
        }
    }
    // An IPv4 proxy reached through a dual-stack listener is still trusted.
    let (status, _, _) = library
        .send(
            Method::GET,
            "/",
            Some("[::ffff:10.0.0.2]:41000"),
            &["alice"],
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn only_one_listed_user_may_read_and_only_by_reading() {
    let library = Library::new();
    let refused: [&[&str]; 5] = [
        &[],
        &["mallory"],
        &["Alice"],
        &["alice "],
        &["alice", "alice"],
    ];
    for users in refused {
        let (status, headers, _) = library
            .send(Method::GET, "/", Some(PROXY), users, &[])
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{users:?}");
        assert_secure(&headers);
    }
    for user in ["alice", "bob"] {
        let (status, headers, _) = library
            .send(Method::GET, "/", Some(PROXY), &[user], &[])
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_secure(&headers);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let (status, headers, _) = library
            .send(method, "/projects/desk", Some(PROXY), &["alice"], &[])
            .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(headers[header::ALLOW], "GET, HEAD");
        assert_secure(&headers);
    }
    let (status, headers, body) = library
        .send(Method::HEAD, "/", Some(PROXY), &["alice"], &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_secure(&headers);
    assert!(body.is_empty());
    let (status, headers, _) = library.get("/mcp").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_secure(&headers);
}

#[tokio::test]
async fn pages_escape_untrusted_text_and_load_nothing_external() {
    let library = Library::new();
    let grid = library.page("/").await;
    let project = library.page("/projects/evil").await;
    let search = library
        .page("/?q=%3Cscript%3E&tag=%22%3E%3Cb%3E&sort=name")
        .await;
    for page in [&grid, &project, &search] {
        assert_self_contained(page);
        for hostile in [
            "<script>alert",
            "<img src=x",
            "<b>bold",
            "<i>tag",
            "<svg onload",
            "<u>CC",
            "\"><b>",
        ] {
            assert!(!page.contains(hostile), "{hostile}");
        }
        assert!(
            page.contains("href=\"./static/app.css\"")
                || page.contains("href=\"../static/app.css\"")
        );
    }
    assert!(grid.contains("&lt;script&gt;alert(&quot;name&quot;)&lt;/script&gt; &amp; 'quoted'"));
    assert!(grid.contains("&lt;i&gt;tag&lt;/i&gt;"));
    assert!(grid.contains("src=\"./projects/desk/cover\""));
    assert!(project.contains("&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"));
    assert!(project.contains("&lt;script&gt;title&lt;/script&gt;"));
    assert!(project.contains("&quot;&gt;&lt;svg onload=alert(4)&gt;"));
    // The file name is shown as text and linked with every unsafe byte encoded.
    assert!(project.contains(
        "href=\"../projects/evil/files/%3Cimg%20src%3Dx%20onerror%3Dalert%282%29%3E.stl\""
    ));
    assert!(project.contains("&lt;img src=x onerror=alert(2)&gt;.stl"));
    // A site address is a plain link only when it is a web page.
    assert!(
        project.contains(
            "<a href=\"https://printables.example/model/1\" rel=\"noopener noreferrer\">"
        )
    );
    assert!(project.contains("javascript:alert(3)"));
    assert!(!project.contains("href=\"javascript:"));
    // The search echoes the query as text.
    assert!(search.contains("value=\"&lt;script&gt;\""));
    assert!(search.contains("value=\"&quot;&gt;&lt;b&gt;\""));
}

#[tokio::test]
async fn the_grid_shows_cards_filters_and_pages() {
    let library = Library::new();
    let grid = library.page("/").await;
    assert!(grid.contains("2 matching projects"), "{grid}");
    assert!(grid.contains("Printed ×1"));
    assert!(grid.contains("Not printed"));
    assert!(grid.contains("Searching by meaning is not configured"));
    assert!(grid.contains("2 of 2 in the library indexed"));
    let printed = library.page("/?printed=yes").await;
    assert!(printed.contains("1 matching project"));
    assert!(printed.contains("Desk organizer"));
    assert!(!printed.contains("&lt;script&gt;"));
    assert!(printed.contains("<option value=\"yes\" selected>Yes</option>"));
    let words = library.page("/?q=drawers").await;
    assert!(words.contains("Desk organizer"));
    assert!(words.contains("sorted by best match"));
    for (query, reason) in [
        ("/?page=0", "page"),
        ("/?page=43", "page"),
        ("/?printed=maybe", "printed"),
        ("/?sort=random", "sort"),
        ("/?q=a&q=b", "more than once"),
    ] {
        let (status, _, body) = library.get(query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert!(String::from_utf8(body).unwrap().contains(reason), "{query}");
    }
    let long = format!("/?q={}", "a".repeat(MAX_QUERY_BYTES + 1));
    assert_eq!(library.get(&long).await.0, StatusCode::BAD_REQUEST);
    let (status, _, body) = library.get(&format!("/?q={}", "w+".repeat(17))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("at most 16 words")
    );
}

#[tokio::test]
async fn project_pages_show_the_full_card() {
    let library = Library::new();
    let page = library.page("/projects/desk").await;
    for expected in [
        "Desk organizer",
        "Three drawers",
        "storage",
        "1 completed slice",
        "0.20mm Standard @BBL P1S",
        "1 recorded print: 1 completed",
        "Workshop X1C",
        "parts/drawer.stl",
        "293.0 KiB",
        "src=\"../projects/desk/cover\"",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    let (status, headers, _) = library.get("/projects/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_secure(&headers);
    let (status, _, _) = library.get("/projects/..").await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn covers_are_served_as_stored_with_their_media_type() {
    let library = Library::new();
    let cover = projects::get(&library.workspace, "desk")
        .unwrap()
        .cover
        .unwrap();
    let stored = library.workspace.read_artifact(&cover.path).unwrap().1;
    let (status, headers, body) = library.get("/projects/desk/cover").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert_secure(&headers);
    assert_eq!(body, stored);
    let tag = headers[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(tag, format!("\"{}\"", cover.sha256));
    let (status, _, body) = library
        .send(
            Method::GET,
            "/projects/desk/cover",
            Some(PROXY),
            &["alice"],
            &[(header::IF_NONE_MATCH, tag.as_str())],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(
        library.get("/projects/evil/cover").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn generated_plate_pictures_show_projects_without_a_cover() {
    let library = Library::new();
    let workspace = &*library.workspace;
    let plan = pictures::Plan {
        render: false,
        project_id: "evil".into(),
        candidates: "candidates".into(),
        steps: Vec::new(),
    };
    let png = fixture::png(8, 6);
    let picture = pictures::store(
        workspace,
        "evil",
        &png,
        "projects/evil/model.stl",
        pictures::Origin::SlicerPreview,
        1_000,
    )
    .unwrap();
    pictures::record_made(workspace, &plan, picture.clone()).unwrap();
    library
        .index
        .pass(&library.workspace, PassKind::Incremental)
        .unwrap();
    let grid = library.page("/").await;
    assert!(
        grid.contains("src=\"./projects/evil/pictures/plate\""),
        "{grid}"
    );
    assert!(
        grid.contains("src=\"./projects/desk/cover\""),
        "a cover still wins"
    );
    let page = library.page("/projects/evil").await;
    assert!(page.contains("src=\"../projects/evil/pictures/plate\""));
    assert!(page.contains("Generated plate preview"));

    let (status, headers, body) = library.get("/projects/evil/pictures/plate").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_secure(&headers);
    assert_eq!(body, png);
    assert_eq!(
        headers[header::ETAG].to_str().unwrap(),
        format!("\"{}\"", picture.sha256)
    );
    assert_eq!(
        library.get("/projects/desk/pictures/plate").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        library.get("/projects/missing/pictures/plate").await.0,
        StatusCode::NOT_FOUND
    );
    let render_plan = pictures::Plan {
        render: true,
        ..plan
    };
    let render = pictures::store(
        workspace,
        "evil",
        &png,
        "projects/evil/model.stl",
        pictures::Origin::BlenderRender,
        2_000,
    )
    .unwrap();
    pictures::record_made(workspace, &render_plan, render).unwrap();
    library
        .index
        .pass(&library.workspace, PassKind::Incremental)
        .unwrap();
    let grid = library.page("/").await;
    assert!(grid.contains("src=\"./projects/evil/pictures/render\""));
    assert!(
        grid.contains("src=\"./projects/desk/cover\""),
        "an explicit cover still wins"
    );
    assert!(
        library
            .page("/projects/evil")
            .await
            .contains("src=\"../projects/evil/pictures/render\"")
    );
    let (status, headers, body) = library.get("/projects/evil/pictures/render").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(body, png);
    assert_secure(&headers);
    assert_eq!(
        library.get("/projects/evil/pictures/plate").await.0,
        StatusCode::OK,
        "both pictures remain available"
    );
}

#[tokio::test]
async fn downloads_in_progress_are_limited() {
    let library = Library::new();
    let uri = "/projects/desk/files/parts/drawer.stl";
    let request = || {
        let mut request = HttpRequest::builder()
            .uri(uri)
            .header(USER_HEADER, "alice")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(PROXY.parse::<SocketAddr>().unwrap()));
        request
    };
    // Responses whose bodies have not been read are downloads in progress.
    let mut open = Vec::new();
    for _ in 0..MAX_CONCURRENT_DOWNLOADS {
        let response = library.router.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response);
    }
    let (status, headers, _) = library.get(uri).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_secure(&headers);
    let (status, ..) = library
        .send(Method::HEAD, uri, Some(PROXY), &["alice"], &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    open.pop();
    let (status, ..) = library.get(uri).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn downloads_stream_project_files_and_nothing_else() {
    let library = Library::new();
    let expected = library
        .workspace
        .read_artifact("projects/desk/parts/drawer.stl")
        .unwrap()
        .1;
    let (status, headers, body) = library.get("/projects/desk/files/parts/drawer.stl").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, expected);
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(headers[header::CONTENT_LENGTH], expected.len().to_string());
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"drawer.stl\"; filename*=UTF-8''drawer.stl"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_secure(&headers);

    let (status, headers, body) = library
        .send(
            Method::HEAD,
            "/projects/desk/files/parts/drawer.stl",
            Some(PROXY),
            &["alice"],
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], expected.len().to_string());
    assert!(body.is_empty());

    let (status, headers, body) = library
        .get("/projects/evil/files/%3Cimg%20src%3Dx%20onerror%3Dalert%282%29%3E.stl")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"solid");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"_img_src_x_onerror_alert_2__.stl\"; \
         filename*=UTF-8''%3Cimg%20src%3Dx%20onerror%3Dalert%282%29%3E.stl"
    );

    std::os::unix::fs::symlink(
        library.root.path().join(".printable/projects/desk.json"),
        library.root.path().join("projects/desk/linked.json"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        library.root.path().join(".printable"),
        library.root.path().join("projects/desk/state"),
    )
    .unwrap();
    for refused in [
        "/projects/desk/files/../../.printable/projects/desk.json",
        "/projects/desk/files/%2e%2e/%2e%2e/.printable/projects/desk.json",
        "/projects/desk/files/parts%2Fdrawer.stl",
        "/projects/desk/files/..%2F..%2F.printable%2Fprojects%2Fdesk.json",
        "/projects/desk/files/parts%5cdrawer.stl",
        "/projects/desk/files/linked.json",
        "/projects/desk/files/state/projects/desk.json",
        "/projects/desk/files/parts",
        "/projects/desk/files/parts/missing.stl",
        "/projects/desk/files/notes.txt",
        "/projects/.printable/files/projects/desk.json",
        "/projects/desk/files/./parts/drawer.stl",
    ] {
        let (status, headers, body) = library.get(refused).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{refused}: {status}"
        );
        assert_secure(&headers);
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(!String::from_utf8_lossy(&body).contains("\"format_version\""));
    }
}

#[tokio::test]
async fn the_listener_reads_the_peer_address() {
    let library = Library::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = library
        .router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{address}/static/app.css");
    let allowed = client
        .get(&url)
        .header(USER_HEADER, "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert_eq!(
        allowed.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
    let refused = client.get(&url).send().await.unwrap();
    assert_eq!(refused.status(), 403);
    server.abort();
}
