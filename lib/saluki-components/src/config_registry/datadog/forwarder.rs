//! Annotations for ForwarderConfiguration keys (endpoint, retry, and forwarder settings).
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel, ValueType};

crate::declare_annotations! {
    // ── Endpoint ──────────────────────────────────────────────────────────────

    /// `api_key` — Datadog API key for authentication.
    API_KEY = SalukiAnnotation {
        schema: &schema::API_KEY,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `site` — Datadog site domain (e.g. `datadoghq.com`).
    SITE = SalukiAnnotation {
        schema: &schema::SITE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `dd_url` — explicit intake URL, overrides `site`.
    /// The schema-declared env vars don't round-trip via the test DatadogRemapper.
    DD_URL = SalukiAnnotation {
        schema: &schema::DD_URL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&[]),
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `additional_endpoints` — extra intake endpoints (JSON map of host → API keys).
    /// Uses a structured test_json because the field uses PickFirst<(DisplayFromStr, _)>.
    ADDITIONAL_ENDPOINTS = SalukiAnnotation {
        schema: &schema::ADDITIONAL_ENDPOINTS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"{"smoke-host-1.example.com": ["smoke-api-key"]}"#),
    };

    // ── ForwarderConfiguration direct fields ──────────────────────────────────

    /// `forwarder_num_workers` — max concurrent requests per endpoint. Schema Float; field usize.
    FORWARDER_NUM_WORKERS = SalukiAnnotation {
        schema: &schema::FORWARDER_NUM_WORKERS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_timeout` — request timeout in seconds. Schema Float; field u64.
    FORWARDER_TIMEOUT = SalukiAnnotation {
        schema: &schema::FORWARDER_TIMEOUT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_high_prio_buffer_size` — max pending requests per endpoint. Schema Float; field usize.
    FORWARDER_HIGH_PRIO_BUFFER_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_HIGH_PRIO_BUFFER_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_connection_reset_interval` — seconds between connection resets. Schema Float; field u64.
    FORWARDER_CONNECTION_RESET_INTERVAL = SalukiAnnotation {
        schema: &schema::FORWARDER_CONNECTION_RESET_INTERVAL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    // ── RetryConfiguration fields ─────────────────────────────────────────────

    /// `forwarder_backoff_base` — base growth rate for retry backoff in seconds.
    FORWARDER_BACKOFF_BASE = SalukiAnnotation {
        schema: &schema::FORWARDER_BACKOFF_BASE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_backoff_factor` — jitter factor for retry backoff.
    FORWARDER_BACKOFF_FACTOR = SalukiAnnotation {
        schema: &schema::FORWARDER_BACKOFF_FACTOR,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_backoff_max` — maximum retry backoff duration in seconds.
    FORWARDER_BACKOFF_MAX = SalukiAnnotation {
        schema: &schema::FORWARDER_BACKOFF_MAX,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_recovery_interval` — error count decrease on success. Schema Float; field u32.
    FORWARDER_RECOVERY_INTERVAL = SalukiAnnotation {
        schema: &schema::FORWARDER_RECOVERY_INTERVAL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_recovery_reset` — reset error count on successful request.
    FORWARDER_RECOVERY_RESET = SalukiAnnotation {
        schema: &schema::FORWARDER_RECOVERY_RESET,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_retry_queue_max_size` — (deprecated) max in-memory retry queue size in bytes. Schema Float; field `Option<u64>`.
    FORWARDER_RETRY_QUEUE_MAX_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_RETRY_QUEUE_MAX_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_retry_queue_payloads_max_size` — max in-memory retry queue size in bytes. Schema Float; field `Option<u64>`.
    FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE = SalukiAnnotation {
        schema: &schema::FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_storage_max_disk_ratio` — max disk usage fraction before stopping on-disk queue.
    FORWARDER_STORAGE_MAX_DISK_RATIO = SalukiAnnotation {
        schema: &schema::FORWARDER_STORAGE_MAX_DISK_RATIO,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `forwarder_storage_max_size_in_bytes` — max on-disk retry queue size. Schema Float; field u64.
    FORWARDER_STORAGE_MAX_SIZE_IN_BYTES = SalukiAnnotation {
        schema: &schema::FORWARDER_STORAGE_MAX_SIZE_IN_BYTES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
        test_json: None,
    };

    /// `forwarder_storage_path` — directory for on-disk retry queue.
    FORWARDER_STORAGE_PATH = SalukiAnnotation {
        schema: &schema::FORWARDER_STORAGE_PATH,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::FORWARDER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
