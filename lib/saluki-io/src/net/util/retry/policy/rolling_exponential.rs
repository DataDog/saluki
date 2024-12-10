use std::sync::{
    atomic::{
        AtomicU32,
        Ordering::{AcqRel, Relaxed},
    },
    Arc,
};

use tokio::time::{sleep, Sleep};
use tower::retry::Policy;
use tracing::debug;

use crate::net::util::retry::{
    classifier::RetryClassifier,
    lifecycle::{DefaultDebugRetryLifecycle, RetryLifecycle},
    ExponentialBackoff,
};

/// A rolling exponential backoff retry policy.
///
/// This policy applies an exponential backoff strategy to requests that are classified as needing to be retried, and
/// maintains a memory of how many errors have occurred prior in order to potentially alter the backoff behavior of
/// retried requests after a successful response has been received.
///
/// ## Rolling backoff behavior (recovery error decrease factor)
///
/// As responses are classified, the number of errors seen (any failed request constitutes an error) is tracked
/// internally, which is then used to drive the exponential backoff behavior. When a request is finally successful,
/// there are two options: reset the error count back to zero, or decrease it by some fixed amount.
///
/// If the recovery error decrease factor isn't set at all, then the error count is reset back to zero after any
/// successful response. This means that if our next request fails, the backoff duration would start back at a low
/// value. If the recovery error decrease factor is set, however, then the error count is only decreased by that fixed
/// amount after a successful response, which means that if our next request fails, the calculated backoff duration
/// would still be reasonably close to the last calculated backoff duration.
///
/// Essentially, setting a recovery error decrease factor allows the calculated backoff duration to increase/decrease
/// more smoothly between failed requests that occur close together.
///
/// # Missing
///
/// - Ability to set an upper bound on retry attempts before giving up.
#[derive(Clone)]
pub struct RollingExponentialBackoffRetryPolicy<C, L = DefaultDebugRetryLifecycle> {
    classifier: C,
    retry_lifecycle: L,
    backoff: ExponentialBackoff,
    recovery_error_decrease_factor: Option<u32>,
    error_count: Arc<AtomicU32>,
}

impl<C> RollingExponentialBackoffRetryPolicy<C> {
    /// Creates a new `RollingExponentialBackoffRetryPolicy` with the given classifier and exponential backoff strategy.
    ///
    /// On successful responses, the error count will be reset back to zero.
    pub fn new(classifier: C, backoff: ExponentialBackoff) -> Self {
        Self {
            classifier,
            retry_lifecycle: DefaultDebugRetryLifecycle,
            backoff,
            recovery_error_decrease_factor: None,
            error_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl<C, L> RollingExponentialBackoffRetryPolicy<C, L> {
    /// Sets the recovery error decrease factor for this policy.
    ///
    /// The given value controls how much the error count should be decreased by after a successful response. If the
    /// value is `None`, then the error count will be reset back to zero after a successful response.
    ///
    /// Defaults to resetting the error count to zero after a successful response.
    pub fn with_recovery_error_decrease_factor(mut self, factor: Option<u32>) -> Self {
        self.recovery_error_decrease_factor = factor;
        self
    }

    /// Sets the retry lifecycle for this policy.
    ///
    /// `RetryLifecycle` allows defining custom hooks that are called at various points within the retry policy, such as
    /// when a request is classified as needing to be retried, when it succeeds, and so on. This can be used to add
    /// customized and contextual logging to retries.
    pub fn with_retry_lifecycle<L2>(self, retry_lifecycle: L2) -> RollingExponentialBackoffRetryPolicy<C, L2> {
        RollingExponentialBackoffRetryPolicy {
            classifier: self.classifier,
            retry_lifecycle,
            backoff: self.backoff,
            recovery_error_decrease_factor: self.recovery_error_decrease_factor,
            error_count: self.error_count,
        }
    }
}

impl<C, L, Req, Res, Error> Policy<Req, Res, Error> for RollingExponentialBackoffRetryPolicy<C, L>
where
    C: RetryClassifier<Res, Error>,
    L: RetryLifecycle<Req, Res, Error>,
    Req: Clone,
{
    type Future = Sleep;

    fn retry(&mut self, request: &mut Req, response: &mut Result<Res, Error>) -> Option<Self::Future> {
        if self.classifier.should_retry(response) {
            // We got an error response, so update our error count and figure out how long to backoff.
            let error_count = self.error_count.fetch_add(1, Relaxed) + 1;
            let backoff_dur = self.backoff.get_backoff_duration(error_count);

            self.retry_lifecycle
                .before_retry(request, response, backoff_dur, error_count);

            Some(sleep(backoff_dur))
        } else {
            self.retry_lifecycle.after_success(request, response);

            // We got a successful response, so update our error count if necessary.
            match self.recovery_error_decrease_factor {
                Some(factor) => {
                    debug!(decrease_factor = factor, "Decreasing error after successful response.");

                    // We never expect this to fail since we never conditionally try to update: we _always_ want to
                    // decrease the error count.
                    let _ = self
                        .error_count
                        .fetch_update(AcqRel, Relaxed, |count| Some(count.saturating_sub(factor)));
                }
                None => {
                    debug!("Resetting error count to zero after successful response.");

                    self.error_count.store(0, Relaxed);
                }
            }

            None
        }
    }

    fn clone_request(&mut self, req: &Req) -> Option<Req> {
        Some(req.clone())
    }
}
