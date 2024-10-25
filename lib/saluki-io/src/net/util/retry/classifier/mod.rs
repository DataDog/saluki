mod http;
pub use self::http::StandardHttpClassifier;

/// Determines whether or not a request should be retried.
///
/// This trait is closely related to [`tower::retry::Policy`], but allows us to decouple the logic of how to classify
/// whether or not a request should be retried from the logic of determining how long to wait before retrying.
pub trait RetryClassifier<Res, Error> {
    /// Returns `true` if the original request should be retried.
    fn should_retry(&self, response: &Result<Res, Error>) -> bool;
}
