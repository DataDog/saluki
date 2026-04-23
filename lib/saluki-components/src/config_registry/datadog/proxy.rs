use crate::config_registry::{generated::schema, structs, SalukiAnnotation};

/// `proxy.http` → `ProxyConfiguration::http_server`
pub const PROXY_HTTP: SalukiAnnotation = SalukiAnnotation {
    schema: &schema::PROXY_HTTP,
    additional_yaml_paths: &[],
    env_var_override: Some(&["DD_PROXY_HTTP", "HTTP_PROXY"]),
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `proxy.https` → `ProxyConfiguration::https_server`
pub const PROXY_HTTPS: SalukiAnnotation = SalukiAnnotation {
    schema: &schema::PROXY_HTTPS,
    additional_yaml_paths: &[],
    env_var_override: Some(&["DD_PROXY_HTTPS", "HTTPS_PROXY"]),
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `proxy.no_proxy` → `ProxyConfiguration::no_proxy`
pub const PROXY_NO_PROXY: SalukiAnnotation = SalukiAnnotation {
    schema: &schema::PROXY_NO_PROXY,
    additional_yaml_paths: &[],
    env_var_override: Some(&["DD_PROXY_NO_PROXY"]),
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `no_proxy_nonexact_match` → `ProxyConfiguration::no_proxy_nonexact_match`
pub const NO_PROXY_NONEXACT_MATCH: SalukiAnnotation = SalukiAnnotation {
    schema: &schema::NO_PROXY_NONEXACT_MATCH,
    additional_yaml_paths: &[],
    env_var_override: Some(&["DD_NO_PROXY_NONEXACT_MATCH"]),
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// `use_proxy_for_cloud_metadata` → `ProxyConfiguration::use_proxy_for_cloud_metadata`
pub const USE_PROXY_FOR_CLOUD_METADATA: SalukiAnnotation = SalukiAnnotation {
    schema: &schema::USE_PROXY_FOR_CLOUD_METADATA,
    additional_yaml_paths: &[],
    env_var_override: Some(&["DD_USE_PROXY_FOR_CLOUD_METADATA"]),
    used_by: &[structs::PROXY_CONFIGURATION],
};

/// All proxy configuration annotations.
pub const ALL: &[&SalukiAnnotation] = &[
    &PROXY_HTTP,
    &PROXY_HTTPS,
    &PROXY_NO_PROXY,
    &NO_PROXY_NONEXACT_MATCH,
    &USE_PROXY_FOR_CLOUD_METADATA,
];
