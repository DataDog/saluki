//! [`SalukiOnly`]: the parsed Saluki-schema-only source input, plus [`seed`].
//!
//! Saluki-schema-only keys are fields that cannot arrive by deserializing Datadog configuration,
//! because they do not exist in the Datadog configuration schema. These structs derive
//! `Deserialize`: the source adapter, not the model crate, owns source deserialization.
//!
//! [`seed`] copies each present Saluki-only value into its destination in a `SalukiConfiguration`.
//! It and the Datadog `drive` write disjoint fields, so the two writers never contend for the same
//! destination.
//!
//! [`seed`]: SalukiOnly::seed
// TODO: consider using derive macros on these for inventory management

use std::time::Duration;

use agent_data_plane_config::control::ListenAddress;
use agent_data_plane_config::domains::traces::{OttlErrorMode, OttlFilter, OttlTransform};
use agent_data_plane_config::SalukiConfiguration;
use serde::Deserialize;

/// The parsed Saluki-schema-only configuration, grouped by subsystem.
///
/// Parsed from `SALUKI_*` / `saluki.yaml`. Every field is `#[serde(default)]`: a deployment may set
/// no Saluki-only keys at all, so each subsystem group falls back to its defaults independently when
/// its keys are absent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SalukiOnly {
    /// Cross-cutting data-plane control knobs (`data_plane.*`).
    pub data_plane: DataPlaneSalukiOnly,
    /// Process memory-accounting knobs.
    pub accounting: AccountingSalukiOnly,
    /// Internal-telemetry verbosity (`metrics_level`).
    pub metrics_level: Option<String>,
    /// Remote-agent IPC string interner byte budget (`remote_agent_string_interner_size_bytes`).
    pub remote_agent_string_interner_size_bytes: Option<usize>,
    /// Encoder knobs shared across the metrics-emitting pipelines.
    pub encoders: EncodersSalukiOnly,
    /// DogStatsD listener, context, and aggregation knobs.
    pub dogstatsd: DogStatsDSalukiOnly,
    /// OTLP receiver and context knobs.
    pub otlp: OtlpSalukiOnly,
    /// APM trace knobs (default env, sampling, SQL obfuscation, OTTL processors).
    pub traces: TracesSalukiOnly,
    /// Checks IPC endpoint (`checks_ipc_endpoint`).
    pub checks_ipc_endpoint: Option<String>,
}

/// Cross-cutting data-plane control knobs (`data_plane.*`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DataPlaneSalukiOnly {
    /// ADP graceful shutdown timeout, in seconds (`data_plane.stop_timeout`).
    pub stop_timeout: Option<u64>,
    /// Whether the checks pipeline is enabled (`data_plane.checks.enabled`).
    pub checks_enabled: Option<bool>,
    /// Whether ADP runs in standalone mode (`data_plane.standalone_mode`).
    pub standalone_mode: Option<bool>,
}

/// Process memory-accounting knobs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AccountingSalukiOnly {
    /// Process memory limit, a byte-size string such as `512MB` (`memory_limit`).
    pub memory_limit: Option<String>,
    /// Memory-accounting slop fraction (`memory_slop_factor`).
    pub memory_slop_factor: Option<f64>,
}

/// Encoder knobs shared across the metrics-emitting pipelines.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EncodersSalukiOnly {
    /// Encoder flush timeout, in seconds (`flush_timeout_secs`).
    pub flush_timeout_secs: Option<u64>,
    /// Maximum metrics per payload (`serializer_max_metrics_per_payload`).
    pub serializer_max_metrics_per_payload: Option<usize>,
    /// ADP-only safety gate authorizing V3 series (`data_plane.metrics.v3.series.enabled`, absent
    /// from the Datadog Agent config schema).
    pub v3_series_enabled: Option<bool>,
}

