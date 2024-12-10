pub mod agent;

mod backoff;
pub use self::backoff::ExponentialBackoff;

mod classifier;
pub use self::classifier::StandardHttpClassifier;

mod lifecycle;
pub use self::lifecycle::StandardHttpRetryLifecycle;

mod policy;
pub use self::policy::{NoopRetryPolicy, RollingExponentialBackoffRetryPolicy};

/// A batteries-included retry policy suitable for HTTP-based clients.
pub type DefaultHttpRetryPolicy =
    RollingExponentialBackoffRetryPolicy<StandardHttpClassifier, StandardHttpRetryLifecycle>;

impl DefaultHttpRetryPolicy {
    /// Creates a new retry policy adapted to HTTP-based clients with the given exponential backoff strategy.
    ///
    /// This policy uses the standard HTTP classifier ([`StandardHttpRClassifier`]) and retry lifecycle ([`StandardHttpRetryLifecycle`]).
    pub fn with_backoff(backoff: ExponentialBackoff) -> Self {
        RollingExponentialBackoffRetryPolicy::new(StandardHttpClassifier, backoff)
            .with_retry_lifecycle(StandardHttpRetryLifecycle)
    }
}
