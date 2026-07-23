//! OTLP domain: the OTLP receiver (gRPC/HTTP transports, logs/metrics activation), the OTLP proxy
//! gating, and OTLP context sizing. OTLP trace handling lives in the `traces` domain.

use std::str::FromStr;

use serde::Serialize;

use crate::Error;

/// Resolved OTLP configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// OTLP receiver transports and per-signal activation.
    pub receiver: Receiver,

    /// OTLP metrics translation settings.
    pub metrics: Metrics,

    /// OTLP trace ingestion settings.
    pub traces: Traces,

    /// OTLP proxy gating and endpoint.
    pub proxy: Proxy,

    /// OTLP context cache sizing.
    pub contexts: Contexts,
}

/// OTLP metrics translation settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Metrics {
    /// How explicit histogram buckets are reported.
    pub histogram_mode: HistogramMode,

    /// Whether histogram count, sum, minimum, and maximum metrics are emitted when available.
    ///
    /// The `nobuckets` mode requires this setting. Defaults to `false`.
    pub send_histogram_aggregations: bool,

    /// Whether every resource attribute is added as a raw metric tag, in addition to the
    /// semantic-convention mappings that are always applied.
    pub resource_attributes_as_tags: bool,

    /// OTLP sum translation settings.
    pub sums: Sums,

    /// Comma-separated list of tags to add to every emitted metric.
    pub tags: String,

    /// OTLP summary translation settings.
    pub summaries: Summaries,
}

/// How explicit OTLP histogram buckets are reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum HistogramMode {
    /// Omit bucket metrics.
    NoBuckets,

    /// Report each bucket as a counter.
    Counters,

    /// Report buckets as distributions.
    #[default]
    Distributions,
}

impl FromStr for HistogramMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "nobuckets" => Ok(Self::NoBuckets),
            "counters" => Ok(Self::Counters),
            "distributions" => Ok(Self::Distributions),
            other => Err(Error::new_without_source(format!(
                "unknown histogram mode `{other}`; expected `nobuckets`, `counters`, or `distributions`"
            ))),
        }
    }
}

/// How cumulative monotonic sums are reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CumulativeMonotonicMode {
    /// Converts cumulative values to deltas and reports them as counts.
    #[default]
    ToDelta,

    /// Reports cumulative values as gauges without converting them to deltas.
    RawValue,
}

impl FromStr for CumulativeMonotonicMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "to_delta" => Ok(Self::ToDelta),
            "raw_value" => Ok(Self::RawValue),
            other => Err(Error::new_without_source(format!(
                "unknown cumulative monotonic sum mode `{other}`; expected `to_delta` or `raw_value`"
            ))),
        }
    }
}

/// Controls how the first value of a cumulative monotonic sum is reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum InitialCumulativeMonotonicValue {
    /// Reports the first value when its series started after the translator process.
    #[default]
    Auto,

    /// Always drops the first value.
    Drop,

    /// Always reports the first value.
    Keep,
}

impl FromStr for InitialCumulativeMonotonicValue {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "drop" => Ok(Self::Drop),
            "keep" => Ok(Self::Keep),
            other => Err(Error::new_without_source(format!(
                "unknown initial cumulative monotonic value `{other}`; expected `auto`, `drop`, or `keep`"
            ))),
        }
    }
}

/// OTLP sum translation settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sums {
    /// Cumulative monotonic sum reporting mode.
    ///
    /// Defaults to `to_delta`, which converts cumulative values to delta counts. Set to `raw_value` to emit
    /// cumulative values as gauges.
    pub cumulative_monotonic_mode: CumulativeMonotonicMode,

    /// Initial cumulative monotonic sum reporting behavior.
    ///
    /// Defaults to `auto`, which reports the value only when its series started after the translator process.
    /// Set this to `drop` to always discard the first value or `keep` to always report it.
    pub initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue,
}

/// OTLP summary translation settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Summaries {
    /// How summary quantiles are reported.
    ///
    /// Defaults to `gauges`, which emits one gauge metric per quantile. Set to `noquantiles` to omit quantile
    /// metrics.
    pub mode: SummaryMode,
}

/// How OTLP summary quantiles are reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum SummaryMode {
    /// Report one gauge metric per quantile.
    #[default]
    Gauges,

    /// Omit quantile metrics.
    NoQuantiles,
}

impl FromStr for SummaryMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "gauges" => Ok(Self::Gauges),
            "noquantiles" => Ok(Self::NoQuantiles),
            other => Err(Error::new_without_source(format!(
                "unknown summary mode `{other}`; expected `gauges` or `noquantiles`"
            ))),
        }
    }
}

/// OTLP receiver transports and per-signal activation.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Receiver {
    /// Whether the receiver accepts OTLP logs.
    pub logs_enabled: bool,

    /// Whether the receiver accepts OTLP metrics.
    pub metrics_enabled: bool,

    /// gRPC receiver settings.
    pub grpc: GrpcReceiver,

    /// HTTP receiver settings.
    pub http: HttpReceiver,
}

/// Default gRPC maximum inbound message size, in MiB.
///
/// The Datadog schema default for `max_recv_msg_size_mib` is `0`, which grpc-go treats as "apply the
/// built-in 4 MiB limit". Translation substitutes this constant for a configured `0` so the model
/// always carries an effective limit.
pub const DEFAULT_GRPC_MAX_RECV_MSG_SIZE_MIB: u64 = 4;

