//! Multi-Region Failover domain. Self-contained: it carries its own failover endpoint (`api_key`,
//! `site`, `dd_url`), distinct from the primary forwarder endpoint in `shared.endpoints`.

use serde::Serialize;

/// Prefix of the Datadog failover-region metrics intake, completed by the failover site.
const MRF_METRICS_ENDPOINT_PREFIX: &str = "https://app.mrf.";

/// Resolved Multi-Region Failover configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Whether multi-region failover is active.
    ///
    /// Defaults to `false`. The failover pipeline is wired when the topology is built, so turning this on takes a
    /// restart, and it takes a reachable failover region as well: an [`api_key`](Self::api_key), plus either a
    /// [`site`](Self::site) or a [`dd_url`](Self::dd_url). Without those, the pipeline is not wired at all.
    pub enabled: bool,

    /// Which metrics are mirrored to the failover region.
    pub metric_mirroring: MetricMirroring,

    /// API key used to authenticate to the failover region.
    pub api_key: Option<String>,

    /// Datadog site of the failover region.
    pub site: Option<String>,

    /// Explicit intake URL for the failover region, overriding the site.
    pub dd_url: Option<String>,
}

/// Which metrics are mirrored to the failover region.
///
/// The two settings are grouped because they are consumed together: the routing state of the failover metrics
/// pipeline is derived from both at once. A consumer that watched them separately could rebuild that state from a
/// fresh value of one and a stale value of the other, describing a configuration that was never published; a live
/// view of this struct delivers both from one configuration version instead.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MetricMirroring {
    /// Whether metrics are mirrored to the failover region.
    ///
    /// Defaults to `false`. Mirroring also requires [`Domain::enabled`], but unlike it this setting is read live,
    /// which is the point of it: an operator starts and stops mirroring on a running process.
    pub enabled: bool,

    /// Metrics permitted to be sent to the failover region.
    ///
    /// Defaults to empty, which mirrors every metric rather than none: the list narrows mirroring, it does not enable
    /// it. Names match exactly. Read live, alongside [`enabled`](Self::enabled), so an operator can restrict mirroring
    /// to the metrics the failover region needs without restarting.
    pub allowlist: Vec<String>,
}

impl Domain {
    /// Returns the failover-region metrics intake URL, if the region is addressable.
    ///
    /// [`dd_url`](Self::dd_url) takes precedence and is used as provided. When only
    /// [`site`](Self::site) is set, the Datadog failover metrics intake is derived from it. Neither
    /// setting has a default, so a failover region that is configured with neither has no endpoint.
    pub fn metrics_endpoint_url(&self) -> Option<String> {
        self.dd_url.clone().or_else(|| {
            self.site
                .as_deref()
                .map(|site| format!("{MRF_METRICS_ENDPOINT_PREFIX}{site}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Domain;

    #[test]
    fn dd_url_takes_precedence_over_site() {
        let domain = Domain {
            site: Some("datadoghq.eu".to_string()),
            dd_url: Some("https://custom-mrf.example.com".to_string()),
            ..Default::default()
        };

        assert_eq!(
            Some("https://custom-mrf.example.com".to_string()),
            domain.metrics_endpoint_url()
        );
    }

    #[test]
    fn the_site_derives_the_failover_metrics_intake() {
        let domain = Domain {
            site: Some("datadoghq.eu".to_string()),
            ..Default::default()
        };

        assert_eq!(
            Some("https://app.mrf.datadoghq.eu".to_string()),
            domain.metrics_endpoint_url()
        );
    }

    #[test]
    fn a_region_configured_with_neither_setting_has_no_endpoint() {
        assert_eq!(None, Domain::default().metrics_endpoint_url());
    }
}
