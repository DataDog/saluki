use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_client_ipc_tls_config};
use futures::TryFutureExt as _;
use http::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    uri::PathAndQuery,
    Request, Response, StatusCode, Uri,
};
use http_body_util::BodyExt as _;
#[cfg(target_os = "linux")]
use http_body_util::Full;
#[cfg(target_os = "linux")]
use hyper::body::Bytes;
use hyper::body::Incoming;
#[cfg(target_os = "linux")]
use prost::Message as _;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{
    client::http::{HttpClient, HttpClientBuilder},
    ListenAddress,
};
use serde::{Deserialize, Serialize};

use crate::{config::DataPlaneConfiguration, dogstatsd_contexts::CONTEXT_DUMP_ROUTE};

/// Typed API client for interacting with the APIs exposed by ADP.
pub struct DataPlaneAPIClient {
    client: HttpClient,
    authority: String,
    authorization: Option<HeaderValue>,
}

#[derive(Serialize)]
struct DogStatsDCaptureBody<'a> {
    duration: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
    compressed: bool,
}

#[derive(Deserialize)]
struct DogStatsDCaptureResponseBody {
    path: String,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct DogStatsDReplaySessionResponseBody {
    session_id: String,
}

impl DataPlaneAPIClient {
    /// Creates a new `DataPlaneAPIClient` from the given generic configuration.
    ///
    /// # Errors
    ///
    /// If the data plane configuration can't be deserialized, or the data plane API endpoints can't be
    /// determined, an error will be returned.
    pub fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let dp_config = DataPlaneConfiguration::from_configuration(config)?;

        let builder = HttpClient::builder().with_tls_config(|b| b.danger_accept_invalid_certs());
        Self::from_builder(builder, dp_config.secure_api_listen_address(), None)
    }

    /// Creates an authenticated `DataPlaneAPIClient` for commands that use the Agent IPC credentials.
    ///
    /// This mirrors the Agent IPC HTTP client contract: verify the configured IPC certificate and attach the Agent
    /// authentication token. The complete Rustls configuration is required because it carries a custom certificate
    /// verifier and client identity that the generic HTTP TLS option builder cannot express.
    ///
    /// This client pins the privileged API server certificate to the configured IPC certificate and authenticates
    /// requests with the raw contents of the configured Agent authentication token file. Unlike the general-purpose
    /// client, it does not impose a whole-request timeout because context dump generation can legitimately take longer
    /// than the default timeout.
    ///
    /// # Errors
    ///
    /// If the data plane or IPC authentication configuration is invalid, the IPC certificate or authentication token
    /// cannot be read, the token cannot be represented as an HTTP header, or the HTTP client cannot be built, an error
    /// is returned.
    pub async fn from_config_with_ipc_auth(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let dp_config = DataPlaneConfiguration::from_configuration(config)?;
        let ipc_config = IpcAuthConfiguration::from_configuration(config)?;
        let tls_config = build_ipc_client_ipc_tls_config(ipc_config.ipc_cert_file_path()).await?;
        let auth_token_path = ipc_config.auth_token_file_path();
        let raw_token = tokio::fs::read(&auth_token_path).await.map_err(|error| {
            generic_error!(
                "Failed to read Agent authentication token from file '{}' ({}).",
                auth_token_path.display(),
                error.kind()
            )
        })?;
        let authorization = authorization_from_token_file_contents(&raw_token, &auth_token_path)?;

        let builder = HttpClient::builder()
            .with_client_tls_config(tls_config)
            .with_connect_timeout(Duration::from_secs(20))
            .without_request_timeout();
        Self::from_builder(builder, dp_config.secure_api_listen_address(), Some(authorization))
    }

    fn from_builder(
        builder: HttpClientBuilder, listen_address: &ListenAddress, authorization: Option<HeaderValue>,
    ) -> Result<Self, GenericError> {
        let (builder, authority) = match listen_address {
            ListenAddress::Tcp(_) => {
                let local_address = listen_address
                    .as_local_connect_addr()
                    .expect("should get local address for TCP");
                (builder, local_address.to_string())
            }

            #[cfg(unix)]
            ListenAddress::Unix(path) => (builder.with_unix_socket_path(path), "127.0.0.1".to_string()),

            _ => {
                return Err(generic_error!(
                    "Expected connection-oriented address (TCP or UDS stream) for privileged API endpoint: {}",
                    listen_address
                ))
            }
        };

        let client = builder
            .build()
            .error_context("Failed to construct API client for privileged API endpoint.")?;

        Ok(Self {
            client,
            authority,
            authorization,
        })
    }

