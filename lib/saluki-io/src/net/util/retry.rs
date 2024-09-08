use std::{
    fmt,
    sync::{
        atomic::{
            AtomicU32,
            Ordering::{AcqRel, Relaxed},
        },
        Arc, Mutex,
    },
    time::Duration,
};

use http::StatusCode;
use rand::{thread_rng, Rng as _, RngCore};
use tokio::time::{sleep, Sleep};
use tower::retry::Policy;
use tracing::debug;

#[derive(Clone)]
pub enum BackoffRng {
    /// A lazily-initialized, thread-local CSPRNG seeded by the operating system.
    ///
    /// Provided by [`rand::ThreadRng`][rand_threadrng].
    ///
    /// [rand_threadrng]: https://docs.rs/rand/latest/rand/rngs/struct.ThreadRng.html
    SecureDefault,

    /// A shared random number generator.
    ///
    /// Useful for testing purposes, where the RNG must be overridden to add determinism. The RNG is shared atomically
    /// behind a mutex, allowing it to be clone, so care should be taken to never use this outside of tests.
    Shared(Arc<Mutex<Box<dyn RngCore + Send + Sync>>>),
}

impl fmt::Debug for BackoffRng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackoffRng::SecureDefault => f.debug_tuple("SecureDefault").finish(),
            BackoffRng::Shared(_) => f.debug_tuple("Shared").finish(),
        }
    }
}

impl RngCore for BackoffRng {
    fn next_u32(&mut self) -> u32 {
        match self {
            BackoffRng::SecureDefault => thread_rng().next_u32(),
            BackoffRng::Shared(rng) => rng.lock().unwrap().next_u32(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        match self {
            BackoffRng::SecureDefault => thread_rng().next_u64(),
            BackoffRng::Shared(rng) => rng.lock().unwrap().next_u64(),
        }
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        match self {
            BackoffRng::SecureDefault => thread_rng().fill_bytes(dest),
            BackoffRng::Shared(rng) => rng.lock().unwrap().fill_bytes(dest),
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        match self {
            BackoffRng::SecureDefault => thread_rng().try_fill_bytes(dest),
            BackoffRng::Shared(rng) => rng.lock().unwrap().try_fill_bytes(dest),
        }
    }
}

/// An exponential backoff strategy
///
/// This backoff strategy provides backoff durations that increase exponentially based on a user-provided error count,
/// with a minimum and maximum bound on the duration. Additionally, jitter can be added to the backoff duration in order
/// to help avoiding multiple callers retrying their requests at the same time.
#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    min_backoff: Duration,
    max_backoff: Duration,
    min_backoff_factor: f64,
    rng: BackoffRng,
}

impl ExponentialBackoff {
    /// Creates a new `ExponentialBackoff` with the given minimum and maximum backoff durations.
    ///
    /// Jitter is not applied to the calculated backoff durations.
    pub fn new(min_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            min_backoff,
            max_backoff,
            min_backoff_factor: 1.0,
            rng: BackoffRng::SecureDefault,
        }
    }

    /// Creates a new `ExponentialBackoff` with the given minimum and maximum backoff durations, and minimum backoff
    /// factor.
    ///
    /// Jitter is applied to the calculated backoff durations based on the minimum backoff factor, such that any given
    /// backoff duration will be between `D/min_backoff_factor` and `D`, where `D` is the calculated backoff duration
    /// for the given external error count. If the minimum backoff factor is set to 1.0 or less, then jitter will be
    /// disabled.
    ///
    /// Concretely, this means that with a minimum backoff duration of 10ms, and a minimum backoff factor of 2.0, the
    /// duration for an error count of one would be 20ms without jitter, but anywhere between 10ms and 20ms with jitter.
    /// For an error count of two, it be 40ms without jitter, but anywhere between 20ms and 40ms with jitter.
    pub fn with_jitter(min_backoff: Duration, max_backoff: Duration, min_backoff_factor: f64) -> Self {
        Self {
            min_backoff,
            max_backoff,
            min_backoff_factor: min_backoff_factor.max(1.0),
            rng: BackoffRng::SecureDefault,
        }
    }

    /// Sets the random number generator to use for calculating jittered backoff durations.
    ///
    /// Useful for testing purposes, where the RNG must be overridden to add determinism. The RNG is shared atomically
    /// behind a mutex, allowing it to be clone, so care should be taken to never use this outside of tests.
    ///
    /// Defaults to a lazily-initialized, thread-local CSPRNG seeded by the operating system.
    pub fn with_rng<R>(self, rng: R) -> Self
    where
        R: RngCore + Send + Sync + 'static,
    {
        ExponentialBackoff {
            min_backoff: self.min_backoff,
            max_backoff: self.max_backoff,
            min_backoff_factor: self.min_backoff_factor,
            rng: BackoffRng::Shared(Arc::new(Mutex::new(Box::new(rng)))),
        }
    }

    /// Calculates the backoff duration for the given error count.
    ///
    /// The error count value is generally user-defined, but should constitute the number of consecutive errors, or
    /// attempts, that have been made when retrying an operation or request.
    pub fn get_backoff_duration(&mut self, error_count: u32) -> Duration {
        if error_count == 0 {
            return self.min_backoff;
        }

        let mut backoff = self.min_backoff.saturating_mul(2u32.saturating_pow(error_count));

        // Apply jitter if necessary.
        if self.min_backoff_factor > 1.0 {
            let backoff_lower = backoff.div_f64(self.min_backoff_factor);
            let backoff_upper = backoff;
            backoff = self.rng.gen_range(backoff_lower..=backoff_upper)
        }

        backoff.clamp(self.min_backoff, self.max_backoff)
    }
}

/// Determines whether or not a request should be retried.
///
/// This trait is closely related to [`tower::retry::Policy`], but allows us to decouple the logic of how to classify
/// whether or not a request should be retried from the logic of determining how long to wait before retrying.
pub trait RetryClassifier<Res, Error> {
    /// Returns `true` if the original request should be retried.
    fn should_retry(&self, response: &Result<Res, Error>) -> bool;
}

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
/// ## Missing
///
/// - Ability to set an upper bound on retry attempts before giving up.
#[derive(Clone)]
pub struct RollingExponentialBackoffRetryPolicy<C> {
    classifier: C,
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
            backoff,
            recovery_error_decrease_factor: None,
            error_count: Arc::new(AtomicU32::new(0)),
        }
    }

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
}

