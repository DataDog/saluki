//! Component-native configuration for the OTLP source, relay, decoder, and forwarder.
//!
//! These structs mirror the OTLP-related component config in `saluki-components`, with all source
//! key names, aliases, and `Deserialize` impls stripped. The shared [`OtlpConfig`] receiver and
//! signal sub-structs are defined here and reused across the OTLP components and the traces domain.

use bytesize::ByteSize;

/// Configuration for the OTLP source component.
///
/// Mirrors `OtlpConfiguration` in `saluki-components`. The injected `workload_provider` is excluded
/// because it is runtime-injected component state, not configuration.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct OtlpSourceConfig {
    /// Receiver and per-signal OTLP configuration.
    pub otlp_config: OtlpConfig,

    /// Total size, in bytes, of the string interner used for contexts.
    ///
    /// Controls how much memory is pre-allocated for interning metric names and tags. When the
    /// interner is full, metrics with unresolved contexts may be dropped depending on
    /// `allow_context_heap_allocations`.
    pub context_string_interner_bytes: ByteSize,

    /// The maximum number of cached contexts to allow.
    ///
    /// A context is the unique combination of a metric name and its tags. This bounds the resolved
    /// context cache, not the number of live contexts. Defaults to 500,000.
    pub cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// This bounds the resolved tagset cache, not the number of live tagsets. Defaults to 500,000.
    pub cached_tagsets_limit: usize,

    /// Whether to allow heap allocations when resolving contexts.
    ///
    /// When `true`, strings that cannot be interned are allocated on the heap, which can lead to
    /// unbounded memory usage. When `false`, a metric whose name and tags cannot all be interned is
    /// skipped. Defaults to `true`.
    pub allow_context_heap_allocations: bool,
}

impl Default for OtlpSourceConfig {
    fn default() -> Self {
        Self {
            otlp_config: OtlpConfig::default(),
            context_string_interner_bytes: ByteSize::mib(2),
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            allow_context_heap_allocations: true,
        }
    }
}

/// Configuration for the OTLP relay component.
///
/// Mirrors `OtlpRelayConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpRelayConfig {
    /// Receiver configuration for the relay's OTLP listeners.
    pub receiver: Receiver,
}

/// Configuration for the OTLP decoder component.
///
/// Mirrors `OtlpDecoderConfiguration` in `saluki-components`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpDecoderConfig {
    /// Traces-specific OTLP configuration.
    pub traces: TracesConfig,
}

/// Configuration for the OTLP forwarder component.
///
/// Mirrors `OtlpForwarderConfiguration` in `saluki-components`. The original component derives the
/// gRPC endpoint from an injected argument and the internal port from config; both are retained
/// here as plain configuration values.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpForwarderConfig {
    /// The Core Agent OTLP gRPC endpoint to forward to.
    pub core_agent_otlp_grpc_endpoint: String,

    /// The internal port on the Core Agent to forward traces to.
    ///
    /// Defaults to 5003.
    pub core_agent_traces_internal_port: u16,
}

/// Top-level OTLP configuration mirroring the receiver and per-signal settings.
///
/// Mirrors `OtlpConfig` in `saluki-components`. Shared by the OTLP source and the traces encoder.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct OtlpConfig {
    /// OTLP receiver configuration.
    pub receiver: Receiver,

    /// Metrics-specific OTLP configuration.
    pub metrics: MetricsConfig,

    /// Logs-specific OTLP configuration.
    pub logs: LogsConfig,

    /// Traces-specific OTLP configuration.
    pub traces: TracesConfig,
}

/// Receiver configuration for OTLP endpoints.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Receiver {
    /// Protocol-specific receiver configuration.
    pub protocols: Protocols,
}

/// Protocol configuration for the OTLP receiver.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Protocols {
    /// gRPC protocol configuration.
    pub grpc: GrpcConfig,

    /// HTTP protocol configuration.
    pub http: HttpConfig,
}

/// gRPC receiver configuration.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GrpcConfig {
    /// The gRPC endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4317`.
    pub endpoint: String,

    /// The transport protocol to use for the gRPC listener.
    ///
    /// Defaults to `tcp`.
    pub transport: String,

    /// Maximum size, in MiB, of a gRPC message that can be received.
    ///
    /// Defaults to 4 MiB.
    pub max_recv_msg_size_mib: u64,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            endpoint: "0.0.0.0:4317".to_string(),
            transport: "tcp".to_string(),
            max_recv_msg_size_mib: 4,
        }
    }
}

/// HTTP receiver configuration.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct HttpConfig {
    /// The HTTP endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4318`.
    pub endpoint: String,

    /// The transport protocol to use for the HTTP listener.
    ///
    /// Defaults to `tcp`.
    pub transport: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            endpoint: "0.0.0.0:4318".to_string(),
            transport: "tcp".to_string(),
        }
    }
}

/// Configuration for OTLP metrics processing.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Configuration for OTLP logs processing.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LogsConfig {
    /// Whether to enable OTLP logs support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Configuration for OTLP traces processing.
///
/// Mirrors `TracesConfig` in `saluki-components`. Shared by the OTLP source/decoder, the traces
/// encoder, and the trace sampler.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TracesConfig {
    /// Whether to enable OTLP traces support.
    ///
    /// Defaults to `true`.
    pub enabled: bool,

    /// Whether to skip deriving Datadog fields from standard OTLP attributes.
    ///
    /// When `true`, only explicit `datadog.*` prefixed attributes are used and fallback resolution
    /// from OTLP semantic conventions is skipped. Defaults to `false`.
    pub ignore_missing_datadog_fields: bool,

    /// Whether `_top_level` and `_dd.measured` are derived using the OTLP span kind.
    ///
    /// Defaults to `true`.
    pub enable_otlp_compute_top_level_by_span_kind: bool,

    /// Probabilistic sampler configuration for OTLP traces.
    pub probabilistic_sampler: ProbabilisticSampler,

    /// Total size, in bytes, of the string interner used for OTLP traces.
    ///
    /// Defaults to 512 KiB.
    pub string_interner_bytes: ByteSize,

    /// The internal port on the Core Agent to forward traces to.
    ///
    /// Defaults to 5003.
    pub internal_port: u16,
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: true,
            probabilistic_sampler: ProbabilisticSampler::default(),
            string_interner_bytes: ByteSize::kib(512),
            internal_port: 5003,
        }
    }
}

/// Configuration for OTLP traces probabilistic sampling.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ProbabilisticSampler {
    /// Percentage of traces to ingest, in the range (0, 100].
    ///
    /// Invalid values (`<= 0` or `> 100`) are disregarded and the default is used. Defaults to
    /// 100.0 (100% sampling).
    pub sampling_percentage: f64,
}

impl Default for ProbabilisticSampler {
    fn default() -> Self {
        Self {
            sampling_percentage: 100.0,
        }
    }
}
