//! Server settings from the environment.
//!
//! Environment names are the deployed Rust configuration API. Parsing is split
//! from `std::env` ([`Settings::from_lookup`]) so the settings table can be
//! tested without racing the process-global environment.

use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use subtle::ConstantTimeEq;
use thiserror::Error;

const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
const DEFAULT_HTTP_PORT: u16 = 8000;
const DEFAULT_BLENDER_HOST: &str = "127.0.0.1";
const DEFAULT_BLENDER_PORT: u16 = 9876;
const DEFAULT_SCAD_CONCURRENCY: usize = 2;
const DEFAULT_RENDER_JOB_QUEUE_DEPTH: usize = 16;
const MAX_RENDER_JOB_QUEUE_DEPTH: usize = 1000;
const DEFAULT_GEOMETRY_WORKER_MEMORY_MIB: u64 = 1024;
const DEFAULT_ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];
const MCP_BEARER_BYTES: usize = 64;

/// The shared MCP client bearer. Debug output is always redacted.
#[derive(Clone)]
pub struct BearerSecret(String);

impl BearerSecret {
    pub fn parse(value: String) -> Result<Self, SettingsError> {
        let valid = value.len() == MCP_BEARER_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(value))
        } else {
            Err(SettingsError::McpBearerInvalid)
        }
    }

    pub(crate) fn matches(&self, candidate: &str) -> bool {
        candidate.len() == MCP_BEARER_BYTES
            && bool::from(self.0.as_bytes().ct_eq(candidate.as_bytes()))
    }
}

impl fmt::Debug for BearerSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerSecret([REDACTED])")
    }
}

/// Validated server settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub http_host: String,
    pub http_port: u16,
    pub blender_host: String,
    pub blender_port: u16,
    /// Separate background Blender endpoint; absent retains the migration runtime.
    pub render_worker_host: Option<String>,
    pub render_worker_port: u16,
    pub workspace_root: Option<PathBuf>,
    pub blender_workspace_root: Option<PathBuf>,
    /// OpenSCAD binary override for compile, render, and cross-section tools.
    pub openscad_bin: Option<PathBuf>,
    /// Dedicated credential-free CAD worker on the private backend network.
    pub cad_endpoint: Option<String>,
    /// Optional typed printer integration; credentials remain inside its clients.
    pub printers: Option<std::sync::Arc<crate::printers::PrinterService>>,
    /// OpenSCAD subprocess concurrency limit.
    pub scad_concurrency: usize,
    /// FFmpeg binary used to encode durable animation frame sequences.
    pub ffmpeg_bin: PathBuf,
    /// Maximum number of non-terminal render jobs admitted process-wide.
    pub render_job_queue_depth: usize,
    /// Exact assembly CSG worker override. By default the server uses a sibling
    /// binary from the same installation directory.
    pub geometry_worker_bin: Option<PathBuf>,
    /// Address-space budget for each isolated exact assembly CSG worker.
    pub geometry_worker_memory_bytes: u64,
    pub file_upload_max_bytes: u64,
    /// Shared bearer required on every MCP transport request.
    pub mcp_bearer: BearerSecret,
    /// Hostnames accepted in the `Host` header for `/mcp` (the transport's
    /// DNS-rebinding guard). The default covers loopback only; a container
    /// deployment must add its service DNS name.
    pub allowed_hosts: Vec<String>,
    /// Exact serialized browser origins; empty rejects every Origin header.
    pub allowed_origins: Vec<String>,
    /// Trusted external base, including any reverse-proxy prefix and trailing slash.
    pub download_base_url: Option<reqwest::Url>,
}

