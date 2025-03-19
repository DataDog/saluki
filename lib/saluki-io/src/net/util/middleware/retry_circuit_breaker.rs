use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use futures::FutureExt as _;
use pin_project_lite::pin_project;
use tower::{retry::Policy, Service};

/// An error from [`RetryCircuitBreaker`].
#[derive(Debug)]
pub enum Error<E, R> {
    /// The inner service responded with an error.
    Service(E),

    /// The circuit breaker is open and requests are being rejected.
    Open(R),
}

impl<E, R> PartialEq for Error<E, R>
where
    E: PartialEq,
    R: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Service(a), Self::Service(b)) => a == b,
            (Self::Open(a), Self::Open(b)) => a == b,
            _ => false,
        }
    }
}

pin_project! {
    /// Response future for [`RetryCircuitBreaker`].
    pub struct ResponseFuture<P, F, Request> {
        state: Arc<Mutex<State<P>>>,
        #[pin]
        inner: Option<F>,
        req: Option<Request>,
    }
}

impl<P, F, T, E, Request> Future for ResponseFuture<P, F, Request>
where
    P: Policy<Request, T, E>,
    P::Future: Send + 'static,
    F: Future<Output = Result<T, E>>,
{
    type Output = Result<T, Error<E, Request>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Our response future exists in two states: the circuit breaker was either closed or open when we created it.
        //
        // When the circuit breaker is open while creating the response future, there's no actual response future to
        // call in this case. We simply store the original request and pass it back while indicating to the caller that
        // the circuit breaker is open. Simple.
        //
        // When the circuit breaker is closed while creating the response future, this means we can proceed, and we
        // generate a legitimate response future to poll. However, the retry policy may return `None` when trying to
        // clone the request, which indicates the request actually isn't eligible to be retried at all. Thus, when we
        // don't have an original request here, we just return the inner service's response as-is. When we _do_ have the
        // original request, we utilize the retry policy to determine if it can be retried, and if so, potentially
        // update our circuit breaker state based on what the retry policy tells us.

        let this = self.project();
        if let Some(inner) = this.inner.as_pin_mut() {
            let mut result = ready!(inner.poll(cx));

            let mut state = this.state.lock().unwrap();
            match this.req.take() {
                Some(mut req) => match state.policy.retry(&mut req, &mut result) {
                    Some(backoff) => {
                        // The policy has indicated that the request should be retried, so we need to open the circuit
                        // breaker by setting the backoff future to use. Another request's retry decision may have
                        // already beat us to the punch, though, so don't overwrite it if it's already set.
                        if state.backoff.is_none() {
                            state.backoff = Some(backoff.boxed());
                        }

                        Poll::Ready(Err(Error::Open(req)))
                    }
                    None => Poll::Ready(result.map_err(Error::Service)),
                },
                None => Poll::Ready(result.map_err(Error::Service)),
            }
        } else {
            Poll::Ready(Err(Error::Open(
                this.req.take().expect("response future polled after completion"),
            )))
        }
    }
}

struct State<P> {
    policy: P,
    backoff: Option<Pin<Box<dyn Future<Output = ()>>>>,
}

impl<P> std::fmt::Debug for State<P>
where
    P: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backoff = if self.backoff.is_some() { "set" } else { "unset" };
        f.debug_struct("State")
            .field("policy", &self.policy)
            .field("backoff", &backoff)
            .finish()
    }
}

/// Wraps a service in a [circuit breaker][circuit_breaker] and signals when a request must be retried at a later time.
///
/// This circuit breaker implementation is specific to retrying requests. In many cases, a request can fail in two
/// ways: unrecoverable errors, which should not be retried, and recoverable errors, which should be retried after a
/// some period of time. When a request can be retried, it may not be advantageous to wait for the given request to
/// be retried successfully, as the request should perhaps be stored in a queue and retried at a later time,
/// potentially to avoid applying backpressure to the client.
///
/// [`RetryCircuitBreaker`] provides this capability by separating the logic of determining whether or not a request
/// should be retried from actually performing the retry itself. When a request leads to an unrecoverable error,
/// that error is immediately passed back to the caller without affecting the circuit breaker state. However, when a
/// recoverable error is encountered, the circuit breaker will signal to the caller that the request should be
/// retried, and update its internal state to open the circuit breaker for a configurable period of time. Further
/// requests to the circuit breaker will be rejected with an error (indicating the open state) until that period of
/// time has passed.
///
/// [circuit_breaker]: https://en.wikipedia.org/wiki/Circuit_breaker_design_pattern
#[derive(Debug)]
pub struct RetryCircuitBreaker<S, P> {
    inner: S,
    state: Arc<Mutex<State<P>>>,
}

