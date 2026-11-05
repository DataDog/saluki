//! Inputs of the trace pipeline: the trace and stats components, obfuscation, and the OTTL processors.

use agent_data_plane_config::{
    domains::{otlp, traces},
    shared::SharedConfiguration,
};
use datadog_agent_apm::{sampling::TraceSamplingSubscription, semantics::SemanticRegistryProvider};
use saluki_components::{
    config::AgentEnv,
    encoders::DatadogTraceConfiguration,
    transforms::{
        ApmStatsTransformConfiguration, CreditCardObfuscationConfig, EsObfuscationConfig, HttpObfuscationConfig,
        MemcachedObfuscationConfig, MongoObfuscationConfig, ObfuscationConfig, OpenSearchObfuscationConfig,
        RedisObfuscationConfig, SqlObfuscationConfig, TraceSamplerConfiguration, ValkeyObfuscationConfig,
    },
};
use stringtheory::MetaString;

use crate::components::ottl::ErrorMode;

/// Builds the trace encoder settings, without a default hostname.
pub(crate) fn trace_configuration(
    traces: &traces::Domain, otlp_traces: &otlp::Traces, shared: &SharedConfiguration, agent_env: AgentEnv,
) -> DatadogTraceConfiguration {
    let compression = &shared.endpoints.compression;

    DatadogTraceConfiguration {
        compressor_kind: compression.compressor_kind.clone(),
        zstd_compressor_level: compression.effective_zstd_level(),
        flush_timeout: shared.metrics_encoding.flush_timeout,
        agent_env,
        target_traces_per_second: traces.target_traces_per_second,
        errors_per_second: traces.errors_per_second,
        error_tracking_standalone: traces.error_tracking_standalone_enabled,
        ignore_missing_datadog_fields: otlp_traces.ignore_missing_datadog_fields,
        default_hostname: None,
    }
}

/// Builds the trace sampler settings.
pub(crate) fn trace_sampler_configuration(
    traces: &traces::Domain, otlp_traces: &otlp::Traces, agent_env: AgentEnv,
    remote_sampling: TraceSamplingSubscription,
) -> TraceSamplerConfiguration {
    TraceSamplerConfiguration {
        probabilistic_sampler_enabled: traces.probabilistic_sampler.enabled,
        probabilistic_hash_seed: traces.probabilistic_sampler.hash_seed,
        probabilistic_full_trace_id: traces
            .features
            .contains(&traces::ApmFeature::ProbabilisticSamplerFullTraceId),
        sampling_percentage: traces.probabilistic_sampler.sampling_percentage,
        error_sampling_enabled: traces.error_sampling_enabled,
        error_tracking_standalone: traces.error_tracking_standalone_enabled,
        errors_per_second: traces.errors_per_second,
        target_traces_per_second: traces.target_traces_per_second,
        extra_sample_rate: traces.extra_sample_rate,
        max_catalog_entries: traces.max_catalog_entries,
        agent_env,
        rare_sampler_enabled: traces.enable_rare_sampler,
        rare_sampler_tps: traces.rare_sampler.tps,
        rare_sampler_cooldown_secs: traces.rare_sampler.cooldown,
        rare_sampler_cardinality: traces.rare_sampler.cardinality,
        compute_top_level_by_span_kind: otlp_traces.enable_compute_top_level_by_span_kind,
        analyzed_spans_by_service: traces.analyzed_spans_by_service.clone(),
        analyzed_rate_by_service: traces.analyzed_rate_by_service.clone(),
        max_events_per_second: traces.max_events_per_second,
        remote_sampling,
    }
}

/// Builds the APM stats transform settings, without a default hostname or workload provider.
pub(crate) fn apm_stats_configuration(
    traces: &traces::Domain, agent_env: AgentEnv, semantic_registry: SemanticRegistryProvider,
) -> ApmStatsTransformConfiguration {
    ApmStatsTransformConfiguration {
        compute_stats_by_span_kind: traces.compute_stats_by_span_kind,
        peer_tags_aggregation: traces.peer_tags_aggregation,
        peer_tags: traces.peer_tags.iter().cloned().map(MetaString::from).collect(),
        agent_env,
        default_hostname: None,
        workload_provider: None,
        semantic_registry,
    }
}

