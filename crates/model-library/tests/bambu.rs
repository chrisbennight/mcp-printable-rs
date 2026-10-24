//! Bambu Lab sign-in against a local fake API and website.

use data_encoding::BASE64URL_NOPAD;
use model_library::{
    BambuAccount, Client, CodeSource, Credentials, LibraryError, Password, SignIn, Site,
};
use serde_json::json;
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

const LOGIN: &str = "/makerworld/v1/user-service/user/login";
const EMAIL_CODE: &str = "/makerworld/v1/user-service/user/sendemail/code";

fn client(server: &MockServer) -> Client {
    Client::loopback(
        &Url::parse(&format!("{}/", server.uri())).unwrap(),
        Credentials::default(),
    )
    .unwrap()
}

fn account() -> BambuAccount {
    BambuAccount::new(
        "maker@example.com".into(),
        Password::new("correct horse".into()).unwrap(),
    )
    .unwrap()
}

fn jwt(exp: u64) -> String {
    let payload = BASE64URL_NOPAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
    format!("eyJhbGciOiJSUzI1NiJ9.{payload}.c2lnbmF0dXJl")
}

async fn password_login(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(body_json(json!({
            "account": "maker@example.com", "password": "correct horse", "apiError": ""
        })))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

async fn email_code_requested(server: &MockServer, times: u64) {
    Mock::given(method("POST"))
        .and(path(EMAIL_CODE))
        .and(body_json(
            json!({"email": "maker@example.com", "type": "codeLogin"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_password_sign_in_can_return_a_token_directly() {
    let server = MockServer::start().await;
    password_login(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "AAB_direct", "refreshToken": "AAB_direct",
            "loginType": "", "expiresIn": 7_776_000
        })),
    )
    .await;
    let SignIn::Complete(session) = client(&server).bambu_sign_in(&account()).await.unwrap() else {
        panic!("expected a token");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires = session.expires_at_unix.unwrap();
    assert!((now + 7_776_000 - 5..=now + 7_776_000 + 5).contains(&expires));
    assert!(!format!("{session:?}").contains("AAB_direct"));
}

#[tokio::test]
async fn an_emailed_code_completes_the_sign_in() {
    let server = MockServer::start().await;
    password_login(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(json!({"accessToken": "", "loginType": "verifyCode"})),
    )
    .await;
    email_code_requested(&server, 1).await;
    Mock::given(method("POST"))
        .and(path(LOGIN))
        .and(body_json(
            json!({"account": "maker@example.com", "code": "123456"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": jwt(1_900_000_000), "expiresIn": 0
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let SignIn::CodeRequired(challenge) = client.bambu_sign_in(&account()).await.unwrap() else {
        panic!("expected a code request");
    };
    assert_eq!(challenge.source(), CodeSource::Email);
    let SignIn::Complete(session) = client
        .bambu_submit_code(&account(), &challenge, " 123 456 ")
        .await
        .unwrap()
    else {
        panic!("expected a token");
    };
    assert_eq!(session.expires_at_unix, Some(1_900_000_000));
}

#[tokio::test]
async fn wrong_and_expired_email_codes_are_rejected_and_expiry_sends_a_new_code() {
    let server = MockServer::start().await;
    for (code, refusal) in [("111111", 2), ("222222", 1)] {
        Mock::given(method("POST"))
            .and(path(LOGIN))
            .and(body_json(
                json!({"account": "maker@example.com", "code": code}),
            ))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"code": refusal})))
            .mount(&server)
            .await;
    }
    email_code_requested(&server, 1).await;
    let client = client(&server);
    let challenge = {
        let password = MockServer::start().await;
        password_login(
            &password,
            ResponseTemplate::new(200).set_body_json(json!({"loginType": "verifyCode"})),
        )
        .await;
        email_code_requested(&password, 1).await;
        let SignIn::CodeRequired(challenge) = self::client(&password)
            .bambu_sign_in(&account())
            .await
            .unwrap()
        else {
            panic!("expected a code request");
        };
        challenge
    };
    assert_eq!(
        client
            .bambu_submit_code(&account(), &challenge, "111111")
            .await
            .unwrap_err(),
        LibraryError::SignInRejected {
            site: Site::MakerWorld,
            reason: "Bambu Lab did not accept the code".into()
        }
    );
    assert_eq!(
        client
            .bambu_submit_code(&account(), &challenge, "222222")
            .await
            .unwrap_err(),
        LibraryError::SignInCodeExpired {
            site: Site::MakerWorld
        }
    );
    assert!(matches!(
        client
            .bambu_submit_code(&account(), &challenge, "12ab")
            .await,
        Err(LibraryError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn an_authenticator_code_is_answered_on_the_website_with_its_csrf_cookie() {
    let server = MockServer::start().await;
    password_login(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"loginType": "tfa", "tfaKey": "key-1"})),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(
            ResponseTemplate::new(204)
                .insert_header("set-cookie", "bbl_csrf_token=csrf-1; Path=/; HttpOnly"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/bambu-web/api/sign-in/tfa"))
        .and(header("x-bbl-csrf-token", "csrf-1"))
        .and(header("cookie", "bbl_csrf_token=csrf-1"))
        .and(body_json(json!({"tfaKey": "key-1", "tfaCode": "654321"})))
        .respond_with(ResponseTemplate::new(200).insert_header(
            "set-cookie",
            format!("token={}; Path=/; Secure", jwt(1_950_000_000)),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let SignIn::CodeRequired(challenge) = client.bambu_sign_in(&account()).await.unwrap() else {
        panic!("expected a code request");
    };
    assert_eq!(challenge.source(), CodeSource::Authenticator);
    assert!(!format!("{challenge:?}").contains("key-1"));
    let SignIn::Complete(session) = client
        .bambu_submit_code(&account(), &challenge, "654321")
        .await
        .unwrap()
    else {
        panic!("expected a token");
    };
    assert_eq!(session.expires_at_unix, Some(1_950_000_000));
}

#[tokio::test]
async fn a_website_that_refuses_this_client_falls_back_to_an_emailed_code() {
    let server = MockServer::start().await;
    password_login(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"loginType": "tfa", "tfaKey": "key-1"})),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/bambu-web/api/csrf"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("cf-mitigated", "challenge")
                .set_body_string("<html>Just a moment...</html>"),
        )
        .mount(&server)
        .await;
    email_code_requested(&server, 1).await;
    let client = client(&server);
    let SignIn::CodeRequired(challenge) = client.bambu_sign_in(&account()).await.unwrap() else {
        panic!("expected a code request");
    };
    let SignIn::CodeRequired(fallback) = client
        .bambu_submit_code(&account(), &challenge, "654321")
        .await
        .unwrap()
    else {
        panic!("expected an emailed code instead");
    };
    assert_eq!(fallback.source(), CodeSource::Email);
}

#[tokio::test]
async fn refused_passwords_and_captchas_are_reported() {
    let refused = MockServer::start().await;
    password_login(
        &refused,
        ResponseTemplate::new(400).set_body_json(json!({"code": 3, "error": "Incorrect password"})),
    )
    .await;
    assert_eq!(
        client(&refused)
            .bambu_sign_in(&account())
            .await
            .unwrap_err(),
        LibraryError::SignInRejected {
            site: Site::MakerWorld,
            reason: "Incorrect password".into()
        }
    );
    let captcha = MockServer::start().await;
    password_login(
        &captcha,
        ResponseTemplate::new(418).set_body_json(json!({"captchaId": "c"})),
    )
    .await;
    assert_eq!(
        client(&captcha)
            .bambu_sign_in(&account())
            .await
            .unwrap_err(),
        LibraryError::Challenged {
            site: Site::MakerWorld
        }
    );
}
