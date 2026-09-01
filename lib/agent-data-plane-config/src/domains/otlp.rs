//! OTLP domain: the OTLP receiver (gRPC/HTTP transports, logs/metrics activation), the OTLP proxy
//! gating, and OTLP context sizing. OTLP trace handling lives in the `traces` domain.

use std::time::Duration;
use std::{num::NonZeroUsize, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::defaults::DEFAULT_STRING_INTERNER_SIZE_BYTES;
use crate::{domains::dogstatsd::OriginTagCardinality, Error};

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

/// Default TTL for cached prior points used when converting cumulative sums to deltas.
pub const DEFAULT_DELTA_TTL: Duration = Duration::from_secs(3600);

/// OTLP metrics translation settings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metrics {
    /// Tag cardinality applied to entity and global tags enriched onto OTLP metrics.
    ///
    /// Defaults to `low`. Set this to `orchestrator` or `high` when the additional series cardinality is acceptable.
    /// `none` disables entity and global tag enrichment.
    pub tag_cardinality: OriginTagCardinality,

    /// How explicit histogram buckets are reported.
    pub histogram_mode: HistogramMode,

    /// Whether histogram count, sum, minimum, and maximum metrics are emitted when available.
    ///
    /// The `nobuckets` mode requires this setting. Defaults to `false`.
    pub send_histogram_aggregations: bool,

    /// Whether every resource attribute is added as a raw metric tag, in addition to the
    /// semantic-convention mappings that are always applied.
    pub resource_attributes_as_tags: bool,

    /// Whether instrumentation scope name, version, and attributes are added as metric tags.
    ///
    /// Defaults to `true`. When `false`, no scope tags are emitted (no `n/a` placeholders).
    /// Disable this in high-cardinality scope environments where per-scope tag overhead outweighs queryability.
    pub instrumentation_scope_metadata_as_tags: bool,

    /// OTLP sum translation settings.
    pub sums: Sums,

    /// Comma-separated list of tags to add to every emitted metric.
    ///
    /// Defaults to empty. When the static-tag resolver produces no tags, this value is preserved.
    /// When it produces one or more tags, those tags replace this value; the two sets are not merged.
    pub tags: String,

    /// OTLP summary translation settings.
    pub summaries: Summaries,

    /// Time-to-live for cached prior data points used when converting cumulative monotonic sums
    /// to deltas. Defaults to `3600` seconds. Must be greater than zero.
    pub delta_ttl: Duration,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            tag_cardinality: OriginTagCardinality::default(),
            histogram_mode: HistogramMode::default(),
            send_histogram_aggregations: false,
            resource_attributes_as_tags: false,
            instrumentation_scope_metadata_as_tags: true,
            sums: Sums::default(),
            tags: String::new(),
            summaries: Summaries::default(),
            delta_ttl: DEFAULT_DELTA_TTL,
        }
    }
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

/// Transport accepted by the OTLP gRPC receiver.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GrpcTransport {
    /// TCP transport.
    #[default]
    Tcp,
    /// Unix stream socket transport.
    Unix,
}

impl GrpcTransport {
    /// Returns the configuration spelling of this transport.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Unix => "unix",
        }
    }
}