/// DogStatsD listener, context, and aggregation Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DogStatsDSalukiOnly {
    /// TCP listen port (`dogstatsd_tcp_port`).
    pub tcp_port: Option<u16>,
    /// Number of receive buffers (`dogstatsd_buffer_count`).
    pub buffer_count: Option<usize>,
    /// Maximum number of receive buffers (`dogstatsd_buffer_count_max`).
    pub buffer_count_max: Option<usize>,
    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT` (`dogstatsd_autoscale_udp_listeners`).
    pub autoscale_udp_listeners: Option<bool>,
    /// Whether to relax decoder strictness (`dogstatsd_permissive_decoding`).
    pub permissive_decoding: Option<bool>,
    /// Maximum cached metric contexts (`dogstatsd_cached_contexts_limit`).
    pub cached_contexts_limit: Option<usize>,
    /// Maximum cached tagsets (`dogstatsd_cached_tagsets_limit`).
    pub cached_tagsets_limit: Option<usize>,
    /// Explicit byte budget for the context interner (`dogstatsd_string_interner_size_bytes`).
    pub string_interner_size_bytes: Option<u64>,
    /// Whether to allow heap allocations for contexts (`dogstatsd_allow_context_heap_allocs`).
    pub allow_context_heap_allocs: Option<bool>,
    /// Floor for metric sample rates (`dogstatsd_minimum_sample_rate`).
    pub minimum_sample_rate: Option<f64>,
    /// Mapper string interner entry count (`dogstatsd_mapper_string_interner_size`).
    pub mapper_string_interner_size: Option<u64>,
    /// Aggregation window size, in seconds (`aggregate_window_duration_seconds`).
    pub aggregate_window_duration_seconds: Option<u64>,
    /// Maximum contexts per aggregation window (`aggregate_context_limit`).
    pub aggregate_context_limit: Option<usize>,
    /// Aggregator flush period (`aggregate_flush_interval`).
    pub aggregate_flush_interval: Option<Duration>,
    /// Whether open aggregation windows are flushed (`aggregate_flush_open_windows`).
    pub aggregate_flush_open_windows: Option<bool>,
    /// Passthrough idle flush delay (`aggregate_passthrough_idle_flush_timeout`).
    pub aggregate_passthrough_idle_flush_timeout: Option<Duration>,
}

/// OTLP receiver and context Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpSalukiOnly {
    /// Whether to allow heap allocations for OTLP contexts (`otlp_allow_context_heap_allocs`).
    pub allow_context_heap_allocs: Option<bool>,
    /// Maximum cached OTLP metric contexts (`otlp_cached_contexts_limit`).
    pub cached_contexts_limit: Option<usize>,
    /// Maximum cached OTLP tagsets (`otlp_cached_tagsets_limit`).
    pub cached_tagsets_limit: Option<usize>,
    /// OTLP context interner entry count (`otlp_string_interner_size`).
    pub string_interner_size: Option<u64>,
    /// OTLP HTTP receiver transport (`otlp_config.receiver.protocols.http.transport`).
    pub receiver_http_transport: Option<String>,
}

/// APM trace Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TracesSalukiOnly {
    /// Default trace environment (`apm_config.default_env`).
    pub default_env: Option<String>,
    /// Whether error sampling is enabled (`apm_config.error_sampling_enabled`).
    pub error_sampling_enabled: Option<bool>,
    /// Rare sampler knobs (`apm_config.rare_sampler.*`).
    pub rare_sampler: RareSamplerSalukiOnly,
    /// SQL obfuscation knobs (`apm_config.obfuscation.sql.*`).
    pub sql_obfuscation: SqlObfuscationSalukiOnly,
    /// OTLP trace knobs (`otlp_config.traces.*`).
    pub otlp: OtlpTracesSalukiOnly,
    /// OTTL span-drop filter (`ottl_filter_config`).
    pub ottl_filter_config: Option<OttlConfigSalukiOnly>,
    /// OTTL span-transform processor (`ottl_transform_config`).
    pub ottl_transform_config: Option<OttlConfigSalukiOnly>,
}

/// Rare-sampler Saluki-schema-only knobs (`apm_config.rare_sampler.*`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RareSamplerSalukiOnly {
    /// Tracked-signature cardinality (`apm_config.rare_sampler.cardinality`).
    pub cardinality: Option<usize>,
    /// Cooldown between rare-sample emissions (`apm_config.rare_sampler.cooldown`).
    pub cooldown: Option<f64>,
    /// Rare-sample traces-per-second budget (`apm_config.rare_sampler.tps`).
    pub tps: Option<f64>,
}

/// SQL obfuscation Saluki-schema-only knobs (`apm_config.obfuscation.sql.*`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SqlObfuscationSalukiOnly {
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

/// OTLP trace ingestion Saluki-schema-only knobs (`otlp_config.traces.*`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OtlpTracesSalukiOnly {
    /// OTLP trace context interner entry count (`otlp_config.traces.string_interner_size`).
    pub string_interner_size: Option<u64>,
    /// Compute top-level spans by span kind
    /// (`otlp_config.traces.enable_otlp_compute_top_level_by_span_kind`).
    pub enable_compute_top_level_by_span_kind: Option<bool>,
    /// Ignore missing Datadog fields on OTLP spans (`otlp_config.traces.ignore_missing_datadog_fields`).
    pub ignore_missing_datadog_fields: Option<bool>,
}

/// One OTTL processor's Saluki-schema-only configuration (`ottl_filter_config` / `ottl_transform_config`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OttlConfigSalukiOnly {
    /// Evaluation error handling mode (`ignore` / `silent` / `propagate`).
    pub error_mode: Option<String>,
    /// OTTL span-drop conditions (`ottl_filter_config.traces.span`).
    pub span_conditions: Vec<String>,
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
    /// Absent values leave the field at its default. The Datadog `drive` writes a disjoint set of
    /// fields, so it does not matter whether `seed` runs before or after the drive.
    pub(crate) fn seed(&self, config: &mut SalukiConfiguration) {
        if let Some(v) = self.data_plane.stop_timeout {
            config.control.stop_timeout = v;
        }
        if let Some(v) = self.data_plane.checks_enabled {
            config.control.checks = v;
        }
        if let Some(v) = self.data_plane.standalone_mode {
            config.control.standalone_mode = v;
        }
        if let Some(v) = self.accounting.memory_limit.clone() {
            config.control.memory_limit = v;
        }
        if let Some(v) = self.accounting.memory_slop_factor {
            config.control.memory_slop_factor = v;
        }
        if let Some(v) = self.remote_agent_string_interner_size_bytes {
            config.control.ipc.remote_agent_string_interner_size_bytes = v;
        }

        if let Some(v) = self.metrics_level.clone() {
            config.shared.metrics_level = v;
        }
        if let Some(v) = self.encoders.flush_timeout_secs {
            config.shared.metrics_encoding.flush_timeout = Duration::from_secs(v);
        }
        if let Some(v) = self.encoders.serializer_max_metrics_per_payload {
            config.shared.metrics_encoding.max_metrics_per_payload = v;
        }
        if let Some(v) = self.encoders.v3_series_enabled {
            config.shared.metrics_encoding.v3_series_enabled = v;
        }

        let dsd = &mut config.domains.dogstatsd;
        if let Some(v) = self.dogstatsd.tcp_port {
            dsd.listeners.tcp_port = v;
        }
        if let Some(v) = self.dogstatsd.buffer_count {
            dsd.listeners.buffer_count = v;
        }
        if let Some(v) = self.dogstatsd.buffer_count_max {
            dsd.listeners.buffer_count_max = v;
        }
        if let Some(v) = self.dogstatsd.autoscale_udp_listeners {
            dsd.listeners.autoscale_udp_listeners = v;
        }
        if let Some(v) = self.dogstatsd.permissive_decoding {
            dsd.listeners.permissive_decoding = v;
        }
        if let Some(v) = self.dogstatsd.cached_contexts_limit {
            dsd.contexts.cached_contexts_limit = v;
        }
        if let Some(v) = self.dogstatsd.cached_tagsets_limit {
            dsd.contexts.cached_tagsets_limit = v;
        }
        if let Some(v) = self.dogstatsd.string_interner_size_bytes {
            dsd.contexts.string_interner_size_bytes = Some(v);
        }
        if let Some(v) = self.dogstatsd.allow_context_heap_allocs {
            dsd.contexts.allow_context_heap_allocs = v;
        }
        if let Some(v) = self.dogstatsd.minimum_sample_rate {
            dsd.contexts.minimum_sample_rate = v;
        }
        if let Some(v) = self.dogstatsd.mapper_string_interner_size {
            dsd.mapper.string_interner_size = v;
        }
        if let Some(v) = self.dogstatsd.aggregate_window_duration_seconds {
            dsd.aggregation.window_duration_seconds = v;
        }
        if let Some(v) = self.dogstatsd.aggregate_context_limit {
            dsd.aggregation.context_limit = v;
        }
        if let Some(v) = self.dogstatsd.aggregate_flush_interval {
            dsd.aggregation.flush_interval = v;
        }
        if let Some(v) = self.dogstatsd.aggregate_flush_open_windows {
            dsd.aggregation.flush_open_windows = v;
        }
        if let Some(v) = self.dogstatsd.aggregate_passthrough_idle_flush_timeout {
            dsd.aggregation.passthrough_idle_flush_timeout = v;
        }

        let otlp = &mut config.domains.otlp;
        if let Some(v) = self.otlp.allow_context_heap_allocs {
            otlp.contexts.allow_context_heap_allocs = v;
        }
        if let Some(v) = self.otlp.cached_contexts_limit {
            otlp.contexts.cached_contexts_limit = v;
        }
        if let Some(v) = self.otlp.cached_tagsets_limit {
            otlp.contexts.cached_tagsets_limit = v;
        }
        if let Some(v) = self.otlp.string_interner_size {
            otlp.contexts.string_interner_size = v;
        }
        if let Some(v) = self.otlp.receiver_http_transport.clone() {
            otlp.receiver.http.transport = v;
        }

        let traces = &mut config.domains.traces;
        if let Some(v) = self.traces.default_env.clone() {
            traces.default_env = v;
        }
        if let Some(v) = self.traces.error_sampling_enabled {
            traces.error_sampling_enabled = v;
        }
        if let Some(v) = self.traces.rare_sampler.cardinality {
            traces.rare_sampler.cardinality = v;
        }
        if let Some(v) = self.traces.rare_sampler.cooldown {
            traces.rare_sampler.cooldown = v;
        }
        if let Some(v) = self.traces.rare_sampler.tps {
            traces.rare_sampler.tps = v;
        }
        if let Some(v) = self.traces.sql_obfuscation.dbms.clone() {
            traces.obfuscation.sql.dbms = v;
        }
        if let Some(v) = self.traces.sql_obfuscation.dollar_quoted_func {
            traces.obfuscation.sql.dollar_quoted_func = v;
        }
        if let Some(v) = self.traces.sql_obfuscation.keep_sql_alias {
            traces.obfuscation.sql.keep_sql_alias = v;
        }
        if let Some(v) = self.traces.sql_obfuscation.replace_digits {
            traces.obfuscation.sql.replace_digits = v;
        }
        if let Some(v) = self.traces.sql_obfuscation.table_names {
            traces.obfuscation.sql.table_names = v;
        }
        if let Some(v) = self.traces.otlp.string_interner_size {
            traces.otlp.string_interner_size = v;
        }
        if let Some(v) = self.traces.otlp.enable_compute_top_level_by_span_kind {
            traces.otlp.enable_compute_top_level_by_span_kind = v;
        }
        if let Some(v) = self.traces.otlp.ignore_missing_datadog_fields {
            traces.otlp.ignore_missing_datadog_fields = v;
        }
        if let Some(filter) = self.traces.ottl_filter_config.clone() {
            traces.ottl_filter = OttlFilter {
                error_mode: parse_ottl_error_mode(filter.error_mode),
                span_conditions: filter.span_conditions,
            };
        }
        if let Some(transform) = self.traces.ottl_transform_config.clone() {
            traces.ottl_transform = OttlTransform {
                error_mode: parse_ottl_error_mode(transform.error_mode),
                trace_statements: transform.trace_statements,
            };
        }

        if let Some(v) = self.checks_ipc_endpoint.clone() {
            config.domains.checks.ipc_endpoint = ListenAddress(v);
        }
    }
}