/// Builds the obfuscator settings.
pub(crate) fn obfuscation_configuration(obfuscation: &traces::Obfuscation) -> ObfuscationConfig {
    ObfuscationConfig {
        credit_cards: CreditCardObfuscationConfig {
            enabled: obfuscation.credit_cards.enabled,
            luhn: obfuscation.credit_cards.luhn,
            keep_values: obfuscation.credit_cards.keep_values.clone(),
        },
        http: HttpObfuscationConfig {
            remove_query_string: obfuscation.http.remove_query_string,
            remove_paths_with_digits: obfuscation.http.remove_paths_with_digits,
        },
        memcached: MemcachedObfuscationConfig {
            enabled: obfuscation.memcached.enabled,
            keep_command: obfuscation.memcached.keep_command,
        },
        redis: RedisObfuscationConfig {
            enabled: obfuscation.redis.enabled,
            remove_all_args: obfuscation.redis.remove_all_args,
        },
        valkey: ValkeyObfuscationConfig {
            enabled: obfuscation.valkey.enabled,
            remove_all_args: obfuscation.valkey.remove_all_args,
        },
        sql: SqlObfuscationConfig {
            dbms: obfuscation.sql.dbms.clone(),
            table_names: obfuscation.sql.table_names,
            replace_digits: obfuscation.sql.replace_digits,
            keep_sql_alias: obfuscation.sql.keep_sql_alias,
            dollar_quoted_func: obfuscation.sql.dollar_quoted_func,
        },
        mongodb: MongoObfuscationConfig {
            enabled: obfuscation.mongodb.enabled,
            keep_values: obfuscation.mongodb.keep_values.clone(),
            obfuscate_sql_values: obfuscation.mongodb.obfuscate_sql_values.clone(),
        },
        elasticsearch: EsObfuscationConfig {
            enabled: obfuscation.elasticsearch.enabled,
            keep_values: obfuscation.elasticsearch.keep_values.clone(),
            obfuscate_sql_values: obfuscation.elasticsearch.obfuscate_sql_values.clone(),
        },
        opensearch: OpenSearchObfuscationConfig {
            enabled: obfuscation.opensearch.enabled,
            keep_values: obfuscation.opensearch.keep_values.clone(),
            obfuscate_sql_values: obfuscation.opensearch.obfuscate_sql_values.clone(),
        },
    }
}

