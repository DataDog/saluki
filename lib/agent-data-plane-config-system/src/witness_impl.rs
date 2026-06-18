use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::{DatadogConfigConsumer, TranslateResult};
use saluki_component_config::{EndpointConfig, ListenAddress, MetricTagFilterEntry};
use serde_json::Value;

mod control;
mod dogstatsd;
mod forwarder;
mod metrics;
mod otlp;
mod traces;
mod workload;

/// Datadog witness consumer that writes directly into the native Saluki model.
pub struct Translator {
    native: SalukiConfiguration,
    api_key: String,
    dd_url: String,
    additional_endpoints: Vec<EndpointConfig>,
    aggregator_stop_timeout_secs: u64,
    forwarder_stop_timeout_secs: u64,
    dogstatsd_bind_host: Option<String>,
    dogstatsd_non_local_traffic: bool,
}

impl Translator {
    /// Creates a translator from a seeded native model.
    pub fn new(base: SalukiConfiguration) -> Self {
        Self {
            native: base,
            api_key: String::new(),
            dd_url: "https://app.datadoghq.com".to_string(),
            additional_endpoints: Vec::new(),
            aggregator_stop_timeout_secs: 2,
            forwarder_stop_timeout_secs: 2,
            dogstatsd_bind_host: None,
            dogstatsd_non_local_traffic: false,
        }
    }

    /// Finishes translation and resolves multi-key fields.
    pub fn finish(mut self) -> SalukiConfiguration {
        let dogstatsd_host = if self.dogstatsd_non_local_traffic {
            Some("0.0.0.0")
        } else {
            self.dogstatsd_bind_host.as_deref()
        };
        if let Some(host) = dogstatsd_host {
            set_network_listen_host(&mut self.native.components.dogstatsd.source.udp_address, host);
            set_network_listen_host(&mut self.native.components.dogstatsd.source.tcp_address, host);
        }

        let mut endpoints = vec![EndpointConfig {
            url: self.dd_url,
            api_key: self.api_key,
        }];
        endpoints.extend(self.additional_endpoints);
        self.native.components.forwarder.datadog.endpoints = endpoints;
        self.native.control.stop_timeout_millis = self
            .aggregator_stop_timeout_secs
            .saturating_add(self.forwarder_stop_timeout_secs)
            .saturating_mul(1000);
        self.native
    }

    fn consume_key(&mut self, key: &str, value: Option<Value>) -> TranslateResult {
        let Some(value) = value else {
            return Ok(());
        };
        let Some(value) = control::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(value) = forwarder::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(value) = dogstatsd::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(value) = otlp::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(value) = metrics::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(value) = traces::consume_key(self, key, value) else {
            return Ok(());
        };
        let Some(_value) = workload::consume_key(self, key, value) else {
            return Ok(());
        };

        unreachable!("unhandled Datadog config witness key: {key}")
    }

    fn consume_additional_endpoints_value(&mut self, value: Value) {
        let Value::Object(map) = value else {
            return;
        };
        for (url, keys) in map {
            match keys {
                Value::Array(keys) => {
                    for key in keys {
                        self.additional_endpoints.push(EndpointConfig {
                            url: url.clone(),
                            api_key: string_value(key),
                        });
                    }
                }
                other => self.additional_endpoints.push(EndpointConfig {
                    url: url.clone(),
                    api_key: string_value(other),
                }),
            }
        }
    }
}

fn bool_value(value: Value) -> bool {
    value.as_bool().unwrap_or(false)
}

fn string_value(value: Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_default()
}

fn usize_value(value: Value, default: usize) -> usize {
    value.as_u64().and_then(|v| v.try_into().ok()).unwrap_or(default)
}

fn u64_value(value: Value, default: u64) -> u64 {
    value.as_u64().unwrap_or(default)
}

fn u16_value(value: Value, default: u16) -> u16 {
    value.as_u64().and_then(|v| v.try_into().ok()).unwrap_or(default)
}

