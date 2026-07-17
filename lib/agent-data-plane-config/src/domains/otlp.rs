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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metrics {
    /// How explicit histogram buckets are reported.
    pub histogram_mode: HistogramMode,

    /// Whether every resource attribute is added as a raw metric tag, in addition to the
    /// semantic-convention mappings that are always applied.
    pub resource_attributes_as_tags: bool,

    /// Cumulative monotonic sum reporting mode.
    ///
    /// Defaults to `to_delta`, which converts cumulative values to delta counts. Set to `raw_value` to emit
    /// cumulative values as gauges.
    pub cumulative_monotonic_mode: String,

    /// Comma-separated list of tags to add to every emitted metric.
    pub tags: String,
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

impl Default for Metrics {
    fn default() -> Self {
        Self {
            histogram_mode: HistogramMode::default(),
            resource_attributes_as_tags: false,
            cumulative_monotonic_mode: "to_delta".to_string(),
            tags: String::new(),
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
