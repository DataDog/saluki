//! [`SalukiOnlyConfiguration`]: the parsed Saluki-schema-only source input, plus [`seed`].
//!
//! Saluki-schema-only keys are those the Datadog Agent schema does not publish. Their authority is
//! Saluki's own source (`SALUKI_*` / `saluki.yaml`), so these structs DO derive `Deserialize` and
//! `Default`. The authoritative inventory of these keys lives in
//! `lib/datadog-agent/config-overlay-model/src/saluki_keys.rs`; the sub-structs below mirror that
//! set, grouped by subsystem.
//!
//! [`SalukiOnlyConfiguration::seed`] produces a base [`SalukiConfiguration`]: it starts from
//! `SalukiConfiguration::default()` and assigns each Saluki-only field into its component-native
//! destination. The Datadog `drive` later overlays its disjoint schema fields on top of this base.
//!
//! [`seed`]: SalukiOnlyConfiguration::seed

use std::num::NonZeroU64;
use std::time::Duration;

use bytesize::ByteSize;

use crate::model::SalukiConfiguration;

/// The parsed Saluki-schema-only configuration, grouped by subsystem.
///
/// Parsed from `SALUKI_*` / `saluki.yaml`. Every field corresponds to a key in `SALUKI_KEYS`.
///
/// Every field is `#[serde(default)]`: the Saluki source is frequently absent entirely (a
/// deployment may set no `SALUKI_*` keys), so each subsystem group must fall back to its defaults
/// independently when its keys are missing.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct SalukiOnlyConfiguration {
    /// Cross-cutting / data-plane control knobs.
    pub data_plane: DataPlaneSalukiOnly,

    /// DogStatsD source knobs.
    pub dogstatsd: DogStatsDSalukiOnly,

    /// DogStatsD mapper knobs.
    pub dogstatsd_mapper: DogStatsDMapperSalukiOnly,

    /// Aggregate transform knobs.
    pub aggregate: AggregateSalukiOnly,

    /// OTLP knobs.
    pub otlp: OtlpSalukiOnly,

    /// Trace obfuscation knobs.
    pub trace_obfuscation: TraceObfuscationSalukiOnly,

    /// Encoder knobs shared across metrics/traces/APM-stats encoders.
    pub encoders: EncodersSalukiOnly,

    /// Process memory-accounting knobs.
    pub accounting: AccountingSalukiOnly,
}

/// Cross-cutting data-plane control knobs (`data_plane.*`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DataPlaneSalukiOnly {
    /// ADP graceful shutdown timeout, in seconds (`data_plane.stop_timeout`).
    pub stop_timeout_secs: Option<u64>,
}

