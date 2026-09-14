//! Recovery from failed accepts.

use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use super::ListenerError;
use crate::net::{util::retry::ExponentialBackoff, ListenAddress};

/// Shortest wait between accepts while the system is out of a resource required to accept connections.
///
/// Small enough that a momentary spike in open descriptors costs a listener almost nothing.
const DEFAULT_MIN_BACKOFF: Duration = Duration::from_millis(10);

/// Longest wait between accepts while the system is out of a resource required to accept connections.
///
/// A sustained shortage then costs one system call a second rather than a spinning core, while still resuming promptly
/// once the resource frees up.
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(1);

/// Jitter applied to the backoff, as the factor the delay may be divided by.
///
/// Every listener in the process draws on the same descriptor table, so a shortage tends to hit all of them at once.
/// Without jitter they would then retry in lockstep, turning one shortage into a repeating thundering herd.
const DEFAULT_BACKOFF_JITTER: f64 = 2.0;

/// Returns the backoff applied to accepts when the system is out of resources, absent an override.
fn default_backoff() -> ExponentialBackoff {
    ExponentialBackoff::with_jitter(DEFAULT_MIN_BACKOFF, DEFAULT_MAX_BACKOFF, DEFAULT_BACKOFF_JITTER)
}

/// What to do about a failed accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAction {
    /// The failure concerned only the connection being accepted.
    ///
    /// Accept again immediately.
    Retry,

    /// The system is out of a resource required to accept connections.
    ///
    /// Wait, then accept again.
    Throttle,

    /// The listener can't produce further connections.
    ///
    /// The error is returned to the caller.
    Fatal,
}

/// Classifies a failed accept.
///
/// Only a failure to accept is ever recoverable. Everything else -- an address that couldn't be bound, a setting that
/// couldn't be applied -- describes a listener that was never usable, or a connection that can't be safely handed on.
fn classify_accept_error(error: &ListenerError) -> RecoveryAction {
    let source = match error {
        ListenerError::FailedToAccept { source, .. } => source,
        _ => return RecoveryAction::Fatal,
    };

    // The peer went away, or the call was interrupted, between the connection arriving and us taking it.
    if matches!(
        source.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    ) {
        return RecoveryAction::Retry;
    }

    // Out of memory is the one exhaustion case with a stable `ErrorKind`; the rest have to be matched on errno.
    if source.kind() == std::io::ErrorKind::OutOfMemory {
        return RecoveryAction::Throttle;
    }

    #[cfg(unix)]
    if let Some(errno) = source.raw_os_error() {
        match errno {
            // Process- or system-wide exhaustion of descriptors or buffers.
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS => return RecoveryAction::Throttle,

            // Network errors already pending on the new connection, which Linux reports out of `accept` rather than
            // out of a later read on the accepted socket. `accept(2)` is explicit that these should be treated like
            // `EAGAIN` and retried.
            libc::EPROTO
            | libc::ENOPROTOOPT
            | libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::EOPNOTSUPP => return RecoveryAction::Retry,
            #[cfg(target_os = "linux")]
            libc::ENONET => return RecoveryAction::Retry,

            _ => {}
        }
    }

    RecoveryAction::Fatal
}

/// Recovery state for accept errors.
///
/// Not all failed accepts are equal: sometimes an incoming connection can experience an error before the application is
/// able to accept it, and other times, transient errors with the underlying system (file descriptor exhaustion, etc)
/// can occur, both of which leave the listener in a state where another accept can be attempted.
///
/// `AcceptRecovery` is responsible for not only categorizing an accept error (retryable vs fatal), but also tracking
/// the state of previous retry attempts in order to apply throttling between consecutive accepts. This ensures that
/// listeners avoid consuming excess system resources by busy looping during an already transient resource exhaustion
/// issue.
pub(super) struct AcceptRecovery {
    backoff: ExponentialBackoff,
    consecutive_throttled_accepts: u32,
}

impl AcceptRecovery {
    /// Creates a new `AcceptRecovery` using the given backoff.
    pub(super) fn from_backoff(backoff: ExponentialBackoff) -> Self {
        Self {
            backoff,
            consecutive_throttled_accepts: 0,
        }
    }

    /// Tracks a successful accept, resetting the throttle state.
    pub(super) fn accept_succeeded(&mut self) {
        self.reset();
    }

    /// Resets the throttle state.
    pub(super) fn reset(&mut self) {
        self.consecutive_throttled_accepts = 0;
    }