impl<S, P> RetryCircuitBreaker<S, P> {
    /// Creates a new [`RetryCircuitBreaker`].
    pub fn new(inner: S, policy: P) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(State { policy, backoff: None })),
        }
    }
}

impl<S, P, Request> Service<Request> for RetryCircuitBreaker<S, P>
where
    S: Service<Request>,
    P: Policy<Request, S::Response, S::Error>,
    P::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error<S::Error, Request>;
    type Future = ResponseFuture<P, S::Future, Request>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        {
            // Check if we're currently in a backoff state.
            let mut state = self.state.lock().unwrap();
            if let Some(backoff) = state.backoff.as_mut() {
                ready!(backoff.as_mut().poll(cx));

                // The backoff future has completed, so we can reset the circuit breaker state.
                state.backoff = None;
            }
        }

        // Check the readiness of the inner service.
        self.inner.poll_ready(cx).map_err(Error::Service)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let response_state = Arc::clone(&self.state);

        let mut state = self.state.lock().unwrap();
        if state.backoff.is_some() {
            ResponseFuture {
                state: response_state,
                inner: None,
                req: Some(req),
            }
        } else {
            // The circuit breaker is closed, so we can proceed with the request.
            let cloned_req = state.policy.clone_request(&req);
            let inner = self.inner.call(req);

            ResponseFuture {
                state: response_state,
                inner: Some(inner),
                req: cloned_req,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{ready, Ready},
        time::Duration,
    };

    use tokio::time::Sleep;
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok};
    use tower::ServiceExt as _;

    use super::*;

    const BACKOFF_DUR: Duration = Duration::from_secs(1);

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum BasicRequest {
        Ok(String),
        Err(String),
    }

    impl BasicRequest {
        fn success<S: AsRef<str>>(value: S) -> Self {
            Self::Ok(value.as_ref().to_string())
        }

        fn failure<S: AsRef<str>>(value: S) -> Self {
            Self::Err(value.as_ref().to_string())
        }

        fn as_service_response(&self) -> Result<String, Error<String, Self>> {
            match self {
                Self::Ok(value) => Ok(value.clone()),
                Self::Err(value) => Err(Error::Service(value.clone())),
            }
        }

        fn as_open_response(&self) -> Result<String, Error<String, Self>> {
            Err(Error::Open(self.clone()))
        }
    }

    impl PartialEq<Result<String, String>> for BasicRequest {
        fn eq(&self, other: &Result<String, String>) -> bool {
            match self {
                Self::Ok(value) => other.as_ref() == Ok(value),
                Self::Err(value) => other.as_ref() == Err(value),
            }
        }
    }

    #[derive(Debug)]
    struct LoopbackService;

    impl Service<BasicRequest> for LoopbackService {
        type Response = String;
        type Error = String;
        type Future = Ready<Result<String, String>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: BasicRequest) -> Self::Future {
            let res = match req {
                BasicRequest::Ok(value) => Ok(value),
                BasicRequest::Err(value) => Err(value),
            };
            ready(res)
        }
    }

    #[derive(Debug)]
    struct CloneableTestRetryPolicy;

    impl<Req, T, E> Policy<Req, T, E> for CloneableTestRetryPolicy
    where
        Req: Clone,
    {
        type Future = Sleep;

        fn retry(&mut self, _: &mut Req, res: &mut Result<T, E>) -> Option<Self::Future> {
            match res {
                Ok(_) => None,
                Err(_) => Some(tokio::time::sleep(BACKOFF_DUR)),
            }
        }

        fn clone_request(&mut self, req: &Req) -> Option<Req> {
            Some(req.clone())
        }
    }

    #[derive(Debug)]
    struct NonCloneableTestRetryPolicy;

    impl<Req, T, E> Policy<Req, T, E> for NonCloneableTestRetryPolicy {
        type Future = Sleep;

        fn retry(&mut self, _: &mut Req, res: &mut Result<T, E>) -> Option<Self::Future> {
            match res {
                Ok(_) => None,
                Err(_) => Some(tokio::time::sleep(BACKOFF_DUR)),
            }
        }

        fn clone_request(&mut self, _: &Req) -> Option<Req> {
            None
        }
    }

    #[tokio::test(start_paused = true)]
    async fn basic() {
        let good_req = BasicRequest::success("good");
        let bad_req = BasicRequest::failure("bad");

        let mut circuit_breaker = RetryCircuitBreaker::new(LoopbackService, CloneableTestRetryPolicy);

        // First request should succeed.
        //
        // We should see that it called through to the inner service.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(good_req.clone());
        let result = fut.await;
        assert_eq!(result, good_req.as_service_response());

        // Second request should fail and should be retried.
        //
        // We should see that it called through to the inner service
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(bad_req.clone());
        let result = fut.await;
        assert_eq!(result, bad_req.as_open_response());

        // When trying to make our third request, we should have to wait for the backoff duration before the service
        // indicates that it's ready for another call.
        let mut svc_fut = tokio_test::task::spawn(circuit_breaker.ready());
        assert_pending!(svc_fut.poll());

        // Advance time past the backoff duration, which should make our service ready.
        tokio::time::advance(BACKOFF_DUR + Duration::from_millis(1)).await;
        assert!(svc_fut.is_woken());
        let svc = assert_ready_ok!(svc_fut.poll());

        let fut = svc.call(good_req.clone());
        let result = fut.await;
        assert_eq!(result, good_req.as_service_response());

        // Fourth request should succeed unimpeded since the breaker is closed again.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(good_req.clone());
        let result = fut.await;
        assert_eq!(result, good_req.as_service_response());
    }

    #[tokio::test]
    async fn retry_policy_no_clone() {
        let good_req = BasicRequest::success("good");
        let bad_req = BasicRequest::failure("bad");

        // First request should succeed.
        //
        // We should see that it called through to the inner service.
        let mut circuit_breaker = RetryCircuitBreaker::new(LoopbackService, NonCloneableTestRetryPolicy);
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(good_req.clone());
        let result = fut.await;
        assert_eq!(result, good_req.as_service_response());

        // Second request should fail and should be a service error, because without being able to clone the request, it
        // can't be retried anyways.
        let mut circuit_breaker = RetryCircuitBreaker::new(LoopbackService, NonCloneableTestRetryPolicy);
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(bad_req.clone());
        let result = fut.await;
        assert_eq!(result, bad_req.as_service_response());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_calls_can_advance() {
        let good_req = BasicRequest::success("good");
        let bad_req = BasicRequest::failure("bad");

        let mut circuit_breaker = RetryCircuitBreaker::new(LoopbackService, CloneableTestRetryPolicy);

        // First request should succeed. This is just a warmup.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let fut = svc.call(good_req.clone());
        let result = fut.await;
        assert_eq!(result, good_req.as_service_response());

        // Now we'll create two calls -- one that should fail and one that succeed -- but won't poll them until both are
        // created. This simulates two concurrent calls happening, and what we want to show is that the circuit breaker
        // should only mark itself as open to _new_ calls after it the state changes to open, and should not affect
        // running requests.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let bad_fut = svc.call(bad_req.clone());

        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let good_fut = svc.call(good_req.clone());

        let bad_result = bad_fut.await;
        assert_eq!(bad_result, bad_req.as_open_response());

        let good_result = good_fut.await;
        assert_eq!(good_result, good_req.as_service_response());

        // Now we'll go to make a fourth request, and we'll manually check the readiness of the service to ensure that
        // we're now in a backoff state.
        let mut svc_fut = tokio_test::task::spawn(circuit_breaker.ready());
        assert_pending!(svc_fut.poll());
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_open_between_ready_and_call() {
        let good_req = BasicRequest::success("good");
        let bad_req = BasicRequest::failure("bad");

        let mut circuit_breaker = RetryCircuitBreaker::new(LoopbackService, CloneableTestRetryPolicy);

        // We'll create two calls -- one that should fail and one that succeed -- but order their creation / polling such that the
        // bad request updates the breaker state to be open _before_ we create the good request. This is to exercise
        // that even though the service may report itself as ready, an in-flight request that completes and ultimately
        // changes the breaker state to open should cause subsequent calls to immediately fail.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");
        let bad_fut = svc.call(bad_req.clone());

        // We're just making sure here that the service is ready to accept another call, but we're not creating that
        // call yet.
        let svc = circuit_breaker.ready().await.expect("should never fail to be ready");

        // Run our bad request first and ensure it fails with an open error.
        let bad_result = bad_fut.await;
        assert_eq!(bad_result, bad_req.as_open_response());

        // Now _create_ the good request and ensure that it also fails with an open error.
        let good_fut = svc.call(good_req.clone());
        let good_result = good_fut.await;
        assert_eq!(good_result, good_req.as_open_response());
    }
}
