//! Output configuration.

use serde::Deserialize;

/// Configuration for the OTLP output.
#[derive(Clone, Debug, Deserialize)]
pub struct OutputConfig {
    /// The OTLP endpoint URL (e.g., "grpc://localhost:4317").
    pub endpoint: String,

    /// Number of telemetry items to batch before sending.
    ///
    /// Note: Currently unused but reserved for future batching optimizations.
    #[serde(default = "default_batch_size")]
    #[allow(dead_code)]
    pub batch_size: usize,
}

fn default_batch_size() -> usize {
    100
}

impl OutputConfig {
    /// Parses the endpoint and returns the host:port for gRPC connection.
    ///
    /// Expects format: "grpc://host:port" or just "host:port"
    pub fn grpc_endpoint(&self) -> String {
        let endpoint = self.endpoint.trim();

        if let Some(rest) = endpoint.strip_prefix("grpc://") {
            format!("http://{}", rest)
        } else if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_string()
        } else {
            // Assume it's just host:port
            format!("http://{}", endpoint)
        }
    }
}
