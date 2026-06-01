//! Annotations for shared Datadog encoder configuration keys.
use crate::config_registry::{
    generated::schema, structs, Pipeline, PipelineAffinity, SalukiAnnotation, Schema, SchemaEntry, SupportLevel,
    ValueType,
};

static FLUSH_TIMEOUT_SECS_SCHEMA: SchemaEntry = SchemaEntry {
    schema: Schema::Saluki,
    yaml_path: "flush_timeout_secs",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static SERIALIZER_MAX_METRICS_PER_PAYLOAD_SCHEMA: SchemaEntry = SchemaEntry {
    schema: Schema::Saluki,
    yaml_path: "serializer_max_metrics_per_payload",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

crate::declare_annotations! {
    /// `serializer_compressor_kind`—compression algorithm for encoder request payloads.
    SERIALIZER_COMPRESSOR_KIND = SalukiAnnotation {
        schema: &schema::SERIALIZER_COMPRESSOR_KIND,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_EVENTS_CONFIGURATION,
            structs::DATADOG_LOGS_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
            structs::DATADOG_TRACE_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };

    /// `serializer_zstd_compressor_level`—zstd compression level for encoder request payloads.
    /// Schema declares Float; field is i32.
    SERIALIZER_ZSTD_COMPRESSOR_LEVEL = SalukiAnnotation {
        schema: &schema::SERIALIZER_ZSTD_COMPRESSOR_LEVEL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_EVENTS_CONFIGURATION,
            structs::DATADOG_LOGS_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
            structs::DATADOG_TRACE_CONFIGURATION,
        ],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };

    /// `flush_timeout_secs`—how long to wait before force-flushing an in-flight payload. ADP-specific.
    FLUSH_TIMEOUT_SECS = SalukiAnnotation {
        schema: &FLUSH_TIMEOUT_SECS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_APM_STATS_ENCODER_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_TRACE_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::Traces, Pipeline::DogStatsD]),
    };

    /// `serializer_max_metrics_per_payload`—max metrics per encoded request payload. ADP-specific.
    SERIALIZER_MAX_METRICS_PER_PAYLOAD = SalukiAnnotation {
        schema: &SERIALIZER_MAX_METRICS_PER_PAYLOAD_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_METRICS_CONFIGURATION],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `log_payloads`—debug-log decoded metric, event, and service check payload contents before encoding.
    LOG_PAYLOADS = SalukiAnnotation {
        schema: &schema::LOG_PAYLOADS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_EVENTS_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD, Pipeline::Traces]),
    };

    /// `serializer_max_payload_size`—max compressed generic payload size.
    SERIALIZER_MAX_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_PAYLOAD_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_EVENTS_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
        ],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `serializer_max_uncompressed_payload_size`—max uncompressed generic payload size.
    SERIALIZER_MAX_UNCOMPRESSED_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_UNCOMPRESSED_PAYLOAD_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_EVENTS_CONFIGURATION,
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
        ],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `serializer_max_series_payload_size`—max compressed V2 series payload size.
    SERIALIZER_MAX_SERIES_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_SERIES_PAYLOAD_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_METRICS_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `serializer_max_series_uncompressed_payload_size`—max uncompressed V2 series payload size.
    SERIALIZER_MAX_SERIES_UNCOMPRESSED_PAYLOAD_SIZE = SalukiAnnotation {
        schema: &schema::SERIALIZER_MAX_SERIES_UNCOMPRESSED_PAYLOAD_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_METRICS_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `use_v2_api.series`—when `false`, send series metrics to the legacy V1 JSON intake at `/api/v1/series`.
    USE_V2_API_SERIES = SalukiAnnotation {
        schema: &schema::USE_V2_API_SERIES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_METRICS_CONFIGURATION],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Checks, Pipeline::DogStatsD]),
    };

    /// `env`—the environment name attached to all emitted telemetry.
    ENV = SalukiAnnotation {
        schema: &schema::ENV,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_APM_STATS_ENCODER_CONFIGURATION,
            structs::DATADOG_TRACE_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.compression_level`—compression level for V3 payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_COMPRESSION_LEVEL = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_COMPRESSION_LEVEL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.series.beta_route`—intake route for V3 beta series payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_BETA_ROUTE = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_BETA_ROUTE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.series.endpoints`—additional endpoints for V3 series payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.series.use_beta`—whether to send V3 series payloads to the beta route.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_USE_BETA = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_USE_BETA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.series.validate`—enable V2/V3 validation mode for series payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_VALIDATE = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_VALIDATE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.sketches.endpoints`—additional endpoints for V3 sketch payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_ENDPOINTS = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_ENDPOINTS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };

    /// `serializer_experimental_use_v3_api.sketches.validate`—enable V2/V3 validation mode for sketch payloads.
    SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_VALIDATE = SalukiAnnotation {
        schema: &schema::SERIALIZER_EXPERIMENTAL_USE_V3_API_SKETCHES_VALIDATE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[
            structs::DATADOG_METRICS_CONFIGURATION,
            structs::FORWARDER_CONFIGURATION,
        ],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Traces]),
    };
}
