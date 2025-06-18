use http::StatusCode;
use reqwest::Client;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

const PRIVILEGED_API_BASE_URL: &str = "https://localhost:5101";

/// Typed API client for interacting with the privileged and unprivileged APIs exposed by ADP.
pub struct APIClient {
    inner: Client,
}

impl APIClient {
    /// Creates a new `APIClient`.
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        Self { inner }
    }

    fn get_privileged_url(&self, path: &str) -> String {
        format!("{}{}", PRIVILEGED_API_BASE_URL, path)
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
            .inner
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
    /// If the request fails, or if the server responds with an unexpected status code, an error is returned.
    pub async fn reset_log_level(&self) -> Result<(), GenericError> {
        let url = self.get_privileged_url("/logging/reset");
        let response = self.inner.post(url).send().await?;

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
            .inner
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
        let response = self.inner.get(url).send().await?;

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
        let response = self.inner.get(url).send().await?;
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
        let response = self.inner.get(url).send().await?;
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
