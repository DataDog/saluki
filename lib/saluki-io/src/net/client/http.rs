//! Basic HTTP client.

use std::{io, task::Poll};

use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{
        connect::{Connect, HttpConnector},
        Client, Error, ResponseFuture,
    },
    rt::TokioExecutor,
};
use tower::{BoxError, Service};

use crate::buf::ChunkedBuffer;

pub type ChunkedHttpsClient<O> = HttpClient<HttpsConnector<HttpConnector>, ChunkedBuffer<O>>;

pub struct HttpClient<C = (), B = ()> {
    inner: Client<C, B>,
}

impl HttpClient<(), ()> {
    pub fn https<B>() -> io::Result<HttpClient<HttpsConnector<HttpConnector>, B>>
    where
        B: Body + Unpin + Send + 'static,
        B::Data: Send,
        B::Error: std::error::Error + Send + Sync,
    {
        HttpsConnectorBuilder::new()
            .with_native_roots()
            .map(|builder| builder.https_or_http().enable_all_versions().build())
            .map(HttpClient::from_connector)
    }
}

impl<C, B> HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    pub fn from_connector(connector: C) -> Self {
        HttpClient {
            inner: Client::builder(TokioExecutor::new()).build(connector),
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