    fn build_uri(&self, path: &str, query: Option<&str>) -> Uri {
        let mut pq = path.to_string();
        if let Some(q) = query {
            pq.push('?');
            pq.push_str(q);
        }

        Uri::builder()
            .scheme("https")
            .authority(self.authority.as_str())
            .path_and_query(pq.parse::<PathAndQuery>().expect("valid path and query"))
            .build()
            .expect("valid URI")
    }

    /// Temporarily overrides the log level for the process.
    ///
    /// The filter directives follow the format used by
    /// [`tracing_subscriber::filter::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives),
    /// which allows for specifying log levels on a global or per-module basis. The duration of the override is
    /// specified in seconds, and the override is reverted after that duration has passed. The same override can be set
    /// again while an override is active in under to "refresh" its override duration.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn set_log_level(&mut self, filter_directives: String, duration_secs: u64) -> Result<(), GenericError> {
        let uri = self.build_uri("/logging/override", Some(&format!("time_secs={duration_secs}")));
        let req = Request::post(uri).body(filter_directives).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(empty_when_success)
    }

    /// Resets the log level for the process.
    ///
    /// This can be used to proactively disable a previous log level override.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn reset_log_level(&mut self) -> Result<(), GenericError> {
        let uri = self.build_uri("/logging/reset", None);
        let req = Request::post(uri).body(String::new()).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(empty_when_success)
    }

    /// Temporarily overrides the metric level for the process.
    ///
    /// Metric levels follow traditional log levels: `trace`, `debug`, `info`, `warn`, and `error`. The duration of the
    /// override is specified in seconds, and the override is reverted after that duration has passed. The same override
    /// can be set again while an override is active in under to "refresh" its override duration.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn set_metric_level(&mut self, level: String, duration_secs: u64) -> Result<(), GenericError> {
        let uri = self.build_uri("/metrics/override", Some(&format!("time_secs={duration_secs}")));
        let req = Request::post(uri).body(level).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(empty_when_success)
    }

    /// Resets the metric level for the process.
    ///
    /// This can be used to proactively disable a previous metric level override.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn reset_metric_level(&mut self) -> Result<(), GenericError> {
        let uri = self.build_uri("/metrics/reset", None);
        let req = Request::post(uri).body(String::new()).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(empty_when_success)
    }

    /// Triggers a statistics collection for DogStatsD metrics.
    ///
    /// Only one statistics collection can be triggered at a time, and an error will be returned if another collection
    /// is already in progress. The collection duration is specified in seconds, and statistics will be collected for
    /// the specified duration before the response is returned.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, an error is returned.
    pub async fn dogstatsd_stats(&mut self, collection_duration_secs: u64) -> Result<String, GenericError> {
        let uri = self.build_uri(
            "/dogstatsd/stats",
            Some(&format!("collection_duration_secs={collection_duration_secs}")),
        );
        let req = Request::get(uri).body(String::new()).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(body_when_success)
    }

    /// Requests an authenticated DogStatsD context dump and returns its path on the server.
    ///
    /// The response contains only the server-local artifact path; this method does not download the dump contents.
    ///
    /// # Errors
    ///
    /// If this client was not created with [`Self::from_config_with_ipc_auth`], the request fails, the server rejects
    /// the request, or the successful response does not contain a JSON string path, an error is returned.
    pub async fn dogstatsd_contexts_dump(&mut self) -> Result<PathBuf, GenericError> {
        let authorization = self.authorization.as_ref().ok_or_else(|| {
            generic_error!(
                "DogStatsD context dumps require an authenticated API client; construct it with `from_config_with_ipc_auth`."
            )
        })?;
        let uri = self.build_uri(CONTEXT_DUMP_ROUTE, None);
        let request = build_dogstatsd_contexts_dump_request(uri, authorization);
        let response = self
            .client
            .send(request)
            .await
            .error_context("Failed to request a DogStatsD context dump from the privileged API endpoint.")?;
        let response = process_response_body(response).await?;
        path_when_context_dump_success(response)
    }

    /// Starts a DogStatsD traffic capture.
    ///
    /// # Errors
    ///
    /// If the request fails, if ADP rejects the capture request, or if the response body can't be decoded, an error is
    /// returned.
    pub async fn dogstatsd_capture(
        &mut self, duration: &str, path: Option<&str>, compressed: bool,
    ) -> Result<String, GenericError> {
        let uri = self.build_uri("/dogstatsd/capture/trigger", None);
        let body = serde_json::to_string(&DogStatsDCaptureBody {
            duration,
            path,
            compressed,
        })
        .error_context("Failed to serialize DogStatsD capture request.")?;
        let req = Request::post(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .expect("valid request");
        let response_body = self
            .client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(body_when_capture_success)?;
        let response = serde_json::from_str::<DogStatsDCaptureResponseBody>(&response_body)
            .error_context("Failed to deserialize DogStatsD capture response.")?;

        Ok(response.path)
    }

    /// Starts a DogStatsD replay session.
    ///
    /// # Errors
    ///
    /// If the request fails, if ADP rejects the session, or if the response body can't be decoded, an error is
    /// returned.
    #[cfg(target_os = "linux")]
    pub async fn dogstatsd_replay_start_session(
        &mut self, state: Option<&datadog_protos::agent::TaggerState>,
    ) -> Result<String, GenericError> {
        let uri = self.build_uri("/dogstatsd/replay/session", None);
        let body = state.map_or_else(Bytes::new, |state| Bytes::from(state.encode_to_vec()));
        let req = Request::post(uri)
            .header(CONTENT_TYPE, "application/x-protobuf")
            .body(Full::new(body))
            .expect("valid request");
        let response_body = self
            .client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(body_when_replay_session_success)?;
        let response = serde_json::from_str::<DogStatsDReplaySessionResponseBody>(&response_body)
            .error_context("Failed to deserialize DogStatsD replay session response.")?;

        Ok(response.session_id)
    }

    /// Finishes a DogStatsD replay session.
    ///
    /// # Errors
    ///
    /// If the request fails, or if ADP rejects the session release, an error is returned.
    #[cfg(target_os = "linux")]
    pub async fn dogstatsd_replay_finish_session(&mut self, session_id: &str) -> Result<(), GenericError> {
        let uri = self.build_uri(&format!("/dogstatsd/replay/session/{session_id}"), None);
        let req = Request::delete(uri)
            .body(Full::new(Bytes::new()))
            .expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(empty_when_replay_session_success)
    }

    /// Retrieves the configuration of the process.
    ///
    /// This is a point-in-time snapshot of the configuration, which could change over time if dynamic configuration is enabled.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, an error is returned.
    pub async fn config(&mut self) -> Result<String, GenericError> {
        let uri = self.build_uri("/config", None);
        let req = Request::get(uri).body(String::new()).expect("valid request");
        self.client
            .send(req)
            .and_then(process_response_body)
            .await
            .and_then(body_when_success)
    }

    /// Retrieves the tags from the workload provider.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider isn't
    /// configured, an error is returned.
    pub async fn workload_tags(&mut self) -> Result<String, GenericError> {
        let uri = self.build_uri("/workload/remote_agent/tags/dump", None);
        let req = Request::get(uri).body(String::new()).expect("valid request");
        let resp = self.client.send(req).and_then(process_response_body).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(generic_error!("Workload provider not configured: no tags available."));
        }

        body_when_success(resp)
    }

    /// Retrieves the External Data entries from the workload provider.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider isn't
    /// configured, an error is returned.
    pub async fn workload_external_data(&mut self) -> Result<String, GenericError> {
        let uri = self.build_uri("/workload/remote_agent/external_data/dump", None);
        let req = Request::get(uri).body(String::new()).expect("valid request");
        let resp = self.client.send(req).and_then(process_response_body).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(generic_error!(
                "Workload provider not configured: no External Data available."
            ));
        }

        Ok(resp.into_body())
    }
}

