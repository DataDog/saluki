use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::{thread_rng, Rng as _, RngCore};

#[derive(Clone)]
pub enum BackoffRng {
    /// A lazily-initialized, thread-local CSPRNG seeded by the operating system.
    ///
    /// Provided by [`rand::ThreadRng`][rand_threadrng].
    ///
    /// [rand_threadrng]: https://docs.rs/rand/latest/rand/rngs/struct.ThreadRng.html
    SecureDefault,

    /// A shared random number generator.
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

/// An exponential backoff strategy.
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
    /// behind a mutex, allowing it to be cloned, so care should be taken to never use this outside of tests.
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
