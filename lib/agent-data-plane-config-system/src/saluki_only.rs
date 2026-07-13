//! [`SalukiOnly`]: the parsed source values absent from the Datadog Agent schema, plus [`seed`].
//!
//! # What this is
//!
//! Saluki-only keys are values ADP consumes that the Datadog Agent schema does not publish, so they
//! cannot be witnessed and driven like Datadog keys. Today they arrive through the same source as
//! Datadog config (a `datadog.yaml`-shaped map / `DD_*` env), so [`SalukiOnly`] is the ADP-only
//! sibling of the generated Datadog source model: a hand-written `Deserialize` view over that same
//! merged map. [`seed`] then copies each present value into its home in [`SalukiConfiguration`].
//! `seed` and the Datadog `drive` write disjoint fields, so they never contend.
//!
//! # Ownership doctrine
//!
//! A key's home is decided solely by membership in the vendored Datadog schema. The key name and
//! prefix are irrelevant: `data_plane.*`, `dogstatsd_*`, and every other namespace can contain
//! either kind of key.
//!
//! - A key present in the Datadog schema is witnessed by `DatadogConfiguration` and driven into
//!   the model. It must not also have a `SalukiOnly` field, serde alias, or `seed` path.
//! - A key absent from the Datadog schema is seeded here and copied into the model by [`seed`].
//!
//! A second SalukiOnly spelling for a Datadog key is not a compatibility feature. It is a duplicate
//! source of truth and a bug; remove it. Removing that duplicate cannot affect customers because
//! the Datadog-schema path remains authoritative.
//!
//! A dotted SalukiOnly key is reached from the environment by an algorithmic transform, never a
//! lookup table or alias: `foo.bar` is reached by `DD_FOO_BAR`. For example,
//! `data_plane.standalone_mode` is reached by `DD_DATA_PLANE_STANDALONE_MODE` and by no other name.
//!
//! The canonical inventory of these keys (with exact `yaml_path`, type, and default) is
//! `SALUKI_KEYS` in the `datadog-agent-config-overlay-model` crate. Keep this struct in sync with
//! it.
//!
//! # The one invariant (read before adding or debugging a value)
//!
//! This struct mirrors the source key hierarchy exactly. Each field is populated by plain
//! serde from the merged map, so its path here must equal the real config-key path, with no
//! `rename` and (aside from a documented multi-key `alias`) nothing papering over a mismatch:
//!
//! - a top-level key (`dogstatsd_tcp_port`, `aggregate_window_duration_seconds`) is a top-level
//!   field of the same name;
//! - a nested key (`apm_config.default_env`, `data_plane.metrics.v3.series.enabled`) is a field on
//!   a matching chain of nested sub-structs.
//!
//! A mismatch does not error. The field deserializes to `None`, `seed` skips it, and the model
//! keeps its default: the value silently fails to transport.
//!
//! # A component isn't getting a Saluki-only value
//!
//! 1. Value doesn't arrive at all: shape/path bug here, or a missing `seed` line. Confirm a field
//!    exists whose full path equals the `yaml_path` in `SALUKI_KEYS` (nesting included), that its
//!    type coerces the source form (durations are [`DurationString`], not `Duration`), and that
//!    `seed` copies it to the model destination the component reads.
//! 2. Value arrives wrong only when the key is absent: the default bug is not here. The default
//!    lives in the model `Default` in `agent-data-plane-config`, because these fields are
//!    `Option<T>` and `seed` writes only when set. Fix the model `Default`, not this struct.
//! 3. Then add or extend the round-trip test below (set the real key, assert the model field). A
//!    missing test is why a silent transport failure was not caught.
//!
//! # Adding a Saluki-only value
//!
//! Add the field (path == `yaml_path`), pick a coercing type, add the `seed` line to the model
//! home, set the model `Default`, and add a round-trip test.
//!
//! [`seed`]: SalukiOnly::seed
// TODO: consider using derive macros on these for inventory management
// TODO: consider separating these into their own namespace, SALUKI_* and saluki.yaml
// TODO: consider not loading these into the same map as Datadog schema configuration