/// OTLP gRPC receiver.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GrpcReceiver {
    /// Address the gRPC receiver listens on.
    pub endpoint: String,

    /// Maximum inbound message size, in MiB.
    pub max_recv_msg_size_mib: u64,

    /// Transport the gRPC receiver binds (for example, `tcp` or `unix`).
    pub transport: String,
}

/// OTLP HTTP receiver.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HttpReceiver {
    /// Address the HTTP receiver listens on.
    pub endpoint: String,

    /// Transport the HTTP receiver binds (for example, `tcp` or `unix`). (not in Datadog Agent
    /// config schema)
    pub transport: String,
}

impl Default for HttpReceiver {
    fn default() -> Self {
        Self {
            // Witnessed; overwritten during drive.
            endpoint: String::new(),
            transport: "tcp".to_string(),
        }
    }
}

/// OTLP trace ingestion settings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Traces {
    /// Whether OTLP trace ingestion is enabled.
    pub enabled: bool,

    /// Internal port the OTLP trace receiver forwards to.
    pub internal_port: u16,

    /// Percentage of OTLP traces the probabilistic sampler keeps.
    pub probabilistic_sampler_sampling_percentage: f64,

    /// Size, in bytes, of the OTLP trace context interner. (not in Datadog Agent config schema)
    pub string_interner_size: u64,

    /// Whether top-level spans are computed from span kind on OTLP traces. (not in Datadog Agent
    /// config schema)
    pub enable_compute_top_level_by_span_kind: bool,

    /// Whether spans missing intake-required fields are ingested rather than rejected. (not in
    /// Datadog Agent config schema)
    pub ignore_missing_datadog_fields: bool,
}

impl Default for Traces {
    fn default() -> Self {
        Self {
            enabled: false,
            internal_port: 0,
            probabilistic_sampler_sampling_percentage: 0.0,
            string_interner_size: 512 * 1024,
            enable_compute_top_level_by_span_kind: true,
            ignore_missing_datadog_fields: false,
        }
    }
}

/// OTLP proxy gating: which signals the proxy forwards, and the proxy receiver endpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Proxy {
    /// Whether the OTLP proxy is enabled.
    pub enabled: bool,

    /// Whether the proxy forwards logs.
    pub logs_enabled: bool,

    /// Whether the proxy forwards metrics.
    pub metrics_enabled: bool,

    /// Whether the proxy forwards traces.
    pub traces_enabled: bool,

    /// Address the proxy's gRPC receiver listens on.
    pub grpc_endpoint: String,
}

/// OTLP context cache sizing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Contexts {
    /// Whether contexts may be heap-allocated when the interner is full. (not in Datadog Agent
    /// config schema)
    pub allow_context_heap_allocs: bool,

    /// Maximum number of metric contexts held in the cache. (not in Datadog Agent config schema)
    pub cached_contexts_limit: usize,

    /// Maximum number of tagsets held in the cache. (not in Datadog Agent config schema)
    pub cached_tagsets_limit: usize,

    /// Size, in bytes, of the context string interner. (not in Datadog Agent config schema)
    pub string_interner_size: u64,
}

impl Default for Contexts {
    fn default() -> Self {
        Self {
            allow_context_heap_allocs: true,
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            string_interner_size: 2 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CumulativeMonotonicMode, InitialCumulativeMonotonicValue};

    #[test]
    fn cumulative_monotonic_mode_parses_known_values() {
        assert_eq!(
            "to_delta"
                .parse::<CumulativeMonotonicMode>()
                .expect("to_delta should parse"),
            CumulativeMonotonicMode::ToDelta
        );
        assert_eq!(
            "raw_value"
                .parse::<CumulativeMonotonicMode>()
                .expect("raw_value should parse"),
            CumulativeMonotonicMode::RawValue
        );
    }

    #[test]
    fn cumulative_monotonic_mode_rejects_unknown_values() {
        let error = "unsupported"
            .parse::<CumulativeMonotonicMode>()
            .expect_err("unsupported mode should be rejected");

        assert_eq!(
            error.to_string(),
            "unknown cumulative monotonic sum mode `unsupported`; expected `to_delta` or `raw_value`"
        );
    }

    #[test]
    fn initial_cumulative_monotonic_value_parses_known_values() {
        for (value, expected) in [
            ("auto", InitialCumulativeMonotonicValue::Auto),
            ("drop", InitialCumulativeMonotonicValue::Drop),
            ("keep", InitialCumulativeMonotonicValue::Keep),
        ] {
            assert_eq!(
                value
                    .parse::<InitialCumulativeMonotonicValue>()
                    .expect("known value should parse"),
                expected
            );
        }
    }

    #[test]
    fn initial_cumulative_monotonic_value_rejects_unknown_values() {
        let error = "unsupported"
            .parse::<InitialCumulativeMonotonicValue>()
            .expect_err("unsupported value should be rejected");

        assert_eq!(
            error.to_string(),
            "unknown initial cumulative monotonic value `unsupported`; expected `auto`, `drop`, or `keep`"
        );
    }
}
