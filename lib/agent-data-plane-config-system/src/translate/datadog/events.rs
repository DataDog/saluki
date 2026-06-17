//! Events-domain translation: the Datadog events encoder's serializer limits and compression.
//!
//! Mirrors the original `DatadogEventsConfiguration::from_configuration`. Payload limits are stored
//! verbatim; the original clamps them to safe ceilings, which remains a component concern.

use saluki_component_config::events::DatadogEventsConfig;

/// `serializer_max_payload_size` -> max compressed event payload size.
pub fn set_max_payload_size(config: &mut DatadogEventsConfig, value: i64) {
    config.max_payload_size = value.max(0) as usize;
}

/// `serializer_max_uncompressed_payload_size` -> max uncompressed event payload size.
pub fn set_max_uncompressed_payload_size(config: &mut DatadogEventsConfig, value: i64) {
    config.max_uncompressed_payload_size = value.max(0) as usize;
}

/// `serializer_compressor_kind` -> compression algorithm.
pub fn set_compressor_kind(config: &mut DatadogEventsConfig, value: String) {
    config.compressor_kind = value;
}

/// `serializer_zstd_compressor_level` -> zstd compression level.
pub fn set_zstd_compressor_level(config: &mut DatadogEventsConfig, value: i64) {
    config.zstd_compressor_level = value as i32;
}

/// `log_payloads` -> whether encoded event payloads are logged.
pub fn set_log_payloads(config: &mut DatadogEventsConfig, value: bool) {
    config.log_payloads = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_size_casts_to_usize() {
        let mut c = DatadogEventsConfig::default();
        set_max_payload_size(&mut c, 1024);
        assert_eq!(c.max_payload_size, 1024);
    }
}
