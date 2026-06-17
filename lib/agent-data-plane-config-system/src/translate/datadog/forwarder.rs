//! Forwarder-domain translation: the Datadog forwarder endpoint, TLS, retry, proxy, and OPW
//! routing, plus multi-region failover.
//!
//! These functions mirror the conversions the original `ForwarderConfiguration` /
//! `RetryConfiguration` / `ProxyConfiguration` / `MrfConfiguration` `from_configuration`
//! constructors performed in `saluki-components`, with the source key names stripped. The leaf
//! structs store raw resolved values; clamps that the original applied lazily in getters (for
//! example, endpoint-concurrency `0 -> 1`) are left to the component and not re-applied here.

use saluki_component_config::forwarder::{
    DatadogForwarderConfig, EndpointConfiguration, ForwarderHttpProtocol, MrfConfig, OpwMetricsConfiguration,
    ProxyConfiguration, RetryConfiguration,
};

/// Returns `Some(s)` when `s` is non-empty, mirroring the original `get_non_empty_string` helper.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Assembles the forwarder endpoint from the scratch keys gathered during the drive.
///
/// Mirrors the original endpoint resolution: `dd_url` takes precedence over `site`, an empty `site`
/// falls back to the Datadog default (`datadoghq.com`), and additional endpoints are carried through
/// for dual shipping. URL normalization and version-prefixing remain a component concern, so the
/// resolved values are stored verbatim.
pub fn assemble_endpoint(
    endpoint: &mut EndpointConfiguration, api_key: Option<String>, dd_url: Option<String>, site: Option<String>,
    additional_endpoints: std::collections::HashMap<String, Vec<String>>,
) {
    endpoint.api_key = api_key.unwrap_or_default();
    endpoint.dd_url = dd_url.and_then(non_empty);
    // The Datadog `site` key always drives, defaulting to `datadoghq.com` when empty.
    endpoint.site = match site {
        Some(s) if !s.is_empty() => s,
        _ => "datadoghq.com".to_string(),
    };
    endpoint.additional_endpoints.0 = additional_endpoints;
}

/// `forwarder_timeout` (seconds) -> request timeout.
pub fn set_request_timeout_secs(config: &mut DatadogForwarderConfig, value: i64) {
    config.request_timeout_secs = value.max(0) as u64;
}

/// `forwarder_max_concurrent_requests` -> per-endpoint concurrency.
pub fn set_endpoint_concurrency(config: &mut DatadogForwarderConfig, value: i64) {
    config.endpoint_concurrency = value.max(0) as usize;
}

/// `forwarder_num_workers` -> endpoint concurrency multiplier.
pub fn set_endpoint_concurrency_multiplier(config: &mut DatadogForwarderConfig, value: i64) {
    config.endpoint_concurrency_multiplier = value.max(0) as usize;
}

/// `forwarder_high_prio_buffer_size` -> per-endpoint pending-request buffer size.
pub fn set_endpoint_buffer_size(config: &mut DatadogForwarderConfig, value: i64) {
    config.endpoint_buffer_size = value.max(0) as usize;
}

/// `forwarder_connection_reset_interval` (seconds) -> connection reset interval.
pub fn set_connection_reset_interval_secs(config: &mut DatadogForwarderConfig, value: i64) {
    config.connection_reset_interval_secs = value.max(0) as u64;
}

/// `forwarder_http_protocol` -> HTTP protocol enum. Unknown values fall back to `Auto`.
pub fn set_http_protocol(config: &mut DatadogForwarderConfig, value: String) {
    config.http_protocol = match value.as_str() {
        "http1" => ForwarderHttpProtocol::Http1,
        _ => ForwarderHttpProtocol::Auto,
    };
}

/// `min_tls_version` -> stored verbatim (the component clamps older versions to 1.2).
pub fn set_min_tls_version(config: &mut DatadogForwarderConfig, value: String) {
    config.min_tls_version = value;
}

/// `skip_ssl_validation` -> TLS validation skip flag.
pub fn set_skip_ssl_validation(config: &mut DatadogForwarderConfig, value: bool) {
    config.skip_ssl_validation = value;
}

/// `sslkeylogfile` -> NSS key-log file path.
pub fn set_sslkeylogfile(config: &mut DatadogForwarderConfig, value: String) {
    config.sslkeylogfile = value;
}

