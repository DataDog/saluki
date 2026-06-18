// @generated from schema_overlay.yaml — DO NOT EDIT BY HAND.
use std::fmt;

use serde_json::Value;

use crate::DatadogConfiguration;

/// Error returned while driving Datadog configuration into a native consumer.
#[derive(Debug)]
pub struct TranslateError {
    message: String,
}

impl TranslateError {
    /// Creates a translation error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TranslateError {}

impl From<serde_json::Error> for TranslateError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Result type for Datadog configuration translation.
pub type TranslateResult = Result<(), TranslateError>;

/// Witness consumer for every Datadog config key with full or partial support.
pub trait DatadogConfigConsumer {
    /// Consumes `additional_endpoints`.
    fn consume_additional_endpoints(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `agent_ipc.grpc_max_message_size`.
    fn consume_agent_ipc_grpc_max_message_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `aggregator_stop_timeout`.
    fn consume_aggregator_stop_timeout(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `allow_arbitrary_tags`.
    fn consume_allow_arbitrary_tags(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `api_key`.
    fn consume_api_key(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.credit_cards.enabled`.
    fn consume_apm_config_obfuscation_credit_cards_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.credit_cards.keep_values`.
    fn consume_apm_config_obfuscation_credit_cards_keep_values(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.credit_cards.luhn`.
    fn consume_apm_config_obfuscation_credit_cards_luhn(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.elasticsearch.enabled`.
    fn consume_apm_config_obfuscation_elasticsearch_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.elasticsearch.keep_values`.
    fn consume_apm_config_obfuscation_elasticsearch_keep_values(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.elasticsearch.obfuscate_sql_values`.
    fn consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.http.remove_paths_with_digits`.
    fn consume_apm_config_obfuscation_http_remove_paths_with_digits(&mut self, value: Option<Value>)
        -> TranslateResult;

    /// Consumes `apm_config.obfuscation.http.remove_query_string`.
    fn consume_apm_config_obfuscation_http_remove_query_string(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.memcached.enabled`.
    fn consume_apm_config_obfuscation_memcached_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.memcached.keep_command`.
    fn consume_apm_config_obfuscation_memcached_keep_command(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.mongodb.enabled`.
    fn consume_apm_config_obfuscation_mongodb_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.mongodb.keep_values`.
    fn consume_apm_config_obfuscation_mongodb_keep_values(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.mongodb.obfuscate_sql_values`.
    fn consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.opensearch.enabled`.
    fn consume_apm_config_obfuscation_opensearch_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.opensearch.keep_values`.
    fn consume_apm_config_obfuscation_opensearch_keep_values(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.opensearch.obfuscate_sql_values`.
    fn consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.redis.enabled`.
    fn consume_apm_config_obfuscation_redis_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.redis.remove_all_args`.
    fn consume_apm_config_obfuscation_redis_remove_all_args(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.valkey.enabled`.
    fn consume_apm_config_obfuscation_valkey_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `apm_config.obfuscation.valkey.remove_all_args`.
    fn consume_apm_config_obfuscation_valkey_remove_all_args(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `auth_token_file_path`.
    fn consume_auth_token_file_path(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `bind_host`.
    fn consume_bind_host(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `cmd_port`.
    fn consume_cmd_port(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `cri_connection_timeout`.
    fn consume_cri_connection_timeout(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `cri_query_timeout`.
    fn consume_cri_query_timeout(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.api_listen_address`.
    fn consume_data_plane_api_listen_address(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity`.
    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `data_plane.dogstatsd.enabled`.
    fn consume_data_plane_dogstatsd_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.enabled`.
    fn consume_data_plane_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.log_file`.
    fn consume_data_plane_log_file(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.otlp.enabled`.
    fn consume_data_plane_otlp_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.otlp.proxy.enabled`.
    fn consume_data_plane_otlp_proxy_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.otlp.proxy.logs.enabled`.
    fn consume_data_plane_otlp_proxy_logs_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.otlp.proxy.metrics.enabled`.
    fn consume_data_plane_otlp_proxy_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.otlp.proxy.receiver.protocols.grpc.endpoint`.
    fn consume_data_plane_otlp_proxy_receiver_protocols_grpc_endpoint(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `data_plane.otlp.proxy.traces.enabled`.
    fn consume_data_plane_otlp_proxy_traces_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.remote_agent_enabled`.
    fn consume_data_plane_remote_agent_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.secure_api_listen_address`.
    fn consume_data_plane_secure_api_listen_address(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `data_plane.use_new_config_stream_endpoint`.
    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dd_url`.
    fn consume_dd_url(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_buffer_size`.
    fn consume_dogstatsd_buffer_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_capture_depth`.
    fn consume_dogstatsd_capture_depth(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_capture_path`.
    fn consume_dogstatsd_capture_path(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_context_expiry_seconds`.
    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_entity_id_precedence`.
    fn consume_dogstatsd_entity_id_precedence(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_eol_required`.
    fn consume_dogstatsd_eol_required(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_flush_incomplete_buckets`.
    fn consume_dogstatsd_flush_incomplete_buckets(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_log_file`.
    fn consume_dogstatsd_log_file(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_log_file_max_rolls`.
    fn consume_dogstatsd_log_file_max_rolls(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_log_file_max_size`.
    fn consume_dogstatsd_log_file_max_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_logging_enabled`.
    fn consume_dogstatsd_logging_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_mapper_cache_size`.
    fn consume_dogstatsd_mapper_cache_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_mapper_profiles`.
    fn consume_dogstatsd_mapper_profiles(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_metrics_stats_enable`.
    fn consume_dogstatsd_metrics_stats_enable(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_no_aggregation_pipeline`.
    fn consume_dogstatsd_no_aggregation_pipeline(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_non_local_traffic`.
    fn consume_dogstatsd_non_local_traffic(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_origin_detection`.
    fn consume_dogstatsd_origin_detection(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_origin_detection_client`.
    fn consume_dogstatsd_origin_detection_client(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_origin_optout_enabled`.
    fn consume_dogstatsd_origin_optout_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_port`.
    fn consume_dogstatsd_port(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_so_rcvbuf`.
    fn consume_dogstatsd_so_rcvbuf(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_socket`.
    fn consume_dogstatsd_socket(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_stream_log_too_big`.
    fn consume_dogstatsd_stream_log_too_big(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_stream_socket`.
    fn consume_dogstatsd_stream_socket(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_string_interner_size`.
    fn consume_dogstatsd_string_interner_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_tag_cardinality`.
    fn consume_dogstatsd_tag_cardinality(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `dogstatsd_tags`.
    fn consume_dogstatsd_tags(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `enable_payloads.events`.
    fn consume_enable_payloads_events(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `enable_payloads.series`.
    fn consume_enable_payloads_series(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `enable_payloads.service_checks`.
    fn consume_enable_payloads_service_checks(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `enable_payloads.sketches`.
    fn consume_enable_payloads_sketches(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `env`.
    fn consume_env(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_apikey_validation_interval`.
    fn consume_forwarder_apikey_validation_interval(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_backoff_base`.
    fn consume_forwarder_backoff_base(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_backoff_factor`.
    fn consume_forwarder_backoff_factor(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_backoff_max`.
    fn consume_forwarder_backoff_max(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_connection_reset_interval`.
    fn consume_forwarder_connection_reset_interval(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_flush_to_disk_mem_ratio`.
    fn consume_forwarder_flush_to_disk_mem_ratio(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_high_prio_buffer_size`.
    fn consume_forwarder_high_prio_buffer_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_http_protocol`.
    fn consume_forwarder_http_protocol(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_max_concurrent_requests`.
    fn consume_forwarder_max_concurrent_requests(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_num_workers`.
    fn consume_forwarder_num_workers(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_outdated_file_in_days`.
    fn consume_forwarder_outdated_file_in_days(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_recovery_interval`.
    fn consume_forwarder_recovery_interval(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_recovery_reset`.
    fn consume_forwarder_recovery_reset(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_retry_queue_capacity_time_interval_sec`.
    fn consume_forwarder_retry_queue_capacity_time_interval_sec(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_retry_queue_max_size`.
    fn consume_forwarder_retry_queue_max_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_retry_queue_payloads_max_size`.
    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_stop_timeout`.
    fn consume_forwarder_stop_timeout(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_storage_max_disk_ratio`.
    fn consume_forwarder_storage_max_disk_ratio(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_storage_max_size_in_bytes`.
    fn consume_forwarder_storage_max_size_in_bytes(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_storage_path`.
    fn consume_forwarder_storage_path(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `forwarder_timeout`.
    fn consume_forwarder_timeout(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `histogram_aggregates`.
    fn consume_histogram_aggregates(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `histogram_copy_to_distribution`.
    fn consume_histogram_copy_to_distribution(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `histogram_copy_to_distribution_prefix`.
    fn consume_histogram_copy_to_distribution_prefix(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `ipc_cert_file_path`.
    fn consume_ipc_cert_file_path(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `log_format_rfc3339`.
    fn consume_log_format_rfc3339(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `log_level`.
    fn consume_log_level(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `log_payloads`.
    fn consume_log_payloads(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `metric_filterlist`.
    fn consume_metric_filterlist(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `metric_filterlist_match_prefix`.
    fn consume_metric_filterlist_match_prefix(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `min_tls_version`.
    fn consume_min_tls_version(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.api_key`.
    fn consume_multi_region_failover_api_key(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.dd_url`.
    fn consume_multi_region_failover_dd_url(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.enabled`.
    fn consume_multi_region_failover_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.failover_metrics`.
    fn consume_multi_region_failover_failover_metrics(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.metric_allowlist`.
    fn consume_multi_region_failover_metric_allowlist(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `multi_region_failover.site`.
    fn consume_multi_region_failover_site(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `no_proxy_nonexact_match`.
    fn consume_no_proxy_nonexact_match(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `observability_pipelines_worker.metrics.enabled`.
    fn consume_observability_pipelines_worker_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `observability_pipelines_worker.metrics.url`.
    fn consume_observability_pipelines_worker_metrics_url(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `origin_detection_unified`.
    fn consume_origin_detection_unified(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.logs.enabled`.
    fn consume_otlp_config_logs_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.metrics.enabled`.
    fn consume_otlp_config_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.receiver.protocols.grpc.endpoint`.
    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib`.
    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `otlp_config.receiver.protocols.grpc.transport`.
    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.receiver.protocols.http.endpoint`.
    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.traces.enabled`.
    fn consume_otlp_config_traces_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.traces.internal_port`.
    fn consume_otlp_config_traces_internal_port(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `otlp_config.traces.probabilistic_sampler.sampling_percentage`.
    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(
        &mut self, value: Option<Value>,
    ) -> TranslateResult;

    /// Consumes `provider_kind`.
    fn consume_provider_kind(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `proxy.http`.
    fn consume_proxy_http(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `proxy.https`.
    fn consume_proxy_https(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `proxy.no_proxy`.
    fn consume_proxy_no_proxy(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_compressor_kind`.
    fn consume_serializer_compressor_kind(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_max_payload_size`.
    fn consume_serializer_max_payload_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_max_series_payload_size`.
    fn consume_serializer_max_series_payload_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_max_series_points_per_payload`.
    fn consume_serializer_max_series_points_per_payload(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_max_series_uncompressed_payload_size`.
    fn consume_serializer_max_series_uncompressed_payload_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_max_uncompressed_payload_size`.
    fn consume_serializer_max_uncompressed_payload_size(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `serializer_zstd_compressor_level`.
    fn consume_serializer_zstd_compressor_level(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `site`.
    fn consume_site(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `skip_ssl_validation`.
    fn consume_skip_ssl_validation(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `sslkeylogfile`.
    fn consume_sslkeylogfile(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_forward_host`.
    fn consume_statsd_forward_host(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_forward_port`.
    fn consume_statsd_forward_port(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_metric_blocklist`.
    fn consume_statsd_metric_blocklist(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_metric_blocklist_match_prefix`.
    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_metric_namespace`.
    fn consume_statsd_metric_namespace(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `statsd_metric_namespace_blacklist`.
    fn consume_statsd_metric_namespace_blacklist(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `syslog_rfc`.
    fn consume_syslog_rfc(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `syslog_uri`.
    fn consume_syslog_uri(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `use_proxy_for_cloud_metadata`.
    fn consume_use_proxy_for_cloud_metadata(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `use_v2_api.series`.
    fn consume_use_v2_api_series(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `vector.metrics.enabled`.
    fn consume_vector_metrics_enabled(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `vector.metrics.url`.
    fn consume_vector_metrics_url(&mut self, value: Option<Value>) -> TranslateResult;

    /// Consumes `vsock_addr`.
    fn consume_vsock_addr(&mut self, value: Option<Value>) -> TranslateResult;
}

/// Drives every supported Datadog key into `consumer`.
pub fn drive(config: &DatadogConfiguration, consumer: &mut impl DatadogConfigConsumer) -> TranslateResult {
    let value = serde_json::to_value(config)?;
    consumer.consume_additional_endpoints(lookup(&value, &["additional_endpoints"]))?;
    consumer.consume_agent_ipc_grpc_max_message_size(lookup(&value, &["agent_ipc", "grpc_max_message_size"]))?;
    consumer.consume_aggregator_stop_timeout(lookup(&value, &["aggregator_stop_timeout"]))?;
    consumer.consume_allow_arbitrary_tags(lookup(&value, &["allow_arbitrary_tags"]))?;
    consumer.consume_api_key(lookup(&value, &["api_key"]))?;
    consumer.consume_apm_config_obfuscation_credit_cards_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "credit_cards", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_credit_cards_keep_values(lookup(
        &value,
        &["apm_config", "obfuscation", "credit_cards", "keep_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_credit_cards_luhn(lookup(
        &value,
        &["apm_config", "obfuscation", "credit_cards", "luhn"],
    ))?;
    consumer.consume_apm_config_obfuscation_elasticsearch_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "elasticsearch", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_elasticsearch_keep_values(lookup(
        &value,
        &["apm_config", "obfuscation", "elasticsearch", "keep_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(lookup(
        &value,
        &["apm_config", "obfuscation", "elasticsearch", "obfuscate_sql_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_http_remove_paths_with_digits(lookup(
        &value,
        &["apm_config", "obfuscation", "http", "remove_paths_with_digits"],
    ))?;
    consumer.consume_apm_config_obfuscation_http_remove_query_string(lookup(
        &value,
        &["apm_config", "obfuscation", "http", "remove_query_string"],
    ))?;
    consumer.consume_apm_config_obfuscation_memcached_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "memcached", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_memcached_keep_command(lookup(
        &value,
        &["apm_config", "obfuscation", "memcached", "keep_command"],
    ))?;
    consumer.consume_apm_config_obfuscation_mongodb_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "mongodb", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_mongodb_keep_values(lookup(
        &value,
        &["apm_config", "obfuscation", "mongodb", "keep_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(lookup(
        &value,
        &["apm_config", "obfuscation", "mongodb", "obfuscate_sql_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_opensearch_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "opensearch", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_opensearch_keep_values(lookup(
        &value,
        &["apm_config", "obfuscation", "opensearch", "keep_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(lookup(
        &value,
        &["apm_config", "obfuscation", "opensearch", "obfuscate_sql_values"],
    ))?;
    consumer.consume_apm_config_obfuscation_redis_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "redis", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_redis_remove_all_args(lookup(
        &value,
        &["apm_config", "obfuscation", "redis", "remove_all_args"],
    ))?;
    consumer.consume_apm_config_obfuscation_valkey_enabled(lookup(
        &value,
        &["apm_config", "obfuscation", "valkey", "enabled"],
    ))?;
    consumer.consume_apm_config_obfuscation_valkey_remove_all_args(lookup(
        &value,
        &["apm_config", "obfuscation", "valkey", "remove_all_args"],
    ))?;
    consumer.consume_auth_token_file_path(lookup(&value, &["auth_token_file_path"]))?;
    consumer.consume_bind_host(lookup(&value, &["bind_host"]))?;
    consumer.consume_cmd_port(lookup(&value, &["cmd_port"]))?;
    consumer.consume_cri_connection_timeout(lookup(&value, &["cri_connection_timeout"]))?;
    consumer.consume_cri_query_timeout(lookup(&value, &["cri_query_timeout"]))?;
    consumer.consume_data_plane_api_listen_address(lookup(&value, &["data_plane", "api_listen_address"]))?;
    consumer.consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(lookup(
        &value,
        &["data_plane", "dogstatsd", "aggregator_tag_filter_cache_capacity"],
    ))?;
    consumer.consume_data_plane_dogstatsd_enabled(lookup(&value, &["data_plane", "dogstatsd", "enabled"]))?;
    consumer.consume_data_plane_enabled(lookup(&value, &["data_plane", "enabled"]))?;
    consumer.consume_data_plane_log_file(lookup(&value, &["data_plane", "log_file"]))?;
    consumer.consume_data_plane_otlp_enabled(lookup(&value, &["data_plane", "otlp", "enabled"]))?;
    consumer.consume_data_plane_otlp_proxy_enabled(lookup(&value, &["data_plane", "otlp", "proxy", "enabled"]))?;
    consumer.consume_data_plane_otlp_proxy_logs_enabled(lookup(
        &value,
        &["data_plane", "otlp", "proxy", "logs", "enabled"],
    ))?;
    consumer.consume_data_plane_otlp_proxy_metrics_enabled(lookup(
        &value,
        &["data_plane", "otlp", "proxy", "metrics", "enabled"],
    ))?;
    consumer.consume_data_plane_otlp_proxy_receiver_protocols_grpc_endpoint(lookup(
        &value,
        &[
            "data_plane",
            "otlp",
            "proxy",
            "receiver",
            "protocols",
            "grpc",
            "endpoint",
        ],
    ))?;
    consumer.consume_data_plane_otlp_proxy_traces_enabled(lookup(
        &value,
        &["data_plane", "otlp", "proxy", "traces", "enabled"],
    ))?;
    consumer.consume_data_plane_remote_agent_enabled(lookup(&value, &["data_plane", "remote_agent_enabled"]))?;
    consumer
        .consume_data_plane_secure_api_listen_address(lookup(&value, &["data_plane", "secure_api_listen_address"]))?;
    consumer.consume_data_plane_use_new_config_stream_endpoint(lookup(
        &value,
        &["data_plane", "use_new_config_stream_endpoint"],
    ))?;
    consumer.consume_dd_url(lookup(&value, &["dd_url"]))?;
    consumer.consume_dogstatsd_buffer_size(lookup(&value, &["dogstatsd_buffer_size"]))?;
    consumer.consume_dogstatsd_capture_depth(lookup(&value, &["dogstatsd_capture_depth"]))?;
    consumer.consume_dogstatsd_capture_path(lookup(&value, &["dogstatsd_capture_path"]))?;
    consumer.consume_dogstatsd_context_expiry_seconds(lookup(&value, &["dogstatsd_context_expiry_seconds"]))?;
    consumer.consume_dogstatsd_entity_id_precedence(lookup(&value, &["dogstatsd_entity_id_precedence"]))?;
    consumer.consume_dogstatsd_eol_required(lookup(&value, &["dogstatsd_eol_required"]))?;
    consumer.consume_dogstatsd_flush_incomplete_buckets(lookup(&value, &["dogstatsd_flush_incomplete_buckets"]))?;
    consumer.consume_dogstatsd_log_file(lookup(&value, &["dogstatsd_log_file"]))?;
    consumer.consume_dogstatsd_log_file_max_rolls(lookup(&value, &["dogstatsd_log_file_max_rolls"]))?;
    consumer.consume_dogstatsd_log_file_max_size(lookup(&value, &["dogstatsd_log_file_max_size"]))?;
    consumer.consume_dogstatsd_logging_enabled(lookup(&value, &["dogstatsd_logging_enabled"]))?;
    consumer.consume_dogstatsd_mapper_cache_size(lookup(&value, &["dogstatsd_mapper_cache_size"]))?;
    consumer.consume_dogstatsd_mapper_profiles(lookup(&value, &["dogstatsd_mapper_profiles"]))?;
    consumer.consume_dogstatsd_metrics_stats_enable(lookup(&value, &["dogstatsd_metrics_stats_enable"]))?;
    consumer.consume_dogstatsd_no_aggregation_pipeline(lookup(&value, &["dogstatsd_no_aggregation_pipeline"]))?;
    consumer.consume_dogstatsd_non_local_traffic(lookup(&value, &["dogstatsd_non_local_traffic"]))?;
    consumer.consume_dogstatsd_origin_detection(lookup(&value, &["dogstatsd_origin_detection"]))?;
    consumer.consume_dogstatsd_origin_detection_client(lookup(&value, &["dogstatsd_origin_detection_client"]))?;
    consumer.consume_dogstatsd_origin_optout_enabled(lookup(&value, &["dogstatsd_origin_optout_enabled"]))?;
    consumer.consume_dogstatsd_port(lookup(&value, &["dogstatsd_port"]))?;
    consumer.consume_dogstatsd_so_rcvbuf(lookup(&value, &["dogstatsd_so_rcvbuf"]))?;
    consumer.consume_dogstatsd_socket(lookup(&value, &["dogstatsd_socket"]))?;
    consumer.consume_dogstatsd_stream_log_too_big(lookup(&value, &["dogstatsd_stream_log_too_big"]))?;
    consumer.consume_dogstatsd_stream_socket(lookup(&value, &["dogstatsd_stream_socket"]))?;
    consumer.consume_dogstatsd_string_interner_size(lookup(&value, &["dogstatsd_string_interner_size"]))?;
    consumer.consume_dogstatsd_tag_cardinality(lookup(&value, &["dogstatsd_tag_cardinality"]))?;
    consumer.consume_dogstatsd_tags(lookup(&value, &["dogstatsd_tags"]))?;
    consumer.consume_enable_payloads_events(lookup(&value, &["enable_payloads", "events"]))?;
    consumer.consume_enable_payloads_series(lookup(&value, &["enable_payloads", "series"]))?;
    consumer.consume_enable_payloads_service_checks(lookup(&value, &["enable_payloads", "service_checks"]))?;
    consumer.consume_enable_payloads_sketches(lookup(&value, &["enable_payloads", "sketches"]))?;
    consumer.consume_env(lookup(&value, &["env"]))?;
    consumer.consume_forwarder_apikey_validation_interval(lookup(&value, &["forwarder_apikey_validation_interval"]))?;
    consumer.consume_forwarder_backoff_base(lookup(&value, &["forwarder_backoff_base"]))?;
    consumer.consume_forwarder_backoff_factor(lookup(&value, &["forwarder_backoff_factor"]))?;
    consumer.consume_forwarder_backoff_max(lookup(&value, &["forwarder_backoff_max"]))?;
    consumer.consume_forwarder_connection_reset_interval(lookup(&value, &["forwarder_connection_reset_interval"]))?;
    consumer.consume_forwarder_flush_to_disk_mem_ratio(lookup(&value, &["forwarder_flush_to_disk_mem_ratio"]))?;
    consumer.consume_forwarder_high_prio_buffer_size(lookup(&value, &["forwarder_high_prio_buffer_size"]))?;
    consumer.consume_forwarder_http_protocol(lookup(&value, &["forwarder_http_protocol"]))?;
    consumer.consume_forwarder_max_concurrent_requests(lookup(&value, &["forwarder_max_concurrent_requests"]))?;
    consumer.consume_forwarder_num_workers(lookup(&value, &["forwarder_num_workers"]))?;
    consumer.consume_forwarder_outdated_file_in_days(lookup(&value, &["forwarder_outdated_file_in_days"]))?;
    consumer.consume_forwarder_recovery_interval(lookup(&value, &["forwarder_recovery_interval"]))?;
    consumer.consume_forwarder_recovery_reset(lookup(&value, &["forwarder_recovery_reset"]))?;
    consumer.consume_forwarder_retry_queue_capacity_time_interval_sec(lookup(
        &value,
        &["forwarder_retry_queue_capacity_time_interval_sec"],
    ))?;
    consumer.consume_forwarder_retry_queue_max_size(lookup(&value, &["forwarder_retry_queue_max_size"]))?;
    consumer.consume_forwarder_retry_queue_payloads_max_size(lookup(
        &value,
        &["forwarder_retry_queue_payloads_max_size"],
    ))?;
    consumer.consume_forwarder_stop_timeout(lookup(&value, &["forwarder_stop_timeout"]))?;
    consumer.consume_forwarder_storage_max_disk_ratio(lookup(&value, &["forwarder_storage_max_disk_ratio"]))?;
    consumer.consume_forwarder_storage_max_size_in_bytes(lookup(&value, &["forwarder_storage_max_size_in_bytes"]))?;
    consumer.consume_forwarder_storage_path(lookup(&value, &["forwarder_storage_path"]))?;
    consumer.consume_forwarder_timeout(lookup(&value, &["forwarder_timeout"]))?;
    consumer.consume_histogram_aggregates(lookup(&value, &["histogram_aggregates"]))?;
    consumer.consume_histogram_copy_to_distribution(lookup(&value, &["histogram_copy_to_distribution"]))?;
    consumer
        .consume_histogram_copy_to_distribution_prefix(lookup(&value, &["histogram_copy_to_distribution_prefix"]))?;
    consumer.consume_ipc_cert_file_path(lookup(&value, &["ipc_cert_file_path"]))?;
    consumer.consume_log_format_rfc3339(lookup(&value, &["log_format_rfc3339"]))?;
    consumer.consume_log_level(lookup(&value, &["log_level"]))?;
    consumer.consume_log_payloads(lookup(&value, &["log_payloads"]))?;
    consumer.consume_metric_filterlist(lookup(&value, &["metric_filterlist"]))?;
    consumer.consume_metric_filterlist_match_prefix(lookup(&value, &["metric_filterlist_match_prefix"]))?;
    consumer.consume_min_tls_version(lookup(&value, &["min_tls_version"]))?;
    consumer.consume_multi_region_failover_api_key(lookup(&value, &["multi_region_failover", "api_key"]))?;
    consumer.consume_multi_region_failover_dd_url(lookup(&value, &["multi_region_failover", "dd_url"]))?;
    consumer.consume_multi_region_failover_enabled(lookup(&value, &["multi_region_failover", "enabled"]))?;
    consumer.consume_multi_region_failover_failover_metrics(lookup(
        &value,
        &["multi_region_failover", "failover_metrics"],
    ))?;
    consumer.consume_multi_region_failover_metric_allowlist(lookup(
        &value,
        &["multi_region_failover", "metric_allowlist"],
    ))?;
    consumer.consume_multi_region_failover_site(lookup(&value, &["multi_region_failover", "site"]))?;
    consumer.consume_no_proxy_nonexact_match(lookup(&value, &["no_proxy_nonexact_match"]))?;
    consumer.consume_observability_pipelines_worker_metrics_enabled(lookup(
        &value,
        &["observability_pipelines_worker", "metrics", "enabled"],
    ))?;
    consumer.consume_observability_pipelines_worker_metrics_url(lookup(
        &value,
        &["observability_pipelines_worker", "metrics", "url"],
    ))?;
    consumer.consume_origin_detection_unified(lookup(&value, &["origin_detection_unified"]))?;
    consumer.consume_otlp_config_logs_enabled(lookup(&value, &["otlp_config", "logs", "enabled"]))?;
    consumer.consume_otlp_config_metrics_enabled(lookup(&value, &["otlp_config", "metrics", "enabled"]))?;
    consumer.consume_otlp_config_receiver_protocols_grpc_endpoint(lookup(
        &value,
        &["otlp_config", "receiver", "protocols", "grpc", "endpoint"],
    ))?;
    consumer.consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(lookup(
        &value,
        &["otlp_config", "receiver", "protocols", "grpc", "max_recv_msg_size_mib"],
    ))?;
    consumer.consume_otlp_config_receiver_protocols_grpc_transport(lookup(
        &value,
        &["otlp_config", "receiver", "protocols", "grpc", "transport"],
    ))?;
    consumer.consume_otlp_config_receiver_protocols_http_endpoint(lookup(
        &value,
        &["otlp_config", "receiver", "protocols", "http", "endpoint"],
    ))?;
    consumer.consume_otlp_config_traces_enabled(lookup(&value, &["otlp_config", "traces", "enabled"]))?;
    consumer.consume_otlp_config_traces_internal_port(lookup(&value, &["otlp_config", "traces", "internal_port"]))?;
    consumer.consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(lookup(
        &value,
        &["otlp_config", "traces", "probabilistic_sampler", "sampling_percentage"],
    ))?;
    consumer.consume_provider_kind(lookup(&value, &["provider_kind"]))?;
    consumer.consume_proxy_http(lookup(&value, &["proxy", "http"]))?;
    consumer.consume_proxy_https(lookup(&value, &["proxy", "https"]))?;
    consumer.consume_proxy_no_proxy(lookup(&value, &["proxy", "no_proxy"]))?;
    consumer.consume_serializer_compressor_kind(lookup(&value, &["serializer_compressor_kind"]))?;
    consumer.consume_serializer_max_payload_size(lookup(&value, &["serializer_max_payload_size"]))?;
    consumer.consume_serializer_max_series_payload_size(lookup(&value, &["serializer_max_series_payload_size"]))?;
    consumer.consume_serializer_max_series_points_per_payload(lookup(
        &value,
        &["serializer_max_series_points_per_payload"],
    ))?;
    consumer.consume_serializer_max_series_uncompressed_payload_size(lookup(
        &value,
        &["serializer_max_series_uncompressed_payload_size"],
    ))?;
    consumer.consume_serializer_max_uncompressed_payload_size(lookup(
        &value,
        &["serializer_max_uncompressed_payload_size"],
    ))?;
    consumer.consume_serializer_zstd_compressor_level(lookup(&value, &["serializer_zstd_compressor_level"]))?;
    consumer.consume_site(lookup(&value, &["site"]))?;
    consumer.consume_skip_ssl_validation(lookup(&value, &["skip_ssl_validation"]))?;
    consumer.consume_sslkeylogfile(lookup(&value, &["sslkeylogfile"]))?;
    consumer.consume_statsd_forward_host(lookup(&value, &["statsd_forward_host"]))?;
    consumer.consume_statsd_forward_port(lookup(&value, &["statsd_forward_port"]))?;
    consumer.consume_statsd_metric_blocklist(lookup(&value, &["statsd_metric_blocklist"]))?;
    consumer.consume_statsd_metric_blocklist_match_prefix(lookup(&value, &["statsd_metric_blocklist_match_prefix"]))?;
    consumer.consume_statsd_metric_namespace(lookup(&value, &["statsd_metric_namespace"]))?;
    consumer.consume_statsd_metric_namespace_blacklist(lookup(&value, &["statsd_metric_namespace_blacklist"]))?;
    consumer.consume_syslog_rfc(lookup(&value, &["syslog_rfc"]))?;
    consumer.consume_syslog_uri(lookup(&value, &["syslog_uri"]))?;
    consumer.consume_use_proxy_for_cloud_metadata(lookup(&value, &["use_proxy_for_cloud_metadata"]))?;
    consumer.consume_use_v2_api_series(lookup(&value, &["use_v2_api", "series"]))?;
    consumer.consume_vector_metrics_enabled(lookup(&value, &["vector", "metrics", "enabled"]))?;
    consumer.consume_vector_metrics_url(lookup(&value, &["vector", "metrics", "url"]))?;
    consumer.consume_vsock_addr(lookup(&value, &["vsock_addr"]))?;
    Ok(())
}

fn lookup(value: &Value, path: &[&str]) -> Option<Value> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    Some(current.clone())
}
