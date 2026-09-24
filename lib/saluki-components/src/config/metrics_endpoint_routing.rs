//! Builds endpoint routing groups from exact-name and literal-prefix metric allow lists.

use std::collections::{BTreeMap, HashMap};

use agent_data_plane_config::shared;
use saluki_error::{generic_error, GenericError};

/// A group of configured endpoints that share exact-name and prefix metric allowlists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointAllowlistGroup {
    /// Configured endpoint identities targeted by this policy.
    pub endpoints: Vec<String>,

    /// Exact metric names permitted to reach these endpoints.
    pub metric_allowlist: Vec<String>,
    /// Literal metric-name prefixes permitted to reach these endpoints.
    pub metric_prefix_allowlist: Vec<String>,
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
        metric_allowlists: &HashMap<String, Vec<String>>, metric_prefix_allowlists: &HashMap<String, Vec<String>>,
        endpoints: &shared::Endpoints,
    ) -> Result<Self, GenericError> {
        let mut selected_endpoints = metric_allowlists
            .keys()
            .chain(metric_prefix_allowlists.keys())
            .cloned()
            .collect::<Vec<_>>();
        selected_endpoints.sort_unstable();
        selected_endpoints.dedup();
        let mut grouped_endpoints = BTreeMap::<(Vec<String>, Vec<String>), Vec<String>>::new();
        let primary_endpoint = endpoints.primary_endpoint();

        for endpoint in &selected_endpoints {
            if endpoint != &primary_endpoint && !endpoints.additional_endpoints.contains_key(endpoint) {
                return Err(generic_error!(
                    "Experimental metrics endpoint-routing policy endpoint '{}' does not match the configured primary \
                     endpoint and is not present in `additional_endpoints`; correct the endpoint, add it and its API \
                     key to `additional_endpoints`, or remove it from \
                     `experimental.metrics_endpoint_routing.metric_allowlist` and \
                     `experimental.metrics_endpoint_routing.metric_prefix_allowlist`.",
                    endpoint
                ));
            }

            let mut canonical_allowlist = metric_allowlists.get(endpoint).cloned().unwrap_or_default();
            canonical_allowlist.sort_unstable();
            canonical_allowlist.dedup();
            let mut canonical_prefixes = metric_prefix_allowlists.get(endpoint).cloned().unwrap_or_default();
            canonical_prefixes.sort_unstable();
            canonical_prefixes.dedup();
            if !canonical_allowlist.is_empty() || !canonical_prefixes.is_empty() {
                grouped_endpoints
                    .entry((canonical_allowlist, canonical_prefixes))
                    .or_default()
                    .push(endpoint.clone());
            }
        }

        let policy_groups = grouped_endpoints
            .into_iter()
            .map(|((metric_allowlist, metric_prefix_allowlist), mut endpoints)| {
                endpoints.sort_unstable();
                EndpointAllowlistGroup {
                    endpoints,
                    metric_allowlist,
                    metric_prefix_allowlist,
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

        let config =
            MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &HashMap::new(), &endpoints())
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

        let config =
            MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &HashMap::new(), &endpoints())
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
                metric_allowlist: vec!["metric.a".to_string(), "metric.b".to_string()],
                metric_prefix_allowlist: vec![],
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

        let config = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &HashMap::new(), &endpoints)
            .expect("site-derived primary endpoint should resolve");
        assert_eq!(config.selected_endpoints(), ["https://app.us5.datadoghq.com"]);
    }

    #[test]
    fn rejects_a_policy_that_does_not_name_a_configured_endpoint() {
        let allowlists = HashMap::from([("https://typo.example.com".to_string(), vec!["metric.a".to_string()])]);

        let error = MetricsEndpointRoutingConfiguration::from_configuration(&allowlists, &HashMap::new(), &endpoints())
            .expect_err("unknown endpoint should be rejected");
        let message = error.to_string();
        assert!(message.contains("https://typo.example.com"));
        assert!(message.contains("primary endpoint"));
        assert!(message.contains("additional_endpoints"));
    }

    #[test]
    fn combines_policy_maps_and_groups_only_identical_name_and_prefix_lists() {
        let names = HashMap::from([
            (PRIMARY.to_string(), vec!["exact".to_string()]),
            ("https://secondary-a.example.com".to_string(), vec!["exact".to_string()]),
        ]);
        let prefixes = HashMap::from([
            (
                PRIMARY.to_string(),
                vec!["b.".to_string(), "a.".to_string(), "a.".to_string()],
            ),
            (
                "https://secondary-a.example.com".to_string(),
                vec!["a.".to_string(), "b.".to_string()],
            ),
            ("https://secondary-b.example.com".to_string(), vec!["a.".to_string()]),
            ("https://secondary-c.example.com".to_string(), vec![]),
        ]);
        let config = MetricsEndpointRoutingConfiguration::from_configuration(&names, &prefixes, &endpoints()).unwrap();
        assert_eq!(config.selected_endpoints().len(), 4);
        assert_eq!(config.policy_groups().len(), 2);
        let combined = config
            .policy_groups()
            .iter()
            .find(|p| !p.metric_allowlist.is_empty())
            .unwrap();
        assert_eq!(combined.endpoints, [PRIMARY, "https://secondary-a.example.com"]);
        assert_eq!(combined.metric_prefix_allowlist, ["a.", "b."]);
        let prefix_only = config
            .policy_groups()
            .iter()
            .find(|p| p.metric_allowlist.is_empty())
            .unwrap();
        assert_eq!(prefix_only.endpoints, ["https://secondary-b.example.com"]);
        assert_eq!(prefix_only.metric_prefix_allowlist, ["a."]);
    }

    #[test]
    fn different_prefixes_do_not_share_an_encoder_group() {
        let names = HashMap::from([
            (PRIMARY.to_string(), vec!["exact".to_string()]),
            ("https://secondary-a.example.com".to_string(), vec!["exact".to_string()]),
        ]);
        let prefixes = HashMap::from([(PRIMARY.to_string(), vec!["a.".to_string()])]);
        let config = MetricsEndpointRoutingConfiguration::from_configuration(&names, &prefixes, &endpoints()).unwrap();
        assert_eq!(config.policy_groups().len(), 2);
    }

    #[test]
    fn rejects_unknown_endpoints_in_prefix_map_including_empty_policies() {
        for list in [vec![], vec!["metric.".to_string()]] {
            let prefixes = HashMap::from([("https://typo.example.com".to_string(), list)]);
            let error =
                MetricsEndpointRoutingConfiguration::from_configuration(&HashMap::new(), &prefixes, &endpoints())
                    .unwrap_err();
            assert!(error.to_string().contains("metric_prefix_allowlist"));
        }
    }
}
