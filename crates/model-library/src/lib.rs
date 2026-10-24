//! Bounded, typed access to public 3D model repositories.
//!
//! Each site is reached only at its fixed API origin. A download follows at
//! most [`MAXIMUM_REDIRECTS`] redirects, and every hop must stay on the site's
//! allowed file hosts over HTTPS. Access tokens are sent only to the site's API
//! origin, never to file hosts. Raw upstream bodies are discarded from errors.

mod bambu;
mod client;
mod makerworld;
mod printables;
mod reference;
mod text;
mod thingiverse;
mod types;
mod wire;

use std::fmt;

use thiserror::Error;
use zeroize::Zeroizing;

pub use bambu::{
    AuthenticatorSecret, BambuAccount, Challenge, CodeSource, Password, Session, SignIn,
};
pub use client::{Client, Credentials, FileStream, MAXIMUM_REDIRECTS};
pub use reference::model_id;
pub use types::{
    FileKind, License, MAXIMUM_SEARCH_LIMIT, ModelDetail, ModelSummary, RemoteFile, SearchPage,
    SearchQuery, SearchSort, Site,
};

/// One site access token. Debug output is always redacted.
#[derive(Clone)]
pub struct Token(Zeroizing<String>);

impl Token {
    /// Validate and protect a token.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty value or one containing whitespace or
    /// control characters, which cannot be sent in an HTTP header.
    pub fn new(value: String) -> Result<Self, LibraryError> {
        let value = Zeroizing::new(value);
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(LibraryError::InvalidConfiguration("access token"));
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }

    /// The token text, for storing a session where only the server can read
    /// it. Never log or return it.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token([REDACTED])")
    }
}

/// Errors expose only typed details and bounded site refusal reasons.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LibraryError {
    #[error("invalid model library configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{site} requires an access token for this operation; none is configured")]
    CredentialsRequired { site: Site },
    #[error("{site} rejected the configured access token; it may have expired")]
    Authentication { site: Site },
    #[error("{site} refused access: {reason}")]
    Forbidden { site: Site, reason: String },
    #[error("{site} has no such model or file")]
    NotFound { site: Site },
    #[error(
        "{site} is asking this server to solve a CAPTCHA; wait, then try again, or download the file in a browser and upload it"
    )]
    Challenged { site: Site },
    #[error("{site} is limiting request rates; wait before trying again")]
    RateLimited { site: Site },
    #[error("{site} is temporarily unavailable")]
    Unavailable { site: Site },
    #[error("{site} returned a response this client does not understand")]
    IncompatibleResponse { site: Site },
    #[error("{site} returned a response larger than this client accepts")]
    ResponseTooLarge { site: Site },
    #[error("the file is larger than the configured limit of {limit} bytes")]
    FileTooLarge { limit: u64 },
    #[error("{site} sent the download to a host outside its allowed file hosts")]
    DisallowedHost { site: Site },
    #[error("the download from {site} was interrupted")]
    Interrupted { site: Site },
    #[error("{site} sign-in failed: {reason}")]
    SignInRejected { site: Site, reason: String },
    #[error("the {site} sign-in code expired; Bambu Lab has emailed a new one")]
    SignInCodeExpired { site: Site },
}