use std::time::Duration;

use agent_data_plane_config::control::ListenAddress;
use agent_data_plane_config::domains::traces::{OttlErrorMode, OttlFilter, OttlTransform};
use agent_data_plane_config::SalukiConfiguration;
use bytesize::ByteSize;
use saluki_config::DurationString;
use serde::Deserialize;

/// The parsed Saluki-schema-only configuration, shaped to mirror the source key hierarchy.
///
/// Flat keys are top-level fields; nested keys live on matching nested sub-structs. Every field is
/// `#[serde(default)]` and `Option`-typed (or an empty collection), so a deployment may set no
/// Saluki-only keys at all and each falls back to its model default. Deserialized from the same
/// merged map as the Datadog source model, so unknown (Datadog) keys are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SalukiOnly {
    // ── top-level scalar keys ────────────────────────────────────────────────
    /// Internal-telemetry verbosity (`metrics_level`).
    pub metrics_level: Option<String>,
    /// Remote-agent IPC string interner byte budget (`remote_agent_string_interner_size_bytes`).
    pub remote_agent_string_interner_size_bytes: Option<usize>,
    /// Checks IPC endpoint (`checks_ipc_endpoint`).
    pub checks_ipc_endpoint: Option<String>,
    /// Process memory limit (`memory_limit`), given as a bare integer number of bytes or a
    /// byte-size string such as `512MB`. `ByteSize` accepts both forms, so a numeric value does not
    /// fail the load.
    pub memory_limit: Option<ByteSize>,
    /// Memory-accounting slop fraction (`memory_slop_factor`).
    pub memory_slop_factor: Option<f64>,
    /// Encoder flush timeout, in seconds (`flush_timeout_secs`).
    pub flush_timeout_secs: Option<u64>,
    /// Maximum metrics per payload (`serializer_max_metrics_per_payload`).
    pub serializer_max_metrics_per_payload: Option<usize>,

    // ── DogStatsD listener/context/mapper keys (all top-level) ────────────────
    /// TCP listen port (`dogstatsd_tcp_port`).
    pub dogstatsd_tcp_port: Option<u16>,
    /// Baseline number of receive buffers (`dogstatsd_buffer_count`).
    pub dogstatsd_buffer_count: Option<usize>,
    /// Maximum number of receive buffers (`dogstatsd_buffer_count_max`).
    pub dogstatsd_buffer_count_max: Option<usize>,
    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT` (`dogstatsd_autoscale_udp_listeners`).
    pub dogstatsd_autoscale_udp_listeners: Option<bool>,
    /// Whether to relax decoder strictness (`dogstatsd_permissive_decoding`).
    pub dogstatsd_permissive_decoding: Option<bool>,
    /// Maximum cached metric contexts (`dogstatsd_cached_contexts_limit`).
    pub dogstatsd_cached_contexts_limit: Option<usize>,
    /// Maximum cached tagsets (`dogstatsd_cached_tagsets_limit`).
    pub dogstatsd_cached_tagsets_limit: Option<usize>,
    /// Explicit byte budget for the context interner (`dogstatsd_string_interner_size_bytes`).
    pub dogstatsd_string_interner_size_bytes: Option<u64>,
    /// Whether to allow heap allocations for contexts (`dogstatsd_allow_context_heap_allocs`).
    pub dogstatsd_allow_context_heap_allocs: Option<bool>,
    /// Floor for metric sample rates (`dogstatsd_minimum_sample_rate`).
    pub dogstatsd_minimum_sample_rate: Option<f64>,
    /// Mapper string interner entry count (`dogstatsd_mapper_string_interner_size`).
    pub dogstatsd_mapper_string_interner_size: Option<u64>,

    // ── aggregation keys (all top-level) ──────────────────────────────────────
    /// Aggregation window size, in seconds (`aggregate_window_duration_seconds`).
    pub aggregate_window_duration_seconds: Option<u64>,
    /// Maximum contexts per aggregation window (`aggregate_context_limit`).
    pub aggregate_context_limit: Option<usize>,
    /// Aggregator flush period (`aggregate_flush_interval`).
    pub aggregate_flush_interval: Option<DurationString>,
    /// Whether open aggregation windows are flushed (`aggregate_flush_open_windows`).
    pub aggregate_flush_open_windows: Option<bool>,
    /// Passthrough idle flush delay (`aggregate_passthrough_idle_flush_timeout`).
    pub aggregate_passthrough_idle_flush_timeout: Option<DurationString>,

    // ── OTLP metric context keys (all top-level) ──────────────────────────────
    /// Whether to allow heap allocations for OTLP contexts (`otlp_allow_context_heap_allocs`).
    pub otlp_allow_context_heap_allocs: Option<bool>,
    /// Maximum cached OTLP metric contexts (`otlp_cached_contexts_limit`).
    pub otlp_cached_contexts_limit: Option<usize>,
    /// Maximum cached OTLP tagsets (`otlp_cached_tagsets_limit`).
    pub otlp_cached_tagsets_limit: Option<usize>,
    /// OTLP context interner entry count (`otlp_string_interner_size`).
    pub otlp_string_interner_size: Option<u64>,

    // ── nested sections ───────────────────────────────────────────────────────
    /// Cross-cutting data-plane knobs (`data_plane.*`).
    pub data_plane: DataPlane,
    /// APM trace knobs (`apm_config.*`).
    pub apm_config: ApmConfig,
    /// OTLP receiver and trace knobs (`otlp_config.*`).
    pub otlp_config: OtlpConfig,
    /// OTTL span-drop filter (`ottl_filter_config`).
    pub ottl_filter_config: Option<OttlFilterConfig>,
    /// OTTL span-transform processor (`ottl_transform_config`).
    pub ottl_transform_config: Option<OttlTransformConfig>,
}

/// `data_plane.*` Saluki-only knobs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlane {
    /// ADP graceful shutdown timeout, in seconds (`data_plane.stop_timeout`).
    pub stop_timeout: Option<u64>,
    /// Whether ADP runs in standalone mode (`data_plane.standalone_mode`).
    pub standalone_mode: Option<bool>,
    /// Checks pipeline gate (`data_plane.checks.*`).
    pub checks: DataPlaneChecks,
    /// Metrics-intake knobs (`data_plane.metrics.*`).
    pub metrics: DataPlaneMetrics,
}

/// `data_plane.checks.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneChecks {
    /// Whether the checks pipeline is enabled (`data_plane.checks.enabled`).
    pub enabled: Option<bool>,
}

