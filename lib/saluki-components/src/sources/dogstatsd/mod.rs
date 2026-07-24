//! DogStatsD source.
//!
//! # Missing
//!
//! - Create a health handle for each listener.
//! - Handle UDS stream framing without treating EOF the same way as UDP and UDS datagram framing.
//! - Track dispatch failures without depending on whether all events were already iterated.

use std::{
    collections::VecDeque,
    future::Future,
    io,
    num::NonZeroUsize,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::spawn_traced_named,
};
use saluki_config::{deserialize_space_separated_or_seq, GenericConfiguration};
use saluki_context::tags::{RawTags, RawTagsFilter};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter, UsageExpr};
use saluki_core::data_model::event::{
    eventd::EventD,
    metric::{Metric, MetricMetadata, MetricOrigin},
    service_check::ServiceCheck,
    Event, EventType,
};
use saluki_core::{
    components::{sources::*, ComponentContext},
    pooling::ElasticObjectPool,
    topology::{interconnect::EventBufferManager, EventsBuffer, OutputDefinition},
};
use saluki_env::{workload::CaptureEntityResolver, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _, FixedSizeVec},
    deser::{
        codec::dogstatsd::*,
        framing::{Framer as _, FramingError, LengthDelimitedFramer},
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, ProcessIdentity, Stream,
    },
};
use serde::{Deserialize, Deserializer};
use serde_with::{serde_as, NoneAsEmptyString};
use snafu::{ResultExt as _, Snafu};
use stringtheory::MetaString;
use tokio::{
    pin, select,
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

mod forwarder;
use self::forwarder::{PacketForwarder, PacketForwarderTarget};

mod framer;
use self::framer::get_framer;
use crate::sources::dogstatsd::tags::{WellKnownTags, WellKnownTagsFilterPredicate};

mod filters;
use self::filters::EnablePayloadsFilter;

mod io_buffer;
use self::io_buffer::IoBufferManager;

mod metrics;
use self::metrics::{build_metrics, Metrics};

mod replay;
use self::replay::{CaptureRecord, CapturedTaggerHandle, TrafficCapture};
pub use self::replay::{
    DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDReplayAPIHandler, DogStatsDReplayControl,
    ReplaySession, TimestampResolution, TrafficCaptureReader, DEFAULT_REPLAY_LOOPS, REPLAY_CREDENTIALS_GID,
};

mod origin;
use self::origin::{
    origin_from_event_packet, origin_from_metric_packet, origin_from_service_check_packet, DogStatsDOriginTagResolver,
    OriginEnrichmentConfiguration, ProcessOrigin,
};

mod resolver;
use self::resolver::ContextResolvers;

mod tags;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Failed to create {} listener: {}", listener_type, source))]
    FailedToCreateListener {
        listener_type: &'static str,
        source: ListenerError,
    },

    #[snafu(display("No listeners configured. Please specify a port (`dogstatsd_port`) or a socket path (`dogstatsd_socket` or `dogstatsd_stream_socket`) to enable a listener."))]
    NoListenersConfigured,

    #[snafu(display("Could not resolve bind_host '{}': {}", host, source))]
    UnresolvableBindHost { host: String, source: std::io::Error },

    #[snafu(display("bind_host '{}' resolved to zero IP addresses.", host))]
    BindHostHasNoAddresses { host: String },
}

/// Baseline byte cost per interner entry, used to convert the Core Agent's entry-count-based
/// `dogstatsd_string_interner_size` to a byte size.
///
/// 4096 entries × 512 bytes = 2 MiB, matching ADP's previous default.
const INTERNER_BASELINE_BYTES_PER_ENTRY: u64 = 512;
const DOGSTATSD_LISTENER_WORKER_COUNT: usize = 1;
const DOGSTATSD_PIPELINE_COUNT: usize = 1;
const MIN_DOGSTATSD_WORKER_COUNT: usize = 2;

fn default_decoder_worker_count(vcpus: usize) -> usize {
    vcpus
        .saturating_sub(DOGSTATSD_LISTENER_WORKER_COUNT + DOGSTATSD_PIPELINE_COUNT)
        .max(MIN_DOGSTATSD_WORKER_COUNT)
}

const fn default_buffer_size() -> usize {
    8192
}

const fn default_buffer_count() -> usize {
    128
}

const fn default_buffer_count_max() -> usize {
    // 32768 buffers at the default 8 KiB size provide about 256 MiB of payload capacity.
    32768
}

const fn default_port() -> u16 {
    8125
}

const fn default_tcp_port() -> u16 {
    0
}

const fn default_statsd_forward_port() -> u16 {
    0
}

const fn default_socket_receive_buffer_size() -> usize {
    0
}

const fn default_allow_context_heap_allocations() -> bool {
    true
}

const fn default_no_aggregation_pipeline_support() -> bool {
    true
}

const fn default_context_string_interner_entry_count() -> u64 {
    4096
}

const fn default_cached_contexts_limit() -> usize {
    500_000
}

const fn default_cached_tagsets_limit() -> usize {
    500_000
}

const fn default_context_expiry_seconds() -> u64 {
    20
}

const fn default_dogstatsd_permissive_decoding() -> bool {
    true
}

const fn default_dogstatsd_minimum_sample_rate() -> f64 {
    0.000000003845
}

const fn default_true() -> bool {
    true
}

/// Returns the core Agent default SDDL applied to DogStatsD Windows named pipes.
const fn default_windows_pipe_security_descriptor() -> &'static str {
    "D:AI(A;;GA;;;WD)"
}

fn default_windows_pipe_security_descriptor_string() -> String {
    default_windows_pipe_security_descriptor().to_string()
}

/// Controls which payload types are forwarded to the backend.
#[derive(Deserialize)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct EnablePayloadsConfiguration {
    /// Whether or not to enable sending series (counter/gauge/rate) payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub series: bool,

    /// Whether or not to enable sending sketch (distribution) payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub sketches: bool,

    /// Whether or not to enable sending event payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub events: bool,

    /// Whether or not to enable sending service check payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub service_checks: bool,
}

impl Default for EnablePayloadsConfiguration {
    fn default() -> Self {
        Self {
            series: true,
            sketches: true,
            events: true,
            service_checks: true,
        }
    }
}

const MIN_CAPTURE_DEPTH: usize = 1024;

const fn default_capture_depth() -> usize {
    MIN_CAPTURE_DEPTH
}

const DOGSTATSD_CAPTURE_DIR: &str = "dsd_capture";

fn deserialize_empty_metastring_as_none<'de, D>(deserializer: D) -> Result<Option<MetaString>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<MetaString>::deserialize(deserializer)?;
    Ok(value.filter(|host| !host.is_empty()))
}

#[derive(Deserialize, Default)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
struct DogStatsDTelemetryConfiguration {
    /// Whether to break down DogStatsD processed-metric telemetry by UDS origin.
    ///
    /// When enabled, metric-message `dogstatsd.processed` telemetry includes an `origin` label derived from the
    /// sender's UDS origin. This can add one telemetry series per origin and should primarily be used for diagnostics.
    ///
    /// Defaults to `false`.
    #[serde(default)]
    dogstatsd_origin: bool,
}

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
#[serde_as]
#[derive(Deserialize, Default)]
#[cfg_attr(test, derive(derive_where::DeriveWhere, serde::Serialize))]
#[cfg_attr(test, derive_where(PartialEq))]
pub struct DogStatsDConfiguration {
    /// Hostname used when DogStatsD metrics do not carry an explicit `host:` tag.
    #[serde(skip)]
    default_hostname: MetaString,

    /// The size of the buffer used to receive messages into, in bytes.
    ///
    /// Payloads can't exceed this size, or they will be truncated, leading to discarded messages.
    ///
    /// Defaults to 8192 bytes.
    #[serde(rename = "dogstatsd_buffer_size", default = "default_buffer_size")]
    buffer_size: usize,

    /// The number of message buffers to allocate up front.
    ///
    /// This is the baseline pool size allocated at startup. The pool then grows on demand up to
    /// `dogstatsd_buffer_count_max` as active stream connections and datagram queues need additional buffers.
    /// Higher values allocate more memory at startup but reduce on-demand allocations during bursts.
    ///
    /// Defaults to 128.
    #[serde(rename = "dogstatsd_buffer_count", default = "default_buffer_count")]
    buffer_count: usize,

    /// The maximum number of message buffers to allocate overall.
    ///
    /// The global pool starts at `dogstatsd_buffer_count` buffers and grows on demand up to this limit. Active stream
    /// connections use these buffers for reads, while connectionless listeners use them to queue received packets for
    /// decoding. Increasing this value lets datagram listeners absorb larger bursts at the cost of up to one additional
    /// `dogstatsd_buffer_size` allocation per buffer. High-throughput workloads with traffic bursts may increase it.
    /// This limit bounds payload buffers, but not per-connection task and channel bookkeeping.
    /// After a short grace period without pool growth, ADP releases idle buffers until the pool returns to
    /// `dogstatsd_buffer_count`.
    ///
    /// The pool never holds fewer buffers than `dogstatsd_buffer_count`, so a value below the baseline is treated as
    /// equal to it.
    ///
    /// Defaults to 32768, or `dogstatsd_buffer_count` if that is larger.
    #[serde(rename = "dogstatsd_buffer_count_max", default = "default_buffer_count_max")]
    buffer_count_max: usize,

    /// The number of workers in the global pool that decodes connectionless DogStatsD packets.
    ///
    /// If set to `0`, the worker count is derived from the available vCPUs using the Core Agent's default formula.
    /// Positive values force an exact worker count. Higher values can improve throughput when decoding is CPU-bound,
    /// but also increase task scheduling and per-worker event buffering overhead.
    ///
    /// Defaults to 0.
    #[serde(rename = "dogstatsd_workers_count", default)]
    workers_count: usize,

    /// The port to listen on in UDP mode.
    ///
    /// If set to `0`, UDP isn't used.
    ///
    /// Defaults to 8125.
    #[serde(rename = "dogstatsd_port", default = "default_port")]
    port: u16,

    /// The size of the DogStatsD UDP/UDS socket receive buffer, in bytes.
    ///
    /// If set to `0`, the OS default is used.
    ///
    /// Defaults to 0.
    #[serde(rename = "dogstatsd_so_rcvbuf", default = "default_socket_receive_buffer_size")]
    socket_receive_buffer_size: usize,

    /// The port to listen on in TCP mode.
    ///
    /// If set to `0`, TCP isn't used.
    ///
    /// Defaults to 0.
    #[serde(rename = "dogstatsd_tcp_port", default = "default_tcp_port")]
    tcp_port: u16,

