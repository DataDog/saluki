//! Traces-domain translation: the trace encoder, the APM stats encoder/transform, the trace
//! sampler, and trace obfuscation.
//!
//! Mirrors the conversions the original `DatadogTraceConfiguration`, `DatadogApmStatsEncoderConfiguration`,
//! `ApmStatsTransformConfiguration`, `TraceSamplerConfiguration`, and `TraceObfuscationConfiguration`
//! constructors performed.
//!
//! # Obfuscation fan-out
//!
//! The original component embedded an `ObfuscationConfig` inside each `ApmConfig`. The native model
//! carries an `ObfuscationConfig` in several places that all derive from the same Datadog
//! `apm_config.obfuscation.*` keys: the canonical `traces.obfuscation.config`, plus the `apm_config`
//! embedded in the trace encoder, the APM stats transform, and the trace sampler. Each obfuscation
//! setter therefore fans out to every copy so they stay consistent, matching the original where one
//! parsed `ObfuscationConfig` flowed into all the APM-config consumers.

use saluki_component_config::traces::{DatadogTraceConfig, ObfuscationConfig, TraceSamplerConfig};

use crate::translate::Translator;

/// Returns mutable references to every native `ObfuscationConfig` derived from the Datadog
/// `apm_config.obfuscation.*` keys.
fn obfuscation_targets(t: &mut Translator) -> Vec<&mut ObfuscationConfig> {
    let native = t.native_mut();
    vec![
        &mut native.components.traces.obfuscation.config,
        &mut native.components.traces.encoder.apm_config.obfuscation,
        &mut native.components.traces.sampler.apm_config.obfuscation,
        &mut native.components.metrics.apm_stats_transform.apm_config.obfuscation,
    ]
}

/// Applies `f` to every native `ObfuscationConfig` copy.
fn for_each_obfuscation(t: &mut Translator, f: impl Fn(&mut ObfuscationConfig)) {
    for target in obfuscation_targets(t) {
        f(target);
    }
}

// ----- trace encoder -----

/// `env` -> default trace environment (mirrored to the APM stats encoder and the APM configs).
pub fn set_env(t: &mut Translator, value: String) {
    let native = t.native_mut();
    native.components.traces.encoder.env = value.clone();
    native.components.metrics.apm_stats_encoder.env = value.clone();
    let meta = stringtheory::MetaString::from(value);
    native.components.traces.encoder.apm_config.default_env = meta.clone();
    native.components.traces.sampler.apm_config.default_env = meta.clone();
    native.components.metrics.apm_stats_transform.apm_config.default_env = meta;
}

/// `serializer_compressor_kind` -> trace encoder compression algorithm.
pub fn set_compressor_kind(config: &mut DatadogTraceConfig, value: String) {
    config.compressor_kind = value;
}

/// `serializer_zstd_compressor_level` -> trace encoder zstd compression level.
pub fn set_zstd_compressor_level(config: &mut DatadogTraceConfig, value: i64) {
    config.zstd_compressor_level = value as i32;
}

// The APM stats encoder carries no compression key; only `env` (above) flows into it.

// ----- trace sampler / OTLP sampling -----

/// `otlp_config.traces.probabilistic_sampler.sampling_percentage` -> the sampler's normalized OTLP
/// sampling rate.
///
/// Mirrors the original derivation: a valid percentage in `(0, 100]` is normalized to `[0, 1]` by
/// dividing by 100; invalid values (`<= 0` or `> 100`) are disregarded and the default (1.0) is
/// kept.
pub fn set_otlp_sampling_rate(sampler: &mut TraceSamplerConfig, percentage: f64) {
    if percentage > 0.0 && percentage <= 100.0 {
        sampler.otlp_sampling_rate = percentage / 100.0;
    } else {
        sampler.otlp_sampling_rate = 1.0;
    }
}

// ----- obfuscation -----

/// `apm_config.obfuscation.credit_cards.enabled`.
pub fn set_obfuscation_credit_cards_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.credit_cards.enabled = value);
}

/// `apm_config.obfuscation.credit_cards.luhn`.
pub fn set_obfuscation_credit_cards_luhn(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.credit_cards.luhn = value);
}

/// `apm_config.obfuscation.credit_cards.keep_values`.
pub fn set_obfuscation_credit_cards_keep_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.credit_cards.keep_values = value.clone());
}

/// `apm_config.obfuscation.http.remove_query_string`.
pub fn set_obfuscation_http_remove_query_string(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.http.remove_query_string = value);
}