/// Stable operator-facing settings errors; the offending variable name is the
/// payload.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("printer integration setting {0} is missing or invalid")]
    Printers(&'static str),
    #[error("PRINTABLE_FILE_UPLOAD_MAX_MIB must be a positive MiB count within u64 bytes")]
    FileUploadLimitInvalid,
    #[error("PRINTABLE_MCP_BEARER is required")]
    McpBearerMissing,
    #[error("PRINTABLE_MCP_BEARER must contain exactly 64 lowercase hexadecimal characters")]
    McpBearerInvalid,
    #[error(
        "PRINTABLE_ALLOWED_ORIGINS must list exact HTTP(S) origins without paths, credentials, or wildcards"
    )]
    AllowedOriginsInvalid,
    #[error("configure only one of PRINTABLE_MCP_BEARER and PRINTABLE_MCP_BEARER_FILE")]
    McpBearerConflict,
    #[error("PRINTABLE_MCP_BEARER_FILE could not be read")]
    McpBearerFileUnreadable,
    #[error(
        "PRINTABLE_DOWNLOAD_BASE_URL must be HTTPS (or loopback HTTP), end with /, and contain no credentials, query, or fragment"
    )]
    DownloadBaseInvalid,
    #[error("{0} must be an integer")]
    PortNotInteger(&'static str),
    #[error("{0} must be between 1 and 65535")]
    PortOutOfRange(&'static str),
    #[error("PRINTABLE_BLENDER_WORKSPACE_ROOT requires PRINTABLE_WORKSPACE_ROOT")]
    BlenderWorkspaceRequiresWorkspace,
    #[error("{0} must be a positive integer")]
    ScadConcurrencyNotPositive(&'static str),
    #[error("{0} must be an integer between 1 and 1000")]
    RenderJobQueueDepthOutOfRange(&'static str),
    #[error("{0} must be a positive integer")]
    GeometryWorkerMemoryNotPositive(&'static str),
    #[error("{0} exceeds the platform address-space range")]
    GeometryWorkerMemoryOutOfRange(&'static str),
}

impl Settings {
    /// Read and validate settings from the process environment.
    pub fn from_env() -> Result<Self, SettingsError> {
        Self::from_sources(|key| std::env::var(key).ok(), read_bearer_file)
    }

    fn from_sources(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&str) -> Result<String, SettingsError>,
    ) -> Result<Self, SettingsError> {
        let inline = get("PRINTABLE_MCP_BEARER");
        let file = nonempty(&get, "PRINTABLE_MCP_BEARER_FILE");
        let bearer = match (inline, file) {
            (Some(_), Some(_)) => return Err(SettingsError::McpBearerConflict),
            (None, Some(path)) => Some(read(&path)?.trim_end_matches(['\r', '\n']).to_owned()),
            (value, None) => value,
        };
        Self::from_lookup(|key| {
            if key == "PRINTABLE_MCP_BEARER" {
                bearer.clone()
            } else {
                get(key)
            }
        })
    }

    /// Pure parse/validate core over an arbitrary lookup, so the settings table
    /// is testable without the process-global environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, SettingsError> {
        let workspace_root = nonempty(&get, "PRINTABLE_WORKSPACE_ROOT");
        let blender_workspace_root = nonempty(&get, "PRINTABLE_BLENDER_WORKSPACE_ROOT");
        // The Blender-side root only makes sense relative to the app-side root.
        if blender_workspace_root.is_some() && workspace_root.is_none() {
            return Err(SettingsError::BlenderWorkspaceRequiresWorkspace);
        }
        Ok(Self {
            http_host: present_or(&get, "PRINTABLE_HTTP_HOST", DEFAULT_HTTP_HOST),
            http_port: port(&get, "PRINTABLE_HTTP_PORT", DEFAULT_HTTP_PORT)?,
            blender_host: present_or(&get, "BLENDER_HOST", DEFAULT_BLENDER_HOST),
            blender_port: port(&get, "BLENDER_PORT", DEFAULT_BLENDER_PORT)?,
            render_worker_host: nonempty(&get, "PRINTABLE_RENDER_WORKER_HOST"),
            render_worker_port: port(&get, "PRINTABLE_RENDER_WORKER_PORT", DEFAULT_BLENDER_PORT)?,
            workspace_root: workspace_root.map(PathBuf::from),
            blender_workspace_root: blender_workspace_root.map(PathBuf::from),
            openscad_bin: nonempty(&get, "OPENSCAD_BIN").map(PathBuf::from),
            cad_endpoint: nonempty(&get, "PRINTABLE_CAD_ENDPOINT"),
            printers: crate::printers::configure(&get)?,
            scad_concurrency: scad_concurrency(&get)?,
            ffmpeg_bin: nonempty(&get, "FFMPEG_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("ffmpeg")),
            render_job_queue_depth: render_job_queue_depth(&get)?,
            geometry_worker_bin: nonempty(&get, "PRINTABLE_GEOMETRY_WORKER_BIN").map(PathBuf::from),
            geometry_worker_memory_bytes: geometry_worker_memory_bytes(&get)?,
            file_upload_max_bytes: file_upload_max_bytes(&get)?,
            mcp_bearer: mcp_bearer(&get)?,
            allowed_hosts: allowed_hosts(nonempty(&get, "PRINTABLE_ALLOWED_HOSTS")),
            allowed_origins: allowed_origins(nonempty(&get, "PRINTABLE_ALLOWED_ORIGINS"))?,
            download_base_url: download_base_url(nonempty(&get, "PRINTABLE_DOWNLOAD_BASE_URL"))?,
        })
    }
}

fn read_bearer_file(path: &str) -> Result<String, SettingsError> {
    let mut value = String::new();
    std::fs::File::open(path)
        .map_err(|_| SettingsError::McpBearerFileUnreadable)?
        .take((MCP_BEARER_BYTES + 3) as u64)
        .read_to_string(&mut value)
        .map_err(|_| SettingsError::McpBearerFileUnreadable)?;
    if value.len() > MCP_BEARER_BYTES + 2 {
        return Err(SettingsError::McpBearerInvalid);
    }
    Ok(value)
}

fn download_base_url(raw: Option<String>) -> Result<Option<reqwest::Url>, SettingsError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(&raw).map_err(|_| SettingsError::DownloadBaseInvalid)?;
    let host = url.host_str().ok_or(SettingsError::DownloadBaseInvalid)?;
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || host.contains('*')
        || url.port() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(SettingsError::DownloadBaseInvalid);
    }
    Ok(Some(url))
}

fn mcp_bearer(get: &impl Fn(&str) -> Option<String>) -> Result<BearerSecret, SettingsError> {
    let value = nonempty(get, "PRINTABLE_MCP_BEARER").ok_or(SettingsError::McpBearerMissing)?;
    BearerSecret::parse(value)
}

fn allowed_origins(raw: Option<String>) -> Result<Vec<String>, SettingsError> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            let url =
                reqwest::Url::parse(value).map_err(|_| SettingsError::AllowedOriginsInvalid)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none_or(|host| host.contains('*'))
                || url.origin().ascii_serialization() != value
            {
                return Err(SettingsError::AllowedOriginsInvalid);
            }
            Ok(value.to_owned())
        })
        .collect()
}

