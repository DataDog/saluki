use tracing::info;

/// Waits for a shutdown signal.
///
/// On Unix, this waits for either `SIGINT` or `SIGTERM`, either of which are used to request a graceful shutdown:
/// `SIGINT` interactively (`Ctrl+C`), and `SIGTERM` by process supervisors (systemd, container runtimes,
/// Kubernetes) during rollouts, evictions, node drains, and container shutdown.
///
/// On Windows, this waits for either `CTRL_C_EVENT` (interactively) or `CTRL_BREAK_EVENT`, the latter being what
/// `dd-procmgr` (which manages ADP as a subprocess on Windows) sends via `GenerateConsoleCtrlEvent` to request a
/// graceful stop.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("Received SIGINT, shutting down..."),
            _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
        }
    }

    #[cfg(windows)]
    {
        let mut ctrl_break = tokio::signal::windows::ctrl_break().expect("failed to install CTRL_BREAK handler");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("Received CTRL_C, shutting down..."),
            _ = ctrl_break.recv() => info!("Received CTRL_BREAK, shutting down..."),
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = tokio::signal::ctrl_c().await;

        info!("Received SIGINT, shutting down...");
    }
}
