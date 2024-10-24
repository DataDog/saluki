use std::time::Duration;

use tracing::debug;

mod http;
pub use self::http::StandardHttpRetryLifecycle;

pub trait RetryLifecycle<Req, Res, Error> {
    fn before_retry(&self, req: &Req, res: &Result<Res, Error>, retry_backoff: Duration, error_count: u32);
    fn after_success(&self, req: &Req, res: &Result<Res, Error>);
}

/// A default retry lifecycle that emits basic debug logs when retrying requests and when requests succeed.
///
/// This lifecycle emits an extremely minimal amount of information, and is generally only useful for debugging purposes
/// to understand if/when retries are happening, but gives no information about the request/response, why a retry
/// decision was made, the code that is ultimately using this retry logic, and so on.
#[derive(Clone)]
pub struct DefaultDebugRetryLifecycle;

impl<Req, Res, Error> RetryLifecycle<Req, Res, Error> for DefaultDebugRetryLifecycle {
    fn before_retry(&self, _: &Req, _: &Result<Res, Error>, retry_backoff: Duration, error_count: u32) {
        debug!(error_count, "Retrying request after {:?}.", retry_backoff);
    }

    fn after_success(&self, _: &Req, _: &Result<Res, Error>) {
        debug!("Request succeeded.");
    }
}