fn authorization_from_token_file_contents(raw_token: &[u8], token_path: &Path) -> Result<HeaderValue, GenericError> {
    if raw_token.is_empty() {
        return Err(generic_error!(
            "Agent authentication token file '{}' is empty.",
            token_path.display()
        ));
    }

    let mut raw_authorization = Vec::with_capacity("Bearer ".len() + raw_token.len());
    raw_authorization.extend_from_slice(b"Bearer ");
    raw_authorization.extend_from_slice(raw_token);
    let mut authorization = HeaderValue::from_bytes(&raw_authorization).map_err(|_| {
        generic_error!(
            "Agent authentication token file '{}' contains characters that are invalid in an HTTP Authorization header.",
            token_path.display()
        )
    })?;
    authorization.set_sensitive(true);
    Ok(authorization)
}

fn build_dogstatsd_contexts_dump_request(uri: Uri, authorization: &HeaderValue) -> Request<String> {
    Request::post(uri)
        .header(AUTHORIZATION, authorization.clone())
        .body(String::new())
        .expect("valid DogStatsD context dump request")
}

fn path_when_context_dump_success(resp: Response<String>) -> Result<PathBuf, GenericError> {
    match resp.status() {
        status if status.is_success() => serde_json::from_str::<PathBuf>(resp.body())
            .error_context("Failed to decode DogStatsD context dump path from response JSON."),
        StatusCode::UNAUTHORIZED => Err(generic_error!(
            "DogStatsD context dump authentication failed ({}); verify the configured Agent authentication token file.",
            resp.status()
        )),
        status => Err(generic_error!(
            "Failed to create DogStatsD context dump ({}): {}.",
            status,
            resp.into_body()
        )),
    }
}

