//! The org propagation marker that `/info` reports.
//!
//! The marker identifies the Datadog organization of the API key without revealing it: the trace-agent fetches the
//! organization's UUID from `/api/v2/validate`, and reports a truncated hash of it as `org_prop_marker`.

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bytes::Bytes;
use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Empty};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::HttpClient;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tracing::{debug, warn};

use crate::common::datadog::api_key::ApiKeyCell;
use crate::common::datadog::config::ForwarderConfiguration;
use crate::common::datadog::io::intake_http_client_builder;

/// Header the API key is sent in.
const DD_API_KEY_HEADER: &str = "DD-API-KEY";

/// Number of retries after the first attempt, as in the trace-agent.
const MAX_RETRIES: u32 = 3;

/// Fetches the org propagation marker of the API key.
pub(super) struct OrgPropagationMarkerFetcher {
    client: HttpClient,
    endpoint: String,
    api_key: ApiKeyCell,
}

impl OrgPropagationMarkerFetcher {
    /// Creates a fetcher that queries `endpoint`, the URL of `/api/v2/validate`, with the current key of `api_key`.
    ///
    /// The HTTP client uses the forwarder's settings, as the trace-agent's client does.
    ///
    /// # Errors
    ///
    /// If the HTTP client can't be built, an error is returned.
    pub(super) fn new(
        forwarder: &ForwarderConfiguration, endpoint: String, api_key: ApiKeyCell,
    ) -> Result<Self, GenericError> {
        let client = intake_http_client_builder(forwarder, forwarder.effective_min_tls_version())?.build()?;
        Ok(Self::with_client(client, endpoint, api_key))
    }

    fn with_client(client: HttpClient, endpoint: String, api_key: ApiKeyCell) -> Self {
        Self {
            client,
            endpoint,
            api_key,
        }
    }

    /// Fetches the marker with the trace-agent's retries: up to four attempts, `base_delay`, twice, and four times
    /// `base_delay` apart.
    ///
    /// Returns `None` once every attempt failed, as the trace-agent then gives up.
    pub(super) async fn fetch_with_retries(mut self, base_delay: Duration) -> Option<String> {
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(base_delay * (1 << (attempt - 1))).await;
            }
            match self.fetch().await {
                Ok(marker) => {
                    debug!(marker, "Fetched the org propagation marker.");
                    return Some(marker);
                }
                Err(error) => warn!(
                    %error,
                    "Org propagation marker fetch attempt {}/{} failed.",
                    attempt + 1,
                    MAX_RETRIES + 1
                ),
            }
        }
        warn!(
            "Org propagation marker fetch failed after {} attempts; `/info` reports none.",
            MAX_RETRIES + 1
        );
        None
    }

    async fn fetch(&mut self) -> Result<String, GenericError> {
        let uri: Uri = self
            .endpoint
            .parse()
            .with_error_context(|| format!("The validate endpoint `{}` is not a valid URL.", self.endpoint))?;
        let request = Request::get(uri)
            .header(DD_API_KEY_HEADER, self.api_key.load().as_ref())
            .body(Empty::<Bytes>::new())
            .error_context("Failed to build the validate request.")?;

        let response = self.client.send(request).await?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .error_context("Failed to read the validate response.")?
            .to_bytes();
        let org_uuid = parse_validate_response(status, &body)?;
        Ok(compute_marker(&org_uuid))
    }
}

/// The part of the `/api/v2/validate` response the marker comes from.
#[derive(Deserialize)]
struct ValidateResponse {
    data: ValidateData,
}

#[derive(Deserialize)]
struct ValidateData {
    #[serde(default)]
    id: String,
}

/// Returns the organization UUID from a `/api/v2/validate` response.
fn parse_validate_response(status: StatusCode, body: &[u8]) -> Result<String, GenericError> {
    if status != StatusCode::OK {
        return Err(generic_error!("unexpected status {}", status.as_u16()));
    }
    let response: ValidateResponse = serde_json::from_slice(body).error_context("decode response")?;
    if response.data.id.is_empty() {
        return Err(generic_error!("empty org UUID in validate response"));
    }
    Ok(response.data.id)
}