impl FromStr for GrpcTransport {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tcp" => Ok(Self::Tcp),
            "unix" => Ok(Self::Unix),
            other => Err(Error::new_without_source(format!(
                "unknown gRPC transport `{other}`; expected `tcp` or `unix`"
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

/// Default gRPC keepalive ping interval: idle time before the server sends a PING to check the
/// connection is still alive.
pub const DEFAULT_GRPC_KEEPALIVE_TIME: Duration = Duration::from_secs(2 * 60 * 60);

/// Default gRPC keepalive ping timeout: time to wait for a PONG before closing the connection.
pub const DEFAULT_GRPC_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Server-side keepalive parameters for the OTLP gRPC receiver.
///
/// All fields are `Duration`. A zero duration is the sentinel for "unset": the translator
/// resolves `time` and `timeout` to their effective defaults, and treats `max_connection_age` and
/// `max_connection_age_grace` as "no limit."
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct KeepaliveServerParameters {
    /// Maximum time a connection may exist before the server sends GOAWAY. A zero duration means no
    /// limit. Lower this to force periodic connection rotation in long-lived deployments.
    pub max_connection_age: Duration,

    /// Grace period after `max_connection_age` before the connection is forcibly closed. A zero
    /// duration means no limit. Increase this to give in-flight RPCs more time to finish during
    /// age-based shutdown.
    pub max_connection_age_grace: Duration,

    /// Idle time before the server sends a keepalive PING. A zero duration is resolved by the
    /// translator to the default of 2 hours. Lower this to detect dead connections faster at the
    /// cost of more frequent PING traffic.
    pub time: Duration,

    /// Time to wait for a PONG after a keepalive PING before closing the connection. A zero duration
    /// is resolved by the translator to the default of 20 seconds. Increase this on networks with
    /// high latency or intermittent delays.
    pub timeout: Duration,
}

/// TLS settings for an OTLP receiver (gRPC or HTTP).
///
/// These configure server-side TLS for the receiver. When `cert_file` and `key_file` are both set, the receiver
/// accepts encrypted connections. When `ca_file` is also set, the server requests client certificates and verifies
/// them if presented, but does not require them (optional verification).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tls {
    /// Path to the PEM-encoded certificate chain file.
    ///
    /// When set together with `key_file`, enables TLS on the receiver. Defaults to empty (TLS disabled).
    pub cert_file: String,

    /// Path to the PEM-encoded private key file.
    ///
    /// The private key must correspond to the leaf certificate in `cert_file`. Defaults to empty (TLS disabled).
    pub key_file: String,

    /// Path to the PEM-encoded CA certificate file for verifying client certificates.
    ///
    /// When set, the server requests client certificates and verifies them against the CA certificates in this file.
    /// Clients that present a certificate must provide a valid one; clients that present no certificate are still
    /// accepted. Defaults to empty (no client certificate verification).
    pub ca_file: String,
}

/// OTLP gRPC receiver.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GrpcReceiver {
    /// Address the gRPC receiver listens on.
    pub endpoint: String,

    /// Maximum inbound message size, in MiB.
    pub max_recv_msg_size_mib: u64,

    /// Transport the gRPC receiver binds. Defaults to `tcp`.
    pub transport: GrpcTransport,

    /// HTTP/2 maximum concurrent streams per connection.
    ///
    /// Defaults to `0`, which means no limit (the server applies no cap). A positive value sets
    /// the `SETTINGS_MAX_CONCURRENT_STREAMS` HTTP/2 setting.
    pub max_concurrent_streams: u32,

    /// Server-side keepalive parameters. Always present; zero durations are resolved to the
    /// grpc-go defaults (2 h interval, 20 s timeout) by the translator.
    pub keepalive: KeepaliveServerParameters,

    /// TLS settings for the gRPC receiver.
    pub tls: Tls,
}

/// OTLP HTTP receiver.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HttpReceiver {
    /// Address the HTTP receiver listens on.
    pub endpoint: String,

    /// Transport the HTTP receiver binds (for example, `tcp` or `unix`). (not in Datadog Agent
    /// config schema)
    pub transport: String,

    /// CORS configuration for the HTTP receiver.
    pub cors: Cors,

    /// TLS settings for the HTTP receiver.
    pub tls: Tls,

    /// Maximum HTTP request body size, in bytes.
    ///
    /// Defaults to `0`, which applies the 20 MiB limit used by the Datadog Agent. A positive value
    /// sets the limit in bytes.
    pub max_request_body_size: u64,
}

impl Default for HttpReceiver {
    fn default() -> Self {
        Self {
            // Witnessed; overwritten during drive.
            endpoint: String::new(),
            transport: "tcp".to_string(),
            cors: Cors::default(),
            tls: Tls::default(),
            max_request_body_size: 0,
        }
    }
}

/// CORS configuration for the OTLP HTTP receiver.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Cors {
    /// Allowed origins for cross-origin requests. A bare `*` allows every origin; a partial
    /// wildcard like `http://*.example.com` matches that prefix and suffix. Empty disables CORS.
    /// Defaults to empty; configure this for browser-based exporters.
    pub allowed_origins: Vec<String>,

    /// Request headers allowed in preflight, beyond the implicit `Accept`, `Accept-Language`,
    /// `Content-Type`, and `Content-Language`. Use `*` to allow any header. Empty also implicitly
    /// allows `X-Requested-With`. Defaults to empty; add headers for browser exporters that send them.
    pub allowed_headers: Vec<String>,

    /// Response headers exposed to the browser via `Access-Control-Expose-Headers`.
    /// Defaults to empty; add headers browser clients need to read.
    pub exposed_headers: Vec<String>,

    /// Seconds browsers may cache a preflight response. Defaults to `0` (no caching); increase
    /// to avoid repeated preflight round-trips for frequent browser requests.
    pub max_age: u64,
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

    /// Non-zero byte budget for the OTLP trace context interner. (not in Datadog Agent config schema)
    ///
    /// Defaults to 512 KiB and cannot exceed 1 GiB.
    pub string_interner_size: NonZeroUsize,

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
            string_interner_size: DEFAULT_STRING_INTERNER_SIZE_BYTES,
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
    use super::{CumulativeMonotonicMode, GrpcReceiver, GrpcTransport, HttpReceiver, InitialCumulativeMonotonicValue};

    #[test]
    fn grpc_transport_parses_known_values() {
        assert_eq!("tcp".parse::<GrpcTransport>().unwrap(), GrpcTransport::Tcp);
        assert_eq!("unix".parse::<GrpcTransport>().unwrap(), GrpcTransport::Unix);
    }

    #[test]
    fn grpc_transport_rejects_unknown_values() {
        assert!("tcp4".parse::<GrpcTransport>().is_err());
        assert!("udp".parse::<GrpcTransport>().is_err());
    }

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

    #[test]
    fn grpc_receiver_defaults_to_agent_compatible_values() {
        let grpc = GrpcReceiver::default();
        assert_eq!(grpc.max_concurrent_streams, 0, "0 means no limit (Agent default)");
    }

    #[test]
    fn http_receiver_defaults_to_agent_compatible_values() {
        let http = HttpReceiver::default();
        assert_eq!(http.max_request_body_size, 0, "0 means 20 MiB default (Agent default)");
    }
}
