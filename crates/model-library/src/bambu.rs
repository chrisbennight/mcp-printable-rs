//! Bambu Lab account sign-in, which MakerWorld downloads need.
//!
//! A password sign-in on Bambu Lab's API host either returns an access token or
//! asks for a second step: a code Bambu Lab emails to the account, or a code
//! from the account's authenticator app. Authenticator codes are answered on
//! Bambu Lab's website host. When that host refuses this client, Bambu Lab is
//! asked to email a code instead, which the API host accepts.

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;
use zeroize::Zeroizing;

use crate::{
    LibraryError, Site, Token,
    client::{Client, Credential, Exchange, classify},
    wire,
};

const SITE: Site = Site::MakerWorld;
/// Authenticator codes change every 30 seconds.
const AUTHENTICATOR_PERIOD_SECONDS: u64 = 30;
/// Shortest authenticator secret accepted, in bytes (80 bits).
const MINIMUM_SECRET_BYTES: usize = 10;
const MAXIMUM_EMAIL_CHARACTERS: usize = 254;

/// A Bambu Lab account password. Debug output is always redacted.
#[derive(Clone)]
pub struct Password(Zeroizing<String>);

impl Password {
    /// Protect a password.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty value or one with control characters.
    pub fn new(value: String) -> Result<Self, LibraryError> {
        let value = Zeroizing::new(value);
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(LibraryError::InvalidConfiguration("Bambu Lab password"));
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Password([REDACTED])")
    }
}

/// The email address and password of a Bambu Lab account.
#[derive(Clone, Debug)]
pub struct BambuAccount {
    email: String,
    password: Password,
}

impl BambuAccount {
    /// Combine an account's email address and password.
    ///
    /// # Errors
    ///
    /// Returns an error unless the account is an email address. Accounts
    /// registered by telephone number sign in through a different service.
    pub fn new(email: String, password: Password) -> Result<Self, LibraryError> {
        let email = email.trim().to_owned();
        let valid = email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        }) && email.chars().count() <= MAXIMUM_EMAIL_CHARACTERS
            && !email
                .chars()
                .any(|character| character.is_whitespace() || character.is_control());
        if !valid {
            return Err(LibraryError::InvalidConfiguration(
                "Bambu Lab account email address",
            ));
        }
        Ok(Self { email, password })
    }

    pub fn email(&self) -> &str {
        &self.email
    }
}

/// The shared secret behind an authenticator app's codes: the base32 text a
/// site shows beside the QR code when two-factor sign-in is set up. Debug
/// output is always redacted.
#[derive(Clone)]
pub struct AuthenticatorSecret(Zeroizing<Vec<u8>>);

impl AuthenticatorSecret {
    /// Decode a base32 secret, ignoring spaces, letter case and padding.
    ///
    /// # Errors
    ///
    /// Returns an error for text that is not base32 or is shorter than 80 bits.
    pub fn new(value: &str) -> Result<Self, LibraryError> {
        let normalized = Zeroizing::new(
            value
                .chars()
                .filter(|character| !character.is_whitespace() && *character != '=')
                .map(|character| character.to_ascii_uppercase())
                .collect::<String>(),
        );
        let invalid = LibraryError::InvalidConfiguration("authenticator secret");
        let decoded = Zeroizing::new(
            BASE32_NOPAD
                .decode(normalized.as_bytes())
                .map_err(|_| invalid.clone())?,
        );
        if decoded.len() < MINIMUM_SECRET_BYTES {
            return Err(invalid);
        }
        Ok(Self(decoded))
    }

    /// The six-digit time-based code (RFC 6238 with HMAC-SHA-1 and 30-second
    /// steps) for a moment in Unix seconds, as an authenticator app shows it.
    pub fn code_at(&self, unix_seconds: u64) -> String {
        let counter = (unix_seconds / AUTHENTICATOR_PERIOD_SECONDS).to_be_bytes();
        let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(&self.0)
            .expect("HMAC accepts keys of any length");
        mac.update(&counter);
        let digest = mac.finalize().into_bytes();
        let offset = usize::from(digest[digest.len() - 1] & 0x0f);
        let value = u32::from_be_bytes([
            digest[offset] & 0x7f,
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]);
        format!("{:06}", value % 1_000_000)
    }