/// `data_plane.metrics.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneMetrics {
    /// V3 metrics-intake knobs (`data_plane.metrics.v3.*`).
    pub v3: DataPlaneMetricsV3,
}

/// `data_plane.metrics.v3.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneMetricsV3 {
    /// V3 series knobs (`data_plane.metrics.v3.series.*`).
    pub series: DataPlaneMetricsV3Series,
}

/// `data_plane.metrics.v3.series.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneMetricsV3Series {
    /// ADP-only safety gate authorizing V3 series (`data_plane.metrics.v3.series.enabled`).
    pub enabled: Option<bool>,
}

/// `apm_config.*` Saluki-only knobs. (The Datadog Agent publishes many other `apm_config.*` keys;
/// those are witnessed and ignored here.)
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApmConfig {
    /// Default trace environment (`apm_config.default_env`).
    pub default_env: Option<String>,
    /// Whether error sampling is enabled (`apm_config.error_sampling_enabled`).
    pub error_sampling_enabled: Option<bool>,
    /// Rare sampler tuning (`apm_config.rare_sampler.*`).
    pub rare_sampler: ApmRareSampler,
    /// SQL obfuscation knobs (`apm_config.obfuscation.*`).
    pub obfuscation: ApmObfuscation,
}

/// `apm_config.rare_sampler.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApmRareSampler {
    /// Tracked-signature cardinality (`apm_config.rare_sampler.cardinality`).
    pub cardinality: Option<usize>,
    /// Cooldown between rare-sample emissions (`apm_config.rare_sampler.cooldown`).
    pub cooldown: Option<f64>,
    /// Rare-sample traces-per-second budget (`apm_config.rare_sampler.tps`).
    pub tps: Option<f64>,
}

