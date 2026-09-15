//! Endpoint-aware selective metric-routing configuration.

use std::collections::{BTreeMap, HashMap};

use agent_data_plane_config::domains::metric_mirroring;
use saluki_error::{generic_error, GenericError};

/// A group of configured secondary endpoints that share one exact-name series allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricMirroringPolicyGroup {
    /// Configured endpoint identities targeted by this policy.
    pub endpoints: Vec<String>,

    /// Exact series metric names permitted to reach these endpoints.
    pub metric_allowlist: Vec<String>,
}

/// Endpoint-aware selective metric-routing configuration.
#[derive(Debug, Eq, PartialEq)]
pub struct MetricMirroringConfiguration {
    selected_endpoints: Vec<String>,
    policy_groups: Vec<MetricMirroringPolicyGroup>,
}

impl MetricMirroringConfiguration {
    /// Resolves endpoint policies against the configured additional endpoints.
    ///
    /// An empty policy map preserves ordinary endpoint routing. Every policy key must exactly match a key in
    /// `additional_endpoints`; this prevents a typo from silently leaving an intended capacity-saving destination on
    /// the unfiltered path. Equivalent allow lists are grouped so they can share one filtered encoder branch.
    ///
    /// # Errors
    ///
    /// Returns an error if an enabled policy names an endpoint that is not configured in `additional_endpoints`.
    pub fn from_configuration(
        mirroring: &metric_mirroring::Domain, additional_endpoints: &HashMap<String, Vec<String>>,
    ) -> Result<Self, GenericError> {
        let mut selected_endpoints = Vec::with_capacity(mirroring.metric_allowlists.len());
        let mut grouped_endpoints = BTreeMap::<Vec<String>, Vec<String>>::new();

        for (endpoint, metric_allowlist) in &mirroring.metric_allowlists {
            if !additional_endpoints.contains_key(endpoint) {
                return Err(generic_error!(
                    "Experimental metric-mirroring policy endpoint '{}' is not present in `additional_endpoints`; add \
                     the endpoint and its API key there, or remove it from \
                     `serializer_experimental.metric_allowlist`.",
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
                MetricMirroringPolicyGroup {
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

    /// Returns every configured secondary endpoint removed from the ordinary unfiltered metrics path.
    pub fn selected_endpoints(&self) -> &[String] {
        &self.selected_endpoints
    }

    /// Returns the filtered routing groups that require an encoder branch.
    ///
    /// Selected endpoints with an empty allowlist are absent because they receive no payloads.
    pub fn policy_groups(&self) -> &[MetricMirroringPolicyGroup] {
        &self.policy_groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn additional_endpoints() -> HashMap<String, Vec<String>> {
        HashMap::from([
            ("https://secondary-a.example.com".to_string(), vec!["key-a".to_string()]),
            ("https://secondary-b.example.com".to_string(), vec!["key-b".to_string()]),
            ("https://secondary-c.example.com".to_string(), vec!["key-c".to_string()]),
        ])
    }

    #[test]
    fn empty_policy_map_preserves_the_ordinary_endpoint_path() {
        let domain = metric_mirroring::Domain {
            metric_allowlists: HashMap::new(),
        };

        let config = MetricMirroringConfiguration::from_configuration(&domain, &additional_endpoints())
            .expect("empty routing should be valid");
        assert!(config.selected_endpoints().is_empty());
        assert!(config.policy_groups().is_empty());
    }

    #[test]
    fn groups_equivalent_allowlists_and_keeps_empty_policies_selected() {
        let domain = metric_mirroring::Domain {
            metric_allowlists: HashMap::from([
                (
                    "https://secondary-a.example.com".to_string(),
                    vec!["metric.b".to_string(), "metric.a".to_string(), "metric.a".to_string()],
                ),
                (
                    "https://secondary-b.example.com".to_string(),
                    vec!["metric.a".to_string(), "metric.b".to_string()],
                ),
                ("https://secondary-c.example.com".to_string(), Vec::new()),
            ]),
        };

        let config = MetricMirroringConfiguration::from_configuration(&domain, &additional_endpoints())
            .expect("configured endpoints should resolve");
        assert_eq!(
            config.selected_endpoints(),
            [
                "https://secondary-a.example.com",
                "https://secondary-b.example.com",
                "https://secondary-c.example.com"
            ]
        );
        assert_eq!(
            config.policy_groups(),
            [MetricMirroringPolicyGroup {
                endpoints: vec![
                    "https://secondary-a.example.com".to_string(),
                    "https://secondary-b.example.com".to_string()
                ],
                metric_allowlist: vec!["metric.a".to_string(), "metric.b".to_string()]
            }]
        );
    }

    #[test]
    fn rejects_a_policy_that_does_not_name_an_additional_endpoint() {
        let domain = metric_mirroring::Domain {
            metric_allowlists: HashMap::from([("https://typo.example.com".to_string(), vec!["metric.a".to_string()])]),
        };

        let error = MetricMirroringConfiguration::from_configuration(&domain, &additional_endpoints())
            .expect_err("unknown endpoint should be rejected");
        let message = error.to_string();
        assert!(message.contains("https://typo.example.com"));
        assert!(message.contains("additional_endpoints"));
    }
}
