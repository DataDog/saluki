//! Inputs of the APM relay.

use std::sync::Arc;

use agent_data_plane_config::domains::traces;
use agent_data_plane_config::shared::Endpoints;
use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_apm::semantics::SemanticRegistryProvider;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_common::sync::live::Live;
use saluki_components::config::AgentEnv;
use saluki_components::relays::apm::{
    ApmRelayConfiguration, InfoCacheObfuscation, InfoConfiguration, InfoJsonObfuscation, InfoObfuscation,
    InfoReplaceRule, InfoTagFilter, InfoTagFilters, TracerFlareConfiguration,
};
use saluki_env::workload::CaptureEntityResolver;
use saluki_env::WorkloadProvider;

use super::forwarder_configuration;

/// The Datadog site when none is configured.
const DEFAULT_SITE: &str = "datadoghq.com";

/// Builds the relay inputs with the effective listen host, the tracer container resolver, the API key tracer flares
/// are sent with, and what `/info` reports.
pub(crate) fn apm_relay_configuration(
    config: &SalukiConfiguration, containerized: bool,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>, api_key: Live<String>,
    agent_client: Option<RemoteAgentClientConfiguration>, semantic_registry: SemanticRegistryProvider,
) -> ApmRelayConfiguration {
    let apm = &config.domains.apm;
    let shared = &config.shared;
    let (bind_host, non_local_traffic) = apm.listen_host(&shared.bind_host, containerized);

    ApmRelayConfiguration {
        receiver_socket: apm.receiver_socket.clone().unwrap_or_default(),
        proxy_destination: apm.proxy_destination.clone(),
        capture_entity_resolver,
        workload_provider,
        bind_host,
        non_local_traffic,
        receiver_port: apm.receiver_port,
        tracer_flare: TracerFlareConfiguration {
            site: shared.endpoints.site.value.clone(),
            api_key,
            forwarder: forwarder_configuration(shared),
        },
        info: info_configuration(config, containerized),
        agent_client,
        semantic_registry,
        validate_endpoint: validate_endpoint(&shared.endpoints),
    }
}

/// Returns the URL of `/api/v2/validate`, as the trace-agent resolves it: under an explicit `dd_url`, or under the
/// `api.` host of the site.
///
/// As in the trace-agent, the path is appended to `dd_url` as is, and `convert_dd_site_fqdn` doesn't apply.
fn validate_endpoint(endpoints: &Endpoints) -> String {
    let base = if endpoints.dd_url.is_explicit() && !endpoints.dd_url.value.is_empty() {
        endpoints.dd_url.value.clone()
    } else {
        let site = match endpoints.site.value.trim() {
            "" => DEFAULT_SITE,
            site => site,
        };
        format!("https://api.{site}")
    };
    format!("{base}/api/v2/validate")
}

/// Builds the trace-agent settings `/info` reports, as the trace-agent resolves them.
///
/// `remote_configuration.enabled` decides whether `/v0.7/config` is listed. Unlike the trace-agent, which disables
/// Remote Configuration on GovCloud sites unless it is set explicitly, this follows the setting as ADP does for its own
/// subscriptions.
fn info_configuration(config: &SalukiConfiguration, containerized: bool) -> InfoConfiguration {
    let apm = &config.domains.apm;
    let traces = &config.domains.traces;
    let default_env = AgentEnv::resolve(
        traces.apm_env.as_deref(),
        traces.env.as_deref(),
        &config.shared.tags.tags,
        &config.shared.tags.extra_tags,
    );

    InfoConfiguration {
        default_env: default_env.as_str().to_string(),
        target_traces_per_second: traces.target_traces_per_second,
        max_events_per_second: traces.max_events_per_second,
        receiver_port: apm.receiver_port,
        receiver_socket: apm.receiver_socket.clone().unwrap_or_default(),
        connection_limit: apm.connection_limit,
        receiver_timeout: apm.receiver_timeout,
        max_request_bytes: apm.max_payload_size,
        statsd_port: config.domains.dogstatsd.listeners.port,
        max_memory: apm.effective_max_memory(containerized),
        max_cpu: apm.effective_max_cpu(containerized),
        analyzed_spans_by_service: traces.analyzed_spans_by_service.clone(),
        features: traces
            .features
            .iter()
            .map(|feature| feature.as_str().to_string())
            .collect(),
        probabilistic_sampler_enabled: traces.probabilistic_sampler.enabled,
        compute_stats_by_span_kind: traces.compute_stats_by_span_kind,
        peer_tags_aggregation: traces.peer_tags_aggregation,
        custom_peer_tags: traces.peer_tags.clone(),
        telemetry_enabled: apm.telemetry_enabled,
        remote_configuration_enabled: config.domains.remote_configuration.enabled,
        obfuscation: info_obfuscation(&traces.obfuscation),
        replace_tags: traces
            .replace_tags
            .iter()
            .map(|rule| InfoReplaceRule {
                name: rule.name.clone(),
                pattern: rule.pattern.clone(),
                repl: rule.repl.clone(),
            })
            .collect(),
        filter_tags: info_tag_filters(&traces.filter_tags),
        filter_tags_regex: info_tag_filters(&traces.filter_tags_regex),
        ignore_resources: traces.ignore_resources.clone(),
    }
}

