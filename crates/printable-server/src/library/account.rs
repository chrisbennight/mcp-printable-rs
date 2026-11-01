//! MakerWorld sign-in for the operator's Bambu Lab account.
//!
//! The server signs in with the configured email address and password when a
//! download needs a token. With a configured authenticator secret it answers
//! authenticator codes itself. Otherwise the download reports which code Bambu
//! Lab wants, the agent asks the user for it, and `sign_in` completes the
//! sign-in. The session can be kept in a server-only state directory so a
//! restart does not require another code.

use std::{
    fs,
    io::{Read as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use model_library::{
    AuthenticatorSecret, BambuAccount, Challenge, Client, CodeSource, LibraryError, Password,
    Session, SignIn, Site, Token,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::{config::SettingsError, error::ToolError};

const ACCOUNT_SETTING: &str = "PRINTABLE_BAMBU_ACCOUNT";
const PASSWORD_SETTING: &str = "PRINTABLE_BAMBU_PASSWORD";
const AUTHENTICATOR_SETTING: &str = "PRINTABLE_BAMBU_AUTHENTICATOR_SECRET";
const TOKEN_SETTING: &str = "PRINTABLE_MAKERWORLD_TOKEN";
const STATE_DIR_SETTING: &str = "PRINTABLE_LIBRARY_STATE_DIR";
const SESSION_FILE: &str = "makerworld-session.json";
const MAXIMUM_SESSION_FILE_BYTES: u64 = 64 * 1024;
/// How long a requested code is awaited before a download may ask again,
/// so repeated downloads do not flood the account with emails.
const PENDING_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// With an authenticator secret, sessions are renewed this close to expiry.
const RENEWAL_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;
/// A token this close to expiry is treated as expired.
const EXPIRY_MARGIN_SECONDS: u64 = 60;
const RENEWAL_RETRY: Duration = Duration::from_secs(60 * 60);

const SIGN_IN_CALL: &str =
    r#"then call library sign_in with {"site": "makerworld", "code": "<code>"} and retry"#;

/// MakerWorld sign-in settings and the current session.
pub(crate) struct MakerWorldAccount {
    account: Option<BambuAccount>,
    authenticator: Option<AuthenticatorSecret>,
    store: Option<SessionStore>,
    /// SHA-256 of `PRINTABLE_MAKERWORLD_TOKEN`, stored with each session so a
    /// newly configured token takes precedence over an older stored session.
    seed_sha256: Option<String>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    session: Option<Session>,
    /// Changes whenever a new session is kept, so a request that failed with
    /// an older token cannot discard a newer one.
    generation: u64,
    pending: Option<Pending>,
    renewal_attempted: Option<Instant>,
    /// Early renewal needed a code from the user. It is not retried for
    /// this session, so Bambu Lab does not keep emailing codes nobody asked
    /// for; the user is asked once the session actually expires.
    renewal_needs_user: bool,
    /// Bambu Lab refused a code computed from the authenticator secret, so
    /// the secret or the server clock is wrong; the user is asked instead.
    authenticator_refused: bool,
    /// The last session could not be written to the state directory.
    store_failed: bool,
}

struct Pending {
    challenge: Challenge,
    since: Instant,
    /// Bambu Lab's website refused the authenticator step, so an emailed
    /// code was requested instead.
    fallback: bool,
}

enum Outcome {
    SignedIn,
    CodeRequired { source: CodeSource, fallback: bool },
}

impl std::fmt::Debug for MakerWorldAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MakerWorldAccount")
            .field("account", &self.account.is_some())
            .field("authenticator", &self.authenticator.is_some())
            .field("persistent", &self.store.is_some())
            .finish_non_exhaustive()
    }
}

impl MakerWorldAccount {
    /// Read the sign-in settings and any stored session. Makes no network
    /// request; checks that a configured state directory is writable.
    pub(crate) fn configure(get: &impl Fn(&str) -> Option<String>) -> Result<Self, SettingsError> {
        let value = |key: &str| get(key).filter(|value| !value.is_empty());
        let account = match (value(ACCOUNT_SETTING), value(PASSWORD_SETTING)) {
            (Some(email), Some(password)) => Some(
                Password::new(password)
                    .and_then(|password| BambuAccount::new(email, password))
                    .map_err(|_| SettingsError::Library(ACCOUNT_SETTING))?,
            ),
            (None, None) => None,
            (Some(_), None) => return Err(SettingsError::Library(PASSWORD_SETTING)),
            (None, Some(_)) => return Err(SettingsError::Library(ACCOUNT_SETTING)),
        };
        let authenticator = value(AUTHENTICATOR_SETTING)
            .map(|secret| {
                let secret = Zeroizing::new(secret);
                AuthenticatorSecret::new(&secret)
                    .map_err(|_| SettingsError::Library(AUTHENTICATOR_SETTING))
            })
            .transpose()?;
        if authenticator.is_some() && account.is_none() {
            return Err(SettingsError::Library(ACCOUNT_SETTING));
        }
        let store = value(STATE_DIR_SETTING)
            .map(|directory| {
                let directory = PathBuf::from(directory);
                let writable = directory.is_absolute()
                    && directory.is_dir()
                    && tempfile::Builder::new()
                        .prefix(".printable-write-check.")
                        .tempfile_in(&directory)
                        .is_ok();
                writable
                    .then(|| SessionStore {
                        file: directory.join(SESSION_FILE),
                    })
                    .ok_or(SettingsError::Library(STATE_DIR_SETTING))
            })
            .transpose()?;
        let token = value(TOKEN_SETTING)
            .map(|token| Token::new(token).map_err(|_| SettingsError::Library(TOKEN_SETTING)))
            .transpose()?;
        let seed_sha256 = token
            .as_ref()
            .map(|token| sha256_hex(token.expose_secret()));
        let stored = store
            .as_ref()
            .zip(account.as_ref())
            .and_then(|(store, account)| store.load(account.email()));
        let session = match (stored, token) {
            (Some(stored), Some(_))
                if stored.seed_sha256 == seed_sha256 && !expired(&stored.session) =>
            {
                Some(stored.session)
            }
            (_, Some(token)) => Some(Session::from_token(token)),
            (Some(stored), None) => Some(stored.session),
            (None, None) => None,
        };
        Ok(Self {
            account,
            authenticator,
            store,
            seed_sha256,
            state: Mutex::new(State {
                session,
                ..State::default()
            }),
        })
    }

    /// The token to start with, from a stored session or the token setting.
    pub(crate) fn initial_token(&self) -> Option<Token> {
        self.state
            .try_lock()
            .ok()
            .and_then(|state| state.session.as_ref().map(|session| session.token.clone()))
    }

    /// Whether downloads can obtain a token, now or by signing in.
    pub(crate) fn downloads_available(&self) -> bool {
        self.account.is_some()
            || self
                .state
                .try_lock()
                .map_or(true, |state| state.session.is_some())
    }

    pub(crate) fn can_sign_in(&self) -> bool {
        self.account.is_some()
    }

    /// Sign-in state for `status`. Reports presence and timing, never values.
    pub(crate) fn report(&self) -> Value {
        let mut report = json!({
            "account_configured": self.account.is_some(),
            "automatic_codes": self.authenticator.is_some(),
            "persistent": self.store.is_some(),
        });
        let Ok(state) = self.state.try_lock() else {
            report["state"] = json!("signing_in");
            return report;
        };
        if state.authenticator_refused {
            report["automatic_codes"] = json!(false);
            report["authenticator_refused"] = json!(true);
        }
        if state.store_failed {
            report["persistence_failed"] = json!(true);
        }
        let pending = live_pending(&state);
        match &state.session {
            Some(session) if !expired(session) => {
                report["state"] = json!("signed_in");
                if let Some(pending) = pending {
                    report["renewal_code_source"] = json!(pending.challenge.source());
                }
            }
            session => match pending {
                Some(pending) => {
                    report["state"] = json!("code_required");
                    report["code_source"] = json!(pending.challenge.source());
                }
                None if session.is_some() => report["state"] = json!("expired"),
                None => report["state"] = json!("signed_out"),
            },
        }
        if let Some(expires) = state
            .session
            .as_ref()
            .and_then(|session| session.expires_at_unix)
        {
            report["expires_unix_ms"] = json!(expires.saturating_mul(1000));
        }
        report
    }

    /// Make sure the client holds a usable MakerWorld token, signing in when
    /// none is held or it has expired. Returns the session's generation for
    /// [`Self::invalidate`].
    pub(crate) async fn ensure(&self, client: &Client) -> Result<u64, ToolError> {
        let mut state = self.state.lock().await;
        let mut expired_token = false;
        if let Some(session) = &state.session {
            if !expired(session) {
                let due = session
                    .expires_at_unix
                    .is_some_and(|expires| expires <= unix_seconds() + RENEWAL_WINDOW_SECONDS);
                let retry_due = state
                    .renewal_attempted
                    .is_none_or(|attempted| attempted.elapsed() >= RENEWAL_RETRY);
                if due
                    && retry_due
                    && !state.renewal_needs_user
                    && !state.authenticator_refused
                    && self.authenticator.is_some()
                {
                    state.renewal_attempted = Some(Instant::now());
                    // Renewal is best-effort while the current token still
                    // works. A code it requested stays available to sign_in.
                    match self.begin(client, &mut state).await {
                        Ok(Outcome::SignedIn) => {}
                        Ok(Outcome::CodeRequired { .. }) => state.renewal_needs_user = true,
                        Err(error) => tracing::warn!(%error, "MakerWorld session renewal failed"),
                    }
                }
                return Ok(state.generation);
            }
            expired_token = true;
            self.forget(client, &mut state);
        }
        if self.account.is_none() {
            let site = Site::MakerWorld;
            return Err(if expired_token {
                LibraryError::Authentication { site }
            } else {
                LibraryError::CredentialsRequired { site }
            }
            .into());
        }
        if let Some(pending) = live_pending(&state) {
            return Err(sign_in_required(
                pending.challenge.source(),
                pending.fallback,
            ));
        }
        match self.begin(client, &mut state).await? {
            Outcome::SignedIn => Ok(state.generation),
            Outcome::CodeRequired { source, fallback } => Err(sign_in_required(source, fallback)),
        }
    }

    /// Discard a session Bambu Lab rejected, unless a newer one replaced it.
    pub(crate) async fn invalidate(&self, client: &Client, generation: u64) {
        let mut state = self.state.lock().await;
        if state.generation == generation {
            self.forget(client, &mut state);
        }
    }

    /// The `sign_in` action: answer the waiting code, or start a sign-in when
    /// no code is waiting.
    pub(crate) async fn sign_in(
        &self,
        client: &Client,
        code: Option<&str>,
    ) -> Result<Value, ToolError> {
        if self.account.is_none() {
            return Err(ToolError::Validation(format!(
                "MakerWorld sign-in needs {ACCOUNT_SETTING} and {PASSWORD_SETTING} on the server"
            )));
        }
        let mut state = self.state.lock().await;
        let outcome = match (code, live_pending(&state)) {
            // Starting again would ask Bambu Lab for yet another email.
            (None, Some(pending)) => Outcome::CodeRequired {
                source: pending.challenge.source(),
                fallback: pending.fallback,
            },
            (None, None) => self.begin(client, &mut state).await?,
            (Some(code), _) => self.answer(client, &mut state, code).await?,
        };
        Ok(match outcome {
            Outcome::SignedIn => {
                let expires = state
                    .session
                    .as_ref()
                    .and_then(|session| session.expires_at_unix);
                json!({
                    "site": Site::MakerWorld,
                    "state": "signed_in",
                    "expires_unix_ms": expires.map(|expires| expires.saturating_mul(1000)),
                })
            }
            Outcome::CodeRequired { source, fallback } => json!({
                "site": Site::MakerWorld,
                "state": "code_required",
                "code_source": source,
                "instructions": instructions(source, fallback),
            }),
        })
    }

    async fn begin(&self, client: &Client, state: &mut State) -> Result<Outcome, ToolError> {
        let account = self.account.as_ref().expect("sign-in requires an account");
        state.pending = None;
        let challenge = match client.bambu_sign_in(account).await? {
            SignIn::Complete(session) => {
                self.keep(client, state, session);
                return Ok(Outcome::SignedIn);
            }
            SignIn::CodeRequired(challenge) => challenge,
        };
        let secret = self
            .authenticator
            .as_ref()
            .filter(|_| !state.authenticator_refused);
        match (secret, challenge.source()) {
            (Some(secret), CodeSource::Authenticator) => {
                let code = Zeroizing::new(secret.current_code());
                match client.bambu_submit_code(account, &challenge, &code).await {
                    Ok(next) => Ok(self.settle(client, state, next, true)),
                    Err(LibraryError::SignInRejected { .. }) => {
                        tracing::warn!(
                            "Bambu Lab refused the code computed from {AUTHENTICATOR_SETTING}; check the secret and the server clock"
                        );
                        state.authenticator_refused = true;
                        Ok(self.wait_for(state, challenge, false))
                    }
                    Err(error) => Err(error.into()),
                }
            }
            _ => Ok(self.wait_for(state, challenge, false)),
        }
    }

    async fn answer(
        &self,
        client: &Client,
        state: &mut State,
        code: &str,
    ) -> Result<Outcome, ToolError> {
        let account = self.account.as_ref().expect("sign-in requires an account");
        let Some(pending) = state.pending.as_ref() else {
            return Err(ToolError::Validation(
                "no MakerWorld sign-in is waiting for a code; call library sign_in without a code to start one".into(),
            ));
        };
        let was_authenticator = pending.challenge.source() == CodeSource::Authenticator;
        let challenge = pending.challenge.clone();
        match client.bambu_submit_code(account, &challenge, code).await {
            Ok(next) => Ok(self.settle(client, state, next, was_authenticator)),
            Err(error @ LibraryError::SignInCodeExpired { .. }) => {
                // Bambu Lab just emailed a new code: wait for that one rather
                // than letting the next request ask for yet another.
                if let Some(pending) = state.pending.as_mut() {
                    pending.since = Instant::now();
                }
                Err(error.into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Record a completed sign-in, or the emailed code requested when the
    /// authenticator step was refused.
    fn settle(
        &self,
        client: &Client,
        state: &mut State,
        next: SignIn,
        was_authenticator: bool,
    ) -> Outcome {
        match next {
            SignIn::Complete(session) => {
                self.keep(client, state, session);
                Outcome::SignedIn
            }
            SignIn::CodeRequired(challenge) => self.wait_for(state, challenge, was_authenticator),
        }
    }

    fn wait_for(&self, state: &mut State, challenge: Challenge, fallback: bool) -> Outcome {
        let source = challenge.source();
        let fallback = fallback && source == CodeSource::Email;
        state.pending = Some(Pending {
            challenge,
            since: Instant::now(),
            fallback,
        });
        Outcome::CodeRequired { source, fallback }
    }

    fn keep(&self, client: &Client, state: &mut State, session: Session) {
        client.set_token(Site::MakerWorld, Some(session.token.clone()));
        if let (Some(store), Some(account)) = (&self.store, &self.account) {
            let saved = store.save(account.email(), &session, self.seed_sha256.as_deref());
            if let Err(error) = &saved {
                tracing::warn!(%error, "MakerWorld session could not be stored");
            }
            state.store_failed = saved.is_err();
        }
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.renewal_needs_user = false;
        state.session = Some(session);
    }

    fn forget(&self, client: &Client, state: &mut State) {
        client.set_token(Site::MakerWorld, None);
        state.session = None;
        if let Some(store) = &self.store {
            store.remove();
        }
    }
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn live_pending(state: &State) -> Option<&Pending> {
    state
        .pending
        .as_ref()
        .filter(|pending| pending.since.elapsed() < PENDING_LIFETIME)
}

fn expired(session: &Session) -> bool {
    session
        .expires_at_unix
        .is_some_and(|expires| expires <= unix_seconds() + EXPIRY_MARGIN_SECONDS)
}

fn instructions(source: CodeSource, fallback: bool) -> String {
    match (source, fallback) {
        (CodeSource::Authenticator, _) => format!(
            "MakerWorld needs a new sign-in. Ask the user for the current 6-digit code from the authenticator app for their Bambu Lab account, {SIGN_IN_CALL}."
        ),
        (CodeSource::Email, false) => format!(
            "MakerWorld needs a new sign-in. Bambu Lab emailed a sign-in code to the account's email address. Ask the user for it, {SIGN_IN_CALL}."
        ),
        (CodeSource::Email, true) => format!(
            "Bambu Lab's website refused the authenticator step from this server, so Bambu Lab emailed a sign-in code to the account's email address instead. Ask the user for it, {SIGN_IN_CALL}."
        ),
    }
}

fn sign_in_required(source: CodeSource, fallback: bool) -> ToolError {
    ToolError::LibrarySignInRequired(instructions(source, fallback))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A session file in a directory only the server can read.
struct SessionStore {
    file: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    format_version: u32,
    account: String,
    token: String,
    expires_at_unix: Option<u64>,
    #[serde(default)]
    seed_sha256: Option<String>,
}

/// A session read back from the state directory.
struct Loaded {
    session: Session,
    seed_sha256: Option<String>,
}

impl SessionStore {
    fn load(&self, account: &str) -> Option<Loaded> {
        let mut bytes = Zeroizing::new(Vec::new());
        fs::File::open(&self.file)
            .ok()?
            .take(MAXIMUM_SESSION_FILE_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        let stored: StoredSession = serde_json::from_slice(&bytes).ok()?;
        if stored.format_version != 1 || stored.account != account {
            return None;
        }
        Some(Loaded {
            session: Session {
                token: Token::new(stored.token).ok()?,
                expires_at_unix: stored.expires_at_unix,
            },
            seed_sha256: stored.seed_sha256,
        })
    }

    /// Replace the session file atomically with owner-only permissions.
    fn save(
        &self,
        account: &str,
        session: &Session,
        seed_sha256: Option<&str>,
    ) -> std::io::Result<()> {
        let directory = self
            .file
            .parent()
            .ok_or_else(|| std::io::Error::other("session file has no directory"))?;
        let bytes = Zeroizing::new(serde_json::to_vec(&StoredSession {
            format_version: 1,
            account: account.to_owned(),
            token: session.token.expose_secret().to_owned(),
            expires_at_unix: session.expires_at_unix,
            seed_sha256: seed_sha256.map(str::to_owned),
        })?);
        let mut temporary = tempfile::Builder::new()
            .prefix(".makerworld-session.")
            .permissions(fs::Permissions::from_mode(0o600))
            .tempfile_in(directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.file).map_err(|error| error.error)?;
        fs::File::open(directory)?.sync_all()
    }

    fn remove(&self) {
        match fs::remove_file(&self.file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "MakerWorld session could not be removed"),
        }
    }
}
