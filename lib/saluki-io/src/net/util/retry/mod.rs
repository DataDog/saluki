mod backoff;
pub use self::backoff::ExponentialBackoff;

mod classifier;
pub use self::classifier::{RetryClassifier, StandardHttpClassifier, StatusCodeRetryPredicate};

mod lifecycle;
pub use self::lifecycle::StandardHttpRetryLifecycle;

mod policy;
pub use self::policy::{NoopRetryPolicy, RollingExponentialBackoffRetryPolicy};

mod queue;
pub use self::queue::{DiskUsageRetrieverImpl, EventContainer, PushResult, RetryQueue, Retryable};

/// A batteries-included retry policy suitable for HTTP-based clients.
pub type DefaultHttpRetryPolicy =
    RollingExponentialBackoffRetryPolicy<StandardHttpClassifier, StandardHttpRetryLifecycle>;

impl DefaultHttpRetryPolicy {
    /// Creates a new retry policy adapted to HTTP-based clients with the given exponential backoff strategy.
    ///
    /// This policy uses the standard HTTP classifier ([`StandardHttpClassifier`]) and retry lifecycle ([`StandardHttpRetryLifecycle`]).
    pub fn with_backoff(backoff: ExponentialBackoff) -> Self {
        Self::with_backoff_and_classifier(backoff, StandardHttpClassifier::new())
    }

    /// Creates a new retry policy adapted to HTTP-based clients with the given exponential backoff strategy and a
    /// pre-built [`StandardHttpClassifier`].
    ///
    /// This is the same as [`DefaultHttpRetryPolicy::with_backoff`], but allows the caller to supply a classifier that
    /// has been customized (for example, with a [`StatusCodeRetryPredicate`]).
    pub fn with_backoff_and_classifier(backoff: ExponentialBackoff, classifier: StandardHttpClassifier) -> Self {
        RollingExponentialBackoffRetryPolicy::new(classifier, backoff).with_retry_lifecycle(StandardHttpRetryLifecycle)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use http::{Request, Response, StatusCode};
    use tower::retry::Policy;

    use super::*;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    type TestRequest = Request<()>;

    fn test_backoff() -> ExponentialBackoff {
        ExponentialBackoff::with_jitter(Duration::from_millis(1), Duration::from_millis(10), 2.0)
    }

    fn test_request() -> TestRequest {
        Request::builder()
            .method("POST")
            .uri("http://localhost/intake")
            .body(())
            .unwrap()
    }

    fn ok_response(status: StatusCode) -> Result<Response<()>, BoxError> {
        Ok(Response::builder().status(status).body(()).unwrap())
    }

    fn would_retry(policy: &mut DefaultHttpRetryPolicy, status: StatusCode) -> bool {
        let mut request = test_request();
        let mut response = ok_response(status);
        Policy::<TestRequest, Response<()>, BoxError>::retry(policy, &mut request, &mut response).is_some()
    }

    #[tokio::test]
    async fn default_http_retry_policy_with_backoff_uses_default_classifier() {
        let mut policy = DefaultHttpRetryPolicy::with_backoff(test_backoff());

        assert!(!would_retry(&mut policy, StatusCode::OK));
        assert!(!would_retry(&mut policy, StatusCode::FORBIDDEN));
        assert!(!would_retry(&mut policy, StatusCode::BAD_REQUEST));
        assert!(would_retry(&mut policy, StatusCode::INTERNAL_SERVER_ERROR));
        assert!(would_retry(&mut policy, StatusCode::TOO_MANY_REQUESTS));
    }

    #[tokio::test]
    async fn default_http_retry_policy_with_backoff_and_classifier_threads_predicate() {
        // Build a classifier that flips 403 to retriable, then ensure the constructed policy honors it.
        let predicate: StatusCodeRetryPredicate = Arc::new(|| true);
        let classifier = StandardHttpClassifier::new().with_status_code_predicate(StatusCode::FORBIDDEN, predicate);
        let mut policy = DefaultHttpRetryPolicy::with_backoff_and_classifier(test_backoff(), classifier);

        assert!(would_retry(&mut policy, StatusCode::FORBIDDEN));
        // Other status codes still follow default classifier behavior.
        assert!(!would_retry(&mut policy, StatusCode::OK));
        assert!(!would_retry(&mut policy, StatusCode::UNAUTHORIZED));
        assert!(would_retry(&mut policy, StatusCode::INTERNAL_SERVER_ERROR));
    }
}
