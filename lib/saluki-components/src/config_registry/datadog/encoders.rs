//! Annotations for shared Datadog encoder configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

static FLUSH_TIMEOUT_SECS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "flush_timeout_secs",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static SERIALIZER_MAX_METRICS_PER_PAYLOAD_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "serializer_max_metrics_per_payload",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

crate::declare_annotations! {
    /// `serializer_compressor_kind` — compression algorithm for encoder request payloads.
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
    };

    /// `serializer_zstd_compressor_level` — zstd compression level for encoder request payloads.
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
    };

    /// `flush_timeout_secs` — how long to wait before force-flushing an in-flight payload. ADP-specific.
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
    };

    /// `serializer_max_metrics_per_payload` — max metrics per encoded request payload. ADP-specific.
    SERIALIZER_MAX_METRICS_PER_PAYLOAD = SalukiAnnotation {
        schema: &SERIALIZER_MAX_METRICS_PER_PAYLOAD_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_METRICS_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `env` — the environment name attached to all emitted telemetry.
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
    };
}
