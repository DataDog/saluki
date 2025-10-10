use http::StatusCode;
use reqwest::Client;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;

use crate::internal::ControlPlaneConfiguration;

/// Typed API client for interacting with the control plane APIs exposed by ADP.
pub struct ControlPlaneAPIClient {
    privileged_api_client: Client,
    privileged_api_base_url: String,
}

impl ControlPlaneAPIClient {
    /// Creates a new `ControlPlaneClient` from the given generic configuration.
    ///
    /// # Errors
    ///
    /// If the control plane configuration can't be deserialized, or the control plane API endpoints cannot be
    /// determined, an error will be returned.
    pub fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let control_plane_config = ControlPlaneConfiguration::from_config(config)?;

        let (privileged_api_client, privileged_api_base_url) = {
            // Figure out if we're dealing with a TCP or Unix domain socket, which informs how we generate the base URL
            // and how we configure the HTTP client.
            let listen_address = control_plane_config.secure_api_listen_address;
            let (base_url, maybe_unix_socket_path) = match &listen_address {
                ListenAddress::Tcp(_) => {
                    let local_address = listen_address
                        .as_local_connect_addr()
                        .expect("should get local address for TCP");
                    (format!("https://{}", local_address), None)
                }

                #[cfg(unix)]
                ListenAddress::Unix(path) => {
                    // For UDS, we have to configure `reqwest` separately to override the host in the URL and use a Unix
                    // socket instead, which means we just use a dummy host in the base URL since it gets ignored in
                    // this case.
                    ("https://127.0.0.1".to_string(), Some(path.as_path()))
                }

                _ => {
                    return Err(generic_error!(
                        "Expected connection-oriented address (TCP or UDS stream) for privileged API endpoint: {}",
                        listen_address
                    ))
                }
            };

            let mut builder = reqwest::Client::builder().danger_accept_invalid_certs(true);
            if let Some(path) = maybe_unix_socket_path {
                builder = builder.unix_socket(path);
            }

            let client = builder
                .build()
                .error_context("Failed to construct API client for privileged API endpoint.")?;

            (client, base_url)
        };

        Ok(Self {
            privileged_api_client,
            privileged_api_base_url,
        })
    }

    fn get_privileged_url(&self, path: &str) -> String {
        format!("{}{}", self.privileged_api_base_url, path)
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
    pub async fn set_log_level(&self, filter_directives: String, duration_secs: u64) -> Result<(), GenericError> {
        let url = self.get_privileged_url("/logging/override");
        let response = self
            .privileged_api_client
            .post(url)
            .query(&[("time_secs", duration_secs)])
            .body(filter_directives)
            .send()
            .await?;

        let _ = process_response(response).await?;
        Ok(())
    }

    /// Resets the log level for the process.
    ///
    /// This can be used to proactively disable a previous log level override.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn reset_log_level(&self) -> Result<(), GenericError> {
        let url = self.get_privileged_url("/logging/reset");
        let response = self.privileged_api_client.post(url).send().await?;

        let _ = process_response(response).await?;
        Ok(())
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
    pub async fn set_metric_level(&self, level: String, duration_secs: u64) -> Result<(), GenericError> {
        let url = self.get_privileged_url("/metrics/override");
        let response = self
            .privileged_api_client
            .post(url)
            .query(&[("time_secs", duration_secs)])
            .body(level)
            .send()
            .await?;

        let _ = process_response(response).await?;
        Ok(())
    }

    /// Resets the metric level for the process.
    ///
    /// This can be used to proactively disable a previous metric level override.
    ///
    /// # Errors
    ///
    /// If the request fails, or the server responds with an unexpected status code, an error is returned.
    pub async fn reset_metric_level(&self) -> Result<(), GenericError> {
        let url = self.get_privileged_url("/metrics/reset");
        let response = self.privileged_api_client.post(url).send().await?;

        let _ = process_response(response).await?;
        Ok(())
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
    pub async fn dogstatsd_stats(&self, collection_duration_secs: u64) -> Result<String, GenericError> {
        let url = self.get_privileged_url("/dogstatsd/stats");
        let response = self
            .privileged_api_client
            .get(url)
            .query(&[("collection_duration_secs", collection_duration_secs)])
            .send()
            .await?;

        let response = process_response(response).await?;
        let response_body = response.text().await.error_context("Failed to read response body.")?;
        Ok(response_body)
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
    pub async fn config(&self) -> Result<String, GenericError> {
        let url = self.get_privileged_url("/config");
        let response = self.privileged_api_client.get(url).send().await?;

        let response = process_response(response).await?;
        let response_body = response.text().await.error_context("Failed to read response body.")?;
        Ok(response_body)
    }

    /// Retrieves the tags from the workload provider.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider is not
    /// configured, an error is returned.
    pub async fn workload_tags(&self) -> Result<String, GenericError> {
        let url = self.get_privileged_url("/workload/remote_agent/tags/dump");
        let response = self.privileged_api_client.get(url).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(generic_error!("Workload provider not configured: no tags available."));
        }

        let response = process_response(response).await?;
        let response_body = response.text().await.error_context("Failed to read response body.")?;
        Ok(response_body)
    }

    /// Retrieves the External Data entries from the workload provider.
    ///
    /// The response body is returned as a plain string with no decoding or modification performed.
    ///
    /// # Errors
    ///
    /// If the request fails, or if the server responds with an unexpected status code, or if a workload provider is not
    /// configured, an error is returned.
    pub async fn workload_external_data(&self) -> Result<String, GenericError> {
        let url = self.get_privileged_url("/workload/remote_agent/external_data/dump");
        let response = self.privileged_api_client.get(url).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(generic_error!("Workload provider not configured: no tags available."));
        }

        let response = process_response(response).await?;
        let response_body = response.text().await.error_context("Failed to read response body.")?;
        Ok(response_body)
    }
}

async fn process_response(response: reqwest::Response) -> Result<reqwest::Response, GenericError> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        let body = response.text().await.unwrap_or_else(|_| String::from("<no body>"));
        Err(generic_error!("Received non-success response ({}): {}.", status, body))
    }
}