impl<C, Req, Res, Error> Policy<Req, Res, Error> for RollingExponentialBackoffRetryPolicy<C>
where
    C: RetryClassifier<Res, Error>,
    Req: Clone,
{
    type Future = Sleep;

    fn retry(&mut self, _request: &mut Req, response: &mut Result<Res, Error>) -> Option<Self::Future> {
        if self.classifier.should_retry(response) {
            // We got an error response, so update our error count and figure out how long to backoff.
            let error_count = self.error_count.fetch_add(1, Relaxed) + 1;
            let backoff_dur = self.backoff.get_backoff_duration(error_count);

            debug!(error_count, ?backoff_dur, "Retrying request with backoff.");

            Some(sleep(backoff_dur))
        } else {
            debug!(error_count = self.error_count.load(Relaxed), "Request successful.");

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

/// A standard HTTP response classifier.
///
/// Generally treats all client (4xx) and server (5xx) errors as retryable, with the exception of a few specific client
/// errors that should not be retried:
///
/// - 400 Bad Request (likely a client-side bug)
/// - 401 Unauthorized (likely a client-side misconfiguration)
/// - 403 Forbidden (likely a client-side misconfiguration)
/// - 413 Payload Too Large (likely a client-side bug)
#[derive(Clone)]
pub struct StandardHttpClassifier;

impl<B, Error> RetryClassifier<http::Response<B>, Error> for StandardHttpClassifier {
    fn should_retry(&self, response: &Result<http::Response<B>, Error>) -> bool {
        match response {
            Ok(resp) => match resp.status() {
                // There's some status codes that likely indicate a fundamental misconfiguration or bug on the client
                // side which won't be resolved by retrying the request.
                StatusCode::BAD_REQUEST
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::PAYLOAD_TOO_LARGE => false,

                // For all other status codes, we'll only retry if they're in the client/server error range.
                status => status.is_client_error() || status.is_server_error(),
            },
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use crate::net::util::retry::ExponentialBackoff;

    fn arb_exponential_backoff(min_backoff_factor: f64) -> impl Strategy<Value = ExponentialBackoff> {
        (1u64..=u64::MAX, 1u64..u64::MAX)
            .prop_map(move |(min_backoff, max_backoff)| {
                let max_backoff = min_backoff.saturating_add(max_backoff);
                ExponentialBackoff::with_jitter(
                    Duration::from_nanos(min_backoff),
                    Duration::from_nanos(max_backoff),
                    min_backoff_factor,
                )
            })
            .prop_perturb(|backoff, rng| backoff.with_rng(rng))
    }

    proptest! {
        #[test]
        fn property_test_exponential_backoff_no_jitter(
            mut backoff in arb_exponential_backoff(1.0),
            error_count in 0..u32::MAX,
            error_count_increase in 1..5u32
        ) {
            // The goal of this test is to show that for some arbitrary error count, the calculated backoff duration we
            // get is always less than or equal to the calculated backoff duration for an error count that is _larger_.
            let first = backoff.get_backoff_duration(error_count);
            let first_followup = backoff.get_backoff_duration(error_count);
            let second = backoff.get_backoff_duration(error_count.saturating_add(error_count_increase));
            let second_followup = backoff.get_backoff_duration(error_count.saturating_add(error_count_increase));

            assert_eq!(first, first_followup);
            assert_eq!(second, second_followup);
            assert!(first <= second);
            assert!(first >= backoff.min_backoff);
            assert!(first <= backoff.max_backoff);
            assert!(second >= backoff.min_backoff);
            assert!(second <= backoff.max_backoff);
        }

        #[test]
        fn property_test_exponential_backoff_default_jitter(
            mut backoff in arb_exponential_backoff(2.0),
            error_count in 0..u32::MAX,
            error_count_increase in 1..5u32
        ) {
            // The goal of this test is to show that for some arbitrary error count, the calculated backoff duration we
            // get is always less than or equal to the calculated backoff duration for an error count that is _larger_.
            let first = backoff.get_backoff_duration(error_count);
            let second = backoff.get_backoff_duration(error_count.saturating_add(error_count_increase));

            assert!(first <= second);
            assert!(first >= backoff.min_backoff);
            assert!(first <= backoff.max_backoff);
            assert!(second >= backoff.min_backoff);
            assert!(second <= backoff.max_backoff);
        }
    }
}
