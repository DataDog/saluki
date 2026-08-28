//! gRPC support for [`HttpServer`][super::http::HttpServer].
//!
//! There is no dedicated gRPC server here, because gRPC does not need one: it is HTTP/2 plus a route naming
//! convention (`/<package>.<Service>/<Method>`) and a trailer-carried status code. `tonic`'s [`Routes`] is an
//! [`axum::Router`] underneath, and [`HttpServer`][super::http::HttpServer] already serves HTTP/2, so a gRPC service
//! is served by handing its routes to an [`HttpServer`][super::http::HttpServer] like any other route set.
//!
//! What that leaves are the parts route matching can't cover on its own: a request that matches nothing has to be
//! answered in whichever protocol the caller spoke, and a request that carries a deadline has to be held to it.
//! [`merge_grpc_routes`] wires up both.
//!
//! Most callers never reach for this module directly. Handing services to
//! [`HttpServer::add_grpc_service`][super::http::HttpServer::add_grpc_service] applies all of it:
//!
//! ```no_run
//! # use saluki_io::net::{server::http::{Http2Config, HttpServer}, ListenAddress};
//! # fn build<S>(service: S, listen_address: ListenAddress)
//! # where
//! #     S: tower::Service<http::Request<tonic::body::Body>, Error = std::convert::Infallible>
//! #         + tonic::server::NamedService + Clone + Send + Sync + 'static,
//! #     S::Response: axum::response::IntoResponse,
//! #     S::Future: Send + 'static,
//! # {
//! let _server = HttpServer::from_listen_address(listen_address)
//!     .add_grpc_service(service)
//!     .with_http2_only()
//!     .with_http2_config(Http2Config::grpc_defaults());
//! # }
//! ```
//!
//! [`merge_grpc_routes`] is for callers that build and serve their own router, such as those still on
//! [`UnsupervisedHttpServer`][super::http::UnsupervisedHttpServer].

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

use axum::{body::Body, response::IntoResponse as _, Router};
use http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, Request, Response, StatusCode};
use pin_project_lite::pin_project;
use tokio::time::{sleep, Sleep};
use tonic::{service::Routes, Status};
use tower::{Layer, Service};
use tracing::trace;

/// Header a gRPC client uses to communicate the deadline it is holding the server to.
const GRPC_TIMEOUT_HEADER: &str = "grpc-timeout";

const SECONDS_PER_HOUR: u64 = 60 * 60;
const SECONDS_PER_MINUTE: u64 = 60;

/// Largest `TimeoutValue` the gRPC specification allows, expressed as a digit count.
///
/// Enforcing it is also what keeps the unit conversions below from overflowing.
const MAX_TIMEOUT_VALUE_DIGITS: usize = 8;

/// Merges gRPC routes into an HTTP router.
///
/// The returned router serves both route sets, and answers anything that matches neither with [`unmatched_route`]. The
/// gRPC routes are additionally wrapped in [`GrpcTimeoutLayer`], so a caller's `grpc-timeout` deadline is enforced.
///
/// `tonic` installs its own fallback on [`Routes`] to return gRPC `UNIMPLEMENTED`, and axum refuses to merge two
/// routers that both define one, so that fallback is dropped in favor of the protocol-aware one. Any fallback set on
/// `http_router` is dropped for the same reason: register a catch-all route instead if you need one.
///
/// # Panics
///
/// Panics if the two route sets define the same path, which is [`Router::merge`]'s behavior. In practice this can only
/// happen if an HTTP route is registered under a path that looks like a gRPC method.
pub fn merge_grpc_routes(http_router: Router, grpc_routes: Routes) -> Router {
    // The deadline layer goes on the gRPC routes alone. `grpc-timeout` is a gRPC concept, and an HTTP route that
    // happens to receive the header has no reason to be held to it.
    let grpc_router = grpc_routes.into_axum_router().reset_fallback().layer(GrpcTimeoutLayer);

    http_router
        .reset_fallback()
        .merge(grpc_router)
        .fallback(unmatched_route)
}

/// A [`Layer`] that holds a gRPC request to the deadline it arrived with.
///
/// See [`GrpcTimeout`] for what that means in practice.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrpcTimeoutLayer;

impl<S> Layer<S> for GrpcTimeoutLayer {
    type Service = GrpcTimeout<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcTimeout { inner }
    }
}

