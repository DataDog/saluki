//! Bounded, cancellable waits for harness operations.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Maximum duration of one polling attempt.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause between polling attempts.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why [`probe`] returned without its operation's output.
#[derive(Debug)]
pub(crate) enum ProbeError {
    /// The cancellation token fired.
    Cancelled { what: String, waited: Duration },
    /// The operation didn't finish within its limit.
    TimedOut {
        what: String,
        limit: Duration,
        source: Box<dyn Error + Send + Sync>,
    },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { what, waited } => write!(f, "Cancelled after {waited:.1?} waiting for {what}."),
            Self::TimedOut { what, limit, .. } => write!(f, "Timed out after {limit:?} waiting for {what}."),
        }
    }
}

impl Error for ProbeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled { .. } => None,
            Self::TimedOut { source, .. } => Some(source.as_ref()),
        }
    }
}

/// Awaits `operation` for at most `limit`, returning early if `cancel` fires.
///
/// `what` identifies the operation and service in error messages. Callers derive `limit` from their step or setup
/// budget; the runner enforces the case deadline through `cancel`.
///
/// # Errors
///
/// Returns [`ProbeError::Cancelled`] if `cancel` is already cancelled or fires during the wait, or
/// [`ProbeError::TimedOut`] if `limit` expires.
pub(crate) async fn probe<F>(
    what: &str, limit: Duration, cancel: &CancellationToken, operation: F,
) -> Result<F::Output, ProbeError>
where
    F: Future,
{
    let started = Instant::now();
    match cancel.run_until_cancelled(tokio::time::timeout(limit, operation)).await {
        Some(Ok(output)) => Ok(output),
        Some(Err(e)) => Err(ProbeError::TimedOut {
            what: what.to_string(),
            limit,
            source: Box::new(e),
        }),
        None => Err(ProbeError::Cancelled {
            what: what.to_string(),
            waited: started.elapsed(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::future::pending;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::{probe, ProbeError};

    #[tokio::test]
    async fn a_completed_operation_returns_its_output_unchanged() {
        let output = probe(
            "an operation",
            Duration::from_secs(1),
            &CancellationToken::new(),
            async { vec![1, 2, 3] },
        )
        .await;

        assert_eq!(output.expect("a ready operation should complete"), [1, 2, 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_already_cancelled_probe_returns_without_polling_its_operation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let polled = AtomicBool::new(false);

        let result = probe("an operation", Duration::from_secs(10), &cancel, async {
            polled.store(true, Ordering::SeqCst);
        })
        .await;

        let error = result.expect_err("a cancelled token should win");
        assert!(
            matches!(
                error,
                ProbeError::Cancelled {
                    waited: Duration::ZERO,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!polled.load(Ordering::SeqCst), "the operation should not be polled");
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_the_wait_reports_how_long_it_waited() {
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            canceller.cancel();
        });

        let result = probe("an operation", Duration::from_secs(10), &cancel, pending::<()>()).await;

        let error = result.expect_err("cancellation should end the wait");
        assert!(
            matches!(error, ProbeError::Cancelled { waited, .. } if waited == Duration::from_millis(1500)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "Cancelled after 1.5s waiting for an operation.");
        assert!(error.source().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_that_outlasts_its_limit_times_out_with_the_elapsed_cause() {
        let started = Instant::now();

        let result = probe(
            "an operation",
            Duration::from_secs(2),
            &CancellationToken::new(),
            pending::<()>(),
        )
        .await;

        assert_eq!(started.elapsed(), Duration::from_secs(2));
        let error = result.expect_err("the limit should end the wait");
        assert!(
            matches!(error, ProbeError::TimedOut { limit, .. } if limit == Duration::from_secs(2)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "Timed out after 2s waiting for an operation.");
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("deadline has elapsed")
        );
    }
}