/// Computes the marker of an organization UUID, as the trace-agent's `computeOPM` does: the first eight bytes of its
/// SHA-256, with the low four bits of the last one cleared, in unpadded URL-safe base64.
fn compute_marker(org_uuid: &str) -> String {
    let digest = Sha256::digest(org_uuid.as_bytes());
    let mut truncated = [0u8; 8];
    truncated.copy_from_slice(&digest[..8]);
    truncated[7] &= 0xF0;
    URL_SAFE_NO_PAD.encode(truncated)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use http::HeaderMap;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn the_marker_matches_the_trace_agent() {
        // Expected values are the output of the trace-agent's `computeOPM`.
        assert_eq!(compute_marker("8dee7c38-00cb-11ea-a77b-8b5a08d3b091"), "PuAey90vQlA");
        assert_eq!(compute_marker("org"), "6Hy0XAWtOJA");
        assert_eq!(compute_marker(""), "47DEQpj8HBA");
    }

    #[test]
    fn the_org_uuid_comes_from_a_successful_validate_response() {
        let body = br#"{"valid":true,"data":{"id":"org-uuid","type":"api_key"}}"#;
        assert_eq!(parse_validate_response(StatusCode::OK, body).unwrap(), "org-uuid");

        assert!(parse_validate_response(StatusCode::FORBIDDEN, body).is_err());
        assert!(parse_validate_response(StatusCode::OK, br#"{"data":{}}"#).is_err());
        assert!(parse_validate_response(StatusCode::OK, b"not json").is_err());
    }

    /// Starts a validate endpoint that fails the first `failures` requests, and records the API key of each request.
    async fn spawn_validate_endpoint(failures: usize) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .route(
                "/api/v2/validate",
                get(
                    move |State(seen): State<Arc<Mutex<Vec<String>>>>, headers: HeaderMap| async move {
                        let mut seen = seen.lock().unwrap();
                        seen.push(headers[DD_API_KEY_HEADER].to_str().unwrap().to_string());
                        if seen.len() <= failures {
                            (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                        } else {
                            (StatusCode::OK, r#"{"valid":true,"data":{"id":"org"}}"#.to_string())
                        }
                    },
                ),
            )
            .with_state(Arc::clone(&seen));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        (format!("http://{addr}/api/v2/validate"), seen)
    }

    fn fetcher(endpoint: String, api_key: &ApiKeyCell) -> OrgPropagationMarkerFetcher {
        let client = HttpClient::builder().build().unwrap();
        OrgPropagationMarkerFetcher::with_client(client, endpoint, api_key.clone())
    }

    #[tokio::test]
    async fn fetches_the_marker_with_the_current_api_key_and_retries() {
        crate::common::datadog::test_util::init_tls();
        let (endpoint, seen) = spawn_validate_endpoint(2).await;
        let api_key = ApiKeyCell::new("first-key").unwrap();
        api_key.refresh("test", "rotated-key");

        let marker = fetcher(endpoint, &api_key)
            .fetch_with_retries(Duration::from_millis(1))
            .await;

        assert_eq!(marker.as_deref(), Some("6Hy0XAWtOJA"));
        assert_eq!(*seen.lock().unwrap(), ["rotated-key", "rotated-key", "rotated-key"]);
    }

    #[tokio::test]
    async fn gives_up_after_four_attempts() {
        crate::common::datadog::test_util::init_tls();
        let (endpoint, seen) = spawn_validate_endpoint(usize::MAX).await;
        let api_key = ApiKeyCell::new("key").unwrap();

        let marker = fetcher(endpoint, &api_key)
            .fetch_with_retries(Duration::from_millis(1))
            .await;

        assert_eq!(marker, None);
        assert_eq!(seen.lock().unwrap().len(), 4);
    }
}
