//! Shared test-support helpers for `saluki-core`'s own unit and integration tests.
//!
//! This module is `#[cfg(test)]`-only and exists so that the runtime (supervision) and topology tests can share a
//! single readiness-polling barrier instead of hand-rolling bespoke poll loops or reaching for blind, fixed-duration
//! `sleep`s as startup/shutdown barriers.

use std::time::Duration;

/// Default overall time budget before [`wait_until`] gives up and panics.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between successive evaluations of the condition.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Polls `condition` until it returns `true`, or panics once the default deadline elapses.
///
/// This is the canonical readiness barrier for the crate's async tests. Prefer it over a blind `sleep` when a test
/// needs to wait for a startup or shutdown condition (a supervisor becoming running, a worker recording that it
/// started, a child count draining to zero, and so on): it re-checks the condition every few milliseconds and returns
/// as soon as it holds, which is both faster and far less flaky than sleeping for a fixed duration and hoping the work
/// finished.
///
/// `description` is folded into the panic message so a timeout points precisely at what never became true.
///
/// # Panics
///
/// Panics if `condition` does not return `true` within the default timeout ([`DEFAULT_TIMEOUT`]).
pub(crate) async fn wait_until(description: &str, condition: impl FnMut() -> bool) {
    wait_until_within(DEFAULT_TIMEOUT, description, condition).await;
}

/// Like [`wait_until`], but with a caller-specified overall time budget.
///
/// # Panics
///
/// Panics if `condition` does not return `true` within `timeout`.
pub(crate) async fn wait_until_within(timeout: Duration, description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting until {description}");
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
