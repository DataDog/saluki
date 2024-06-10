//! Basic HTTP client.

use std::io;

use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{
        connect::{Connect, HttpConnector},
        Client, Error,
    },
    rt::TokioExecutor,
};
use rustls::crypto::aws_lc_rs::default_provider as aws_lc_rs_default_provider;

use crate::buf::ChunkedBytesBuffer;

pub type ChunkedHttpsClient = HttpClient<HttpsConnector<HttpConnector>, ChunkedBytesBuffer>;

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
            .with_provider_and_native_roots(aws_lc_rs_default_provider())
            .map(|builder| builder.https_or_http().enable_all_versions().build())
            .map(HttpClient::from_connector)
    }
}

impl<C, B> HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Send + 'static + Unpin,
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
