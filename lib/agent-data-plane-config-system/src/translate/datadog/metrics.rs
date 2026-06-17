//! Metrics-domain translation: the Datadog metrics encoder's serializer limits and compression
//! settings.
//!
//! Mirrors the conversions the original `DatadogMetricsConfiguration::from_configuration` performed.
//! The original clamps the configured payload limits to safe ceilings lazily; the leaf struct stores
//! the configured value verbatim and clamping remains a component concern. `max_metrics_per_payload`
//! and `flush_timeout_secs` have no Datadog witness key -- they are Saluki-schema-only and seeded.

use saluki_component_config::metrics::DatadogMetricsConfig;

/// `serializer_max_series_points_per_payload` -> max series points per payload.
pub fn set_max_series_points_per_payload(config: &mut DatadogMetricsConfig, value: i64) {
    config.max_series_points_per_payload = value.max(0) as usize;
}

/// `serializer_max_payload_size` -> max compressed payload size.
pub fn set_max_payload_size(config: &mut DatadogMetricsConfig, value: i64) {
    config.max_payload_size = value.max(0) as usize;
}

/// `serializer_max_uncompressed_payload_size` -> max uncompressed payload size.
pub fn set_max_uncompressed_payload_size(config: &mut DatadogMetricsConfig, value: i64) {
    config.max_uncompressed_payload_size = value.max(0) as usize;
}

/// `serializer_max_series_payload_size` -> max compressed series payload size.
pub fn set_max_series_payload_size(config: &mut DatadogMetricsConfig, value: i64) {
    config.max_series_payload_size = value.max(0) as usize;
}

/// `serializer_max_series_uncompressed_payload_size` -> max uncompressed series payload size.
pub fn set_max_series_uncompressed_payload_size(config: &mut DatadogMetricsConfig, value: i64) {
    config.max_series_uncompressed_payload_size = value.max(0) as usize;
}

/// `serializer_compressor_kind` -> compression algorithm.
pub fn set_compressor_kind(config: &mut DatadogMetricsConfig, value: String) {
    config.compressor_kind = value;
}

/// `serializer_zstd_compressor_level` -> zstd compression level.
pub fn set_zstd_compressor_level(config: &mut DatadogMetricsConfig, value: i64) {
    config.zstd_compressor_level = value as i32;
}

/// `use_v2_api.series` -> whether the v2 series API is used.
pub fn set_use_v2_api_series(config: &mut DatadogMetricsConfig, value: bool) {
    config.use_v2_api_series = value;
}

/// `log_payloads` -> whether encoded metric payloads are logged.
pub fn set_log_payloads(config: &mut DatadogMetricsConfig, value: bool) {
    config.log_payloads = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_points_casts_to_usize() {
        let mut c = DatadogMetricsConfig::default();
        set_max_series_points_per_payload(&mut c, 5000);
        assert_eq!(c.max_series_points_per_payload, 5000);
    }
}