/// Maps the configured OTTL error mode onto the processors' error mode.
pub(crate) fn ottl_error_mode(mode: traces::OttlErrorMode) -> ErrorMode {
    match mode {
        traces::OttlErrorMode::Ignore => ErrorMode::Ignore,
        traces::OttlErrorMode::Silent => ErrorMode::Silent,
        traces::OttlErrorMode::Propagate => ErrorMode::Propagate,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_data_plane_config::{
        shared::{Compression, Endpoints, MetricsEncoding},
        ConfigValue,
    };

    use super::*;

    fn agent_env() -> AgentEnv {
        AgentEnv::resolve(None, Some("prod"), &[], &[])
    }

    /// Returns a traces domain in which every value the adapters copy is distinct from its default and from the other
    /// values of its type.
    fn distinct_traces() -> traces::Domain {
        traces::Domain {
            compute_stats_by_span_kind: true,
            peer_tags: vec!["peer.tag".to_string()],
            peer_tags_aggregation: false,
            error_sampling_enabled: false,
            error_tracking_standalone_enabled: true,
            errors_per_second: 1.5,
            target_traces_per_second: 2.5,
            extra_sample_rate: 3.5,
            analyzed_rate_by_service: HashMap::from([("rate-service".to_string(), 0.25)]),
            analyzed_spans_by_service: HashMap::from([(
                "span-service".to_string(),
                HashMap::from([("operation".to_string(), 0.75)]),
            )]),
            max_events_per_second: 4.5,
            max_catalog_entries: 11,
            features: vec![traces::ApmFeature::ProbabilisticSamplerFullTraceId],
            enable_rare_sampler: true,
            rare_sampler: traces::RareSampler {
                cardinality: 12,
                cooldown: 5.5,
                tps: 6.5,
            },
            probabilistic_sampler: traces::ProbabilisticSampler {
                enabled: true,
                hash_seed: 13,
                sampling_percentage: 7.5,
            },
            ..Default::default()
        }
    }

    fn distinct_otlp_traces() -> otlp::Traces {
        otlp::Traces {
            enable_compute_top_level_by_span_kind: false,
            ignore_missing_datadog_fields: true,
            ..Default::default()
        }
    }

    #[test]
    fn trace_configuration_comes_from_resolved_configuration() {
        let shared = SharedConfiguration {
            endpoints: Endpoints {
                compression: Compression {
                    compressor_kind: "gzip".to_string(),
                    adp_zstd_level: ConfigValue::explicit(5),
                    agent_zstd_level: ConfigValue::explicit(9),
                },
                ..Default::default()
            },
            metrics_encoding: MetricsEncoding {
                flush_timeout: std::time::Duration::from_millis(21),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = trace_configuration(&distinct_traces(), &distinct_otlp_traces(), &shared, agent_env());

        assert_eq!("gzip", config.compressor_kind);
        // ADP's own level wins over the Agent's when both are explicit.
        assert_eq!(5, config.zstd_compressor_level);
        assert_eq!(std::time::Duration::from_millis(21), config.flush_timeout);
        assert_eq!("prod", config.agent_env.as_str());
        assert_eq!(2.5, config.target_traces_per_second);
        assert_eq!(1.5, config.errors_per_second);
        assert!(config.error_tracking_standalone);
        assert!(config.ignore_missing_datadog_fields);
        assert_eq!(None, config.default_hostname);
    }

    #[test]
    fn trace_sampler_configuration_comes_from_resolved_configuration() {
        let config = trace_sampler_configuration(
            &distinct_traces(),
            &distinct_otlp_traces(),
            agent_env(),
            TraceSamplingSubscription::default(),
        );

        assert!(config.probabilistic_sampler_enabled);
        assert_eq!(13, config.probabilistic_hash_seed);
        assert!(config.probabilistic_full_trace_id);
        assert_eq!(7.5, config.sampling_percentage);
        assert!(!config.error_sampling_enabled);
        assert!(config.error_tracking_standalone);
        assert_eq!(1.5, config.errors_per_second);
        assert_eq!(2.5, config.target_traces_per_second);
        assert_eq!(3.5, config.extra_sample_rate);
        assert_eq!(11, config.max_catalog_entries);
        assert_eq!("prod", config.agent_env.as_str());
        assert!(config.rare_sampler_enabled);
        assert_eq!(6.5, config.rare_sampler_tps);
        assert_eq!(5.5, config.rare_sampler_cooldown_secs);
        assert_eq!(12, config.rare_sampler_cardinality);
        assert!(!config.compute_top_level_by_span_kind);
        assert_eq!(
            HashMap::from([(
                "span-service".to_string(),
                HashMap::from([("operation".to_string(), 0.75)]),
            )]),
            config.analyzed_spans_by_service
        );
        assert_eq!(
            HashMap::from([("rate-service".to_string(), 0.25)]),
            config.analyzed_rate_by_service
        );
        assert_eq!(4.5, config.max_events_per_second);
    }

    #[test]
    fn only_the_full_trace_id_feature_enables_full_trace_id_sampling() {
        // Unrecognized flags are carried, not rejected, and enable nothing.
        let traces = traces::Domain {
            features: vec![traces::ApmFeature::Other("table_names".to_owned())],
            ..Default::default()
        };
        let config = trace_sampler_configuration(
            &traces,
            &otlp::Traces::default(),
            agent_env(),
            TraceSamplingSubscription::default(),
        );

        assert!(!config.probabilistic_full_trace_id);
    }

    #[test]
    fn apm_stats_configuration_comes_from_resolved_configuration() {
        let config = apm_stats_configuration(&distinct_traces(), agent_env(), SemanticRegistryProvider::default());

        assert!(config.compute_stats_by_span_kind);
        assert!(!config.peer_tags_aggregation);
        assert_eq!(vec![MetaString::from("peer.tag")], config.peer_tags);
        assert_eq!("prod", config.agent_env.as_str());
        assert_eq!(None, config.default_hostname);
        assert!(config.workload_provider.is_none());
    }

    #[test]
    fn obfuscation_configuration_comes_from_resolved_configuration() {
        let json_query = |name: &str| traces::JsonQueryObfuscation {
            enabled: true,
            keep_values: vec![format!("{name}.keep")],
            obfuscate_sql_values: vec![format!("{name}.sql")],
        };
        let obfuscation = traces::Obfuscation {
            credit_cards: traces::CreditCardObfuscation {
                enabled: true,
                keep_values: vec!["card.keep".to_string()],
                luhn: false,
            },
            elasticsearch: json_query("elasticsearch"),
            http: traces::HttpObfuscation {
                remove_paths_with_digits: true,
                remove_query_string: false,
            },
            memcached: traces::MemcachedObfuscation {
                enabled: false,
                keep_command: true,
            },
            mongodb: json_query("mongodb"),
            opensearch: json_query("opensearch"),
            redis: traces::CacheObfuscation {
                enabled: true,
                remove_all_args: false,
            },
            valkey: traces::CacheObfuscation {
                enabled: false,
                remove_all_args: true,
            },
            sql: traces::SqlObfuscation {
                dbms: "postgresql".to_string(),
                dollar_quoted_func: true,
                keep_sql_alias: false,
                replace_digits: true,
                table_names: false,
            },
        };
        let config = obfuscation_configuration(&obfuscation);

        assert!(config.credit_cards.enabled);
        assert_eq!(vec!["card.keep".to_string()], config.credit_cards.keep_values);
        assert!(!config.credit_cards.luhn);
        assert!(config.http.remove_paths_with_digits);
        assert!(!config.http.remove_query_string);
        assert!(!config.memcached.enabled);
        assert!(config.memcached.keep_command);
        assert!(config.redis.enabled);
        assert!(!config.redis.remove_all_args);
        assert!(!config.valkey.enabled);
        assert!(config.valkey.remove_all_args);
        assert_eq!("postgresql", config.sql.dbms);
        assert!(config.sql.dollar_quoted_func);
        assert!(!config.sql.keep_sql_alias);
        assert!(config.sql.replace_digits);
        assert!(!config.sql.table_names);
        for (name, enabled, keep_values, obfuscate_sql_values) in [
            (
                "mongodb",
                config.mongodb.enabled,
                &config.mongodb.keep_values,
                &config.mongodb.obfuscate_sql_values,
            ),
            (
                "elasticsearch",
                config.elasticsearch.enabled,
                &config.elasticsearch.keep_values,
                &config.elasticsearch.obfuscate_sql_values,
            ),
            (
                "opensearch",
                config.opensearch.enabled,
                &config.opensearch.keep_values,
                &config.opensearch.obfuscate_sql_values,
            ),
        ] {
            assert!(enabled, "{name}");
            assert_eq!(&vec![format!("{name}.keep")], keep_values, "{name}");
            assert_eq!(&vec![format!("{name}.sql")], obfuscate_sql_values, "{name}");
        }
    }

    #[test]
    fn ottl_error_mode_maps_every_variant() {
        for (configured, expected) in [
            (traces::OttlErrorMode::Ignore, ErrorMode::Ignore),
            (traces::OttlErrorMode::Silent, ErrorMode::Silent),
            (traces::OttlErrorMode::Propagate, ErrorMode::Propagate),
        ] {
            assert_eq!(expected, ottl_error_mode(configured), "{configured:?}");
        }
    }
}
