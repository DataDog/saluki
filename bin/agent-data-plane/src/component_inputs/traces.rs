//! Inputs of the trace pipeline: the trace and stats components, obfuscation, and the OTTL processors.

use agent_data_plane_config::{
    domains::{otlp, traces},
    shared::SharedConfiguration,
};
use datadog_agent_apm::{sampling::TraceSamplingSubscription, semantics::SemanticRegistryProvider};
use libdd_trace_obfuscation::{
    json::JsonObfuscator,
    obfuscation_config::{
        CreditCardConfig, HttpConfig, JsonObfuscatorConfig, MemcachedConfig, ObfuscationConfig, RedisConfig, SqlConfig,
        SqlObfuscationMode,
    },
};
use saluki_components::{
    config::AgentEnv,
    encoders::DatadogTraceConfiguration,
    transforms::{
        ApmStatsTransformConfiguration, ReplaceRule, TraceSamplerConfiguration, TraceTagReplacerConfiguration,
    },
};
use stringtheory::MetaString;
use tracing::warn;

use crate::components::ottl::ErrorMode;

/// Builds the trace encoder settings, without a default hostname or workload provider.
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
        workload_provider: None,
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

/// Builds the trace tag replacer settings: the configured `replace_tags` rules, then the stack trace rules when
/// `apm_config.obfuscation.remove_stack_traces` is set.
pub(crate) fn trace_tag_replacer_configuration(traces: &traces::Domain) -> TraceTagReplacerConfiguration {
    let rules = traces
        .replace_tags
        .iter()
        .map(|rule| ReplaceRule {
            name: rule.name.clone(),
            pattern: rule.pattern.clone(),
            repl: rule.repl.clone(),
        })
        .collect();
    TraceTagReplacerConfiguration::new(rules, traces.obfuscation.remove_stack_traces)
}

/// Builds the obfuscation configuration from the resolved trace settings.
///
/// The SQL options come from the `apm_config.obfuscation.sql` settings and from the beta feature flags in
/// `apm_config.features`. When no SQL obfuscation mode is configured, the `sqllexer` feature selects `obfuscate_only`.
///
/// Tag replacement rules are left out: the tag replacer transform owns them.
pub(crate) fn obfuscation_configuration(traces: &traces::Domain) -> ObfuscationConfig {
    let obfuscation = &traces.obfuscation;
    let has_feature = |feature: traces::ApmFeature| traces.features.contains(&feature);

    let sql = SqlConfig {
        replace_digits: obfuscation.sql.replace_digits
            || has_feature(traces::ApmFeature::QuantizeSqlTables)
            || has_feature(traces::ApmFeature::ReplaceSqlDigits),
        keep_sql_alias: obfuscation.sql.keep_sql_alias || has_feature(traces::ApmFeature::KeepSqlAlias),
        dollar_quoted_func: obfuscation.sql.dollar_quoted_func || has_feature(traces::ApmFeature::DollarQuotedFunc),
        obfuscation_mode: sql_obfuscation_mode(
            obfuscation.sql_obfuscation_mode,
            has_feature(traces::ApmFeature::Sqllexer),
        ),
        ..SqlConfig::default()
    };

    if obfuscation.sql.table_names {
        warn!(
            config_key = "apm_config.obfuscation.sql.table_names",
            "Ignoring the setting: SQL table names are not collected, so the `sql.tables` tag is not set."
        );
    }
    if has_feature(traces::ApmFeature::TableNames) {
        warn!(
            config_key = "apm_config.features",
            "Ignoring the `table_names` feature: SQL table names are not collected, so the `sql.tables` tag is not set."
        );
    }

    let json = |query: &traces::JsonQueryObfuscation| {
        JsonObfuscator::new(JsonObfuscatorConfig {
            enabled: query.enabled,
            keep_keys: query.keep_values.iter().cloned().collect(),
            transform_keys: query.obfuscate_sql_values.iter().cloned().collect(),
        })
    };

    ObfuscationConfig {
        tag_replace_rules: Vec::new(),
        http: HttpConfig {
            remove_query_string: obfuscation.http.remove_query_string,
            remove_path_digits: obfuscation.http.remove_paths_with_digits,
        },
        memcached: MemcachedConfig {
            enabled: obfuscation.memcached.enabled,
            keep_command: obfuscation.memcached.keep_command,
        },
        redis: RedisConfig {
            enabled: obfuscation.redis.enabled,
            remove_all_args: obfuscation.redis.remove_all_args,
        },
        valkey: RedisConfig {
            enabled: obfuscation.valkey.enabled,
            remove_all_args: obfuscation.valkey.remove_all_args,
        },
        credit_cards: CreditCardConfig {
            enabled: obfuscation.credit_cards.enabled,
            luhn: obfuscation.credit_cards.luhn,
            keep_values: obfuscation.credit_cards.keep_values.iter().cloned().collect(),
        },
        sql,
        elasticsearch: json(&obfuscation.elasticsearch),
        opensearch: json(&obfuscation.opensearch),
        mongodb: json(&obfuscation.mongodb),
    }
}

