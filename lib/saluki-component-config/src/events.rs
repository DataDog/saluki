//! Component-native configuration for the Datadog events encoder.
//!
//! Mirrors `DatadogEventsConfiguration` in `saluki-components` with source key names and the
//! `Deserialize` impl stripped.

/// Configuration for the Datadog events encoder component.
///
/// Mirrors `DatadogEventsConfiguration` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogEventsConfig {
    /// Maximum compressed payload size, in bytes.
    pub max_payload_size: usize,

    /// Maximum uncompressed payload size, in bytes.
    pub max_uncompressed_payload_size: usize,

    /// The compression algorithm to use for outgoing payloads.
    ///
    /// Defaults to `zstd`.
    pub compressor_kind: String,

    /// The zstd compression level to use when `compressor_kind` is `zstd`.
    pub zstd_compressor_level: i32,

    /// Whether to log encoded payloads.
    ///
    /// Defaults to `false`.
    pub log_payloads: bool,
}

impl Default for DatadogEventsConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            compressor_kind: "zstd".to_string(),
            zstd_compressor_level: 3,
            log_payloads: false,
        }
    }
}
