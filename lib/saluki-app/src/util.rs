//! General-purpose application utilities.

use std::{ffi::OsStr, num::NonZeroUsize};

use saluki_error::{generic_error, GenericError};
use tracing::info;

/// Environment variable that overrides the parallelism of the async runtime.
const WORKER_THREADS_ENV_VAR: &str = "SALUKI_WORKER_THREADS";

/// Minimum parallelism of the async runtime when the parallelism isn't overridden.
///
/// With a single worker thread, any task doing a long stretch of synchronous work stalls every other task on the
/// runtime, including those responsible for health checks. A second worker thread lets those tasks keep running, even
/// when the system reports a parallelism of one.
const MIN_ASYNC_RUNTIME_PARALLELISM: usize = 2;

/// Determines the parallelism -- the number of worker threads -- to use for the primary async runtime.
///
/// If `SALUKI_WORKER_THREADS` is set, its value is used as-is. Otherwise, the
/// [available parallelism][std::thread::available_parallelism] of the system is used, or a minimum of two worker
/// threads, whichever is greater.
///
/// # Errors
///
/// If `SALUKI_WORKER_THREADS` is set but isn't a positive integer, an error is returned.
pub fn get_async_runtime_parallelism() -> Result<usize, GenericError> {
    get_async_runtime_parallelism_inner(
        std::env::var_os(WORKER_THREADS_ENV_VAR).as_deref(),
        std::thread::available_parallelism().ok(),
    )
}

fn get_async_runtime_parallelism_inner(
    parallelism_override: Option<&OsStr>, available_parallelism: Option<NonZeroUsize>,
) -> Result<usize, GenericError> {
    match parallelism_override {
        Some(raw_override) => raw_override
            .to_str()
            .and_then(|s| s.parse::<NonZeroUsize>().ok())
            .map(NonZeroUsize::get)
            .ok_or_else(|| {
                generic_error!(
                    "`{}` must be a positive integer, got {:?}.",
                    WORKER_THREADS_ENV_VAR,
                    raw_override
                )
            }),
        None => Ok(available_parallelism
            .map_or(1, NonZeroUsize::get)
            .max(MIN_ASYNC_RUNTIME_PARALLELISM)),
    }
}

/// Waits for a shutdown signal.
///
/// On Unix, this waits for either `SIGINT` or `SIGTERM`, either of which are used to request a graceful shutdown:
/// `SIGINT` interactively (`Ctrl+C`), and `SIGTERM` by process supervisors (systemd, container runtimes,
/// Kubernetes) during rollouts, evictions, node drains, and container shutdown.
///
/// On Windows, this waits for either `CTRL_C_EVENT` (interactively) or `CTRL_BREAK_EVENT`, the latter being what a
/// parent process supervisor sends via `GenerateConsoleCtrlEvent` to request a graceful stop.
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

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, num::NonZeroUsize};

    use super::{get_async_runtime_parallelism_inner, MIN_ASYNC_RUNTIME_PARALLELISM, WORKER_THREADS_ENV_VAR};

    #[test]
    fn async_runtime_parallelism_uses_available_parallelism_above_minimum() {
        let parallelism = get_async_runtime_parallelism_inner(None, NonZeroUsize::new(8)).expect("no override");
        assert_eq!(parallelism, 8);
    }

    #[test]
    fn async_runtime_parallelism_enforces_minimum_without_override() {
        for available_parallelism in [None, NonZeroUsize::new(1)] {
            let parallelism = get_async_runtime_parallelism_inner(None, available_parallelism).expect("no override");
            assert_eq!(parallelism, MIN_ASYNC_RUNTIME_PARALLELISM);
        }
    }

    #[test]
    fn async_runtime_parallelism_uses_override_as_is() {
        for (raw_override, expected) in [("1", 1), ("3", 3), ("16", 16)] {
            let parallelism = get_async_runtime_parallelism_inner(Some(OsStr::new(raw_override)), NonZeroUsize::new(8))
                .expect("valid override");
            assert_eq!(parallelism, expected);
        }
    }

    #[test]
    fn async_runtime_parallelism_rejects_invalid_override() {
        for raw_override in ["", "0", "-1", "four", " 4"] {
            let error = get_async_runtime_parallelism_inner(Some(OsStr::new(raw_override)), NonZeroUsize::new(8))
                .expect_err("invalid override is rejected");
            assert!(
                error.to_string().contains(WORKER_THREADS_ENV_VAR),
                "error should name the environment variable: {error}"
            );
        }
    }
}