/// `allow_arbitrary_tags` -> arbitrary-tag header flag.
pub fn set_allow_arbitrary_tags(config: &mut DatadogForwarderConfig, value: bool) {
    config.allow_arbitrary_tags = value;
}

/// `forwarder_apikey_validation_interval` (minutes) -> API-key validation interval.
pub fn set_api_key_validation_interval_mins(config: &mut DatadogForwarderConfig, value: i64) {
    config.api_key_validation_interval_mins = value;
}

// ----- retry -----

/// `forwarder_backoff_factor` -> retry backoff factor.
pub fn set_backoff_factor(retry: &mut RetryConfiguration, value: i64) {
    retry.backoff_factor = value as f64;
}

/// `forwarder_backoff_base` (seconds) -> retry backoff base.
pub fn set_backoff_base(retry: &mut RetryConfiguration, value: i64) {
    retry.backoff_base = value as f64;
}

/// `forwarder_backoff_max` (seconds) -> retry backoff ceiling.
pub fn set_backoff_max(retry: &mut RetryConfiguration, value: i64) {
    retry.backoff_max = value as f64;
}

/// `forwarder_recovery_interval` -> error-count decrease factor on success.
pub fn set_recovery_error_decrease_factor(retry: &mut RetryConfiguration, value: i64) {
    retry.recovery_error_decrease_factor = value.max(0) as u32;
}

/// `forwarder_recovery_reset` -> whether a success resets the error count.
pub fn set_recovery_reset(retry: &mut RetryConfiguration, value: bool) {
    retry.recovery_reset = value;
}

/// `forwarder_retry_queue_payloads_max_size` (bytes) -> in-memory retry-queue size.
pub fn set_retry_queue_payloads_max_size(retry: &mut RetryConfiguration, value: i64) {
    retry.retry_queue_payloads_max_size = Some(value.max(0) as u64);
}

/// `forwarder_retry_queue_max_size` (bytes, deprecated) -> deprecated retry-queue size.
pub fn set_retry_queue_max_size(retry: &mut RetryConfiguration, value: i64) {
    retry.retry_queue_max_size = Some(value.max(0) as u64);
}

/// `forwarder_storage_max_size_in_bytes` -> on-disk retry-queue size.
pub fn set_storage_max_size_bytes(retry: &mut RetryConfiguration, value: i64) {
    retry.storage_max_size_bytes = value.max(0) as u64;
}

/// `forwarder_flush_to_disk_mem_ratio` -> in-memory-to-disk flush ratio.
pub fn set_flush_to_disk_mem_ratio(retry: &mut RetryConfiguration, value: f64) {
    retry.flush_to_disk_mem_ratio = value;
}

/// `forwarder_storage_path` -> on-disk retry-queue directory.
pub fn set_storage_path(retry: &mut RetryConfiguration, value: String) {
    retry.storage_path = std::path::PathBuf::from(value);
}

/// `forwarder_storage_max_disk_ratio` -> max disk usage ratio.
pub fn set_storage_max_disk_ratio(retry: &mut RetryConfiguration, value: f64) {
    retry.storage_max_disk_ratio = value;
}

/// `forwarder_outdated_file_in_days` -> max on-disk retry-file age.
pub fn set_outdated_file_in_days(retry: &mut RetryConfiguration, value: i64) {
    retry.outdated_file_in_days = value.max(0) as u32;
}

/// `forwarder_retry_queue_capacity_time_interval_sec` -> capacity estimation window.
pub fn set_capacity_time_interval_secs(retry: &mut RetryConfiguration, value: i64) {
    retry.capacity_time_interval_secs = value.max(0) as u64;
}

// ----- proxy -----

/// Returns a mutable reference to the proxy config, materializing the default when first touched.
fn proxy_mut(config: &mut DatadogForwarderConfig) -> &mut ProxyConfiguration {
    config.proxy.get_or_insert_with(ProxyConfiguration::default)
}

/// `proxy.http` -> HTTP proxy server (empty clears).
pub fn set_proxy_http(config: &mut DatadogForwarderConfig, value: String) {
    proxy_mut(config).http_server = non_empty(value);
}