    /// The code for the current moment.
    pub fn current_code(&self) -> String {
        self.code_at(unix_seconds())
    }
}

impl fmt::Debug for AuthenticatorSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatorSecret([REDACTED])")
    }
}

/// Where the code for a sign-in's second step comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeSource {
    /// Bambu Lab emailed a code to the account's address.
    Email,
    /// The account's authenticator app shows the code.
    Authenticator,
}

/// A sign-in waiting for a code. Debug output is always redacted.
#[derive(Clone)]
pub struct Challenge {
    source: CodeSource,
    tfa_key: Option<Zeroizing<String>>,
}

impl Challenge {
    pub fn source(&self) -> CodeSource {
        self.source
    }

    fn email() -> Self {
        Self {
            source: CodeSource::Email,
            tfa_key: None,
        }
    }
}

impl fmt::Debug for Challenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Challenge")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// A signed-in MakerWorld session.
#[derive(Clone, Debug)]
pub struct Session {
    pub token: Token,
    /// When the token expires, in Unix seconds, when Bambu Lab says.
    pub expires_at_unix: Option<u64>,
}

impl Session {
    /// A session for a token obtained elsewhere, with the expiry it carries.
    pub fn from_token(token: Token) -> Self {
        let expires_at_unix = token_expiry(token.expose());
        Self {
            token,
            expires_at_unix,
        }
    }
}

/// The result of one sign-in step.
#[derive(Debug)]
pub enum SignIn {
    Complete(Session),
    CodeRequired(Challenge),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Login {
    #[serde(default)]
    access_token: String,
    #[serde(default, deserialize_with = "wire::count")]
    expires_in: Option<u64>,
    #[serde(default)]
    login_type: String,
    #[serde(default)]
    tfa_key: String,
}

#[derive(Deserialize)]
struct LoginRefusal {
    code: Option<i64>,
}

impl Client {
    fn bambu_api(&self, segments: &[&str]) -> Url {
        let mut url = self.endpoints.api(SITE).clone();
        url.path_segments_mut()
            .expect("fixed API origin has a path")
            .pop_if_empty()
            .extend(segments);
        url
    }

    fn bambu_web(&self, path: &str) -> Url {
        self.endpoints
            .bambu_web
            .join(path)
            .expect("fixed website path")
    }

    /// Start a password sign-in. When Bambu Lab asks for an emailed code, it
    /// is asked to send one.
    ///
    /// # Errors
    ///
    /// Returns an error when Bambu Lab refuses the account, challenges this
    /// client, or answers in a way this client does not understand.
    pub async fn bambu_sign_in(&self, account: &BambuAccount) -> Result<SignIn, LibraryError> {
        let body = json!({
            "account": account.email,
            "password": account.password.0.as_str(),
            "apiError": "",
        });
        let url = self.bambu_api(&["v1", "user-service", "user", "login"]);
        let exchange = self.exchange(SITE, self.api_post(url, &body)).await?;
        if !exchange.status.is_success() {
            return Err(sign_in_refusal(
                &exchange,
                "Bambu Lab refused the account email address or password",
            ));
        }
        let login: Login = serde_json::from_slice(&exchange.body)
            .map_err(|_| LibraryError::IncompatibleResponse { site: SITE })?;
        if let Some(session) = session(&login) {
            return Ok(SignIn::Complete(session));
        }
        match login.login_type.as_str() {
            "verifyCode" => {
                self.request_email_code(account).await?;
                Ok(SignIn::CodeRequired(Challenge::email()))
            }
            "tfa" if !login.tfa_key.is_empty() => Ok(SignIn::CodeRequired(Challenge {
                source: CodeSource::Authenticator,
                tfa_key: Some(Zeroizing::new(login.tfa_key)),
            })),
            _ => Err(LibraryError::IncompatibleResponse { site: SITE }),
        }
    }