/// `apm_config.obfuscation.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApmObfuscation {
    /// SQL obfuscation knobs (`apm_config.obfuscation.sql.*`).
    pub sql: ApmObfuscationSql,
}

/// `apm_config.obfuscation.sql.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApmObfuscationSql {
    /// SQL dialect (`apm_config.obfuscation.sql.dbms`).
    pub dbms: Option<String>,
    /// Preserve dollar-quoted SQL functions (`apm_config.obfuscation.sql.dollar_quoted_func`).
    pub dollar_quoted_func: Option<bool>,
    /// Preserve SQL aliases (`apm_config.obfuscation.sql.keep_sql_alias`).
    pub keep_sql_alias: Option<bool>,
    /// Replace digits during SQL obfuscation (`apm_config.obfuscation.sql.replace_digits`).
    pub replace_digits: Option<bool>,
    /// Collect table names during SQL obfuscation (`apm_config.obfuscation.sql.table_names`).
    pub table_names: Option<bool>,
}

/// `otlp_config.*` Saluki-only knobs. (The Datadog Agent publishes many other `otlp_config.*` keys;
/// those are witnessed and ignored here.)
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpConfig {
    /// OTLP receiver knobs (`otlp_config.receiver.*`).
    pub receiver: OtlpConfigReceiver,
    /// OTLP trace-ingestion knobs (`otlp_config.traces.*`).
    pub traces: OtlpConfigTraces,
}

/// `otlp_config.receiver.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpConfigReceiver {
    /// OTLP receiver protocol knobs (`otlp_config.receiver.protocols.*`).
    pub protocols: OtlpConfigReceiverProtocols,
}

/// `otlp_config.receiver.protocols.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpConfigReceiverProtocols {
    /// OTLP HTTP receiver knobs (`otlp_config.receiver.protocols.http.*`).
    pub http: OtlpConfigReceiverProtocolsHttp,
}

/// `otlp_config.receiver.protocols.http.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpConfigReceiverProtocolsHttp {
    /// OTLP HTTP receiver transport (`otlp_config.receiver.protocols.http.transport`).
    pub transport: Option<String>,
}

/// `otlp_config.traces.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpConfigTraces {
    /// OTLP trace context interner entry count (`otlp_config.traces.string_interner_size`).
    pub string_interner_size: Option<u64>,
    /// Compute top-level spans by span kind
    /// (`otlp_config.traces.enable_otlp_compute_top_level_by_span_kind`).
    pub enable_otlp_compute_top_level_by_span_kind: Option<bool>,
    /// Ignore missing Datadog fields on OTLP spans (`otlp_config.traces.ignore_missing_datadog_fields`).
    pub ignore_missing_datadog_fields: Option<bool>,
}

/// The `ottl_filter_config` object: OTTL span-drop filter.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OttlFilterConfig {
    /// Evaluation error handling mode (`ottl_filter_config.error_mode`: `ignore` / `silent` /
    /// `propagate`).
    pub error_mode: Option<String>,
    /// OTTL trace filters (`ottl_filter_config.traces.*`).
    pub traces: OttlFilterTraces,
}

/// `ottl_filter_config.traces.*`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OttlFilterTraces {
    /// OTTL span-drop conditions (`ottl_filter_config.traces.span`).
    pub span: Vec<String>,
}

/// The `ottl_transform_config` object: OTTL span-transform processor.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OttlTransformConfig {
    /// Evaluation error handling mode (`ottl_transform_config.error_mode`: `ignore` / `silent` /
    /// `propagate`).
    pub error_mode: Option<String>,
    /// OTTL span-mutating statements (`ottl_transform_config.trace_statements`).
    pub trace_statements: Vec<String>,
}

