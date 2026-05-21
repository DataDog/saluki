use std::sync::Arc;

use http::Response;

use super::RetryClassifier;

/// A predicate that decides whether a response should be treated as retriable.
///
/// The predicate receives the response and returns `true` if the response should be retried.
pub type HttpRetryPredicate<B = ()> = Arc<dyn Fn(&Response<B>) -> bool + Send + Sync>;

fn default_should_retry<B>(response: &Response<B>) -> bool {
    let status = response.status();

    match status {
        // There are some status codes that likely indicate a fundamental misconfiguration or bug on the client side
        // which won't be resolved by retrying the request.
        http::StatusCode::BAD_REQUEST
        | http::StatusCode::UNAUTHORIZED
        | http::StatusCode::FORBIDDEN
        | http::StatusCode::PAYLOAD_TOO_LARGE => false,

        // For all other status codes, we'll only retry if they're in the client/server error range.
        _ => status.is_client_error() || status.is_server_error(),
    }
}

/// A standard HTTP response classifier.
///
/// Generally treats all client (4xx) and server (5xx) errors as retriable, with the exception of a few specific client
/// errors that shouldn't be retried:
///
/// - 400 Bad Request (likely a client-side bug)
/// - 401 Unauthorized (likely a client-side misconfiguration)
/// - 403 Forbidden (likely a client-side misconfiguration)
/// - 413 Payload Too Large (likely a client-side bug)
///
/// Additional [`HttpRetryPredicate`]s can be registered via [`StandardHttpClassifier::with_predicate`]. A response is
/// retried if any predicate — including the default — returns `true` (OR semantics). This allows callers to
/// selectively unlock retries for status codes that the default predicate would not retry, without affecting other
/// status codes.
pub struct StandardHttpClassifier<B = ()> {
    predicates: Vec<HttpRetryPredicate<B>>,
}

impl<B> Clone for StandardHttpClassifier<B> {
    fn clone(&self) -> Self {
        Self {
            predicates: self.predicates.clone(),
        }
    }
}

impl<B: 'static> Default for StandardHttpClassifier<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: 'static> StandardHttpClassifier<B> {
    /// Creates a new [`StandardHttpClassifier`] with the default status-code predicate installed.
    pub fn new() -> Self {
        Self {
            predicates: vec![Arc::new(default_should_retry::<B>)],
        }
    }

    /// Adds a predicate.
    ///
    /// A response is retried if any predicate — including the default — returns `true` (OR semantics).
    pub fn with_predicate(mut self, predicate: HttpRetryPredicate<B>) -> Self {
        self.predicates.push(predicate);
        self
    }
}

impl<B, Error> RetryClassifier<http::Response<B>, Error> for StandardHttpClassifier<B> {
    fn should_retry(&self, response: &Result<http::Response<B>, Error>) -> bool {
        match response {
            Ok(resp) => self.predicates.iter().any(|p| p(resp)),
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use http::StatusCode;

    use super::*;

    type TestResponse = Result<http::Response<()>, ()>;

    fn ok(status: StatusCode) -> TestResponse {
        Ok(http::Response::builder().status(status).body(()).unwrap())
    }

    fn err() -> TestResponse {
        Err(())
    }

    fn classify(classifier: &StandardHttpClassifier<()>, response: &TestResponse) -> bool {
        <StandardHttpClassifier<()> as RetryClassifier<http::Response<()>, ()>>::should_retry(classifier, response)
    }

    #[test]
    fn default_classifier_retries_5xx_and_most_4xx() {
        let classifier = StandardHttpClassifier::new();

        assert!(!classify(&classifier, &ok(StatusCode::OK)));
        assert!(!classify(&classifier, &ok(StatusCode::NO_CONTENT)));

        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(classify(&classifier, &ok(status)), "{} should be retried", status);
        }

        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::NOT_FOUND,
        ] {
            assert!(classify(&classifier, &ok(status)), "{} should be retried", status);
        }
    }

    #[test]
    fn default_classifier_does_not_retry_known_client_misconfig() {
        let classifier = StandardHttpClassifier::new();

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::PAYLOAD_TOO_LARGE,
        ] {
            assert!(!classify(&classifier, &ok(status)), "{} should not be retried", status);
        }
    }

    #[test]
    fn default_classifier_retries_transport_error() {
        let classifier = StandardHttpClassifier::new();
        assert!(classify(&classifier, &err()));
    }

    #[test]
    fn predicate_adds_retry_for_403() {
        let classifier = StandardHttpClassifier::new()
            .with_predicate(Arc::new(|response| response.status() == StatusCode::FORBIDDEN));

        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));
        // Sibling client-misconfig statuses without a matching predicate keep their default (non-retriable) behavior.
        assert!(!classify(&classifier, &ok(StatusCode::UNAUTHORIZED)));
        assert!(!classify(&classifier, &ok(StatusCode::BAD_REQUEST)));
        // Status codes that are retried by default are unaffected.
        assert!(classify(&classifier, &ok(StatusCode::INTERNAL_SERVER_ERROR)));
    }

    #[test]
    fn predicate_is_re_evaluated_each_call() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);
        let predicate: HttpRetryPredicate =
            Arc::new(move |response| response.status() == StatusCode::FORBIDDEN && flag_clone.load(Ordering::SeqCst));

        let classifier = StandardHttpClassifier::new().with_predicate(predicate);

        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        flag.store(true, Ordering::SeqCst);
        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        flag.store(false, Ordering::SeqCst);
        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));
    }
}
