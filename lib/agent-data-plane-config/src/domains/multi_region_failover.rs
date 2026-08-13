//! Multi-Region Failover domain. Self-contained: it carries its own failover endpoint (`api_key`,
//! `site`, `dd_url`), distinct from the primary forwarder endpoint in `shared.endpoints`.

use serde::Serialize;

/// Prefix of the Datadog failover-region metrics intake, completed by the failover site.
const MRF_METRICS_ENDPOINT_PREFIX: &str = "https://app.mrf.";

/// Resolved Multi-Region Failover configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
