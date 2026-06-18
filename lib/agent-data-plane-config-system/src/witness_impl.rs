use agent_data_plane_config::{PipelineGate, SalukiConfiguration};
use datadog_agent_config::{DatadogConfigConsumer, TranslateResult};
use saluki_component_config::{EndpointConfig, ListenAddress, MetricTagFilterEntry};
use serde_json::Value;

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
        let Some(value) = consume_control_key(self, key, value) else {
            return Ok(());
        };

        match key {
            "additional_endpoints" => self.consume_additional_endpoints_value(value),
            "allow_arbitrary_tags" => self.native.components.forwarder.datadog.allow_arbitrary_tags = bool_value(value),
            "api_key" => self.api_key = string_value(value),
            "apm_config.obfuscation.credit_cards.enabled" => {
                self.native.components.traces.obfuscation.credit_cards.enabled = bool_value(value);
            }
            "apm_config.obfuscation.credit_cards.keep_values" => {
                self.native.components.traces.obfuscation.credit_cards.keep_values = string_vec_value(value);
            }
            "apm_config.obfuscation.credit_cards.luhn" => {
                self.native.components.traces.obfuscation.credit_cards.luhn = bool_value(value);
            }
            "apm_config.obfuscation.elasticsearch.enabled" => {
                self.native.components.traces.obfuscation.elasticsearch.enabled = bool_value(value);
            }
            "apm_config.obfuscation.elasticsearch.keep_values" => {
                self.native.components.traces.obfuscation.elasticsearch.keep_values = string_vec_value(value);
            }
            "apm_config.obfuscation.elasticsearch.obfuscate_sql_values" => {
                self.native
                    .components
                    .traces
                    .obfuscation
                    .elasticsearch
                    .obfuscate_sql_values = string_vec_value(value);
            }
            "apm_config.obfuscation.http.remove_paths_with_digits" => {
                self.native.components.traces.obfuscation.http.remove_paths_with_digits = bool_value(value);
            }
            "apm_config.obfuscation.http.remove_query_string" => {
                self.native.components.traces.obfuscation.http.remove_query_string = bool_value(value);
            }
            "apm_config.obfuscation.memcached.enabled" => {
                self.native.components.traces.obfuscation.memcached.enabled = bool_value(value);
            }
            "apm_config.obfuscation.memcached.keep_command" => {
                self.native.components.traces.obfuscation.memcached.keep_command = bool_value(value);
            }
            "apm_config.obfuscation.mongodb.enabled" => {
                self.native.components.traces.obfuscation.mongodb.enabled = bool_value(value);
            }
            "apm_config.obfuscation.mongodb.keep_values" => {
                self.native.components.traces.obfuscation.mongodb.keep_values = string_vec_value(value);
            }
            "apm_config.obfuscation.mongodb.obfuscate_sql_values" => {
                self.native.components.traces.obfuscation.mongodb.obfuscate_sql_values = string_vec_value(value);
            }
            "apm_config.obfuscation.opensearch.enabled" => {
                self.native.components.traces.obfuscation.opensearch.enabled = bool_value(value);
            }
            "apm_config.obfuscation.opensearch.keep_values" => {
                self.native.components.traces.obfuscation.opensearch.keep_values = string_vec_value(value);
            }
            "apm_config.obfuscation.opensearch.obfuscate_sql_values" => {
                self.native
                    .components
                    .traces
                    .obfuscation
                    .opensearch
                    .obfuscate_sql_values = string_vec_value(value);
            }
            "apm_config.obfuscation.redis.enabled" => {
                self.native.components.traces.obfuscation.redis.enabled = bool_value(value);
            }
            "apm_config.obfuscation.redis.remove_all_args" => {
                self.native.components.traces.obfuscation.redis.remove_all_args = bool_value(value);
            }
            "apm_config.obfuscation.valkey.enabled" => {
                self.native.components.traces.obfuscation.valkey.enabled = bool_value(value);
            }
            "apm_config.obfuscation.valkey.remove_all_args" => {
                self.native.components.traces.obfuscation.valkey.remove_all_args = bool_value(value);
            }
            "dd_url" => self.dd_url = string_value(value),
            "bind_host" => {
                self.dogstatsd_bind_host = optional_string_value(value);
            }
            "cri_connection_timeout" => {
                self.native.components.workload.source.cri_connection_timeout_secs = u64_value(value, 0);
            }
            "cri_query_timeout" => {
                self.native.components.workload.source.cri_query_timeout_secs = u64_value(value, 0);
            }
            "dogstatsd_buffer_size" => {
                self.native.components.dogstatsd.source.buffer_size = usize_value(value, 8192);
            }
            "dogstatsd_capture_depth" => {
                self.native.components.dogstatsd.source.capture_depth = usize_value(value, 1024);
            }
            "dogstatsd_capture_path" => {
                self.native.components.dogstatsd.source.capture_path = string_value(value);
            }
            "dogstatsd_context_expiry_seconds" => {
                self.native.components.dogstatsd.source.context_expiry_seconds = u64_value(value, 20);
            }
            "dogstatsd_entity_id_precedence" => {
                self.native.components.dogstatsd.source.origin.entity_id_precedence = bool_value(value);
            }
            "dogstatsd_eol_required" => {
                self.native.components.dogstatsd.source.eol_required = string_vec_value(value);
            }
            "dogstatsd_flush_incomplete_buckets" => {
                self.native.components.dogstatsd.aggregate.flush_open_windows = bool_value(value);
            }
            "dogstatsd_no_aggregation_pipeline" => {
                let enabled = bool_value(value);
                self.native.components.dogstatsd.source.no_aggregation_pipeline_support = enabled;
                self.native
                    .components
                    .dogstatsd
                    .aggregate
                    .passthrough_timestamped_metrics = enabled;
            }
            "dogstatsd_non_local_traffic" => {
                let enabled = bool_value(value);
                self.native.components.dogstatsd.source.non_local_traffic = enabled;
                self.dogstatsd_non_local_traffic = enabled;
            }
            "dogstatsd_origin_detection" => {
                self.native.components.dogstatsd.source.origin.enabled = bool_value(value);
            }
            "dogstatsd_origin_detection_client" => {
                self.native.components.dogstatsd.source.origin.client_detection = bool_value(value);
            }
            "dogstatsd_origin_optout_enabled" => {
                self.native.components.dogstatsd.source.origin.optout_enabled = bool_value(value);
            }
            "dogstatsd_so_rcvbuf" => {
                self.native.components.dogstatsd.source.socket_receive_buffer_size = usize_value(value, 0);
            }
            "dogstatsd_stream_log_too_big" => {
                self.native.components.dogstatsd.source.stream_log_too_big = bool_value(value);
            }
            "dogstatsd_stream_socket" => {
                self.native.components.dogstatsd.source.socket_stream_path = optional_string_value(value);
            }
            "dogstatsd_string_interner_size" => {
                self.native
                    .components
                    .dogstatsd
                    .source
                    .context_string_interner_entry_count = u64_value(value, 4096);
            }
            "dogstatsd_tag_cardinality" => {
                self.native.components.dogstatsd.source.origin.tag_cardinality = string_value(value);
            }
            "enable_payloads.events" => {
                self.native.components.dogstatsd.source.enable_payloads.events = bool_value(value);
            }
            "enable_payloads.series" => {
                self.native.components.dogstatsd.source.enable_payloads.series = bool_value(value);
            }
            "enable_payloads.service_checks" => {
                self.native.components.dogstatsd.source.enable_payloads.service_checks = bool_value(value);
            }
            "enable_payloads.sketches" => {
                self.native.components.dogstatsd.source.enable_payloads.sketches = bool_value(value);
            }
            "origin_detection_unified" => {
                self.native.components.dogstatsd.source.origin.unified_detection = bool_value(value);
            }
            "provider_kind" => {
                self.native.components.dogstatsd.source.provider_kind = string_value(value);
            }
            "statsd_forward_host" => {
                self.native.components.dogstatsd.source.statsd_forward_host = optional_string_value(value);
            }
            "statsd_forward_port" => {
                self.native.components.dogstatsd.source.statsd_forward_port = u16_value(value, 0);
            }
            "dogstatsd_mapper_cache_size" => {
                self.native.components.dogstatsd.mapper.cache_size = usize_value(value, 1000);
            }
            "dogstatsd_mapper_profiles" => {
                self.native.components.dogstatsd.mapper.profiles = array_value(value);
            }
            "dogstatsd_logging_enabled" => {
                self.native.components.dogstatsd.debug_log.logging_enabled = bool_value(value);
            }
            "dogstatsd_log_file" => {
                self.native.components.dogstatsd.debug_log.log_file = string_value(value);
            }
            "dogstatsd_log_file_max_size" => {
                self.native.components.dogstatsd.debug_log.log_file_max_size_bytes = u64_value(value, 10 * 1024 * 1024);
            }
            "dogstatsd_log_file_max_rolls" => {
                self.native.components.dogstatsd.debug_log.log_file_max_rolls = usize_value(value, 3);
            }
            "dogstatsd_metrics_stats_enable" => {
                self.native.components.dogstatsd.debug_log.metrics_stats_enabled = bool_value(value);
            }
            "dogstatsd_port" => {
                let port = u16_value(value, 8125);
                self.native.components.dogstatsd.source.udp_address = ListenAddress::Udp(format!("127.0.0.1:{port}"));
            }
            "dogstatsd_socket" => {
                let socket = string_value(value);
                self.native.components.dogstatsd.source.socket_path = (!socket.is_empty()).then_some(socket);
            }
            "dogstatsd_tags" => self.native.components.dogstatsd.source.additional_tags = string_vec_value(value),
            "dogstatsd_string_interner_size_bytes" => {
                self.native
                    .components
                    .dogstatsd
                    .source
                    .context_string_interner_size_bytes = u64_value(value, 2 * 1024 * 1024);
            }
            "dogstatsd_cached_contexts_limit" => {
                self.native.components.dogstatsd.source.cached_contexts_limit = usize_value(value, 500_000);
            }
            "metric_filterlist" => {
                let values = string_vec_value(value);
                self.native.components.dogstatsd.prefix_filter.metric_filterlist = values.clone();
                self.native.components.dogstatsd.post_aggregate_filter.metric_filterlist = values;
            }
            "metric_filterlist_match_prefix" => {
                let value = bool_value(value);
                self.native
                    .components
                    .dogstatsd
                    .prefix_filter
                    .metric_filterlist_match_prefix = value;
                self.native
                    .components
                    .dogstatsd
                    .post_aggregate_filter
                    .metric_filterlist_match_prefix = value;
            }
            "statsd_metric_blocklist" => {
                let values = string_vec_value(value);
                self.native.components.dogstatsd.prefix_filter.metric_blocklist = values.clone();
                self.native.components.dogstatsd.post_aggregate_filter.metric_blocklist = values;
            }
            "statsd_metric_blocklist_match_prefix" => {
                let value = bool_value(value);
                self.native
                    .components
                    .dogstatsd
                    .prefix_filter
                    .metric_blocklist_match_prefix = value;
                self.native
                    .components
                    .dogstatsd
                    .post_aggregate_filter
                    .metric_blocklist_match_prefix = value;
            }
            "metric_tag_filterlist" => {
                self.native.components.dogstatsd.tag_filterlist.entries = metric_tag_filter_entries(value);
            }
            "data_plane.dogstatsd.aggregator_tag_filter_cache_capacity" => {
                self.native.components.dogstatsd.tag_filterlist.cache_capacity = usize_value(value, 100_000);
            }
            "otlp_config.logs.enabled" => {
                let enabled = bool_value(value);
                self.native.control.otlp.logs.enabled = enabled;
                self.native.components.otlp.source.logs_enabled = enabled;
            }
            "otlp_config.metrics.enabled" => {
                let enabled = bool_value(value);
                self.native.control.otlp.metrics.enabled = enabled;
                self.native.components.otlp.source.metrics_enabled = enabled;
            }
            "otlp_config.traces.enabled" => {
                let enabled = bool_value(value);
                self.native.control.otlp.traces.enabled = enabled;
                self.native.components.otlp.source.traces_enabled = enabled;
            }
            "otlp_config.receiver.protocols.grpc.endpoint" => {
                self.native.components.otlp.source.grpc_endpoint = string_value(value);
            }
            "otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib" => {
                self.native.components.otlp.source.grpc_max_recv_msg_size_mib = u64_value(value, 4);
            }
            "otlp_config.receiver.protocols.grpc.transport" => {
                self.native.components.otlp.source.grpc_transport = string_value(value);
            }
            "otlp_config.receiver.protocols.http.endpoint" => {
                self.native.components.otlp.source.http_endpoint = string_value(value);
            }
            "otlp_config.traces.internal_port" => {
                self.native.components.otlp.source.traces_internal_port = u16_value(value, 5003);
            }
            "otlp_config.traces.probabilistic_sampler.sampling_percentage" => {
                let percentage = f64_value(value, 100.0);
                self.native.components.otlp.source.traces_sampling_percentage = percentage;
                self.native.components.traces.sampler.rate = percentage / 100.0;
            }
            "forwarder_apikey_validation_interval" => {
                self.native
                    .components
                    .forwarder
                    .datadog
                    .api_key_validation_interval_mins = i64_value(value, 60);
            }
            "forwarder_backoff_base" => {
                self.native.components.forwarder.datadog.retry.backoff_base_secs = f64_value(value, 2.0);
            }
            "forwarder_backoff_factor" => {
                self.native.components.forwarder.datadog.retry.backoff_factor = f64_value(value, 2.0);
            }
            "forwarder_backoff_max" => {
                self.native.components.forwarder.datadog.retry.backoff_max_secs = f64_value(value, 64.0);
            }
            "forwarder_connection_reset_interval" => {
                self.native.components.forwarder.datadog.connection_reset_interval_secs = u64_value(value, 0);
            }
            "forwarder_flush_to_disk_mem_ratio" => {
                self.native.components.forwarder.datadog.retry.flush_to_disk_mem_ratio = f64_value(value, 0.5);
            }
            "forwarder_high_prio_buffer_size" => {
                self.native.components.forwarder.datadog.endpoint_buffer_size = usize_value(value, 100);
            }
            "forwarder_http_protocol" => {
                self.native.components.forwarder.datadog.http_protocol = string_value(value);
            }
            "forwarder_max_concurrent_requests" => {
                self.native.components.forwarder.datadog.endpoint_concurrency = usize_value(value, 10);
            }
            "forwarder_num_workers" => {
                self.native.components.forwarder.datadog.endpoint_concurrency_multiplier = usize_value(value, 1);
            }
            "forwarder_outdated_file_in_days" => {
                self.native.components.forwarder.datadog.retry.outdated_file_in_days = u32_value(value, 10);
            }
            "forwarder_recovery_interval" => {
                self.native.components.forwarder.datadog.retry.recovery_interval = u32_value(value, 2);
            }
            "forwarder_recovery_reset" => {
                self.native.components.forwarder.datadog.retry.recovery_reset = bool_value(value);
            }
            "forwarder_retry_queue_capacity_time_interval_sec" => {
                self.native
                    .components
                    .forwarder
                    .datadog
                    .retry
                    .capacity_time_interval_secs = u64_value(value, 15 * 60);
            }
            "forwarder_retry_queue_max_size" => {
                self.native
                    .components
                    .forwarder
                    .datadog
                    .retry
                    .retry_queue_max_size_bytes = Some(u64_value(value, 0));
            }
            "forwarder_retry_queue_payloads_max_size" => {
                self.native
                    .components
                    .forwarder
                    .datadog
                    .retry
                    .retry_queue_payloads_max_size_bytes = Some(u64_value(value, 15 * 1024 * 1024));
            }
            "forwarder_storage_max_disk_ratio" => {
                self.native.components.forwarder.datadog.retry.storage_max_disk_ratio = f64_value(value, 0.8);
            }
            "forwarder_storage_max_size_in_bytes" => {
                self.native.components.forwarder.datadog.retry.max_disk_size_bytes = u64_value(value, 0);
            }
            "forwarder_storage_path" => {
                self.native.components.forwarder.datadog.retry.storage_path = string_value(value);
            }
            "forwarder_timeout" => {
                self.native.components.forwarder.datadog.request_timeout_millis =
                    u64_value(value, 20).saturating_mul(1000);
            }
            "histogram_aggregates" => {
                self.native
                    .components
                    .dogstatsd
                    .post_aggregate_filter
                    .histogram_aggregates = string_vec_value(value);
            }
            "histogram_copy_to_distribution" => {
                self.native
                    .components
                    .dogstatsd
                    .aggregate
                    .histogram_copy_to_distribution = bool_value(value);
            }
            "histogram_copy_to_distribution_prefix" => {
                self.native
                    .components
                    .dogstatsd
                    .aggregate
                    .histogram_copy_to_distribution_prefix = string_value(value);
            }
            "histogram_percentiles" => {
                self.native
                    .components
                    .dogstatsd
                    .post_aggregate_filter
                    .histogram_percentiles = string_vec_value(value);
            }
            "log_payloads" => {
                self.native.components.metrics.datadog_encoder.log_payloads = bool_value(value);
            }
            "min_tls_version" => {
                self.native.components.forwarder.datadog.min_tls_version = string_value(value);
            }
            "no_proxy_nonexact_match" => {
                self.native.components.forwarder.datadog.proxy.no_proxy_nonexact_match = bool_value(value);
            }
            "observability_pipelines_worker.metrics.enabled" => {
                self.native.components.forwarder.datadog.opw_metrics.opw_enabled = bool_value(value);
            }
            "observability_pipelines_worker.metrics.url" => {
                self.native.components.forwarder.datadog.opw_metrics.opw_url = string_value(value);
            }
            "proxy.http" => {
                self.native.components.forwarder.datadog.proxy.http = optional_string_value(value);
            }
            "proxy.https" => {
                self.native.components.forwarder.datadog.proxy.https = optional_string_value(value);
            }
            "proxy.no_proxy" => {
                self.native.components.forwarder.datadog.proxy.no_proxy = string_vec_value(value);
            }
            "serializer_compressor_kind" => {
                self.native.components.metrics.datadog_encoder.compressor_kind = string_value(value);
            }
            "serializer_max_payload_size" => {
                self.native.components.metrics.datadog_encoder.max_payload_size = usize_value(value, 2_621_440);
            }
            "serializer_max_series_payload_size" => {
                self.native.components.metrics.datadog_encoder.max_series_payload_size = usize_value(value, 512_000);
            }
            "serializer_max_series_points_per_payload" => {
                self.native
                    .components
                    .metrics
                    .datadog_encoder
                    .max_series_points_per_payload = usize_value(value, 10_000);
            }
            "serializer_max_series_uncompressed_payload_size" => {
                self.native
                    .components
                    .metrics
                    .datadog_encoder
                    .max_series_uncompressed_payload_size = usize_value(value, 5_242_880);
            }
            "serializer_max_uncompressed_payload_size" => {
                self.native
                    .components
                    .metrics
                    .datadog_encoder
                    .max_uncompressed_payload_size = usize_value(value, 4_194_304);
            }
            "serializer_zstd_compressor_level" => {
                self.native.components.metrics.datadog_encoder.compression_level = i32_value(value, 3);
            }
            "skip_ssl_validation" => {
                self.native.components.forwarder.datadog.tls.verify = !bool_value(value);
            }
            "sslkeylogfile" => {
                self.native.components.forwarder.datadog.ssl_key_log_file = string_value(value);
            }
            "statsd_metric_namespace" => {
                self.native.components.dogstatsd.prefix_filter.metric_prefix = string_value(value);
            }
            "statsd_metric_namespace_blacklist" => {
                self.native.components.dogstatsd.prefix_filter.metric_prefix_blocklist = string_vec_value(value);
            }
            "statsd_metric_namespace_blocklist" => {
                self.native.components.dogstatsd.prefix_filter.metric_prefix_blocklist = string_vec_value(value);
            }
            "use_proxy_for_cloud_metadata" => {
                self.native
                    .components
                    .forwarder
                    .datadog
                    .proxy
                    .use_proxy_for_cloud_metadata = bool_value(value);
            }
            "use_v2_api.series" => {
                self.native.components.metrics.datadog_encoder.use_v2_api_series = bool_value(value);
            }
            "vector.metrics.enabled" => {
                self.native.components.forwarder.datadog.opw_metrics.vector_enabled = bool_value(value);
            }
            "vector.metrics.url" => {
                self.native.components.forwarder.datadog.opw_metrics.vector_url = string_value(value);
            }
            "url" => self.dd_url = string_value(value),
            "site" => {
                let site = string_value(value);
                if !site.is_empty() {
                    self.dd_url = format!("https://app.{site}");
                }
            }
            "multi_region_failover.enabled" => {
                self.native.components.metrics.multi_region_failover.enabled = bool_value(value);
            }
            "multi_region_failover.failover_metrics" => {
                self.native.components.metrics.multi_region_failover.failover_metrics = bool_value(value);
            }
            "multi_region_failover.api_key" => {
                self.native.components.metrics.multi_region_failover.api_key = Some(string_value(value));
            }
            "multi_region_failover.dd_url" => {
                self.native.components.metrics.multi_region_failover.endpoint = Some(string_value(value));
            }
            "multi_region_failover.site" => {
                let site = string_value(value);
                if !site.is_empty() {
                    self.native.components.metrics.multi_region_failover.endpoint =
                        Some(format!("https://app.mrf.{site}"));
                }
            }
            "multi_region_failover.metric_allowlist" => {
                self.native.components.metrics.multi_region_failover.metric_allowlist = string_vec_value(value);
            }
            unknown => unreachable!("unhandled Datadog config witness key: {unknown}"),
        }
        Ok(())
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

