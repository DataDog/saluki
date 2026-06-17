//! Component-native configuration for trace-handling components.
//!
//! Covers the Datadog traces encoder, the APM stats encoder, the APM stats transform, the trace
//! sampler, and the trace obfuscation transform. Mirrors the corresponding structs in
//! `saluki-components` with source key names, aliases, and `Deserialize` impls stripped.
//!
//! Injected/derived state is excluded: computed `version`/`agent_version` (derived from app
//! details), `hostname`/`default_hostname`/`agent_hostname` (derived from the environment provider),
//! and `workload_provider` handles.

use stringtheory::MetaString;

use crate::otlp::TracesConfig;

/// Configuration for the Datadog traces encoder component.
///
/// Mirrors `DatadogTraceConfiguration` in `saluki-components`. The injected `default_hostname` and
/// computed `version` fields are excluded; the embedded [`ApmConfig`] and OTLP [`TracesConfig`] are
/// retained as their resolved config-only mirrors.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogTraceConfig {
    /// The compression algorithm to use for outgoing payloads.
    ///
    /// Defaults to `zstd`.
    pub compressor_kind: String,

    /// The zstd compression level to use when `compressor_kind` is `zstd`.
    pub zstd_compressor_level: i32,

    /// The flush timeout, in seconds.
    pub flush_timeout_secs: u64,

    /// APM configuration applied to trace processing.
    pub apm_config: ApmConfig,

    /// OTLP traces configuration.
    pub otlp_traces: TracesConfig,

    /// The default environment to tag traces with.
    ///
    /// Defaults to `none`.
    pub env: String,
}

impl Default for DatadogTraceConfig {
    fn default() -> Self {
        Self {
            compressor_kind: "zstd".to_string(),
            zstd_compressor_level: 3,
            flush_timeout_secs: 2,
            apm_config: ApmConfig::default(),
            otlp_traces: TracesConfig::default(),
            env: "none".to_string(),
        }
    }
}

/// Configuration for the APM stats encoder component.
///
/// Mirrors `DatadogApmStatsEncoderConfiguration` in `saluki-components`. The injected
/// `agent_hostname` and computed `agent_version` fields are excluded.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DatadogApmStatsEncoderConfig {
    /// The flush timeout, in seconds.
    pub flush_timeout_secs: u64,

    /// The default environment to tag stats with.
    ///
    /// Defaults to `none`.
    pub env: String,
}

impl Default for DatadogApmStatsEncoderConfig {
    fn default() -> Self {
        Self {
            flush_timeout_secs: 2,
            env: "none".to_string(),
        }
    }
}

/// Configuration for the APM stats transform component.
///
/// Mirrors `ApmStatsTransformConfiguration` in `saluki-components`. The injected `default_hostname`
/// and `workload_provider` fields are excluded, leaving the embedded [`ApmConfig`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ApmStatsTransformConfig {
    /// APM configuration applied to stats computation.
    pub apm_config: ApmConfig,
}

/// Configuration for the trace sampler transform component.
///
/// Mirrors `TraceSamplerConfiguration` in `saluki-components`. The original component derives
/// `otlp_sampling_rate` from the OTLP traces probabilistic sampler at construction time; that
/// derived rate is retained here as a config-only field alongside the embedded [`ApmConfig`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TraceSamplerConfig {
    /// APM configuration driving sampler behavior.
    pub apm_config: ApmConfig,

    /// The normalized OTLP probabilistic sampling rate, in the range [0.0, 1.0].
    ///
    /// Defaults to 1.0 (100% sampling), mirroring the OTLP probabilistic sampler default.
    pub otlp_sampling_rate: f64,
}

impl Default for TraceSamplerConfig {
    fn default() -> Self {
        Self {
            apm_config: ApmConfig::default(),
            otlp_sampling_rate: 1.0,
        }
    }
}

/// Configuration for the trace obfuscation transform component.
///
/// Mirrors `TraceObfuscationConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct TraceObfuscationConfig {
    /// The obfuscation settings to apply.
    pub config: ObfuscationConfig,
}

/// APM configuration shared across trace components.
///
/// Mirrors `ApmConfig` in `saluki-components`. The injected `error_tracking_standalone`,
/// `enable_rare_sampler`, `hostname`, and `obfuscation` fields in the original come from a wrapper
/// or the environment; here they are plain config fields except for `hostname`, which is excluded
/// as environment-derived state.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ApmConfig {
    /// Target traces per second for priority sampling.
    ///
    /// Defaults to 10.0.
    pub target_traces_per_second: f64,

    /// Target traces per second for error sampling.
    ///
    /// Defaults to 10.0.
    pub errors_per_second: f64,

    /// Probabilistic sampler configuration.
    pub probabilistic_sampler: ProbabilisticSamplerConfig,

    /// Whether error sampling is enabled.
    ///
    /// When enabled, traces containing errors are kept even if probabilistic sampling would drop
    /// them. Defaults to `true`.
    pub error_sampling_enabled: bool,

    /// Whether standalone error tracking is enabled.
    ///
    /// Defaults to `false`.
    pub error_tracking_standalone: bool,

    /// Whether stats are computed based on a span's `span.kind`.
    ///
    /// Defaults to `true`.
    pub compute_stats_by_span_kind: bool,

    /// Whether peer-related tags are aggregated.
    ///
    /// Defaults to `true`.
    pub peer_tags_aggregation: bool,

    /// Supplementary peer tags beyond the defaults.
    ///
    /// Defaults to empty.
    pub peer_tags: Vec<MetaString>,

    /// The default environment to use when traces do not provide one.
    ///
    /// Defaults to `none`.
    pub default_env: MetaString,

    /// Whether the rare sampler is enabled.
    ///
    /// Defaults to `false`.
    pub enable_rare_sampler: bool,

    /// Rare sampler configuration.
    pub rare_sampler: RareSamplerConfig,

    /// Obfuscation configuration for trace data.
    pub obfuscation: ObfuscationConfig,
}

