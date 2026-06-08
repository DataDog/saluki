//! Annotations for configuration keys that Saluki doesn't support.
use crate::config_registry::{generated::schema, Pipeline, PipelineAffinity, SalukiAnnotation, Severity, SupportLevel};

crate::declare_annotations! {
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };

    /// `forwarder_low_prio_buffer_size` - low-priority request queue size.
    FORWARDER_LOW_PRIO_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_LOW_PRIO_BUFFER_SIZE,
        // ADP low-priority pending transactions use the byte-bounded retry queue. #1362
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        // Metrics encoder (dd_metrics_encode) is used by DogStatsD, Checks, and OTLP native (Traces active); APM traces use a separate encoder.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
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
        // Metrics encoder (dd_metrics_encode) is used by DogStatsD, Checks, and OTLP native (Traces active); APM traces use a separate encoder.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
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
        // Metrics encoder (dd_metrics_encode) is used by DogStatsD, Checks, and OTLP native (Traces active); APM traces use a separate encoder.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
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
        // Metrics encoder (dd_metrics_encode) is used by DogStatsD, Checks, and OTLP native (Traces active); APM traces use a separate encoder.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
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
        // Metrics encoder (dd_metrics_encode) is used by DogStatsD, Checks, and OTLP native (Traces active); APM traces use a separate encoder.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
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
        // TLS is process-wide.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        // TLS is process-wide.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
    };
    /// `autoscaling.failover.enabled` - autoscaling failover metric routing via DCA.
    AUTOSCALING_FAILOVER_ENABLED = SalukiAnnotation {
        schema: &schema::AUTOSCALING_FAILOVER_ENABLED,
        // Not implemented. #1684
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // Autoscaling failover routes metric series to the Cluster Agent for HPA; applies to DogStatsD and Checks metric producers, not APM.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks]),
    };
    /// `autoscaling.failover.metrics` - metric names forwarded to DCA for failover.
    AUTOSCALING_FAILOVER_METRICS = SalukiAnnotation {
        schema: &schema::AUTOSCALING_FAILOVER_METRICS,
        // Not implemented. #1684
        support_level: SupportLevel::Incompatible(Severity::Medium),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // Autoscaling failover routes metric series to the Cluster Agent for HPA; applies to DogStatsD and Checks metric producers, not APM.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
    };
    /// `forwarder_requeue_buffer_size` - forwarder in-memory requeue buffer size.
    FORWARDER_REQUEUE_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_REQUEUE_BUFFER_SIZE,
        // Not implemented. #1755
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };
    /// `forwarder_stop_timeout` - forwarder graceful stop drain timeout.
    FORWARDER_STOP_TIMEOUT = SalukiAnnotation {
        schema: &schema::FORWARDER_STOP_TIMEOUT,
        // Not implemented. #1754
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // The forwarder is potentially used by any pipeline.
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };
    /// `heroku_dyno` - Heroku dyno name override for agent telemetry.
    HEROKU_DYNO = SalukiAnnotation {
        schema: &schema::HEROKU_DYNO,
        // Not planned: this changes core Agent heartbeat telemetry in the Heroku Agent package
        // path, where ADP is not launched. #1753
        support_level: SupportLevel::Incompatible(Severity::High),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // Agent/host metadata applied to all payloads.
        pipeline_affinity: PipelineAffinity::CrossCutting,
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
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
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
    };

    /// `cluster_agent.enabled` - enable Cluster Agent connectivity.
    CLUSTER_AGENT_ENABLED = SalukiAnnotation {
        schema: &schema::CLUSTER_AGENT_ENABLED,
        // Not implemented. Required for autoscaling failover routing. #1684
        support_level: SupportLevel::Incompatible(Severity::Low),
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
        // DCA connectivity is needed in Saluki for autoscaling failover (#1684), which affects DogStatsD and Checks metric pipelines.
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks]),
    };

}
