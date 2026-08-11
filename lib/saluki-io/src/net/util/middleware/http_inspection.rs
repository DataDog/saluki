use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};

use http::{Response, StatusCode};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

/// A callback invoked when a response with a matching status code is observed.
type InspectorFn = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
struct Inspector {
    status: StatusCode,
    callback: InspectorFn,
}

/// A [`Layer`] that observes HTTP responses and invokes registered callbacks based on their status code.
///
/// Inspectors are registered against a specific [`StatusCode`] via [`with_inspector`][Self::with_inspector]. Whenever
/// the wrapped service produces a successful (`Ok`) response whose status matches, the corresponding callback is
/// invoked. Multiple inspectors may be registered, including more than one for the same status, in which case all
/// matching callbacks are invoked. The response itself is always passed through unchanged, and service errors never
/// trigger callbacks.
///
/// This layer is purely observational: it does not modify requests or responses, retry, or short-circuit. Because it
/// sees the raw response from the inner service, placing it *below* a layer that transforms responses into errors (such
/// as a retry circuit breaker) allows callbacks to observe responses that would otherwise never surface to the caller.
///
/// Callbacks are invoked from within the response future, which may be polled concurrently, so they must be `Send` and
/// `Sync`. Any throttling or deduplication of callback invocations is the caller's responsibility.
#[derive(Clone, Default)]
pub struct HttpInspectionLayer {
    inspectors: Vec<Inspector>,
}

impl HttpInspectionLayer {
    /// Creates a new `HttpInspectionLayer` with no registered inspectors.
    ///
    /// Without any inspectors, the layer passes every response through unchanged and invokes nothing.
    pub fn new() -> Self {
        Self { inspectors: Vec::new() }
    }

    /// Registers a callback to be invoked whenever a response with the given status code is observed.
    ///
    /// The callback is invoked once per matching response, including responses produced by retried requests when this
    /// layer sits below a retrying layer.
    pub fn with_inspector<F>(mut self, status: StatusCode, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.inspectors.push(Inspector {
            status,
            callback: Arc::new(callback),
        });
        self
    }
}

impl<S> Layer<S> for HttpInspectionLayer {
    type Service = HttpInspection<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HttpInspection {
            inner,
            inspectors: Arc::new(self.inspectors.clone()),
        }
    }
}

/// Service produced by [`HttpInspectionLayer`].
///
/// See [`HttpInspectionLayer`] for details on the inspection behavior.
#[derive(Clone)]
pub struct HttpInspection<S> {
    inner: S,
    inspectors: Arc<Vec<Inspector>>,
}

impl<S, Request, ResBody> Service<Request> for HttpInspection<S>
where
    S: Service<Request, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        ResponseFuture {
            inner: self.inner.call(req),
            inspectors: Arc::clone(&self.inspectors),
        }
    }
}

pin_project! {
    /// Response future for [`HttpInspection`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        inspectors: Arc<Vec<Inspector>>,
    }
}

impl<F, ResBody, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = ready!(this.inner.poll(cx));

        // Only successful responses carry a status code to inspect; errors are passed through untouched.
        if let Ok(response) = &result {
            let status = response.status();
            for inspector in this.inspectors.iter() {
                if inspector.status == status {
                    (inspector.callback)();
                }
            }
        }

        Poll::Ready(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        future::{ready, Ready},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tower::ServiceExt as _;

    use super::*;

    /// A service that always responds with a fixed status code.
    #[derive(Clone)]
    struct StatusService(StatusCode);

    impl Service<()> for StatusService {
        type Response = Response<()>;
        type Error = Infallible;
        type Future = Ready<Result<Response<()>, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            ready(Ok(Response::builder().status(self.0).body(()).unwrap()))
        }
    }

    /// A service that always fails, to exercise the error pass-through path.
    #[derive(Clone)]
    struct ErrorService;

    impl Service<()> for ErrorService {
        type Response = Response<()>;
        type Error = &'static str;
        type Future = Ready<Result<Response<()>, &'static str>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            ready(Err("boom"))
        }
    }

    fn counting_inspector(counter: &Arc<AtomicUsize>) -> impl Fn() + Send + Sync + 'static {
        let counter = Arc::clone(counter);
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn fires_callback_on_matching_status() {
        let hits = Arc::new(AtomicUsize::new(0));
        let layer = HttpInspectionLayer::new().with_inspector(StatusCode::FORBIDDEN, counting_inspector(&hits));

        let response = layer
            .layer(StatusService(StatusCode::FORBIDDEN))
            .oneshot(())
            .await
            .expect("service should not error");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_fire_callback_on_other_status() {
        let hits = Arc::new(AtomicUsize::new(0));
        let layer = HttpInspectionLayer::new().with_inspector(StatusCode::FORBIDDEN, counting_inspector(&hits));

        let response = layer
            .layer(StatusService(StatusCode::OK))
            .oneshot(())
            .await
            .expect("service should not error");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn does_not_fire_callback_on_service_error() {
        let hits = Arc::new(AtomicUsize::new(0));
        let layer = HttpInspectionLayer::new().with_inspector(StatusCode::FORBIDDEN, counting_inspector(&hits));

        let error = layer
            .layer(ErrorService)
            .oneshot(())
            .await
            .expect_err("service should error");

        assert_eq!(error, "boom");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn only_matching_inspectors_fire() {
        let forbidden_hits = Arc::new(AtomicUsize::new(0));
        let ok_hits = Arc::new(AtomicUsize::new(0));
        let layer = HttpInspectionLayer::new()
            .with_inspector(StatusCode::FORBIDDEN, counting_inspector(&forbidden_hits))
            .with_inspector(StatusCode::OK, counting_inspector(&ok_hits));

        layer
            .layer(StatusService(StatusCode::OK))
            .oneshot(())
            .await
            .expect("service should not error");

        assert_eq!(forbidden_hits.load(Ordering::SeqCst), 0);
        assert_eq!(ok_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_inspectors_for_a_status_fire() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let layer = HttpInspectionLayer::new()
            .with_inspector(StatusCode::FORBIDDEN, counting_inspector(&first))
            .with_inspector(StatusCode::FORBIDDEN, counting_inspector(&second));

        layer
            .layer(StatusService(StatusCode::FORBIDDEN))
            .oneshot(())
            .await
            .expect("service should not error");

        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn passes_response_through_without_inspectors() {
        let response = HttpInspectionLayer::new()
            .layer(StatusService(StatusCode::IM_A_TEAPOT))
            .oneshot(())
            .await
            .expect("service should not error");

        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }
}
