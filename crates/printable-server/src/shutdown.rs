//! Register shutdown signals before accepting requests, then drain HTTP work.

use std::{future::Future, io};

/// Install both Unix signal handlers before the returned future is polled.
/// Docker's ordinary stop signal and an interactive interrupt share the same
/// shutdown path. An installation failure prevents serving traffic.
#[cfg(unix)]
pub fn signal() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        tracing::info!("shutdown requested; draining accepted HTTP work");
    })
}

#[cfg(not(unix))]
pub fn signal() -> io::Result<impl Future<Output = ()>> {
    Ok(async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "shutdown signal failed");
        }
    })
}