    /// The host to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this value is non-empty and `statsd_forward_port` is non-zero. Setup failures
    /// are logged, and send failures are tracked through telemetry.
    ///
    /// Defaults to unset.
    #[serde(
        rename = "statsd_forward_host",
        default,
        deserialize_with = "deserialize_empty_metastring_as_none"
    )]
    statsd_forward_host: Option<MetaString>,

    /// The port to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this value is non-zero and `statsd_forward_host` is non-empty.
    ///
    /// Defaults to 0.
    #[serde(rename = "statsd_forward_port", default = "default_statsd_forward_port")]
    statsd_forward_port: u16,

    /// The Unix domain socket path to listen on, in datagram mode.
    ///
    /// If not set, UDS (in datagram mode) isn't used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_socket", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode.
    ///
    /// If not set, UDS (in stream mode) isn't used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_stream_socket", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    socket_stream_path: Option<String>,

    /// Controls whether ADP logs oversized DogStatsD stream frames.
    ///
    /// When set to `true`, ADP emits a warning when a UDS stream frame exceeds the
    /// configured DogStatsD buffer size. The frame is still rejected either way.
    ///
    /// Enable this when diagnosing clients that send oversized UDS stream frames.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_stream_log_too_big", default)]
    stream_log_too_big: bool,

    /// The Windows named pipe name to listen on.
    ///
    /// If set, ADP listens for DogStatsD stream traffic on `\\.\pipe\<name>` on Windows.
    /// The listener is unsupported on non-Windows platforms.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_pipe_name", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    pipe_name: Option<String>,

    /// Windows named pipe security descriptor.
    ///
    /// This SDDL descriptor is applied when creating the named pipe listener.
    ///
    /// Defaults to `D:AI(A;;GA;;;WD)`.
    #[serde(
        rename = "dogstatsd_windows_pipe_security_descriptor",
        default = "default_windows_pipe_security_descriptor_string"
    )]
    windows_pipe_security_descriptor: String,

    /// Whether ADP lowers DogStatsD parse-failure logs to debug level.
    ///
    /// When set to `true`, invalid metrics, events, and service checks still increment decode-failure telemetry, but
    /// their parse-failure logs are emitted at debug level instead of warning level. Enable this to suppress noisy
    /// parse-error logs from misbehaving clients.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_disable_verbose_logs", default)]
    disable_verbose_logs: bool,

    /// Listener types that require DogStatsD messages to be newline-terminated.
    ///
    /// Valid values are `udp`, `uds`, and `named_pipe`. Invalid values are ignored.
    ///
    /// Enable this when DogStatsD clients must reject packets or stream frames that don't end with a newline.
    ///
    /// Defaults to unset, which accepts the final message without a newline.
    #[serde(
        rename = "dogstatsd_eol_required",
        default,
        deserialize_with = "deserialize_space_separated_or_seq"
    )]
    eol_required: Vec<String>,

    /// The host address to bind DogStatsD UDP and TCP listeners to.
    ///
    /// When set, UDP and TCP listeners bind to this address. Accepts either an IP literal (for example,
    /// `192.168.1.50`, `::1`) or a hostname that resolves via DNS (for example, `agent.internal`).
    /// Ignored when `dogstatsd_non_local_traffic` is `true`.
    ///
    /// Defaults to unset, which binds to `127.0.0.1`.
    #[serde(rename = "bind_host", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    bind_host: Option<String>,

    /// Whether or not to listen for non-local traffic in UDP mode.
    ///
    /// If set to `true`, the listener will accept packets from any interface/address. Otherwise, the source will only
    /// listen on the address specified by `bind_host`, or `127.0.0.1` if `bind_host` isn't set.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_non_local_traffic", default)]
    non_local_traffic: bool,

    /// Whether to autoscale UDP stream handlers using `SO_REUSEPORT`.
    ///
    /// When enabled on Linux, the DogStatsD source binds multiple UDP sockets to the configured port with
    /// `SO_REUSEPORT`, allowing the kernel to load-balance incoming datagrams across independent stream handler
    /// tasks. The number of sockets scales with available vCPUs: one stream handler base, plus one additional
    /// per 8 vCPUs, capped at 4 total.
    ///
    /// Has no effect on non-Linux platforms because `SO_REUSEPORT` doesn't provide kernel-level load balancing
    /// there; a warning is logged at startup if enabled outside of Linux.
    ///
    /// Enable this on multi-vCPU Linux deployments where UDP DogStatsD throughput is bottlenecked on a single
    /// receive task.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_autoscale_udp_listeners", default)]
    autoscale_udp_listeners: bool,

    /// Whether or not to allow heap allocations when resolving contexts.
    ///
    /// When resolving contexts during parsing, the metric name and tags are interned to reduce memory usage. The
    /// interner has a fixed size, however, which means some strings can fail to be interned if the interner is full.
    /// When set to `true`, we allow these strings to be allocated on the heap like normal, but this can lead to
    /// increased (unbounded) memory usage. When set to `false`, if the metric name and all of its tags can't be
    /// interned, the metric is skipped.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_allow_context_heap_allocs",
        default = "default_allow_context_heap_allocations"
    )]
    allow_context_heap_allocations: bool,

    /// Whether or not to enable support for no-aggregation pipelines.
    ///
    /// When enabled, this influences how metrics are parsed, specifically around user-provided metric timestamps. When
    /// metric timestamps are present, it's used as a signal to any aggregation transforms that the metric shouldn't
    /// be aggregated.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_no_aggregation_pipeline",
        default = "default_no_aggregation_pipeline_support"
    )]
    no_aggregation_pipeline_support: bool,

    /// Number of entries for the string interner, as interpreted by the Core Datadog Agent.
    ///
    /// When `dogstatsd_string_interner_size_bytes` isn't set, this value is multiplied by 512 bytes per entry to
    /// derive the interner byte size. This provides backwards compatibility for customers migrating configurations
    /// from the Core Agent, where this setting represents an entry count rather than a byte size.
    ///
    /// Defaults to 4096 entries, which yields 2 MiB when converted.
    #[serde(
        rename = "dogstatsd_string_interner_size",
        default = "default_context_string_interner_entry_count"
    )]
    context_string_interner_entry_count: u64,

    /// Total size of the string interner used for contexts, in bytes.
    ///
    /// When set, this takes priority over `dogstatsd_string_interner_size`. This controls the amount of memory that
    /// can be used to intern metric names and tags. If the interner is full, metrics with contexts that haven't
    /// already been resolved may or may not be dropped, depending on the value of `allow_context_heap_allocations`.
    #[serde(rename = "dogstatsd_string_interner_size_bytes", default)]
    context_string_interner_size_bytes: Option<ByteSize>,

    /// The maximum number of cached contexts to allow.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit doesn't affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(
        rename = "dogstatsd_cached_contexts_limit",
        default = "default_cached_contexts_limit"
    )]
    cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// This is the maximum number of resolved tagsets that can be cached at any given time. This limit doesn't affect
    /// the total number of tagsets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "dogstatsd_cached_tagsets_limit", default = "default_cached_tagsets_limit")]
    cached_tagsets_limit: usize,

    /// The number of seconds after which cached contexts will expire.
    ///
    /// Higher values allow for more effective caching for sparse metrics at the cost of increased memory usage.
    ///
    /// Defaults to 20 seconds.
    #[serde(
        rename = "dogstatsd_context_expiry_seconds",
        default = "default_context_expiry_seconds"
    )]
    context_expiry_seconds: u64,

    /// Whether or not to enable permissive mode in the decoder.
    ///
    /// Permissive mode allows the decoder to relax its strictness around the allowed payloads, which lets it match the
    /// decoding behavior of the Datadog Agent.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_permissive_decoding",
        default = "default_dogstatsd_permissive_decoding"
    )]
    permissive_decoding: bool,

    /// The minimum sample rate allowed for metrics.
    ///
    /// When metrics are sent with a sample rate _lower_ than this value then it will be clamped to this value. This is
    /// done in order to ensure an upper bound on how many equivalent samples are tracked for the metric, as high sample
    /// rates (very small numbers, such as `0.00000001`) can lead to large memory growth.
    ///
    /// A warning log will be emitted when clamping occurs, as this represents an effective loss of metric samples.
    ///
    /// Defaults to `0.000000003845`. (~260M samples)
    #[serde(
        rename = "dogstatsd_minimum_sample_rate",
        default = "default_dogstatsd_minimum_sample_rate"
    )]
    minimum_sample_rate: f64,

    /// Which payload types to forward to the backend.
    #[serde(rename = "enable_payloads", default)]
    enable_payloads: EnablePayloadsConfiguration,

    /// Configuration related to origin detection and enrichment.
    #[serde(flatten, default)]
    origin_enrichment: OriginEnrichmentConfiguration,

    /// Configuration related to DogStatsD telemetry.
    #[serde(default)]
    telemetry: DogStatsDTelemetryConfiguration,

    /// Workload provider to utilize for origin detection/enrichment.
    #[serde(skip)]
    #[cfg_attr(test, derive_where(skip))]
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,

    /// Resolver to use for mapping live sender PIDs to container entities before deferred processing.
    #[serde(skip, default)]
    #[cfg_attr(test, derive_where(skip))]
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,

    /// Additional tags to add to all metrics.
    #[serde(rename = "dogstatsd_tags", default)]
    additional_tags: Vec<String>,

    /// The directory where DogStatsD capture files are written by default.
    ///
    /// When set to an empty path, the source attempts to derive the directory from `run_path` by appending
    /// `dsd_capture`. If neither value is available, callers must provide an explicit capture path when starting a
    /// capture session.
    ///
    /// Defaults to empty.
    #[serde(rename = "dogstatsd_capture_path", default)]
    capture_path: PathBuf,

    /// The maximum number of captured packets that can be queued for persistence.
    ///
    /// This controls the depth of the in-process capture queue. Values below `1024` are raised to `1024` before the
    /// capture writer starts, preventing a zero-depth rendezvous channel from serializing DogStatsD stream handlers
    /// behind capture persistence.
    ///
    /// Defaults to `1024`.
    #[serde(rename = "dogstatsd_capture_depth", default = "default_capture_depth")]
    capture_depth: usize,

    #[serde(skip, default)]
    #[cfg_attr(test, derive_where(skip))]
    capture_control: DogStatsDCaptureControl,

    #[serde(skip, default)]
    #[cfg_attr(test, derive_where(skip))]
    replay_control: DogStatsDReplayControl,

    /// Provider kind tag appended to all metrics as `provider_kind:<value>`.
    ///
    /// Set via `DD_PROVIDER_KIND` by the Helm chart on GKE Autopilot (`gke-autopilot`) and GKE on
    /// Google Distributed Cloud (`gke-gdc`). When empty or absent, no tag is added.
    ///
    /// Defaults to `""` (disabled).
    #[serde(default)]
    provider_kind: String,
}

#[derive(Clone, Copy, Default)]
struct EolRequired {
    udp: bool,
    uds: bool,
    named_pipe: bool,
}

impl EolRequired {
    fn from_config_values(values: &[String]) -> Self {
        let mut eol_required = Self::default();

        for value in values {
            match value.as_str() {
                "udp" => eol_required.udp = true,
                "uds" => eol_required.uds = true,
                "named_pipe" => eol_required.named_pipe = true,
                _ => warn!(
                    value,
                    "Invalid dogstatsd_eol_required value. Expected 'udp', 'uds', or 'named_pipe'."
                ),
            }
        }

        eol_required
    }

    fn for_listener(&self, listen_addr: &ListenAddress) -> bool {
        match listen_addr {
            ListenAddress::Udp(_) => self.udp,
            ListenAddress::Tcp(_) => false,
            ListenAddress::Unixgram(_) | ListenAddress::Unix(_) => self.uds,
            ListenAddress::NamedPipe { .. } => self.named_pipe,
        }
    }
}

/// Resolves a `bind_host` string to an `IpAddr`.
///
/// Accepts either an IP literal (no DNS required) or a hostname (resolved via async DNS). Returns
/// `UnresolvableBindHost` if the lookup fails, or `BindHostHasNoAddresses` if it succeeds but
/// returns no addresses.
async fn resolve_bind_host(host: &str) -> Result<std::net::IpAddr, Error> {
    let mut addrs = tokio::net::lookup_host((host, 0u16))
        .await
        .context(UnresolvableBindHost { host: host.to_string() })?;
    addrs
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| Error::BindHostHasNoAddresses { host: host.to_string() })
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut dogstatsd_config: Self = config.as_typed()?;
        dogstatsd_config.fix_empty_capture_path(config);
        dogstatsd_config.fix_capture_depth();
        Ok(dogstatsd_config)
    }

    /// Gets both the `additional_tags` and any others specified by other configuration fields, such as `provider_kind`.
    fn additional_tags(&self) -> Vec<String> {
        if self.provider_kind.is_empty() {
            return self.additional_tags.clone();
        }

        let mut tags = self.additional_tags.clone();
        tags.push(format!("provider_kind:{}", self.provider_kind.clone()));
        tags
    }

    fn fix_capture_depth(&mut self) {
        self.capture_depth = self.capture_depth.max(MIN_CAPTURE_DEPTH);
    }

    /// Returns the effective string interner size in bytes.
    ///
    /// If `dogstatsd_string_interner_size_bytes` is set, it's used directly. Otherwise,
    /// `dogstatsd_string_interner_size` (an entry count) is multiplied by 512 bytes per entry to derive the byte
    /// size.
    fn effective_context_string_interner_bytes(&self) -> ByteSize {
        match self.context_string_interner_size_bytes {
            Some(explicit_bytes) => explicit_bytes,
            None => {
                saluki_antithesis::always_le!(
                    self.context_string_interner_entry_count,
                    u64::MAX / INTERNER_BASELINE_BYTES_PER_ENTRY,
                    "dogstatsd interner byte-size multiply does not overflow",
                    { "entry_count": self.context_string_interner_entry_count }
                );
                ByteSize::b(
                    self.context_string_interner_entry_count
                        .saturating_mul(INTERNER_BASELINE_BYTES_PER_ENTRY),
                )
            }
        }
    }

    fn eol_required(&self) -> EolRequired {
        EolRequired::from_config_values(&self.eol_required)
    }

    fn statsd_forward_target(&self) -> Option<(&MetaString, u16)> {
        let host = self.statsd_forward_host.as_ref()?;
        if self.statsd_forward_port == 0 {
            return None;
        }

        Some((host, self.statsd_forward_port))
    }

    fn packet_forwarder_target(&self) -> Option<PacketForwarderTarget> {
        let (host, port) = self.statsd_forward_target()?;
        Some(PacketForwarderTarget::new(host.clone(), port))
    }

    /// Returns the number of UDP stream handlers to spawn, derived from `dogstatsd_autoscale_udp_listeners` and
    /// the number of available vCPUs.
    ///
    /// Returns `None` when autoscaling is disabled, which keeps the legacy single-socket behavior. The platform
    /// gate for `SO_REUSEPORT` lives inside the listener—this method intentionally stays platform-agnostic.
    fn udp_streams_to_yield(&self) -> Option<NonZeroUsize> {
        if !self.autoscale_udp_listeners {
            return None;
        }

        #[cfg(not(target_os = "linux"))]
        if self.autoscale_udp_listeners {
            warn!("UDP stream handler autoscaling not supported on non-Linux platforms. Default to single stream handler.");
            return None;
        }

        let vcpus = std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
        let streams = (1 + vcpus / 8).min(4);
        NonZeroUsize::new(streams)
    }

    /// Returns the effective maximum size of the I/O buffer pool.
    ///
    /// The pool can never hold fewer buffers than the configured baseline, so a `dogstatsd_buffer_count_max` below
    /// `dogstatsd_buffer_count` (including a legacy config that only raised `dogstatsd_buffer_count`) is treated as
    /// equal to the baseline rather than reducing capacity.
    fn effective_max_buffer_count(&self) -> usize {
        self.buffer_count_max.max(self.buffer_count)
    }

    fn decoder_worker_count(&self) -> NonZeroUsize {
        let worker_count = if self.workers_count == 0 {
            let vcpus = std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
            default_decoder_worker_count(vcpus)
        } else {
            self.workers_count
        };

        NonZeroUsize::new(worker_count).expect("DogStatsD decoder worker count must be non-zero")
    }

    /// Sets the default hostname used when DogStatsD metrics do not carry an explicit `host:` tag.
    pub fn with_default_hostname(mut self, hostname: impl Into<MetaString>) -> Self {
        self.default_hostname = hostname.into();
        self
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment won't be enabled.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }

    /// Sets the resolver to use for mapping live sender PIDs before deferring DogStatsD packet processing.
    ///
    /// This resolver pins the sender entity while socket credentials are current so origin enrichment and traffic
    /// capture do not resolve a stale or reused PID later. It is configured separately from the workload provider
    /// because it only needs a narrow live-PID lookup.
    ///
    /// Defaults to unset.
    pub fn with_capture_entity_resolver<R>(mut self, capture_entity_resolver: R) -> Self
    where
        R: CaptureEntityResolver + Send + Sync + 'static,
    {
        self.capture_entity_resolver = Some(Arc::new(capture_entity_resolver));
        self
    }

    /// Returns the shared control handle for DogStatsD traffic capture.
    pub fn capture_control(&self) -> DogStatsDCaptureControl {
        self.capture_control.clone()
    }

    /// Returns an HTTP API handler exposing the DogStatsD capture control surface.
    pub fn capture_api_handler(&self) -> DogStatsDCaptureAPIHandler {
        DogStatsDCaptureAPIHandler::new(self.capture_control.clone())
    }

    /// Returns the shared control handle for DogStatsD traffic replay.
    pub fn replay_control(&self) -> DogStatsDReplayControl {
        self.replay_control.clone()
    }

    /// Returns an HTTP API handler exposing the DogStatsD replay control surface.
    pub fn replay_api_handler(&self) -> DogStatsDReplayAPIHandler {
        DogStatsDReplayAPIHandler::new(self.replay_control.clone())
    }

    fn fix_empty_capture_path(&mut self, config: &GenericConfiguration) {
        if self.capture_path.parent().is_some() {
            return;
        }

        let capture_path = match config.try_get_typed::<PathBuf>("run_path") {
            Ok(Some(mut run_path)) => {
                run_path.push(DOGSTATSD_CAPTURE_DIR);
                run_path
            }
            Ok(None) => {
                debug!(
                    "`dogstatsd_capture_path` and `run_path` were empty. Default DogStatsD capture path is unavailable."
                );
                return;
            }
            Err(e) => {
                debug!(
                    error = %e,
                    "Failed to read `run_path` from configuration. Default DogStatsD capture path is unavailable."
                );
                return;
            }
        };

        self.capture_path = capture_path;
    }

    /// Using the current configuration, determines which listeners should be created and adds an address for each into
    /// a `Vec<ListenAddress>`. This function has no side effects so that it can be unit tested whereas build_listeners`
    /// actually binds the listeners on the system.
    ///
    /// `bind_host` is the pre-resolved IP that UDP and TCP listeners should bind to (provided by
    /// `resolve_bind_host`). Precedence matches the Agent:
    ///   - `non_local_traffic=true` → `0.0.0.0` (`bind_host` ignored)
    ///   - `bind_host=Some(ip)`     → `ip`
    ///   - `bind_host=None`         → `127.0.0.1`
    fn build_addresses(&self, bind_host: Option<std::net::IpAddr>) -> Vec<ListenAddress> {
        let bind_ip: std::net::IpAddr = if self.non_local_traffic {
            [0, 0, 0, 0].into()
        } else {
            bind_host.unwrap_or_else(|| [127, 0, 0, 1].into())
        };

        let mut addresses: Vec<ListenAddress> = Vec::new();

        if self.port != 0 {
            addresses.push(ListenAddress::Udp(std::net::SocketAddr::new(bind_ip, self.port)));
        }

        if self.tcp_port != 0 {
            addresses.push(ListenAddress::Tcp(std::net::SocketAddr::new(bind_ip, self.tcp_port)));
        }

        if let Some(socket_path) = &self.socket_path {
            addresses.push(ListenAddress::Unixgram(socket_path.into()));
        }

        if let Some(socket_stream_path) = &self.socket_stream_path {
            addresses.push(ListenAddress::Unix(socket_stream_path.into()));
        }

        if let Some(pipe_name) = &self.pipe_name {
            addresses.push(ListenAddress::named_pipe_with_input_buffer_size(
                pipe_name,
                &self.windows_pipe_security_descriptor,
                self.buffer_size as u32,
            ));
        }

        addresses
    }

    fn uds_origin_detection_unsupported_on_platform(&self, addresses: &[ListenAddress]) -> bool {
        self.origin_enrichment.enabled()
            && cfg!(not(target_os = "linux"))
            && addresses
                .iter()
                .any(|address| matches!(address, ListenAddress::Unixgram(_) | ListenAddress::Unix(_)))
    }

    fn warn_if_uds_origin_detection_unsupported(&self, addresses: &[ListenAddress]) {
        if self.uds_origin_detection_unsupported_on_platform(addresses) {
            warn!(
                "DogStatsD UDS origin detection is enabled, but PID-based Unix socket credentials are unsupported on \
                 this platform. Metrics are accepted without PID-based origin enrichment."
            );
        }
    }

    /// Builds the appropriate `Listener` objects.
    async fn build_listeners(&self) -> Result<Vec<Listener>, Error> {
        // Resolve `bind_host` to an IP (via DNS if needed). Skip the lookup when
        // `non_local_traffic=true` since `bind_host` is ignored in that branch—matches Go's
        // laziness and avoids failing startup on an unresolvable hostname that wouldn't be used.
        let bind_host: Option<std::net::IpAddr> = if self.non_local_traffic {
            None
        } else {
            match &self.bind_host {
                Some(host) => Some(resolve_bind_host(host).await?),
                None => None,
            }
        };

        let addresses = self.build_addresses(bind_host);
        self.warn_if_uds_origin_detection_unsupported(&addresses);
        let mut listeners = Vec::new();
        let socket_receive_buffer_size =
            (self.socket_receive_buffer_size != 0).then_some(self.socket_receive_buffer_size);
        let udp_streams_to_yield = self.udp_streams_to_yield();
        for address in addresses {
            let listener_type = address.listener_type();
            let listener_streams = matches!(address, ListenAddress::Udp(_))
                .then_some(udp_streams_to_yield)
                .flatten();
            let listener = Listener::from_listen_address(address, listener_streams)
                .await
                .context(FailedToCreateListener { listener_type })?
                .with_receive_buffer_size(socket_receive_buffer_size);

            listeners.push(listener);
        }
        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let listeners = self.build_listeners().await?;
        if listeners.is_empty() {
            return Err(Error::NoListenersConfigured.into());
        }

        // Every listener requires at least one I/O buffer to ensure that all listeners can be serviced without
        // deadlocking any of the others. Multi-socket connectionless listeners require one buffer per yielded socket.
        let min_buffers: usize = listeners.iter().map(Listener::min_buffer_reservation).sum();
        let max_buffers = self.effective_max_buffer_count();
        if max_buffers < min_buffers {
            return Err(generic_error!(
                "The maximum I/O buffer count ({}) must be at least {} to service all configured listeners.",
                max_buffers,
                min_buffers,
            ));
        }

        let origin_detection_enabled = self.origin_enrichment.enabled();
        // Single CapturedTaggerHandle is cloned to both the resolver (reader of the captured store) and the replay
        // control surface (writer). Both sides reference the same atomic slot.
        let captured_tagger = CapturedTaggerHandle::new();

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(|provider| {
            DogStatsDOriginTagResolver::new(self.origin_enrichment.clone(), provider, captured_tagger.clone())
        });
        let context_resolvers = ContextResolvers::new(self, &context, maybe_origin_tags_resolver)
            .error_context("Failed to create context resolvers.")?;

        let codec_config = DogStatsDCodecConfiguration::default()
            .with_timestamps(self.no_aggregation_pipeline_support)
            .with_permissive_mode(self.permissive_decoding)
            .with_minimum_sample_rate(self.minimum_sample_rate)
            .with_client_origin_detection(self.origin_enrichment.origin_detection_client);

        let codec = DogStatsDCodec::from_configuration(codec_config);
        let eol_required = self.eol_required();

        let enable_payloads_filter = EnablePayloadsFilter::default()
            .with_allow_series(self.enable_payloads.series)
            .with_allow_sketches(self.enable_payloads.sketches)
            .with_allow_events(self.enable_payloads.events)
            .with_allow_service_checks(self.enable_payloads.service_checks);
        let traffic_capture = TrafficCapture::with_workload_provider(
            self.capture_path.clone(),
            self.capture_depth.max(MIN_CAPTURE_DEPTH),
            self.workload_provider.clone(),
        );
        self.capture_control.bind(traffic_capture.clone());
        let packet_forwarder_target = self.packet_forwarder_target();

        self.replay_control.bind(captured_tagger);

        // The pool allocates `buffer_count` buffers up front and may grow on demand up to `max_buffers`. The effective
        // maximum is never below the baseline, so configs that only raise `dogstatsd_buffer_count` keep their full
        // capacity instead of being silently reduced to the `dogstatsd_buffer_count_max` default.
        let (io_buffer_pool, io_buffer_pool_shrinker) =
            build_io_buffer_pool(self.buffer_count, max_buffers, self.buffer_size);
        Ok(Box::new(DogStatsD {
            listeners,
            decoder_worker_count: self.decoder_worker_count(),
            io_buffer_pool,
            io_buffer_queue_capacity: max_buffers,
            io_buffer_pool_shrinker: Box::pin(io_buffer_pool_shrinker),
            codec,
            context_resolvers,
            default_hostname: self.default_hostname.clone(),
            enabled_filter: enable_payloads_filter,
            origin_detection_enabled,
            origin_telemetry_enabled: self.telemetry.dogstatsd_origin,
            stream_log_too_big: self.stream_log_too_big,
            disable_verbose_logs: self.disable_verbose_logs,
            eol_required,
            additional_tags: self.additional_tags().into(),
            capture_entity_resolver: self.capture_entity_resolver.clone(),
            traffic_capture,
            packet_forwarder_target,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("events", EventType::EventD),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
            ]
        });
        &OUTPUTS
    }
}

