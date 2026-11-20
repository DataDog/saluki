//! Helpers for tests that drive the router with requests.

use async_compression::tokio::write::{GzipEncoder, ZstdEncoder};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt as _;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt as _;
use tower::ServiceExt as _;

use super::{initialize_app_router, DogStatsDForwardingState};

/// The `User-Agent` of every request that `post` sends.
pub const USER_AGENT: &str = "test-sender/1.2.3";

/// The content encodings that tests send: none, `gzip`, and `zstd`.
pub const ENCODINGS: [Option<&str>; 3] = [None, Some("gzip"), Some("zstd")];

pub fn router() -> Router {
    initialize_app_router(DogStatsDForwardingState::new())
}

/// Compresses `data` with the given content encoding, or returns it as is when there is none.
pub async fn encode(encoding: Option<&str>, data: &[u8]) -> Vec<u8> {
    match encoding {
        None => data.to_vec(),
        Some("gzip") => {
            let mut encoder = GzipEncoder::new(Vec::new());
            encoder.write_all(data).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        Some("zstd") => {
            let mut encoder = ZstdEncoder::new(Vec::new());
            encoder.write_all(data).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        Some(other) => panic!("unsupported test encoding: {other}"),
    }
}

/// Posts `body` as is, declaring `encoding` in the headers, and returns the response status.
pub async fn post(router: &Router, path: &str, encoding: Option<&str>, body: Vec<u8>) -> StatusCode {
    let mut request = Request::post(path).header("user-agent", USER_AGENT);
    if let Some(encoding) = encoding {
        request = request.header("content-encoding", encoding);
    }

    router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

/// Encodes `payload` with each of the `ENCODINGS` and posts it, expecting the intake to accept every request.
pub async fn post_in_each_encoding(router: &Router, path: &str, payload: &[u8]) {
    for encoding in ENCODINGS {
        let body = encode(encoding, payload).await;
        assert_eq!(post(router, path, encoding, body).await, StatusCode::ACCEPTED);
    }
}

/// Returns the body of a successful `GET` request.
pub async fn get_bytes(router: &Router, path: &str) -> Vec<u8> {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    response.into_body().collect().await.unwrap().to_bytes().to_vec()
}

/// Returns the JSON body of a successful `GET` request.
pub async fn get_json<T>(router: &Router, path: &str) -> T
where
    T: DeserializeOwned,
{
    serde_json::from_slice(&get_bytes(router, path).await).unwrap()
}
