//! Logs-domain translation: the Datadog logs encoder's compression settings.
//!
//! Mirrors the original `DatadogLogsConfiguration::from_configuration`, which reads only the
//! compression keys (its payload limits are hardcoded).

use saluki_component_config::logs::DatadogLogsConfig;

/// `serializer_compressor_kind` -> compression algorithm.
pub fn set_compressor_kind(config: &mut DatadogLogsConfig, value: String) {
    config.compressor_kind = value;
}

/// `serializer_zstd_compressor_level` -> zstd compression level.
pub fn set_zstd_compressor_level(config: &mut DatadogLogsConfig, value: i64) {
    config.zstd_compressor_level = value as i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_kind_assigns() {
        let mut c = DatadogLogsConfig::default();
        set_compressor_kind(&mut c, "gzip".to_string());
        assert_eq!(c.compressor_kind, "gzip");
    }
}