impl MemoryBounds for DogStatsDConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let additional_buffers = self.effective_max_buffer_count().saturating_sub(self.buffer_count);
        let adjusted_buffer_size = get_adjusted_buffer_size(self.buffer_size);

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DogStatsD>("source struct")
            // We allocate the baseline buffer pool up front.
            .with_expr(UsageExpr::product(
                "buffers",
                UsageExpr::config("dogstatsd_buffer_count", self.buffer_count),
                UsageExpr::config("dogstatsd_buffer_size", adjusted_buffer_size),
            ))
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_expr(UsageExpr::config(
                "dogstatsd_string_interner_size_bytes",
                self.effective_context_string_interner_bytes().as_u64() as usize,
            ));

        // The pool can grow on demand up to its maximum, so account for the additional headroom as firm usage.
        builder.firm().with_expr(UsageExpr::product(
            "elastic buffers",
            UsageExpr::constant("dogstatsd_buffer_count_max_extra", additional_buffers),
            UsageExpr::config("dogstatsd_buffer_size", adjusted_buffer_size),
        ));
    }
}

/// DogStatsD source.
pub struct DogStatsD {
    listeners: Vec<Listener>,
    decoder_worker_count: NonZeroUsize,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    io_buffer_queue_capacity: usize,
    io_buffer_pool_shrinker: Pin<Box<dyn Future<Output = ()> + Send>>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    enabled_filter: EnablePayloadsFilter,
    origin_detection_enabled: bool,
    origin_telemetry_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    eol_required: EolRequired,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

struct ListenerContext {
    shutdown_handle: ShutdownHandle,
    listener: Listener,
    datagram_sender: mpsc::Sender<QueuedDatagram>,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    origin_detection_enabled: bool,
    origin_telemetry_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    eol_required: EolRequired,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

#[derive(Clone)]
struct HandlerContext {
    listen_addr: ListenAddress,
    eol_required: bool,
    codec: DogStatsDCodec,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    datagram_sender: mpsc::Sender<QueuedDatagram>,
    datagram_context: Option<Arc<DatagramSocketContext>>,
    metrics: Metrics,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    additional_tags: Arc<[String]>,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    traffic_capture: TrafficCapture,
    packet_forwarder: Option<PacketForwarder>,
}

#[derive(Clone)]
struct DatagramDecoderContext {
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    origin_detection_enabled: bool,
    disable_verbose_logs: bool,
    additional_tags: Arc<[String]>,
    traffic_capture: TrafficCapture,
}

struct DatagramSocketContext {
    listen_addr: ListenAddress,
    eol_required: bool,
    metrics: Metrics,
    packet_forwarder: Option<PacketForwarder>,
}

struct QueuedDatagram {
    result: io::Result<ReceivedBuffer>,
    socket_context: Arc<DatagramSocketContext>,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        let mut pool_shrinker_shutdown_coordinator = ShutdownCoordinator::default();
        spawn_traced_named(
            "dogstatsd-io-buffer-pool-shrinker",
            process_io_buffer_pool_shrinker(
                self.io_buffer_pool_shrinker,
                pool_shrinker_shutdown_coordinator.register(),
            ),
        );

        let (datagram_sender, datagram_receiver) = mpsc::channel(self.io_buffer_queue_capacity);
        let datagram_receiver = Arc::new(Mutex::new(datagram_receiver));
        let datagram_decoder_context = DatagramDecoderContext {
            codec: self.codec.clone(),
            context_resolvers: self.context_resolvers.clone(),
            default_hostname: self.default_hostname.clone(),
            origin_detection_enabled: self.origin_detection_enabled,
            disable_verbose_logs: self.disable_verbose_logs,
            additional_tags: self.additional_tags.clone(),
            traffic_capture: self.traffic_capture.clone(),
        };

        let mut datagram_decoder_tasks = Vec::with_capacity(self.decoder_worker_count.get());
        for worker_id in 0..self.decoder_worker_count.get() {
            datagram_decoder_tasks.push(spawn_traced_named(
                format!("dogstatsd-datagram-decoder-{worker_id}"),
                process_datagram_decoder(
                    datagram_receiver.clone(),
                    context.clone(),
                    datagram_decoder_context.clone(),
                    self.enabled_filter,
                ),
            ));
        }
        drop(datagram_receiver);

        let mut listener_shutdown_coordinator = ShutdownCoordinator::default();
        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
            let task_name = format!("dogstatsd-listener-{}", listener.listen_address().listener_type());

            // TODO: Create a health handle for each listener.
            //
            // We need to rework `HealthRegistry` to look a little more like `ComponentRegistry` so that we can have it
            // already be scoped properly, otherwise all we can do here at present is either have a relative name, like
            // `uds-stream`, or try and hardcode the full component name, which we will inevitably forget to update if
            // we tweak the topology configuration, etc.
            let listener_context = ListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
                datagram_sender: datagram_sender.clone(),
                io_buffer_pool: self.io_buffer_pool.clone(),
                codec: self.codec.clone(),
                context_resolvers: self.context_resolvers.clone(),
                default_hostname: self.default_hostname.clone(),
                origin_detection_enabled: self.origin_detection_enabled,
                origin_telemetry_enabled: self.origin_telemetry_enabled,
                stream_log_too_big: self.stream_log_too_big,
                disable_verbose_logs: self.disable_verbose_logs,
                eol_required: self.eol_required,
                additional_tags: self.additional_tags.clone(),
                capture_entity_resolver: self.capture_entity_resolver.clone(),
                traffic_capture: self.traffic_capture.clone(),
                packet_forwarder_target: self.packet_forwarder_target.clone(),
            };

            spawn_traced_named(
                task_name,
                process_listener(context.clone(), listener_context, self.enabled_filter),
            );
        }
        drop(datagram_sender);

        health.mark_ready();
        debug!("DogStatsD source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        //
        // We also handle liveness here, which doesn't really matter for _this_ task, since the real work is happening
        // in the listeners, but we need to satisfy the health checker.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping DogStatsD source...");

        shutdown_listeners_and_drain_datagram_decoders(listener_shutdown_coordinator, datagram_decoder_tasks).await?;
        pool_shrinker_shutdown_coordinator.shutdown_and_wait().await;

        debug!("DogStatsD source stopped.");

        Ok(())
    }
}

async fn process_io_buffer_pool_shrinker(
    io_buffer_pool_shrinker: Pin<Box<dyn Future<Output = ()> + Send>>, shutdown_handle: ShutdownHandle,
) {
    pin!(shutdown_handle);

    select! {
        _ = &mut shutdown_handle => {
            debug!("I/O buffer pool shrinker received shutdown signal.");
        },
        _ = io_buffer_pool_shrinker => {
            debug!("I/O buffer pool shrinker stopped.");
        },
    }
}

fn build_io_buffer_pool(
    min_buffers: usize, max_buffers: usize, buffer_size: usize,
) -> (ElasticObjectPool<BytesBuffer>, impl Future<Output = ()> + Send) {
    saluki_antithesis::always_le!(
        buffer_size,
        usize::MAX - 4,
        "dogstatsd buffer size add does not overflow",
        { "buffer_size": buffer_size }
    );
    let adjusted_buffer_size = get_adjusted_buffer_size(buffer_size);
    ElasticObjectPool::with_builder("dsd_packet_bufs", min_buffers, max_buffers, move || {
        FixedSizeVec::with_capacity(adjusted_buffer_size)
    })
}

fn is_connectionless_listen_address(listen_addr: &ListenAddress) -> bool {
    match listen_addr {
        ListenAddress::Udp(_) => true,
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => true,
        _ => false,
    }
}

