//! The Datadog witness implementation: `impl DatadogConfigConsumer for Translator`.
//!
//! Every `consume_<key>` method here is mechanically thin: it forwards the witnessed value to a free
//! function in the owning subsystem module (or stashes it into the translator scratch for the few
//! multi-key assembled fields). All business logic -- casts, unit conversions, enum parsing, fixups
//! -- lives in the subsystem modules, not here.
//!
//! Methods that can fail (enum parsing, address parsing, JSON parsing, `ByteSize` parsing) call the
//! subsystem function, and on `Err` record the error via `Translator::record_error`; the driver
//! surfaces the first recorded error. On error the native field keeps its seeded/default value.

pub mod checks;
pub mod control;
pub mod dogstatsd;
pub mod events;
pub mod forwarder;
pub mod logs;
pub mod metrics;
pub mod otlp;
pub mod service_checks;
pub mod traces;
pub mod workload;

use std::collections::HashMap;

use datadog_agent_config::{DatadogConfigConsumer, TranslateError};

use crate::translate::Translator;

impl Translator {
    /// Runs a fallible subsystem setter and records any error it returns.
    fn try_set(&mut self, result: Result<(), TranslateError>) {
        if let Err(e) = result {
            self.record_error(e);
        }
    }
}

impl DatadogConfigConsumer for Translator {
    // ----- forwarder: endpoint scratch -----

    fn consume_api_key(&mut self, value: String) {
        self.set_api_key_scratch(value);
    }

    fn consume_dd_url(&mut self, value: String) {
        self.set_dd_url_scratch(value);
    }

    fn consume_site(&mut self, value: String) {
        self.set_site_scratch(value);
    }

    fn consume_additional_endpoints(&mut self, value: HashMap<String, Vec<String>>) {
        self.set_additional_endpoints_scratch(value);
    }

    // ----- forwarder: scalar fields -----