fn consume_control_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "agent_ipc.grpc_max_message_size" => {
            translator.native.control.agent_ipc_grpc_max_message_size = Some(usize_value(value, 0));
        }
        "aggregator_stop_timeout" => translator.aggregator_stop_timeout_secs = u64_value(value, 2),
        "auth_token_file_path" => {
            translator.native.control.ipc_auth.auth_token_file_path = optional_string_value(value);
        }
        "cmd_port" => {
            translator.native.control.cmd_port = Some(u16_value(value, 5001));
        }
        "data_plane.api_listen_address" => {
            translator.native.control.api_listen_address = ListenAddress::Tcp(string_value(value));
        }
        "data_plane.dogstatsd.enabled" => {
            translator.native.control.dogstatsd = PipelineGate {
                enabled: bool_value(value),
            };
        }
        "data_plane.enabled" => {
            translator.native.control.enabled = bool_value(value);
        }
        "data_plane.log_file" => {
            translator.native.control.data_plane_log_file = optional_string_value(value);
        }
        "data_plane.otlp.enabled" => {
            translator.native.control.otlp.native = PipelineGate {
                enabled: bool_value(value),
            };
        }
        "data_plane.otlp.proxy.enabled" => {
            translator.native.control.otlp.proxy.enabled = bool_value(value);
        }
        "data_plane.otlp.proxy.logs.enabled" => {
            translator.native.control.otlp.logs = PipelineGate {
                enabled: bool_value(value),
            };
        }
        "data_plane.otlp.proxy.metrics.enabled" => {
            translator.native.control.otlp.metrics = PipelineGate {
                enabled: bool_value(value),
            };
        }
        "data_plane.otlp.proxy.receiver.protocols.grpc.endpoint" => {
            translator.native.control.otlp.proxy.core_agent_otlp_grpc_endpoint = string_value(value);
        }
        "data_plane.otlp.proxy.traces.enabled" => {
            translator.native.control.otlp.traces = PipelineGate {
                enabled: bool_value(value),
            };
        }
        "data_plane.remote_agent_enabled" => translator.native.control.remote_agent_enabled = bool_value(value),
        "data_plane.secure_api_listen_address" => {
            translator.native.control.secure_api_listen_address = ListenAddress::Tcp(string_value(value));
        }
        "data_plane.use_new_config_stream_endpoint" => {
            translator.native.control.use_new_config_stream_endpoint = bool_value(value);
        }
        "env" => {
            translator.native.control.env = optional_string_value(value);
        }
        "forwarder_stop_timeout" => translator.forwarder_stop_timeout_secs = u64_value(value, 2),
        "ipc_cert_file_path" => {
            translator.native.control.ipc_auth.ipc_cert_file_path = optional_string_value(value);
        }
        "log_format_rfc3339" => {
            translator.native.control.log_format_rfc3339 = Some(bool_value(value));
        }
        "log_level" => translator.native.control.log_level = Some(string_value(value)),
        "syslog_rfc" => {
            translator.native.control.syslog_rfc = Some(bool_value(value));
        }
        "syslog_uri" => {
            translator.native.control.syslog_uri = optional_string_value(value);
        }
        "vsock_addr" => {
            translator.native.control.vsock_addr = optional_string_value(value);
        }
        _ => return Some(value),
    }
    None
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