    /// Answer a sign-in's second step with a code. If Bambu Lab's website
    /// refuses this client for an authenticator code, Bambu Lab is asked to
    /// email a code instead, and an email challenge is returned.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::SignInRejected`] for a wrong or expired code,
    /// or another error when the exchange fails.
    pub async fn bambu_submit_code(
        &self,
        account: &BambuAccount,
        challenge: &Challenge,
        code: &str,
    ) -> Result<SignIn, LibraryError> {
        let code: String = code
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        if !(4..=10).contains(&code.len()) || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(LibraryError::InvalidRequest(
                "the code must be the digits Bambu Lab emailed or the authenticator app shows"
                    .into(),
            ));
        }
        match (&challenge.source, &challenge.tfa_key) {
            (CodeSource::Authenticator, Some(tfa_key)) => {
                self.submit_authenticator_code(account, tfa_key, &code)
                    .await
            }
            _ => self.submit_email_code(account, &code).await,
        }
    }

    async fn request_email_code(&self, account: &BambuAccount) -> Result<(), LibraryError> {
        let url = self.bambu_api(&["v1", "user-service", "user", "sendemail", "code"]);
        let body = json!({"email": account.email, "type": "codeLogin"});
        let exchange = self.exchange(SITE, self.api_post(url, &body)).await?;
        if exchange.status.is_success() {
            return Ok(());
        }
        Err(classify(
            SITE,
            exchange.status,
            Credential::NotApplicable,
            exchange.challenged(),
            &exchange.body,
        ))
    }

    async fn submit_email_code(
        &self,
        account: &BambuAccount,
        code: &str,
    ) -> Result<SignIn, LibraryError> {
        let url = self.bambu_api(&["v1", "user-service", "user", "login"]);
        let body = json!({"account": account.email, "code": code});
        let exchange = self.exchange(SITE, self.api_post(url, &body)).await?;
        if exchange.status == StatusCode::BAD_REQUEST && !exchange.challenged() {
            let refusal: Option<LoginRefusal> = serde_json::from_slice(&exchange.body).ok();
            return Err(match refusal.and_then(|refusal| refusal.code) {
                Some(1) => {
                    self.request_email_code(account).await?;
                    LibraryError::SignInCodeExpired { site: SITE }
                }
                Some(2) => rejected("Bambu Lab did not accept the code"),
                _ => sign_in_refusal(&exchange, "Bambu Lab did not accept the code"),
            });
        }
        if !exchange.status.is_success() {
            return Err(sign_in_refusal(
                &exchange,
                "Bambu Lab did not accept the code",
            ));
        }
        let login: Login = serde_json::from_slice(&exchange.body)
            .map_err(|_| LibraryError::IncompatibleResponse { site: SITE })?;
        session(&login)
            .map(SignIn::Complete)
            .ok_or(LibraryError::IncompatibleResponse { site: SITE })
    }

    async fn submit_authenticator_code(
        &self,
        account: &BambuAccount,
        tfa_key: &str,
        code: &str,
    ) -> Result<SignIn, LibraryError> {
        let csrf = self
            .exchange(SITE, self.api_get(self.bambu_web("api/csrf")))
            .await?;
        let Some(csrf_token) = csrf
            .cookie("bbl_csrf_token")
            .filter(|_| csrf.status.is_success())
        else {
            return self.fall_back_to_email(account, &csrf).await;
        };
        let request = self
            .api_post(
                self.bambu_web("api/sign-in/tfa"),
                &json!({"tfaKey": tfa_key, "tfaCode": code}),
            )
            .header("x-bbl-csrf-token", csrf_token.as_str())
            .header(
                reqwest::header::COOKIE,
                format!("bbl_csrf_token={csrf_token}"),
            );
        let exchange = self.exchange(SITE, request).await?;
        if exchange.challenged()
            || String::from_utf8_lossy(&exchange.body).contains("missing_cookie")
        {
            return self.fall_back_to_email(account, &exchange).await;
        }
        if !exchange.status.is_success() {
            return Err(sign_in_refusal(
                &exchange,
                "Bambu Lab did not accept the authenticator code",
            ));
        }
        if let Some(session) = serde_json::from_slice::<Login>(&exchange.body)
            .ok()
            .and_then(|login| session(&login))
        {
            return Ok(SignIn::Complete(session));
        }
        let token = exchange
            .cookie("token")
            .and_then(|token| Token::new(token).ok())
            .ok_or(LibraryError::IncompatibleResponse { site: SITE })?;
        let mut session = Session::from_token(token);
        if session.expires_at_unix.is_none() {
            session.expires_at_unix = exchange
                .cookie_max_age("token")
                .map(|seconds| unix_seconds().saturating_add(seconds));
        }
        Ok(SignIn::Complete(session))
    }

