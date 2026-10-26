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