/// Host vars: absent uses the default; present values, including empty, are
/// retained so invalid bind values fail at the actual listener boundary.
fn present_or(get: &impl Fn(&str) -> Option<String>, key: &str, default: &str) -> String {
    get(key).unwrap_or_else(|| default.to_string())
}

/// Optional path/list vars: absent or empty means disabled.
fn nonempty(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key).filter(|v| !v.is_empty())
}

/// Port vars: absent uses the default. A well-formed integer literal outside
/// `1..=65535`, including one too large for `i64`, is out-of-range rather than
/// syntactically invalid.
fn port(
    get: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: u16,
) -> Result<u16, SettingsError> {
    match get(key) {
        None => Ok(default),
        Some(raw) => {
            let trimmed = raw.trim();
            match trimmed.parse::<i64>() {
                Ok(n) if (1..=65535).contains(&n) => Ok(n as u16),
                Ok(_) => Err(SettingsError::PortOutOfRange(key)),
                // Numeric literals that overflow i64 are still syntactically
                // integers and therefore fail the range contract.
                Err(_) if is_integer_literal(trimmed) => Err(SettingsError::PortOutOfRange(key)),
                Err(_) => Err(SettingsError::PortNotInteger(key)),
            }
        }
    }
}

/// Whether `s` is a plain decimal integer literal with an optional sign and at
/// least one digit.
fn is_integer_literal(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// SCAD concurrency: absent uses the default; a configured value must be a
/// positive integer.
fn scad_concurrency(get: &impl Fn(&str) -> Option<String>) -> Result<usize, SettingsError> {
    const KEY: &str = "PRINTABLE_SCAD_CONCURRENCY";
    match nonempty(get, KEY) {
        None => Ok(DEFAULT_SCAD_CONCURRENCY),
        Some(raw) => {
            let n: i64 = raw
                .trim()
                .parse()
                .map_err(|_| SettingsError::ScadConcurrencyNotPositive(KEY))?;
            if n >= 1 {
                Ok(n as usize)
            } else {
                Err(SettingsError::ScadConcurrencyNotPositive(KEY))
            }
        }
    }
}

fn render_job_queue_depth(get: &impl Fn(&str) -> Option<String>) -> Result<usize, SettingsError> {
    const KEY: &str = "PRINTABLE_RENDER_JOB_QUEUE_DEPTH";
    match nonempty(get, KEY) {
        None => Ok(DEFAULT_RENDER_JOB_QUEUE_DEPTH),
        Some(raw) => {
            let value = raw
                .trim()
                .parse::<usize>()
                .map_err(|_| SettingsError::RenderJobQueueDepthOutOfRange(KEY))?;
            if !(1..=MAX_RENDER_JOB_QUEUE_DEPTH).contains(&value) {
                Err(SettingsError::RenderJobQueueDepthOutOfRange(KEY))
            } else {
                Ok(value)
            }
        }
    }
}

fn file_upload_max_bytes(get: &impl Fn(&str) -> Option<String>) -> Result<u64, SettingsError> {
    const KEY: &str = "PRINTABLE_FILE_UPLOAD_MAX_MIB";
    let value = nonempty(get, KEY)
        .unwrap_or_else(|| "1024".into())
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .and_then(|value| value.checked_mul(1024 * 1024));
    value.ok_or(SettingsError::FileUploadLimitInvalid)
}

fn geometry_worker_memory_bytes(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<u64, SettingsError> {
    const KEY: &str = "PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB";
    match nonempty(get, KEY) {
        None => Ok(DEFAULT_GEOMETRY_WORKER_MEMORY_MIB * 1024 * 1024),
        Some(raw) => {
            let value = raw
                .trim()
                .parse::<u64>()
                .map_err(|_| SettingsError::GeometryWorkerMemoryNotPositive(KEY))?;
            if value == 0 {
                Err(SettingsError::GeometryWorkerMemoryNotPositive(KEY))
            } else {
                value
                    .checked_mul(1024 * 1024)
                    .ok_or(SettingsError::GeometryWorkerMemoryOutOfRange(KEY))
            }
        }
    }
}

/// Parse the CSV `Host`-allowlist, falling back to the loopback default set.
fn allowed_hosts(raw: Option<String>) -> Vec<String> {
    let parsed: Option<Vec<String>> = raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect()
    });
    match parsed {
        Some(v) if !v.is_empty() => v,
        _ => DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `Settings::from_lookup` env source backed by a fixed map — no process
    /// environment, so the table runs parallel-safe.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut map = HashMap::from([(
            "PRINTABLE_MCP_BEARER".to_string(),
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            )
            .to_string(),
        )]);
        map.extend(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn all_absent_yields_defaults() {
        let s = Settings::from_lookup(lookup(&[])).unwrap();
        assert_eq!(s.http_host, "127.0.0.1");
        assert_eq!(s.http_port, 8000);
        assert_eq!(s.blender_host, "127.0.0.1");
        assert_eq!(s.blender_port, 9876);
        assert_eq!(s.workspace_root, None);
        assert_eq!(s.blender_workspace_root, None);
        assert_eq!(s.openscad_bin, None);
        assert_eq!(s.scad_concurrency, 2);
        assert_eq!(s.ffmpeg_bin, PathBuf::from("ffmpeg"));
        assert_eq!(s.render_job_queue_depth, 16);
        assert_eq!(s.geometry_worker_bin, None);
        assert_eq!(s.geometry_worker_memory_bytes, 1024 * 1024 * 1024);
        assert_eq!(s.allowed_hosts, vec!["localhost", "127.0.0.1", "::1"]);
        assert!(s.allowed_origins.is_empty());
        assert!(format!("{s:?}").contains("BearerSecret([REDACTED])"));
        assert!(!format!("{s:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn mcp_bearer_is_required_and_strictly_lowercase_hex() {
        assert_eq!(
            Settings::from_lookup(|_| None).unwrap_err(),
            SettingsError::McpBearerMissing
        );
        assert_eq!(
            Settings::from_lookup(lookup(&[("PRINTABLE_MCP_BEARER", "")])).unwrap_err(),
            SettingsError::McpBearerMissing
        );
        for invalid in [
            "abc",
            "G123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let error =
                Settings::from_lookup(lookup(&[("PRINTABLE_MCP_BEARER", invalid)])).unwrap_err();
            assert_eq!(error, SettingsError::McpBearerInvalid);
        }

        let secret = BearerSecret::parse(
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            )
            .to_string(),
        )
        .expect("test bearer is valid");
        assert!(secret.matches(concat!(
            "0123456789abcdef",
            "0123456789abcdef",
            "0123456789abcdef",
            "0123456789abcdef"
        )));
        for mismatch in [
            concat!(
                "f123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            ),
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "f123456789abcdef",
                "0123456789abcdef"
            ),
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdee"
            ),
        ] {
            assert!(!secret.matches(mismatch));
        }
    }

    #[test]
    fn bearer_file_is_bounded_and_exclusive_with_inline_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("bearer");
        std::fs::write(&path, format!("{}\n", "a".repeat(MCP_BEARER_BYTES))).unwrap();
        let path = path.to_str().unwrap();
        let get = |key: &str| (key == "PRINTABLE_MCP_BEARER_FILE").then(|| path.to_owned());
        let settings = Settings::from_sources(get, read_bearer_file).unwrap();
        assert!(settings.mcp_bearer.matches(&"a".repeat(MCP_BEARER_BYTES)));
        assert!(!format!("{settings:?}").contains(&"a".repeat(MCP_BEARER_BYTES)));
        assert_eq!(
            Settings::from_sources(lookup(&[("PRINTABLE_MCP_BEARER_FILE", path)]), |_| panic!(
                "must not read conflicting source"
            ))
            .unwrap_err(),
            SettingsError::McpBearerConflict
        );
        std::fs::write(path, "a".repeat(1024)).unwrap();
        assert_eq!(
            Settings::from_sources(get, read_bearer_file).unwrap_err(),
            SettingsError::McpBearerInvalid
        );
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            Settings::from_sources(get, read_bearer_file).unwrap_err(),
            SettingsError::McpBearerFileUnreadable
        );
    }

    #[test]
    fn download_base_requires_trusted_https_or_loopback_http_and_safe_prefix() {
        for value in [
            "https://files.example.com/",
            "https://files.example.com/printable/",
            "http://localhost:8000/",
            "http://127.0.0.1:8000/",
            "http://[::1]:8000/",
        ] {
            let settings =
                Settings::from_lookup(lookup(&[("PRINTABLE_DOWNLOAD_BASE_URL", value)])).unwrap();
            assert_eq!(settings.download_base_url.unwrap().as_str(), value);
        }
        for value in [
            "http://files.example.com/",
            "ftp://files.example.com/",
            "https://user:password@files.example.com/",
            "https://files.example.com/prefix",
            "https://files.example.com/?token=value",
            "https://files.example.com/#fragment",
            "https://*.example.com/",
        ] {
            let error = Settings::from_lookup(lookup(&[("PRINTABLE_DOWNLOAD_BASE_URL", value)]))
                .unwrap_err();
            assert_eq!(error, SettingsError::DownloadBaseInvalid);
            assert!(!error.to_string().contains(value));
        }
    }

    #[test]
    fn http_port_non_integer_reports_the_integer_error() {
        let err = Settings::from_lookup(lookup(&[("PRINTABLE_HTTP_PORT", "abc")])).unwrap_err();
        assert_eq!(err, SettingsError::PortNotInteger("PRINTABLE_HTTP_PORT"));
        assert_eq!(err.to_string(), "PRINTABLE_HTTP_PORT must be an integer");
    }

    #[test]
    fn http_port_empty_is_an_integer_error_not_a_default() {
        let err = Settings::from_lookup(lookup(&[("PRINTABLE_HTTP_PORT", "")])).unwrap_err();
        assert_eq!(err, SettingsError::PortNotInteger("PRINTABLE_HTTP_PORT"));
    }

    #[test]
    fn http_port_out_of_range_reports_the_range_error() {
        // The last is a valid integer literal too large for i64: still numeric,
        // so out-of-range, not a syntax error.
        for raw in ["0", "70000", "-1", "99999999999999999999999"] {
            let err = Settings::from_lookup(lookup(&[("PRINTABLE_HTTP_PORT", raw)])).unwrap_err();
            assert_eq!(
                err,
                SettingsError::PortOutOfRange("PRINTABLE_HTTP_PORT"),
                "raw={raw}"
            );
        }
        assert_eq!(
            SettingsError::PortOutOfRange("PRINTABLE_HTTP_PORT").to_string(),
            "PRINTABLE_HTTP_PORT must be between 1 and 65535"
        );
    }

    #[test]
    fn blender_port_errors_name_the_blender_var() {
        let err = Settings::from_lookup(lookup(&[("BLENDER_PORT", "nope")])).unwrap_err();
        assert_eq!(err.to_string(), "BLENDER_PORT must be an integer");
        let err = Settings::from_lookup(lookup(&[("BLENDER_PORT", "99999")])).unwrap_err();
        assert_eq!(err.to_string(), "BLENDER_PORT must be between 1 and 65535");
    }

    #[test]
    fn blender_workspace_root_requires_the_app_workspace_root() {
        let err = Settings::from_lookup(lookup(&[(
            "PRINTABLE_BLENDER_WORKSPACE_ROOT",
            "/data/blender",
        )]))
        .unwrap_err();
        assert_eq!(err, SettingsError::BlenderWorkspaceRequiresWorkspace);
        assert_eq!(
            err.to_string(),
            "PRINTABLE_BLENDER_WORKSPACE_ROOT requires PRINTABLE_WORKSPACE_ROOT"
        );
    }

    #[test]
    fn both_workspace_roots_set_is_accepted() {
        let s = Settings::from_lookup(lookup(&[
            ("PRINTABLE_WORKSPACE_ROOT", "/data/app"),
            ("PRINTABLE_BLENDER_WORKSPACE_ROOT", "/data/blender"),
        ]))
        .unwrap();
        assert_eq!(s.workspace_root, Some(PathBuf::from("/data/app")));
        assert_eq!(
            s.blender_workspace_root,
            Some(PathBuf::from("/data/blender"))
        );
    }

    #[test]
    fn optional_paths_are_read_when_non_empty() {
        let s = Settings::from_lookup(lookup(&[
            ("OPENSCAD_BIN", "/usr/bin/openscad"),
            (
                "PRINTABLE_GEOMETRY_WORKER_BIN",
                "/usr/local/bin/printable-geometry-worker",
            ),
            ("FFMPEG_BIN", "/opt/ffmpeg"),
        ]))
        .unwrap();
        assert_eq!(s.openscad_bin, Some(PathBuf::from("/usr/bin/openscad")));
        assert_eq!(
            s.geometry_worker_bin,
            Some(PathBuf::from("/usr/local/bin/printable-geometry-worker"))
        );
        assert_eq!(s.ffmpeg_bin, PathBuf::from("/opt/ffmpeg"));
    }

    #[test]
    fn allowed_hosts_parses_csv_and_falls_back() {
        let s = Settings::from_lookup(lookup(&[("PRINTABLE_ALLOWED_HOSTS", "a, b")])).unwrap();
        assert_eq!(s.allowed_hosts, vec!["a".to_string(), "b".to_string()]);
        let s = Settings::from_lookup(lookup(&[("PRINTABLE_ALLOWED_HOSTS", "  ")])).unwrap();
        assert_eq!(s.allowed_hosts, vec!["localhost", "127.0.0.1", "::1"]);
    }

    #[test]
    fn browser_origins_require_exact_serialized_http_origins() {
        let s = Settings::from_lookup(lookup(&[(
            "PRINTABLE_ALLOWED_ORIGINS",
            "https://app.example.com, http://localhost:3000, http://[::1]:3000",
        )]))
        .unwrap();
        assert_eq!(
            s.allowed_origins,
            [
                "https://app.example.com",
                "http://localhost:3000",
                "http://[::1]:3000"
            ]
        );
        for value in [
            "*",
            "null",
            "https://*.example.com",
            "https://app.example.com/",
            "https://user:password@app.example.com",
            "https://app.example.com/path",
            "https://app.example.com?query",
            "https://app.example.com#fragment",
            "file:///tmp/page",
        ] {
            let error =
                Settings::from_lookup(lookup(&[("PRINTABLE_ALLOWED_ORIGINS", value)])).unwrap_err();
            assert_eq!(error, SettingsError::AllowedOriginsInvalid);
            assert!(!error.to_string().contains(value));
        }
    }

    #[test]
    fn scad_concurrency_must_be_positive() {
        assert_eq!(
            Settings::from_lookup(lookup(&[("PRINTABLE_SCAD_CONCURRENCY", "4")]))
                .unwrap()
                .scad_concurrency,
            4
        );
        let err =
            Settings::from_lookup(lookup(&[("PRINTABLE_SCAD_CONCURRENCY", "0")])).unwrap_err();
        assert_eq!(
            err,
            SettingsError::ScadConcurrencyNotPositive("PRINTABLE_SCAD_CONCURRENCY")
        );
    }

    #[test]
    fn render_job_queue_depth_is_bounded_by_retained_metadata() {
        assert_eq!(
            Settings::from_lookup(lookup(&[("PRINTABLE_RENDER_JOB_QUEUE_DEPTH", "8")]))
                .unwrap()
                .render_job_queue_depth,
            8
        );
        for invalid in ["0", "1001", "-1", "nope"] {
            let error =
                Settings::from_lookup(lookup(&[("PRINTABLE_RENDER_JOB_QUEUE_DEPTH", invalid)]))
                    .unwrap_err();
            assert_eq!(
                error,
                SettingsError::RenderJobQueueDepthOutOfRange("PRINTABLE_RENDER_JOB_QUEUE_DEPTH")
            );
        }
    }

    #[test]
    fn geometry_worker_memory_must_be_positive() {
        assert_eq!(
            Settings::from_lookup(lookup(&[("PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB", "2048",)]))
                .unwrap()
                .geometry_worker_memory_bytes,
            2048 * 1024 * 1024
        );
        for invalid in ["0", "-1", "nope"] {
            let error =
                Settings::from_lookup(lookup(&[("PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB", invalid)]))
                    .unwrap_err();
            assert_eq!(
                error,
                SettingsError::GeometryWorkerMemoryNotPositive(
                    "PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB"
                )
            );
        }
        let error = Settings::from_lookup(lookup(&[(
            "PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB",
            "18446744073709551615",
        )]))
        .unwrap_err();
        assert_eq!(
            error,
            SettingsError::GeometryWorkerMemoryOutOfRange("PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB")
        );
    }
}

#[cfg(test)]
mod file_upload_tests {
    use super::*;

    #[test]
    fn incoming_file_budget_rejects_zero_invalid_and_overflow() {
        assert_eq!(
            file_upload_max_bytes(&|_| None).unwrap(),
            1024 * 1024 * 1024
        );
        for value in ["0", "-1", "invalid", "18446744073709551615"] {
            assert_eq!(
                file_upload_max_bytes(&|_| Some(value.into())),
                Err(SettingsError::FileUploadLimitInvalid)
            );
        }
        assert_eq!(
            file_upload_max_bytes(&|_| Some("256".into())).unwrap(),
            256 * 1024 * 1024
        );
    }
}
