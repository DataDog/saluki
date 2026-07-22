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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HttpReceiver {
    /// Address the HTTP receiver listens on.
    pub endpoint: String,

    /// Transport the HTTP receiver binds (for example, `tcp` or `unix`). (not in Datadog Agent
    /// config schema)
    pub transport: String,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Contexts {
    /// Whether contexts may be heap-allocated when the interner is full. (not in Datadog Agent
    /// config schema)
    pub allow_context_heap_allocs: bool,

    /// Maximum number of metric contexts held in the cache. (not in Datadog Agent config schema)
    pub cached_contexts_limit: usize,

    /// Maximum number of tagsets held in the cache. (not in Datadog Agent config schema)
    pub cached_tagsets_limit: usize,

    /// Number of entries the context string interner holds. (not in Datadog Agent config schema)
    pub string_interner_size: u64,
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
