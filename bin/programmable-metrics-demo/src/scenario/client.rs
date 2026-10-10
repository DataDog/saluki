//! A client of the demo server's JSON API, as the scenario runner drives it.

use std::time::Duration;

use bytes::Bytes;
use http::{header::CONTENT_TYPE, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full};
use rustls::RootCertStore;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::HttpClient;
use serde_json::Value;

/// How long one request to the demo server may take. Compiling waits for ADP's active plan when it changed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The largest response read.
const MAX_RESPONSE_SIZE: usize = 32 * 1024 * 1024;

/// A client of the demo server on a loopback port.
pub struct DemoClient {
    client: HttpClient,
    authority: String,
}

impl DemoClient {
    /// Creates a client of the demo server listening on `port`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(port: u16) -> Result<Self, GenericError> {
        // Plain HTTP on loopback: no certificate authority is needed.
        let client = HttpClient::builder()
            .with_request_timeout(REQUEST_TIMEOUT)
            .with_tls_config(|tls| tls.with_root_cert_store(RootCertStore::empty()))
            .build()
            .error_context("Failed to build the HTTP client for the demo server.")?;
        Ok(Self {
            client,
            authority: format!("127.0.0.1:{}", port),
        })
    }

    /// Sends `GET path` and returns the status and the body.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or its body cannot be read.
    pub async fn get(&mut self, path: &str) -> Result<(StatusCode, Bytes), GenericError> {
        let request = Request::get(self.uri(path)?)
            .body(Full::new(Bytes::new()))
            .error_context("Failed to build a request to the demo server.")?;
        self.send(request).await
    }

    /// Sends `GET path` and reads a `200 OK` answer as JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, is not answered with `200 OK`, or its body is not JSON.
    pub async fn get_json(&mut self, path: &str) -> Result<Value, GenericError> {
        let (status, body) = self.get(path).await?;
        ok_json(path, status, &body)
    }

    /// Sends `POST path` with a JSON body and returns the status and the JSON answer.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or the answer is not JSON.
    pub async fn post_json(&mut self, path: &str, body: &Value) -> Result<(StatusCode, Value), GenericError> {
        let request = Request::post(self.uri(path)?)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .error_context("Failed to build a request to the demo server.")?;
        let (status, body) = self.send(request).await?;
        let value = serde_json::from_slice(&body).with_error_context(|| {
            format!(
                "POST {} answered {} with a body that is not JSON: {}",
                path,
                status,
                String::from_utf8_lossy(&body[..body.len().min(512)])
            )
        })?;
        Ok((status, value))
    }

    fn uri(&self, path: &str) -> Result<Uri, GenericError> {
        Uri::builder()
            .scheme("http")
            .authority(self.authority.as_str())
            .path_and_query(path)
            .build()
            .with_error_context(|| format!("Invalid demo server path `{}`.", path))
    }

    async fn send(&mut self, request: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes), GenericError> {
        let what = format!("{} {}", request.method(), request.uri());
        let response = self
            .client
            .send(request)
            .await
            .with_error_context(|| format!("{} failed.", what))?;
        let status = response.status();
        let body = http_body_util::Limited::new(response.into_body(), MAX_RESPONSE_SIZE)
            .collect()
            .await
            .map_err(|error| generic_error!("Failed to read the answer of {}: {}", what, error))?
            .to_bytes();
        Ok((status, body))
    }
}

/// Reads a `200 OK` answer of `path` as JSON.
fn ok_json(path: &str, status: StatusCode, body: &[u8]) -> Result<Value, GenericError> {
    if status != StatusCode::OK {
        return Err(generic_error!(
            "GET {} answered {}: {}",
            path,
            status,
            String::from_utf8_lossy(&body[..body.len().min(512)])
        ));
    }
    serde_json::from_slice(body).with_error_context(|| format!("GET {} answered with a body that is not JSON.", path))
}