/// A [`Service`] that bounds how long the inner service has to answer a gRPC request.
///
/// A gRPC client states its deadline in the `grpc-timeout` request header. Enforcing it server-side means a request
/// the caller has already given up on stops consuming resources, rather than running to completion so its response can
/// be discarded.
///
/// A request without the header, or with a header that doesn't parse, is passed through with no deadline. Silently
/// ignoring a malformed value is what the gRPC specification calls for: a deadline the server can't read is not grounds
/// for rejecting the request.
///
/// An expired deadline is answered with `DEADLINE_EXCEEDED`, which is the code the specification assigns to it. Note
/// that `tonic`'s own timeout middleware answers with `CANCELLED` instead, an artifact of how it routes the expiry
/// through its generic error handling.
///
/// # Missing
///
/// There is no server-side maximum to bound a client that asks for an unreasonably long deadline, because nothing here
/// needs one yet. Adding it means taking the shorter of the two durations.
#[derive(Clone, Copy, Debug)]
pub struct GrpcTimeout<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for GrpcTimeout<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>, Error = Infallible>,
    ResBody: Default,
{
    type Response = Response<ResBody>;
    type Error = Infallible;
    type Future = GrpcTimeoutFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let deadline = match parse_grpc_timeout(req.headers()) {
            Ok(deadline) => deadline,
            Err(value) => {
                trace!(header = ?value, "Ignoring malformed `grpc-timeout` header.");
                None
            }
        };

        GrpcTimeoutFuture {
            inner: self.inner.call(req),
            deadline: deadline.map(sleep),
        }
    }
}

pin_project! {
    /// Response future for [`GrpcTimeout`].
    pub struct GrpcTimeoutFuture<F> {
        #[pin]
        inner: F,

        #[pin]
        deadline: Option<Sleep>,
    }
}

impl<F, ResBody> Future for GrpcTimeoutFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, Infallible>>,
    ResBody: Default,
{
    type Output = Result<Response<ResBody>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // Poll the request first, so that a response which is already ready wins over a deadline that expires on the
        // same poll. Answering a request we have the answer to is never worse than reporting that it timed out.
        if let Poll::Ready(result) = this.inner.poll(cx) {
            return Poll::Ready(result);
        }

        if let Some(deadline) = this.deadline.as_pin_mut() {
            ready!(deadline.poll(cx));

            // Returning here drops the inner future, which is what actually cancels the work in flight.
            return Poll::Ready(Ok(Status::deadline_exceeded(
                "Deadline expired before operation could complete.",
            )
            .into_http()));
        }

        Poll::Pending
    }
}

/// Parses the deadline carried by the `grpc-timeout` header, if there is one.
///
/// Returns the offending value when the header is present but doesn't parse, so the caller can report what it saw.
///
/// The encoding is `TimeoutValue TimeoutUnit`, where the value is at most eight digits and the unit is one of `H`
/// (hours), `M` (minutes), `S` (seconds), `m` (milliseconds), `u` (microseconds), or `n` (nanoseconds). See the
/// [gRPC over HTTP/2 specification][spec].
///
/// [spec]: https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md
fn parse_grpc_timeout(headers: &HeaderMap) -> Result<Option<Duration>, &HeaderValue> {
    let Some(value) = headers.get(GRPC_TIMEOUT_HEADER) else {
        return Ok(None);
    };

    // `to_str` only succeeds for ASCII, so splitting off the last byte can't land in the middle of a character.
    let encoded = value.to_str().map_err(|_| value)?;
    if encoded.is_empty() {
        return Err(value);
    }
    let (timeout_value, timeout_unit) = encoded.split_at(encoded.len() - 1);

    if timeout_value.len() > MAX_TIMEOUT_VALUE_DIGITS {
        return Err(value);
    }

    let timeout_value: u64 = timeout_value.parse().map_err(|_| value)?;

    let timeout = match timeout_unit {
        "H" => Duration::from_secs(timeout_value * SECONDS_PER_HOUR),
        "M" => Duration::from_secs(timeout_value * SECONDS_PER_MINUTE),
        "S" => Duration::from_secs(timeout_value),
        "m" => Duration::from_millis(timeout_value),
        "u" => Duration::from_micros(timeout_value),
        "n" => Duration::from_nanos(timeout_value),
        _ => return Err(value),
    };

    Ok(Some(timeout))
}