/// Parses an OTTL error mode string, defaulting to the model default (`Propagate`) when absent or
/// unrecognized.
fn parse_ottl_error_mode(mode: Option<String>) -> OttlErrorMode {
    match mode.as_deref() {
        Some("ignore") => OttlErrorMode::Ignore,
        Some("silent") => OttlErrorMode::Silent,
        _ => OttlErrorMode::Propagate,
    }
}

impl SalukiOnly {
    /// Copies each present Saluki-only value into its destination in `config`.
    ///
    /// Absent values leave the field at its model default. The Datadog `drive` writes a disjoint set
    /// of fields, so it does not matter whether `seed` runs before or after the drive.
    pub(crate) fn seed(&self, config: &mut SalukiConfiguration) {
        // control
        if let Some(v) = self.data_plane.stop_timeout {
            config.control.stop_timeout = v;
        }
        if let Some(v) = self.data_plane.standalone_mode {
            config.control.standalone_mode = v;
        }
        if let Some(v) = self.data_plane.checks.enabled {
            config.control.checks = v;
        }
        if let Some(v) = self.memory_limit {
            config.control.memory_limit = v.as_u64();
        }
        if let Some(v) = self.memory_slop_factor {
            config.control.memory_slop_factor = v;
        }
        if let Some(v) = self.remote_agent_string_interner_size_bytes {
            config.control.ipc.remote_agent_string_interner_size_bytes = v;
        }

        // shared
        if let Some(v) = self.metrics_level.clone() {
            config.shared.metrics_level = v;
        }
        if let Some(v) = self.flush_timeout_secs {
            config.shared.metrics_encoding.flush_timeout = Duration::from_secs(v);
        }
        if let Some(v) = self.serializer_max_metrics_per_payload {
            config.shared.metrics_encoding.max_metrics_per_payload = v;
        }
        if let Some(v) = self.data_plane.metrics.v3.series.enabled {
            config.shared.metrics_encoding.v3_series_enabled = v;
        }

        // domains.dogstatsd
        let dsd = &mut config.domains.dogstatsd;
        if let Some(v) = self.dogstatsd_tcp_port {
            dsd.listeners.tcp_port = v;
        }
        if let Some(v) = self.dogstatsd_buffer_count {
            dsd.listeners.buffer_count = v;
        }
        if let Some(v) = self.dogstatsd_buffer_count_max {
            dsd.listeners.buffer_count_max = v;
        }
        if let Some(v) = self.dogstatsd_autoscale_udp_listeners {
            dsd.listeners.autoscale_udp_listeners = v;
        }
        if let Some(v) = self.dogstatsd_permissive_decoding {
            dsd.listeners.permissive_decoding = v;
        }
        if let Some(v) = self.dogstatsd_cached_contexts_limit {
            dsd.contexts.cached_contexts_limit = v;
        }
        if let Some(v) = self.dogstatsd_cached_tagsets_limit {
            dsd.contexts.cached_tagsets_limit = v;
        }
        if let Some(v) = self.dogstatsd_string_interner_size_bytes {
            dsd.contexts.string_interner_size_bytes = Some(v);
        }
        if let Some(v) = self.dogstatsd_allow_context_heap_allocs {
            dsd.contexts.allow_context_heap_allocs = v;
        }
        if let Some(v) = self.dogstatsd_minimum_sample_rate {
            dsd.contexts.minimum_sample_rate = v;
        }
        if let Some(v) = self.dogstatsd_mapper_string_interner_size {
            dsd.mapper.string_interner_size = v;
        }
        if let Some(v) = self.aggregate_window_duration_seconds {
            dsd.aggregation.window_duration_seconds = v;
        }
        if let Some(v) = self.aggregate_context_limit {
            dsd.aggregation.context_limit = v;
        }
        if let Some(v) = self.aggregate_flush_interval {
            dsd.aggregation.flush_interval = v.as_duration();
        }
        if let Some(v) = self.aggregate_flush_open_windows {
            dsd.aggregation.flush_open_windows = v;
        }
        if let Some(v) = self.aggregate_passthrough_idle_flush_timeout {
            dsd.aggregation.passthrough_idle_flush_timeout = v.as_duration();
        }

        // domains.otlp
        let otlp = &mut config.domains.otlp;
        if let Some(v) = self.otlp_allow_context_heap_allocs {
            otlp.contexts.allow_context_heap_allocs = v;
        }
        if let Some(v) = self.otlp_cached_contexts_limit {
            otlp.contexts.cached_contexts_limit = v;
        }
        if let Some(v) = self.otlp_cached_tagsets_limit {
            otlp.contexts.cached_tagsets_limit = v;
        }
        if let Some(v) = self.otlp_string_interner_size {
            otlp.contexts.string_interner_size = v;
        }
        if let Some(v) = self.otlp_config.receiver.protocols.http.transport.clone() {
            otlp.receiver.http.transport = v;
        }

        // domains.traces
        let traces = &mut config.domains.traces;
        if let Some(v) = self.apm_config.default_env.clone() {
            traces.default_env = v;
        }
        if let Some(v) = self.apm_config.error_sampling_enabled {
            traces.error_sampling_enabled = v;
        }
        if let Some(v) = self.apm_config.rare_sampler.cardinality {
            traces.rare_sampler.cardinality = v;
        }
        if let Some(v) = self.apm_config.rare_sampler.cooldown {
            traces.rare_sampler.cooldown = v;
        }
        if let Some(v) = self.apm_config.rare_sampler.tps {
            traces.rare_sampler.tps = v;
        }
        if let Some(v) = self.apm_config.obfuscation.sql.dbms.clone() {
            traces.obfuscation.sql.dbms = v;
        }
        if let Some(v) = self.apm_config.obfuscation.sql.dollar_quoted_func {
            traces.obfuscation.sql.dollar_quoted_func = v;
        }
        if let Some(v) = self.apm_config.obfuscation.sql.keep_sql_alias {
            traces.obfuscation.sql.keep_sql_alias = v;
        }
        if let Some(v) = self.apm_config.obfuscation.sql.replace_digits {
            traces.obfuscation.sql.replace_digits = v;
        }
        if let Some(v) = self.apm_config.obfuscation.sql.table_names {
            traces.obfuscation.sql.table_names = v;
        }
        if let Some(v) = self.otlp_config.traces.string_interner_size {
            traces.otlp.string_interner_size = v;
        }
        if let Some(v) = self.otlp_config.traces.enable_otlp_compute_top_level_by_span_kind {
            traces.otlp.enable_compute_top_level_by_span_kind = v;
        }
        if let Some(v) = self.otlp_config.traces.ignore_missing_datadog_fields {
            traces.otlp.ignore_missing_datadog_fields = v;
        }
        if let Some(filter) = &self.ottl_filter_config {
            traces.ottl_filter = OttlFilter {
                error_mode: parse_ottl_error_mode(filter.error_mode.clone()),
                span_conditions: filter.traces.span.clone(),
            };
        }
        if let Some(transform) = &self.ottl_transform_config {
            traces.ottl_transform = OttlTransform {
                error_mode: parse_ottl_error_mode(transform.error_mode.clone()),
                trace_statements: transform.trace_statements.clone(),
            };
        }

        // domains.checks
        if let Some(v) = self.checks_ipc_endpoint.clone() {
            config.domains.checks.ipc_endpoint = ListenAddress(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Round-trip guard and executable inventory: set every Saluki-only key at its real config path,
    /// deserialize + seed, and assert it reached its model home. A knob that silently fails to
    /// transport (shape/path mismatch, wrong coercion type, or a missing `seed` line) shows up here
    /// as a failed assertion. Copy a case when adding a knob.
    #[test]
    fn every_saluki_only_key_transports_to_the_model() {
        let map = json!({
            // top-level scalars
            "metrics_level": "debug",
            "remote_agent_string_interner_size_bytes": 4096,
            "checks_ipc_endpoint": "localhost:5006",
            "memory_limit": "512MB",
            "memory_slop_factor": 0.3,
            "flush_timeout_secs": 7,
            "serializer_max_metrics_per_payload": 999,
            // dogstatsd listener/context/mapper
            "dogstatsd_tcp_port": 8126,
            "dogstatsd_buffer_count": 64,
            "dogstatsd_buffer_count_max": 512,
            "dogstatsd_autoscale_udp_listeners": true,
            "dogstatsd_permissive_decoding": false,
            "dogstatsd_cached_contexts_limit": 500000,
            "dogstatsd_cached_tagsets_limit": 400000,
            "dogstatsd_string_interner_size_bytes": 1048576,
            "dogstatsd_allow_context_heap_allocs": true,
            "dogstatsd_minimum_sample_rate": 0.25,
            "dogstatsd_mapper_string_interner_size": 2048,
            // aggregation
            "aggregate_window_duration_seconds": 30,
            "aggregate_context_limit": 250000,
            "aggregate_flush_interval": "20s",
            "aggregate_flush_open_windows": true,
            "aggregate_passthrough_idle_flush_timeout": "2s",
            // otlp metric contexts
            "otlp_allow_context_heap_allocs": true,
            "otlp_cached_contexts_limit": 111,
            "otlp_cached_tagsets_limit": 222,
            "otlp_string_interner_size": 333,
            // nested: data_plane
            "data_plane": {
                "stop_timeout": 45,
                "standalone_mode": true,
                "checks": { "enabled": true },
                "metrics": { "v3": { "series": { "enabled": true } } }
            },
            // nested: apm_config
            "apm_config": {
                "default_env": "staging",
                "error_sampling_enabled": true,
                "rare_sampler": { "cardinality": 9, "cooldown": 1.5, "tps": 3.0 },
                "obfuscation": {
                    "sql": {
                        "dbms": "postgresql",
                        "dollar_quoted_func": true,
                        "keep_sql_alias": true,
                        "replace_digits": true,
                        "table_names": true
                    }
                }
            },
            // nested: otlp_config
            "otlp_config": {
                "receiver": { "protocols": { "http": { "transport": "tcp" } } },
                "traces": {
                    "string_interner_size": 777,
                    "enable_otlp_compute_top_level_by_span_kind": true,
                    "ignore_missing_datadog_fields": true
                }
            },
            // top-level objects
            "ottl_filter_config": { "error_mode": "ignore", "traces": { "span": ["attributes[\"a\"] == \"b\""] } },
            "ottl_transform_config": { "error_mode": "silent", "trace_statements": ["set(name, \"x\")"] },
        });

        let saluki_only: SalukiOnly = serde_json::from_value(map).expect("saluki-only source deserializes");
        let mut config = SalukiConfiguration::default();
        saluki_only.seed(&mut config);

        // control
        assert_eq!(config.control.stop_timeout, 45);
        assert!(config.control.standalone_mode);
        assert!(config.control.checks);
        assert_eq!(config.control.memory_limit, ByteSize::mb(512).as_u64());
        assert_eq!(config.control.memory_slop_factor, 0.3);
        assert_eq!(config.control.ipc.remote_agent_string_interner_size_bytes, 4096);

        // shared
        assert_eq!(config.shared.metrics_level, "debug");
        assert_eq!(config.shared.metrics_encoding.flush_timeout, Duration::from_secs(7));
        assert_eq!(config.shared.metrics_encoding.max_metrics_per_payload, 999);
        assert!(config.shared.metrics_encoding.v3_series_enabled);

        // domains.dogstatsd
        let dsd = &config.domains.dogstatsd;
        assert_eq!(dsd.listeners.tcp_port, 8126);
        assert_eq!(dsd.listeners.buffer_count, 64);
        assert_eq!(dsd.listeners.buffer_count_max, 512);
        assert!(dsd.listeners.autoscale_udp_listeners);
        assert!(!dsd.listeners.permissive_decoding);
        assert_eq!(dsd.contexts.cached_contexts_limit, 500_000);
        assert_eq!(dsd.contexts.cached_tagsets_limit, 400_000);
        assert_eq!(dsd.contexts.string_interner_size_bytes, Some(1_048_576));
        assert!(dsd.contexts.allow_context_heap_allocs);
        assert_eq!(dsd.contexts.minimum_sample_rate, 0.25);
        assert_eq!(dsd.mapper.string_interner_size, 2048);
        assert_eq!(dsd.aggregation.window_duration_seconds, 30);
        assert_eq!(dsd.aggregation.context_limit, 250_000);
        assert_eq!(dsd.aggregation.flush_interval, Duration::from_secs(20));
        assert!(dsd.aggregation.flush_open_windows);
        assert_eq!(dsd.aggregation.passthrough_idle_flush_timeout, Duration::from_secs(2));

        // domains.otlp
        let otlp = &config.domains.otlp;
        assert!(otlp.contexts.allow_context_heap_allocs);
        assert_eq!(otlp.contexts.cached_contexts_limit, 111);
        assert_eq!(otlp.contexts.cached_tagsets_limit, 222);
        assert_eq!(otlp.contexts.string_interner_size, 333);
        assert_eq!(otlp.receiver.http.transport, "tcp");

        // domains.traces
        let traces = &config.domains.traces;
        assert_eq!(traces.default_env, "staging");
        assert!(traces.error_sampling_enabled);
        assert_eq!(traces.rare_sampler.cardinality, 9);
        assert_eq!(traces.rare_sampler.cooldown, 1.5);
        assert_eq!(traces.rare_sampler.tps, 3.0);
        assert_eq!(traces.obfuscation.sql.dbms, "postgresql");
        assert!(traces.obfuscation.sql.dollar_quoted_func);
        assert!(traces.obfuscation.sql.keep_sql_alias);
        assert!(traces.obfuscation.sql.replace_digits);
        assert!(traces.obfuscation.sql.table_names);
        assert_eq!(traces.otlp.string_interner_size, 777);
        assert!(traces.otlp.enable_compute_top_level_by_span_kind);
        assert!(traces.otlp.ignore_missing_datadog_fields);
        assert_eq!(traces.ottl_filter.error_mode, OttlErrorMode::Ignore);
        assert_eq!(
            traces.ottl_filter.span_conditions,
            vec!["attributes[\"a\"] == \"b\"".to_string()]
        );
        assert_eq!(traces.ottl_transform.error_mode, OttlErrorMode::Silent);
        assert_eq!(
            traces.ottl_transform.trace_statements,
            vec!["set(name, \"x\")".to_string()]
        );

        // domains.checks
        assert_eq!(config.domains.checks.ipc_endpoint.0, "localhost:5006");
    }

    /// `memory_limit` is a byte size the source may express as a bare integer (bytes) or a suffixed
    /// string. Both must deserialize to the same byte count; a bare integer previously failed the
    /// whole config load.
    #[test]
    fn memory_limit_accepts_a_bare_integer_or_a_string() {
        for (value, expected) in [
            (json!({ "memory_limit": 1 }), 1),
            (json!({ "memory_limit": "512MB" }), ByteSize::mb(512).as_u64()),
        ] {
            let saluki_only: SalukiOnly = serde_json::from_value(value).expect("memory_limit deserializes");
            let mut config = SalukiConfiguration::default();
            saluki_only.seed(&mut config);
            assert_eq!(config.control.memory_limit, expected);
        }
    }

    /// An absent key leaves the model default in place (the common case). `seed` writes only present
    /// options, so this exercises the `Option`-in-source / default-in-model split. A wrong value
    /// here means the model `Default` is wrong, not this struct.
    #[test]
    fn absent_keys_leave_model_defaults() {
        let saluki_only: SalukiOnly = serde_json::from_value(json!({})).expect("empty source deserializes");
        let mut config = SalukiConfiguration::default();
        saluki_only.seed(&mut config);

        let agg = &config.domains.dogstatsd.aggregation;
        assert_eq!(agg.window_duration_seconds, 10);
        assert_eq!(agg.context_limit, 1_000_000);
        assert_eq!(agg.flush_interval, Duration::from_secs(15));
        assert_eq!(agg.passthrough_idle_flush_timeout, Duration::from_secs(1));
    }
}
