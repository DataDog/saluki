//! Builds endpoint routing groups from exact-name metric allow lists.

use std::collections::{BTreeMap, HashMap};

use agent_data_plane_config::shared;
use saluki_error::{generic_error, GenericError};

/// A group of configured endpoints that share one exact-name metric allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointAllowlistGroup {
    /// Configured endpoint identities targeted by this policy.
    pub endpoints: Vec<String>,

    /// Exact metric names permitted to reach these endpoints.
    pub metric_allowlist: Vec<String>,
}

/// Endpoint-aware selective metric-routing configuration.
#[derive(Debug, Eq, PartialEq)]
pub struct MetricsEndpointRoutingConfiguration {
    selected_endpoints: Vec<String>,
    policy_groups: Vec<EndpointAllowlistGroup>,
}

impl MetricsEndpointRoutingConfiguration {
    /// Resolves endpoint policies against the configured primary and additional endpoints.
    ///
    /// An empty policy map preserves ordinary endpoint routing. Every policy key must exactly match a key in
    /// `additional_endpoints` or the effective primary endpoint; this prevents a typo from silently leaving an intended
    /// capacity-saving destination on the unfiltered path. Equivalent allow lists are grouped so they can share one
    /// filtered encoder branch.
    ///
    /// # Errors
    ///
    /// Returns an error if a policy names neither the primary endpoint nor an endpoint configured in
    /// `additional_endpoints`.
    pub fn from_configuration(
        metric_allowlists: &HashMap<String, Vec<String>>, endpoints: &shared::Endpoints,
    ) -> Result<Self, GenericError> {
        let mut selected_endpoints = Vec::with_capacity(metric_allowlists.len());
        let mut grouped_endpoints = BTreeMap::<Vec<String>, Vec<String>>::new();
        let primary_endpoint = endpoints.primary_endpoint();

        for (endpoint, metric_allowlist) in metric_allowlists {
            if endpoint != &primary_endpoint && !endpoints.additional_endpoints.contains_key(endpoint) {
                return Err(generic_error!(
                    "Experimental metrics endpoint-routing policy endpoint '{}' does not match the configured primary \
                     endpoint and is not present in `additional_endpoints`; correct the endpoint, add it and its API \
                     key to `additional_endpoints`, or remove it from \
                     `experimental.metrics_endpoint_routing.metric_allowlist`.",
                    endpoint
                ));
            }

            selected_endpoints.push(endpoint.clone());

            let mut canonical_allowlist = metric_allowlist.clone();
            canonical_allowlist.sort_unstable();
            canonical_allowlist.dedup();
            if !canonical_allowlist.is_empty() {
                grouped_endpoints
                    .entry(canonical_allowlist)
                    .or_default()
                    .push(endpoint.clone());
            }
        }

        selected_endpoints.sort_unstable();
        let policy_groups = grouped_endpoints
            .into_iter()
            .map(|(metric_allowlist, mut endpoints)| {
                endpoints.sort_unstable();
                EndpointAllowlistGroup {
                    endpoints,
                    metric_allowlist,
                }
            })
            .collect();

        Ok(Self {
            selected_endpoints,
            policy_groups,
        })
    }

    /// Returns every configured endpoint removed from the ordinary unfiltered metric path.
    pub fn selected_endpoints(&self) -> &[String] {
        &self.selected_endpoints
    }

    /// Returns the filtered routing groups that require an encoder branch.
    ///
    /// Selected endpoints with an empty allowlist are absent because they receive no metric payloads.
    pub fn policy_groups(&self) -> &[EndpointAllowlistGroup] {
        &self.policy_groups
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_data_plane_config::ConfigValue;

    use super::*;

    const PRIMARY: &str = "https://primary.example.com";

    fn endpoints() -> shared::Endpoints {
        shared::Endpoints {
            dd_url: ConfigValue::explicit(PRIMARY.to_string()),
            additional_endpoints: HashMap::from([
                ("https://secondary-a.example.com".to_string(), vec!["key-a".to_string()]),
                ("https://secondary-b.example.com".to_string(), vec!["key-b".to_string()]),
                ("https://secondary-c.example.com".to_string(), vec!["key-c".to_string()]),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn empty_policy_map_preserves_the_ordinary_endpoint_path() {
        let allowlists = HashMap::new();

        let config = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &endpoints())
            .expect("empty routing should be valid");
        assert!(config.selected_endpoints().is_empty());
        assert!(config.policy_groups().is_empty());
    }

    #[test]
    fn groups_equivalent_primary_and_additional_allowlists_and_keeps_empty_policies_selected() {
        let allowlists = HashMap::from([
            (
                PRIMARY.to_string(),
                vec!["metric.b".to_string(), "metric.a".to_string(), "metric.a".to_string()],
            ),
            (
                "https://secondary-b.example.com".to_string(),
                vec!["metric.a".to_string(), "metric.b".to_string()],
            ),
            ("https://secondary-c.example.com".to_string(), Vec::new()),
        ]);

        let config = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &endpoints())
            .expect("configured endpoints should resolve");
        assert_eq!(
            config.selected_endpoints(),
            [
                "https://primary.example.com",
                "https://secondary-b.example.com",
                "https://secondary-c.example.com"
            ]
        );
        assert_eq!(
            config.policy_groups(),
            [EndpointAllowlistGroup {
                endpoints: vec![
                    "https://primary.example.com".to_string(),
                    "https://secondary-b.example.com".to_string()
                ],
                metric_allowlist: vec!["metric.a".to_string(), "metric.b".to_string()]
            }]
        );
    }

    #[test]
    fn accepts_a_site_derived_primary_endpoint() {
        let endpoints = shared::Endpoints {
            site: ConfigValue::explicit("us5.datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };
        let allowlists = HashMap::from([(
            "https://app.us5.datadoghq.com".to_string(),
            vec!["metric.a".to_string()],
        )]);

        let config = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &endpoints)
            .expect("site-derived primary endpoint should resolve");
        assert_eq!(config.selected_endpoints(), ["https://app.us5.datadoghq.com"]);
    }

    #[test]
    fn rejects_a_policy_that_does_not_name_a_configured_endpoint() {
        let allowlists = HashMap::from([("https://typo.example.com".to_string(), vec!["metric.a".to_string()])]);

        let error = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &endpoints())
            .expect_err("unknown endpoint should be rejected");
        let message = error.to_string();
        assert!(message.contains("https://typo.example.com"));
        assert!(message.contains("primary endpoint"));
        assert!(message.contains("additional_endpoints"));
    }
}
