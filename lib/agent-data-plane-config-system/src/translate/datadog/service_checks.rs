//! Service-checks-domain translation: the Datadog service checks encoder's serializer limits and
//! compression.
//!
//! Mirrors the original `DatadogServiceChecksConfiguration::from_configuration`. Payload limits are
//! stored verbatim; the original clamps them to safe ceilings, which remains a component concern.

use saluki_component_config::service_checks::DatadogServiceChecksConfig;

/// `serializer_max_payload_size` -> max compressed service-check payload size.
pub fn set_max_payload_size(config: &mut DatadogServiceChecksConfig, value: i64) {
    config.max_payload_size = value.max(0) as usize;
}

/// `serializer_max_uncompressed_payload_size` -> max uncompressed service-check payload size.
pub fn set_max_uncompressed_payload_size(config: &mut DatadogServiceChecksConfig, value: i64) {
    config.max_uncompressed_payload_size = value.max(0) as usize;
}

/// `serializer_compressor_kind` -> compression algorithm.
pub fn set_compressor_kind(config: &mut DatadogServiceChecksConfig, value: String) {
    config.compressor_kind = value;
}

/// `serializer_zstd_compressor_level` -> zstd compression level.
pub fn set_zstd_compressor_level(config: &mut DatadogServiceChecksConfig, value: i64) {
    config.zstd_compressor_level = value as i32;
}

/// `log_payloads` -> whether encoded service-check payloads are logged.
pub fn set_log_payloads(config: &mut DatadogServiceChecksConfig, value: bool) {
    config.log_payloads = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_level_casts_to_i32() {
        let mut c = DatadogServiceChecksConfig::default();
        set_zstd_compressor_level(&mut c, 6);
        assert_eq!(c.zstd_compressor_level, 6);
    }
}