/// DogStatsD source Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DogStatsDSalukiOnly {
    /// Whether to allow heap allocations for contexts (`dogstatsd_allow_context_heap_allocs`).
    pub allow_context_heap_allocs: Option<bool>,

    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT` (`dogstatsd_autoscale_udp_listeners`).
    pub autoscale_udp_listeners: Option<bool>,

    /// Number of receive buffers (`dogstatsd_buffer_count`).
    pub buffer_count: Option<usize>,

    /// Max cached metric contexts (`dogstatsd_cached_contexts_limit`).
    pub cached_contexts_limit: Option<usize>,

    /// Max cached tagsets (`dogstatsd_cached_tagsets_limit`).
    pub cached_tagsets_limit: Option<usize>,

    /// Floor for metric sample rates (`dogstatsd_minimum_sample_rate`).
    pub minimum_sample_rate: Option<f64>,

    /// Whether to relax decoder strictness (`dogstatsd_permissive_decoding`).
    pub permissive_decoding: Option<bool>,

    /// Explicit byte budget for the context interner (`dogstatsd_string_interner_size_bytes`).
    pub string_interner_size_bytes: Option<u64>,

    /// TCP listen port for DogStatsD (`dogstatsd_tcp_port`).
    pub tcp_port: Option<u16>,
}

/// DogStatsD mapper Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DogStatsDMapperSalukiOnly {
    /// Mapper string interner capacity, in bytes (`dogstatsd_mapper_string_interner_size`).
    pub string_interner_size: Option<u64>,
}

/// Aggregate transform Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AggregateSalukiOnly {
    /// Aggregation window size, in seconds (`aggregate_window_duration_seconds`).
    pub window_duration_seconds: Option<NonZeroU64>,

    /// Aggregator flush period (`aggregate_flush_interval`).
    pub flush_interval: Option<Duration>,

    /// Max contexts per aggregation window (`aggregate_context_limit`).
    pub context_limit: Option<usize>,

    /// Idle counter keep-alive duration, in seconds (`counter_expiry_seconds`).
    pub counter_expiry_seconds: Option<u64>,

    /// Passthrough buffer flush delay (`aggregate_passthrough_idle_flush_timeout`).
    pub passthrough_idle_flush_timeout: Option<Duration>,
}

/// OTLP Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct OtlpSalukiOnly {
    /// Enable OTLP top-level-by-span-kind
    /// (`otlp_config.traces.enable_otlp_compute_top_level_by_span_kind`).
    pub traces_enable_otlp_compute_top_level_by_span_kind: Option<bool>,

    /// Ignore missing Datadog fields in OTLP (`otlp_config.traces.ignore_missing_datadog_fields`).
    pub traces_ignore_missing_datadog_fields: Option<bool>,

    /// OTLP trace string interner capacity, in bytes (`otlp_config.traces.string_interner_size`).
    pub traces_string_interner_size: Option<u64>,

    /// OTLP HTTP receiver transport (`otlp_config.receiver.protocols.http.transport`).
    pub receiver_http_transport: Option<String>,

    /// Whether to allow heap allocations for OTLP contexts (`otlp_allow_context_heap_allocs`).
    pub allow_context_heap_allocs: Option<bool>,

    /// Max cached OTLP metric contexts (`otlp_cached_contexts_limit`).
    pub cached_contexts_limit: Option<usize>,

    /// Max cached OTLP tagsets (`otlp_cached_tagsets_limit`).
    pub cached_tagsets_limit: Option<usize>,

    /// OTLP context interner capacity, in bytes (`otlp_string_interner_size`).
    pub string_interner_size: Option<u64>,
}

/// Trace obfuscation Saluki-schema-only knobs (`apm_config.obfuscation.sql.*`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct TraceObfuscationSalukiOnly {
    /// SQL obfuscation DBMS dialect (`apm_config.obfuscation.sql.dbms`).
    pub sql_dbms: Option<String>,

    /// Preserve dollar-quoted SQL functions (`apm_config.obfuscation.sql.dollar_quoted_func`).
    pub sql_dollar_quoted_func: Option<bool>,

    /// Preserve SQL aliases in obfuscation (`apm_config.obfuscation.sql.keep_sql_alias`).
    pub sql_keep_sql_alias: Option<bool>,

    /// Replace digits in SQL obfuscation (`apm_config.obfuscation.sql.replace_digits`).
    pub sql_replace_digits: Option<bool>,

    /// Collect table names during obfuscation (`apm_config.obfuscation.sql.table_names`).
    pub sql_table_names: Option<bool>,
}

/// Encoder Saluki-schema-only knobs shared across encoders.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct EncodersSalukiOnly {
    /// Encoder flush timeout, in seconds (`flush_timeout_secs`).
    ///
    /// Shared by the metrics encoder, the APM stats encoder, and the traces encoder.
    pub flush_timeout_secs: Option<u64>,

    /// Max metrics per payload (`serializer_max_metrics_per_payload`).
    pub serializer_max_metrics_per_payload: Option<usize>,
}

/// Process memory-accounting Saluki-schema-only knobs.
///
/// These keys (`memory_limit`, `memory_slop_factor`) drive the process-wide memory limiter, which is
/// cross-cutting infrastructure rather than a single component or a topology gate. There is no leaf
/// destination for them yet, so they are parsed here and not assigned in [`SalukiOnlyConfiguration::seed`].
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AccountingSalukiOnly {
    /// Process memory limit (`memory_limit`).
    pub memory_limit: Option<String>,

    /// Memory accounting slop fraction (`memory_slop_factor`).
    pub memory_slop_factor: Option<f64>,
}

impl SalukiOnlyConfiguration {
    /// Produces a base [`SalukiConfiguration`] seeded with defaults and Saluki-schema-only values.
    ///
    /// Starts from `SalukiConfiguration::default()` and assigns each present Saluki-only value into
    /// its component-native destination. Absent values leave the default in place. The Datadog
    /// `drive` later overlays its disjoint schema fields on top of this base; a Saluki-only field
    /// and a Datadog-schema field never target the same destination.
    pub fn seed(&self) -> SalukiConfiguration {
        let mut c = SalukiConfiguration::default();

        // ----- data_plane (control) -----
        if let Some(secs) = self.data_plane.stop_timeout_secs {
            c.control.stop_timeout = Duration::from_secs(secs);
        }

        // ----- dogstatsd source -----
        let dsd = &mut c.components.dogstatsd.source;
        if let Some(v) = self.dogstatsd.allow_context_heap_allocs {
            dsd.allow_context_heap_allocations = v;
        }
        if let Some(v) = self.dogstatsd.autoscale_udp_listeners {
            dsd.autoscale_udp_listeners = v;
        }
        if let Some(v) = self.dogstatsd.buffer_count {
            dsd.buffer_count = v;
        }
        if let Some(v) = self.dogstatsd.cached_contexts_limit {
            dsd.cached_contexts_limit = v;
        }
        if let Some(v) = self.dogstatsd.cached_tagsets_limit {
            dsd.cached_tagsets_limit = v;
        }
        if let Some(v) = self.dogstatsd.minimum_sample_rate {
            dsd.minimum_sample_rate = v;
        }
        if let Some(v) = self.dogstatsd.permissive_decoding {
            dsd.permissive_decoding = v;
        }
        if let Some(v) = self.dogstatsd.string_interner_size_bytes {
            dsd.context_string_interner_size_bytes = Some(ByteSize(v));
        }
        if let Some(v) = self.dogstatsd.tcp_port {
            dsd.tcp_port = v;
        }

        // ----- dogstatsd mapper -----
        if let Some(v) = self.dogstatsd_mapper.string_interner_size {
            c.components.dogstatsd.mapper.context_string_interner_bytes = ByteSize(v);
        }

        // ----- aggregate -----
        let agg = &mut c.components.dogstatsd.aggregate;
        if let Some(v) = self.aggregate.window_duration_seconds {
            agg.window_duration_seconds = v;
        }
        if let Some(v) = self.aggregate.flush_interval {
            agg.primary_flush_interval = v;
        }
        if let Some(v) = self.aggregate.context_limit {
            agg.context_limit = v;
        }
        if let Some(v) = self.aggregate.counter_expiry_seconds {
            agg.counter_expiry_seconds = Some(v);
        }
        if let Some(v) = self.aggregate.passthrough_idle_flush_timeout {
            agg.passthrough_idle_flush_timeout = v;
        }

        // ----- otlp -----
        let otlp_source = &mut c.components.otlp.source;
        if let Some(v) = self.otlp.allow_context_heap_allocs {
            otlp_source.allow_context_heap_allocations = v;
        }
        if let Some(v) = self.otlp.cached_contexts_limit {
            otlp_source.cached_contexts_limit = v;
        }
        if let Some(v) = self.otlp.cached_tagsets_limit {
            otlp_source.cached_tagsets_limit = v;
        }
        if let Some(v) = self.otlp.string_interner_size {
            otlp_source.context_string_interner_bytes = ByteSize(v);
        }
        // The OTLP traces knobs land on the source's embedded `otlp_config.traces`; the decoder and
        // traces encoder carry their own `TracesConfig` copies that the translator keeps in sync.
        let otlp_traces = &mut c.components.otlp.source.otlp_config.traces;
        if let Some(v) = self.otlp.traces_enable_otlp_compute_top_level_by_span_kind {
            otlp_traces.enable_otlp_compute_top_level_by_span_kind = v;
        }
        if let Some(v) = self.otlp.traces_ignore_missing_datadog_fields {
            otlp_traces.ignore_missing_datadog_fields = v;
        }
        if let Some(v) = self.otlp.traces_string_interner_size {
            otlp_traces.string_interner_bytes = ByteSize(v);
        }
        if let Some(v) = self.otlp.receiver_http_transport.clone() {
            c.components.otlp.source.otlp_config.receiver.protocols.http.transport = v;
        }

        // ----- trace obfuscation -----
        let sql = &mut c.components.traces.obfuscation.config.sql;
        if let Some(v) = self.trace_obfuscation.sql_dbms.clone() {
            sql.dbms = v;
        }
        if let Some(v) = self.trace_obfuscation.sql_dollar_quoted_func {
            sql.dollar_quoted_func = v;
        }
        if let Some(v) = self.trace_obfuscation.sql_keep_sql_alias {
            sql.keep_sql_alias = v;
        }
        if let Some(v) = self.trace_obfuscation.sql_replace_digits {
            sql.replace_digits = v;
        }
        if let Some(v) = self.trace_obfuscation.sql_table_names {
            sql.table_names = v;
        }

        // ----- encoders -----
        if let Some(v) = self.encoders.flush_timeout_secs {
            c.components.metrics.datadog_encoder.flush_timeout_secs = v;
            c.components.metrics.apm_stats_encoder.flush_timeout_secs = v;
            c.components.traces.encoder.flush_timeout_secs = v;
        }
        if let Some(v) = self.encoders.serializer_max_metrics_per_payload {
            c.components.metrics.datadog_encoder.max_metrics_per_payload = v;
        }

        // ----- accounting -----
        // TODO(translate): `memory_limit` / `memory_slop_factor` drive the process-wide memory
        // limiter. There is no component-native or control destination for them yet, so they are
        // parsed (see `AccountingSalukiOnly`) but not assigned here.

        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_seed_equals_default() {
        let seeded = SalukiOnlyConfiguration::default().seed();
        assert_eq!(seeded, SalukiConfiguration::default());
    }

    #[test]
    fn seed_assigns_into_native_destinations() {
        let mut only = SalukiOnlyConfiguration::default();
        only.dogstatsd.tcp_port = Some(9000);
        only.dogstatsd.cached_contexts_limit = Some(42);
        only.otlp.string_interner_size = Some(1024);
        only.encoders.flush_timeout_secs = Some(7);
        only.data_plane.stop_timeout_secs = Some(30);

        let seeded = only.seed();

        assert_eq!(seeded.components.dogstatsd.source.tcp_port, 9000);
        assert_eq!(seeded.components.dogstatsd.source.cached_contexts_limit, 42);
        assert_eq!(
            seeded.components.otlp.source.context_string_interner_bytes,
            ByteSize(1024)
        );
        assert_eq!(seeded.components.metrics.datadog_encoder.flush_timeout_secs, 7);
        assert_eq!(seeded.components.traces.encoder.flush_timeout_secs, 7);
        assert_eq!(seeded.control.stop_timeout, Duration::from_secs(30));
    }

    #[test]
    fn saluki_configuration_serializes() {
        // The `/config/internal` view relies on `SalukiConfiguration: Serialize`.
        let json = serde_json::to_string(&SalukiConfiguration::default()).expect("serialize");
        assert!(json.contains("control"));
        assert!(json.contains("components"));
    }
}
