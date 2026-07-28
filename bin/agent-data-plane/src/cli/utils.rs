use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_client_ipc_tls_config};
use futures::TryFutureExt as _;
use http::{header::CONTENT_TYPE, uri::PathAndQuery, Request, Response, StatusCode, Uri};
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
use saluki_io::net::{client::http::HttpClient, ListenAddress};
use serde::{Deserialize, Serialize};

use crate::config::DataPlaneConfiguration;

/// Typed API client for interacting with the APIs exposed by ADP.
pub struct DataPlaneAPIClient {
    client: HttpClient,
    authority: String,
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
    /// If the data plane or IPC authentication configuration can't be loaded, the IPC certificate can't be read or
    /// parsed into a client TLS configuration, the privileged API endpoint isn't connection-oriented, or the HTTP
    /// client can't be constructed, an error is returned.
    pub async fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let dp_config = DataPlaneConfiguration::from_configuration(config)
            .error_context("Failed to load data plane configuration for privileged API client.")?;
        let ipc_config = IpcAuthConfiguration::from_configuration(config)
            .error_context("Failed to load IPC authentication configuration for privileged API client.")?;
        let ipc_cert_file_path = ipc_config.ipc_cert_file_path();
        let client_tls_config = build_ipc_client_ipc_tls_config(&ipc_cert_file_path)
            .await
            .with_error_context(|| {
                format!(
                    "Failed to load IPC TLS certificate and construct client TLS configuration from '{}'.",
                    ipc_cert_file_path.display()
                )
            })?;

        let listen_address = dp_config.secure_api_listen_address();
        let builder = HttpClient::builder().with_client_tls_config(client_tls_config);

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
            .error_context("Failed to construct mTLS API client for privileged API endpoint.")?;

        Ok(Self { client, authority })
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
    use std::{convert::Infallible, fs, time::Duration};

    use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
    use http::{Request, Response, StatusCode};
    use http_body_util::Full;
    use hyper::{body::Bytes, service::service_fn};
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use saluki_config::config_from;
    use saluki_io::net::{listener::ConnectionOrientedListener, server::http::HttpServer, ListenAddress};
    use serde_json::json;
    use tokio::time::timeout;

    use super::{body_when_capture_success, DataPlaneAPIClient};
    #[cfg(target_os = "linux")]
    use super::{body_when_replay_session_success, empty_when_replay_session_success};

    #[tokio::test]
    async fn from_config_authenticates_to_privileged_api_with_configured_ipc_identity() {
        let _ = saluki_tls::initialize_default_crypto_provider();

        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()])
            .expect("self-signed localhost certificate should be generated");
        let temp_dir = tempfile::tempdir().expect("temporary certificate directory should be created");
        let cert_path = temp_dir.path().join("ipc-cert.pem");
        fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
            .expect("certificate and private key should be written");

        let listener = ConnectionOrientedListener::from_listen_address(
            ListenAddress::try_from("tcp://127.0.0.1:0").expect("ephemeral TCP address should parse"),
        )
        .await
        .expect("privileged API listener should bind");
        let listen_addr = listener.local_addr().expect("listener should have a local address");
        let server_tls_config = build_ipc_server_tls_config(&cert_path)
            .await
            .expect("production IPC server TLS config should build");
        let (request_tx, mut request_rx) = tokio::sync::mpsc::channel(1);
        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
            let request_tx = request_tx.clone();
            async move {
                request_tx
                    .send(request.uri().path().to_string())
                    .await
                    .expect("test should receive the request path");
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                    b"{\"source\":\"privileged-api\"}",
                ))))
            }
        });
        let (server_shutdown, error_handle) = HttpServer::from_listener(listener, service)
            .with_tls_config(server_tls_config)
            .listen();

        let config = config_from(json!({
            "ipc_cert_file_path": cert_path,
            "data_plane": {
                "secure_api_listen_address": format!("tcp://{listen_addr}"),
            },
        }))
        .await;
        let mut api_client = DataPlaneAPIClient::from_config(&config)
            .await
            .expect("privileged API client should build from configuration");

        let body = timeout(Duration::from_secs(5), api_client.config())
            .await
            .expect("privileged API request should not time out")
            .expect("privileged API request should succeed");
        let request_path = timeout(Duration::from_secs(5), request_rx.recv())
            .await
            .expect("privileged API handler should be reached before timeout")
            .expect("privileged API server should report the request path");

        assert_eq!(body, "{\"source\":\"privileged-api\"}");
        assert_eq!(request_path, "/config");

        timeout(Duration::from_secs(5), server_shutdown.shutdown_and_wait())
            .await
            .expect("privileged API server should shut down before timeout");
        let server_error = timeout(Duration::from_secs(5), error_handle)
            .await
            .expect("privileged API error handle should resolve before timeout");
        assert!(server_error.is_none(), "privileged API server failed: {server_error:?}");
    }

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
}
