use std::future::Ready;

use tower::retry::Policy;

mod rolling_exponential;
pub use self::rolling_exponential::RollingExponentialBackoffRetryPolicy;

/// A no-op retry policy that never retries requests.
#[derive(Clone, Debug)]
pub struct NoopRetryPolicy;

impl<Req, Res, Error> Policy<Req, Res, Error> for NoopRetryPolicy {
    type Future = Ready<()>;

    fn retry(&mut self, _: &mut Req, _: &mut Result<Res, Error>) -> Option<Self::Future> {
        None
    }

    fn clone_request(&mut self, _: &Req) -> Option<Req> {
        None
    }
}