impl Default for ApmConfig {
    fn default() -> Self {
        Self {
            target_traces_per_second: 10.0,
            errors_per_second: 10.0,
            probabilistic_sampler: ProbabilisticSamplerConfig::default(),
            error_sampling_enabled: true,
            error_tracking_standalone: false,
            compute_stats_by_span_kind: true,
            peer_tags_aggregation: true,
            peer_tags: Vec::new(),
            default_env: MetaString::from_static("none"),
            enable_rare_sampler: false,
            rare_sampler: RareSamplerConfig::default(),
            obfuscation: ObfuscationConfig::default(),
        }
    }
}

/// Probabilistic sampler configuration within [`ApmConfig`].
///
/// Mirrors the private `ProbabilisticSamplerConfig` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ProbabilisticSamplerConfig {
    /// Whether probabilistic sampling is enabled.
    ///
    /// Defaults to `false`.
    pub enabled: bool,

    /// Sampling percentage in the range [0, 100].
    ///
    /// Defaults to 100.0 (keep all traces). Values outside the range are treated as 100.
    pub sampling_percentage: f64,
}

impl Default for ProbabilisticSamplerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sampling_percentage: 100.0,
        }
    }
}

/// Rare sampler configuration within [`ApmConfig`].
///
/// Mirrors the private `RareSamplerConfig` in `saluki-components`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RareSamplerConfig {
    /// Target traces per second for the rare sampler.
    pub tps: f64,

    /// Cooldown duration, in seconds.
    pub cooldown: f64,

    /// Maximum cardinality the rare sampler tracks.
    pub cardinality: usize,
}

impl Default for RareSamplerConfig {
    fn default() -> Self {
        Self {
            tps: 5.0,
            cooldown: 300.0,
            cardinality: 200,
        }
    }
}

/// Obfuscation configuration for trace data.
///
/// Mirrors `ObfuscationConfig` in `saluki-components`, aggregating per-technology obfuscation
/// settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ObfuscationConfig {
    /// Credit card obfuscation settings.
    pub credit_cards: CreditCardObfuscationConfig,

    /// HTTP URL obfuscation settings.
    pub http: HttpObfuscationConfig,

    /// Memcached obfuscation settings.
    pub memcached: MemcachedObfuscationConfig,

    /// Redis obfuscation settings.
    pub redis: RedisObfuscationConfig,

    /// Valkey obfuscation settings.
    pub valkey: ValkeyObfuscationConfig,

    /// SQL obfuscation settings.
    pub sql: SqlObfuscationConfig,

    /// MongoDB obfuscation settings.
    pub mongo: MongoObfuscationConfig,

    /// Elasticsearch obfuscation settings.
    pub es: EsObfuscationConfig,

    /// OpenSearch obfuscation settings.
    pub open_search: OpenSearchObfuscationConfig,
}

/// Credit card obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    pub enabled: bool,

    /// Whether to apply the Luhn check before obfuscating.
    pub luhn: bool,

    /// Values to keep (not obfuscate).
    pub keep_values: Vec<String>,
}

/// HTTP URL obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct HttpObfuscationConfig {
    /// Whether to remove the query string from HTTP URLs.
    pub remove_query_string: bool,

    /// Whether to remove path segments containing digits.
    pub remove_path_digits: bool,
}

/// Memcached obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MemcachedObfuscationConfig {
    /// Whether Memcached obfuscation is enabled.
    pub enabled: bool,

    /// Whether to keep the Memcached command.
    pub keep_command: bool,
}

/// Redis obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    pub enabled: bool,

    /// Whether to remove all command arguments.
    pub remove_all_args: bool,
}

/// Valkey obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    pub enabled: bool,

    /// Whether to remove all command arguments.
    pub remove_all_args: bool,
}

/// SQL obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SqlObfuscationConfig {
    /// The DBMS dialect to assume when obfuscating.
    pub dbms: String,

    /// Whether to collect table names.
    pub table_names: bool,

    /// Whether to replace digits in identifiers.
    pub replace_digits: bool,

    /// Whether to keep SQL aliases.
    pub keep_sql_alias: bool,

    /// Whether to obfuscate dollar-quoted function bodies.
    pub dollar_quoted_func: bool,
}

/// MongoDB obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct MongoObfuscationConfig {
    /// Whether MongoDB obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values are kept (not obfuscated).
    pub keep_values: Vec<String>,

    /// Keys whose values are obfuscated as SQL.
    pub obfuscate_sql_values: Vec<String>,
}

/// Elasticsearch obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct EsObfuscationConfig {
    /// Whether Elasticsearch obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values are kept (not obfuscated).
    pub keep_values: Vec<String>,

    /// Keys whose values are obfuscated as SQL.
    pub obfuscate_sql_values: Vec<String>,
}

/// OpenSearch obfuscation settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OpenSearchObfuscationConfig {
    /// Whether OpenSearch obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values are kept (not obfuscated).
    pub keep_values: Vec<String>,

    /// Keys whose values are obfuscated as SQL.
    pub obfuscate_sql_values: Vec<String>,
}