/// `proxy.https` -> HTTPS proxy server (empty clears).
pub fn set_proxy_https(config: &mut DatadogForwarderConfig, value: String) {
    proxy_mut(config).https_server = non_empty(value);
}

/// `proxy.no_proxy` -> proxy bypass list.
pub fn set_proxy_no_proxy(config: &mut DatadogForwarderConfig, value: Vec<String>) {
    proxy_mut(config).no_proxy = value;
}

/// `no_proxy_nonexact_match` -> whether `no_proxy` uses suffix/CIDR matching.
pub fn set_no_proxy_nonexact_match(config: &mut DatadogForwarderConfig, value: bool) {
    proxy_mut(config).no_proxy_nonexact_match = value;
}

/// `use_proxy_for_cloud_metadata` -> whether proxy applies to cloud metadata endpoints.
pub fn set_use_proxy_for_cloud_metadata(config: &mut DatadogForwarderConfig, value: bool) {
    proxy_mut(config).use_proxy_for_cloud_metadata = value;
}

// ----- OPW / Vector metrics routing -----

/// `observability_pipelines_worker.metrics.enabled` -> OPW metrics routing flag.
pub fn set_opw_metrics_enabled(opw: &mut OpwMetricsConfiguration, value: bool) {
    opw.observability_pipelines_worker_enabled = value;
}

/// `observability_pipelines_worker.metrics.url` -> OPW metrics endpoint URL.
pub fn set_opw_metrics_url(opw: &mut OpwMetricsConfiguration, value: String) {
    opw.observability_pipelines_worker_url = value;
}

/// `vector.metrics.enabled` (deprecated) -> Vector metrics routing flag.
pub fn set_vector_metrics_enabled(opw: &mut OpwMetricsConfiguration, value: bool) {
    opw.vector_enabled = value;
}

/// `vector.metrics.url` (deprecated) -> Vector metrics endpoint URL.
pub fn set_vector_metrics_url(opw: &mut OpwMetricsConfiguration, value: String) {
    opw.vector_url = value;
}

// ----- multi-region failover (metrics domain) -----

/// `multi_region_failover.enabled` -> MRF enable flag.
pub fn set_mrf_enabled(mrf: &mut MrfConfig, value: bool) {
    mrf.enabled = value;
}

/// `multi_region_failover.failover_metrics` -> whether metrics participate in failover.
pub fn set_mrf_failover_metrics(mrf: &mut MrfConfig, value: bool) {
    mrf.failover_metrics = value;
}

/// `multi_region_failover.metric_allowlist` -> failover metric allowlist.
pub fn set_mrf_metric_allowlist(mrf: &mut MrfConfig, value: Vec<String>) {
    mrf.metric_allowlist = value;
}

/// `multi_region_failover.api_key` -> failover region API key (empty clears).
pub fn set_mrf_api_key(mrf: &mut MrfConfig, value: String) {
    mrf.api_key = non_empty(value);
}

/// `multi_region_failover.dd_url` -> failover region custom URL (empty clears).
pub fn set_mrf_dd_url(mrf: &mut MrfConfig, value: String) {
    mrf.dd_url = non_empty(value);
}

/// `multi_region_failover.site` -> failover region site (empty clears).
pub fn set_mrf_site(mrf: &mut MrfConfig, value: String) {
    mrf.site = non_empty(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_protocol_parses_known_and_falls_back() {
        let mut c = DatadogForwarderConfig::default();
        set_http_protocol(&mut c, "http1".to_string());
        assert_eq!(c.http_protocol, ForwarderHttpProtocol::Http1);
        set_http_protocol(&mut c, "nonsense".to_string());
        assert_eq!(c.http_protocol, ForwarderHttpProtocol::Auto);
    }

    #[test]
    fn endpoint_assembles_with_site_default_and_additional() {
        let mut ep = EndpointConfiguration::default();
        let mut additional = std::collections::HashMap::new();
        additional.insert("https://example.com".to_string(), vec!["k1".to_string()]);
        assemble_endpoint(
            &mut ep,
            Some("abc".to_string()),
            None,
            Some(String::new()),
            additional.clone(),
        );
        assert_eq!(ep.api_key, "abc");
        assert_eq!(ep.dd_url, None);
        assert_eq!(ep.site, "datadoghq.com");
        assert_eq!(ep.additional_endpoints.0, additional);
    }
}
