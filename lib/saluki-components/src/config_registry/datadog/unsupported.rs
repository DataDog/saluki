//! Annotations for configuration keys that Saluki does not support.
use crate::config_registry::{generated::schema, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `allow_arbitrary_tags` - signal backend tag validation relaxation.
    ALLOW_ARBITRARY_TAGS = SalukiAnnotation {
        schema: &schema::ALLOW_ARBITRARY_TAGS,
        // Not implemented. #1377
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_capture_depth` - traffic capture channel depth.
    DOGSTATSD_CAPTURE_DEPTH = SalukiAnnotation {
        schema: &schema::DOGSTATSD_CAPTURE_DEPTH,
        // Traffic capture not implemented. #1381
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_capture_path` - traffic capture file location.
    DOGSTATSD_CAPTURE_PATH = SalukiAnnotation {
        schema: &schema::DOGSTATSD_CAPTURE_PATH,
        // Traffic capture not implemented. #1381
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_context_expiry_seconds` - context cache TTL.
    DOGSTATSD_CONTEXT_EXPIRY_SECONDS = SalukiAnnotation {
        schema: &schema::DOGSTATSD_CONTEXT_EXPIRY_SECONDS,
        // ADP hardcodes 30s, ignores this config key. #1340
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_eol_required` - require newline-terminated messages.
    DOGSTATSD_EOL_REQUIRED = SalukiAnnotation {
        schema: &schema::DOGSTATSD_EOL_REQUIRED,
        // Codec supports EOL enforcement but config not wired. #1339
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `logging_frequency` - transaction success log interval.
    LOGGING_FREQUENCY = SalukiAnnotation {
        schema: &schema::LOGGING_FREQUENCY,
        // Intentionally unused. ADP logs success below info level.
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `observability_pipelines_worker.metrics.enabled` - route metrics to OPW.
    OBSERVABILITY_PIPELINES_WORKER_METRICS_ENABLED = SalukiAnnotation {
        schema: &schema::OBSERVABILITY_PIPELINES_WORKER_METRICS_ENABLED,
        // Not implemented. #1586
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `observability_pipelines_worker.metrics.url` - OPW metrics intake URL.
    OBSERVABILITY_PIPELINES_WORKER_METRICS_URL = SalukiAnnotation {
        schema: &schema::OBSERVABILITY_PIPELINES_WORKER_METRICS_URL,
        // Not implemented. #1586
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
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
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `vector.metrics.enabled` - route metrics to OPW (legacy alias).
    VECTOR_METRICS_ENABLED = SalukiAnnotation {
        schema: &schema::VECTOR_METRICS_ENABLED,
        // Legacy alias for OPW. Not implemented. #1586
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };

    /// `vector.metrics.url` - OPW metrics intake URL (legacy alias).
    VECTOR_METRICS_URL = SalukiAnnotation {
        schema: &schema::VECTOR_METRICS_URL,
        // Legacy alias for OPW. Not implemented. #1586
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
}
