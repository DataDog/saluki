//! Basic HTTP client.

use std::{task::Poll, time::Duration};

use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{
        connect::{Connect, HttpConnector},
        Client, Error, ResponseFuture,
    },
    rt::{TokioExecutor, TokioTimer},
};
use saluki_error::GenericError;
use saluki_tls::ClientTLSConfigBuilder;
use tower::{BoxError, Service};

use crate::buf::ChunkedBuffer;

use super::replay::ReplayBody;

pub type ChunkedHttpsClient<O> = HttpClient<HttpsConnector<HttpConnector>, ReplayBody<ChunkedBuffer<O>>>;

/// A batteries-included HTTP client.
///
/// ## Features
///
/// - TLS support (HTTPS) using the platform's native certificate store
/// - automatically selects between HTTP/1.1 and HTTP/2 based on ALPN negotiation
#[derive(Clone)]
pub struct HttpClient<C = (), B = ()> {
    inner: Client<C, B>,
}

impl HttpClient<(), ()> {
    /// Creates a new `HttpClient` with default configuration.
    ///
    /// ## Errors
    ///
    /// If there was an error building the TLS configuration for the client, an error will be returned.
    pub fn https<B>() -> Result<HttpClient<HttpsConnector<HttpConnector>, B>, GenericError>
    where
        B: Body + Clone + Unpin + Send + 'static,
        B::Data: Send,
        B::Error: std::error::Error + Send + Sync,
    {
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false);
        http_connector.set_connect_timeout(Some(Duration::from_secs(30)));

        let tls_config = ClientTLSConfigBuilder::new().build()?;
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(http_connector);

        Ok(HttpClient::from_connector(connector))
    }
}

impl<C, B> HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Clone + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    pub fn from_connector(connector: C) -> Self {
        HttpClient {
            inner: Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(5)
                .pool_idle_timeout(Duration::from_secs(45))
                .pool_timer(TokioTimer::new())
                .build(connector),
        }
    }

    pub async fn send(&self, req: Request<B>) -> Result<Response<Incoming>, Error> {
        self.inner.request(req).await
    }
}

impl<C, B> Service<Request<B>> for HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Response = hyper::Response<Incoming>;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        self.inner.call(req)
    }
}
