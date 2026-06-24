//! The Datadog witness implementation.
//!
//! Each `consume_<key>` method handles an incoming [`DatadogConfiguration`] field value
//! and assigns it to the proper place in [`SalukiConfiguration`].
use std::collections::HashMap;

use datadog_agent_config::{DatadogConfigConsumer, TranslateError};
use saluki_context::origin::OriginTagCardinality;
use stringtheory::MetaString;

use crate::translate::Translator;

/// Returns `Some(s)` when `s` is non-empty, `None` otherwise.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

impl DatadogConfigConsumer for Translator {
    fn consume_additional_endpoints(&mut self, _value: HashMap<String, Vec<String>>) {}
    fn consume_agent_ipc_grpc_max_message_size(&mut self, _value: i64) {}
    fn consume_aggregator_stop_timeout(&mut self, _value: i64) {}
    fn consume_allow_arbitrary_tags(&mut self, _value: bool) {}
    fn consume_api_key(&mut self, _value: String) {}
    fn consume_apm_config_obfuscation_credit_cards_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_credit_cards_keep_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_credit_cards_luhn(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_elasticsearch_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_elasticsearch_keep_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_http_remove_paths_with_digits(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_http_remove_query_string(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_memcached_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_memcached_keep_command(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_mongodb_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_mongodb_keep_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_opensearch_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_opensearch_keep_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(&mut self, _value: Vec<String>) {}
    fn consume_apm_config_obfuscation_redis_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_redis_remove_all_args(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_valkey_enabled(&mut self, _value: bool) {}
    fn consume_apm_config_obfuscation_valkey_remove_all_args(&mut self, _value: bool) {}
    fn consume_autoscaling_failover_enabled(&mut self, _value: bool) {}
    fn consume_autoscaling_failover_metrics(&mut self, _value: Vec<String>) {}

    fn consume_bind_host(&mut self, value: String) {
        self.saluki.components.dogstatsd.source.bind_host = non_empty(value);
    }

    fn consume_cluster_agent_auth_token(&mut self, _value: String) {}
    fn consume_cluster_agent_enabled(&mut self, _value: bool) {}
    fn consume_cluster_agent_kubernetes_service_name(&mut self, _value: String) {}
    fn consume_cluster_agent_url(&mut self, _value: String) {}
    fn consume_cmd_port(&mut self, _value: i64) {}
    fn consume_cri_connection_timeout(&mut self, _value: i64) {}
    fn consume_cri_query_timeout(&mut self, _value: i64) {}
    fn consume_data_plane_api_listen_address(&mut self, _value: String) {}
    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(&mut self, _value: i64) {}
    fn consume_data_plane_log_file(&mut self, _value: String) {}
    fn consume_data_plane_otlp_proxy_logs_enabled(&mut self, _value: bool) {}
    fn consume_data_plane_otlp_proxy_metrics_enabled(&mut self, _value: bool) {}
    fn consume_data_plane_otlp_proxy_traces_enabled(&mut self, _value: bool) {}
    fn consume_data_plane_remote_agent_enabled(&mut self, _value: bool) {}
    fn consume_data_plane_secure_api_listen_address(&mut self, _value: String) {}
    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, _value: bool) {}
    fn consume_dd_url(&mut self, _value: String) {}

    fn consume_dogstatsd_buffer_size(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.buffer_size = value.max(0) as usize;
    }

    fn consume_dogstatsd_capture_depth(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.capture_depth = value.max(0) as usize;
    }

    fn consume_dogstatsd_capture_path(&mut self, value: String) {
        self.saluki.components.dogstatsd.source.capture_path = std::path::PathBuf::from(value);
    }

    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.context_expiry_seconds = value.max(0) as u64;
    }

    fn consume_dogstatsd_disable_verbose_logs(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.disable_verbose_logs = value;
    }

    fn consume_dogstatsd_entity_id_precedence(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .source
            .origin_enrichment
            .entity_id_precedence = value;
    }

    fn consume_dogstatsd_eol_required(&mut self, value: Vec<String>) {
        self.saluki.components.dogstatsd.source.eol_required = value;
    }

    fn consume_dogstatsd_flush_incomplete_buckets(&mut self, _value: bool) {}
    fn consume_dogstatsd_log_file(&mut self, _value: String) {}
    fn consume_dogstatsd_log_file_max_rolls(&mut self, _value: i64) {}
    fn consume_dogstatsd_log_file_max_size(&mut self, _value: String) {}
    fn consume_dogstatsd_logging_enabled(&mut self, _value: bool) {}
    fn consume_dogstatsd_mapper_cache_size(&mut self, _value: i64) {}
    fn consume_dogstatsd_mapper_profiles(&mut self, _value: Vec<::serde_json::Value>) {}
    fn consume_dogstatsd_metrics_stats_enable(&mut self, _value: bool) {}

    fn consume_dogstatsd_no_aggregation_pipeline(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.no_aggregation_pipeline_support = value;
    }

    fn consume_dogstatsd_non_local_traffic(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.non_local_traffic = value;
    }

    fn consume_dogstatsd_origin_detection(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.origin_enrichment.enabled = value;
    }

    fn consume_dogstatsd_origin_detection_client(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .source
            .origin_enrichment
            .origin_detection_client = value;
    }

    fn consume_dogstatsd_origin_optout_enabled(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .source
            .origin_enrichment
            .origin_detection_optout = value;
    }

    fn consume_dogstatsd_port(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.port = value.clamp(0, u16::MAX as i64) as u16;
    }

    fn consume_dogstatsd_so_rcvbuf(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.socket_receive_buffer_size = value.max(0) as usize;
    }

    fn consume_dogstatsd_socket(&mut self, value: Option<String>) {
        self.saluki.components.dogstatsd.source.socket_path = value.and_then(non_empty);
    }

    fn consume_dogstatsd_stream_log_too_big(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.stream_log_too_big = value;
    }

    fn consume_dogstatsd_stream_socket(&mut self, value: String) {
        self.saluki.components.dogstatsd.source.socket_stream_path = non_empty(value);
    }

    fn consume_dogstatsd_string_interner_size(&mut self, value: i64) {
        self.saluki
            .components
            .dogstatsd
            .source
            .context_string_interner_entry_count = value.max(0) as u64;
    }

    fn consume_dogstatsd_tag_cardinality(&mut self, value: String) {
        match OriginTagCardinality::try_from(value.as_str()) {
            Ok(c) => {
                self.saluki
                    .components
                    .dogstatsd
                    .source
                    .origin_enrichment
                    .tag_cardinality = c
            }
            Err(reason) => self.record_error(TranslateError::for_key("dogstatsd_tag_cardinality", reason)),
        }
    }

    fn consume_dogstatsd_tags(&mut self, value: Vec<String>) {
        self.saluki.components.dogstatsd.source.additional_tags = value;
    }

    fn consume_enable_payloads_events(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.enable_payloads.events = value;
    }

    fn consume_enable_payloads_series(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.enable_payloads.series = value;
    }

    fn consume_enable_payloads_service_checks(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.enable_payloads.service_checks = value;
    }

    fn consume_enable_payloads_sketches(&mut self, value: bool) {
        self.saluki.components.dogstatsd.source.enable_payloads.sketches = value;
    }

    fn consume_env(&mut self, _value: String) {}
    fn consume_forwarder_apikey_validation_interval(&mut self, _value: i64) {}
    fn consume_forwarder_backoff_base(&mut self, _value: i64) {}
    fn consume_forwarder_backoff_factor(&mut self, _value: i64) {}
    fn consume_forwarder_backoff_max(&mut self, _value: i64) {}
    fn consume_forwarder_connection_reset_interval(&mut self, _value: i64) {}
    fn consume_forwarder_flush_to_disk_mem_ratio(&mut self, _value: f64) {}
    fn consume_forwarder_high_prio_buffer_size(&mut self, _value: i64) {}
    fn consume_forwarder_http_protocol(&mut self, _value: String) {}
    fn consume_forwarder_max_concurrent_requests(&mut self, _value: i64) {}
    fn consume_forwarder_num_workers(&mut self, _value: i64) {}
    fn consume_forwarder_outdated_file_in_days(&mut self, _value: i64) {}
    fn consume_forwarder_recovery_interval(&mut self, _value: i64) {}
    fn consume_forwarder_recovery_reset(&mut self, _value: bool) {}
    fn consume_forwarder_retry_queue_capacity_time_interval_sec(&mut self, _value: i64) {}
    fn consume_forwarder_retry_queue_max_size(&mut self, _value: i64) {}
    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, _value: i64) {}
    fn consume_forwarder_stop_timeout(&mut self, _value: i64) {}
    fn consume_forwarder_storage_max_disk_ratio(&mut self, _value: f64) {}
    fn consume_forwarder_storage_max_size_in_bytes(&mut self, _value: i64) {}
    fn consume_forwarder_storage_path(&mut self, _value: String) {}
    fn consume_forwarder_timeout(&mut self, _value: i64) {}
    fn consume_histogram_aggregates(&mut self, _value: Vec<String>) {}
    fn consume_histogram_copy_to_distribution(&mut self, _value: bool) {}
    fn consume_histogram_copy_to_distribution_prefix(&mut self, _value: String) {}
    fn consume_log_format_rfc3339(&mut self, _value: bool) {}
    fn consume_log_level(&mut self, _value: String) {}
    fn consume_log_payloads(&mut self, _value: bool) {}
    fn consume_metric_filterlist(&mut self, value: Vec<String>) {
        self.saluki.components.dogstatsd.prefix_filter.metric_filterlist = value;
    }

    fn consume_metric_filterlist_match_prefix(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .prefix_filter
            .metric_filterlist_match_prefix = value;
    }
    fn consume_min_tls_version(&mut self, _value: String) {}
    fn consume_multi_region_failover_api_key(&mut self, _value: String) {}
    fn consume_multi_region_failover_dd_url(&mut self, _value: String) {}
    fn consume_multi_region_failover_enabled(&mut self, _value: bool) {}
    fn consume_multi_region_failover_failover_metrics(&mut self, _value: bool) {}
    fn consume_multi_region_failover_metric_allowlist(&mut self, _value: Vec<String>) {}
    fn consume_multi_region_failover_site(&mut self, _value: String) {}
    fn consume_no_proxy_nonexact_match(&mut self, _value: bool) {}
    fn consume_observability_pipelines_worker_metrics_enabled(&mut self, _value: bool) {}
    fn consume_observability_pipelines_worker_metrics_url(&mut self, _value: String) {}

    fn consume_origin_detection_unified(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .source
            .origin_enrichment
            .origin_detection_unified = value;
    }

    fn consume_otlp_config_logs_enabled(&mut self, _value: bool) {}
    fn consume_otlp_config_metrics_enabled(&mut self, _value: bool) {}
    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, _value: String) {}
    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(&mut self, _value: i64) {}
    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, _value: String) {}
    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, _value: String) {}
    fn consume_otlp_config_traces_enabled(&mut self, _value: bool) {}
    fn consume_otlp_config_traces_internal_port(&mut self, _value: i64) {}
    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(&mut self, _value: f64) {}

    fn consume_provider_kind(&mut self, value: String) {
        self.saluki.components.dogstatsd.source.provider_kind = value;
    }

    fn consume_proxy_http(&mut self, _value: String) {}
    fn consume_proxy_https(&mut self, _value: String) {}
    fn consume_proxy_no_proxy(&mut self, _value: Vec<String>) {}
    fn consume_serializer_compressor_kind(&mut self, _value: String) {}
    fn consume_serializer_max_payload_size(&mut self, _value: i64) {}
    fn consume_serializer_max_series_payload_size(&mut self, _value: i64) {}
    fn consume_serializer_max_series_points_per_payload(&mut self, _value: i64) {}
    fn consume_serializer_max_series_uncompressed_payload_size(&mut self, _value: i64) {}
    fn consume_serializer_max_uncompressed_payload_size(&mut self, _value: i64) {}
    fn consume_serializer_zstd_compressor_level(&mut self, _value: i64) {}
    fn consume_site(&mut self, _value: String) {}
    fn consume_skip_ssl_validation(&mut self, _value: bool) {}
    fn consume_sslkeylogfile(&mut self, _value: String) {}

    fn consume_statsd_forward_host(&mut self, value: String) {
        self.saluki.components.dogstatsd.source.statsd_forward_host = non_empty(value).map(MetaString::from);
    }

    fn consume_statsd_forward_port(&mut self, value: i64) {
        self.saluki.components.dogstatsd.source.statsd_forward_port = value.clamp(0, u16::MAX as i64) as u16;
    }

    fn consume_statsd_metric_blocklist(&mut self, value: Vec<String>) {
        self.saluki.components.dogstatsd.prefix_filter.metric_blocklist = value;
    }

    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: bool) {
        self.saluki
            .components
            .dogstatsd
            .prefix_filter
            .metric_blocklist_match_prefix = value;
    }

    fn consume_statsd_metric_namespace(&mut self, value: String) {
        self.saluki.components.dogstatsd.prefix_filter.metric_prefix = value;
    }

    fn consume_statsd_metric_namespace_blacklist(&mut self, value: Vec<String>) {
        self.saluki.components.dogstatsd.prefix_filter.metric_prefix_blocklist = value;
    }
    fn consume_syslog_rfc(&mut self, _value: bool) {}
    fn consume_syslog_uri(&mut self, _value: String) {}
    fn consume_use_proxy_for_cloud_metadata(&mut self, _value: bool) {}
    fn consume_use_v2_api_series(&mut self, _value: bool) {}
    fn consume_vector_metrics_enabled(&mut self, _value: bool) {}
    fn consume_vector_metrics_url(&mut self, _value: String) {}
    fn consume_vsock_addr(&mut self, _value: String) {}

    fn translate_error(&mut self) -> Option<TranslateError> {
        self.errors.first().cloned()
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::SalukiConfiguration;
    use datadog_agent_config::DatadogConfigConsumer;
    use saluki_context::origin::OriginTagCardinality;

    use crate::translate::Translator;

    #[test]
    fn tag_cardinality_parses_known_values() {
        let mut t = Translator::new(SalukiConfiguration::default());
        t.consume_dogstatsd_tag_cardinality("high".to_string());
        assert_eq!(
            t.finish().components.dogstatsd.source.origin_enrichment.tag_cardinality,
            OriginTagCardinality::High
        );
    }

    #[test]
    fn tag_cardinality_records_error_on_unknown_value() {
        let mut t = Translator::new(SalukiConfiguration::default());
        t.consume_dogstatsd_tag_cardinality("bogus".to_string());
        assert!(t.translate_error().is_some());
    }
}
