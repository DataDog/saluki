//! Annotations for the aggregate transform configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// ADP-specific keys not present in the vendored Agent schema.
static AGGREGATE_WINDOW_DURATION_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "aggregate_window_duration",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

static AGGREGATE_FLUSH_INTERVAL_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "aggregate_flush_interval",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

static AGGREGATE_CONTEXT_LIMIT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "aggregate_context_limit",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static AGGREGATE_FLUSH_OPEN_WINDOWS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "aggregate_flush_open_windows",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

static COUNTER_EXPIRY_SECONDS_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "counter_expiry_seconds",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

static AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "aggregate_passthrough_idle_flush_timeout",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

crate::declare_annotations! {
    /// `aggregate_window_duration` — size of each aggregation window.
    /// Duration fields serialize as {secs, nanos}; inject as object with test_json.
    AGGREGATE_WINDOW_DURATION = SalukiAnnotation {
        schema: &AGGREGATE_WINDOW_DURATION_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"{"secs": 42, "nanos": 0}"#),
    };

    /// `aggregate_flush_interval` — how often to flush aggregation buckets.
    AGGREGATE_FLUSH_INTERVAL = SalukiAnnotation {
        schema: &AGGREGATE_FLUSH_INTERVAL_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"{"secs": 42, "nanos": 0}"#),
    };

    /// `aggregate_context_limit` — max distinct metric contexts per window.
    AGGREGATE_CONTEXT_LIMIT = SalukiAnnotation {
        schema: &AGGREGATE_CONTEXT_LIMIT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `aggregate_flush_open_windows` — flush open buckets on shutdown.
    AGGREGATE_FLUSH_OPEN_WINDOWS = SalukiAnnotation {
        schema: &AGGREGATE_FLUSH_OPEN_WINDOWS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `counter_expiry_seconds` — idle counter lifetime before removal.
    COUNTER_EXPIRY_SECONDS = SalukiAnnotation {
        schema: &COUNTER_EXPIRY_SECONDS_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &["dogstatsd_expiry_seconds"],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dogstatsd_no_aggregation_pipeline` — pass through pre-timestamped metrics immediately.
    DOGSTATSD_NO_AGGREGATION_PIPELINE = SalukiAnnotation {
        schema: &schema::DOGSTATSD_NO_AGGREGATION_PIPELINE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION, structs::DOGSTATSD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `aggregate_passthrough_idle_flush_timeout` — max buffer time for passthrough metrics.
    AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT = SalukiAnnotation {
        schema: &AGGREGATE_PASSTHROUGH_IDLE_FLUSH_TIMEOUT_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"{"secs": 42, "nanos": 0}"#),
    };

    /// `histogram_aggregates` — list of aggregates to compute for histogram metrics.
    HISTOGRAM_AGGREGATES = SalukiAnnotation {
        schema: &schema::HISTOGRAM_AGGREGATES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"["count"]"#),
    };

    /// `histogram_copy_to_distribution` — also emit histograms as distributions.
    HISTOGRAM_COPY_TO_DISTRIBUTION = SalukiAnnotation {
        schema: &schema::HISTOGRAM_COPY_TO_DISTRIBUTION,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `histogram_copy_to_distribution_prefix` — prefix for distribution copies of histograms.
    HISTOGRAM_COPY_TO_DISTRIBUTION_PREFIX = SalukiAnnotation {
        schema: &schema::HISTOGRAM_COPY_TO_DISTRIBUTION_PREFIX,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::AGGREGATE_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