async fn process_listener(
    source_context: SourceContext, listener_context: ListenerContext, enabled_filter: EnablePayloadsFilter,
) {
    let ListenerContext {
        shutdown_handle,
        mut listener,
        datagram_sender,
        io_buffer_pool,
        codec,
        context_resolvers,
        default_hostname,
        origin_detection_enabled,
        origin_telemetry_enabled,
        stream_log_too_big,
        disable_verbose_logs,
        eol_required,
        additional_tags,
        capture_entity_resolver,
        traffic_capture,
        packet_forwarder_target,
    } = listener_context;

    pin!(shutdown_handle);

    let listen_addr = listener.listen_address().clone();
    let metrics = build_metrics(
        &listen_addr,
        source_context.component_context(),
        origin_telemetry_enabled,
    );
    let packet_forwarder = packet_forwarder_target
        .as_ref()
        .map(|target| target.to_forwarder(metrics.clone()));
    if let Some(packet_forwarder) = &packet_forwarder {
        packet_forwarder.spawn_connect();
    }
    let datagram_context = is_connectionless_listen_address(&listen_addr).then(|| {
        Arc::new(DatagramSocketContext {
            listen_addr: listen_addr.clone(),
            eol_required: eol_required.for_listener(&listen_addr),
            metrics: metrics.clone(),
            packet_forwarder: packet_forwarder.clone(),
        })
    });

    let mut stream_shutdown_coordinator = ShutdownCoordinator::default();

    info!(%listen_addr, "DogStatsD listener started.");

    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!(%listen_addr, "Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            result = listener.accept() => match result {
                Ok(stream) => {
                    debug!(%listen_addr, "Spawning new stream handler.");

                    let handler_context = HandlerContext {
                        listen_addr: listen_addr.clone(),
                        eol_required: eol_required.for_listener(&listen_addr),
                        codec: codec.clone(),
                        io_buffer_pool: io_buffer_pool.clone(),
                        datagram_sender: datagram_sender.clone(),
                        datagram_context: datagram_context.clone(),
                        metrics: metrics.clone(),
                        context_resolvers: context_resolvers.clone(),
                        default_hostname: default_hostname.clone(),
                        origin_detection_enabled,
                        stream_log_too_big,
                        disable_verbose_logs,
                        additional_tags: additional_tags.clone(),
                        capture_entity_resolver: capture_entity_resolver.clone(),
                        traffic_capture: traffic_capture.clone(),
                        packet_forwarder: packet_forwarder.clone(),
                    };

                    let task_name = format!(
                        "dogstatsd-stream-handler-{}",
                        listen_addr.listener_type(),
                    );
                    spawn_traced_named(task_name, process_stream(stream, source_context.clone(), handler_context, stream_shutdown_coordinator.register(), enabled_filter));
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            }
        }
    }

    stream_shutdown_coordinator.shutdown_and_wait().await;

    info!(%listen_addr, "DogStatsD listener stopped.");
}

async fn process_stream(
    stream: Stream, source_context: SourceContext, handler_context: HandlerContext, shutdown_handle: ShutdownHandle,
    enabled_filter: EnablePayloadsFilter,
) {
    select! {
        _ = shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, handler_context, enabled_filter) => {},
    }
}

fn origin_detection_failed_for_telemetry(
    origin_detection_enabled: bool, bytes_read: usize, peer_addr: &ConnectionAddress,
) -> bool {
    origin_detection_enabled && bytes_read > 0 && peer_addr.has_process_credential_telemetry_error()
}

struct ReceivedBuffer {
    buffer: BytesBuffer,
    bytes_read: usize,
    peer_addr: ConnectionAddress,
    process_origin: Option<ProcessOrigin>,
    buffer_handoff: ReadBufferHandoff,
}

struct ReadBufferHandoff(Option<oneshot::Sender<BytesBuffer>>);

impl ReadBufferHandoff {
    fn return_to_reader(self, buffer: BytesBuffer) {
        if let Some(sender) = self.0 {
            let _ = sender.send(buffer);
        }
    }
}

struct BufferedStreamReader {
    receiver: Option<mpsc::Receiver<io::Result<ReceivedBuffer>>>,
    task: JoinHandle<()>,
}

impl BufferedStreamReader {
    fn new(
        stream: Stream, io_buffer_pool: ElasticObjectPool<BytesBuffer>, memory_limiter: MemoryLimiter,
        capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    ) -> Self {
        debug_assert!(!stream.is_connectionless());
        let (packets_tx, receiver) = mpsc::channel(1);
        let task = spawn_traced_named(
            "dogstatsd-stream-reader",
            receive_connected_stream(
                stream,
                io_buffer_pool,
                memory_limiter,
                capture_entity_resolver,
                packets_tx,
            ),
        );

        Self {
            receiver: Some(receiver),
            task,
        }
    }

    fn take_receiver(&mut self) -> mpsc::Receiver<io::Result<ReceivedBuffer>> {
        self.receiver.take().expect("Buffered stream receiver already taken")
    }
}

impl Drop for BufferedStreamReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn receive_connected_stream(
    mut stream: Stream, io_buffer_pool: ElasticObjectPool<BytesBuffer>, memory_limiter: MemoryLimiter,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    packets_tx: mpsc::Sender<io::Result<ReceivedBuffer>>,
) {
    debug!("Stream reader started.");

    let mut buffer_manager = IoBufferManager::new(&io_buffer_pool);

    loop {
        select! {
            _ = packets_tx.closed() => break,
            _ = memory_limiter.wait_for_capacity() => {}
        }

        let mut buffer = select! {
            _ = packets_tx.closed() => break,
            buffer = buffer_manager.take_buffer() => buffer,
        };
        let (bytes_read, peer_addr) = match select! {
            _ = packets_tx.closed() => break,
            result = stream.receive(&mut buffer) => result,
        } {
            Ok(received) => received,
            Err(error) => {
                buffer_manager.return_buffer(buffer);
                let _ = packets_tx.send(Err(error)).await;
                break;
            }
        };
        let process_origin = resolve_process_origin(capture_entity_resolver.as_deref(), &peer_addr);

        let (buffer_sender, returned_buffer) = oneshot::channel();
        let received = ReceivedBuffer {
            buffer,
            bytes_read,
            peer_addr,
            process_origin,
            buffer_handoff: ReadBufferHandoff(Some(buffer_sender)),
        };

        if packets_tx.send(Ok(received)).await.is_err() {
            break;
        }

        match returned_buffer.await {
            Ok(buffer) if buffer.has_remaining() => buffer_manager.return_buffer(buffer),
            Ok(buffer) => drop(buffer),
            Err(_) => break,
        }
    }

    debug!("Stream reader stopped.");
}

async fn receive_connectionless_stream(
    mut stream: Stream, io_buffer_pool: ElasticObjectPool<BytesBuffer>, memory_limiter: MemoryLimiter,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    datagram_sender: mpsc::Sender<QueuedDatagram>, socket_context: Arc<DatagramSocketContext>,
) {
    debug!(listen_addr = %socket_context.listen_addr, "Datagram reader started.");

    let mut buffer_manager = IoBufferManager::new(&io_buffer_pool);
    loop {
        select! {
            _ = datagram_sender.closed() => break,
            _ = memory_limiter.wait_for_capacity() => {}
        }

        let mut buffer = select! {
            _ = datagram_sender.closed() => break,
            buffer = buffer_manager.take_buffer() => buffer,
        };
        let result = match select! {
            _ = datagram_sender.closed() => break,
            result = stream.receive(&mut buffer) => result,
        } {
            Ok((bytes_read, peer_addr)) => {
                let process_origin = resolve_process_origin(capture_entity_resolver.as_deref(), &peer_addr);
                Ok(ReceivedBuffer {
                    buffer,
                    bytes_read,
                    peer_addr,
                    process_origin,
                    buffer_handoff: ReadBufferHandoff(None),
                })
            }
            Err(error) => {
                buffer_manager.return_buffer(buffer);
                Err(error)
            }
        };

        let receive_failed = result.is_err();
        let queued = QueuedDatagram {
            result,
            socket_context: socket_context.clone(),
        };
        if datagram_sender.send(queued).await.is_err() {
            break;
        }
        if receive_failed {
            continue;
        }
    }

    debug!(listen_addr = %socket_context.listen_addr, "Datagram reader stopped.");
}

async fn process_datagram_decoder(
    datagram_receiver: Arc<Mutex<mpsc::Receiver<QueuedDatagram>>>, source_context: SourceContext,
    decoder_context: DatagramDecoderContext, enabled_filter: EnablePayloadsFilter,
) {
    drive_datagram_decoder(datagram_receiver, source_context, decoder_context, enabled_filter).await;
    debug!("Datagram decoder drained its queue.");
}

async fn shutdown_listeners_and_drain_datagram_decoders(
    listener_shutdown_coordinator: ShutdownCoordinator, datagram_decoder_tasks: Vec<JoinHandle<()>>,
) -> Result<(), GenericError> {
    listener_shutdown_coordinator.shutdown_and_wait().await;

    for decoder_task in datagram_decoder_tasks {
        decoder_task
            .await
            .error_context("DogStatsD datagram decoder stopped unexpectedly while draining.")?;
    }

    Ok(())
}

async fn drive_datagram_decoder(
    datagram_receiver: Arc<Mutex<mpsc::Receiver<QueuedDatagram>>>, source_context: SourceContext,
    decoder_context: DatagramDecoderContext, enabled_filter: EnablePayloadsFilter,
) {
    let DatagramDecoderContext {
        codec,
        mut context_resolvers,
        default_hostname,
        origin_detection_enabled,
        disable_verbose_logs,
        additional_tags,
        traffic_capture,
    } = decoder_context;
    let mut stream_capture = StreamCaptureState::new();
    let mut event_buffer_manager = EventBufferManager::default();
    let mut last_listen_addr = None;
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        select! {
            maybe_datagram = async {
                datagram_receiver.lock().await.recv().await
            } => {
                let Some(QueuedDatagram { result, socket_context }) = maybe_datagram else {
                    break;
                };
                let DatagramSocketContext {
                    listen_addr,
                    eol_required,
                    metrics,
                    packet_forwarder,
                } = socket_context.as_ref();
                last_listen_addr = Some(listen_addr.clone());

                let ReceivedBuffer {
                    mut buffer,
                    bytes_read,
                    peer_addr,
                    process_origin,
                    buffer_handoff,
                } = match result {
                    Ok(received) => received,
                    Err(error) => {
                        metrics.packet_receive_failure().increment(1);
                        warn!(%listen_addr, %error, "I/O error while reading datagram. Continuing listener.");
                        continue;
                    }
                };
                let io_buffer = &mut buffer;
                let payload = received_payload(io_buffer, bytes_read);

                capture_uds_traffic(
                    listen_addr,
                    &traffic_capture,
                    &peer_addr,
                    process_origin.as_ref(),
                    payload,
                    &mut stream_capture,
                );

                metrics.packet_receive_success().increment(1);
                metrics.bytes_received().increment(bytes_read as u64);
                metrics.bytes_received_size().record(bytes_read as f64);
                if origin_detection_failed_for_telemetry(origin_detection_enabled, bytes_read, &peer_addr) {
                    metrics.origin_detection_errors().increment(1);
                }

                trace!(
                    buffer_len = io_buffer.remaining(),
                    buffer_cap = io_buffer.remaining_mut(),
                    %listen_addr,
                    %peer_addr,
                    "Received {} bytes from datagram socket.",
                    bytes_read
                );

                let mut framer = get_framer(listen_addr, *eol_required);
                loop {
                    match framer.next_frame(io_buffer, true) {
                        Ok(Some(frame)) => {
                            trace!(%listen_addr, %peer_addr, ?frame, "Decoded frame.");
                            if let Some(forwarder) = packet_forwarder {
                                forwarder.forward(frame.clone()).await;
                            }
                            match handle_frame(
                                &frame[..],
                                &codec,
                                &mut context_resolvers,
                                metrics,
                                origin_detection_enabled,
                                process_origin.as_ref(),
                                enabled_filter,
                                &additional_tags,
                                &default_hostname,
                            ) {
                                Ok(Some(event)) => {
                                    if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                        debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                                        dispatch_events(event_buffer, &source_context, listen_addr).await;
                                    }
                                }
                                Ok(None) => continue,
                                Err(error) => {
                                    log_parse_failure(
                                        disable_verbose_logs,
                                        listen_addr,
                                        &peer_addr,
                                        &frame,
                                        &error,
                                    );
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            metrics.framing_errors().increment(1);
                            debug!(%listen_addr, %peer_addr, %error, "Error decoding datagram frame. Continuing listener.");
                            break;
                        }
                    }
                }

                buffer_handoff.return_to_reader(buffer);
            }
            _ = buffer_flush.tick() => {
                if let (Some(event_buffer), Some(listen_addr)) =
                    (event_buffer_manager.consume(), last_listen_addr.as_ref())
                {
                    dispatch_events(event_buffer, &source_context, listen_addr).await;
                }
            }
        }
    }

    if let (Some(event_buffer), Some(listen_addr)) = (event_buffer_manager.consume(), last_listen_addr.as_ref()) {
        dispatch_events(event_buffer, &source_context, listen_addr).await;
    }
}

async fn drive_stream(
    stream: Stream, source_context: SourceContext, handler_context: HandlerContext,
    enabled_filter: EnablePayloadsFilter,
) {
    if stream.is_connectionless() {
        let memory_limiter = source_context.topology_context().memory_limiter().clone();
        let socket_context = handler_context
            .datagram_context
            .clone()
            .expect("connectionless stream must have a datagram context");
        receive_connectionless_stream(
            stream,
            handler_context.io_buffer_pool,
            memory_limiter,
            handler_context.capture_entity_resolver,
            handler_context.datagram_sender,
            socket_context,
        )
        .await;
        return;
    }

    drive_connected_stream(stream, source_context, handler_context, enabled_filter).await;
}

async fn drive_connected_stream(
    stream: Stream, source_context: SourceContext, handler_context: HandlerContext,
    enabled_filter: EnablePayloadsFilter,
) {
    let listen_addr = handler_context.listen_addr.clone();
    let metrics = handler_context.metrics.clone();
    let memory_limiter = source_context.topology_context().memory_limiter().clone();
    let mut stream_reader = BufferedStreamReader::new(
        stream,
        handler_context.io_buffer_pool.clone(),
        memory_limiter,
        handler_context.capture_entity_resolver.clone(),
    );
    let receiver = stream_reader.take_receiver();

    debug!(%listen_addr, "Stream handler started.");

    metrics.connections_active().increment(1);
    drive_decoder(receiver, source_context, handler_context, enabled_filter).await;
    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
}

