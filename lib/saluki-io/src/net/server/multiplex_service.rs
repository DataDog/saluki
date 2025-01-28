use std::{
    convert::Infallible,
    task::{Context, Poll},
};

use axum::{
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
};
use futures::{future::BoxFuture, ready};
use http::Request;
use hyper::body::Incoming;
use tower::Service;

/// A [`Service`] that multiplexes requests between two underlying REST-ful and gRPC services.
///
/// In some scenarios, it can be useful to expose gRPC services on the same existing REST-ful HTTP endpoints as gRPC
/// defaults to using HTTP/2, and doing so allows the use of a single exposed endpoint/port, leading to reduced
/// configuration and complexity.
///
/// This service takes two services -- one for REST-ful requests and one for gRPC requests -- and multiplexes incoming
/// requests between the two by inspecting the request headers, specifically the `Content-Type` header. If the header
/// starts with `application/grpc`, the request is assumed to be a gRPC request and is forwarded to the gRPC service.
/// Otherwise, the request is assumed to be a REST-ful request and is forwarded to the REST-ful service.
pub struct MultiplexService<A, B> {
    rest: A,
    rest_ready: bool,
    grpc: B,
    grpc_ready: bool,
}

impl<A, B> MultiplexService<A, B> {
    /// Creates a new `MultiplexService` from the given REST-ful and gRPC services.
    pub fn new(rest: A, grpc: B) -> Self {
        Self {
            rest,
            rest_ready: false,
            grpc,
            grpc_ready: false,
        }
    }
}

impl<A, B> Clone for MultiplexService<A, B>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            rest: self.rest.clone(),
            grpc: self.grpc.clone(),
            // Don't assume either service is ready when cloning.
            rest_ready: false,
            grpc_ready: false,
        }
    }
}

impl<A, B> Service<Request<Incoming>> for MultiplexService<A, B>
where
    A: Service<Request<Incoming>, Error = Infallible>,
    A::Response: IntoResponse,
    A::Future: Send + 'static,
    B: Service<Request<Incoming>>,
    B::Response: IntoResponse,
    B::Future: Send + 'static,
{
    type Response = Response;
    type Error = B::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // We drive both services to readiness as we need to ensure either service can handle a request before we can
        // actually accept a call, given that we don't know what type of request we're about to get.
        loop {
            match (self.rest_ready, self.grpc_ready) {
                (true, true) => {
                    return Ok(()).into();
                }
                (false, _) => {
                    ready!(self.rest.poll_ready(cx)).map_err(|err| match err {})?;
                    self.rest_ready = true;
                }
                (_, false) => {
                    ready!(self.grpc.poll_ready(cx))?;
                    self.grpc_ready = true;
                }
            }
        }
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        assert!(
            self.grpc_ready,
            "grpc service not ready. Did you forget to call `poll_ready`?"
        );
        assert!(
            self.rest_ready,
            "rest service not ready. Did you forget to call `poll_ready`?"
        );

        // Figure out which service this request should go to, reset that service's readiness, and call it.
        if is_grpc_request(&req) {
            self.grpc_ready = false;
            let future = self.grpc.call(req);
            Box::pin(async move {
                let res = future.await?;
                Ok(res.into_response())
            })
        } else {
            self.rest_ready = false;
            let future = self.rest.call(req);
            Box::pin(async move {
                let res = future.await.map_err(|err| match err {})?;
                Ok(res.into_response())
            })
        }
    }
}

fn is_grpc_request<B>(req: &Request<B>) -> bool {
    // We specifically check if the header value _starts_ with `application/grpc` as the gRPC spec allows for additional
    // suffixes to describe how the payload is encoded (i.e. `application/grpc+proto` when encoded via Protocol Buffers
    // vs `application/grpc+json` when encoded via JSON for gRPC-Web).
    req.headers()
        .get(CONTENT_TYPE)
        .map(|content_type| content_type.as_bytes())
        .filter(|content_type| content_type.starts_with(b"application/grpc"))
        .is_some()
}
