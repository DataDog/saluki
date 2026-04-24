//! Annotations for DogStatsD source configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

static DOGSTATSD_ALLOW_CONTEXT_HEAP_ALLOCS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_allow_context_heap_allocs",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static DOGSTATSD_BUFFER_COUNT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_buffer_count",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static DOGSTATSD_CACHED_CONTEXTS_LIMIT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_cached_contexts_limit",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static DOGSTATSD_CACHED_TAGSETS_LIMIT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_cached_tagsets_limit",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static DOGSTATSD_MINIMUM_SAMPLE_RATE_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_minimum_sample_rate",
    env_vars: &[],
    value_type: ValueType::Float,
    default: None,
};

static DOGSTATSD_PERMISSIVE_DECODING_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_permissive_decoding",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static DOGSTATSD_STRING_INTERNER_SIZE_BYTES_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_string_interner_size_bytes",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static DOGSTATSD_TCP_PORT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_tcp_port",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

// The Agent schema defines these as nested properties under an `enable_payloads` section:
//   enable_payloads.events, enable_payloads.series, etc.
// (see ENABLE_PAYLOADS_EVENTS / ENABLE_PAYLOADS_SERIES etc. in generated/schema.rs)
//
// Saluki's DogStatsDConfiguration uses flat underscore-separated field names with no `#[serde(rename)]`,
// so the yaml_path for deserialization is `enable_payloads_events` (not `enable_payloads.events`).
// The two naming conventions are incompatible at the config-loader level — setting the dotted path
// produces a nested object that the flat field cannot consume. Custom statics with the flat paths
// are required until the struct field naming is aligned with the Agent schema.
// TODO: https://github.com/DataDog/saluki/issues — align enable_payloads field names with schema
static ENABLE_PAYLOADS_EVENTS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads_events",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static ENABLE_PAYLOADS_SERIES_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads_series",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static ENABLE_PAYLOADS_SERVICE_CHECKS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads_service_checks",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

static ENABLE_PAYLOADS_SKETCHES_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "enable_payloads_sketches",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: Some("true"),
};

crate::declare_annotations! {
    // ── Schema-based ──────────────────────────────────────────────────────────

    /// `dogstatsd_buffer_size` — receive buffer size in bytes. Schema says Float; field is usize.
    DOGSTATSD_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_BUFFER_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `dogstatsd_entity_id_precedence` — client entity ID takes precedence over UDS origin.
    DOGSTATSD_ENTITY_ID_PRECEDENCE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_ENTITY_ID_PRECEDENCE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_non_local_traffic` — accept packets from non-localhost addresses.
    DOGSTATSD_NON_LOCAL_TRAFFIC = SalukiAnnotation {
        schema: &schema::DOGSTATSD_NON_LOCAL_TRAFFIC,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_origin_detection` — enable UDS-based origin detection.
    DOGSTATSD_ORIGIN_DETECTION = SalukiAnnotation {
        schema: &schema::DOGSTATSD_ORIGIN_DETECTION,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_origin_optout_enabled` — skip enrichment when cardinality=none.
    DOGSTATSD_ORIGIN_OPTOUT_ENABLED = SalukiAnnotation {
        schema: &schema::DOGSTATSD_ORIGIN_OPTOUT_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_port` — UDP port. Schema says Float; field is u16.
    DOGSTATSD_PORT = SalukiAnnotation {
        schema: &schema::DOGSTATSD_PORT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `dogstatsd_socket` — UDS datagram socket path.
    DOGSTATSD_SOCKET = SalukiAnnotation {
        schema: &schema::DOGSTATSD_SOCKET,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_stream_socket` — UDS stream socket path.
    DOGSTATSD_STREAM_SOCKET = SalukiAnnotation {
        schema: &schema::DOGSTATSD_STREAM_SOCKET,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_string_interner_size` — context interner entry count. Schema Float; field u64.
    DOGSTATSD_STRING_INTERNER_SIZE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_STRING_INTERNER_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `dogstatsd_tag_cardinality` — default tag cardinality for origin enrichment.
    /// Default is "Low"; inject "high" as test value to differ from default.
    DOGSTATSD_TAG_CARDINALITY = SalukiAnnotation {
        schema: &schema::DOGSTATSD_TAG_CARDINALITY,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#""high""#),
    };

    /// `dogstatsd_tags` — additional tags applied to all ingested metrics.
    DOGSTATSD_TAGS = SalukiAnnotation {
        schema: &schema::DOGSTATSD_TAGS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `origin_detection_unified` — use unified entity ID resolution for origin enrichment.
    ORIGIN_DETECTION_UNIFIED = SalukiAnnotation {
        schema: &schema::ORIGIN_DETECTION_UNIFIED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    // ── ADP-specific ──────────────────────────────────────────────────────────

    /// `dogstatsd_allow_context_heap_allocs` — allow heap allocations when interner is full.
    DOGSTATSD_ALLOW_CONTEXT_HEAP_ALLOCS = SalukiAnnotation {
        schema: &DOGSTATSD_ALLOW_CONTEXT_HEAP_ALLOCS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_buffer_count` — number of message buffers to pre-allocate.
    DOGSTATSD_BUFFER_COUNT = SalukiAnnotation {
        schema: &DOGSTATSD_BUFFER_COUNT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_cached_contexts_limit` — max cached metric contexts.
    DOGSTATSD_CACHED_CONTEXTS_LIMIT = SalukiAnnotation {
        schema: &DOGSTATSD_CACHED_CONTEXTS_LIMIT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_cached_tagsets_limit` — max cached tag sets.
    DOGSTATSD_CACHED_TAGSETS_LIMIT = SalukiAnnotation {
        schema: &DOGSTATSD_CACHED_TAGSETS_LIMIT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_minimum_sample_rate` — minimum allowed sample rate.
    DOGSTATSD_MINIMUM_SAMPLE_RATE = SalukiAnnotation {
        schema: &DOGSTATSD_MINIMUM_SAMPLE_RATE_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_permissive_decoding` — relax decoding strictness for Agent compat.
    DOGSTATSD_PERMISSIVE_DECODING = SalukiAnnotation {
        schema: &DOGSTATSD_PERMISSIVE_DECODING_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_string_interner_size_bytes` — explicit interner byte budget (overrides entry count).
    DOGSTATSD_STRING_INTERNER_SIZE_BYTES = SalukiAnnotation {
        schema: &DOGSTATSD_STRING_INTERNER_SIZE_BYTES_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_tcp_port` — TCP port (0 = disabled).
    DOGSTATSD_TCP_PORT = SalukiAnnotation {
        schema: &DOGSTATSD_TCP_PORT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `enable_payloads_events` — forward event payloads.
    ENABLE_PAYLOADS_EVENTS = SalukiAnnotation {
        schema: &ENABLE_PAYLOADS_EVENTS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `enable_payloads_series` — forward series (counter/gauge/rate) payloads.
    ENABLE_PAYLOADS_SERIES = SalukiAnnotation {
        schema: &ENABLE_PAYLOADS_SERIES_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `enable_payloads_service_checks` — forward service check payloads.
    ENABLE_PAYLOADS_SERVICE_CHECKS = SalukiAnnotation {
        schema: &ENABLE_PAYLOADS_SERVICE_CHECKS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `enable_payloads_sketches` — forward sketch (distribution) payloads.
    ENABLE_PAYLOADS_SKETCHES = SalukiAnnotation {
        schema: &ENABLE_PAYLOADS_SKETCHES_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
