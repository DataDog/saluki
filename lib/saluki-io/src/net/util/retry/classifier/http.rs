use std::{collections::HashMap, sync::Arc};

use http::StatusCode;

use super::RetryClassifier;

/// A predicate that decides whether a response with a particular [`StatusCode`] should be treated as retriable.
///
/// The predicate is invoked at classification time, on every response that matches its status code, allowing the
/// decision to be re-evaluated dynamically (for example, against runtime-updated configuration).
pub type StatusCodeRetryPredicate = Arc<dyn Fn() -> bool + Send + Sync>;

/// A standard HTTP response classifier.
///
/// Generally treats all client (4xx) and server (5xx) errors as retriable, with the exception of a few specific client
/// errors that should not be retried:
///
/// - 400 Bad Request (likely a client-side bug)
/// - 401 Unauthorized (likely a client-side misconfiguration)
/// - 403 Forbidden (likely a client-side misconfiguration)
/// - 413 Payload Too Large (likely a client-side bug)
///
/// The default classification for any [`StatusCode`] can be overridden by registering a [`StatusCodeRetryPredicate`]
/// for that status via [`StandardHttpClassifier::with_status_code_predicate`] (or
/// [`StandardHttpClassifier::set_status_code_predicate`]). When a response is received whose status code has a
/// predicate registered, the predicate is consulted and its return value is used as the retriability decision,
/// overriding the default behavior described above.
#[derive(Clone, Default)]
pub struct StandardHttpClassifier {
    status_code_predicates: HashMap<StatusCode, StatusCodeRetryPredicate>,
}

impl StandardHttpClassifier {
    /// Creates a new [`StandardHttpClassifier`] with no per-status-code predicates installed.
    pub fn new() -> Self {
        Self {
            status_code_predicates: HashMap::new(),
        }
    }

    /// Builder-style: registers `predicate` as the retriability override for `status`.
    ///
    /// Replaces any previously registered predicate for the same status code. See
    /// [`StandardHttpClassifier::set_status_code_predicate`] for the in-place equivalent.
    pub fn with_status_code_predicate(mut self, status: StatusCode, predicate: StatusCodeRetryPredicate) -> Self {
        self.set_status_code_predicate(status, predicate);
        self
    }

    /// Registers `predicate` as the retriability override for `status`.
    ///
    /// Replaces any previously registered predicate for the same status code.
    pub fn set_status_code_predicate(&mut self, status: StatusCode, predicate: StatusCodeRetryPredicate) {
        self.status_code_predicates.insert(status, predicate);
    }

    /// Removes any predicate previously registered for `status`.
    ///
    /// After removal, responses with `status` revert to the classifier's default behavior. If no predicate was
    /// registered for `status`, this is a no-op.
    pub fn remove_status_code_predicate(&mut self, status: StatusCode) {
        self.status_code_predicates.remove(&status);
    }
}

impl<B, Error> RetryClassifier<http::Response<B>, Error> for StandardHttpClassifier {
    fn should_retry(&self, response: &Result<http::Response<B>, Error>) -> bool {
        match response {
            Ok(resp) => {
                let status = resp.status();

                // If a per-status-code predicate is installed, it takes precedence over the default classification.
                if let Some(predicate) = self.status_code_predicates.get(&status) {
                    return predicate();
                }

                match status {
                    // There's some status codes that likely indicate a fundamental misconfiguration or bug on the
                    // client side which won't be resolved by retrying the request.
                    StatusCode::BAD_REQUEST
                    | StatusCode::UNAUTHORIZED
                    | StatusCode::FORBIDDEN
                    | StatusCode::PAYLOAD_TOO_LARGE => false,

                    // For all other status codes, we'll only retry if they're in the client/server error range.
                    status => status.is_client_error() || status.is_server_error(),
                }
            }
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    type TestResponse = Result<http::Response<()>, ()>;

    fn ok(status: StatusCode) -> TestResponse {
        Ok(http::Response::builder().status(status).body(()).unwrap())
    }

    fn err() -> TestResponse {
        Err(())
    }

    fn classify(classifier: &StandardHttpClassifier, response: &TestResponse) -> bool {
        <StandardHttpClassifier as RetryClassifier<http::Response<()>, ()>>::should_retry(classifier, response)
    }

    fn always(value: bool) -> StatusCodeRetryPredicate {
        Arc::new(move || value)
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
    fn predicate_overrides_default_for_403_when_true() {
        let classifier = StandardHttpClassifier::new().with_status_code_predicate(StatusCode::FORBIDDEN, always(true));

        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));
        // Sibling client-misconfig statuses without a predicate keep their default (non-retriable) behavior.
        assert!(!classify(&classifier, &ok(StatusCode::UNAUTHORIZED)));
        assert!(!classify(&classifier, &ok(StatusCode::BAD_REQUEST)));
    }

    #[test]
    fn predicate_overrides_default_for_403_when_false() {
        // A `false` predicate on 403 matches the default, so to *prove* the predicate was consulted we also install a
        // `false` predicate on 500 (which would otherwise be retried) and assert it flips.
        let classifier = StandardHttpClassifier::new()
            .with_status_code_predicate(StatusCode::FORBIDDEN, always(false))
            .with_status_code_predicate(StatusCode::INTERNAL_SERVER_ERROR, always(false));

        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));
        assert!(!classify(&classifier, &ok(StatusCode::INTERNAL_SERVER_ERROR)));
    }

    #[test]
    fn predicate_is_re_evaluated_each_call() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);
        let predicate: StatusCodeRetryPredicate = Arc::new(move || flag_clone.load(Ordering::SeqCst));

        let classifier = StandardHttpClassifier::new().with_status_code_predicate(StatusCode::FORBIDDEN, predicate);

        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        flag.store(true, Ordering::SeqCst);
        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        flag.store(false, Ordering::SeqCst);
        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));
    }

    #[test]
    fn remove_status_code_predicate_restores_default() {
        let mut classifier =
            StandardHttpClassifier::new().with_status_code_predicate(StatusCode::FORBIDDEN, always(true));
        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        classifier.remove_status_code_predicate(StatusCode::FORBIDDEN);
        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        // Removing a predicate that was never installed is a no-op.
        classifier.remove_status_code_predicate(StatusCode::IM_A_TEAPOT);
        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));
    }

    #[test]
    fn set_status_code_predicate_replaces_existing() {
        let mut classifier =
            StandardHttpClassifier::new().with_status_code_predicate(StatusCode::FORBIDDEN, always(true));
        assert!(classify(&classifier, &ok(StatusCode::FORBIDDEN)));

        classifier.set_status_code_predicate(StatusCode::FORBIDDEN, always(false));
        assert!(!classify(&classifier, &ok(StatusCode::FORBIDDEN)));
    }
}