async fn collect_body(body: Incoming) -> Option<String> {
    // `Collected::to_bytes()` merges all frames. Do not use `Buf::chunk().to_vec()` on an aggregated body: `chunk()`
    // is only the first contiguous slice (often ~16 KiB), which truncates large JSON such as `/config` responses.
    let bytes = body.collect().await.ok()?.to_bytes();
    String::from_utf8(bytes.into()).ok()
}

async fn process_response_body(response: Response<Incoming>) -> Result<Response<String>, GenericError> {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let body = collect_body(body).await.unwrap_or_else(|| String::from("<no body>"));

    if !status.is_server_error() {
        Ok(Response::from_parts(parts, body))
    } else {
        Err(generic_error!("Received non-success response ({}): {}.", status, body))
    }
}

fn body_when_capture_success(resp: Response<String>) -> Result<String, GenericError> {
    match resp.status() {
        status if status.is_success() => Ok(resp.into_body()),
        StatusCode::BAD_REQUEST | StatusCode::PRECONDITION_FAILED => Err(generic_error!("{}", resp.into_body())),
        status => Err(generic_error!(
            "Failed to start DogStatsD capture ({}): {}.",
            status,
            resp.into_body()
        )),
    }
}

#[cfg(target_os = "linux")]
fn body_when_replay_session_success(resp: Response<String>) -> Result<String, GenericError> {
    match resp.status() {
        status if status.is_success() => Ok(resp.into_body()),
        StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
            Err(generic_error!("{}", resp.into_body()))
        }
        status => Err(generic_error!(
            "Failed to start DogStatsD replay session ({}): {}.",
            status,
            resp.into_body()
        )),
    }
}

fn body_when_success(resp: Response<String>) -> Result<String, GenericError> {
    if resp.status().is_success() {
        Ok(resp.into_body())
    } else {
        Err(generic_error!(
            "Received non-success response ({}): {}.",
            resp.status(),
            resp.into_body()
        ))
    }
}

fn empty_when_success(resp: Response<String>) -> Result<(), GenericError> {
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(generic_error!(
            "Received non-success response ({}): {}.",
            resp.status(),
            resp.into_body()
        ))
    }
}

