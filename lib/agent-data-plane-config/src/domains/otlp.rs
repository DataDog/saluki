//! OTLP domain: the OTLP receiver (gRPC/HTTP transports, logs/metrics activation), the OTLP proxy
//! gating, and OTLP context sizing. OTLP trace handling lives in the `traces` domain.

use serde::Serialize;

/// Default transport for the OTLP HTTP receiver. (not in the Datadog Agent config schema)
pub fn default_otlp_http_receiver_transport() -> String {
    "tcp".to_owned()
}

/// Default byte budget for the OTLP metric context string interner (2 MiB). (not in the Datadog
/// Agent config schema)
pub const fn default_otlp_context_string_interner_size() -> u64 {
    2 * 1024 * 1024
}

/// Default maximum number of cached OTLP metric contexts. (not in the Datadog Agent config schema)
pub const fn default_otlp_cached_contexts_limit() -> usize {
    500_000
}

/// Default maximum number of cached OTLP tagsets. (not in the Datadog Agent config schema)
pub const fn default_otlp_cached_tagsets_limit() -> usize {
    500_000
}

/// Default for whether OTLP metric contexts may be heap-allocated when the interner is full. (not in
/// the Datadog Agent config schema)
pub const fn default_otlp_allow_context_heap_allocs() -> bool {
    true
}

/// Resolved OTLP configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// OTLP receiver transports and per-signal activation.
    pub receiver: Receiver,

    /// OTLP proxy gating and endpoint.
    pub proxy: Proxy,

    /// OTLP context cache sizing.
    pub contexts: Contexts,
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
            // Witnessed key: overwritten by the Datadog driver, so this is a placeholder.
            endpoint: String::default(),
            // Saluki-schema-only key: seeded, so this default must match what the OTLP components
            // expect when the key is absent.
            transport: default_otlp_http_receiver_transport(),
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
///
/// Every field here is a Saluki-schema-only knob (seeded), so the defaults below must match what the
/// OTLP source expects when the keys are absent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Contexts {
    /// Whether contexts may be heap-allocated when the interner is full. (not in Datadog Agent
    /// config schema)
    pub allow_context_heap_allocs: bool,

    /// Maximum number of metric contexts held in the cache. (not in Datadog Agent config schema)
    pub cached_contexts_limit: usize,

    /// Maximum number of tagsets held in the cache. (not in Datadog Agent config schema)
    pub cached_tagsets_limit: usize,

    /// Byte budget of the context string interner. (not in Datadog Agent config schema)
    pub string_interner_size: u64,
}

impl Default for Contexts {
    fn default() -> Self {
        Self {
            allow_context_heap_allocs: default_otlp_allow_context_heap_allocs(),
            cached_contexts_limit: default_otlp_cached_contexts_limit(),
            cached_tagsets_limit: default_otlp_cached_tagsets_limit(),
            string_interner_size: default_otlp_context_string_interner_size(),
        }
    }
}