/// Selects the SQL obfuscation mode: the configured one, else `obfuscate_only` when the `sqllexer` feature is on.
fn sql_obfuscation_mode(configured: traces::SqlObfuscationMode, sqllexer: bool) -> SqlObfuscationMode {
    #[allow(deprecated)]
    match configured {
        traces::SqlObfuscationMode::ObfuscateOnly => SqlObfuscationMode::ObfuscateOnly,
        traces::SqlObfuscationMode::NormalizeOnly => SqlObfuscationMode::NormalizeOnly,
        traces::SqlObfuscationMode::ObfuscateAndNormalize => SqlObfuscationMode::ObfuscateAndNormalize,
        traces::SqlObfuscationMode::Default if sqllexer => SqlObfuscationMode::ObfuscateOnly,
        traces::SqlObfuscationMode::Default | traces::SqlObfuscationMode::Legacy => SqlObfuscationMode::Unspecified,
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
        assert!(config.workload_provider.is_none());
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
        // Other flags are carried, not rejected, and enable nothing here.
        let traces = traces::Domain {
            features: vec![traces::ApmFeature::TableNames],
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

    fn json_query(name: &str) -> traces::JsonQueryObfuscation {
        traces::JsonQueryObfuscation {
            enabled: true,
            keep_values: vec![format!("{name}.keep")],
            obfuscate_sql_values: vec![format!("{name}.sql")],
        }
    }

    fn domain_with(obfuscation: traces::Obfuscation, features: &[traces::ApmFeature]) -> traces::Domain {
        traces::Domain {
            obfuscation,
            features: features.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn obfuscation_configuration_comes_from_resolved_configuration() {
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
            remove_stack_traces: true,
            valkey: traces::CacheObfuscation {
                enabled: false,
                remove_all_args: true,
            },
            sql: traces::SqlObfuscation {
                dbms: String::new(),
                dollar_quoted_func: true,
                keep_sql_alias: false,
                replace_digits: true,
                table_names: false,
            },
            sql_obfuscation_mode: traces::SqlObfuscationMode::NormalizeOnly,
            ..Default::default()
        };
        let config = obfuscation_configuration(&domain_with(obfuscation, &[]));

        assert!(config.credit_cards.enabled);
        assert!(config.credit_cards.keep_values.contains("card.keep"));
        assert!(!config.credit_cards.luhn);
        assert!(config.http.remove_path_digits);
        assert!(!config.http.remove_query_string);
        assert!(!config.memcached.enabled);
        assert!(config.memcached.keep_command);
        assert!(config.redis.enabled);
        assert!(!config.redis.remove_all_args);
        assert!(!config.valkey.enabled);
        assert!(config.valkey.remove_all_args);
        assert!(config.sql.dollar_quoted_func);
        assert!(!config.sql.keep_sql_alias);
        assert!(config.sql.replace_digits);
        assert_eq!(SqlObfuscationMode::NormalizeOnly, config.sql.obfuscation_mode);
        assert!(
            config.tag_replace_rules.is_empty(),
            "the tag replacer transform owns the rules"
        );
        for (name, json) in [
            ("mongodb", &config.mongodb),
            ("elasticsearch", &config.elasticsearch),
            ("opensearch", &config.opensearch),
        ] {
            assert!(json.config().enabled, "{name}");
            assert!(json.config().keep_keys.contains(&format!("{name}.keep")), "{name}");
            assert!(json.config().transform_keys.contains(&format!("{name}.sql")), "{name}");
        }
    }

    // The SQL options can come from feature flags as well as from the obfuscation settings.
    #[test]
    fn sql_options_follow_the_feature_flags() {
        let config = obfuscation_configuration(&domain_with(
            traces::Obfuscation::default(),
            &[
                traces::ApmFeature::QuantizeSqlTables,
                traces::ApmFeature::KeepSqlAlias,
                traces::ApmFeature::DollarQuotedFunc,
            ],
        ));
        assert!(config.sql.replace_digits);
        assert!(config.sql.keep_sql_alias);
        assert!(config.sql.dollar_quoted_func);

        let config = obfuscation_configuration(&domain_with(
            traces::Obfuscation::default(),
            &[traces::ApmFeature::ReplaceSqlDigits],
        ));
        assert!(config.sql.replace_digits, "either flag turns on digit replacement");
        assert!(!config.sql.keep_sql_alias);

        let config = obfuscation_configuration(&domain_with(traces::Obfuscation::default(), &[]));
        assert!(!config.sql.replace_digits);
        assert!(!config.sql.keep_sql_alias);
        assert!(!config.sql.dollar_quoted_func);
    }

    #[test]
    fn sql_obfuscation_mode_prefers_the_configured_mode_over_the_sqllexer_feature() {
        let mode = |configured, sqllexer| {
            let obfuscation = traces::Obfuscation {
                sql_obfuscation_mode: configured,
                ..Default::default()
            };
            let features: &[traces::ApmFeature] = if sqllexer { &[traces::ApmFeature::Sqllexer] } else { &[] };
            obfuscation_configuration(&domain_with(obfuscation, features))
                .sql
                .obfuscation_mode
        };

        assert_eq!(
            SqlObfuscationMode::ObfuscateOnly,
            mode(traces::SqlObfuscationMode::Default, true)
        );
        assert_eq!(
            SqlObfuscationMode::ObfuscateAndNormalize,
            mode(traces::SqlObfuscationMode::ObfuscateAndNormalize, true)
        );
        assert_eq!(
            SqlObfuscationMode::NormalizeOnly,
            mode(traces::SqlObfuscationMode::NormalizeOnly, false)
        );
        #[allow(deprecated)]
        let unspecified = SqlObfuscationMode::Unspecified;
        assert_eq!(unspecified, mode(traces::SqlObfuscationMode::Default, false));
        assert_eq!(unspecified, mode(traces::SqlObfuscationMode::Legacy, true));
        assert_eq!(unspecified, mode(traces::SqlObfuscationMode::Legacy, false));
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
