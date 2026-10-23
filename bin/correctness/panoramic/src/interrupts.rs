//! Merges platform-specific interrupt signals into one channel.
//!
//! The TUI handles raw-mode interrupt keys separately as key events.

use tokio::sync::mpsc;
use tracing::warn;

/// Forwards signal notifications as `$source`, logging handler installation failures.
macro_rules! forward {
    ($tx:expr, $source:expr, $installed:expr) => {
        match $installed {
            Ok(mut stream) => {
                let tx = $tx.clone();
                tokio::spawn(async move {
                    while stream.recv().await.is_some() {
                        if tx.send($source).is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => warn!(error = %e, "Failed to install the {} handler; it will not interrupt the run.", $source),
        }
    };
}

/// Receives SIGINT and SIGTERM on Unix, or Ctrl-C and Ctrl-Break on Windows.
///
/// Requests follow channel send order; signal streams may coalesce deliveries.
pub(crate) struct Interrupts {
    rx: mpsc::UnboundedReceiver<&'static str>,
}

impl Interrupts {
    /// Installs signal handlers and forwarding tasks, logging and skipping installation failures.
    ///
    /// Installed handlers replace default termination. Forwarded requests stay queued until [`Interrupts::recv`].
    pub(crate) fn install() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            forward!(tx, "SIGINT", signal(SignalKind::interrupt()));
            forward!(tx, "SIGTERM", signal(SignalKind::terminate()));
        }

        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c};
            forward!(tx, "Ctrl-C", ctrl_c());
            forward!(tx, "Ctrl-Break", ctrl_break());
        }

        Self { rx }
    }

    /// Returns the next request's source, or `None` after all senders close and queued requests are drained.
    ///
    /// Dropping the returned future leaves pending requests queued.
    pub(crate) async fn recv(&mut self) -> Option<&'static str> {
        self.rx.recv().await
    }

    /// Creates an interrupt channel for tests to drive directly.
    #[cfg(test)]
    pub(crate) fn manual() -> (mpsc::UnboundedSender<&'static str>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Self { rx })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn raise_and_receive(interrupts: &mut Interrupts, signal: &str) -> Option<&'static str> {
        let status = std::process::Command::new("kill")
            .args([signal, &std::process::id().to_string()])
            .status()
            .expect("kill should run");
        assert!(status.success(), "kill {} failed: {}", signal, status);

        tokio::time::timeout(Duration::from_secs(5), interrupts.recv())
            .await
            .unwrap_or_else(|_| panic!("the {} signal should arrive within 5s", signal))
    }

    #[tokio::test]
    async fn sigint_and_sigterm_are_reported_as_interrupts() {
        // Register handlers before signalling the test process to keep it alive.
        let mut interrupts = Interrupts::install();

        assert_eq!(raise_and_receive(&mut interrupts, "-TERM").await, Some("SIGTERM"));
        assert_eq!(raise_and_receive(&mut interrupts, "-INT").await, Some("SIGINT"));
    }
}
