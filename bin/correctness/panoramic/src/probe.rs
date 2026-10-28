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

/// Shortest request limit, so curl never gets `--max-time 0`, which disables its limit.
const MIN_REQUEST: Duration = Duration::from_millis(1);

/// Limits for one polling attempt.
///
/// A timed-out attempt abandons its Docker exec without stopping it, so a request sent from inside the target container
/// bounds itself with `request`. The request gets half of the attempt, reserving the rest for Docker, so a target that
/// accepts the connection but never answers normally fails the request before the attempt times out. This is not
/// guaranteed: Docker overhead can exceed the reserve, and the 1 ms floor can exceed an attempt that is nearly spent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AttemptLimits {
    /// Limit for the whole attempt.
    pub(crate) attempt: Duration,
    /// Limit for a request sent from inside the target container.
    pub(crate) request: Duration,
}

impl AttemptLimits {
    /// Returns the limits for an attempt with `remaining` left in its step budget.
    pub(crate) fn new(remaining: Duration) -> Self {
        let attempt = remaining.min(PROBE_TIMEOUT);
        Self {
            attempt,
            request: (attempt / 2).max(MIN_REQUEST),
        }
    }

    /// Returns `request` as a curl `--max-time` value.
    pub(crate) fn curl_max_time(&self) -> String {
        format!("{:.3}", self.request.as_secs_f64())
    }
}

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

    use super::{probe, AttemptLimits, ProbeError, PROBE_TIMEOUT};

    #[test]
    fn an_in_container_request_gets_half_of_its_attempt() {
        let full = AttemptLimits::new(Duration::from_secs(60));
        assert_eq!(full.attempt, PROBE_TIMEOUT);
        assert_eq!(full.request, PROBE_TIMEOUT / 2);

        let short = AttemptLimits::new(Duration::from_secs(3));
        assert_eq!(short.attempt, Duration::from_secs(3));
        assert_eq!(short.request, Duration::from_millis(1500));
        assert_eq!(short.curl_max_time(), "1.500");

        let spent = AttemptLimits::new(Duration::ZERO);
        assert_eq!(spent.attempt, Duration::ZERO);
        assert_eq!(spent.curl_max_time(), "0.001", "curl treats a zero limit as no limit");
    }

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
