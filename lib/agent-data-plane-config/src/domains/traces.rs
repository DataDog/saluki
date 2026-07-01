//! Traces domain: APM trace processing (env, sampling, obfuscation) plus OTLP trace ingestion.

use serde::Serialize;

/// Resolved traces configuration.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Domain {
    /// Environment tag applied to traces.
    pub env: String,

    /// Environment used for traces that carry no explicit environment. (not in Datadog Agent config
    /// schema)
    pub default_env: String,

    /// Whether trace stats are computed separately per span kind.
    pub compute_stats_by_span_kind: bool,

    /// Span tags promoted to peer tags for peer-service aggregation.
    pub peer_tags: Vec<String>,

    /// Whether stats are aggregated by peer tags.
    pub peer_tags_aggregation: bool,

    /// Whether error spans are sampled independently of the base sampler. (not in Datadog Agent
    /// config schema)
    pub error_sampling_enabled: bool,

    /// Whether error tracking runs standalone, without full trace ingestion.
    pub error_tracking_standalone_enabled: bool,

    /// Target number of error traces sampled per second.
    pub errors_per_second: f64,

    /// Target number of traces sampled per second.
    pub target_traces_per_second: f64,

    /// Whether the rare-span sampler is enabled.
    pub enable_rare_sampler: bool,

    /// Rare-span sampler settings.
    pub rare_sampler: RareSampler,

    /// Probabilistic sampler settings.
    pub probabilistic_sampler: ProbabilisticSampler,

    /// Per-subsystem trace obfuscation settings.
    pub obfuscation: Obfuscation,

    /// OTLP trace ingestion settings.
    pub otlp: OtlpTraces,

    /// OTTL span-drop filter settings.
    pub ottl_filter: OttlFilter,

    /// OTTL span-transform settings.
    pub ottl_transform: OttlTransform,
}

/// Rare-span sampler.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RareSampler {
    /// Maximum number of distinct span signatures tracked. (not in Datadog Agent config schema)
    pub cardinality: usize,

    /// Cooldown, in seconds, before a signature may be sampled again. (not in Datadog Agent config
    /// schema)
    pub cooldown: f64,

    /// Target rare-span traces sampled per second. (not in Datadog Agent config schema)
    pub tps: f64,
}

/// APM probabilistic sampler.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProbabilisticSampler {
    /// Whether the probabilistic sampler is enabled.
    pub enabled: bool,

    /// Percentage of traces the probabilistic sampler keeps.
    pub sampling_percentage: f64,
}

/// OTLP trace ingestion specifics.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OtlpTraces {
    /// Whether OTLP trace ingestion is enabled.
    pub enabled: bool,

    /// Internal port the OTLP trace receiver forwards to.
    pub internal_port: u16,

    /// Percentage of OTLP traces the probabilistic sampler keeps.
    pub probabilistic_sampler_sampling_percentage: f64,

    /// Number of entries the OTLP trace context interner holds. (not in Datadog Agent config
    /// schema)
    pub string_interner_size: u64,

    /// Whether top-level spans are computed from span kind on OTLP traces. (not in Datadog Agent
    /// config schema)
    pub enable_compute_top_level_by_span_kind: bool,

    /// Whether spans missing intake-required fields are ingested rather than rejected. (not in
    /// Datadog Agent config schema)
    pub ignore_missing_datadog_fields: bool,
}

/// Trace obfuscation, one group per supported subsystem.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Obfuscation {
    /// Credit-card obfuscation in span metadata.
    pub credit_cards: CreditCardObfuscation,

    /// Elasticsearch query obfuscation.
    pub elasticsearch: JsonQueryObfuscation,

    /// HTTP path and query obfuscation.
    pub http: HttpObfuscation,

    /// Memcached command obfuscation.
    pub memcached: MemcachedObfuscation,

    /// MongoDB query obfuscation.
    pub mongodb: JsonQueryObfuscation,

    /// OpenSearch query obfuscation.
    pub opensearch: JsonQueryObfuscation,

    /// Redis command obfuscation.
    pub redis: CacheObfuscation,

    /// Valkey command obfuscation.
    pub valkey: CacheObfuscation,

    /// SQL query obfuscation. (not in Datadog Agent config schema)
    pub sql: SqlObfuscation,
}

/// Credit-card obfuscation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CreditCardObfuscation {
    /// Whether credit-card numbers are obfuscated.
    pub enabled: bool,

    /// Tag or field names whose values are not obfuscated.
    pub keep_values: Vec<String>,

    /// Whether a Luhn check is applied before a value is treated as a card number.
    pub luhn: bool,
}

/// Obfuscation shape shared by the JSON-query engines (Elasticsearch, MongoDB, OpenSearch).
#[derive(Clone, Debug, Default, Serialize)]
pub struct JsonQueryObfuscation {
    /// Whether queries are obfuscated.
    pub enabled: bool,

    /// JSON keys whose values are not obfuscated.
    pub keep_values: Vec<String>,

    /// JSON keys whose values are obfuscated as embedded SQL.
    pub obfuscate_sql_values: Vec<String>,
}

/// HTTP path/query obfuscation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HttpObfuscation {
    /// Whether path segments containing digits are removed.
    pub remove_paths_with_digits: bool,

    /// Whether the query string is removed.
    pub remove_query_string: bool,
}

/// Memcached command obfuscation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MemcachedObfuscation {
    /// Whether Memcached commands are obfuscated.
    pub enabled: bool,

    /// Whether the command verb is preserved.
    pub keep_command: bool,
}

/// Obfuscation shape shared by the key/value caches (redis, valkey).
#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheObfuscation {
    /// Whether cache commands are obfuscated.
    pub enabled: bool,

    /// Whether all command arguments are removed.
    pub remove_all_args: bool,
}

/// SQL obfuscation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SqlObfuscation {
    /// SQL dialect the obfuscator parses against.
    pub dbms: String,

    /// Whether dollar-quoted function bodies are preserved.
    pub dollar_quoted_func: bool,

    /// Whether column and table aliases are preserved.
    pub keep_sql_alias: bool,

    /// Whether digits in identifiers are replaced with a placeholder.
    pub replace_digits: bool,

    /// Whether table names are collected as metadata.
    pub table_names: bool,
}

/// Error-handling mode for OTTL condition/statement evaluation, shared by the OTTL filter and
/// transform processors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum OttlErrorMode {
    /// Log evaluation errors and continue.
    Ignore,
    /// Swallow evaluation errors silently and continue.
    Silent,
    /// Propagate the error up the pipeline; the payload is dropped.
    #[default]
    Propagate,
}

/// OTTL filter processor: span-drop conditions applied during trace enrichment.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OttlFilter {
    /// How evaluation errors in the filter conditions are handled. (not in Datadog Agent config
    /// schema)
    pub error_mode: OttlErrorMode,

    /// OTTL conditions; a span matching any of them is dropped. (not in Datadog Agent config
    /// schema)
    pub span_conditions: Vec<String>,
}

/// OTTL transform processor: span-mutating statements applied during trace enrichment.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OttlTransform {
    /// How evaluation errors in the transform statements are handled. (not in Datadog Agent config
    /// schema)
    pub error_mode: OttlErrorMode,

    /// OTTL statements applied to each span. (not in Datadog Agent config schema)
    pub trace_statements: Vec<String>,
}
