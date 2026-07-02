//! Multi-Region Failover domain. Self-contained: it carries its own failover endpoint (`api_key`,
//! `site`, `dd_url`), distinct from the primary forwarder endpoint in `shared.endpoints`.

use serde::Serialize;

/// Resolved Multi-Region Failover configuration.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Domain {
    /// Whether multi-region failover is active.
    pub enabled: bool,

    /// Whether metrics are mirrored to the failover region.
    pub failover_metrics: bool,

    /// Metrics permitted to be sent to the failover region.
    pub metric_allowlist: Vec<String>,

    /// API key used to authenticate to the failover region.
    pub api_key: Option<String>,

    /// Datadog site of the failover region.
    pub site: Option<String>,

    /// Explicit intake URL for the failover region, overriding the site.
    pub dd_url: Option<String>,
}