async fn drive_decoder(
    mut stream_receiver: mpsc::Receiver<io::Result<ReceivedBuffer>>, source_context: SourceContext,
    handler_context: HandlerContext, enabled_filter: EnablePayloadsFilter,
) {
    let HandlerContext {
        listen_addr,
        eol_required,
        codec,
        metrics,
        mut context_resolvers,
        default_hostname,
        origin_detection_enabled,
        stream_log_too_big,
        disable_verbose_logs,
        additional_tags,
        traffic_capture,
        packet_forwarder,
        ..
    } = handler_context;
    let mut framer = get_framer(&listen_addr, eol_required);

    let mut stream_capture = StreamCaptureState::new();
    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();
    let mut last_process_origin = None;

    'read: loop {
        let mut eof = false;

        select! {
            // We read from the stream.
            maybe_read_result = stream_receiver.recv() => match maybe_read_result {
                Some(Ok(received)) => {
                    let ReceivedBuffer {
                        mut buffer,
                        bytes_read,
                        peer_addr,
                        process_origin,
                        buffer_handoff,
                    } = received;
                    let io_buffer = &mut buffer;
                    if process_origin.is_some() {
                        last_process_origin = process_origin;
                    }

                    if bytes_read == 0 {
                        eof = true;
                    }

                    let payload = received_payload(io_buffer, bytes_read);

                    capture_uds_traffic(
                        &listen_addr,
                        &traffic_capture,
                        &peer_addr,
                        last_process_origin.as_ref(),
                        payload,
                        &mut stream_capture,
                    );

                    metrics.bytes_received().increment(bytes_read as u64);
                    metrics.bytes_received_size().record(bytes_read as f64);
                    let origin_detection_failed =
                        origin_detection_failed_for_telemetry(origin_detection_enabled, bytes_read, &peer_addr);

                    trace!(
                        buffer_len = io_buffer.remaining(),
                        buffer_cap = io_buffer.remaining_mut(),
                        eof,
                        %listen_addr,
                        %peer_addr,
                        "Received {} bytes from stream.",
                        bytes_read
                    );

                    if should_drop_oversized_named_pipe_frame(&listen_addr, io_buffer) {
                        metrics.framing_errors().increment(1);
                        debug!(%listen_addr, %peer_addr, "DogStatsD named pipe frame exceeded the configured buffer size. Dropping frame.");
                        io_buffer.clear();
                        buffer_handoff.return_to_reader(buffer);
                        continue 'read;
                    }

                    'frame: loop {
                        let frame_result = framer.next_frame(io_buffer, eof);
                        let completed_outer_frames = framer.take_completed_outer_frames();
                        if completed_outer_frames > 0 {
                            metrics.packet_receive_success().increment(completed_outer_frames as u64);
                        }
                        if origin_detection_failed && completed_outer_frames > 0 {
                            metrics.origin_detection_errors().increment(completed_outer_frames as u64);
                        }

                        match frame_result {
                            Ok(Some(frame)) => {
                                if matches!(listen_addr, ListenAddress::NamedPipe { .. }) {
                                    metrics.packet_receive_success().increment(1);
                                }
                                trace!(%listen_addr, %peer_addr, ?frame, "Decoded frame.");
                                if let Some(forwarder) = &packet_forwarder {
                                    forwarder.forward(frame.clone()).await;
                                }
                                match handle_frame(
                                    &frame[..],
                                    &codec,
                                    &mut context_resolvers,
                                    &metrics,
                                    origin_detection_enabled,
                                    last_process_origin.as_ref(),
                                    enabled_filter,
                                    &additional_tags,
                                    &default_hostname,
                                ) {
                                    Ok(Some(event)) => {
                                        if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                            debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                                            dispatch_events(event_buffer, &source_context, &listen_addr).await;
                                        }
                                    },
                                    Ok(None) => {
                                        // We didn't decode an event, but there was no inherent error. This is likely
                                        // due to hitting resource limits, etc.
                                        //
                                        // Simply continue on.
                                        continue
                                    },
                                    Err(e) => {
                                        log_parse_failure(disable_verbose_logs, &listen_addr, &peer_addr, &frame, &e);
                                    },
                                }
                            }
                            Err(e) => {
                                metrics.framing_errors().increment(1);
                                if should_warn_stream_log_too_big(&listen_addr, &e, stream_log_too_big) {
                                    warn!(
                                        %listen_addr,
                                        %peer_addr,
                                        error = %e,
                                        "DogStatsD stream frame exceeded the configured buffer size."
                                    );
                                }

                                debug!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Stopping stream.");
                                break 'read;
                            }
                            Ok(None) => {
                                trace!(%listen_addr, %peer_addr, "Not enough data to decode another frame.");
                                if eof {
                                    debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                                    break 'read;
                                } else {
                                    break 'frame;
                                }
                            }
                        }
                    }

                    buffer_handoff.return_to_reader(buffer);
                },
                Some(Err(e)) => {
                    metrics.packet_receive_failure().increment(1);
                    warn!(%listen_addr, error = %e, "I/O error while decoding. Stopping stream.");
                    break 'read;
                },
                None => {
                    warn!(%listen_addr, "Buffered stream reader stopped unexpectedly. Stopping stream.");
                    break 'read;
                }
            },

            _ = buffer_flush.tick() => {
                if let Some(event_buffer) = event_buffer_manager.consume() {
                    dispatch_events(event_buffer, &source_context, &listen_addr).await;
                }
            },

        }
    }

    if let Some(event_buffer) = event_buffer_manager.consume() {
        dispatch_events(event_buffer, &source_context, &listen_addr).await;
    }
}

fn should_drop_oversized_named_pipe_frame(listen_addr: &ListenAddress, buffer: &BytesBuffer) -> bool {
    matches!(listen_addr, ListenAddress::NamedPipe { .. })
        && buffer.remaining_mut() == 0
        && memchr::memchr(b'\n', buffer.chunk()).is_none()
}

fn should_warn_stream_log_too_big(listen_addr: &ListenAddress, error: &FramingError, stream_log_too_big: bool) -> bool {
    stream_log_too_big
        && matches!(listen_addr, ListenAddress::Unix(_))
        && matches!(error, FramingError::InvalidFrame { .. })
}

fn log_parse_failure(
    disable_verbose_logs: bool, listen_addr: &ListenAddress, peer_addr: &ConnectionAddress, frame: &[u8],
    error: &ParseError,
) {
    let frame = String::from_utf8_lossy(frame);
    if disable_verbose_logs {
        debug!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    } else {
        warn!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    }
}

fn capture_uds_traffic(
    listen_addr: &ListenAddress, traffic_capture: &TrafficCapture, peer_addr: &ConnectionAddress,
    process_origin: Option<&ProcessOrigin>, payload: &[u8], stream_capture: &mut StreamCaptureState,
) {
    if payload.is_empty() || !traffic_capture.is_ongoing() {
        return;
    }

    match listen_addr {
        ListenAddress::Unixgram(_) => {
            let _ = traffic_capture.enqueue(build_capture_record(
                process_id_from_peer_addr(peer_addr),
                process_origin,
                payload,
            ));
        }
        ListenAddress::Unix(_) => {
            stream_capture.update_peer_metadata(peer_addr);
            stream_capture.pending.extend(payload);

            while let Ok(Some(outer_payload)) = stream_capture
                .outer_framer
                .next_frame(&mut stream_capture.pending, false)
            {
                let _ = traffic_capture.enqueue(build_capture_record(
                    stream_capture.last_pid,
                    process_origin,
                    &outer_payload,
                ));
            }
        }
        _ => {}
    }
}

struct StreamCaptureState {
    outer_framer: LengthDelimitedFramer,
    pending: VecDeque<u8>,
    last_pid: Option<i32>,
}

impl StreamCaptureState {
    fn new() -> Self {
        Self {
            outer_framer: LengthDelimitedFramer,
            pending: VecDeque::new(),
            last_pid: None,
        }
    }

    fn update_peer_metadata(&mut self, peer_addr: &ConnectionAddress) {
        if let Some(process_id) = process_id_from_peer_addr(peer_addr) {
            self.last_pid = Some(process_id);
        }
    }
}

fn build_capture_record(
    process_id: Option<i32>, process_origin: Option<&ProcessOrigin>, payload: &[u8],
) -> CaptureRecord {
    CaptureRecord {
        timestamp_ns: capture_timestamp_ns(),
        payload: payload.to_vec(),
        pid: process_id,
        ancillary: Vec::new(),
        container_id: process_origin
            .and_then(ProcessOrigin::container_entity_id)
            .map(ToString::to_string),
    }
}

fn process_id_from_peer_addr(peer_addr: &ConnectionAddress) -> Option<i32> {
    match peer_addr {
        ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(creds)) => Some(creds.pid),
        _ => None,
    }
}

fn resolve_process_origin(
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, peer_addr: &ConnectionAddress,
) -> Option<ProcessOrigin> {
    let creds = peer_addr.process_credentials()?;
    if creds.gid == REPLAY_CREDENTIALS_GID {
        return Some(ProcessOrigin::Replay(creds.uid));
    }

    let process_id = u32::try_from(creds.pid).ok()?;
    Some(match capture_entity_resolver {
        Some(resolver) => ProcessOrigin::Pinned(resolver.resolve_container_entity_for_live_pid(process_id)),
        None => ProcessOrigin::Unpinned(process_id),
    })
}

fn received_payload(buffer: &BytesBuffer, bytes_read: usize) -> &[u8] {
    let chunk = buffer.chunk();
    let start = chunk.len().saturating_sub(bytes_read);
    &chunk[start..]
}

fn capture_timestamp_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn handle_frame(
    frame: &[u8], codec: &DogStatsDCodec, context_resolvers: &mut ContextResolvers, source_metrics: &Metrics,
    origin_detection_enabled: bool, process_origin: Option<&ProcessOrigin>, enabled_filter: EnablePayloadsFilter,
    additional_tags: &[String], default_hostname: &MetaString,
) -> Result<Option<Event>, ParseError> {
    let resolve_telemetry_origin = || {
        (source_metrics.origin_telemetry_enabled() && origin_detection_enabled)
            .then(|| {
                process_origin
                    .and_then(ProcessOrigin::container_entity_id)
                    .map(ToString::to_string)
            })
            .flatten()
    };

    let parsed = match codec.decode_packet(frame) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Try and determine what the message type was, if possible, to increment the correct error counter.
            match parse_message_type(frame) {
                MessageType::MetricSample => {
                    source_metrics.record_metric_parse_failed(resolve_telemetry_origin().as_deref())
                }
                MessageType::Event => source_metrics.event_decode_failed().increment(1),
                MessageType::ServiceCheck => source_metrics.service_check_decode_failed().increment(1),
            }

            return Err(e);
        }
    };

    let event = match parsed {
        ParsedPacket::Metric(metric_packet) => {
            if metric_packet.num_points == 0 {
                return Ok(None);
            }
            let events_len = metric_packet.num_points;
            if !enabled_filter.allow_metric(&metric_packet) {
                trace!(
                    metric.name = metric_packet.metric_name,
                    "Skipping metric due to filter configuration."
                );
                return Ok(None);
            }

            match handle_metric_packet(
                metric_packet,
                context_resolvers,
                process_origin,
                additional_tags,
                default_hostname,
            ) {
                Some(metric) => {
                    source_metrics.record_metrics_received(events_len, resolve_telemetry_origin().as_deref());
                    Event::Metric(metric)
                }
                None => {
                    // We can only fail to get a metric back if we failed to resolve the context.
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
        ParsedPacket::Event(event) => {
            if !enabled_filter.allow_event(&event) {
                trace!("Skipping event {} due to filter configuration.", event.title);
                return Ok(None);
            }
            match handle_event_packet(event, context_resolvers, process_origin, additional_tags) {
                Some(event) => {
                    source_metrics.events_received().increment(1);
                    Event::EventD(event)
                }
                None => {
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
        ParsedPacket::ServiceCheck(service_check) => {
            if !enabled_filter.allow_service_check(&service_check) {
                trace!(
                    "Skipping service check {} due to filter configuration.",
                    service_check.name
                );
                return Ok(None);
            }
            match handle_service_check_packet(service_check, context_resolvers, process_origin, additional_tags) {
                Some(service_check) => {
                    source_metrics.service_checks_received().increment(1);
                    Event::ServiceCheck(service_check)
                }
                None => {
                    source_metrics.failed_context_resolve_total().increment(1);
                    return Ok(None);
                }
            }
        }
    };

    Ok(Some(event))
}

fn handle_metric_packet(
    packet: MetricPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String], default_hostname: &MetaString,
) -> Option<Metric> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let origin = origin_from_metric_packet(&packet, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);

    let hostname = well_known_tags.hostname.unwrap_or(default_hostname);

    // Try to resolve the context for this metric.
    let maybe_context =
        context_resolver.resolve_with_host_and_origin_tags(packet.metric_name, hostname, tags, origin_tags);

    match maybe_context {
        Some(context) => {
            let metric_origin = well_known_tags
                .jmx_check_name
                .map(MetricOrigin::jmx_check)
                .unwrap_or_else(MetricOrigin::dogstatsd);
            let metadata = MetricMetadata::default()
                .with_origin(metric_origin)
                .with_unit(packet.unit.map_or_else(MetaString::empty, MetaString::from_static));

            Some(Metric::from_parts(context, packet.values, metadata))
        }
        // We failed to resolve the context, likely due to not having enough interner capacity.
        None => None,
    }
}

fn handle_event_packet(
    packet: EventPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<EventD> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let origin = origin_from_event_packet(&packet, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags_resolver = context_resolvers.tags();
    let tags = tags_resolver.create_tag_set(tags)?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior in pkg/aggregator/aggregator.go (addEvent), which sets e.Ts = time.Now().Unix()
    // for any event with Ts == 0.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let eventd = EventD::new(packet.title, packet.text)
        .with_timestamp(timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_aggregation_key(packet.aggregation_key.map(|s| s.into()))
        .with_alert_type(packet.alert_type)
        .with_priority(packet.priority)
        // When no source type is provided, default to "api"—the same default the stock Datadog
        // Agent applies when serializing DogStatsD events to the intake JSON format. The agent
        // groups events by source type name and uses "api" as the key for events without an
        // explicit `s:` field. See: pkg/serializer/internal/metrics/events.go (writeItem).
        .with_source_type_name(Some(
            packet
                .source_type_name
                .map(|s| s.into())
                .unwrap_or_else(|| "api".into()),
        ))
        .with_alert_type(packet.alert_type)
        .with_tags(tags)
        .with_origin_tags(origin_tags);

    Some(eventd)
}

fn handle_service_check_packet(
    packet: ServiceCheckPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<ServiceCheck> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let origin = origin_from_service_check_packet(&packet, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags_resolver = context_resolvers.tags();
    let tags = tags_resolver.create_tag_set(tags)?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior, which sets the timestamp to time.Now().Unix() for any service check with a zero
    // timestamp.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_tags(tags)
        .with_origin_tags(origin_tags)
        .with_message(packet.message.map(|s| s.into()));

    Some(service_check)
}

fn get_filtered_tags_iterator<'a>(
    raw_tags: RawTags<'a>, additional_tags: &'a [String],
) -> impl Iterator<Item = &'a str> + Clone {
    // This filters out "well-known" tags from the raw tags in the DogStatsD packet, and then chains on any additional tags
    // that were configured on the source.
    RawTagsFilter::exclude(raw_tags, WellKnownTagsFilterPredicate).chain(additional_tags.iter().map(|s| s.as_str()))
}

async fn dispatch_events(mut event_buffer: EventsBuffer, source_context: &SourceContext, listen_addr: &ListenAddress) {
    debug!(%listen_addr, events_len = event_buffer.len(), "Forwarding events.");

    // TODO: This is maybe a little dicey because if we fail to dispatch the events, we may not have iterated over all of
    // them, so there might still be eventd events when get to the service checks point, and eventd events and/or service
    // check events when we get to the metrics point, and so on.
    //
    // There's probably something to be said for erroring out fully if this happens, since we should only fail to
    // dispatch if the downstream component fails entirely... and unless we have a way to restart the component, then
    // we're going to continue to fail to dispatch any more events until the process is restarted anyways.

    // Dispatch any eventd events, if present.
    if event_buffer.has_event_type(EventType::EventD) {
        let eventd_events = event_buffer.extract(Event::is_eventd);
        let events_output = source_context.dispatcher().buffered_named("events");

        // The `events` output is always wired in the DSD topology, so a missing output is an invariant violation that
        // crashes this component.
        if events_output.is_err() {
            saluki_antithesis::unreachable!("dsd 'events' output missing at dispatch");
        }

        if let Err(e) = events_output
            .expect("events output should always exist")
            .send_all(eventd_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch eventd events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "events" });
        }
    }

    // Dispatch any service check events, if present.
    if event_buffer.has_event_type(EventType::ServiceCheck) {
        let service_check_events = event_buffer.extract(Event::is_service_check);
        let service_checks_output = source_context.dispatcher().buffered_named("service_checks");

        if service_checks_output.is_err() {
            saluki_antithesis::unreachable!("dsd 'service_checks' output missing at dispatch");
        }

        if let Err(e) = service_checks_output
            .expect("service checks output should always exist")
            .send_all(service_check_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch service check events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "service_checks" });
        }
    }

    // Finally, if there are events left, they'll be metrics, so dispatch them.
    if !event_buffer.is_empty() {
        if let Err(e) = source_context
            .dispatcher()
            .dispatch_named("metrics", event_buffer)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch metric events.");

            saluki_antithesis::unreachable!("dsd dispatch failed mid-buffer", { "stream": "metrics" });
        }
    }
}