fn u32_value(value: Value, default: u32) -> u32 {
    value.as_u64().and_then(|v| v.try_into().ok()).unwrap_or(default)
}

fn i32_value(value: Value, default: i32) -> i32 {
    value.as_i64().and_then(|v| v.try_into().ok()).unwrap_or(default)
}

fn i64_value(value: Value, default: i64) -> i64 {
    value.as_i64().unwrap_or(default)
}

fn f64_value(value: Value, default: f64) -> f64 {
    value.as_f64().unwrap_or(default)
}

fn optional_string_value(value: Value) -> Option<String> {
    let value = string_value(value);
    (!value.is_empty()).then_some(value)
}

fn set_network_listen_host(address: &mut ListenAddress, host: &str) {
    match address {
        ListenAddress::Udp(value) | ListenAddress::Tcp(value) => {
            let port = value.rsplit_once(':').map(|(_, port)| port).unwrap_or_default();
            if !port.is_empty() {
                *value = format!("{host}:{port}");
            }
        }
        ListenAddress::Disabled | ListenAddress::Unix(_) => {}
    }
}

fn array_value(value: Value) -> Vec<Value> {
    match value {
        Value::Array(values) => values,
        _ => Vec::new(),
    }
}

fn string_vec_value(value: Value) -> Vec<String> {
    match value {
        Value::Array(values) => values.into_iter().map(string_value).collect(),
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn metric_tag_filter_entries(value: Value) -> Vec<MetricTagFilterEntry> {
    match value {
        Value::Array(values) => values
            .into_iter()
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect(),
        _ => Vec::new(),
    }
}

impl DatadogConfigConsumer for Translator {
    fn consume_additional_endpoints(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("additional_endpoints", value)
    }

    fn consume_agent_ipc_grpc_max_message_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("agent_ipc.grpc_max_message_size", value)
    }

    fn consume_aggregator_stop_timeout(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("aggregator_stop_timeout", value)
    }

    fn consume_allow_arbitrary_tags(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("allow_arbitrary_tags", value)
    }

    fn consume_api_key(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("api_key", value)
    }

    fn consume_auth_token_file_path(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("auth_token_file_path", value)
    }

    fn consume_apm_config_obfuscation_credit_cards_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.credit_cards.enabled", value)
    }

    fn consume_apm_config_obfuscation_credit_cards_keep_values(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.credit_cards.keep_values", value)
    }

    fn consume_apm_config_obfuscation_credit_cards_luhn(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.credit_cards.luhn", value)
    }

    fn consume_apm_config_obfuscation_elasticsearch_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.elasticsearch.enabled", value)
    }

    fn consume_apm_config_obfuscation_elasticsearch_keep_values(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.elasticsearch.keep_values", value)
    }

    fn consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.elasticsearch.obfuscate_sql_values", value)
    }

    fn consume_apm_config_obfuscation_http_remove_paths_with_digits(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.http.remove_paths_with_digits", value)
    }

    fn consume_apm_config_obfuscation_http_remove_query_string(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.http.remove_query_string", value)
    }

    fn consume_apm_config_obfuscation_memcached_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.memcached.enabled", value)
    }

    fn consume_apm_config_obfuscation_memcached_keep_command(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.memcached.keep_command", value)
    }

    fn consume_apm_config_obfuscation_mongodb_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.mongodb.enabled", value)
    }

    fn consume_apm_config_obfuscation_mongodb_keep_values(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.mongodb.keep_values", value)
    }

    fn consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.mongodb.obfuscate_sql_values", value)
    }

    fn consume_apm_config_obfuscation_opensearch_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.opensearch.enabled", value)
    }

    fn consume_apm_config_obfuscation_opensearch_keep_values(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.opensearch.keep_values", value)
    }

    fn consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.opensearch.obfuscate_sql_values", value)
    }

    fn consume_apm_config_obfuscation_redis_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.redis.enabled", value)
    }

    fn consume_apm_config_obfuscation_redis_remove_all_args(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.redis.remove_all_args", value)
    }

    fn consume_apm_config_obfuscation_valkey_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.valkey.enabled", value)
    }

    fn consume_apm_config_obfuscation_valkey_remove_all_args(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("apm_config.obfuscation.valkey.remove_all_args", value)
    }

    fn consume_bind_host(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("bind_host", value)
    }

    fn consume_cmd_port(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("cmd_port", value)
    }

    fn consume_cri_connection_timeout(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("cri_connection_timeout", value)
    }

    fn consume_cri_query_timeout(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("cri_query_timeout", value)
    }

    fn consume_data_plane_api_listen_address(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.api_listen_address", value)
    }

    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("data_plane.dogstatsd.aggregator_tag_filter_cache_capacity", value)
    }

    fn consume_data_plane_dogstatsd_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.dogstatsd.enabled", value)
    }

    fn consume_data_plane_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.enabled", value)
    }

    fn consume_data_plane_log_file(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.log_file", value)
    }

    fn consume_data_plane_otlp_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.otlp.enabled", value)
    }

    fn consume_data_plane_otlp_proxy_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.otlp.proxy.enabled", value)
    }

    fn consume_data_plane_otlp_proxy_logs_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.otlp.proxy.logs.enabled", value)
    }

    fn consume_data_plane_otlp_proxy_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.otlp.proxy.metrics.enabled", value)
    }

    fn consume_data_plane_otlp_proxy_receiver_protocols_grpc_endpoint(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("data_plane.otlp.proxy.receiver.protocols.grpc.endpoint", value)
    }

    fn consume_data_plane_otlp_proxy_traces_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.otlp.proxy.traces.enabled", value)
    }

    fn consume_data_plane_remote_agent_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.remote_agent_enabled", value)
    }

    fn consume_data_plane_secure_api_listen_address(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.secure_api_listen_address", value)
    }

    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("data_plane.use_new_config_stream_endpoint", value)
    }

    fn consume_dd_url(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dd_url", value)
    }

    fn consume_dogstatsd_buffer_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_buffer_size", value)
    }

    fn consume_dogstatsd_capture_depth(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_capture_depth", value)
    }

    fn consume_dogstatsd_capture_path(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_capture_path", value)
    }

    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_context_expiry_seconds", value)
    }

    fn consume_dogstatsd_entity_id_precedence(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_entity_id_precedence", value)
    }

    fn consume_dogstatsd_eol_required(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_eol_required", value)
    }

    fn consume_dogstatsd_flush_incomplete_buckets(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_flush_incomplete_buckets", value)
    }

    fn consume_dogstatsd_log_file(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_log_file", value)
    }

    fn consume_dogstatsd_log_file_max_rolls(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_log_file_max_rolls", value)
    }

    fn consume_dogstatsd_log_file_max_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_log_file_max_size", value)
    }

    fn consume_dogstatsd_logging_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_logging_enabled", value)
    }

    fn consume_dogstatsd_mapper_cache_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_mapper_cache_size", value)
    }

    fn consume_dogstatsd_mapper_profiles(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_mapper_profiles", value)
    }

    fn consume_dogstatsd_metrics_stats_enable(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_metrics_stats_enable", value)
    }

    fn consume_dogstatsd_no_aggregation_pipeline(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_no_aggregation_pipeline", value)
    }

    fn consume_dogstatsd_non_local_traffic(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_non_local_traffic", value)
    }

    fn consume_dogstatsd_origin_detection(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_origin_detection", value)
    }

    fn consume_dogstatsd_origin_detection_client(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_origin_detection_client", value)
    }

    fn consume_dogstatsd_origin_optout_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_origin_optout_enabled", value)
    }

    fn consume_dogstatsd_port(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_port", value)
    }

    fn consume_dogstatsd_so_rcvbuf(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_so_rcvbuf", value)
    }

    fn consume_dogstatsd_socket(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_socket", value)
    }

    fn consume_dogstatsd_stream_log_too_big(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_stream_log_too_big", value)
    }

    fn consume_dogstatsd_stream_socket(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_stream_socket", value)
    }

    fn consume_dogstatsd_string_interner_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_string_interner_size", value)
    }

    fn consume_dogstatsd_tag_cardinality(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_tag_cardinality", value)
    }

    fn consume_dogstatsd_tags(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("dogstatsd_tags", value)
    }

    fn consume_enable_payloads_events(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("enable_payloads.events", value)
    }

    fn consume_enable_payloads_series(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("enable_payloads.series", value)
    }

    fn consume_enable_payloads_service_checks(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("enable_payloads.service_checks", value)
    }

    fn consume_enable_payloads_sketches(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("enable_payloads.sketches", value)
    }

    fn consume_env(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("env", value)
    }

    fn consume_forwarder_apikey_validation_interval(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_apikey_validation_interval", value)
    }

    fn consume_forwarder_backoff_base(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_backoff_base", value)
    }

    fn consume_forwarder_backoff_factor(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_backoff_factor", value)
    }

    fn consume_forwarder_backoff_max(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_backoff_max", value)
    }

    fn consume_forwarder_connection_reset_interval(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_connection_reset_interval", value)
    }

    fn consume_forwarder_flush_to_disk_mem_ratio(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_flush_to_disk_mem_ratio", value)
    }

    fn consume_forwarder_high_prio_buffer_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_high_prio_buffer_size", value)
    }

    fn consume_forwarder_http_protocol(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_http_protocol", value)
    }

    fn consume_forwarder_max_concurrent_requests(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_max_concurrent_requests", value)
    }

    fn consume_forwarder_num_workers(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_num_workers", value)
    }

    fn consume_forwarder_outdated_file_in_days(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_outdated_file_in_days", value)
    }

    fn consume_forwarder_recovery_interval(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_recovery_interval", value)
    }

    fn consume_forwarder_recovery_reset(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_recovery_reset", value)
    }

    fn consume_forwarder_retry_queue_capacity_time_interval_sec(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_retry_queue_capacity_time_interval_sec", value)
    }

    fn consume_forwarder_retry_queue_max_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_retry_queue_max_size", value)
    }

    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_retry_queue_payloads_max_size", value)
    }

    fn consume_forwarder_stop_timeout(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_stop_timeout", value)
    }

    fn consume_forwarder_storage_max_disk_ratio(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_storage_max_disk_ratio", value)
    }

    fn consume_forwarder_storage_max_size_in_bytes(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_storage_max_size_in_bytes", value)
    }

    fn consume_forwarder_storage_path(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_storage_path", value)
    }

    fn consume_forwarder_timeout(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("forwarder_timeout", value)
    }

    fn consume_histogram_aggregates(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("histogram_aggregates", value)
    }

    fn consume_histogram_copy_to_distribution(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("histogram_copy_to_distribution", value)
    }

    fn consume_histogram_copy_to_distribution_prefix(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("histogram_copy_to_distribution_prefix", value)
    }

    fn consume_ipc_cert_file_path(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("ipc_cert_file_path", value)
    }

    fn consume_log_format_rfc3339(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("log_format_rfc3339", value)
    }

    fn consume_log_level(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("log_level", value)
    }

    fn consume_log_payloads(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("log_payloads", value)
    }

    fn consume_metric_filterlist(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("metric_filterlist", value)
    }

    fn consume_metric_filterlist_match_prefix(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("metric_filterlist_match_prefix", value)
    }

    fn consume_min_tls_version(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("min_tls_version", value)
    }

    fn consume_multi_region_failover_api_key(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.api_key", value)
    }

    fn consume_multi_region_failover_dd_url(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.dd_url", value)
    }

    fn consume_multi_region_failover_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.enabled", value)
    }

    fn consume_multi_region_failover_failover_metrics(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.failover_metrics", value)
    }

    fn consume_multi_region_failover_metric_allowlist(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.metric_allowlist", value)
    }

    fn consume_multi_region_failover_site(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("multi_region_failover.site", value)
    }

    fn consume_no_proxy_nonexact_match(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("no_proxy_nonexact_match", value)
    }

    fn consume_observability_pipelines_worker_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("observability_pipelines_worker.metrics.enabled", value)
    }

    fn consume_observability_pipelines_worker_metrics_url(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("observability_pipelines_worker.metrics.url", value)
    }

    fn consume_origin_detection_unified(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("origin_detection_unified", value)
    }

    fn consume_otlp_config_logs_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.logs.enabled", value)
    }

    fn consume_otlp_config_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.metrics.enabled", value)
    }

    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.receiver.protocols.grpc.endpoint", value)
    }

    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib", value)
    }

    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.receiver.protocols.grpc.transport", value)
    }

    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.receiver.protocols.http.endpoint", value)
    }

    fn consume_otlp_config_traces_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.traces.enabled", value)
    }

    fn consume_otlp_config_traces_internal_port(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("otlp_config.traces.internal_port", value)
    }

    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(
        &mut self, value: Option<Value>,
    ) -> TranslateResult {
        self.consume_key("otlp_config.traces.probabilistic_sampler.sampling_percentage", value)
    }

    fn consume_provider_kind(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("provider_kind", value)
    }

    fn consume_proxy_http(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("proxy.http", value)
    }

    fn consume_proxy_https(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("proxy.https", value)
    }

    fn consume_proxy_no_proxy(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("proxy.no_proxy", value)
    }

    fn consume_serializer_compressor_kind(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_compressor_kind", value)
    }

    fn consume_serializer_max_payload_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_max_payload_size", value)
    }

    fn consume_serializer_max_series_payload_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_max_series_payload_size", value)
    }

    fn consume_serializer_max_series_points_per_payload(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_max_series_points_per_payload", value)
    }

    fn consume_serializer_max_series_uncompressed_payload_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_max_series_uncompressed_payload_size", value)
    }

    fn consume_serializer_max_uncompressed_payload_size(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_max_uncompressed_payload_size", value)
    }

    fn consume_serializer_zstd_compressor_level(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("serializer_zstd_compressor_level", value)
    }

    fn consume_site(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("site", value)
    }

    fn consume_skip_ssl_validation(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("skip_ssl_validation", value)
    }

    fn consume_sslkeylogfile(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("sslkeylogfile", value)
    }

    fn consume_statsd_forward_host(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_forward_host", value)
    }

    fn consume_statsd_forward_port(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_forward_port", value)
    }

    fn consume_statsd_metric_blocklist(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_metric_blocklist", value)
    }

    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_metric_blocklist_match_prefix", value)
    }

    fn consume_statsd_metric_namespace(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_metric_namespace", value)
    }

    fn consume_statsd_metric_namespace_blacklist(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("statsd_metric_namespace_blacklist", value)
    }

    fn consume_syslog_rfc(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("syslog_rfc", value)
    }

    fn consume_syslog_uri(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("syslog_uri", value)
    }

    fn consume_use_proxy_for_cloud_metadata(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("use_proxy_for_cloud_metadata", value)
    }

    fn consume_use_v2_api_series(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("use_v2_api.series", value)
    }

    fn consume_vector_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("vector.metrics.enabled", value)
    }

    fn consume_vector_metrics_url(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("vector.metrics.url", value)
    }

    fn consume_vsock_addr(&mut self, value: Option<Value>) -> TranslateResult {
        self.consume_key("vsock_addr", value)
    }
}