fn info_obfuscation(obfuscation: &traces::Obfuscation) -> InfoObfuscation {
    let json = |json: &traces::JsonQueryObfuscation| InfoJsonObfuscation {
        enabled: json.enabled,
        keep_values: json.keep_values.clone(),
    };
    let cache = |cache: &traces::CacheObfuscation| InfoCacheObfuscation {
        enabled: cache.enabled,
        remove_all_args: cache.remove_all_args,
    };
    InfoObfuscation {
        elasticsearch: json(&obfuscation.elasticsearch),
        opensearch: json(&obfuscation.opensearch),
        mongodb: json(&obfuscation.mongodb),
        sql_exec_plan: obfuscation.sql_exec_plan_enabled,
        sql_exec_plan_normalize: obfuscation.sql_exec_plan_normalize_enabled,
        sql_obfuscation_mode: obfuscation.sql_obfuscation_mode.as_str().to_string(),
        http_remove_query_string: obfuscation.http.remove_query_string,
        http_remove_paths_with_digits: obfuscation.http.remove_paths_with_digits,
        remove_stack_traces: obfuscation.remove_stack_traces,
        redis: cache(&obfuscation.redis),
        valkey: cache(&obfuscation.valkey),
        memcached_enabled: obfuscation.memcached.enabled,
        memcached_keep_command: obfuscation.memcached.keep_command,
        credit_cards_enabled: obfuscation.credit_cards.enabled,
        credit_cards_luhn: obfuscation.credit_cards.luhn,
        credit_cards_keep_values: obfuscation.credit_cards.keep_values.clone(),
    }
}