const fn get_adjusted_buffer_size(buffer_size: usize) -> usize {
    // This is a little goofy, but hear me out:
    //
    // In the Datadog Agent, the way the UDS listener works is that if it's in stream mode, it will do a standalone
    // socket read to get _just_ the length delimiter, which is 4 bytes. After that, it will do a read to get the packet
    // data itself, up to the limit of `dogstatsd_buffer_size`. This means that a _full_ UDS stream packet can be up to
    // `dogstatsd_buffer_size + 4` bytes.
    //
    // This isn't a problem in the Agent due to how it does the reads, but it's a problem for us because we want to be
    // able to get an entire frame in a single buffer for the purpose of decoding the frame. Rather than rewriting our
    // read loop such that we have to change the logic depending on UDP/UDS datagram vs UDS stream, we simply increase
    // the buffer size by 4 bytes to account for the length delimiter.
    //
    // We do it this way so that we don't have to change the buffer size in the configuration, since if you just ported
    // over a Datadog Agent configuration, the value would be too small, and vise versa.
    buffer_size + 4
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::ErrorKind,
        net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex, OnceLock,
        },
        time::Duration,
    };

    #[cfg(unix)]
    use bytes::Buf as _;
    use bytes::{BufMut as _, Bytes};
    use bytesize::ByteSize;
    use metrics::{Key, Label};
    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use saluki_config::ConfigurationLoader;
    use saluki_context::{ContextResolverBuilder, TagsResolverBuilder};
    #[cfg(unix)]
    use saluki_core::accounting::MemoryLimiter;
    use saluki_core::{
        components::ComponentContext,
        pooling::{helpers::get_pooled_object_via_builder, ObjectPool as _},
    };
    #[cfg(target_os = "linux")]
    use saluki_env::workload::providers::TestWorkloadProvider;
    use saluki_env::workload::{CaptureEntityResolver, EntityId};
    #[cfg(unix)]
    use saluki_io::net::Stream;
    use saluki_io::{
        buf::{BytesBuffer, FixedSizeVec},
        deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration, ParsedPacket},
        net::{ConnectionAddress, ListenAddress, ProcessCredentials, ProcessIdentity},
    };
    use saluki_metrics::test::TestRecorder;
    use serde_json::json;
    use stringtheory::MetaString;
    #[cfg(unix)]
    use tokio::{
        io::AsyncWriteExt as _,
        net::{UnixDatagram, UnixStream},
        task::yield_now,
    };
    use tokio::{
        net::UdpSocket,
        sync::{mpsc, Mutex},
        time::timeout,
    };

    use super::{
        build_io_buffer_pool, default_buffer_size, default_decoder_worker_count,
        default_windows_pipe_security_descriptor,
        filters::EnablePayloadsFilter,
        forwarder::{
            ConnectedPacketForwarder, ForwardPacket, PacketForwarder, PacketForwarderTarget, FORWARDER_QUEUE_CAPACITY,
        },
        handle_frame, handle_metric_packet,
        metrics::build_metrics,
        origin_detection_failed_for_telemetry, resolve_process_origin, shutdown_listeners_and_drain_datagram_decoders,
        ContextResolvers, DatagramSocketContext, DogStatsDConfiguration, ProcessOrigin, QueuedDatagram,
        DOGSTATSD_CAPTURE_DIR, MIN_CAPTURE_DEPTH,
    };
    #[cfg(unix)]
    use super::{receive_connected_stream, receive_connectionless_stream, received_payload, ReceivedBuffer};
    #[cfg(target_os = "linux")]
    use super::{DogStatsDOriginTagResolver, Listener, OriginEnrichmentConfiguration};

    const LINUX_EAFNOSUPPORT: i32 = 97;
    const MACOS_EAFNOSUPPORT: i32 = 47;

    fn is_ipv6_unavailable_error(error: &std::io::Error) -> bool {
        matches!(error.kind(), ErrorKind::AddrNotAvailable | ErrorKind::Unsupported)
            || matches!(error.raw_os_error(), Some(LINUX_EAFNOSUPPORT | MACOS_EAFNOSUPPORT))
    }

    fn test_component_context() -> ComponentContext {
        ComponentContext::test_source("dogstatsd_test")
    }

    fn test_datagram_socket_context(listen_addr: ListenAddress) -> Arc<DatagramSocketContext> {
        Arc::new(DatagramSocketContext {
            metrics: build_metrics(&listen_addr, &test_component_context(), false),
            listen_addr,
            eol_required: false,
            packet_forwarder: None,
        })
    }

    #[derive(Default)]
    struct CaptureTestEntityResolver {
        pid_map: StdMutex<HashMap<u32, EntityId>>,
    }

    impl CaptureTestEntityResolver {
        fn with_pid_mapping(process_id: u32, entity_id: EntityId) -> Self {
            let mut pid_map = HashMap::new();
            pid_map.insert(process_id, entity_id);
            Self {
                pid_map: StdMutex::new(pid_map),
            }
        }

        #[cfg(target_os = "linux")]
        fn set_pid_mapping(&self, process_id: u32, entity_id: EntityId) {
            self.pid_map
                .lock()
                .expect("PID map lock should not be poisoned")
                .insert(process_id, entity_id);
        }
    }

    impl CaptureEntityResolver for CaptureTestEntityResolver {
        fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
            self.pid_map
                .lock()
                .expect("PID map lock should not be poisoned")
                .get(&process_id)
                .cloned()
        }
    }

    fn packet_forwarder_from_sender(
        target_port: u16, packets_tx: mpsc::Sender<ForwardPacket>, metrics: super::metrics::Metrics,
    ) -> PacketForwarder {
        let mut forwarder =
            PacketForwarderTarget::new(MetaString::from_static("127.0.0.1"), target_port).to_forwarder(metrics);
        forwarder.connected = Arc::new(OnceLock::from(packets_tx));
        forwarder
    }

    fn processed_metric_key(listener_type: &'static str, origin: Option<&str>) -> Key {
        let mut labels = vec![
            Label::from_static_parts("component_id", "dogstatsd_test"),
            Label::from_static_parts("component_type", "source"),
            Label::from_static_parts("listener_type", listener_type),
            Label::from_static_parts("message_type", "metrics"),
        ];
        if let Some(origin) = origin {
            labels.push(Label::new("origin", origin.to_string()));
        }

        Key::from_parts("component_events_received_total", labels)
    }

    fn test_context_resolvers() -> ContextResolvers {
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver)
    }

    #[test]
    fn origin_telemetry_does_not_resolve_origin_when_origin_detection_is_disabled() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, true);
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let mut context_resolvers = test_context_resolvers();
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        }));
        let process_origin = resolve_process_origin(Some(&capture_entity_resolver), &peer_addr);

        let event = handle_frame(
            b"test_metric:1|c",
            &codec,
            &mut context_resolvers,
            &metrics,
            false,
            process_origin.as_ref(),
            EnablePayloadsFilter::default(),
            &[],
            &MetaString::from_static("default-host"),
        )
        .expect("frame should parse");

        assert!(event.is_some());
        assert_eq!(
            recorder.counter(processed_metric_key("unixgram", Some("container_id://pid-container"))),
            None
        );
        assert_eq!(recorder.counter(processed_metric_key("unixgram", Some(""))), Some(1));
    }

    #[test]
    fn origin_telemetry_records_resolved_origin_when_origin_detection_is_enabled() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, true);
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let mut context_resolvers = test_context_resolvers();
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        }));
        let process_origin = resolve_process_origin(Some(&capture_entity_resolver), &peer_addr);

        let event = handle_frame(
            b"test_metric:1|c",
            &codec,
            &mut context_resolvers,
            &metrics,
            true,
            process_origin.as_ref(),
            EnablePayloadsFilter::default(),
            &[],
            &MetaString::from_static("default-host"),
        )
        .expect("frame should parse");

        assert!(event.is_some());
        assert_eq!(
            recorder.counter(processed_metric_key("unixgram", Some("container_id://pid-container"))),
            Some(1)
        );
        assert_eq!(recorder.counter(processed_metric_key("unixgram", Some(""))), Some(0));
    }

    #[test]
    fn no_metrics_when_interner_full_allocations_disallowed() {
        // We're specifically testing here that when we don't allow outside allocations, we should not be able to
        // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
        // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
        // _inlined_ either, since that will get around the interner being full.
        //
        // We set our metric name to be longer than 31 bytes (the inlining limit) to ensure this.

        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_heap_allocations(false)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
        let input = "big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
            panic!("Failed to parse packet.");
        };

        let maybe_metric = handle_metric_packet(
            packet,
            &mut context_resolvers,
            None,
            &[],
            &MetaString::from_static("default-host"),
        );
        assert!(maybe_metric.is_none());
    }

    #[test]
    fn metric_host_tag_disambiguates_contexts_without_remaining_tag() {
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let mut context_resolvers = test_context_resolvers();
        let default_hostname = MetaString::from_static("default-host");

        let packets = [
            ("unset", b"test_metric_name:1|g".as_slice(), "default-host"),
            ("empty", b"test_metric_name:2|g|#host:".as_slice(), ""),
            (
                "explicit_default",
                b"test_metric_name:3|g|#host:default-host".as_slice(),
                "default-host",
            ),
            (
                "custom",
                b"test_metric_name:4|g|#host:custom-host".as_slice(),
                "custom-host",
            ),
        ];

        let mut metrics = Vec::new();
        for (case, raw, expected_host) in packets {
            let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(raw) else {
                panic!("Failed to parse {case} packet.");
            };
            let metric = handle_metric_packet(packet, &mut context_resolvers, None, &[], &default_hostname)
                .unwrap_or_else(|| panic!("{case} metric should resolve"));

            assert_eq!(metric.context().host(), Some(expected_host), "{case} context host");
            assert!(metric.context().tags().into_iter().all(|tag| tag.name() != "host"));
            metrics.push(metric);
        }

        assert_eq!(metrics[0].context(), metrics[2].context());
        assert_ne!(metrics[0].context(), metrics[1].context());
        assert_ne!(metrics[0].context(), metrics[3].context());
        assert_ne!(metrics[1].context(), metrics[3].context());
    }

    #[test]
    fn metric_with_additional_tags() {
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_heap_allocations(false)
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
        let existing_tags = ["tag1:value1", "tag2:value2", "tag3:value3"];
        let existing_tags_str = existing_tags.join(",");

        let input = format!("test_metric_name:1|c|#{}", existing_tags_str);
        let additional_tags = [
            "tag4:value4".to_string(),
            "tag5:value5".to_string(),
            "tag6:value6".to_string(),
        ];

        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
            panic!("Failed to parse packet.");
        };
        let maybe_metric = handle_metric_packet(
            packet,
            &mut context_resolvers,
            None,
            &additional_tags,
            &MetaString::from_static("default-host"),
        );
        assert!(maybe_metric.is_some());

        let metric = maybe_metric.unwrap();
        let context = metric.context();

        for tag in existing_tags {
            assert!(context.tags().has_tag(tag));
        }

        for tag in additional_tags {
            assert!(context.tags().has_tag(tag));
        }
    }

    fn deser_config(json: &str) -> DogStatsDConfiguration {
        serde_json::from_str(json).expect("failed to deserialize config")
    }

    fn udp_listen_address() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn tcp_listen_address() -> ListenAddress {
        ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn named_pipe_listen_address() -> ListenAddress {
        ListenAddress::named_pipe_with_input_buffer_size(
            "datadog-dogstatsd",
            default_windows_pipe_security_descriptor(),
            default_buffer_size() as u32,
        )
    }

    #[test]
    fn build_addresses_includes_named_pipe_when_configured() {
        let config = deser_config(
            r#"{
                "dogstatsd_port": 0,
                "dogstatsd_pipe_name": "datadog-dogstatsd"
            }"#,
        );

        let addresses = config.build_addresses(None);

        assert_eq!(addresses, vec![named_pipe_listen_address()]);
    }

    #[test]
    fn build_addresses_uses_dogstatsd_buffer_size_for_named_pipe_input_buffer() {
        let config = deser_config(
            r#"{
                "dogstatsd_port": 0,
                "dogstatsd_pipe_name": "datadog-dogstatsd",
                "dogstatsd_buffer_size": 16384
            }"#,
        );

        let addresses = config.build_addresses(None);

        let [ListenAddress::NamedPipe { input_buffer_size, .. }] = addresses.as_slice() else {
            panic!("expected only a named pipe listen address, got {addresses:?}");
        };
        assert_eq!(*input_buffer_size, Some(16_384));
    }

    #[test]
    fn eol_required_matches_named_pipe_listener_type() {
        let config = deser_config(r#"{"dogstatsd_eol_required": ["named_pipe"]}"#);
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&named_pipe_listen_address()));
        assert!(!eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));
    }

    #[test]
    fn interner_size_defaults_to_2mib() {
        let config = deser_config("{}");
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
    }

    #[test]
    fn socket_receive_buffer_size_defaults_to_zero() {
        let config = deser_config("{}");
        assert_eq!(config.socket_receive_buffer_size, 0);
    }

    #[test]
    fn socket_receive_buffer_size_from_config() {
        let config = deser_config(r#"{"dogstatsd_so_rcvbuf": 131072}"#);
        assert_eq!(config.socket_receive_buffer_size, 131_072);
    }

    #[test]
    fn stream_log_too_big_defaults_to_false() {
        let config = deser_config("{}");
        assert!(!config.stream_log_too_big);
    }

    #[test]
    fn stream_log_too_big_from_config() {
        let config = deser_config(r#"{"dogstatsd_stream_log_too_big": true}"#);
        assert!(config.stream_log_too_big);
    }

    #[test]
    fn disable_verbose_logs_defaults_to_false() {
        let config = deser_config("{}");
        assert!(!config.disable_verbose_logs);
    }

    #[test]
    fn disable_verbose_logs_from_config() {
        let config = deser_config(r#"{"dogstatsd_disable_verbose_logs": true}"#);
        assert!(config.disable_verbose_logs);
    }

    #[test]
    fn statsd_forward_defaults_disabled() {
        let config = deser_config("{}");
        assert!(config.statsd_forward_host.is_none());
        assert_eq!(config.statsd_forward_port, 0);
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_empty_host_disabled() {
        let config = deser_config(r#"{"statsd_forward_host": "", "statsd_forward_port": 9125}"#);
        assert!(config.statsd_forward_host.is_none());
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_zero_port_disabled() {
        let config = deser_config(r#"{"statsd_forward_host": "127.0.0.1", "statsd_forward_port": 0}"#);
        assert_eq!(config.statsd_forward_host.as_deref(), Some("127.0.0.1"));
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_host_and_port_enabled() {
        let config = deser_config(r#"{"statsd_forward_host": "127.0.0.1", "statsd_forward_port": 9125}"#);
        let (host, port) = config.statsd_forward_target().expect("forwarding should be enabled");
        assert_eq!(host.as_ref(), "127.0.0.1");
        assert_eq!(port, 9125);
    }

    #[test]
    fn statsd_forward_invalid_target_still_builds_forwarder_handle() {
        let config = deser_config(r#"{"statsd_forward_host": "not a valid host", "statsd_forward_port": 9125}"#);
        assert!(config.packet_forwarder_target().is_some());
    }

    #[tokio::test]
    async fn packet_forwarder_sends_payload_bytes() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("receiver should bind");
        let receiver_addr = receiver.local_addr().expect("receiver should have an address");
        let forwarder = ConnectedPacketForwarder::connect("127.0.0.1", receiver_addr.port())
            .await
            .expect("forwarder should connect");
        let payload = b"daemon:666|g|#sometag1:somevalue1,sometag2:somevalue2";

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, false);
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(receiver_addr.port(), packets_tx, metrics);

        packet_forwarder.forward(Bytes::copy_from_slice(payload)).await;

        let mut actual = [0u8; 128];
        let (received_len, _) = timeout(Duration::from_secs(1), receiver.recv_from(&mut actual))
            .await
            .expect("receive should not time out")
            .expect("receiver should receive payload");

        assert_eq!(&actual[..received_len], payload);
        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "ok"),
                ]
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                ]
            )),
            Some(payload.len() as u64)
        );
        worker.abort();
    }

    #[tokio::test]
    async fn packet_forwarder_sends_payload_bytes_to_ipv6_target() {
        let receiver = match UdpSocket::bind("[::1]:0").await {
            Ok(receiver) => receiver,
            Err(e) if is_ipv6_unavailable_error(&e) => return,
            Err(e) => panic!("receiver should bind: {e}"),
        };
        let receiver_addr = receiver.local_addr().expect("receiver should have an address");
        let forwarder = ConnectedPacketForwarder::connect("::1", receiver_addr.port())
            .await
            .expect("forwarder should connect");
        let payload = b"daemon:666|g|#ip:6";

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, false);
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(receiver_addr.port(), packets_tx, metrics);

        packet_forwarder.forward(Bytes::copy_from_slice(payload)).await;

        let mut actual = [0u8; 128];
        let (received_len, _) = timeout(Duration::from_secs(1), receiver.recv_from(&mut actual))
            .await
            .expect("receive should not time out")
            .expect("receiver should receive payload");

        assert_eq!(&actual[..received_len], payload);
        worker.abort();
    }

    #[tokio::test]
    async fn packet_forwarder_waits_when_queue_is_full() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, false);
        let (packets_tx, _packets_rx) = mpsc::channel(FORWARDER_QUEUE_CAPACITY);
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics);

        for _ in 0..FORWARDER_QUEUE_CAPACITY {
            packet_forwarder.forward(Bytes::from_static(b"queued:1|c")).await;
        }

        assert!(
            timeout(
                Duration::from_millis(100),
                packet_forwarder.forward(Bytes::from_static(b"blocked:1|c")),
            )
            .await
            .is_err(),
            "forwarding should wait for queue capacity instead of dropping"
        );
    }

    #[tokio::test]
    async fn packet_forwarder_send_error_increments_error_telemetry() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let context = test_component_context();
        let metrics = build_metrics(&listen_addr, &context, false);
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("socket should bind");
        let forwarder = ConnectedPacketForwarder {
            socket,
            target: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9125)),
        };
        let (packets_tx, packets_rx) = mpsc::channel(1);
        let worker = tokio::spawn(forwarder.run(packets_rx, metrics.clone()));
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics);

        packet_forwarder.forward(Bytes::from_static(b"daemon:666|g")).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            if recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "error"),
                ],
            )) == Some(1)
            {
                break;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "forwarding error telemetry should be recorded"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.abort();
    }

    #[test]
    fn unsupported_platform_process_credentials_do_not_count_as_origin_detection_telemetry_errors() {
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
            saluki_io::net::ProcessCredentialsError::UnsupportedPlatform,
        ));

        assert!(!origin_detection_failed_for_telemetry(true, 1, &peer_addr));
    }

    #[test]
    fn invalid_process_credentials_count_as_origin_detection_telemetry_errors() {
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
            saluki_io::net::ProcessCredentialsError::InvalidCredentials,
        ));

        assert!(origin_detection_failed_for_telemetry(true, 1, &peer_addr));
    }

    #[test]
    fn autoscale_udp_listeners_defaults_to_false() {
        let config = deser_config("{}");
        assert!(!config.autoscale_udp_listeners);
        assert!(config.udp_streams_to_yield().is_none());
    }

    #[test]
    fn effective_max_buffer_count_never_below_baseline() {
        // A legacy config that only raised `dogstatsd_buffer_count` keeps its full capacity rather than being capped
        // to the `dogstatsd_buffer_count_max` default.
        let legacy = deser_config(r#"{"dogstatsd_buffer_count": 65536}"#);
        assert_eq!(legacy.effective_max_buffer_count(), 65536);

        // An explicit maximum above the baseline is honored as-is.
        let explicit = deser_config(r#"{"dogstatsd_buffer_count": 128, "dogstatsd_buffer_count_max": 512}"#);
        assert_eq!(explicit.effective_max_buffer_count(), 512);

        // A maximum below the baseline is treated as equal to the baseline.
        let below = deser_config(r#"{"dogstatsd_buffer_count": 200, "dogstatsd_buffer_count_max": 64}"#);
        assert_eq!(below.effective_max_buffer_count(), 200);
    }

    #[test]
    fn decoder_worker_count_matches_core_agent_defaults() {
        assert_eq!(default_decoder_worker_count(1), 2);
        assert_eq!(default_decoder_worker_count(4), 2);
        assert_eq!(default_decoder_worker_count(8), 6);
    }

    #[test]
    fn decoder_worker_count_honors_explicit_override() {
        let config = deser_config(r#"{"dogstatsd_workers_count": 1}"#);

        assert_eq!(config.decoder_worker_count().get(), 1);
    }

    #[tokio::test]
    async fn global_datagram_receiver_distributes_packets_to_workers() {
        let (sender, receiver) = mpsc::channel(2);
        let receiver = Arc::new(Mutex::new(receiver));
        let first_worker = receiver.clone();
        let second_worker = receiver;
        let socket_context = test_datagram_socket_context(udp_listen_address());

        sender
            .send(QueuedDatagram {
                result: Err(std::io::Error::other("first")),
                socket_context: socket_context.clone(),
            })
            .await
            .expect("first packet should be queued");
        sender
            .send(QueuedDatagram {
                result: Err(std::io::Error::other("second")),
                socket_context,
            })
            .await
            .expect("second packet should be queued");

        let (first, second) = tokio::join!(async { first_worker.lock().await.recv().await }, async {
            second_worker.lock().await.recv().await
        },);

        assert!(first.expect("first worker should receive a packet").result.is_err());
        assert!(second.expect("second worker should receive a packet").result.is_err());
    }

    #[tokio::test]
    async fn shutdown_drains_queued_datagrams_after_listeners_stop() {
        let mut listener_shutdown_coordinator = ShutdownCoordinator::default();
        let listener_shutdown = listener_shutdown_coordinator.register();
        let (sender, mut receiver) = mpsc::channel(2);
        sender.send(()).await.expect("first datagram should be queued");
        sender.send(()).await.expect("second datagram should be queued");

        let listener_task = tokio::spawn(async move {
            listener_shutdown.await;
            drop(sender);
        });
        let decoded = Arc::new(AtomicUsize::new(0));
        let decoder_count = decoded.clone();
        let decoder_task = tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                decoder_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        shutdown_listeners_and_drain_datagram_decoders(listener_shutdown_coordinator, vec![decoder_task])
            .await
            .expect("datagram decoder should drain cleanly");
        listener_task.await.expect("listener task should stop cleanly");

        assert_eq!(decoded.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn dogstatsd_io_buffer_pool_grows_on_demand_until_limit() {
        let min_buffers = 2;
        let max_buffers = 3;
        let (pool, shrinker) = build_io_buffer_pool(min_buffers, max_buffers, default_buffer_size());

        let mut initial_buffers = Vec::with_capacity(min_buffers);
        for _ in 0..min_buffers {
            initial_buffers.push(
                timeout(Duration::from_secs(1), pool.acquire())
                    .await
                    .expect("initial buffer should be available"),
            );
        }
        let on_demand_buffer = timeout(Duration::from_secs(1), pool.acquire())
            .await
            .expect("pool should grow on demand before hitting the limit");

        let capped_acquire = timeout(Duration::from_millis(25), pool.acquire()).await;
        assert!(capped_acquire.is_err(), "pool should wait once it reaches the limit");

        drop(initial_buffers.pop().expect("initial buffer should still be held"));
        timeout(Duration::from_secs(1), pool.acquire())
            .await
            .expect("returned buffer should unblock acquisition");

        drop(on_demand_buffer);
        drop(shrinker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_datagram_reader_is_bounded_by_io_buffer_pool() {
        let temp_dir = tempfile::tempdir().expect("temp directory should be created");
        let socket_path = temp_dir.path().join("dogstatsd.socket");
        let receiver = UnixDatagram::bind(&socket_path).expect("receiver should bind");
        let sender = UnixDatagram::unbound().expect("sender should be created");
        let (pool, shrinker) = build_io_buffer_pool(2, 2, default_buffer_size());
        let (packets_tx, mut packets_rx) = mpsc::channel(3);
        let listen_addr = ListenAddress::Unixgram(socket_path.clone());
        let socket_context = Arc::new(DatagramSocketContext {
            metrics: build_metrics(&listen_addr, &test_component_context(), false),
            listen_addr,
            eol_required: false,
            packet_forwarder: None,
        });
        let reader = tokio::spawn(receive_connectionless_stream(
            Stream::from(receiver),
            pool,
            MemoryLimiter::noop(),
            None,
            packets_tx,
            socket_context,
        ));
        let payloads: [&[u8]; 3] = [b"first", b"second", b"third"];

        for payload in payloads {
            sender
                .send_to(payload, &socket_path)
                .await
                .expect("payload should send");
        }

        timeout(Duration::from_secs(1), async {
            while packets_rx.len() < 2 {
                yield_now().await;
            }
        })
        .await
        .expect("reader should fill the two-buffer pool");
        assert_eq!(packets_rx.len(), 2);

        let first = packets_rx
            .recv()
            .await
            .expect("first packet should be queued")
            .result
            .expect("first receive should succeed");
        assert_eq!(received_payload(&first.buffer, first.bytes_read), payloads[0]);
        drop(first);

        timeout(Duration::from_secs(1), async {
            while packets_rx.len() < 2 {
                yield_now().await;
            }
        })
        .await
        .expect("returning a buffer should allow the third packet to be read");

        for expected in &payloads[1..] {
            let received = packets_rx
                .recv()
                .await
                .expect("packet should be queued")
                .result
                .expect("receive should succeed");
            assert_eq!(received_payload(&received.buffer, received.bytes_read), *expected);
        }

        drop(packets_rx);
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop when the queue closes")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn uds_datagram_reader_pins_origin_before_decode() {
        let temp_dir = tempfile::tempdir().expect("temp directory should be created");
        let socket_path = temp_dir.path().join("dogstatsd.socket");
        let listen_addr = ListenAddress::Unixgram(socket_path.clone());
        let mut listener = Listener::from_listen_address(listen_addr.clone(), None)
            .await
            .expect("listener should bind");
        let stream = listener.accept().await.expect("listener should yield its socket");
        let sender = UnixDatagram::unbound().expect("sender should be created");
        let process_id = std::process::id();
        let original_entity = EntityId::from_local_data("ci-original-container").expect("container entity");
        let reused_entity = EntityId::from_local_data("ci-reused-container").expect("container entity");
        let capture_entity_resolver = Arc::new(CaptureTestEntityResolver::with_pid_mapping(
            process_id,
            original_entity.clone(),
        ));
        let (pool, shrinker) = build_io_buffer_pool(1, 1, default_buffer_size());
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let reader = tokio::spawn(receive_connectionless_stream(
            stream,
            pool,
            MemoryLimiter::noop(),
            Some(capture_entity_resolver.clone()),
            packets_tx,
            test_datagram_socket_context(listen_addr),
        ));

        sender
            .send_to(b"test.metric:1|c", &socket_path)
            .await
            .expect("payload should send");
        let received = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("packet should be received")
            .expect("reader should remain active")
            .result
            .expect("receive should succeed");
        assert_eq!(
            received.process_origin,
            Some(ProcessOrigin::Pinned(Some(original_entity.clone())))
        );

        // Simulate the sender exiting and its PID being reused before a decoder worker reaches the queued packet.
        capture_entity_resolver.set_pid_mapping(process_id, reused_entity.clone());

        let mut workload_provider = TestWorkloadProvider::new();
        workload_provider.add_entity(original_entity, &["container:original"]);
        workload_provider.add_entity(reused_entity, &["container:reused"]);
        let origin_config: OriginEnrichmentConfiguration =
            serde_json::from_value(json!({ "dogstatsd_origin_detection": true }))
                .expect("origin configuration should deserialize");
        let origin_resolver = DogStatsDOriginTagResolver::new(
            origin_config,
            Arc::new(workload_provider),
            super::CapturedTaggerHandle::new(),
        );
        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();
        let mut context_resolvers = ContextResolvers::manual_with_origin(
            context_resolver.clone(),
            context_resolver,
            tags_resolver,
            origin_resolver,
        );
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(b"test.metric:1|c") else {
            panic!("metric should parse");
        };
        let metric = handle_metric_packet(
            packet,
            &mut context_resolvers,
            received.process_origin.as_ref(),
            &[],
            &MetaString::from_static("default-host"),
        )
        .expect("metric context should resolve");

        assert!(metric.context().origin_tags().has_tag("container:original"));
        assert!(!metric.context().origin_tags().has_tag("container:reused"));

        drop(packets_rx);
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop when the queue closes")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_oriented_reader_preserves_partial_frames() {
        let (mut sender, receiver) = UnixStream::pair().expect("stream pair should be created");
        let (pool, shrinker) = build_io_buffer_pool(1, 1, default_buffer_size());
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let reader = tokio::spawn(receive_connected_stream(
            Stream::from(receiver),
            pool,
            MemoryLimiter::noop(),
            None,
            packets_tx,
        ));

        sender.write_all(b"partial").await.expect("first payload should send");
        let first = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("first read should finish")
            .expect("reader should remain active")
            .expect("first read should succeed");
        assert_eq!(first.buffer.chunk(), b"partial");
        let ReceivedBuffer {
            buffer, buffer_handoff, ..
        } = first;
        buffer_handoff.return_to_reader(buffer);

        sender.write_all(b"-frame").await.expect("second payload should send");
        let second = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("second read should finish")
            .expect("reader should remain active")
            .expect("second read should succeed");
        assert_eq!(second.bytes_read, b"-frame".len());
        assert_eq!(second.buffer.chunk(), b"partial-frame");
        let ReceivedBuffer {
            buffer, buffer_handoff, ..
        } = second;
        buffer_handoff.return_to_reader(buffer);

        drop(packets_rx);
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop when the queue closes")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn autoscale_udp_listeners_from_config_linux() {
        let config = deser_config(r#"{"dogstatsd_autoscale_udp_listeners": true}"#);
        assert!(config.autoscale_udp_listeners);

        let streams = config
            .udp_streams_to_yield()
            .expect("autoscale yields at least 1 stream");
        let n = streams.get();
        assert!(
            (1..=4).contains(&n),
            "expected 1..=4 streams from vCPU formula, got {n}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn warns_for_uds_origin_detection_on_non_linux() {
        let config = deser_config(
            r#"{
                "dogstatsd_origin_detection": true,
                "dogstatsd_port": 0,
                "dogstatsd_socket": "/tmp/dsd.sock"
            }"#,
        );
        let addresses = config.build_addresses(None);

        assert!(config.uds_origin_detection_unsupported_on_platform(&addresses));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn does_not_warn_for_udp_origin_detection_on_non_linux() {
        let config = deser_config(r#"{"dogstatsd_origin_detection": true}"#);
        let addresses = config.build_addresses(None);

        assert!(!config.uds_origin_detection_unsupported_on_platform(&addresses));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn autoscale_udp_listeners_from_config_non_linux() {
        let config = deser_config(r#"{"dogstatsd_autoscale_udp_listeners": true}"#);
        assert!(config.autoscale_udp_listeners);

        assert_eq!(None, config.udp_streams_to_yield());
    }

    #[test]
    fn eol_required_defaults_to_no_listeners() {
        let config = deser_config("{}");
        let eol_required = config.eol_required();

        assert!(!eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));
    }

    #[test]
    fn eol_required_matches_configured_listener_types() {
        let config = deser_config(r#"{"dogstatsd_eol_required": ["udp", "uds"]}"#);
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));

        #[cfg(unix)]
        {
            assert!(eol_required.for_listener(&ListenAddress::Unixgram("/tmp/dsd.sock".into())));
            assert!(eol_required.for_listener(&ListenAddress::Unix("/tmp/dsd-stream.sock".into())));
        }
    }

    #[test]
    fn eol_required_accepts_space_separated_string() {
        let config = deser_config(r#"{"dogstatsd_eol_required": "udp uds"}"#);
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&udp_listen_address()));
    }

    #[test]
    fn drops_full_named_pipe_buffer_without_newline() {
        let named_pipe_stream = named_pipe_listen_address();
        let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(8));
        buffer.put_slice(b"12345678");

        assert!(super::should_drop_oversized_named_pipe_frame(
            &named_pipe_stream,
            &buffer
        ));
    }

    #[test]
    fn keeps_named_pipe_partial_frame_when_buffer_has_capacity() {
        let named_pipe_stream = named_pipe_listen_address();
        let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(9));
        buffer.put_slice(b"12345678");

        assert!(!super::should_drop_oversized_named_pipe_frame(
            &named_pipe_stream,
            &buffer
        ));
    }

    #[test]
    fn keeps_full_named_pipe_buffer_with_newline() {
        let named_pipe_stream = named_pipe_listen_address();
        let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(8));
        buffer.put_slice(b"1234567\n");

        assert!(!super::should_drop_oversized_named_pipe_frame(
            &named_pipe_stream,
            &buffer
        ));
    }

    #[test]
    fn stream_log_too_big_warns_for_enabled_length_delimited_stream_invalid_frames() {
        let uds_stream = ListenAddress::Unix("/tmp/dsd-stream.sock".into());
        let named_pipe_stream = named_pipe_listen_address();
        let tcp_stream = ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
        let error = saluki_io::deser::framing::FramingError::InvalidFrame {
            frame_len: 8193,
            reason: "frame length exceeds buffer capacity",
        };

        assert!(super::should_warn_stream_log_too_big(&uds_stream, &error, true));
        assert!(!super::should_warn_stream_log_too_big(&uds_stream, &error, false));
        assert!(!super::should_warn_stream_log_too_big(&named_pipe_stream, &error, true));
        assert!(!super::should_warn_stream_log_too_big(&tcp_stream, &error, true));
    }

    #[test]
    fn interner_size_from_entry_count() {
        // A Core Agent migration config with entry count 4096 should yield 2 MiB, not 4096 bytes.
        let config = deser_config(r#"{"dogstatsd_string_interner_size": 4096}"#);
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
    }

    #[test]
    fn interner_size_from_explicit_bytes() {
        let config = deser_config(r#"{"dogstatsd_string_interner_size_bytes": 4194304}"#);
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(4194304));
    }

    #[test]
    fn interner_size_explicit_bytes_takes_priority() {
        let config = deser_config(
            r#"{"dogstatsd_string_interner_size": 4096, "dogstatsd_string_interner_size_bytes": 8388608}"#,
        );
        // The _bytes key (8 MiB) takes priority over the entry count.
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(8388608));
    }

    #[test]
    fn interner_size_custom_entry_count() {
        let config = deser_config(r#"{"dogstatsd_string_interner_size": 8192}"#);
        // 8192 entries * 512 bytes = 4 MiB
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(4));
    }

    /// Asserts that two lists of ListenAddress are equivalent.
    fn address_list_eq(expected: &mut [ListenAddress], actual: &mut [ListenAddress]) -> Result<(), String> {
        if expected.len() != actual.len() {
            return Err(format!(
                "length mismatch: expected {} addresses, got {}",
                expected.len(),
                actual.len()
            ));
        }

        expected.sort_by_key(|a| a.to_string());
        actual.sort_by_key(|a| a.to_string());

        for (e, a) in expected.iter().zip(actual.iter()) {
            let (es, as_) = (e.to_string(), a.to_string());
            if es != as_ {
                return Err(format!("address mismatch: expected {}, got {}", es, as_));
            }
        }

        Ok(())
    }

    /// This test verifies that we didn't accidentally break the `build_addresses_no_listeners` helper function which
    /// would render all further tests useless.
    #[test]
    fn build_addresses_assertion_function_works() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 123,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            // Close, but not quite! This is intentionally *not* 127.0.0.1 to test that the assertion will fail
            Ipv4Addr::new(127, 0, 0, 2),
            123,
        )))];
        let mut actual = config.build_addresses(None);
        assert!(address_list_eq(&mut expected, &mut actual).is_err())
    }

    /// With all four listener gates off, `build_addresses` returns an empty Vec.
    #[test]
    fn build_addresses_no_listeners() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// UDP port set, `non_local_traffic=false` -> UDP listener bound to `127.0.0.1`.
    #[test]
    fn build_addresses_udp_local_only() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            8125,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// UDP port set, `non_local_traffic=true` -> UDP listener bound to `0.0.0.0`.
    #[test]
    fn build_addresses_udp_non_local_only() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: true,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            8125,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// TCP port set, `non_local_traffic=false` -> TCP listener bound to `127.0.0.1`.
    #[test]
    fn build_addresses_tcp_local_only() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            9000,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// TCP port set, `non_local_traffic=true` -> TCP listener bound to `0.0.0.0`.
    #[test]
    fn build_addresses_tcp_non_local_only() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: None,
            non_local_traffic: true,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            9000,
        )))];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// `socket_path` set -> a `Unixgram` address is produced with that path.
    #[test]
    fn build_addresses_unixgram_only() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 0,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Unixgram("/tmp/dsd.sock".into())];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// `socket_stream_path` set -> a `Unix` (stream) address is produced with that path.
    #[test]
    fn build_addresses_unix_stream_only() {
        let config = DogStatsDConfiguration {
            port: 0,
            tcp_port: 0,
            socket_path: None,
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![ListenAddress::Unix("/tmp/dsd-stream.sock".into())];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// All four listener types enabled at once, with `non_local_traffic=true`.
    #[test]
    fn build_addresses_all_four_non_local() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: true,
            ..Default::default()
        };
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// All four listener types enabled at once, with `non_local_traffic=false`.
    #[test]
    fn build_addresses_all_four_local() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: false,
            ..Default::default()
        };
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(None);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=false` -> both UDP and TCP
    /// bind to that IP. Includes a UDS datagram socket to confirm `bind_host` doesn't affect it.
    #[test]
    fn build_addresses_bind_host_applies_to_udp_and_tcp() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 9000,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            socket_stream_path: None,
            non_local_traffic: false,
            ..Default::default()
        };
        let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 9000))),
            ListenAddress::Unixgram("/tmp/dsd.sock".into()),
        ];
        let mut actual = config.build_addresses(bind_host);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    /// Passing `Some(ip)` to `build_addresses` with `non_local_traffic=true` -> both UDP and TCP
    /// bind to `0.0.0.0`; the `bind_host` parameter is ignored (precedence matches the Agent).
    /// Includes a UDS stream socket to confirm `bind_host` doesn't affect it.
    #[test]
    fn build_addresses_non_local_clobbers_bind_host() {
        let config = DogStatsDConfiguration {
            port: 8125,
            tcp_port: 9000,
            socket_path: None,
            socket_stream_path: Some("/tmp/dsd-stream.sock".to_string()),
            non_local_traffic: true,
            ..Default::default()
        };
        let bind_host = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        let mut expected = vec![
            ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 8125))),
            ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 9000))),
            ListenAddress::Unix("/tmp/dsd-stream.sock".into()),
        ];
        let mut actual = config.build_addresses(bind_host);
        address_list_eq(&mut expected, &mut actual).unwrap();
    }

    #[test]
    fn non_finite_metric_values_are_silently_dropped() {
        // The Datadog Agent sends NaN gauges (for example, encode_ms.avg computed as 0.0/0.0 in Go).
        // FloatIter skips non-finite values with a debug log, so decode_packet returns Ok with
        // num_points == 0. handle_frame then returns Ok(None) for zero-point packets, which is
        // the existing silent-drop path (no warning emitted).
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        for input in &[b"my.gauge:NaN|g" as &[u8], b"my.gauge:inf|g", b"my.gauge:-inf|g"] {
            match codec.decode_packet(input).expect("should decode without error") {
                ParsedPacket::Metric(packet) => assert_eq!(
                    packet.num_points, 0,
                    "non-finite value should be dropped, leaving 0 valid points"
                ),
                _ => panic!("expected Metric packet"),
            }
        }
    }

    #[tokio::test]
    async fn fix_empty_capture_path_sets_path_from_run_path() {
        const RUN_PATH: &str = "/my/little/run_path";

        let base_config_values = json!({ "run_path": RUN_PATH });
        let (config, _) = ConfigurationLoader::for_tests(Some(base_config_values), None, false).await;

        let dogstatsd_config = DogStatsDConfiguration::from_configuration(&config).expect("should deserialize");

        let expected = PathBuf::from(RUN_PATH).join(DOGSTATSD_CAPTURE_DIR);
        assert_eq!(expected, dogstatsd_config.capture_path);
    }

    #[tokio::test]
    async fn fix_empty_capture_path_keeps_explicit_path() {
        const RUN_PATH: &str = "/my/little/run_path";
        const CAPTURE_PATH: &str = "/custom/path/to/capture";

        let base_config_values = json!({ "run_path": RUN_PATH, "dogstatsd_capture_path": CAPTURE_PATH });
        let (config, _) = ConfigurationLoader::for_tests(Some(base_config_values), None, false).await;

        let dogstatsd_config = DogStatsDConfiguration::from_configuration(&config).expect("should deserialize");

        assert_eq!(PathBuf::from(CAPTURE_PATH), dogstatsd_config.capture_path);
    }

    #[tokio::test]
    async fn from_configuration_normalizes_capture_depth() {
        let cases = [
            (json!({}), MIN_CAPTURE_DEPTH),
            (json!({ "dogstatsd_capture_depth": 0 }), MIN_CAPTURE_DEPTH),
            (json!({ "dogstatsd_capture_depth": 2048 }), 2048),
        ];

        for (base_config_values, expected_depth) in cases {
            let (config, _) = ConfigurationLoader::for_tests(Some(base_config_values), None, false).await;
            let dogstatsd_config = DogStatsDConfiguration::from_configuration(&config).expect("should deserialize");

            assert_eq!(expected_depth, dogstatsd_config.capture_depth);
        }
    }

    #[test]
    fn capture_entity_resolver_is_configured_separately_from_workload_provider() {
        let config =
            DogStatsDConfiguration::default().with_capture_entity_resolver(CaptureTestEntityResolver::default());

        assert!(config.capture_entity_resolver.is_some());
        assert!(config.workload_provider.is_none());
    }

    #[test]
    fn resolve_process_origin_pins_live_entity() {
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        }));

        assert_eq!(
            resolve_process_origin(Some(&capture_entity_resolver), &peer_addr),
            Some(ProcessOrigin::Pinned(Some(
                EntityId::from_local_data("ci-pid-container").expect("container entity")
            )))
        );
    }

    #[test]
    fn build_capture_record_ignores_payload_local_data() {
        let record = super::build_capture_record(None, None, b"test.metric:1|c|c:ci-local-container\n");

        assert_eq!(record.container_id, None);
        assert!(record.ancillary.is_empty());
    }

    #[test]
    fn stream_capture_state_preserves_last_pid_without_new_creds() {
        let mut stream_capture = super::StreamCaptureState::new();

        stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(
            ProcessCredentials {
                pid: 42,
                uid: 0,
                gid: 0,
            },
        )));
        stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable));

        assert_eq!(stream_capture.last_pid, Some(42));
    }

    #[test]
    fn resolve_process_origin_preserves_live_pid_without_entity_resolver() {
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 12345,
            uid: 1000,
            gid: 1000,
        }));

        assert_eq!(
            resolve_process_origin(None, &peer_addr),
            Some(ProcessOrigin::Unpinned(12345))
        );
    }

    #[test]
    fn resolve_process_origin_unpacks_captured_pid_when_replay_gid_present() {
        let captured_pid: u32 = 99887766;
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 12345,        // our PID (irrelevant for replay)
            uid: captured_pid, // captured PID packed by the sender
            gid: super::REPLAY_CREDENTIALS_GID,
        }));

        assert_eq!(
            resolve_process_origin(None, &peer_addr),
            Some(ProcessOrigin::Replay(captured_pid))
        );
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DogStatsDConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DOGSTATSD_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DogStatsDConfiguration>()
                    .expect("DogStatsDConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
