//! Shared fixtures for the Datadog component tests.
//!
//! The fixtures state the values a translated Agent configuration carries when nothing is configured, so a test only
//! has to state what it is actually varying. They copy those defaults by hand, so they can drift from the schema.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use saluki_common::sync::live::{self, LivePublisher, LiveSource};

use super::api_key::{ApiKeyView, LiveApiKeys};
use super::config::{ForwarderConfiguration, ForwarderHttpProtocol, OpwMetricsConfiguration};
use super::endpoints::EndpointConfiguration;
use super::protocol::{UseV3ApiConfig, UseV3ApiSeriesConfig, V3ApiConfig, V3SeriesMode};
use super::proxy::ProxyConfiguration;
use super::retry::RetryConfiguration;
use crate::encoders::DatadogMetricsConfiguration;

/// Test API key, distinct from any endpoint-specific key a test configures.
pub(crate) const TEST_API_KEY: &str = "test-api-key";

/// Primary endpoint the Agent's default `site` resolves to.
pub(crate) const DEFAULT_PRIMARY_ENDPOINT: &str = "https://app.datadoghq.com";

/// Returns the V3 series settings the Agent's schema defaults resolve to.
pub(crate) fn default_series_config() -> UseV3ApiSeriesConfig {
    UseV3ApiSeriesConfig {
        enabled: V3SeriesMode::DatadogOnly,
        endpoints: HashMap::new(),
    }
}

/// Returns retry settings carrying the Agent's schema defaults.
pub(crate) fn retry_configuration() -> RetryConfiguration {
    RetryConfiguration {
        backoff_factor: 2.0,
        backoff_base: 2.0,
        backoff_max: 64.0,
        recovery_error_decrease_factor: 2,
        recovery_reset: false,
        queue_max_size_bytes: 15 * 1024 * 1024,
        storage_max_size_bytes: 0,
        flush_to_disk_mem_ratio: 0.5,
        storage_path: PathBuf::new(),
        run_path: None,
        storage_max_disk_ratio: 0.8,
        outdated_file_in_days: 10,
        capacity_time_interval_secs: 900,
    }
}

/// Returns forwarder settings carrying the Agent's schema defaults, sending to the default primary endpoint.
pub(crate) fn forwarder_configuration() -> ForwarderConfiguration {
    ForwarderConfiguration {
        endpoint_concurrency: 10,
        endpoint_concurrency_multiplier: 1,
        request_timeout_secs: 20,
        endpoint_buffer_size: 100,
        endpoint: EndpointConfiguration::new(
            TEST_API_KEY.to_string(),
            DEFAULT_PRIMARY_ENDPOINT.to_string(),
            HashMap::new(),
        ),
        retry: retry_configuration(),
        proxy: ProxyConfiguration {
            http: String::new(),
            https: String::new(),
            no_proxy: Vec::new(),
            no_proxy_nonexact_match: false,
            use_proxy_for_cloud_metadata: false,
        },
        opw_metrics: OpwMetricsConfiguration::default(),
        http_protocol: ForwarderHttpProtocol::Auto,
        connection_reset_interval_secs: 0,
        stop_wait_for_inflight: false,
        v3_api: V3ApiConfig::default(),
        use_v3_api: UseV3ApiConfig {
            series: default_series_config(),
        },
        serializer_compressor_kind: "zstd".to_string(),
        skip_ssl_validation: false,
        sslkeylogfile: String::new(),
        min_tls_version: "tlsv1.2".to_string(),
        tls_handshake_timeout: Duration::from_secs(10),
        allow_arbitrary_tags: false,
        api_key_validation_interval_mins: 60,
        routing_targets: None,
    }
}

/// Returns metrics encoder settings carrying the Agent's schema defaults, sending to the default primary endpoint.
pub(crate) fn datadog_metrics_configuration() -> DatadogMetricsConfiguration {
    DatadogMetricsConfiguration {
        max_metrics_per_payload: 10_000,
        max_payload_size: 2_621_440,
        max_uncompressed_payload_size: 4_194_304,
        max_series_payload_size: 512_000,
        max_series_uncompressed_payload_size: 5_242_880,
        max_series_points_per_payload: 10_000,
        flush_timeout: Duration::from_secs(2),
        compressor_kind: "zstd".to_string(),
        zstd_compressor_level: 3,
        use_v2_series_api: true,
        log_payloads: false,
        additional_tags: None,
        v3_api: V3ApiConfig::default(),
        use_v3_api: UseV3ApiConfig {
            series: default_series_config(),
        },
        opw_metrics: OpwMetricsConfiguration::default(),
        primary_endpoint: DEFAULT_PRIMARY_ENDPOINT.to_string(),
        additional_endpoints: HashMap::new(),
        routing: None,
    }
}

/// The values a test changes while a forwarder runs.
#[derive(Clone, Default)]
pub(crate) struct LiveRoot {
    /// Primary API key.
    pub(crate) api_key: String,

    /// Additional endpoints, keyed by URL with their API keys.
    pub(crate) additional_endpoints: HashMap<String, Vec<String>>,

    /// API key of a single-destination override, which configuration can leave unset.
    pub(crate) override_api_key: Option<String>,

    /// Whether secret resolution might replace a rejected API key.
    pub(crate) secrets_in_use: bool,
}

/// Values a test can replace, and the live views projected out of them.
///
/// This stands in for the configuration system: `publish` is what the system does once it has translated an update.
/// Keeping the publisher keeps the views' channel open.
pub(crate) struct LiveConfiguration {
    pub(crate) publisher: LivePublisher<LiveRoot>,
    pub(crate) source: LiveSource<LiveRoot>,
}

impl LiveConfiguration {
    /// Creates a channel holding `root`.
    pub(crate) fn new(root: LiveRoot) -> Self {
        let (publisher, source) = live::channel(root);
        Self { publisher, source }
    }

    /// Returns the live views a Datadog forwarder's endpoints take their API keys from.
    pub(crate) fn api_keys(&self) -> LiveApiKeys {
        LiveApiKeys {
            primary: Some(ApiKeyView::Required(self.source.live(|root| &root.api_key))),
            additional: Some(self.source.live(|root| &root.additional_endpoints)),
        }
    }
}
