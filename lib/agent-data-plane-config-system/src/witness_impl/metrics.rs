use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "log_payloads" => {
            translator.native.components.metrics.datadog_encoder.log_payloads = bool_value(value);

            None
        }
        "serializer_compressor_kind" => {
            translator.native.components.metrics.datadog_encoder.compressor_kind = string_value(value);

            None
        }
        "serializer_max_payload_size" => {
            translator.native.components.metrics.datadog_encoder.max_payload_size = usize_value(value, 2_621_440);

            None
        }
        "serializer_max_series_payload_size" => {
            translator
                .native
                .components
                .metrics
                .datadog_encoder
                .max_series_payload_size = usize_value(value, 512_000);

            None
        }
        "serializer_max_series_points_per_payload" => {
            translator
                .native
                .components
                .metrics
                .datadog_encoder
                .max_series_points_per_payload = usize_value(value, 10_000);

            None
        }
        "serializer_max_series_uncompressed_payload_size" => {
            translator
                .native
                .components
                .metrics
                .datadog_encoder
                .max_series_uncompressed_payload_size = usize_value(value, 5_242_880);

            None
        }
        "serializer_max_uncompressed_payload_size" => {
            translator
                .native
                .components
                .metrics
                .datadog_encoder
                .max_uncompressed_payload_size = usize_value(value, 4_194_304);

            None
        }
        "serializer_zstd_compressor_level" => {
            translator.native.components.metrics.datadog_encoder.compression_level = i32_value(value, 3);

            None
        }
        "use_v2_api.series" => {
            translator.native.components.metrics.datadog_encoder.use_v2_api_series = bool_value(value);

            None
        }
        "multi_region_failover.enabled" => {
            translator.native.components.metrics.multi_region_failover.enabled = bool_value(value);

            None
        }
        "multi_region_failover.failover_metrics" => {
            translator
                .native
                .components
                .metrics
                .multi_region_failover
                .failover_metrics = bool_value(value);

            None
        }
        "multi_region_failover.api_key" => {
            translator.native.components.metrics.multi_region_failover.api_key = Some(string_value(value));

            None
        }
        "multi_region_failover.dd_url" => {
            translator.native.components.metrics.multi_region_failover.endpoint = Some(string_value(value));

            None
        }
        "multi_region_failover.site" => {
            let site = string_value(value);
            if !site.is_empty() {
                translator.native.components.metrics.multi_region_failover.endpoint =
                    Some(format!("https://app.mrf.{site}"));
            }

            None
        }
        "multi_region_failover.metric_allowlist" => {
            translator
                .native
                .components
                .metrics
                .multi_region_failover
                .metric_allowlist = string_vec_value(value);

            None
        }
        _ => Some(value),
    }
}
