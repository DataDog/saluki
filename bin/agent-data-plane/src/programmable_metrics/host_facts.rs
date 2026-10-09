//! Host facts: what lowering needs to know about the running process, captured once at boot.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_components::config::ClusterAgentConfiguration;
use saluki_core::data_model::tags::SharedTagSet;
use saluki_env::{EnvironmentProvider as _, HostProvider as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::canonical::MAX_SAFE_INTEGER;

use crate::{
    components::host_tags::fetch_host_tags, config::DataPlaneConfiguration, internal::env::ADPEnvironmentProvider,
};

/// Facts about the host and the process, fixed for the life of the process.
///
/// Lowering is a pure function of configuration and these facts: anything it needs that configuration alone does not
/// determine, such as a value read from the process environment or fetched from the Core Agent, is captured here once,
/// at boot, so that re-lowering on a configuration change never reads it again and never resets it.
#[derive(Clone, Debug, PartialEq)]
pub struct HostFacts {
    /// The wall-clock time at which the process started.
    ///
    /// Time-based parameters derive from it, so that re-lowering never resets them: the host-tags deadline is the
    /// process start plus `expected_tags_duration`.
    pub process_start: SystemTime,

    /// The resolved hostname.
    pub hostname: String,

    /// Whether the process runs in standalone mode, without the Core Agent.
    ///
    /// This is the configuration's `data_plane.standalone_mode` at boot. Lowering decides structure from the startup
    /// configuration itself, as `run.rs` does, which agrees with it.
    pub standalone: bool,

    /// The host's tags, fetched from the Core Agent at boot, when host tags enrichment is enabled.
    ///
    /// Enrichment is enabled only when the process is connected to the Core Agent and `expected_tags_duration` is not
    /// zero. When it is enabled, a failure to fetch the tags fails the boot, as it does natively.
    pub host_tags: Option<SharedTagSet>,

    /// The Cluster Agent endpoint that autoscaling failover forwards to, resolved at boot.
    ///
    /// This is the endpoint [`ClusterAgentConfiguration::endpoint_and_token`] resolves: `None` unless forwarding to the
    /// Cluster Agent is enabled, an `https` endpoint resolves from `cluster_agent.url` or from the Kubernetes service
    /// environment, and a token is configured. Service discovery reads the process environment, which is why the
    /// endpoint is a host fact.
    pub cluster_agent_endpoint: Option<String>,
}

impl HostFacts {
    /// Captures the host facts at boot.
    ///
    /// `process_start` is the wall-clock time at which the process started. The hostname comes from the environment
    /// provider; host tags, when enrichment is enabled, from the Core Agent, which `client_config` reaches.
    ///
    /// # Errors
    ///
    /// Returns an error if the hostname cannot be resolved, or if host tags enrichment is enabled and the tags cannot
    /// be fetched, both of which fail the boot natively too.
    pub async fn capture(
        process_start: SystemTime, config: &SalukiConfiguration, env_provider: &ADPEnvironmentProvider,
        client_config: Option<&RemoteAgentClientConfiguration>,
    ) -> Result<Self, GenericError> {
        let dp = DataPlaneConfiguration::from_configuration(config);
        let standalone = dp.standalone_mode();
        let hostname = env_provider
            .host()
            .get_hostname()
            .await
            .error_context("Failed to get the hostname for programmable metrics.")?;

        let host_tags = if host_tags_enabled(config) {
            let client_config = client_config
                .ok_or_else(|| generic_error!("Remote Agent client configuration is required in connected mode."))?;
            Some(fetch_host_tags(client_config).await?)
        } else {
            None
        };

        Ok(Self {
            process_start,
            hostname,
            standalone,
            host_tags,
            cluster_agent_endpoint: cluster_agent_configuration(config)
                .endpoint_and_token()
                .map(|(endpoint, _)| endpoint),
        })
    }

    /// Returns the host-tags deadline for the given expected tags duration, as a whole number of seconds since the
    /// Unix epoch.
    ///
    /// The deadline is the process start plus `expected_tags_duration`, rounded up to the next whole second, so that
    /// a plan can always write it as a duration: the wall clock reads the process start to the nanosecond, and a
    /// nanosecond count since the epoch is beyond the integers a plan holds. Rounding up keeps host tags at least as
    /// long as configured, as the native deadline, which starts when the transform is built, after the process starts.
    /// A process start before the epoch counts as the epoch, and a deadline later than 2^53 − 1 seconds since the
    /// epoch (about 285 million years) is that many seconds.
    pub fn host_tags_deadline(&self, expected_tags_duration: Duration) -> Duration {
        let deadline = self
            .process_start
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_add(expected_tags_duration);
        let secs = deadline
            .as_secs()
            .saturating_add(u64::from(deadline.subsec_nanos() != 0))
            .min(MAX_SAFE_INTEGER as u64);
        Duration::from_secs(secs)
    }
}

/// Returns `true` if host tags enrichment is enabled: when the process is connected to the Core Agent and
/// `expected_tags_duration` is not zero, as `run.rs` decides whether to add the `host_tags` transform.
pub(super) fn host_tags_enabled(config: &SalukiConfiguration) -> bool {
    !DataPlaneConfiguration::from_configuration(config).standalone_mode()
        && !config.shared.tags.expected_tags_duration.is_zero()
}

/// Returns the Cluster Agent configuration, as `run.rs` builds it for the autoscaling failover branch.
pub(super) fn cluster_agent_configuration(config: &SalukiConfiguration) -> ClusterAgentConfiguration {
    let cluster_agent = &config.shared.cluster_agent;
    ClusterAgentConfiguration {
        enabled: cluster_agent.enabled,
        url: cluster_agent.url.clone(),
        auth_token: cluster_agent.auth_token.clone(),
        kubernetes_service_name: cluster_agent.kubernetes_service_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::programmable_metrics::test_support::{configuration, environment_provider};

    #[tokio::test]
    async fn standalone_facts_need_no_core_agent() {
        let config = configuration(
            r#"
            cluster_agent:
              enabled: true
              url: cluster-agent.example.com:5005
              auth_token: secret-token
            "#,
        )
        .await;
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let facts = HostFacts::capture(start, &config, &environment_provider(&config).await, None)
            .await
            .unwrap();
        assert_eq!(
            facts,
            HostFacts {
                process_start: start,
                hostname: "lowering-host".to_string(),
                standalone: true,
                host_tags: None,
                // The URL is completed to `https`, as the native resolution does.
                cluster_agent_endpoint: Some("https://cluster-agent.example.com:5005".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn the_cluster_agent_endpoint_needs_forwarding_enabled_and_a_token() {
        for yaml in [
            "cluster_agent: {enabled: false, url: https://ca.example.com, auth_token: t}",
            "cluster_agent: {enabled: true, url: https://ca.example.com}",
            // An explicit URL that does not resolve to `https` yields no endpoint rather than service discovery.
            "cluster_agent: {enabled: true, url: http://ca.example.com, auth_token: t}",
        ] {
            let config = configuration(yaml).await;
            let facts = HostFacts::capture(UNIX_EPOCH, &config, &environment_provider(&config).await, None)
                .await
                .unwrap();
            assert_eq!(facts.cluster_agent_endpoint, None, "{}", yaml);
        }
    }

    #[tokio::test]
    async fn host_tags_are_fetched_only_when_connected_with_an_expected_duration() {
        for (yaml, expected) in [
            ("", false),
            ("expected_tags_duration: 5m", false),
            ("data_plane: {standalone_mode: false}", false),
            ("data_plane: {standalone_mode: false}\nexpected_tags_duration: 5m", true),
        ] {
            assert_eq!(host_tags_enabled(&configuration(yaml).await), expected, "{:?}", yaml);
        }

        // Connected with a duration, but without a way to reach the Core Agent: the boot fails.
        let config = configuration("data_plane: {standalone_mode: false}\nexpected_tags_duration: 5m").await;
        let error = HostFacts::capture(UNIX_EPOCH, &config, &environment_provider(&config).await, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Remote Agent client configuration is required in connected mode."
        );
    }

    #[test]
    fn the_host_tags_deadline_is_the_process_start_plus_the_expected_duration() {
        let facts = |process_start| HostFacts {
            process_start,
            hostname: String::new(),
            standalone: false,
            host_tags: None,
            cluster_agent_endpoint: None,
        };
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let secs = Duration::from_secs;
        let cases = [
            (start, secs(300), secs(1_700_000_300)),
            // A wall clock reads the start to the nanosecond; the deadline rounds up to a whole second.
            (
                start + Duration::from_nanos(123_456_789),
                secs(300),
                secs(1_700_000_301),
            ),
            (start + Duration::from_nanos(1), secs(300), secs(1_700_000_301)),
            (start, Duration::from_millis(1_500), secs(1_700_000_002)),
            (
                start + Duration::from_millis(500),
                Duration::from_millis(500),
                secs(1_700_000_001),
            ),
            // A start before the epoch counts as the epoch.
            (UNIX_EPOCH - secs(5), secs(60), secs(60)),
            // A deadline beyond what a plan's integers hold is the largest one they do.
            (start, Duration::MAX, secs(9_007_199_254_740_991)),
        ];
        for (process_start, expected_tags_duration, expected) in cases {
            assert_eq!(
                facts(process_start).host_tags_deadline(expected_tags_duration),
                expected,
                "{:?} + {:?}",
                process_start,
                expected_tags_duration
            );
        }
    }
}
