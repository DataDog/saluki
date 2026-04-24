//! Annotations for OTLP source, decoder, and relay configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// ADP-specific keys not present in the vendored Agent schema.
static OTLP_CONFIG_TRACES_ENABLE_TOP_LEVEL_BY_SPAN_KIND_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.enable_otlp_compute_top_level_by_span_kind",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static OTLP_CONFIG_TRACES_IGNORE_MISSING_DATADOG_FIELDS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.ignore_missing_datadog_fields",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

static OTLP_CONFIG_TRACES_STRING_INTERNER_SIZE_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.traces.string_interner_size",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TRANSPORT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_config.receiver.protocols.http.transport",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

static OTLP_ALLOW_CONTEXT_HEAP_ALLOCS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_allow_context_heap_allocs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static OTLP_CACHED_CONTEXTS_LIMIT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_cached_contexts_limit",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static OTLP_CACHED_TAGSETS_LIMIT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_cached_tagsets_limit",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static OTLP_STRING_INTERNER_SIZE_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "otlp_string_interner_size",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

crate::declare_annotations! {
    // ── Receiver ──────────────────────────────────────────────────────────────

    /// `otlp_config.receiver.protocols.grpc.endpoint`
    OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_RELAY_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib`
    /// Schema says Float but the Rust field is u64.
    OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_MAX_RECV_MSG_SIZE_MIB = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_MAX_RECV_MSG_SIZE_MIB,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_RELAY_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `otlp_config.receiver.protocols.grpc.transport`
    OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TRANSPORT = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_TRANSPORT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_RELAY_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.receiver.protocols.http.endpoint`
    OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_RELAY_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.receiver.protocols.http.transport` — ADP-specific, not in Agent schema.
    OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TRANSPORT = SalukiAnnotation {
        schema: &OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_TRANSPORT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_RELAY_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    // ── Traces ────────────────────────────────────────────────────────────────

    /// `otlp_config.traces.enabled`.
    OTLP_CONFIG_TRACES_ENABLED = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_TRACES_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.traces.ignore_missing_datadog_fields` — ADP-specific, default false.
    OTLP_CONFIG_TRACES_IGNORE_MISSING_DATADOG_FIELDS = SalukiAnnotation {
        schema: &OTLP_CONFIG_TRACES_IGNORE_MISSING_DATADOG_FIELDS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.traces.enable_otlp_compute_top_level_by_span_kind`.
    OTLP_CONFIG_TRACES_ENABLE_TOP_LEVEL_BY_SPAN_KIND = SalukiAnnotation {
        schema: &OTLP_CONFIG_TRACES_ENABLE_TOP_LEVEL_BY_SPAN_KIND_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.traces.internal_port` — schema says Float but field is u16.
    OTLP_CONFIG_TRACES_INTERNAL_PORT = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_TRACES_INTERNAL_PORT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `otlp_config.traces.probabilistic_sampler.sampling_percentage` — default 100.0.
    /// The schema-declared env var doesn't round-trip via the DatadogRemapper for this deep path.
    OTLP_CONFIG_TRACES_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_TRACES_PROBABILISTIC_SAMPLER_SAMPLING_PERCENTAGE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&[]),
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_config.traces.string_interner_size` — ADP-specific ByteSize, default 512 KiB.
    OTLP_CONFIG_TRACES_STRING_INTERNER_SIZE = SalukiAnnotation {
        schema: &OTLP_CONFIG_TRACES_STRING_INTERNER_SIZE_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_DECODER_CONFIGURATION, structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    // ── Logs / Metrics ────────────────────────────────────────────────────────

    /// `otlp_config.logs.enabled` — saluki defaults to true but schema says false.
    /// Explicit test_json because schema default disagrees with Rust implementation default.
    OTLP_CONFIG_LOGS_ENABLED = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_LOGS_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: Some("false"),
    };

    /// `otlp_config.metrics.enabled`.
    OTLP_CONFIG_METRICS_ENABLED = SalukiAnnotation {
        schema: &schema::OTLP_CONFIG_METRICS_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    // ── OtlpConfiguration source-specific ─────────────────────────────────────

    /// `otlp_allow_context_heap_allocs`.
    OTLP_ALLOW_CONTEXT_HEAP_ALLOCS = SalukiAnnotation {
        schema: &OTLP_ALLOW_CONTEXT_HEAP_ALLOCS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_cached_contexts_limit` — ADP-specific, default 500,000.
    OTLP_CACHED_CONTEXTS_LIMIT = SalukiAnnotation {
        schema: &OTLP_CACHED_CONTEXTS_LIMIT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_cached_tagsets_limit` — ADP-specific, default 500,000.
    OTLP_CACHED_TAGSETS_LIMIT = SalukiAnnotation {
        schema: &OTLP_CACHED_TAGSETS_LIMIT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `otlp_string_interner_size` — ADP-specific ByteSize context interner, default 2 MiB.
    OTLP_STRING_INTERNER_SIZE = SalukiAnnotation {
        schema: &OTLP_STRING_INTERNER_SIZE_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::OTLP_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