fn info_tag_filters(filters: &traces::TagFilters) -> InfoTagFilters {
    let rules = |rules: &[traces::TagFilter]| {
        rules
            .iter()
            .map(|rule| InfoTagFilter {
                key: rule.key.clone(),
                value: rule.value.clone(),
            })
            .collect()
    };
    InfoTagFilters {
        require: rules(&filters.require),
        reject: rules(&filters.reject),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_data_plane_config::domains::traces::{ApmFeature, ReplaceRule, SqlObfuscationMode, TagFilter};
    use agent_data_plane_config::ConfigValue;
    use saluki_env::workload::EntityId;

    use super::*;

    struct NoContainerResolver;

    impl CaptureEntityResolver for NoContainerResolver {
        fn resolve_container_entity_for_live_pid(&self, _process_id: u32) -> Option<EntityId> {
            None
        }
    }

    fn relay_configuration(config: &SalukiConfiguration, containerized: bool) -> ApmRelayConfiguration {
        apm_relay_configuration(
            config,
            containerized,
            None,
            None,
            Live::new_fixed(String::new()),
            None,
            SemanticRegistryProvider::default(),
        )
    }

    #[test]
    fn apm_relay_configuration_comes_from_resolved_configuration() {
        let mut config = SalukiConfiguration::default();
        config.domains.apm.receiver_port = 9126;
        config.domains.apm.receiver_socket = Some("/tmp/receiver.sock".to_string());
        config.domains.apm.proxy_destination = "unix:///tmp/destination.sock".to_string();
        config.domains.apm.non_local_traffic = ConfigValue::explicit(true);
        config.shared.bind_host = ConfigValue::explicit(Some("192.0.2.1".to_string()));
        config.shared.endpoints.site = ConfigValue::explicit("datadoghq.eu".to_string());
        let resolver: Arc<dyn CaptureEntityResolver + Send + Sync> = Arc::new(NoContainerResolver);
        let relay = apm_relay_configuration(
            &config,
            false,
            Some(resolver.clone()),
            None,
            Live::new_fixed("api-key".to_string()),
            None,
            SemanticRegistryProvider::default(),
        );

        assert_eq!(relay.receiver_port, 9126);
        assert_eq!(relay.receiver_socket, "/tmp/receiver.sock");
        assert_eq!(relay.proxy_destination, "unix:///tmp/destination.sock");
        assert_eq!(relay.bind_host.as_deref(), Some("192.0.2.1"));
        assert!(relay.non_local_traffic);
        assert!(Arc::ptr_eq(relay.capture_entity_resolver.as_ref().unwrap(), &resolver));
        assert_eq!(relay.tracer_flare.site, "datadoghq.eu");
        assert_eq!(relay.tracer_flare.api_key.as_str(), "api-key");
        assert!(relay.agent_client.is_none());
    }

    #[test]
    fn apm_relay_configuration_resolves_container_fallback_and_disabled_socket() {
        let mut config = SalukiConfiguration::default();
        config.shared.bind_host = ConfigValue::defaulted(Some("localhost".to_string()));
        for containerized in [false, true] {
            let relay = relay_configuration(&config, containerized);
            assert_eq!(relay.non_local_traffic, containerized);
            assert_eq!(relay.bind_host, None);
            assert!(relay.receiver_socket.is_empty());
            assert!(relay.capture_entity_resolver.is_none());
        }
    }

    #[test]
    fn info_configuration_comes_from_resolved_configuration() {
        let mut config = SalukiConfiguration::default();
        let apm = &mut config.domains.apm;
        apm.receiver_port = 9126;
        apm.receiver_socket = Some("/tmp/receiver.sock".to_string());
        apm.connection_limit = 11;
        apm.receiver_timeout = 12;
        apm.max_payload_size = 13;
        apm.max_memory = ConfigValue::explicit(14.0);
        apm.max_cpu_percent = ConfigValue::explicit(150.0);
        apm.telemetry_enabled = true;
        config.domains.dogstatsd.listeners.port = 9125;
        config.domains.remote_configuration.enabled = true;
        let traces = &mut config.domains.traces;
        traces.apm_env = Some("Staging".to_string());
        traces.target_traces_per_second = 15.0;
        traces.max_events_per_second = 16.0;
        traces.analyzed_spans_by_service =
            HashMap::from([("web".to_string(), HashMap::from([("request".to_string(), 0.5)]))]);
        traces.features = vec![ApmFeature::Other("sqllexer".to_string())];
        traces.probabilistic_sampler.enabled = true;
        traces.compute_stats_by_span_kind = true;
        traces.peer_tags_aggregation = true;
        traces.peer_tags = vec!["custom.peer".to_string()];
        traces.replace_tags = vec![ReplaceRule {
            name: "*".to_string(),
            pattern: "secret".to_string(),
            repl: "?".to_string(),
        }];
        traces.filter_tags.require = vec![TagFilter {
            key: "env".to_string(),
            value: Some("prod".to_string()),
        }];
        traces.filter_tags_regex.reject = vec![TagFilter {
            key: "debug".to_string(),
            value: None,
        }];
        traces.ignore_resources = vec!["GET /ping".to_string()];
        let obfuscation = &mut traces.obfuscation;
        obfuscation.elasticsearch.enabled = true;
        obfuscation.elasticsearch.keep_values = vec!["es".to_string()];
        obfuscation.opensearch.keep_values = vec!["os".to_string()];
        obfuscation.mongodb.enabled = true;
        obfuscation.sql_exec_plan_enabled = true;
        obfuscation.sql_obfuscation_mode = SqlObfuscationMode::NormalizeOnly;
        obfuscation.http.remove_query_string = true;
        obfuscation.remove_stack_traces = true;
        obfuscation.valkey.remove_all_args = true;
        obfuscation.memcached.keep_command = true;
        obfuscation.credit_cards.luhn = true;
        obfuscation.credit_cards.keep_values = vec!["card".to_string()];

        let info = relay_configuration(&config, false).info;

        // The environment is normalized as a tag value.
        assert_eq!(info.default_env, "staging");
        assert_eq!(info.target_traces_per_second, 15.0);
        assert_eq!(info.max_events_per_second, 16.0);
        assert_eq!(info.receiver_port, 9126);
        assert_eq!(info.receiver_socket, "/tmp/receiver.sock");
        assert_eq!(info.connection_limit, 11);
        assert_eq!(info.receiver_timeout, 12);
        assert_eq!(info.max_request_bytes, 13);
        assert_eq!(info.statsd_port, 9125);
        assert_eq!(info.max_memory, 14.0);
        assert_eq!(info.max_cpu, 1.5);
        assert_eq!(info.analyzed_spans_by_service["web"]["request"], 0.5);
        assert_eq!(info.features, ["sqllexer"]);
        assert!(info.probabilistic_sampler_enabled);
        assert!(info.compute_stats_by_span_kind);
        assert!(info.peer_tags_aggregation);
        assert_eq!(info.custom_peer_tags, ["custom.peer"]);
        assert!(info.telemetry_enabled);
        assert!(info.remote_configuration_enabled);
        assert_eq!(
            (
                info.replace_tags[0].name.as_str(),
                info.replace_tags[0].pattern.as_str(),
                info.replace_tags[0].repl.as_str()
            ),
            ("*", "secret", "?")
        );
        assert_eq!(info.filter_tags.require[0].key, "env");
        assert_eq!(info.filter_tags.require[0].value.as_deref(), Some("prod"));
        assert!(info.filter_tags.reject.is_empty());
        assert!(info.filter_tags_regex.require.is_empty());
        assert_eq!(info.filter_tags_regex.reject[0].key, "debug");
        assert_eq!(info.ignore_resources, ["GET /ping"]);

        let obfuscation = &info.obfuscation;
        assert!(obfuscation.elasticsearch.enabled);
        assert_eq!(obfuscation.elasticsearch.keep_values, ["es"]);
        assert!(!obfuscation.opensearch.enabled);
        assert_eq!(obfuscation.opensearch.keep_values, ["os"]);
        assert!(obfuscation.mongodb.enabled);
        assert!(obfuscation.mongodb.keep_values.is_empty());
        assert!(obfuscation.sql_exec_plan);
        assert!(!obfuscation.sql_exec_plan_normalize);
        assert_eq!(obfuscation.sql_obfuscation_mode, "normalize_only");
        assert!(obfuscation.http_remove_query_string);
        assert!(!obfuscation.http_remove_paths_with_digits);
        assert!(obfuscation.remove_stack_traces);
        assert!(!obfuscation.redis.remove_all_args);
        assert!(obfuscation.valkey.remove_all_args);
        assert!(!obfuscation.memcached_enabled);
        assert!(obfuscation.memcached_keep_command);
        assert!(!obfuscation.credit_cards_enabled);
        assert!(obfuscation.credit_cards_luhn);
        assert_eq!(obfuscation.credit_cards_keep_values, ["card"]);
    }

    #[test]
    fn info_configuration_disables_unset_resource_limits_in_a_container() {
        let mut config = SalukiConfiguration::default();
        config.domains.apm.max_memory = ConfigValue::defaulted(5e8);
        config.domains.apm.max_cpu_percent = ConfigValue::defaulted(50.0);

        let info = relay_configuration(&config, false).info;
        assert_eq!((info.max_memory, info.max_cpu), (5e8, 0.5));
        let info = relay_configuration(&config, true).info;
        assert_eq!((info.max_memory, info.max_cpu), (0.0, 0.0));
    }

    #[test]
    fn the_validate_endpoint_follows_dd_url_then_the_site() {
        let mut endpoints = Endpoints {
            site: ConfigValue::defaulted("datadoghq.com".to_string()),
            dd_url: ConfigValue::defaulted("https://app.datadoghq.com".to_string()),
            ..Default::default()
        };
        assert_eq!(
            validate_endpoint(&endpoints),
            "https://api.datadoghq.com/api/v2/validate"
        );

        endpoints.site = ConfigValue::explicit(" datadoghq.eu ".to_string());
        assert_eq!(
            validate_endpoint(&endpoints),
            "https://api.datadoghq.eu/api/v2/validate"
        );

        endpoints.site = ConfigValue::explicit(String::new());
        assert_eq!(
            validate_endpoint(&endpoints),
            "https://api.datadoghq.com/api/v2/validate"
        );

        endpoints.dd_url = ConfigValue::explicit("https://proxy.example.com/".to_string());
        assert_eq!(
            validate_endpoint(&endpoints),
            "https://proxy.example.com//api/v2/validate"
        );
    }

    #[test]
    fn info_configuration_reports_none_without_an_environment() {
        let info = relay_configuration(&SalukiConfiguration::default(), false).info;
        assert_eq!(info.default_env, "none");
    }
}
