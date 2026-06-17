//! Component-native configuration for the Datadog logs encoder.
//!
//! Mirrors `DatadogLogsConfiguration` in `saluki-components` with source key names and the
//! `Deserialize` impl stripped.

/// Configuration for the Datadog logs encoder component.
///
/// Mirrors `DatadogLogsConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogLogsConfig {
    /// The compression algorithm to use for outgoing payloads.
    ///
    /// Defaults to `zstd`.
    pub compressor_kind: String,

    /// The zstd compression level to use when `compressor_kind` is `zstd`.
    pub zstd_compressor_level: i32,
}

impl Default for DatadogLogsConfig {
    fn default() -> Self {
        Self {
            compressor_kind: "zstd".to_string(),
            zstd_compressor_level: 3,
        }
    }
}