#[cfg(target_os = "linux")]
fn empty_when_replay_session_success(resp: Response<String>) -> Result<(), GenericError> {
    match resp.status() {
        status if status.is_success() => Ok(()),
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Err(generic_error!("{}", resp.into_body())),
        status => Err(generic_error!(
            "Failed to finish DogStatsD replay session ({}): {}.",
            status,
            resp.into_body()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use datadog_agent_commons::ipc::tls::{build_ipc_client_ipc_tls_config, build_ipc_server_tls_config};
    use http::{header::AUTHORIZATION, HeaderValue, Method, Response, StatusCode, Uri};
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_rustls::TlsAcceptor;

    use super::{
        authorization_from_token_file_contents, body_when_capture_success, build_dogstatsd_contexts_dump_request,
        path_when_context_dump_success, DataPlaneAPIClient,
    };
    #[cfg(target_os = "linux")]
    use super::{body_when_replay_session_success, empty_when_replay_session_success};
    use crate::dogstatsd_contexts::CONTEXT_DUMP_ROUTE;

    #[test]
    fn dogstatsd_capture_failed_precondition_surfaces_server_message() {
        let response = Response::builder()
            .status(StatusCode::PRECONDITION_FAILED)
            .body("capture already in progress".to_string())
            .expect("valid response");
        let error = body_when_capture_success(response).expect_err("precondition failure should be an error");

        assert_eq!(error.to_string(), "capture already in progress");
    }

    #[test]
    fn dogstatsd_capture_invalid_argument_surfaces_server_message() {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("missing duration unit".to_string())
            .expect("valid response");
        let error = body_when_capture_success(response).expect_err("invalid arguments should be an error");

        assert_eq!(error.to_string(), "missing duration unit");
    }

    #[test]
    fn dogstatsd_capture_other_errors_keep_context() {
        let response = Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("route not found".to_string())
            .expect("valid response");
        let error = body_when_capture_success(response).expect_err("unexpected status should be an error");

        let message = error.to_string();
        assert!(message.contains("Failed to start DogStatsD capture"));
        assert!(message.contains("route not found"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dogstatsd_replay_session_conflict_surfaces_server_message() {
        let response = Response::builder()
            .status(StatusCode::CONFLICT)
            .body("DogStatsD replay already in progress.".to_string())
            .expect("valid response");
        let error = body_when_replay_session_success(response).expect_err("conflict should be an error");

        assert_eq!(error.to_string(), "DogStatsD replay already in progress.");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dogstatsd_replay_finish_conflict_surfaces_server_message() {
        let response = Response::builder()
            .status(StatusCode::CONFLICT)
            .body("session does not own active replay".to_string())
            .expect("valid response");
        let error = empty_when_replay_session_success(response).expect_err("conflict should be an error");

        assert_eq!(error.to_string(), "session does not own active replay");
    }

    #[test]
    fn dogstatsd_contexts_authorization_preserves_raw_token_and_is_sensitive() {
        let authorization = authorization_from_token_file_contents(b" token with spaces ", Path::new("token-file"))
            .expect("visible token bytes should be accepted");

        assert_eq!(authorization, "Bearer  token with spaces ");
        assert!(authorization.is_sensitive());
    }

    #[test]
    fn dogstatsd_contexts_authorization_rejects_empty_or_invalid_token_without_exposing_it() {
        let path = Path::new("/private/auth_token");
        let empty_error = authorization_from_token_file_contents(b"", path).expect_err("empty token must fail");
        assert!(empty_error.to_string().contains("/private/auth_token"));

        let invalid_error =
            authorization_from_token_file_contents(b"secret\ntoken", path).expect_err("newline in token must fail");
        let message = invalid_error.to_string();
        assert!(message.contains("/private/auth_token"));
        assert!(!message.contains("secret"));
    }

    #[test]
    fn dogstatsd_contexts_request_is_authenticated_empty_post() {
        let mut auth = HeaderValue::from_static("Bearer exact-token");
        auth.set_sensitive(true);
        let uri = Uri::from_static("https://127.0.0.1:5101/agent/dogstatsd-contexts-dump");

        let request = build_dogstatsd_contexts_dump_request(uri, &auth);

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().path(), CONTEXT_DUMP_ROUTE);
        assert!(request.body().is_empty());
        let request_auth = request.headers().get(AUTHORIZATION).expect("authorization header");
        assert_eq!(request_auth, "Bearer exact-token");
        assert!(request_auth.is_sensitive());
    }

    #[tokio::test]
    async fn dogstatsd_contexts_unauthenticated_client_errors_before_network() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let client = saluki_io::net::client::http::HttpClient::builder()
            .with_tls_config(|builder| builder.danger_accept_invalid_certs())
            .build()
            .expect("test client should build");
        let mut client = DataPlaneAPIClient {
            client,
            authority: "unresolvable.invalid:1".to_string(),
            authorization: None,
        };

        let error = client
            .dogstatsd_contexts_dump()
            .await
            .expect_err("an unauthenticated client must be rejected");

        assert!(error.to_string().contains("authenticated"));
    }

    #[test]
    fn dogstatsd_contexts_success_decodes_json_path() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(r#""/var/run/datadog/dogstatsd_contexts.json.zstd""#.to_string())
            .expect("valid response");

        let path = path_when_context_dump_success(response).expect("successful response should decode");

        assert_eq!(path, PathBuf::from("/var/run/datadog/dogstatsd_contexts.json.zstd"));
    }

    #[test]
    fn dogstatsd_contexts_malformed_success_is_actionable() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .body("not-json".to_string())
            .expect("valid response");

        let error = path_when_context_dump_success(response).expect_err("malformed JSON should fail");

        assert!(error.to_string().contains("DogStatsD context dump path"));
    }

    #[test]
    fn dogstatsd_contexts_unauthorized_error_is_safe() {
        let response = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body("rejected Bearer exact-token".to_string())
            .expect("valid response");

        let error = path_when_context_dump_success(response).expect_err("unauthorized response should fail");
        let message = error.to_string();

        assert!(message.contains("authentication"));
        assert!(message.contains("401"));
        assert!(!message.contains("rejected"));
        assert!(!message.contains("exact-token"));
    }

    #[test]
    fn dogstatsd_contexts_server_errors_include_status_and_body() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let response = Response::builder()
                .status(status)
                .body("dump unavailable".to_string())
                .expect("valid response");

            let error = path_when_context_dump_success(response).expect_err("server error should fail");
            let message = error.to_string();

            assert!(message.contains(status.as_str()), "missing status in: {message}");
            assert!(
                message.contains("dump unavailable"),
                "missing response body in: {message}"
            );
        }
    }

    fn write_generated_ipc_certificate(path: &std::path::Path) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_string()]).expect("certificate generation should succeed");
        std::fs::write(path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
            .expect("certificate file should be written");
    }

    async fn start_context_dump_tls_server(
        certificate_path: &std::path::Path,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>>,
    ) {
        let server_config = build_ipc_server_tls_config(certificate_path)
            .await
            .expect("server TLS configuration should build");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener.local_addr().expect("test listener should have an address");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(error) => return Ok(Some(error.to_string())),
            };
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request)?;
            assert!(request.starts_with("POST /agent/dogstatsd-contexts-dump HTTP/1.1\r\n"));
            assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("authorization: Bearer exact-token")),
                "request must contain the exact bearer header"
            );

            let body = r#""/tmp/dogstatsd_contexts.json.zstd""#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await?;
            Ok(None)
        });

        (address, task)
    }

    fn authenticated_test_client(
        address: std::net::SocketAddr, tls_config: rustls::ClientConfig,
    ) -> DataPlaneAPIClient {
        let mut authorization = HeaderValue::from_static("Bearer exact-token");
        authorization.set_sensitive(true);
        let client = saluki_io::net::client::http::HttpClient::builder()
            .with_client_tls_config(tls_config)
            .with_connect_timeout(Duration::from_secs(2))
            .without_request_timeout()
            .build()
            .expect("test HTTP client should build");
        DataPlaneAPIClient {
            client,
            authority: format!("localhost:{}", address.port()),
            authorization: Some(authorization),
        }
    }

    #[tokio::test]
    async fn dogstatsd_contexts_certificate_pinning_accepts_match_and_rejects_mismatch() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let _ = saluki_tls::initialize_default_crypto_provider();
            let directory = tempfile::tempdir().expect("temporary directory should be created");
            let certificate_a = directory.path().join("ipc-a.pem");
            let certificate_b = directory.path().join("ipc-b.pem");
            write_generated_ipc_certificate(&certificate_a);
            write_generated_ipc_certificate(&certificate_b);

            let (matching_address, matching_server) = start_context_dump_tls_server(&certificate_a).await;
            let matching_tls = build_ipc_client_ipc_tls_config(&certificate_a)
                .await
                .expect("matching client TLS configuration should build");
            let mut matching_client = authenticated_test_client(matching_address, matching_tls);
            let path = matching_client
                .dogstatsd_contexts_dump()
                .await
                .expect("matching pinned certificate should succeed");
            assert_eq!(path, PathBuf::from("/tmp/dogstatsd_contexts.json.zstd"));
            assert!(matching_server
                .await
                .expect("matching server task should join")
                .expect("matching server should complete")
                .is_none());

            let (mismatched_address, mismatched_server) = start_context_dump_tls_server(&certificate_a).await;
            let mismatched_tls = build_ipc_client_ipc_tls_config(&certificate_b)
                .await
                .expect("mismatched client TLS configuration should build");
            let mut mismatched_client = authenticated_test_client(mismatched_address, mismatched_tls);
            let error = mismatched_client
                .dogstatsd_contexts_dump()
                .await
                .expect_err("mismatched pinned certificate must fail");
            let chain = format!("{error:#}").to_ascii_lowercase();
            assert!(
                chain.contains("certificate") || chain.contains("unknownissuer") || chain.contains("unknown issuer"),
                "expected a certificate verification failure, got: {error:#}"
            );
            assert!(
                mismatched_server
                    .await
                    .expect("mismatched server task should join")
                    .expect("mismatched server should complete")
                    .is_some(),
                "the HTTP server must not receive a request after TLS verification fails"
            );
        })
        .await
        .expect("certificate pinning test should finish within its timeout");
    }
}
