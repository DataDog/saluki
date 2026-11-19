//! Retries of the Docker operations that fail now and then for a reason that clears by itself.

use std::{
    collections::hash_map::DefaultHasher,
    future::Future,
    hash::{Hash as _, Hasher as _},
    time::Duration,
};

use bollard::errors::Error;
use saluki_error::GenericError;
use tokio::time::sleep;
use tracing::warn;

/// Configuration for transient error retries.
///
/// Transient error retries are a class of retries that are applied to a select few operations that
/// have known error cases where retrying is safe and likely to fix the issue.
const TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS: u32 = 3;
const TRANSIENT_ERROR_RETRY_BASE_BACKOFF: Duration = Duration::from_millis(250);
const TRANSIENT_ERROR_RETRY_MAX_JITTER: Duration = Duration::from_millis(500);

/// Returns `true` if `error` is a transient error that can be retried.
fn is_transient_error(error: &GenericError) -> bool {
    is_transient_hns_error(error)
}

/// Returns `true` if `error` is a transient Windows HNS collision.
///
/// Windows containers are networked through the Host Network Service, and HNS serializes poorly: when several
/// containers create a NAT network or attach an endpoint at the same moment, the losing caller gets a 500 from the
/// daemon carrying `hnsCall failed in Win32: The object already exists. (0x1392)`. The object in question is internal
/// to HNS -- airlock names every network after a unique isolation group, so this is never a name conflict of ours --
/// and the conflict clears once the winning call completes.
///
/// The predicate matches on the message rather than on the container OS because `hnsCall` has no analogue on Linux,
/// which makes it inert there. It deliberately requires the `0x1392` code as well as `hnsCall`: other HNS failures
/// indicate real breakage and should fail fast rather than be papered over by a retry.
fn is_transient_hns_error(error: &GenericError) -> bool {
    error.downcast_ref::<Error>().is_some_and(|error| match error {
        Error::DockerResponseServerError { message, .. } => message.contains("hnsCall") && message.contains("0x1392"),
        _ => false,
    })
}

/// Returns how long to wait before the given transient error retry attempt.
///
/// `seed` keeps concurrent callers out of step with each other. Isolation group IDs are unique per
/// test, so two drivers that collided at the same instant draw different jitter and don't line
/// back up on the retry.
fn transient_error_retry_backoff(attempt: u32, seed: &str) -> Duration {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    attempt.hash(&mut hasher);

    let jitter_ms = hasher.finish() % (TRANSIENT_ERROR_RETRY_MAX_JITTER.as_millis() as u64 + 1);

    TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt + Duration::from_millis(jitter_ms)
}

/// Runs `run_attempt`, retrying it when it fails with a known transient error.
///
/// Any other error, and the error from the final attempt, is returned to the caller untouched. "Known" transient errors
/// are hard-coded/categorized by hand, so this function mostly represents known areas where we've seen transient issues
/// that can generally be solved by simply retrying.
///
/// `run_attempt` must be safe to re-run: every call site either re-checks for the resource it is creating, or targets a
/// Docker endpoint that is idempotent.
///
/// Each retry is logged at warning level so that runs which only passed because of a retry stay visible in CI output,
/// rather than the mitigation quietly hiding how often the collision fires.
pub(crate) async fn with_transient_error_retry<F, Fut, T>(
    operation: &str, seed: &str, mut run_attempt: F,
) -> Result<T, GenericError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, GenericError>>,
{
    let mut attempt = 1;
    loop {
        match run_attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if attempt >= TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS || !is_transient_error(&e) {
                    return Err(e);
                }

                let backoff = transient_error_retry_backoff(attempt, seed);
                warn!(
                    seed,
                    error = %e,
                    "Transient error during {}. Retrying in {:?} (attempt {} of {}).",
                    operation,
                    backoff,
                    attempt + 1,
                    TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS
                );

                sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_error::generic_error;

    use super::*;

    fn docker_server_error(status_code: u16, message: &str) -> GenericError {
        Error::DockerResponseServerError {
            status_code,
            message: message.to_string(),
        }
        .into()
    }

    #[test]
    fn hns_object_already_exists_is_treated_as_transient() {
        // Verbatim message from a failed `test-integration-windows-amd64` run.
        let error = docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
        );

        assert!(is_transient_hns_error(&error));
    }

    #[test]
    fn transient_hns_error_is_still_detected_through_added_context() {
        let error = docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
        )
        .context("Failed to start container 'airlock-q9183x1a-target'");

        assert!(is_transient_hns_error(&error));
    }

    #[test]
    fn other_failures_are_not_treated_as_transient_hns_errors() {
        // A different HNS failure mode: real breakage, not a collision worth papering over.
        assert!(!is_transient_hns_error(&docker_server_error(
            500,
            "failed during hnsCallRawResponse: hnsCall failed in Win32: The system cannot find the file specified. (0x2)",
        )));

        // A name conflict of ours, which a retry would never clear.
        assert!(!is_transient_hns_error(&docker_server_error(
            409,
            "Conflict. The container name \"/airlock-q9183x1a-target\" is already in use",
        )));

        assert!(!is_transient_hns_error(&generic_error!("something else went wrong")));
    }

    #[test]
    fn transient_error_retry_backoff_grows_per_attempt_and_stays_within_bounds() {
        for attempt in 1..TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS {
            let backoff = transient_error_retry_backoff(attempt, "q9183x1a");

            assert!(backoff >= TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt);
            assert!(backoff <= TRANSIENT_ERROR_RETRY_BASE_BACKOFF * attempt + TRANSIENT_ERROR_RETRY_MAX_JITTER);
        }
    }

    #[test]
    fn transient_error_retry_backoff_differs_between_isolation_groups() {
        // Racers that collided at the same instant must not retry on the same schedule, or they
        // just collide again. Isolation group IDs are taken from real failing runs.
        assert_ne!(
            transient_error_retry_backoff(1, "q9183x1a"),
            transient_error_retry_backoff(1, "fajekrvm")
        );
    }

    #[tokio::test]
    async fn with_transient_error_retry_retries_transient_failures_until_one_succeeds() {
        let attempts = std::cell::Cell::new(0);

        let result = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            if attempts.get() < TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS {
                Err(docker_server_error(
                    500,
                    "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
                ))
            } else {
                Ok(attempts.get())
            }
        })
        .await;

        assert_eq!(result.unwrap(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
        assert_eq!(attempts.get(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn with_transient_error_retryy_gives_up_after_the_attempt_budget() {
        let attempts = std::cell::Cell::new(0);

        let result: Result<(), GenericError> = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            Err(docker_server_error(
                500,
                "failed during hnsCallRawResponse: hnsCall failed in Win32: The object already exists. (0x1392)",
            ))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), TRANSIENT_ERROR_RETRY_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn with_transient_error_retry_does_not_retry_unrelated_failures() {
        let attempts = std::cell::Cell::new(0);

        let result: Result<(), GenericError> = with_transient_error_retry("test operation", "q9183x1a", || async {
            attempts.set(attempts.get() + 1);
            Err(generic_error!("something else went wrong"))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }
}