/// `apm_config.obfuscation.http.remove_paths_with_digits`.
pub fn set_obfuscation_http_remove_paths_with_digits(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.http.remove_path_digits = value);
}

/// `apm_config.obfuscation.memcached.enabled`.
pub fn set_obfuscation_memcached_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.memcached.enabled = value);
}

/// `apm_config.obfuscation.memcached.keep_command`.
pub fn set_obfuscation_memcached_keep_command(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.memcached.keep_command = value);
}

/// `apm_config.obfuscation.redis.enabled`.
pub fn set_obfuscation_redis_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.redis.enabled = value);
}

/// `apm_config.obfuscation.redis.remove_all_args`.
pub fn set_obfuscation_redis_remove_all_args(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.redis.remove_all_args = value);
}

/// `apm_config.obfuscation.valkey.enabled`.
pub fn set_obfuscation_valkey_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.valkey.enabled = value);
}

/// `apm_config.obfuscation.valkey.remove_all_args`.
pub fn set_obfuscation_valkey_remove_all_args(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.valkey.remove_all_args = value);
}

/// `apm_config.obfuscation.mongodb.enabled`.
pub fn set_obfuscation_mongodb_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.mongo.enabled = value);
}

/// `apm_config.obfuscation.mongodb.keep_values`.
pub fn set_obfuscation_mongodb_keep_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.mongo.keep_values = value.clone());
}

/// `apm_config.obfuscation.mongodb.obfuscate_sql_values`.
pub fn set_obfuscation_mongodb_obfuscate_sql_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.mongo.obfuscate_sql_values = value.clone());
}

/// `apm_config.obfuscation.elasticsearch.enabled`.
pub fn set_obfuscation_elasticsearch_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.es.enabled = value);
}

/// `apm_config.obfuscation.elasticsearch.keep_values`.
pub fn set_obfuscation_elasticsearch_keep_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.es.keep_values = value.clone());
}

/// `apm_config.obfuscation.elasticsearch.obfuscate_sql_values`.
pub fn set_obfuscation_elasticsearch_obfuscate_sql_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.es.obfuscate_sql_values = value.clone());
}

/// `apm_config.obfuscation.opensearch.enabled`.
pub fn set_obfuscation_opensearch_enabled(t: &mut Translator, value: bool) {
    for_each_obfuscation(t, |o| o.open_search.enabled = value);
}

/// `apm_config.obfuscation.opensearch.keep_values`.
pub fn set_obfuscation_opensearch_keep_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.open_search.keep_values = value.clone());
}

/// `apm_config.obfuscation.opensearch.obfuscate_sql_values`.
pub fn set_obfuscation_opensearch_obfuscate_sql_values(t: &mut Translator, value: Vec<String>) {
    for_each_obfuscation(t, |o| o.open_search.obfuscate_sql_values = value.clone());
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::{SalukiConfiguration, SalukiOnlyConfiguration};

    use super::*;

    #[test]
    fn otlp_sampling_rate_normalizes_and_clamps() {
        let mut s = TraceSamplerConfig::default();
        set_otlp_sampling_rate(&mut s, 50.0);
        assert_eq!(s.otlp_sampling_rate, 0.5);
        set_otlp_sampling_rate(&mut s, 0.0);
        assert_eq!(s.otlp_sampling_rate, 1.0);
        set_otlp_sampling_rate(&mut s, 150.0);
        assert_eq!(s.otlp_sampling_rate, 1.0);
    }

    #[test]
    fn obfuscation_fans_out_to_all_copies() {
        let mut t = Translator::new(SalukiOnlyConfiguration::default().seed());
        set_obfuscation_credit_cards_enabled(&mut t, true);
        set_env(&mut t, "prod".to_string());
        let native: SalukiConfiguration = t.finish();
        assert!(native.components.traces.obfuscation.config.credit_cards.enabled);
        assert!(
            native
                .components
                .traces
                .encoder
                .apm_config
                .obfuscation
                .credit_cards
                .enabled
        );
        assert!(
            native
                .components
                .traces
                .sampler
                .apm_config
                .obfuscation
                .credit_cards
                .enabled
        );
        assert!(
            native
                .components
                .metrics
                .apm_stats_transform
                .apm_config
                .obfuscation
                .credit_cards
                .enabled
        );
        assert_eq!(native.components.traces.encoder.env, "prod");
        assert_eq!(native.components.metrics.apm_stats_encoder.env, "prod");
    }
}