    /// Bambu Lab's website answered with a challenge or refused this
    /// client's request shape; ask for an emailed code, which the API host
    /// accepts, rather than imitating a browser.
    async fn fall_back_to_email(
        &self,
        account: &BambuAccount,
        refused: &Exchange,
    ) -> Result<SignIn, LibraryError> {
        if refused.status.is_server_error() {
            return Err(LibraryError::Unavailable { site: SITE });
        }
        self.request_email_code(account).await?;
        Ok(SignIn::CodeRequired(Challenge::email()))
    }
}

fn rejected(reason: &str) -> LibraryError {
    LibraryError::SignInRejected {
        site: SITE,
        reason: reason.into(),
    }
}

/// Classify a refused sign-in step, keeping the site's own reason.
fn sign_in_refusal(exchange: &Exchange, default: &str) -> LibraryError {
    if exchange.challenged() {
        return LibraryError::Challenged { site: SITE };
    }
    match exchange.status.as_u16() {
        400 | 401 | 403 => LibraryError::SignInRejected {
            site: SITE,
            reason: exchange.reason().unwrap_or_else(|| default.into()),
        },
        _ => classify(
            SITE,
            exchange.status,
            Credential::NotApplicable,
            false,
            &exchange.body,
        ),
    }
}

fn session(login: &Login) -> Option<Session> {
    let token = Token::new(login.access_token.clone()).ok()?;
    let expires_at_unix = login
        .expires_in
        .filter(|seconds| *seconds > 0)
        .map(|seconds| unix_seconds().saturating_add(seconds))
        .or_else(|| token_expiry(token.expose()));
    Some(Session {
        token,
        expires_at_unix,
    })
}

/// The `exp` claim of a JSON Web Token, when the token is one.
fn token_expiry(token: &str) -> Option<u64> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let decoded = BASE64URL_NOPAD
        .decode(payload.trim_end_matches('=').as_bytes())
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()?
        .get("exp")?
        .as_u64()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticator_codes_match_the_rfc_6238_test_vectors() {
        // RFC 6238 appendix B, SHA-1, key "12345678901234567890", last six digits.
        let secret =
            AuthenticatorSecret::new(&BASE32_NOPAD.encode(b"12345678901234567890")).unwrap();
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(secret.code_at(time), code, "{time}");
        }
    }

    #[test]
    fn secrets_accept_app_formatting_and_refuse_short_or_invalid_text() {
        let spaced = "gezd gnbv gy3t qojq gezd gnbv gy3t qojq====";
        let compact = BASE32_NOPAD.encode(b"12345678901234567890");
        assert_eq!(
            AuthenticatorSecret::new(spaced).unwrap().code_at(59),
            AuthenticatorSecret::new(&compact).unwrap().code_at(59)
        );
        assert!(AuthenticatorSecret::new("GEZDGNBV").is_err());
        assert!(AuthenticatorSecret::new("not base32!").is_err());
        let secret = AuthenticatorSecret::new(spaced).unwrap();
        assert_eq!(format!("{secret:?}"), "AuthenticatorSecret([REDACTED])");
    }

    #[test]
    fn accounts_need_an_email_address_and_never_show_the_password() {
        let password = || Password::new("SENTINEL pass".into()).unwrap();
        let account = BambuAccount::new(" maker@example.com ".into(), password()).unwrap();
        assert_eq!(account.email(), "maker@example.com");
        assert!(!format!("{account:?}").contains("SENTINEL"));
        for email in ["+15551234567", "maker@", "@example.com", "a b@example.com"] {
            assert!(
                BambuAccount::new(email.into(), password()).is_err(),
                "{email}"
            );
        }
        assert!(Password::new(String::new()).is_err());
        assert!(Password::new("line\nbreak".into()).is_err());
    }

    #[test]
    fn token_expiry_reads_the_json_web_token_claim() {
        let payload = BASE64URL_NOPAD.encode(br#"{"exp":1893456000,"username":"u_1"}"#);
        assert_eq!(
            token_expiry(&format!("e30.{payload}.c2ln")),
            Some(1_893_456_000)
        );
        assert_eq!(token_expiry("AAB_opaque"), None);
        assert_eq!(token_expiry("a.b.c.d"), None);
    }
}
