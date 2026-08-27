//! Shared configuration fixtures for the Datadog component tests.
//!
//! The typed model's own `Default` is all zero values, because a witnessed setting's default belongs
//! to the source schema and translation always writes it. A component test therefore cannot use
//! `SharedConfiguration::default()` as a stand-in for "nothing configured": it would hand the
//! component a zero payload limit and an empty compressor. These fixtures state the defaults a
//! translated Agent configuration carries, so a test only has to state what it is actually varying.

use std::sync::Arc;
use std::time::Duration;

use agent_data_plane_config::{
    shared::{Compression, Endpoints, Forwarder, MetricsEncoding, SharedConfiguration, Tls},
    ConfigValue, Live, SalukiConfiguration,
};
use arc_swap::ArcSwap;
use tokio::sync::watch;

use super::endpoints::{ApiKeyView, LiveApiKeys};

/// Test API key, distinct from any endpoint-specific key a test configures.
pub(crate) const TEST_API_KEY: &str = "test-api-key";

/// Returns shared configuration carrying the schema defaults for the settings the Datadog
/// components read.
///
/// This is not a complete translated configuration: a setting no component here consults is left at
/// its zero-valued model default rather than restated. Add a field when a test needs it, and read the
/// schema for the default rather than assuming this fixture supplies one.
pub(crate) fn shared_configuration() -> SharedConfiguration {
    SharedConfiguration {
        endpoints: Endpoints {
            api_key: TEST_API_KEY.to_string(),
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            compression: Compression {
                compressor_kind: "zstd".to_string(),
                ..Default::default()
            },
            tls: Tls {
                min_tls_version: "tlsv1.2".to_string(),
                handshake_timeout: Duration::from_secs(10),
                ..Default::default()
            },
            forwarder: Forwarder {
                apikey_validation_interval: 60,
                backoff_base: 2.0,
                backoff_factor: 2.0,
                backoff_max: 64.0,
                flush_to_disk_mem_ratio: 0.5,
                high_prio_buffer_size: 100,
                max_concurrent_requests: 10,
                num_workers: 1,
                outdated_file_in_days: 10,
                recovery_interval: 2,
                retry_queue_capacity_time_interval_sec: 900,
                retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
                storage_max_disk_ratio: 0.8,
                timeout: 20,
                ..Default::default()
            },
            ..Default::default()
        },
        metrics_encoding: MetricsEncoding {
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            use_v2_series_api: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A configuration cell a test can replace, and the live views projected out of it.
///
/// This stands in for the configuration system: `store` is what the system does once it has
/// translated an update, and a view created here reads whatever was stored last.
pub(crate) struct LiveConfiguration {
    cell: Arc<ArcSwap<SalukiConfiguration>>,

    // The views hold receivers, so keeping the sender alive keeps the channel open.
    tick: watch::Sender<()>,
}

impl LiveConfiguration {
    /// Creates a cell holding `config`.
    pub(crate) fn new(config: SalukiConfiguration) -> Self {
        Self {
            cell: Arc::new(ArcSwap::from_pointee(config)),
            tick: watch::channel(()).0,
        }
    }

    /// Replaces the stored configuration and notifies the views, as the system does after a
    /// successful translation.
    pub(crate) fn store(&self, config: SalukiConfiguration) {
        self.cell.store(Arc::new(config));
        self.tick.send_replace(());
    }

    /// Returns the live views a forwarder's endpoints refresh their API keys from.
    pub(crate) fn api_keys(&self) -> LiveApiKeys {
        LiveApiKeys {
            primary: Some(ApiKeyView::Required(
                self.live(|config| &config.shared.endpoints.api_key),
            )),
            additional: Some(self.live(|config| &config.shared.endpoints.additional_endpoints)),
        }
    }

    /// Returns a live view of one projection of the stored configuration.
    pub(crate) fn live<T>(
        &self, project: impl for<'a> Fn(&'a SalukiConfiguration) -> &'a T + Send + Sync + 'static,
    ) -> Live<T>
    where
        T: Clone + PartialEq + 'static,
    {
        Live::new_dynamic(Arc::clone(&self.cell), self.tick.subscribe(), project)
    }
}
