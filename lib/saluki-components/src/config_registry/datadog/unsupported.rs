//! Annotations for configuration keys that Saluki doesn't support.
use crate::config_registry::{generated::schema, SalukiAnnotation, Severity, SupportLevel};

crate::declare_annotations! {
    /// `allow_arbitrary_tags` - signal backend tag validation relaxation.
    ALLOW_ARBITRARY_TAGS = SalukiAnnotation {
        schema: &schema::ALLOW_ARBITRARY_TAGS,
        // Not implemented. #1377
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `cri_connection_timeout` - CRI runtime connection timeout.
    CRI_CONNECTION_TIMEOUT = SalukiAnnotation {
        schema: &schema::CRI_CONNECTION_TIMEOUT,
        // Not implemented. ADP hardcodes CRI timeout. #1348
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `cri_query_timeout` - CRI runtime query timeout.
    CRI_QUERY_TIMEOUT = SalukiAnnotation {
        schema: &schema::CRI_QUERY_TIMEOUT,
        // Not implemented. ADP hardcodes CRI timeout. #1348
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_disable_verbose_logs` - suppress noisy parse error logs.
    DOGSTATSD_DISABLE_VERBOSE_LOGS = SalukiAnnotation {
        schema: &schema::DOGSTATSD_DISABLE_VERBOSE_LOGS,
        // ADP logs parse failures unconditionally, no toggle. #1350
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_pipe_name` - Windows named pipe path.
    DOGSTATSD_PIPE_NAME = SalukiAnnotation {
        schema: &schema::DOGSTATSD_PIPE_NAME,
        // Windows support not implemented. #1466
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_stats_buffer` - internal stats buffer size.
    DOGSTATSD_STATS_BUFFER = SalukiAnnotation {
        schema: &schema::DOGSTATSD_STATS_BUFFER,
        // No expvar stats endpoint in ADP. #1352
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_stats_enable` - enable internal stats endpoint.
    DOGSTATSD_STATS_ENABLE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_STATS_ENABLE,
        // No expvar stats endpoint in ADP. #1352
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_stats_port` - internal stats endpoint port.
    DOGSTATSD_STATS_PORT = SalukiAnnotation {
        schema: &schema::DOGSTATSD_STATS_PORT,
        // No expvar stats endpoint in ADP. #1352
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_telemetry_enabled_listener_id` - per-listener telemetry tagging.
    DOGSTATSD_TELEMETRY_ENABLED_LISTENER_ID = SalukiAnnotation {
        schema: &schema::DOGSTATSD_TELEMETRY_ENABLED_LISTENER_ID,
        // Not feasible to thread listener ID through ADP topology. Not planned.
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_windows_pipe_security_descriptor` - Windows named pipe ACL.
    DOGSTATSD_WINDOWS_PIPE_SECURITY_DESCRIPTOR = SalukiAnnotation {
        schema: &schema::DOGSTATSD_WINDOWS_PIPE_SECURITY_DESCRIPTOR,
        // Windows support not implemented. #1466
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_apikey_validation_interval` - API key check interval.
    FORWARDER_APIKEY_VALIDATION_INTERVAL = SalukiAnnotation {
        schema: &schema::FORWARDER_APIKEY_VALIDATION_INTERVAL,
        // Runtime API key refresh not implemented. #1357
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_flush_to_disk_mem_ratio` - mem-to-disk flush threshold.
    FORWARDER_FLUSH_TO_DISK_MEM_RATIO = SalukiAnnotation {
        schema: &schema::FORWARDER_FLUSH_TO_DISK_MEM_RATIO,
        // Not implemented. #1364
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_http_protocol` - HTTP version selection.
    FORWARDER_HTTP_PROTOCOL = SalukiAnnotation {
        schema: &schema::FORWARDER_HTTP_PROTOCOL,
        // Not implemented. #1361
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_low_prio_buffer_size` - low-priority request queue size.
    FORWARDER_LOW_PRIO_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_LOW_PRIO_BUFFER_SIZE,
        // ADP has no separate low-priority queue. #1362
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_max_concurrent_requests` - max concurrent HTTP requests.
    FORWARDER_MAX_CONCURRENT_REQUESTS = SalukiAnnotation {
        schema: &schema::FORWARDER_MAX_CONCURRENT_REQUESTS,
        // ADP concurrency model differs. #1363
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_outdated_file_in_days` - retry file retention period.
    FORWARDER_OUTDATED_FILE_IN_DAYS = SalukiAnnotation {
        schema: &schema::FORWARDER_OUTDATED_FILE_IN_DAYS,
        // Retry file retention not implemented. #1360
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_retry_queue_capacity_time_interval_sec` - retry queue time-based capacity.
    FORWARDER_RETRY_QUEUE_CAPACITY_TIME_INTERVAL_SEC = SalukiAnnotation {
        schema: &schema::FORWARDER_RETRY_QUEUE_CAPACITY_TIME_INTERVAL_SEC,
        // Not implemented. #1365
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `log_format_rfc3339` - use RFC3339 timestamp format.
    LOG_FORMAT_RFC3339 = SalukiAnnotation {
        schema: &schema::LOG_FORMAT_RFC3339,
        // Not implemented. #1373
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `min_tls_version` - minimum TLS version for HTTPS.
    MIN_TLS_VERSION = SalukiAnnotation {
        schema: &schema::MIN_TLS_VERSION,
        // Not implemented. #1370
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };





    /// `serializer_experimental_use_v3_api.compression_level` - V3 API compression level.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_COMPRESSION_LEVEL = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_COMPRESSION_LEVEL,
        // V3 metrics API not implemented. #1468
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_experimental_use_v3_api.series.endpoints` - V3 API series endpoints.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS,
        // V3 metrics API not implemented. #1468
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_experimental_use_v3_api.series.validate` - V3 API series validation.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_VALIDATE = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_VALIDATE,
        // V3 metrics API not implemented. #1468
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_experimental_use_v3_api.sketches.endpoints` - V3 API sketches endpoints.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_ENDPOINTS = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_ENDPOINTS,
        // V3 metrics API not implemented. #1468
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_experimental_use_v3_api.sketches.validate` - V3 API sketches validation.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_VALIDATE = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_VALIDATE,
        // V3 metrics API not implemented. #1468
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_max_payload_size` - max compressed payload size.
    SERIALIZER_MAX_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_PAYLOAD_SIZE,
        // Not configurable in ADP. #1354
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_max_series_payload_size` - max series compressed size.
    SERIALIZER_MAX_SERIES_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_SERIES_PAYLOAD_SIZE,
        // Not configurable in ADP. #1354
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_max_series_points_per_payload` - max series points per payload.
    SERIALIZER_MAX_SERIES_POINTS_PER_PAYLOAD = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_SERIES_POINTS_PER_PAYLOAD,
        // Not configurable in ADP. #1354
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_max_series_uncompressed_payload_size` - max series uncompressed size.
    SERIALIZER_MAX_SERIES_UNCOMPRESSED_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_SERIES_UNCOMPRESSED_PAYLOAD_SIZE,
        // Not configurable in ADP. #1354
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `serializer_max_uncompressed_payload_size` - max uncompressed payload size.
    SERIALIZER_MAX_UNCOMPRESSED_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_UNCOMPRESSED_PAYLOAD_SIZE,
        // Not configurable in ADP. #1354
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `sslkeylogfile` - TLS key log file path.
    SSLKEYLOGFILE = SalukiAnnotation {
        schema: &schema::SSLKEYLOGFILE,
        // Not implemented. #1372
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_forward_host` - host for packet forwarding.
    STATSD_FORWARD_HOST = SalukiAnnotation {
        schema: &schema::STATSD_FORWARD_HOST,
        // Packet forwarding not implemented. #1476
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_forward_port` - port for packet forwarding.
    STATSD_FORWARD_PORT = SalukiAnnotation {
        schema: &schema::STATSD_FORWARD_PORT,
        // Packet forwarding not implemented. #1476
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `telemetry.enabled` - global telemetry toggle.
    TELEMETRY_ENABLED = SalukiAnnotation {
        schema: &schema::TELEMETRY_ENABLED,
        // ADP uses data_plane.telemetry_enabled instead. #1338
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `tls_handshake_timeout` - HTTP TLS handshake timeout.
    TLS_HANDSHAKE_TIMEOUT = SalukiAnnotation {
        schema: &schema::TLS_HANDSHAKE_TIMEOUT,
        // Not implemented. Request timeout covers the gap. #178
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };





    /// `aggregator_buffer_size` - aggregator input channel depth.
    AGGREGATOR_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::AGGREGATOR_BUFFER_SIZE,
        // Not implemented. #1681
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `aggregator_flush_metrics_and_serialize_in_parallel_buffer_size` - parallel flush buffer size.
    AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_BUFFER_SIZE,
        // Not implemented. #1681
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `aggregator_flush_metrics_and_serialize_in_parallel_chan_size` - parallel flush channel size.
    AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_CHAN_SIZE = SalukiAnnotation {
        schema: &schema::AGGREGATOR_FLUSH_METRICS_AND_SERIALIZE_IN_PARALLEL_CHAN_SIZE,
        // Not implemented. #1681
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `aggregator_stop_timeout` - aggregator shutdown drain timeout.
    AGGREGATOR_STOP_TIMEOUT = SalukiAnnotation {
        schema: &schema::AGGREGATOR_STOP_TIMEOUT,
        // Not implemented. #1681
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `aggregator_use_tags_store` - shared tag deduplication store toggle.
    AGGREGATOR_USE_TAGS_STORE = SalukiAnnotation {
        schema: &schema::AGGREGATOR_USE_TAGS_STORE,
        // Not implemented. #1681
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `autoscaling.failover.enabled` - autoscaling failover metric routing via DCA.
    AUTOSCALING_FAILOVER_ENABLED = SalukiAnnotation {
        schema: &schema::AUTOSCALING_FAILOVER_ENABLED,
        // Not implemented. #1684
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `autoscaling.failover.metrics` - metric names forwarded to DCA for failover.
    AUTOSCALING_FAILOVER_METRICS = SalukiAnnotation {
        schema: &schema::AUTOSCALING_FAILOVER_METRICS,
        // Not implemented. #1684
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `config_id` - Fleet Automation config ID on payloads.
    CONFIG_ID = SalukiAnnotation {
        schema: &schema::CONFIG_ID,
        // Not implemented. #1685
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `dogstatsd_experimental_http.enabled` - experimental HTTP/H2C DSD listener toggle.
    DOGSTATSD_EXPERIMENTAL_HTTP_ENABLED = SalukiAnnotation {
        schema: &schema::DOGSTATSD_EXPERIMENTAL_HTTP_ENABLED,
        // Not implemented. #1682
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `dogstatsd_experimental_http.listen_address` - experimental HTTP DSD listener bind address.
    DOGSTATSD_EXPERIMENTAL_HTTP_LISTEN_ADDRESS = SalukiAnnotation {
        schema: &schema::DOGSTATSD_EXPERIMENTAL_HTTP_LISTEN_ADDRESS,
        // Not implemented. #1682
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `dogstatsd_host_socket_path` - host UDS socket dir for admission controller.
    DOGSTATSD_HOST_SOCKET_PATH = SalukiAnnotation {
        schema: &schema::DOGSTATSD_HOST_SOCKET_PATH,
        // Not implemented. #1687
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `dogstatsd_mapper_cache_size` - LRU cache size for mapper regex results.
    DOGSTATSD_MAPPER_CACHE_SIZE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_MAPPER_CACHE_SIZE,
        // Not implemented. #1687
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `enable_json_stream_shared_compressor_buffers` - shared JSON stream compressor buffer allocation.
    ENABLE_JSON_STREAM_SHARED_COMPRESSOR_BUFFERS = SalukiAnnotation {
        schema: &schema::ENABLE_JSON_STREAM_SHARED_COMPRESSOR_BUFFERS,
        // Not implemented. #1686
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `enable_payloads.json_to_v1_intake` - legacy /api/v1/intake JSON payload delivery.
    ENABLE_PAYLOADS_JSON_TO_V1_INTAKE = SalukiAnnotation {
        schema: &schema::ENABLE_PAYLOADS_JSON_TO_V1_INTAKE,
        // Not implemented. #1686
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `entity_id` - agent pod entity ID injected by DCA webhook.
    ENTITY_ID = SalukiAnnotation {
        schema: &schema::ENTITY_ID,
        // Not implemented. #1685
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `forwarder_requeue_buffer_size` - forwarder in-memory requeue buffer size.
    FORWARDER_REQUEUE_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_REQUEUE_BUFFER_SIZE,
        // Not implemented. #1680
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `forwarder_stop_timeout` - forwarder graceful stop drain timeout.
    FORWARDER_STOP_TIMEOUT = SalukiAnnotation {
        schema: &schema::FORWARDER_STOP_TIMEOUT,
        // Not implemented. #1680
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `heroku_dyno` - Heroku dyno name override for agent telemetry.
    HEROKU_DYNO = SalukiAnnotation {
        schema: &schema::HEROKU_DYNO,
        // Not implemented. #1685
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `log_payloads` - debug-log serialized payloads before send.
    LOG_PAYLOADS = SalukiAnnotation {
        schema: &schema::LOG_PAYLOADS,
        // Not implemented. #1686
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.api_key` - API key for the MRF failover region.
    MULTI_REGION_FAILOVER_API_KEY = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_API_KEY,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.dd_url` - intake URL for the MRF failover region.
    MULTI_REGION_FAILOVER_DD_URL = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_DD_URL,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.enabled` - multi-region failover mode.
    MULTI_REGION_FAILOVER_ENABLED = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_ENABLED,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.failover_metrics` - metrics forwarding to the failover region.
    MULTI_REGION_FAILOVER_FAILOVER_METRICS = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_FAILOVER_METRICS,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.metric_allowlist` - metric name allowlist for MRF forwarding.
    MULTI_REGION_FAILOVER_METRIC_ALLOWLIST = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_METRIC_ALLOWLIST,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `multi_region_failover.site` - Datadog site for the MRF failover region.
    MULTI_REGION_FAILOVER_SITE = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_SITE,
        // Not implemented. #1678
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `telemetry.dogstatsd.aggregator_channel_latency_buckets` - DSD-to-aggregator channel latency histogram buckets.
    TELEMETRY_DOGSTATSD_AGGREGATOR_CHANNEL_LATENCY_BUCKETS = SalukiAnnotation {
        schema: &schema::TELEMETRY_DOGSTATSD_AGGREGATOR_CHANNEL_LATENCY_BUCKETS,
        // Not implemented. #1679
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `telemetry.dogstatsd.listeners_channel_latency_buckets` - DSD listener channel latency histogram buckets.
    TELEMETRY_DOGSTATSD_LISTENERS_CHANNEL_LATENCY_BUCKETS = SalukiAnnotation {
        schema: &schema::TELEMETRY_DOGSTATSD_LISTENERS_CHANNEL_LATENCY_BUCKETS,
        // Not implemented. #1679
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `telemetry.dogstatsd.listeners_latency_buckets` - DSD listener processing latency histogram buckets.
    TELEMETRY_DOGSTATSD_LISTENERS_LATENCY_BUCKETS = SalukiAnnotation {
        schema: &schema::TELEMETRY_DOGSTATSD_LISTENERS_LATENCY_BUCKETS,
        // Not implemented. #1679
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
    /// `telemetry.dogstatsd_origin` - per-origin processed-metric telemetry counters.
    TELEMETRY_DOGSTATSD_ORIGIN = SalukiAnnotation {
        schema: &schema::TELEMETRY_DOGSTATSD_ORIGIN,
        // Not implemented. #1679
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `cluster_agent.enabled` - enable Cluster Agent connectivity.
    CLUSTER_AGENT_ENABLED = SalukiAnnotation {
        schema: &schema::CLUSTER_AGENT_ENABLED,
        // Not implemented. Required for autoscaling failover routing. #1684
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

}
