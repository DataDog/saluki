use std::{path::PathBuf, time::Duration};

use bytes::Buf as _;
use datadog_protos::agent::{AgentSecureClient, CaptureTriggerRequest};
use futures::TryFutureExt as _;
use http::{uri::PathAndQuery, Request, Response, StatusCode, Uri};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use saluki_config::GenericConfiguration;
use saluki_env::helpers::tonic::BearerAuthInterceptor;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{
    build_datadog_agent_client_ipc_tls_config,
    client::http::{HttpClient, HttpsCapableConnectorBuilder},
    get_ipc_cert_file_path, GrpcTargetAddress, ListenAddress,
};
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint},
    Code, Status,
};

use crate::{config::DataPlaneConfiguration, internal::platform::PlatformSettings};

type SecureDataPlaneService = InterceptedService<Channel, BearerAuthInterceptor>;

/// Typed API client for interacting with the APIs exposed by ADP.
pub struct DataPlaneAPIClient {
    client: HttpClient,
    authority: String,
}

/// Typed gRPC client for interacting with the secure gRPC API exposed by ADP.
pub struct DataPlaneSecureClient {
    client: AgentSecureClient<SecureDataPlaneService>,
}

impl DataPlaneAPIClient {
    /// Creates a new `DataPlaneAPIClient` from the given generic configuration.
    ///
    /// # Errors
    ///
    /// If the data plane configuration can't be deserialized, or the data plane API endpoints cannot be
    /// determined, an error will be returned.
    pub fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let dp_config = DataPlaneConfiguration::from_configuration(config)?;

        let listen_address = dp_config.secure_api_listen_address();

        let mut builder = HttpClient::builder().with_tls_config(|b| b.danger_accept_invalid_certs());

        let authority = match listen_address {
            ListenAddress::Tcp(_) => {
                let local_address = listen_address
                    .as_local_connect_addr()
                    .expect("should get local address for TCP");
                local_address.to_string()
            }

            #[cfg(unix)]
            ListenAddress::Unix(path) => {
                builder = builder.with_unix_socket_path(path);
                "127.0.0.1".to_string()
            }

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
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider is not
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
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider is not
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

impl DataPlaneSecureClient {
    /// Creates a new `DataPlaneSecureClient` from the given generic configuration.
    ///
    /// # Errors
    ///
    /// If the data plane configuration can't be deserialized, or the secure gRPC client cannot be created, an error
    /// will be returned.
    pub async fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let dp_config = DataPlaneConfiguration::from_configuration(config)?;
        let target_address = GrpcTargetAddress::try_from_listen_addr(dp_config.secure_api_listen_address())
            .ok_or_else(|| {
                generic_error!(
                    "Expected connection-oriented address (TCP or UDS stream) for privileged API endpoint: {}",
                    dp_config.secure_api_listen_address()
                )
            })?;

        let mut auth_token_file_path = config
            .try_get_typed::<PathBuf>("auth_token_file_path")
            .error_context("Failed to get Agent auth token file path.")?
            .unwrap_or_else(PlatformSettings::get_auth_token_path);
        if auth_token_file_path.as_os_str().is_empty() {
            auth_token_file_path = PlatformSettings::get_auth_token_path();
        }

        let mut ipc_cert_file_path = config
            .try_get_typed::<Option<PathBuf>>("ipc_cert_file_path")
            .error_context("Failed to get Agent IPC cert file path.")?
            .flatten();
        if ipc_cert_file_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            ipc_cert_file_path = None;
        }

        let auth_interceptor = BearerAuthInterceptor::from_file(&auth_token_file_path).await?;
        let ipc_cert_file_path = get_ipc_cert_file_path(ipc_cert_file_path.as_ref(), &auth_token_file_path);
        let client_tls_config = build_datadog_agent_client_ipc_tls_config(ipc_cert_file_path).await?;

        let mut connector_builder =
            HttpsCapableConnectorBuilder::default().with_connect_timeout(Duration::from_secs(2));
        let endpoint = match &target_address {
            GrpcTargetAddress::Tcp(addr) => Endpoint::from_shared(format!("https://{addr}"))
                .map_err(|e| generic_error!("Failed to construct ADP gRPC endpoint URI ({}).", e))?,
            #[cfg(unix)]
            GrpcTargetAddress::Unix(path) => {
                connector_builder = connector_builder.with_unix_socket_path(path);
                Endpoint::from_static("https://127.0.0.1")
            }
            #[cfg(not(unix))]
            GrpcTargetAddress::Unix(_) => {
                return Err(generic_error!(
                    "Unix domain sockets are not supported for gRPC clients on this platform."
                ))
            }
        };

        let https_connector = connector_builder.build(client_tls_config)?;
        let channel = endpoint
            .connect_timeout(Duration::from_secs(2))
            .connect_with_connector(https_connector)
            .await
            .with_error_context(|| format!("Failed to connect to Agent Data Plane API at '{}'.", target_address))?;

        Ok(Self {
            client: AgentSecureClient::new(InterceptedService::new(channel, auth_interceptor)),
        })
    }

    /// Starts a DogStatsD traffic capture through the secure gRPC API.
    ///
    /// # Errors
    ///
    /// If the RPC fails, or if ADP rejects the capture request, an error is returned.
    pub async fn dogstatsd_capture(
        &mut self, duration: &str, path: Option<&str>, compressed: bool,
    ) -> Result<String, GenericError> {
        let response = self
            .client
            .dogstatsd_capture_trigger(CaptureTriggerRequest {
                duration: duration.to_string(),
                path: path.unwrap_or_default().to_string(),
                compressed,
            })
            .await
            .map_err(map_dogstatsd_capture_error)?
            .into_inner();

        Ok(response.path)
    }
}

async fn collect_body(body: Incoming) -> Option<String> {
    let body = body.collect().await.ok()?.aggregate();
    String::from_utf8(body.chunk().to_vec()).ok()
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

fn map_dogstatsd_capture_error(error: Status) -> GenericError {
    match error.code() {
        Code::FailedPrecondition | Code::InvalidArgument => generic_error!("{}", error.message()),
        _ => generic_error!(
            "Failed to start DogStatsD capture ({}): {}.",
            error.code(),
            error.message()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::map_dogstatsd_capture_error;
    use tonic::{Code, Status};

    #[test]
    fn dogstatsd_capture_failed_precondition_surfaces_server_message() {
        let error = map_dogstatsd_capture_error(Status::new(Code::FailedPrecondition, "capture already in progress"));

        assert_eq!(error.to_string(), "capture already in progress");
    }

    #[test]
    fn dogstatsd_capture_invalid_argument_surfaces_server_message() {
        let error = map_dogstatsd_capture_error(Status::new(Code::InvalidArgument, "missing duration unit"));

        assert_eq!(error.to_string(), "missing duration unit");
    }

    #[test]
    fn dogstatsd_capture_other_errors_keep_context() {
        let error = map_dogstatsd_capture_error(Status::new(Code::Unavailable, "connection closed"));

        let message = error.to_string();
        assert!(message.contains("Failed to start DogStatsD capture"));
        assert!(message.contains("connection closed"));
    }
}
