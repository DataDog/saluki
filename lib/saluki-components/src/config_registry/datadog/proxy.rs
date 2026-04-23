use crate::config_registry::{structs, ConfigKey, ValueType};

/// `proxy.http` → `ProxyConfiguration::http_server`
pub const PROXY_HTTP: ConfigKey = ConfigKey {
    yaml_paths: &["proxy.http"],
    env_vars: &["DD_PROXY_HTTP", "HTTP_PROXY"],
    value_type: ValueType::String,
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `proxy.https` → `ProxyConfiguration::https_server`
pub const PROXY_HTTPS: ConfigKey = ConfigKey {
    yaml_paths: &["proxy.https"],
    env_vars: &["DD_PROXY_HTTPS", "HTTPS_PROXY"],
    value_type: ValueType::String,
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `proxy.no_proxy` → `ProxyConfiguration::no_proxy`
pub const PROXY_NO_PROXY: ConfigKey = ConfigKey {
    yaml_paths: &["proxy.no_proxy"],
    env_vars: &["DD_PROXY_NO_PROXY"],
    value_type: ValueType::StringList,
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `no_proxy_nonexact_match` → `ProxyConfiguration::no_proxy_nonexact_match`
pub const NO_PROXY_NONEXACT_MATCH: ConfigKey = ConfigKey {
    yaml_paths: &["no_proxy_nonexact_match"],
    env_vars: &["DD_NO_PROXY_NONEXACT_MATCH"],
    value_type: ValueType::Bool,
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `use_proxy_for_cloud_metadata` → `ProxyConfiguration::use_proxy_for_cloud_metadata`
pub const USE_PROXY_FOR_CLOUD_METADATA: ConfigKey = ConfigKey {
    yaml_paths: &["use_proxy_for_cloud_metadata"],
    env_vars: &["DD_USE_PROXY_FOR_CLOUD_METADATA"],
    value_type: ValueType::Bool,
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// All proxy configuration keys.
pub const ALL: &[&ConfigKey] = &[
    &PROXY_HTTP,
    &PROXY_HTTPS,
    &PROXY_NO_PROXY,
    &NO_PROXY_NONEXACT_MATCH,
    &USE_PROXY_FOR_CLOUD_METADATA,
];
