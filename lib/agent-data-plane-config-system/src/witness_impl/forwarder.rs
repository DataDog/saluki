use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "additional_endpoints" => {
            translator.consume_additional_endpoints_value(value);
            None
        }
        "allow_arbitrary_tags" => {
            translator.native.components.forwarder.datadog.allow_arbitrary_tags = bool_value(value);
            None
        }
        "api_key" => {
            translator.api_key = string_value(value);
            None
        }
        "dd_url" => {
            translator.dd_url = string_value(value);
            None
        }
        "forwarder_apikey_validation_interval" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .api_key_validation_interval_mins = i64_value(value, 60);

            None
        }
        "forwarder_backoff_base" => {
            translator.native.components.forwarder.datadog.retry.backoff_base_secs = f64_value(value, 2.0);

            None
        }
        "forwarder_backoff_factor" => {
            translator.native.components.forwarder.datadog.retry.backoff_factor = f64_value(value, 2.0);

            None
        }
        "forwarder_backoff_max" => {
            translator.native.components.forwarder.datadog.retry.backoff_max_secs = f64_value(value, 64.0);

            None
        }
        "forwarder_connection_reset_interval" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .connection_reset_interval_secs = u64_value(value, 0);

            None
        }
        "forwarder_flush_to_disk_mem_ratio" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .flush_to_disk_mem_ratio = f64_value(value, 0.5);

            None
        }
        "forwarder_high_prio_buffer_size" => {
            translator.native.components.forwarder.datadog.endpoint_buffer_size = usize_value(value, 100);

            None
        }
        "forwarder_http_protocol" => {
            translator.native.components.forwarder.datadog.http_protocol = string_value(value);

            None
        }
        "forwarder_max_concurrent_requests" => {
            translator.native.components.forwarder.datadog.endpoint_concurrency = usize_value(value, 10);

            None
        }
        "forwarder_num_workers" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .endpoint_concurrency_multiplier = usize_value(value, 1);

            None
        }
        "forwarder_outdated_file_in_days" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .outdated_file_in_days = u32_value(value, 10);

            None
        }
        "forwarder_recovery_interval" => {
            translator.native.components.forwarder.datadog.retry.recovery_interval = u32_value(value, 2);

            None
        }
        "forwarder_recovery_reset" => {
            translator.native.components.forwarder.datadog.retry.recovery_reset = bool_value(value);

            None
        }
        "forwarder_retry_queue_capacity_time_interval_sec" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .capacity_time_interval_secs = u64_value(value, 15 * 60);

            None
        }
        "forwarder_retry_queue_max_size" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .retry_queue_max_size_bytes = Some(u64_value(value, 0));

            None
        }
        "forwarder_retry_queue_payloads_max_size" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .retry_queue_payloads_max_size_bytes = Some(u64_value(value, 15 * 1024 * 1024));

            None
        }
        "forwarder_storage_max_disk_ratio" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .retry
                .storage_max_disk_ratio = f64_value(value, 0.8);

            None
        }
        "forwarder_storage_max_size_in_bytes" => {
            translator.native.components.forwarder.datadog.retry.max_disk_size_bytes = u64_value(value, 0);

            None
        }
        "forwarder_storage_path" => {
            translator.native.components.forwarder.datadog.retry.storage_path = string_value(value);

            None
        }
        "forwarder_timeout" => {
            translator.native.components.forwarder.datadog.request_timeout_millis =
                u64_value(value, 20).saturating_mul(1000);

            None
        }
        "min_tls_version" => {
            translator.native.components.forwarder.datadog.min_tls_version = string_value(value);

            None
        }
        "no_proxy_nonexact_match" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .proxy
                .no_proxy_nonexact_match = bool_value(value);

            None
        }
        "observability_pipelines_worker.metrics.enabled" => {
            translator.native.components.forwarder.datadog.opw_metrics.opw_enabled = bool_value(value);

            None
        }
        "observability_pipelines_worker.metrics.url" => {
            translator.native.components.forwarder.datadog.opw_metrics.opw_url = string_value(value);

            None
        }
        "proxy.http" => {
            translator.native.components.forwarder.datadog.proxy.http = optional_string_value(value);

            None
        }
        "proxy.https" => {
            translator.native.components.forwarder.datadog.proxy.https = optional_string_value(value);

            None
        }
        "proxy.no_proxy" => {
            translator.native.components.forwarder.datadog.proxy.no_proxy = string_vec_value(value);

            None
        }
        "skip_ssl_validation" => {
            translator.native.components.forwarder.datadog.tls.verify = !bool_value(value);

            None
        }
        "sslkeylogfile" => {
            translator.native.components.forwarder.datadog.ssl_key_log_file = string_value(value);

            None
        }
        "use_proxy_for_cloud_metadata" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .proxy
                .use_proxy_for_cloud_metadata = bool_value(value);

            None
        }
        "vector.metrics.enabled" => {
            translator
                .native
                .components
                .forwarder
                .datadog
                .opw_metrics
                .vector_enabled = bool_value(value);

            None
        }
        "vector.metrics.url" => {
            translator.native.components.forwarder.datadog.opw_metrics.vector_url = string_value(value);

            None
        }
        "url" => {
            translator.dd_url = string_value(value);
            None
        }
        "site" => {
            let site = string_value(value);
            if !site.is_empty() {
                translator.dd_url = format!("https://app.{site}");
            }

            None
        }
        _ => Some(value),
    }
}
