use std::{
    future::{pending, Future},
    pin::Pin,
};

use tokio::sync::oneshot;

/// A shutdown signal for a process.
///
/// This struct can be used to wait for a shutdown signal from the supervisor to which the process belongs.
pub struct ProcessShutdown {
    shutdown: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

/// A handle to trigger process shutdown.
pub struct ShutdownHandle {
    shutdown_tx: oneshot::Sender<()>,
}

impl ProcessShutdown {
    /// Creates a new `ProcessShutdown` and `ShutdownHandle` pair.
    ///
    /// When `ShutdownHandle` is triggered, or dropped, `ProcessShutdown` will resolve.
    pub fn paired() -> (Self, ShutdownHandle) {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = ShutdownHandle { shutdown_tx };

        let process_shutdown = Self {
            shutdown: Some(Box::pin(async move {
                let _ = shutdown_rx.await;
            })),
        };

        (process_shutdown, handle)
    }

    /// Creates a new `ProcessShutdown` from the given `future`.
    ///
    /// `ProcessShutdown` will be resolve only once `future` resolves.
    pub fn wrapped<F: Future + Send + 'static>(future: F) -> Self {
        Self {
            shutdown: Some(Box::pin(async move {
                future.await;
            })),
        }
    }

    /// Creates a new `ProcessShutdown` that never resolves.
    ///
    /// This is useful for cases where a `ProcessShutdown` is required, but no shutdown signal is expected.
    pub fn noop() -> Self {
        Self {
            shutdown: Some(Box::pin(async { pending().await })),
        }
    }

    /// Waits for the shutdown signal to be received.
    ///
    /// If the shutdown signal has been received during a previous call to this function, this function will return
    /// immediately for all subsequent calls.
    pub async fn wait_for_shutdown(&mut self) {
        if let Some(shutdown_rx) = self.shutdown.take() {
            let _ = shutdown_rx.await;
        }
    }
}

impl ShutdownHandle {
    /// Triggers the process to shutdown.
    pub fn trigger(self) {
        let _ = self.shutdown_tx.send(());
    }
}
