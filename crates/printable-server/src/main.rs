//! Printable MCP server entry point: streamable HTTP at `/mcp`, `/healthz`
//! liveness, and `/readyz` dependency readiness, plus the optional read-only
//! library web page on a listener of its own.
//! A `--healthcheck` self-probe supports the container HEALTHCHECK.

use std::{future::IntoFuture, net::SocketAddr, process::ExitCode};

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use printable_server::{config::Settings, server::build_routers};

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
    let signal = printable_server::shutdown::signal().context("register shutdown signals")?;
    // Bind first, then report the *actual* bound endpoint. Binding through
    // `ToSocketAddrs` resolves a hostname (`localhost`) or IPv6 literal (`::1`)
    // correctly; reading `local_addr()` back captures an OS-assigned ephemeral
    // port (`--port 0`) rather than the requested value.
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("bind {host}:{port}"))?;
    let addr = listener.local_addr().context("resolve bound address")?;
    // The web page gets a listener of its own, bound before any work starts.
    let web_listener = match &settings.web {
        None => None,
        Some(web) => {
            if web.listen.port() == addr.port() {
                anyhow::bail!(
                    "PRINTABLE_WEB_LISTEN must use a different port from the MCP listener"
                );
            }
            let web_listener = tokio::net::TcpListener::bind(web.listen)
                .await
                .with_context(|| format!("bind the web page listener {}", web.listen))?;
            Some(web_listener)
        }
    };

    // Fold the actual bound address into Settings so printable_status reports the
    // endpoint the server is really listening on, not the requested one.
    settings.http_host = addr.ip().to_string();
    settings.http_port = addr.port();

    let cancel = CancellationToken::new();
    let routers = build_routers(&settings, cancel.clone())?;

    tracing::info!(%addr, "printable-server listening");
    let web_shutdown = cancel.clone();
    let shutdown = async move {
        signal.await;
        cancel.cancel();
    };
    let mcp = axum::serve(listener, routers.mcp)
        .with_graceful_shutdown(shutdown)
        .into_future();
    let (Some(web_listener), Some(web)) = (web_listener, routers.web) else {
        mcp.await.context("axum serve")?;
        return Ok(ExitCode::SUCCESS);
    };
    let web_addr = web_listener
        .local_addr()
        .context("resolve the web page address")?;
    tracing::info!(addr = %web_addr, "project library web page listening");
    // The peer address lets the web page refuse anything but its trusted proxy.
    let web = axum::serve(
        web_listener,
        web.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { web_shutdown.cancelled().await })
    .into_future();
    let mcp = async { mcp.await.context("axum serve") };
    let web = async { web.await.context("web page serve") };
    tokio::try_join!(mcp, web)?;
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
    async fn the_web_page_never_shares_the_mcp_port() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let root = workspace.path().to_str().expect("UTF-8 path").to_string();
        let free = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
        let port = free.local_addr().expect("reserved address").port();
        drop(free);
        let listen = format!("0.0.0.0:{port}");
        let settings = Settings::from_lookup(|key| {
            let value = match key {
                "PRINTABLE_MCP_BEARER" => concat!(
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef",
                    "0123456789abcdef"
                ),
                "PRINTABLE_WORKSPACE_ROOT" => root.as_str(),
                "PRINTABLE_WEB_LISTEN" => listen.as_str(),
                "PRINTABLE_WEB_TRUSTED_PROXIES" => "127.0.0.1",
                "PRINTABLE_WEB_USER_HEADER" => "X-Printable-User",
                "PRINTABLE_WEB_USERS" => "alice",
                _ => return None,
            };
            Some(value.to_string())
        })
        .expect("web page settings");

        let error = run_server(settings, "127.0.0.1".to_string(), port)
            .await
            .expect_err("the web page cannot use the MCP port");

        assert_eq!(
            error.to_string(),
            "PRINTABLE_WEB_LISTEN must use a different port from the MCP listener"
        );
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