    /// Attempts to recover from a failed accept.
    ///
    /// If the error is recoverable (either immediately or after a delay), `Ok(())` is returned after any relevant
    /// backoff delay has occurred. Otherwise, `Err(error)` is returned with the original error. All non-accept errors
    /// are always passed through as-is.
    pub(super) async fn recover(
        &mut self, listen_address: &ListenAddress, error: ListenerError,
    ) -> Result<(), ListenerError> {
        match classify_accept_error(&error) {
            RecoveryAction::Retry => {
                debug!(%listen_address, %error, "Failed to accept an incoming connection. Retrying.");
            }
            RecoveryAction::Throttle => {
                let delay = self.backoff.get_backoff_duration(self.consecutive_throttled_accepts);
                self.consecutive_throttled_accepts = self.consecutive_throttled_accepts.saturating_add(1);

                warn!(%listen_address, %error, ?delay, "Failed to accept an incoming connection. Retrying shortly.");

                sleep(delay).await;
            }
            RecoveryAction::Fatal => return Err(error),
        }

        Ok(())
    }
}

impl Default for AcceptRecovery {
    fn default() -> Self {
        Self::from_backoff(default_backoff())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::net::listener::SOCKET_RECV_BUFFER_SIZE_SETTING;

    /// Builds the error an accept loop would actually see for a given `errno`.
    #[cfg(unix)]
    fn accept_errno(errno: i32) -> ListenerError {
        ListenerError::FailedToAccept {
            address: ListenAddress::Tcp(([127, 0, 0, 1], 0).into()),
            source: io::Error::from_raw_os_error(errno),
        }
    }

    #[cfg(unix)]
    #[test]
    fn resource_exhaustion_throttles_rather_than_stopping_the_listener() {
        // The case this classification exists for: hitting the open-file limit says nothing about the listener, and
        // the connection is still queued, so stopping would take the listener down over a condition that clears on
        // its own.
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(
                classify_accept_error(&accept_errno(errno)),
                RecoveryAction::Throttle,
                "errno {errno} should throttle"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn per_connection_failures_retry_immediately() {
        // Nothing about the listener changed, so there is nothing to wait for.
        for errno in [
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EPROTO,
            libc::EHOSTUNREACH,
            libc::ENETUNREACH,
        ] {
            assert_eq!(
                classify_accept_error(&accept_errno(errno)),
                RecoveryAction::Retry,
                "errno {errno} should retry"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unrecognized_accept_failure_stops_the_listener() {
        // Retrying an error we can't account for is how an accept loop spins forever, so anything unclassified stops
        // the listener.
        assert_eq!(classify_accept_error(&accept_errno(libc::EBADF)), RecoveryAction::Fatal);
    }

    #[test]
    fn only_accept_failures_are_recoverable() {
        // A listener that never bound, or a stream that couldn't be configured, isn't something an accept loop can
        // retry its way out of.
        let bind = ListenerError::FailedToBind {
            address: ListenAddress::Tcp(([127, 0, 0, 1], 0).into()),
            source: io::Error::from_raw_os_error(1),
        };
        assert_eq!(classify_accept_error(&bind), RecoveryAction::Fatal);

        let configure = ListenerError::FailedToConfigureStream {
            setting: SOCKET_RECV_BUFFER_SIZE_SETTING,
            stream_type: "tcp",
            source: io::Error::from_raw_os_error(1),
        };
        assert_eq!(classify_accept_error(&configure), RecoveryAction::Fatal);
    }

    #[test]
    fn the_default_backoff_stays_within_its_bounds() {
        let mut backoff = default_backoff();

        // The first wait is the floor exactly, so a one-off shortage costs a listener almost nothing.
        assert_eq!(backoff.get_backoff_duration(0), DEFAULT_MIN_BACKOFF);

        // Past that, jitter makes each draw a range rather than a value, so the bounds are what there is to assert --
        // and they have to hold however long a shortage lasts, including for counts large enough to overflow a naive
        // doubling.
        for consecutive in [1, 2, 8, 32, 64, u32::MAX] {
            let delay = backoff.get_backoff_duration(consecutive);
            assert!(
                delay >= DEFAULT_MIN_BACKOFF && delay <= DEFAULT_MAX_BACKOFF,
                "delay for {consecutive} consecutive failures should be within bounds, got {delay:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_successful_accept_clears_the_throttle_history() {
        let mut recovery = AcceptRecovery::default();
        let address = ListenAddress::Tcp(([127, 0, 0, 1], 0).into());

        // Walk the backoff up, then clear it, and check the next wait starts from the floor again rather than carrying
        // on from where it left off.
        for _ in 0..4 {
            recovery
                .recover(&address, accept_errno(libc::EMFILE))
                .await
                .expect("resource exhaustion should be recovered from");
        }
        assert_ne!(recovery.consecutive_throttled_accepts, 0);

        recovery.accept_succeeded();
        assert_eq!(recovery.consecutive_throttled_accepts, 0);

        recovery.reset();
        assert_eq!(recovery.consecutive_throttled_accepts, 0);
    }

    #[tokio::test]
    async fn an_unrecoverable_failure_is_handed_back() {
        let mut recovery = AcceptRecovery::default();
        let address = ListenAddress::Tcp(([127, 0, 0, 1], 0).into());

        let error = recovery
            .recover(&address, accept_errno(libc::EBADF))
            .await
            .expect_err("an unclassified failure should be handed back");
        assert!(matches!(error, ListenerError::FailedToAccept { .. }));
    }
}
