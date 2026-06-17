//! Component-native configuration for the Datadog metrics encoder.
//!
//! Mirrors `DatadogMetricsConfiguration` in `saluki-components` with source key names and the
//! `Deserialize` impl stripped. The injected `additional_tags` ([`SharedTagSet`]) field is excluded
//! as runtime-injected state.
//!
//! [`SharedTagSet`]: saluki_context::tags::SharedTagSet

/// Configuration for the Datadog metrics encoder component.
///
/// Mirrors `DatadogMetricsConfiguration` in `saluki-components`. The `additional_tags`
/// (`Option<SharedTagSet>`) field of the original is excluded because it is injected at runtime, not
/// configured.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogMetricsConfig {
    /// Maximum number of metrics per payload.
    pub max_metrics_per_payload: usize,

    /// Maximum compressed payload size, in bytes.
    pub max_payload_size: usize,

    /// Maximum uncompressed payload size, in bytes.
    pub max_uncompressed_payload_size: usize,

    /// Maximum compressed series payload size, in bytes.
    pub max_series_payload_size: usize,

    /// Maximum uncompressed series payload size, in bytes.
    pub max_series_uncompressed_payload_size: usize,

    /// Maximum number of series points per payload.
    pub max_series_points_per_payload: usize,

    /// The flush timeout, in seconds.
    pub flush_timeout_secs: u64,

    /// The compression algorithm to use for outgoing payloads.
    ///
    /// Defaults to `zstd`.
    pub compressor_kind: String,

    /// The zstd compression level to use when `compressor_kind` is `zstd`.
    pub zstd_compressor_level: i32,

    /// Whether to use the v2 series API.
    pub use_v2_api_series: bool,

    /// Whether to log encoded payloads.
    ///
    /// Defaults to `false`.
    pub log_payloads: bool,
}

impl Default for DatadogMetricsConfig {
    fn default() -> Self {
        Self {
            max_metrics_per_payload: 10_000,
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            flush_timeout_secs: 2,
            compressor_kind: "zstd".to_string(),
            zstd_compressor_level: 3,
            use_v2_api_series: true,
            log_payloads: false,
        }
    }
}