    fn consume_forwarder_timeout(&mut self, value: i64) {
        forwarder::set_request_timeout_secs(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_max_concurrent_requests(&mut self, value: i64) {
        forwarder::set_endpoint_concurrency(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_num_workers(&mut self, value: i64) {
        forwarder::set_endpoint_concurrency_multiplier(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_high_prio_buffer_size(&mut self, value: i64) {
        forwarder::set_endpoint_buffer_size(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_connection_reset_interval(&mut self, value: i64) {
        forwarder::set_connection_reset_interval_secs(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_http_protocol(&mut self, value: String) {
        forwarder::set_http_protocol(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_min_tls_version(&mut self, value: String) {
        forwarder::set_min_tls_version(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_skip_ssl_validation(&mut self, value: bool) {
        forwarder::set_skip_ssl_validation(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_sslkeylogfile(&mut self, value: String) {
        forwarder::set_sslkeylogfile(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_allow_arbitrary_tags(&mut self, value: bool) {
        forwarder::set_allow_arbitrary_tags(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_forwarder_apikey_validation_interval(&mut self, value: i64) {
        forwarder::set_api_key_validation_interval_mins(&mut self.native_mut().components.forwarder.datadog, value);
    }

    // ----- forwarder: retry -----

    fn consume_forwarder_backoff_factor(&mut self, value: i64) {
        forwarder::set_backoff_factor(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_backoff_base(&mut self, value: i64) {
        forwarder::set_backoff_base(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_backoff_max(&mut self, value: i64) {
        forwarder::set_backoff_max(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_recovery_interval(&mut self, value: i64) {
        forwarder::set_recovery_error_decrease_factor(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_recovery_reset(&mut self, value: bool) {
        forwarder::set_recovery_reset(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_retry_queue_payloads_max_size(&mut self, value: i64) {
        forwarder::set_retry_queue_payloads_max_size(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_retry_queue_max_size(&mut self, value: i64) {
        forwarder::set_retry_queue_max_size(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_storage_max_size_in_bytes(&mut self, value: i64) {
        forwarder::set_storage_max_size_bytes(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_flush_to_disk_mem_ratio(&mut self, value: f64) {
        forwarder::set_flush_to_disk_mem_ratio(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_storage_path(&mut self, value: String) {
        forwarder::set_storage_path(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_storage_max_disk_ratio(&mut self, value: f64) {
        forwarder::set_storage_max_disk_ratio(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_outdated_file_in_days(&mut self, value: i64) {
        forwarder::set_outdated_file_in_days(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    fn consume_forwarder_retry_queue_capacity_time_interval_sec(&mut self, value: i64) {
        forwarder::set_capacity_time_interval_secs(&mut self.native_mut().components.forwarder.datadog.retry, value);
    }

    // ----- forwarder: proxy -----

    fn consume_proxy_http(&mut self, value: String) {
        forwarder::set_proxy_http(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_proxy_https(&mut self, value: String) {
        forwarder::set_proxy_https(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_proxy_no_proxy(&mut self, value: Vec<String>) {
        forwarder::set_proxy_no_proxy(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_no_proxy_nonexact_match(&mut self, value: bool) {
        forwarder::set_no_proxy_nonexact_match(&mut self.native_mut().components.forwarder.datadog, value);
    }

    fn consume_use_proxy_for_cloud_metadata(&mut self, value: bool) {
        forwarder::set_use_proxy_for_cloud_metadata(&mut self.native_mut().components.forwarder.datadog, value);
    }

    // ----- forwarder: OPW / Vector metrics routing -----

    fn consume_observability_pipelines_worker_metrics_enabled(&mut self, value: bool) {
        forwarder::set_opw_metrics_enabled(&mut self.native_mut().components.forwarder.datadog.opw_metrics, value);
    }

    fn consume_observability_pipelines_worker_metrics_url(&mut self, value: String) {
        forwarder::set_opw_metrics_url(&mut self.native_mut().components.forwarder.datadog.opw_metrics, value);
    }

    fn consume_vector_metrics_enabled(&mut self, value: bool) {
        forwarder::set_vector_metrics_enabled(&mut self.native_mut().components.forwarder.datadog.opw_metrics, value);
    }

    fn consume_vector_metrics_url(&mut self, value: String) {
        forwarder::set_vector_metrics_url(&mut self.native_mut().components.forwarder.datadog.opw_metrics, value);
    }

    // ----- metrics: multi-region failover -----

    fn consume_multi_region_failover_enabled(&mut self, value: bool) {
        forwarder::set_mrf_enabled(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    fn consume_multi_region_failover_failover_metrics(&mut self, value: bool) {
        forwarder::set_mrf_failover_metrics(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    fn consume_multi_region_failover_metric_allowlist(&mut self, value: Vec<String>) {
        forwarder::set_mrf_metric_allowlist(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    fn consume_multi_region_failover_api_key(&mut self, value: String) {
        forwarder::set_mrf_api_key(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    fn consume_multi_region_failover_dd_url(&mut self, value: String) {
        forwarder::set_mrf_dd_url(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    fn consume_multi_region_failover_site(&mut self, value: String) {
        forwarder::set_mrf_site(&mut self.native_mut().components.metrics.multi_region_failover, value);
    }

    // ----- metrics encoder (serializer.* fan-in to metrics, events, service_checks) -----

    fn consume_serializer_max_series_points_per_payload(&mut self, value: i64) {
        metrics::set_max_series_points_per_payload(&mut self.native_mut().components.metrics.datadog_encoder, value);
    }

    fn consume_serializer_max_payload_size(&mut self, value: i64) {
        let native = self.native_mut();
        metrics::set_max_payload_size(&mut native.components.metrics.datadog_encoder, value);
        events::set_max_payload_size(&mut native.components.events.encoder, value);
        service_checks::set_max_payload_size(&mut native.components.service_checks.encoder, value);
    }

    fn consume_serializer_max_uncompressed_payload_size(&mut self, value: i64) {
        let native = self.native_mut();
        metrics::set_max_uncompressed_payload_size(&mut native.components.metrics.datadog_encoder, value);
        events::set_max_uncompressed_payload_size(&mut native.components.events.encoder, value);
        service_checks::set_max_uncompressed_payload_size(&mut native.components.service_checks.encoder, value);
    }

    fn consume_serializer_max_series_payload_size(&mut self, value: i64) {
        metrics::set_max_series_payload_size(&mut self.native_mut().components.metrics.datadog_encoder, value);
    }

    fn consume_serializer_max_series_uncompressed_payload_size(&mut self, value: i64) {
        metrics::set_max_series_uncompressed_payload_size(
            &mut self.native_mut().components.metrics.datadog_encoder,
            value,
        );
    }

    fn consume_serializer_compressor_kind(&mut self, value: String) {
        let native = self.native_mut();
        metrics::set_compressor_kind(&mut native.components.metrics.datadog_encoder, value.clone());
        events::set_compressor_kind(&mut native.components.events.encoder, value.clone());
        service_checks::set_compressor_kind(&mut native.components.service_checks.encoder, value.clone());
        logs::set_compressor_kind(&mut native.components.logs.encoder, value.clone());
        traces::set_compressor_kind(&mut native.components.traces.encoder, value);
    }

    fn consume_serializer_zstd_compressor_level(&mut self, value: i64) {
        let native = self.native_mut();
        metrics::set_zstd_compressor_level(&mut native.components.metrics.datadog_encoder, value);
        events::set_zstd_compressor_level(&mut native.components.events.encoder, value);
        service_checks::set_zstd_compressor_level(&mut native.components.service_checks.encoder, value);
        logs::set_zstd_compressor_level(&mut native.components.logs.encoder, value);
        traces::set_zstd_compressor_level(&mut native.components.traces.encoder, value);
    }

    fn consume_use_v2_api_series(&mut self, value: bool) {
        metrics::set_use_v2_api_series(&mut self.native_mut().components.metrics.datadog_encoder, value);
    }

    fn consume_log_payloads(&mut self, value: bool) {
        let native = self.native_mut();
        metrics::set_log_payloads(&mut native.components.metrics.datadog_encoder, value);
        events::set_log_payloads(&mut native.components.events.encoder, value);
        service_checks::set_log_payloads(&mut native.components.service_checks.encoder, value);
    }

    // ----- traces encoder / apm / obfuscation -----

    fn consume_env(&mut self, value: String) {
        traces::set_env(self, value);
    }

    fn consume_otlp_config_traces_probabilistic_sampler_sampling_percentage(&mut self, value: f64) {
        // Drives both the OTLP traces sampling percentage (fanned out to source + decoder) and the
        // trace sampler's normalized OTLP sampling rate.
        traces::set_otlp_sampling_rate(&mut self.native_mut().components.traces.sampler, value);
        otlp::set_traces_sampling_percentage(self, value);
    }

    fn consume_apm_config_obfuscation_credit_cards_enabled(&mut self, value: bool) {
        traces::set_obfuscation_credit_cards_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_credit_cards_keep_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_credit_cards_keep_values(self, value);
    }

    fn consume_apm_config_obfuscation_credit_cards_luhn(&mut self, value: bool) {
        traces::set_obfuscation_credit_cards_luhn(self, value);
    }

    fn consume_apm_config_obfuscation_elasticsearch_enabled(&mut self, value: bool) {
        traces::set_obfuscation_elasticsearch_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_elasticsearch_keep_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_elasticsearch_keep_values(self, value);
    }

    fn consume_apm_config_obfuscation_elasticsearch_obfuscate_sql_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_elasticsearch_obfuscate_sql_values(self, value);
    }

    fn consume_apm_config_obfuscation_http_remove_paths_with_digits(&mut self, value: bool) {
        traces::set_obfuscation_http_remove_paths_with_digits(self, value);
    }

    fn consume_apm_config_obfuscation_http_remove_query_string(&mut self, value: bool) {
        traces::set_obfuscation_http_remove_query_string(self, value);
    }

    fn consume_apm_config_obfuscation_memcached_enabled(&mut self, value: bool) {
        traces::set_obfuscation_memcached_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_memcached_keep_command(&mut self, value: bool) {
        traces::set_obfuscation_memcached_keep_command(self, value);
    }

    fn consume_apm_config_obfuscation_mongodb_enabled(&mut self, value: bool) {
        traces::set_obfuscation_mongodb_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_mongodb_keep_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_mongodb_keep_values(self, value);
    }

    fn consume_apm_config_obfuscation_mongodb_obfuscate_sql_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_mongodb_obfuscate_sql_values(self, value);
    }

    fn consume_apm_config_obfuscation_opensearch_enabled(&mut self, value: bool) {
        traces::set_obfuscation_opensearch_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_opensearch_keep_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_opensearch_keep_values(self, value);
    }

    fn consume_apm_config_obfuscation_opensearch_obfuscate_sql_values(&mut self, value: Vec<String>) {
        traces::set_obfuscation_opensearch_obfuscate_sql_values(self, value);
    }

    fn consume_apm_config_obfuscation_redis_enabled(&mut self, value: bool) {
        traces::set_obfuscation_redis_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_redis_remove_all_args(&mut self, value: bool) {
        traces::set_obfuscation_redis_remove_all_args(self, value);
    }

    fn consume_apm_config_obfuscation_valkey_enabled(&mut self, value: bool) {
        traces::set_obfuscation_valkey_enabled(self, value);
    }

    fn consume_apm_config_obfuscation_valkey_remove_all_args(&mut self, value: bool) {
        traces::set_obfuscation_valkey_remove_all_args(self, value);
    }

    // ----- dogstatsd source -----

    fn consume_dogstatsd_buffer_size(&mut self, value: i64) {
        dogstatsd::set_buffer_size(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_port(&mut self, value: i64) {
        dogstatsd::set_port(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_so_rcvbuf(&mut self, value: i64) {
        dogstatsd::set_socket_receive_buffer_size(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_socket(&mut self, value: Option<String>) {
        dogstatsd::set_socket_path(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_stream_socket(&mut self, value: String) {
        dogstatsd::set_socket_stream_path(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_stream_log_too_big(&mut self, value: bool) {
        dogstatsd::set_stream_log_too_big(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_eol_required(&mut self, value: Vec<String>) {
        dogstatsd::set_eol_required(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_bind_host(&mut self, value: String) {
        dogstatsd::set_bind_host(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_non_local_traffic(&mut self, value: bool) {
        dogstatsd::set_non_local_traffic(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_string_interner_size(&mut self, value: i64) {
        dogstatsd::set_context_string_interner_entry_count(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_context_expiry_seconds(&mut self, value: i64) {
        dogstatsd::set_context_expiry_seconds(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_tags(&mut self, value: Vec<String>) {
        dogstatsd::set_additional_tags(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_capture_path(&mut self, value: String) {
        dogstatsd::set_capture_path(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_capture_depth(&mut self, value: i64) {
        dogstatsd::set_capture_depth(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_provider_kind(&mut self, value: String) {
        dogstatsd::set_provider_kind(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_statsd_forward_host(&mut self, value: String) {
        dogstatsd::set_statsd_forward_host(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_statsd_forward_port(&mut self, value: i64) {
        dogstatsd::set_statsd_forward_port(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_no_aggregation_pipeline(&mut self, value: bool) {
        dogstatsd::set_no_aggregation_pipeline_support(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_enable_payloads_series(&mut self, value: bool) {
        dogstatsd::set_enable_payloads_series(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_enable_payloads_sketches(&mut self, value: bool) {
        dogstatsd::set_enable_payloads_sketches(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_enable_payloads_events(&mut self, value: bool) {
        dogstatsd::set_enable_payloads_events(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_enable_payloads_service_checks(&mut self, value: bool) {
        dogstatsd::set_enable_payloads_service_checks(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_origin_detection(&mut self, value: bool) {
        dogstatsd::set_origin_detection(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_entity_id_precedence(&mut self, value: bool) {
        dogstatsd::set_entity_id_precedence(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_origin_detection_unified(&mut self, value: bool) {
        dogstatsd::set_origin_detection_unified(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_origin_optout_enabled(&mut self, value: bool) {
        dogstatsd::set_origin_detection_optout(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_origin_detection_client(&mut self, value: bool) {
        dogstatsd::set_origin_detection_client(&mut self.native_mut().components.dogstatsd.source, value);
    }

    fn consume_dogstatsd_tag_cardinality(&mut self, value: String) {
        let r = dogstatsd::set_tag_cardinality(&mut self.native_mut().components.dogstatsd.source, value);
        self.try_set(r);
    }

    // ----- dogstatsd mapper -----

    fn consume_dogstatsd_mapper_cache_size(&mut self, value: i64) {
        dogstatsd::set_mapper_cache_size(&mut self.native_mut().components.dogstatsd.mapper, value);
    }

    fn consume_dogstatsd_mapper_profiles(&mut self, value: Vec<serde_json::Value>) {
        let r = dogstatsd::set_mapper_profiles(&mut self.native_mut().components.dogstatsd.mapper, value);
        self.try_set(r);
    }

    // ----- dogstatsd debug log -----

    fn consume_dogstatsd_metrics_stats_enable(&mut self, value: bool) {
        dogstatsd::set_metrics_stats_enabled(&mut self.native_mut().components.dogstatsd.debug_log, value);
    }

    fn consume_dogstatsd_logging_enabled(&mut self, value: bool) {
        dogstatsd::set_logging_enabled(&mut self.native_mut().components.dogstatsd.debug_log, value);
    }

    fn consume_dogstatsd_log_file(&mut self, value: String) {
        dogstatsd::set_log_file(&mut self.native_mut().components.dogstatsd.debug_log, value);
    }

    fn consume_dogstatsd_log_file_max_size(&mut self, value: String) {
        let r = dogstatsd::set_log_file_max_size(&mut self.native_mut().components.dogstatsd.debug_log, value);
        self.try_set(r);
    }

    fn consume_dogstatsd_log_file_max_rolls(&mut self, value: i64) {
        dogstatsd::set_log_file_max_rolls(&mut self.native_mut().components.dogstatsd.debug_log, value);
    }

    // ----- dogstatsd aggregate (histogram) -----

    fn consume_histogram_aggregates(&mut self, value: Vec<String>) {
        let native = self.native_mut();
        dogstatsd::set_histogram_aggregates(&mut native.components.dogstatsd.aggregate, value.clone());
        dogstatsd::set_post_histogram_aggregates(&mut native.components.dogstatsd.post_aggregate_filter, value);
    }

    fn consume_dogstatsd_flush_incomplete_buckets(&mut self, value: bool) {
        dogstatsd::set_flush_incomplete_buckets(&mut self.native_mut().components.dogstatsd.aggregate, value);
    }

    fn consume_histogram_copy_to_distribution(&mut self, value: bool) {
        dogstatsd::set_histogram_copy_to_distribution(&mut self.native_mut().components.dogstatsd.aggregate, value);
    }

    fn consume_histogram_copy_to_distribution_prefix(&mut self, value: String) {
        dogstatsd::set_histogram_copy_to_distribution_prefix(
            &mut self.native_mut().components.dogstatsd.aggregate,
            value,
        );
    }

    fn consume_data_plane_dogstatsd_aggregator_tag_filter_cache_capacity(&mut self, value: i64) {
        dogstatsd::set_tag_filter_cache_capacity(&mut self.native_mut().components.dogstatsd.tag_filterlist, value);
    }

    // ----- dogstatsd prefix + post-aggregate filters -----

    fn consume_statsd_metric_namespace(&mut self, value: String) {
        dogstatsd::set_prefix_metric_prefix(&mut self.native_mut().components.dogstatsd.prefix_filter, value);
    }

    fn consume_statsd_metric_namespace_blacklist(&mut self, value: Vec<String>) {
        dogstatsd::set_prefix_metric_prefix_blocklist(&mut self.native_mut().components.dogstatsd.prefix_filter, value);
    }

    fn consume_metric_filterlist(&mut self, value: Vec<String>) {
        let native = self.native_mut();
        dogstatsd::set_prefix_metric_filterlist(&mut native.components.dogstatsd.prefix_filter, value.clone());
        dogstatsd::set_post_metric_filterlist(&mut native.components.dogstatsd.post_aggregate_filter, value);
    }

    fn consume_metric_filterlist_match_prefix(&mut self, value: bool) {
        let native = self.native_mut();
        dogstatsd::set_prefix_metric_filterlist_match_prefix(&mut native.components.dogstatsd.prefix_filter, value);
        dogstatsd::set_post_metric_filterlist_match_prefix(
            &mut native.components.dogstatsd.post_aggregate_filter,
            value,
        );
    }

    fn consume_statsd_metric_blocklist(&mut self, value: Vec<String>) {
        let native = self.native_mut();
        dogstatsd::set_prefix_metric_blocklist(&mut native.components.dogstatsd.prefix_filter, value.clone());
        dogstatsd::set_post_metric_blocklist(&mut native.components.dogstatsd.post_aggregate_filter, value);
    }

    fn consume_statsd_metric_blocklist_match_prefix(&mut self, value: bool) {
        let native = self.native_mut();
        dogstatsd::set_prefix_metric_blocklist_match_prefix(&mut native.components.dogstatsd.prefix_filter, value);
        dogstatsd::set_post_metric_blocklist_match_prefix(
            &mut native.components.dogstatsd.post_aggregate_filter,
            value,
        );
    }

    // ----- otlp -----

    fn consume_otlp_config_receiver_protocols_grpc_endpoint(&mut self, value: String) {
        otlp::set_grpc_endpoint(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_receiver_protocols_grpc_transport(&mut self, value: String) {
        otlp::set_grpc_transport(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib(&mut self, value: i64) {
        otlp::set_grpc_max_recv_msg_size_mib(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_receiver_protocols_http_endpoint(&mut self, value: String) {
        otlp::set_http_endpoint(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_metrics_enabled(&mut self, value: bool) {
        otlp::set_metrics_enabled(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_logs_enabled(&mut self, value: bool) {
        otlp::set_logs_enabled(&mut self.native_mut().components.otlp.source, value);
    }

    fn consume_otlp_config_traces_enabled(&mut self, value: bool) {
        otlp::set_traces_enabled(self, value);
    }

    fn consume_otlp_config_traces_internal_port(&mut self, value: i64) {
        otlp::set_traces_internal_port(self, value);
    }

    // ----- workload -----

    fn consume_cri_connection_timeout(&mut self, value: i64) {
        workload::set_cri_connection_timeout_secs(&mut self.native_mut().components.workload.config, value);
    }

    fn consume_cri_query_timeout(&mut self, value: i64) {
        workload::set_cri_query_timeout_secs(&mut self.native_mut().components.workload.config, value);
    }

    fn consume_agent_ipc_grpc_max_message_size(&mut self, value: i64) {
        workload::set_remote_agent_string_interner_size_bytes(&mut self.native_mut().components.workload.config, value);
    }

    // ----- control (data_plane.* + stop-timeout components) -----

    fn consume_data_plane_api_listen_address(&mut self, value: String) {
        let r = control::set_api_listen_address(&mut self.native_mut().control, value);
        self.try_set(r);
    }

    fn consume_data_plane_secure_api_listen_address(&mut self, value: String) {
        let r = control::set_secure_api_listen_address(&mut self.native_mut().control, value);
        self.try_set(r);
    }

    fn consume_data_plane_remote_agent_enabled(&mut self, value: bool) {
        control::set_remote_agent_enabled(&mut self.native_mut().control, value);
    }

    fn consume_data_plane_use_new_config_stream_endpoint(&mut self, value: bool) {
        control::set_use_new_config_stream_endpoint(&mut self.native_mut().control, value);
    }

    fn consume_data_plane_log_file(&mut self, value: String) {
        control::set_log_file(&mut self.native_mut().control, value);
    }

    fn consume_data_plane_otlp_proxy_traces_enabled(&mut self, value: bool) {
        control::set_otlp_proxy_traces_enabled(&mut self.native_mut().control, value);
    }

    fn consume_data_plane_otlp_proxy_metrics_enabled(&mut self, value: bool) {
        control::set_otlp_proxy_metrics_enabled(&mut self.native_mut().control, value);
    }

    fn consume_data_plane_otlp_proxy_logs_enabled(&mut self, value: bool) {
        control::set_otlp_proxy_logs_enabled(&mut self.native_mut().control, value);
    }

    fn consume_aggregator_stop_timeout(&mut self, value: i64) {
        control::set_aggregator_stop_timeout(self, value);
    }

    fn consume_forwarder_stop_timeout(&mut self, value: i64) {
        control::set_forwarder_stop_timeout(self, value);
    }

    // ----- bootstrap / logging / IPC keys with no runtime-model destination -----
    //
    // These keys are genuinely outside the translated runtime `SalukiConfiguration`. They configure
    // process logging, the Core Agent IPC port, or VSOCK transport, all of which are bootstrap-phase
    // concerns surfaced through `BootstrapConfiguration`, not component or control runtime config. We
    // deliberately accept them here as no-ops rather than inventing a runtime destination; see the
    // build report. They are still witnessed (so they cannot be silently dropped from the overlay),
    // and a future bootstrap-routing pass can attach them.

    fn consume_cmd_port(&mut self, _value: i64) {}

    fn consume_log_level(&mut self, _value: String) {}

    fn consume_log_format_rfc3339(&mut self, _value: bool) {}

    fn consume_syslog_rfc(&mut self, _value: bool) {}

    fn consume_syslog_uri(&mut self, _value: String) {}

    fn consume_vsock_addr(&mut self, _value: String) {}

    // ----- error surfacing -----

    fn translate_error(&mut self) -> Option<TranslateError> {
        self.errors.first().cloned()
    }
}