/// Answers a request that matched no route, in the protocol the caller used.
///
/// gRPC callers get the `UNIMPLEMENTED` status they expect, and everyone else gets a plain `404 Not Found`.
///
/// Answering a gRPC caller with a bare 404 would mostly work -- the gRPC specification has clients map `404` to
/// `UNIMPLEMENTED` when no `grpc-status` is present -- but it costs nothing to return the status directly, and doing so
/// keeps the response identical to what a standalone gRPC server would send.
pub async fn unmatched_route(headers: HeaderMap) -> Response<Body> {
    if is_grpc_request(&headers) {
        Status::unimplemented("").into_http()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Returns `true` if the given headers indicate a gRPC request.
///
/// The check is on the `Content-Type` header rather than the request path, since a request that reaches this point
/// matched no route and so its path says nothing useful.
pub fn is_grpc_request(headers: &HeaderMap) -> bool {
    // We specifically check if the header value _starts_ with `application/grpc` as the gRPC spec allows for additional
    // suffixes to describe how the payload is encoded (i.e. `application/grpc+proto` when encoded via Protocol Buffers
    // vs `application/grpc+json` when encoded via JSON for gRPC-Web).
    headers
        .get(CONTENT_TYPE)
        .map(|content_type| content_type.as_bytes())
        .is_some_and(|content_type| content_type.starts_with(b"application/grpc"))
}

#[cfg(test)]
mod tests {
    use tonic::server::NamedService;
    use tower::{util::service_fn, ServiceExt as _};

    use super::*;

    fn headers_with_content_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(value).expect("should be a valid header"),
        );
        headers
    }

    /// Reads the gRPC status code off a response, if it carries one.
    fn grpc_status(response: &Response<Body>) -> Option<&str> {
        response
            .headers()
            .get("grpc-status")
            .and_then(|value| value.to_str().ok())
    }

    /// Parses a `grpc-timeout` header value, discarding the offending value on failure.
    fn parse_timeout(value: &str) -> Result<Option<Duration>, ()> {
        let mut headers = HeaderMap::new();
        headers.insert(
            GRPC_TIMEOUT_HEADER,
            HeaderValue::from_str(value).expect("should be a valid header"),
        );

        parse_grpc_timeout(&headers).map_err(|_| ())
    }

    /// Runs a handler that takes `handler_delay` to answer, behind the deadline layer.
    async fn call_with_deadline(timeout_header: Option<&str>, handler_delay: Duration) -> Response<Body> {
        let service = GrpcTimeoutLayer.layer(service_fn(move |_req: Request<Body>| async move {
            tokio::time::sleep(handler_delay).await;
            Ok::<_, Infallible>(Response::new(Body::empty()))
        }));

        let mut request = Request::new(Body::empty());
        if let Some(timeout_header) = timeout_header {
            request.headers_mut().insert(
                GRPC_TIMEOUT_HEADER,
                HeaderValue::from_str(timeout_header).expect("should be a valid header"),
            );
        }

        service.oneshot(request).await.expect("service should not fail")
    }

    /// A gRPC service that never answers quickly enough to beat a deadline.
    #[derive(Clone)]
    struct SlowService;

    impl NamedService for SlowService {
        const NAME: &'static str = "test.SlowService";
    }

    impl Service<Request<tonic::body::Body>> for SlowService {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<tonic::body::Body>) -> Self::Future {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(Response::new(Body::empty()))
            })
        }
    }

    #[test]
    fn detects_grpc_content_types() {
        // The bare type and the encoding-suffixed forms are all gRPC.
        for content_type in ["application/grpc", "application/grpc+proto", "application/grpc+json"] {
            assert!(
                is_grpc_request(&headers_with_content_type(content_type)),
                "'{content_type}' should be detected as gRPC"
            );
        }
    }

    #[test]
    fn does_not_detect_non_grpc_content_types() {
        for content_type in ["application/json", "application/x-protobuf", "text/plain"] {
            assert!(
                !is_grpc_request(&headers_with_content_type(content_type)),
                "'{content_type}' should not be detected as gRPC"
            );
        }

        assert!(
            !is_grpc_request(&HeaderMap::new()),
            "a request without a content type should not be detected as gRPC"
        );
    }

    #[tokio::test]
    async fn unmatched_grpc_request_gets_unimplemented() {
        // gRPC reports errors as a 200 with a `grpc-status` header. UNIMPLEMENTED is code 12.
        let response = unmatched_route(headers_with_content_type("application/grpc")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("grpc-status").and_then(|v| v.to_str().ok()),
            Some("12")
        );
    }

    #[tokio::test]
    async fn unmatched_http_request_gets_not_found() {
        let response = unmatched_route(headers_with_content_type("application/json")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get("grpc-status").is_none());
    }
    #[test]
    fn parses_every_timeout_unit() {
        assert_eq!(parse_timeout("3H"), Ok(Some(Duration::from_secs(3 * 60 * 60))));
        assert_eq!(parse_timeout("1M"), Ok(Some(Duration::from_secs(60))));
        assert_eq!(parse_timeout("42S"), Ok(Some(Duration::from_secs(42))));
        assert_eq!(parse_timeout("13m"), Ok(Some(Duration::from_millis(13))));
        assert_eq!(parse_timeout("2u"), Ok(Some(Duration::from_micros(2))));
        assert_eq!(parse_timeout("82n"), Ok(Some(Duration::from_nanos(82))));
    }

    #[test]
    fn parses_the_largest_permitted_timeout_value() {
        // Eight digits of hours is the ceiling the specification allows, and is what the digit cap exists to keep the
        // unit conversion from overflowing on.
        assert_eq!(
            parse_timeout("99999999H"),
            Ok(Some(Duration::from_secs(99_999_999 * 60 * 60)))
        );
    }

    #[test]
    fn absent_timeout_header_yields_no_deadline() {
        assert_eq!(
            parse_grpc_timeout(&HeaderMap::new()).expect("an absent header should not be an error"),
            None
        );
    }

    #[test]
    fn rejects_malformed_timeout_values() {
        // In order: an unknown unit, more digits than the specification allows, a non-numeric value, a unit with no
        // value, a value with no unit, and an empty header.
        for value in ["82f", "123456789H", "oneH", "S", "8", ""] {
            assert!(parse_timeout(value).is_err(), "'{value}' should not parse");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_expiry_answers_with_deadline_exceeded() {
        // gRPC reports errors as a 200 with a `grpc-status` header. DEADLINE_EXCEEDED is code 4.
        let response = call_with_deadline(Some("50m"), Duration::from_secs(10)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(grpc_status(&response), Some("4"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_response_within_the_deadline_is_passed_through() {
        let response = call_with_deadline(Some("10S"), Duration::from_millis(50)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(grpc_status(&response), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_without_a_deadline_is_not_bounded() {
        let response = call_with_deadline(None, Duration::from_secs(60 * 60)).await;
        assert_eq!(grpc_status(&response), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_malformed_deadline_is_ignored() {
        // The specification treats an unreadable deadline as no deadline: it is not grounds for rejecting the request.
        let response = call_with_deadline(Some("howlong"), Duration::from_secs(60 * 60)).await;
        assert_eq!(grpc_status(&response), None);
    }

    #[tokio::test(start_paused = true)]
    async fn merged_grpc_routes_are_bound_by_their_deadline() {
        let router = merge_grpc_routes(Router::new(), Routes::new(SlowService));
        let request = Request::builder()
            .uri("/test.SlowService/Method")
            .header(CONTENT_TYPE, "application/grpc")
            .header(GRPC_TIMEOUT_HEADER, "50m")
            .body(Body::empty())
            .expect("should build request");

        let response = router.oneshot(request).await.expect("router should answer");
        assert_eq!(grpc_status(&response), Some("4"));
    }

    #[tokio::test(start_paused = true)]
    async fn merged_http_routes_are_not_bound_by_a_grpc_deadline() {
        // The deadline layer is attached to the gRPC routes alone, so an HTTP route that happens to receive the header
        // runs to completion rather than being cut short by a convention it has nothing to do with.
        let slow_route = axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            "done"
        });
        let router = merge_grpc_routes(Router::new().route("/slow", slow_route), Routes::default());
        let request = Request::builder()
            .uri("/slow")
            .header(GRPC_TIMEOUT_HEADER, "50m")
            .body(Body::empty())
            .expect("should build request");

        let response = router.oneshot(request).await.expect("router should answer");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(grpc_status(&response), None);
    }
}
