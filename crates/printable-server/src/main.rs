//! Printable MCP server entry point: streamable HTTP at `/mcp`, `/healthz`
//! liveness, and `/readyz` dependency readiness.
//! A `--healthcheck` self-probe supports the container HEALTHCHECK.

use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use printable_server::{config::Settings, server::build_router};

#[derive(Parser, Debug)]
#[command(
    name = "printable-server",
    version,
    about = "MCP server for Blender/OpenSCAD 3D modeling"
)]
struct Cli {
    /// Bind host (overrides PRINTABLE_HTTP_HOST).
    #[arg(long)]
    host: Option<String>,

    /// Bind port (overrides PRINTABLE_HTTP_PORT).
    #[arg(long)]
    port: Option<u16>,

    /// MCP transport. Only `streamable-http` is supported; anything else is
    /// rejected so the contract is explicit.
    #[arg(long, default_value = "streamable-http")]
    transport: String,

    /// Loopback healthcheck for the container HEALTHCHECK directive: probe
    /// `/healthz` against the bound address and exit 0/1.
    #[arg(long, hide = true)]
    healthcheck: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(2);
        }
    };

    init_tracing();

    if cli.transport != "streamable-http" {
        eprintln!(
            "unsupported transport `{}` — only `streamable-http` is implemented",
            cli.transport
        );
        return ExitCode::from(2);
    }

    let host = cli.host.unwrap_or_else(|| settings.http_host.clone());
    let port = cli.port.unwrap_or(settings.http_port);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let result = runtime.block_on(async move {
        if cli.healthcheck {
            return run_healthcheck(&host, port).await;
        }
        run_server(settings, host, port).await
    });

    match result {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = %e, "fatal");
            ExitCode::from(1)
        }
    }
}

/// Initialize tracing from `RUST_LOG` (default `info`). Logging configuration
/// stays outside application settings.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .try_init();
}

async fn run_server(mut settings: Settings, host: String, port: u16) -> anyhow::Result<ExitCode> {
    // Bind first, then report the *actual* bound endpoint. Binding through
    // `ToSocketAddrs` resolves a hostname (`localhost`) or IPv6 literal (`::1`)
    // correctly; reading `local_addr()` back captures an OS-assigned ephemeral
    // port (`--port 0`) rather than the requested value.
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("bind {host}:{port}"))?;
    let addr = listener.local_addr().context("resolve bound address")?;

    // Fold the actual bound address into Settings so printable_status reports the
    // endpoint the server is really listening on, not the requested one.
    settings.http_host = addr.ip().to_string();
    settings.http_port = addr.port();

    let cancel = CancellationToken::new();
    let app = build_router(&settings, cancel.clone())?;

    tracing::info!(%addr, "printable-server listening");
    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum serve")?;
    Ok(ExitCode::SUCCESS)
}

/// Probe `/healthz` on the bound address. A wildcard bind is dialed on its
/// loopback, and IPv6 literals are bracketed so the URL is valid.
async fn run_healthcheck(host: &str, port: u16) -> anyhow::Result<ExitCode> {
    let target = healthcheck_authority(host, port);
    let url = format!("http://{target}/healthz");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(&url)
        .send()
        .await?;
    if resp.status().is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

/// The `host:port` authority for the healthcheck URL: map wildcard binds to
/// their loopback and bracket IPv6 literals so the URL is valid.
fn healthcheck_authority(host: &str, port: u16) -> String {
    let target = match host {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        h if h.contains(':') && !h.starts_with('[') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("{target}:{port}")
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use printable_server::config::Settings;

    use super::{healthcheck_authority, run_server};

    #[test]
    fn healthcheck_authority_maps_wildcards_and_brackets_ipv6() {
        assert_eq!(healthcheck_authority("0.0.0.0", 8000), "127.0.0.1:8000");
        assert_eq!(healthcheck_authority("::", 8000), "[::1]:8000");
        assert_eq!(healthcheck_authority("[::]", 8000), "[::1]:8000");
        assert_eq!(healthcheck_authority("::1", 8000), "[::1]:8000");
        assert_eq!(healthcheck_authority("[::1]", 8000), "[::1]:8000");
        assert_eq!(healthcheck_authority("127.0.0.1", 8000), "127.0.0.1:8000");
        assert_eq!(healthcheck_authority("localhost", 8000), "localhost:8000");
    }

    #[tokio::test]
    async fn bind_failure_retains_endpoint_context_and_io_cause() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied loopback port");
        let port = occupied.local_addr().expect("occupied address").port();
        let settings = Settings::from_lookup(|key| {
            (key == "PRINTABLE_MCP_BEARER").then(|| {
                concat!(
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef"
                )
                .to_string()
            })
        })
        .expect("default settings");

        let error = run_server(settings, "127.0.0.1".to_string(), port)
            .await
            .expect_err("a second listener cannot bind the occupied port");

        assert_eq!(error.to_string(), format!("bind 127.0.0.1:{port}"));
        let source = error
            .chain()
            .last()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("bind context retains the source I/O error");
        assert_eq!(source.kind(), ErrorKind::AddrInUse);
    }
}
