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
    io, mem,
    num::NonZeroUsize,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes};
use bytesize::ByteSize;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::spawn_traced_named,
};
use saluki_context::tags::{RawTags, RawTagsFilter};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter, UsageExpr};
use saluki_core::data_model::event::{
    eventd::EventD,
    metric::{Metric, MetricMetadata, MetricOrigin},
    service_check::ServiceCheck,
    Event, EventType,
};
use saluki_core::{
    components::{sources::*, BuildContext},
    pooling::{ElasticObjectPool, ObjectPool as _},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_env::{workload::CaptureEntityResolver, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _, CollapsibleReadWriteIoBuffer as _, FixedSizeVec, ReadIoBuffer as _},
    deser::{
        codec::dogstatsd::*,
        framing::{Framer as _, FramingError, LengthDelimitedFramer},
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, ProcessIdentity, Stream,
    },
};
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
use self::framer::{get_framer, DsdFramer};
use crate::sources::dogstatsd::tags::{WellKnownTags, WellKnownTagsFilterPredicate};

mod filters;
use self::filters::EnablePayloadsFilter;

mod metrics;
use self::metrics::{build_metrics, Metrics};

mod replay;
use self::replay::{CaptureRecord, CapturedTaggerHandle, TrafficCapture};
pub use self::replay::{
    DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDReplayAPIHandler, DogStatsDReplayControl,
    ReplaySession, TimestampResolution, TrafficCaptureReader, DEFAULT_REPLAY_LOOPS, REPLAY_CREDENTIALS_GID,
};

mod origin;
pub use self::origin::OriginEnrichmentConfiguration;
use self::origin::{
    origin_from_event_packet, origin_from_metric_packet, origin_from_service_check_packet, DogStatsDOriginTagResolver,
    ProcessOrigin,
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

/// Controls which payload types are forwarded to the backend.
pub struct EnablePayloadsConfiguration {
    /// Whether or not to enable sending series (counter/gauge/rate) payloads.
    pub series: bool,

    /// Whether or not to enable sending sketch (distribution) payloads.
    pub sketches: bool,

    /// Whether or not to enable sending event payloads.
    pub events: bool,

    /// Whether or not to enable sending service check payloads.
    pub service_checks: bool,
}

const MIN_CAPTURE_DEPTH: usize = 1024;

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
pub struct DogStatsDConfiguration {
    /// Hostname used when DogStatsD metrics do not carry an explicit `host:` tag.
    pub default_hostname: MetaString,

    /// The size of the buffer used to receive messages into, in bytes.
    ///
    /// Payloads can't exceed this size, or they will be truncated, leading to discarded messages.
    pub buffer_size: usize,

    /// The number of message buffers to allocate up front.
    ///
    /// This is the baseline pool size allocated at startup. The pool then grows on demand up to
    /// `buffer_count_max` as active stream connections and datagram queues need additional buffers.
    /// Higher values allocate more memory at startup but reduce on-demand allocations during bursts.
    pub buffer_count: usize,

    /// The maximum number of message buffers to allocate overall.
    ///
    /// The global pool starts at `buffer_count` buffers and grows on demand up to this limit. Active stream
    /// connections use these buffers for reads, while connectionless listeners use them to queue received packets for
    /// decoding. Increasing this value lets datagram listeners absorb larger bursts at the cost of up to one additional
    /// `buffer_size` allocation per buffer. This limit bounds payload buffers, but not per-connection task and channel
    /// bookkeeping. After a short grace period without pool growth, ADP releases idle buffers until the pool returns to
    /// `buffer_count`.
    ///
    /// The pool never holds fewer buffers than `buffer_count`, so a value below the baseline is treated as equal to it.
    pub buffer_count_max: usize,

    /// The number of workers in the global pool that decodes connectionless DogStatsD packets.
    ///
    /// If set to `0`, the worker count is derived from the available vCPUs using the Core Agent's default formula.
    /// Positive values force an exact worker count. Higher values can improve throughput when decoding is CPU-bound,
    /// but also increase task scheduling and per-worker event buffering overhead.
    pub workers_count: usize,

    /// The port to listen on in UDP mode.
    ///
    /// If set to `0`, UDP isn't used.
    pub port: u16,

    /// The size of the receive buffer requested for each DogStatsD socket, in bytes.
    ///
    /// Applies to the UDP, TCP, and UDS sockets alike. If set to `0`, the OS default is used.
    pub socket_receive_buffer_size: usize,

    /// The port to listen on in TCP mode.
    ///
    /// If set to `0`, TCP isn't used.
    pub tcp_port: u16,

    /// The host to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this value is set and `statsd_forward_port` is non-zero. Setup failures
    /// are logged, and send failures are tracked through telemetry.
    pub statsd_forward_host: Option<MetaString>,

    /// The port to forward framed DogStatsD messages to over UDP.
    ///
    /// Forwarding is enabled only when this value is non-zero and `statsd_forward_host` is set.
    pub statsd_forward_port: u16,

    /// The Unix domain socket path to listen on, in datagram mode.
    ///
    /// If not set, UDS (in datagram mode) isn't used.
    pub socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode.
    ///
    /// If not set, UDS (in stream mode) isn't used.
    pub socket_stream_path: Option<String>,

    /// Controls whether ADP logs oversized DogStatsD stream frames.
    ///
    /// When set to `true`, ADP emits a warning when a UDS stream frame exceeds the
    /// configured DogStatsD buffer size. The frame is still rejected either way.
    ///
    /// Enable this when diagnosing clients that send oversized UDS stream frames.
    pub stream_log_too_big: bool,

    /// The Windows named pipe name to listen on.
    ///
    /// If set, ADP listens for DogStatsD stream traffic on `\\.\pipe\<name>` on Windows.
    /// The listener is unsupported on non-Windows platforms.
    pub pipe_name: Option<String>,

    /// Windows named pipe security descriptor.
    ///
    /// This SDDL descriptor is applied when creating the named pipe listener.
    pub windows_pipe_security_descriptor: String,

    /// Whether ADP lowers DogStatsD parse-failure logs to debug level.
    ///
    /// When set to `true`, invalid metrics, events, and service checks still increment decode-failure telemetry, but
    /// their parse-failure logs are emitted at debug level instead of warning level. Enable this to suppress noisy
    /// parse-error logs from misbehaving clients.
    pub disable_verbose_logs: bool,

    /// Listener types that require DogStatsD messages to be newline-terminated.
    ///
    /// Valid values are `udp`, `uds`, and `named_pipe`. Invalid values are ignored.
    ///
    /// An empty list accepts the final message without a newline.
    pub eol_required: Vec<String>,

    /// The host address to bind DogStatsD UDP and TCP listeners to.
    ///
    /// When set, UDP and TCP listeners bind to this address. Accepts either an IP literal (for example,
    /// `192.168.1.50`, `::1`) or a hostname that resolves via DNS (for example, `agent.internal`).
    /// Ignored when `non_local_traffic` is `true`, and binds to `127.0.0.1` when unset.
    pub bind_host: Option<String>,

    /// Whether or not to listen for non-local traffic.
    ///
    /// If set to `true`, the UDP and TCP listeners bind to `0.0.0.0` and accept traffic from any interface. Otherwise,
    /// they bind to `bind_host`, or `127.0.0.1` if `bind_host` isn't set.
    pub non_local_traffic: bool,

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
    pub autoscale_udp_listeners: bool,

    /// Whether or not to allow heap allocations when resolving contexts.
    ///
    /// When resolving contexts during parsing, the metric name and tags are interned to reduce memory usage. The
    /// interner has a fixed size, however, which means some strings can fail to be interned if the interner is full.
    /// When set to `true`, we allow these strings to be allocated on the heap like normal, but this can lead to
    /// increased (unbounded) memory usage. When set to `false`, if the metric name and all of its tags can't be
    /// interned, the metric is skipped.
    pub allow_context_heap_allocations: bool,

    /// Whether or not to enable support for no-aggregation pipelines.
    ///
    /// When enabled, this influences how metrics are parsed, specifically around user-provided metric timestamps. When
    /// metric timestamps are present, it's used as a signal to any aggregation transforms that the metric shouldn't
    /// be aggregated.
    pub no_aggregation_pipeline_support: bool,

    /// Number of entries for the string interner, as interpreted by the Core Datadog Agent.
    ///
    /// When `context_string_interner_size_bytes` isn't set, this value is multiplied by 512 bytes per entry to
    /// derive the interner byte size. This provides backwards compatibility for customers migrating configurations
    /// from the Core Agent, where this setting represents an entry count rather than a byte size.
    pub context_string_interner_entry_count: u64,

    /// Total size of the string interner used for contexts, in bytes.
    ///
    /// When set, this takes priority over `context_string_interner_entry_count`. This controls the amount of memory
    /// that can be used to intern metric names and tags. If the interner is full, metrics with contexts that haven't
    /// already been resolved may or may not be dropped, depending on the value of `allow_context_heap_allocations`.
    pub context_string_interner_size_bytes: Option<ByteSize>,

    /// The maximum number of cached contexts to allow.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit doesn't affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    pub cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// This is the maximum number of resolved tagsets that can be cached at any given time. This limit doesn't affect
    /// the total number of tagsets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    pub cached_tagsets_limit: usize,

    /// The number of seconds after which cached contexts will expire.
    ///
    /// Higher values allow for more effective caching for sparse metrics at the cost of increased memory usage.
    pub context_expiry_seconds: u64,

    /// Whether or not to enable permissive mode in the decoder.
    ///
    /// Permissive mode allows the decoder to relax its strictness around the allowed payloads, which lets it match the
    /// decoding behavior of the Datadog Agent.
    pub permissive_decoding: bool,

    /// The minimum sample rate allowed for metrics.
    ///
    /// When metrics are sent with a sample rate _lower_ than this value then it will be clamped to this value. This is
    /// done in order to ensure an upper bound on how many equivalent samples are tracked for the metric, as high sample
    /// rates (very small numbers, such as `0.00000001`) can lead to large memory growth.
    ///
    /// A warning log will be emitted when clamping occurs, as this represents an effective loss of metric samples.
    pub minimum_sample_rate: f64,

    /// Which payload types to forward to the backend.
    pub enable_payloads: EnablePayloadsConfiguration,

    /// Configuration related to origin detection and enrichment.
    pub origin_enrichment: OriginEnrichmentConfiguration,

    /// Whether to break down DogStatsD processed-metric telemetry by UDS origin.
    ///
    /// When enabled, metric-message `dogstatsd.processed` telemetry includes an `origin` label derived from the
    /// sender's UDS origin. This can add one telemetry series per origin and should primarily be used for diagnostics.
    pub origin_telemetry_enabled: bool,

    /// Workload provider to utilize for origin enrichment.
    ///
    /// Detected origins are only resolved to workload tags when a provider is set. Origin detection itself is
    /// controlled by `origin_enrichment`.
    pub workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,

    /// Resolver to use for mapping live sender PIDs to container entities before deferred processing.
    ///
    /// This resolver pins the sender entity while socket credentials are current so origin enrichment and traffic
    /// capture do not resolve a stale or reused PID later. It is set separately from the workload provider because it
    /// only needs a narrow live-PID lookup.
    pub capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,

    /// Additional tags to add to all metrics.
    ///
    /// These are sorted and deduplicated before use, so callers may append tags required by the running environment.
    pub additional_tags: Vec<String>,

    /// The directory where DogStatsD capture files are written by default.
    ///
    /// When set to an empty path, callers must provide an explicit capture path when starting a capture session.
    pub capture_path: PathBuf,

    /// The maximum number of captured packets that can be queued for persistence.
    ///
    /// This controls the depth of the in-process capture queue. Values below `1024` are raised to `1024` before the
    /// capture writer starts, preventing a zero-depth rendezvous channel from serializing DogStatsD stream handlers
    /// behind capture persistence.
    pub capture_depth: usize,

    /// Control surface that starts and stops DogStatsD traffic capture.
    ///
    /// This is runtime wiring rather than configuration: the source binds the capture it creates to this handle, so
    /// the caller creates the handle and keeps a clone to serve the capture API.
    pub capture_control: DogStatsDCaptureControl,

    /// Control surface that starts and stops DogStatsD traffic replay.
    ///
    /// This is runtime wiring rather than configuration: the source binds the captured tagger to this handle, so the
    /// caller creates the handle and keeps a clone to serve the replay API.
    pub replay_control: DogStatsDReplayControl,
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

#[cfg(test)]
impl DogStatsDConfiguration {
    /// Creates a fixture configuration, for tests that exercise source behavior rather than configuration.
    ///
    /// Values match the effective defaults the configuration system resolves, so a test only states the settings it
    /// cares about. `capture_depth` is the exception: a default configuration leaves it at `0`, which the capture
    /// writer raises to its minimum, so the fixture states that minimum directly.
    fn for_test() -> Self {
        Self {
            default_hostname: MetaString::default(),
            buffer_size: 8192,
            buffer_count: 128,
            buffer_count_max: 32_768,
            workers_count: 0,
            port: 8125,
            socket_receive_buffer_size: 0,
            tcp_port: 0,
            statsd_forward_host: None,
            statsd_forward_port: 0,
            socket_path: None,
            socket_stream_path: None,
            stream_log_too_big: false,
            pipe_name: None,
            windows_pipe_security_descriptor: "D:AI(A;;GA;;;WD)".to_string(),
            disable_verbose_logs: false,
            eol_required: Vec::new(),
            bind_host: None,
            non_local_traffic: false,
            autoscale_udp_listeners: false,
            allow_context_heap_allocations: true,
            no_aggregation_pipeline_support: true,
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: None,
            cached_contexts_limit: 500_000,
            cached_tagsets_limit: 500_000,
            context_expiry_seconds: 20,
            permissive_decoding: true,
            minimum_sample_rate: 0.000000003845,
            enable_payloads: EnablePayloadsConfiguration {
                series: true,
                sketches: true,
                events: true,
                service_checks: true,
            },
            origin_enrichment: OriginEnrichmentConfiguration::for_test(),
            origin_telemetry_enabled: false,
            workload_provider: None,
            capture_entity_resolver: None,
            additional_tags: Vec::new(),
            capture_path: PathBuf::new(),
            capture_depth: MIN_CAPTURE_DEPTH,
            capture_control: DogStatsDCaptureControl::default(),
            replay_control: DogStatsDReplayControl::default(),
        }
    }
}

impl DogStatsDConfiguration {
    /// Gets the effective source-wide DogStatsD tags.
    fn additional_tags(&self) -> Vec<String> {
        let mut tags = self.additional_tags.clone();
        tags.sort_unstable();
        tags.dedup();
        tags
    }

    /// Returns the effective string interner size in bytes.
    ///
    /// If `context_string_interner_size_bytes` is set, it's used directly. Otherwise,
    /// `context_string_interner_entry_count` is multiplied by 512 bytes per entry to derive the byte size.
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
        self.origin_enrichment.enabled
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
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
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

        let origin_detection_enabled = self.origin_enrichment.enabled;
        // Single CapturedTaggerHandle is cloned to both the resolver (reader of the captured store) and the replay
        // control surface (writer). Both sides reference the same atomic slot.
        let captured_tagger = CapturedTaggerHandle::new();

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(|provider| {
            DogStatsDOriginTagResolver::new(self.origin_enrichment.clone(), provider, captured_tagger.clone())
        });
        let context_resolvers = ContextResolvers::new(self, context.component_context(), maybe_origin_tags_resolver)
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
            self.capture_depth,
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
            origin_telemetry_enabled: self.origin_telemetry_enabled,
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
    origin_telemetry_enabled: bool,
    eol_required: EolRequired,
    decoder_context: DecoderContext,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

#[derive(Clone)]
struct HandlerContext {
    listen_addr: ListenAddress,
    eol_required: bool,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    datagram_sender: mpsc::Sender<QueuedDatagram>,
    datagram_context: Option<Arc<DatagramSocketContext>>,
    metrics: Metrics,
    decoder_context: DecoderContext,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    packet_forwarder: Option<PacketForwarder>,
}

#[derive(Clone)]
struct DecoderContext {
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    enabled_filter: EnablePayloadsFilter,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
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

struct DogStatsDDecoder {
    source_context: SourceContext,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    enabled_filter: EnablePayloadsFilter,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    additional_tags: Arc<[String]>,
    traffic_capture: TrafficCapture,
    event_buffer: Option<EventsBuffer>,
}

#[derive(Clone, Copy)]
enum BufferDecodeMode {
    Connectionless,
    Connected,
}

impl BufferDecodeMode {
    fn is_eof(self, bytes_read: usize) -> bool {
        match self {
            Self::Connectionless => true,
            Self::Connected => bytes_read == 0,
        }
    }

    fn should_stop_on_eof(self, eof: bool) -> bool {
        matches!(self, Self::Connected) && eof
    }

    fn should_stop_on_framing_error(self) -> bool {
        matches!(self, Self::Connected)
    }
}

struct BufferDecodeContext<'a> {
    listen_addr: &'a ListenAddress,
    metrics: &'a Metrics,
    packet_forwarder: Option<&'a PacketForwarder>,
    mode: BufferDecodeMode,
    framer: DsdFramer,
    stream_capture: StreamCaptureState,
}

impl<'a> BufferDecodeContext<'a> {
    fn new(
        listen_addr: &'a ListenAddress, eol_required: bool, metrics: &'a Metrics,
        packet_forwarder: Option<&'a PacketForwarder>, mode: BufferDecodeMode,
    ) -> Self {
        Self {
            listen_addr,
            metrics,
            packet_forwarder,
            mode,
            framer: get_framer(listen_addr, eol_required),
            stream_capture: StreamCaptureState::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeOutcome {
    Continue,
    Stop,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();

        context
            .spawner()
            .spawn_interruptible("io_buffer_pool_shrinker", self.io_buffer_pool_shrinker)
            .await
            .error_context("Failed to spawn I/O buffer pool shrinker.")?;

        let (datagram_sender, datagram_receiver) = mpsc::channel(self.io_buffer_queue_capacity);
        let datagram_receiver = Arc::new(Mutex::new(datagram_receiver));
        let decoder_context = DecoderContext {
            codec: self.codec.clone(),
            context_resolvers: self.context_resolvers.clone(),
            default_hostname: self.default_hostname.clone(),
            enabled_filter: self.enabled_filter,
            origin_detection_enabled: self.origin_detection_enabled,
            stream_log_too_big: self.stream_log_too_big,
            disable_verbose_logs: self.disable_verbose_logs,
            additional_tags: self.additional_tags.clone(),
            traffic_capture: self.traffic_capture.clone(),
        };

        // Decoders must drain their queue to completion, so they deliberately ignore the shutdown signal and stop only
        // once the datagram channel closes -- which happens when every listener and stream handler has dropped its
        // sender. The coordinator here is only used for its handle-drop accounting, so we can wait for that draining
        // to finish; see `shutdown_listeners_and_drain_datagram_decoders`.
        let mut decoder_shutdown_coordinator = ShutdownCoordinator::default();
        for worker_id in 0..self.decoder_worker_count.get() {
            let decoder_shutdown = decoder_shutdown_coordinator.register();
            let datagram_receiver = datagram_receiver.clone();
            let decoder_source_context = context.clone();
            let decoder_context = decoder_context.clone();

            context
                .spawner()
                .spawn_noninterruptible(format!("datagram_decoder_{worker_id}"), move |_shutdown| async move {
                    let _decoder_shutdown = decoder_shutdown;
                    process_datagram_decoder(datagram_receiver, decoder_source_context, decoder_context).await;
                })
                .await
                .error_context("Failed to spawn DogStatsD datagram decoder.")?;
        }
        drop(datagram_receiver);

        let mut listener_shutdown_coordinator = ShutdownCoordinator::default();
        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
            let task_name = format!("listener_{}", listener.listen_address().listener_type());
            let listener_source_context = context.clone();

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
                origin_telemetry_enabled: self.origin_telemetry_enabled,
                eol_required: self.eol_required,
                decoder_context: decoder_context.clone(),
                capture_entity_resolver: self.capture_entity_resolver.clone(),
                packet_forwarder_target: self.packet_forwarder_target.clone(),
            };

            context
                .spawner()
                .spawn_noninterruptible(task_name, move |shutdown| {
                    process_listener(listener_source_context, listener_context, shutdown)
                })
                .await
                .error_context("Failed to spawn DogStatsD listener.")?;
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

        shutdown_listeners_and_drain_datagram_decoders(listener_shutdown_coordinator, decoder_shutdown_coordinator)
            .await;

        debug!("DogStatsD source stopped.");

        Ok(())
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
    source_context: SourceContext, listener_context: ListenerContext, process_shutdown: ShutdownHandle,
) {
    let ListenerContext {
        shutdown_handle,
        mut listener,
        datagram_sender,
        io_buffer_pool,
        origin_telemetry_enabled,
        eol_required,
        decoder_context,
        capture_entity_resolver,
        packet_forwarder_target,
    } = listener_context;

    pin!(shutdown_handle, process_shutdown);

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
        if let Err(e) = packet_forwarder.spawn_connect(source_context.spawner()).await {
            warn!(%listen_addr, error = %e, "Could not start statsd packet forwarding.");
        }
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
            // This separate shutdown path is when we've been _explicitly_ signaled by the supervisor itself to
            // shutdown, rather than a logical/orderly topology shutdown. This is a corner case for when a component is
            // being forcefully shutdown for some reason.
            _ = &mut process_shutdown => {
                debug!(%listen_addr, "Supervisor signalled shutdown. Waiting for existing stream handlers to finish...");
                break;
            }
            result = listener.accept() => match result {
                Ok(stream) => {
                    debug!(%listen_addr, "Spawning new stream handler.");

                    let handler_context = HandlerContext {
                        listen_addr: listen_addr.clone(),
                        eol_required: eol_required.for_listener(&listen_addr),
                        io_buffer_pool: io_buffer_pool.clone(),
                        datagram_sender: datagram_sender.clone(),
                        datagram_context: datagram_context.clone(),
                        metrics: metrics.clone(),
                        decoder_context: decoder_context.clone(),
                        capture_entity_resolver: capture_entity_resolver.clone(),
                        packet_forwarder: packet_forwarder.clone(),
                    };

                    let task_name = format!("conn_{}", listen_addr.listener_type());

                    // The coordinator handle stays even though this is now a supervised child. Supervision makes the
                    // handler a sibling of the listener, not a descendant, so it alone wouldn't keep "the listener
                    // waits for its own streams" true -- the handle-drop accounting below is what does.
                    let stream_shutdown = stream_shutdown_coordinator.register();
                    let handler_source_context = source_context.clone();
                    let handler = process_stream(stream, handler_source_context, handler_context, stream_shutdown);

                    if let Err(e) = source_context.spawner().spawn_interruptible(task_name, handler).await {
                        error!(%listen_addr, error = %e, "Failed to spawn stream handler.");
                    }
                }
                Err(e) => {
                    // TODO: We shouldn't actually bail out here just because of an error during accept,
                    // since it could be a temporary failure like hitting the open file limit on the system.
                    //
                    // However, we need to add sufficient guardrails to `Listener::accept` so that retrying doesn't
                    // lead to thrashing in an endless loop or anything... so I'm leaving it like this for now.
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
) {
    select! {
        _ = shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, handler_context) => {},
    }
}

fn origin_detection_failed_for_telemetry(
    origin_detection_enabled: bool, bytes_read: usize, peer_addr: &ConnectionAddress,
) -> bool {
    origin_detection_enabled && bytes_read > 0 && peer_addr.has_process_credential_telemetry_error()
}

struct ReceivedBuffer {
    buffer: Option<BytesBuffer>,
    bytes_read: usize,
    peer_addr: ConnectionAddress,
    process_origin: Option<ProcessOrigin>,
    buffer_sender: Option<oneshot::Sender<BytesBuffer>>,
}

impl ReceivedBuffer {
    fn with_return(
        buffer: BytesBuffer, bytes_read: usize, peer_addr: ConnectionAddress, process_origin: Option<ProcessOrigin>,
    ) -> (Self, oneshot::Receiver<BytesBuffer>) {
        let (buffer_sender, returned_buffer) = oneshot::channel();
        (
            Self {
                buffer: Some(buffer),
                bytes_read,
                peer_addr,
                process_origin,
                buffer_sender: Some(buffer_sender),
            },
            returned_buffer,
        )
    }

    fn without_return(
        buffer: BytesBuffer, bytes_read: usize, peer_addr: ConnectionAddress, process_origin: Option<ProcessOrigin>,
    ) -> Self {
        Self {
            buffer: Some(buffer),
            bytes_read,
            peer_addr,
            process_origin,
            buffer_sender: None,
        }
    }

    fn parts_mut(&mut self) -> (&mut BytesBuffer, usize, &ConnectionAddress, Option<&ProcessOrigin>) {
        let Self {
            buffer,
            bytes_read,
            peer_addr,
            process_origin,
            ..
        } = self;
        (
            buffer.as_mut().expect("Received buffer already taken."),
            *bytes_read,
            peer_addr,
            process_origin.as_ref(),
        )
    }

    #[cfg(all(test, unix))]
    fn buffer(&self) -> &BytesBuffer {
        self.buffer.as_ref().expect("Received buffer already taken.")
    }

    #[cfg(all(test, unix))]
    fn buffer_mut(&mut self) -> &mut BytesBuffer {
        self.buffer.as_mut().expect("Received buffer already taken.")
    }
}

impl Drop for ReceivedBuffer {
    fn drop(&mut self) {
        if let Some(sender) = self.buffer_sender.take() {
            let buffer = self.buffer.take().expect("Received buffer already taken.");
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
        origin_detection_enabled: bool, traffic_capture: TrafficCapture,
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
                origin_detection_enabled,
                traffic_capture,
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
    origin_detection_enabled: bool, traffic_capture: TrafficCapture,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    packets_tx: mpsc::Sender<io::Result<ReceivedBuffer>>,
) {
    debug!("Stream reader started.");

    let mut retained_buffer: Option<BytesBuffer> = None;
    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = match retained_buffer.take() {
            Some(mut buffer) => {
                buffer.collapse();
                buffer
            }
            None => acquire_io_buffer(&io_buffer_pool).await,
        };
        let (bytes_read, peer_addr) = match stream.receive(&mut buffer).await {
            Ok(received) => received,
            Err(error) => {
                let _ = packets_tx.send(Err(error)).await;
                break;
            }
        };
        let process_origin = resolve_process_origin_if_needed(
            origin_detection_enabled,
            &traffic_capture,
            capture_entity_resolver.as_deref(),
            &peer_addr,
        );

        let (received, returned_buffer) = ReceivedBuffer::with_return(buffer, bytes_read, peer_addr, process_origin);

        if packets_tx.send(Ok(received)).await.is_err() {
            debug!("Failed to enqueue DogStatsD packet for decoding: receiver dropped.");
            break;
        }

        match returned_buffer.await {
            Ok(buffer) if buffer.has_remaining() => retained_buffer = Some(buffer),
            Ok(buffer) => drop(buffer),
            Err(_) => break,
        }
    }

    debug!("Stream reader stopped.");
}

async fn receive_connectionless_stream(
    mut stream: Stream, io_buffer_pool: ElasticObjectPool<BytesBuffer>, memory_limiter: MemoryLimiter,
    origin_detection_enabled: bool, traffic_capture: TrafficCapture,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    datagram_sender: mpsc::Sender<QueuedDatagram>, socket_context: Arc<DatagramSocketContext>,
) {
    debug!(listen_addr = %socket_context.listen_addr, "Datagram reader started.");

    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = acquire_io_buffer(&io_buffer_pool).await;
        let result = match stream.receive(&mut buffer).await {
            Ok((bytes_read, peer_addr)) => {
                let process_origin = resolve_process_origin_if_needed(
                    origin_detection_enabled,
                    &traffic_capture,
                    capture_entity_resolver.as_deref(),
                    &peer_addr,
                );
                Ok(ReceivedBuffer::without_return(
                    buffer,
                    bytes_read,
                    peer_addr,
                    process_origin,
                ))
            }
            Err(error) => Err(error),
        };

        let receive_failed = result.is_err();
        let queued = QueuedDatagram {
            result,
            socket_context: socket_context.clone(),
        };
        if datagram_sender.send(queued).await.is_err() {
            debug!(
                listen_addr = %socket_context.listen_addr,
                "Failed to enqueue DogStatsD packet for decoding: receiver dropped."
            );
            break;
        }
        if receive_failed {
            continue;
        }
    }

    debug!(listen_addr = %socket_context.listen_addr, "Datagram reader stopped.");
}

async fn acquire_io_buffer(io_buffer_pool: &ElasticObjectPool<BytesBuffer>) -> BytesBuffer {
    let buffer = io_buffer_pool.acquire().await;
    trace!(
        remaining = buffer.remaining(),
        capacity = buffer.capacity(),
        "Acquired new buffer from pool."
    );
    buffer
}

async fn process_datagram_decoder(
    datagram_receiver: Arc<Mutex<mpsc::Receiver<QueuedDatagram>>>, source_context: SourceContext,
    decoder_context: DecoderContext,
) {
    drive_datagram_decoder(datagram_receiver, source_context, decoder_context).await;
    debug!("Datagram decoder drained its queue.");
}

/// Stops the listeners and then waits for the datagram decoders to finish draining.
///
/// The order matters and is what makes shutdown lossless: the listeners (and, transitively, their stream handlers) are
/// the only holders of a datagram sender, so waiting for them first is what closes the decoders' queue. Only then can
/// the decoders observe the close, drain what's left, and drop their handles.
async fn shutdown_listeners_and_drain_datagram_decoders(
    listener_shutdown_coordinator: ShutdownCoordinator, decoder_shutdown_coordinator: ShutdownCoordinator,
) {
    listener_shutdown_coordinator.shutdown_and_wait().await;
    decoder_shutdown_coordinator.shutdown_and_wait().await;
}

impl DogStatsDDecoder {
    fn new(source_context: SourceContext, decoder_context: DecoderContext) -> Self {
        let DecoderContext {
            codec,
            context_resolvers,
            default_hostname,
            enabled_filter,
            origin_detection_enabled,
            stream_log_too_big,
            disable_verbose_logs,
            additional_tags,
            traffic_capture,
        } = decoder_context;

        Self {
            source_context,
            codec,
            context_resolvers,
            default_hostname,
            enabled_filter,
            origin_detection_enabled,
            stream_log_too_big,
            disable_verbose_logs,
            additional_tags,
            traffic_capture,
            event_buffer: None,
        }
    }

    async fn decode_buffer(
        &mut self, context: &mut BufferDecodeContext<'_>, mut received: ReceivedBuffer,
    ) -> DecodeOutcome {
        let (buffer, bytes_read, peer_addr, process_origin) = received.parts_mut();
        self.decode_buffer_contents(context, buffer, bytes_read, peer_addr, process_origin)
            .await
    }

    async fn decode_buffer_contents(
        &mut self, context: &mut BufferDecodeContext<'_>, io_buffer: &mut BytesBuffer, bytes_read: usize,
        peer_addr: &ConnectionAddress, process_origin: Option<&ProcessOrigin>,
    ) -> DecodeOutcome {
        let listen_addr = context.listen_addr;
        let metrics = context.metrics;
        let packet_forwarder = context.packet_forwarder;
        let mode = context.mode;

        let payload = received_payload(io_buffer, bytes_read);
        capture_uds_traffic(
            listen_addr,
            &self.traffic_capture,
            peer_addr,
            process_origin,
            payload,
            &mut context.stream_capture,
        );

        metrics.bytes_received().increment(bytes_read as u64);
        metrics.bytes_received_size().record(bytes_read as f64);
        let origin_detection_failed =
            origin_detection_failed_for_telemetry(self.origin_detection_enabled, bytes_read, peer_addr);

        if matches!(mode, BufferDecodeMode::Connectionless) {
            metrics.packet_receive_success().increment(1);
            if origin_detection_failed {
                metrics.origin_detection_errors().increment(1);
            }
        }

        let eof = mode.is_eof(bytes_read);
        trace!(
            buffer_len = io_buffer.remaining(),
            buffer_cap = io_buffer.remaining_mut(),
            %listen_addr,
            %peer_addr,
            eof,
            "Received {} bytes from socket.",
            bytes_read
        );

        if should_drop_oversized_named_pipe_frame(listen_addr, io_buffer) {
            metrics.framing_errors().increment(1);
            debug!(%listen_addr, %peer_addr, "DogStatsD named pipe frame exceeded the configured buffer size. Dropping frame.");
            io_buffer.clear();
            return DecodeOutcome::Continue;
        }

        loop {
            let frame_result = context.framer.next_frame(io_buffer, eof);
            let completed_outer_frames = context.framer.take_completed_outer_frames();
            if completed_outer_frames > 0 {
                metrics
                    .packet_receive_success()
                    .increment(completed_outer_frames as u64);
            }
            if origin_detection_failed && completed_outer_frames > 0 {
                metrics
                    .origin_detection_errors()
                    .increment(completed_outer_frames as u64);
            }

            match frame_result {
                Ok(Some(frame)) => {
                    if matches!(listen_addr, ListenAddress::NamedPipe { .. }) {
                        capture_named_pipe_frame(&self.traffic_capture, &frame);
                        metrics.packet_receive_success().increment(1);
                    }
                    self.decode_frame(frame, listen_addr, peer_addr, process_origin, metrics, packet_forwarder)
                        .await;
                }
                Ok(None) => {
                    if mode.should_stop_on_eof(eof) {
                        debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                        return DecodeOutcome::Stop;
                    }
                    return DecodeOutcome::Continue;
                }
                Err(error) => {
                    metrics.framing_errors().increment(1);
                    if should_warn_stream_log_too_big(listen_addr, &error, self.stream_log_too_big) {
                        warn!(
                            %listen_addr,
                            %peer_addr,
                            error = %error,
                            "DogStatsD stream frame exceeded the configured buffer size."
                        );
                    }

                    if mode.should_stop_on_framing_error() {
                        debug!(%listen_addr, %peer_addr, %error, "Error decoding frame. Stopping stream.");
                        return DecodeOutcome::Stop;
                    }

                    debug!(%listen_addr, %peer_addr, %error, "Error decoding datagram frame. Continuing listener.");
                    return DecodeOutcome::Continue;
                }
            }
        }
    }

    async fn decode_frame(
        &mut self, frame: Bytes, listen_addr: &ListenAddress, peer_addr: &ConnectionAddress,
        process_origin: Option<&ProcessOrigin>, metrics: &Metrics, packet_forwarder: Option<&PacketForwarder>,
    ) {
        trace!(%listen_addr, %peer_addr, ?frame, "Decoded frame.");
        if let Some(forwarder) = packet_forwarder {
            forwarder.forward(frame.clone()).await;
        }

        match handle_frame(
            &frame[..],
            &self.codec,
            &mut self.context_resolvers,
            metrics,
            self.origin_detection_enabled,
            process_origin,
            self.enabled_filter,
            &self.additional_tags,
            &self.default_hostname,
        ) {
            Ok(Some(event)) => {
                if let Some(event_buffer) = self.buffer_event(event) {
                    debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                    dispatch_events(event_buffer, &self.source_context).await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                log_parse_failure(self.disable_verbose_logs, listen_addr, peer_addr, &frame, &error);
            }
        }
    }

    fn buffer_event(&mut self, event: Event) -> Option<EventsBuffer> {
        let event_buffer = self.event_buffer.get_or_insert_default();
        match event_buffer.try_push(event) {
            Some(event) => {
                let full_event_buffer = mem::take(event_buffer);
                assert!(
                    event_buffer.try_push(event).is_none(),
                    "New event buffer is unexpectedly full."
                );
                Some(full_event_buffer)
            }
            None => None,
        }
    }

    async fn flush_events(&mut self) {
        if let Some(event_buffer) = self.event_buffer.take() {
            dispatch_events(event_buffer, &self.source_context).await;
        }
    }
}

async fn drive_datagram_decoder(
    datagram_receiver: Arc<Mutex<mpsc::Receiver<QueuedDatagram>>>, source_context: SourceContext,
    decoder_context: DecoderContext,
) {
    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context);
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
                {
                    let DatagramSocketContext {
                        listen_addr,
                        eol_required,
                        metrics,
                        packet_forwarder,
                    } = socket_context.as_ref();

                    let received = match result {
                        Ok(received) => received,
                        Err(error) => {
                            metrics.packet_receive_failure().increment(1);
                            warn!(%listen_addr, %error, "I/O error while reading datagram. Continuing listener.");
                            continue;
                        }
                    };

                    let mut buffer_decode_context = BufferDecodeContext::new(
                        listen_addr,
                        *eol_required,
                        metrics,
                        packet_forwarder.as_ref(),
                        BufferDecodeMode::Connectionless,
                    );
                    let outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
                    debug_assert_eq!(outcome, DecodeOutcome::Continue);
                }
            }
            _ = buffer_flush.tick() => {
                decoder.flush_events().await;
            }
        }
    }

    decoder.flush_events().await;
}

async fn drive_stream(stream: Stream, source_context: SourceContext, handler_context: HandlerContext) {
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
            handler_context.decoder_context.origin_detection_enabled,
            handler_context.decoder_context.traffic_capture.clone(),
            handler_context.capture_entity_resolver,
            handler_context.datagram_sender,
            socket_context,
        )
        .await;
        return;
    }

    drive_connected_stream(stream, source_context, handler_context).await;
}

async fn drive_connected_stream(stream: Stream, source_context: SourceContext, handler_context: HandlerContext) {
    let listen_addr = handler_context.listen_addr.clone();
    let metrics = handler_context.metrics.clone();
    let memory_limiter = source_context.topology_context().memory_limiter().clone();
    let mut stream_reader = BufferedStreamReader::new(
        stream,
        handler_context.io_buffer_pool.clone(),
        memory_limiter,
        handler_context.decoder_context.origin_detection_enabled,
        handler_context.decoder_context.traffic_capture.clone(),
        handler_context.capture_entity_resolver.clone(),
    );
    let receiver = stream_reader.take_receiver();

    debug!(%listen_addr, "Stream handler started.");

    metrics.connections_active().increment(1);
    drive_decoder(receiver, source_context, handler_context).await;
    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
}

async fn drive_decoder(
    mut stream_receiver: mpsc::Receiver<io::Result<ReceivedBuffer>>, source_context: SourceContext,
    handler_context: HandlerContext,
) {
    let HandlerContext {
        listen_addr,
        eol_required,
        metrics,
        decoder_context,
        packet_forwarder,
        ..
    } = handler_context;
    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context);
    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buffer_decode_context = BufferDecodeContext::new(
        &listen_addr,
        eol_required,
        &metrics,
        packet_forwarder.as_ref(),
        BufferDecodeMode::Connected,
    );

    'read: loop {
        select! {
            // We read from the stream.
            maybe_read_result = stream_receiver.recv() => match maybe_read_result {
                Some(Ok(received)) => {
                    let outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
                    if outcome == DecodeOutcome::Stop {
                        break 'read;
                    }
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
                decoder.flush_events().await;
            },

        }
    }

    decoder.flush_events().await;
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

fn capture_named_pipe_frame(traffic_capture: &TrafficCapture, frame: &[u8]) {
    if !frame.is_empty() && traffic_capture.is_ongoing() {
        let _ = traffic_capture.enqueue(build_capture_record(None, None, frame));
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

fn resolve_process_origin_if_needed(
    origin_detection_enabled: bool, traffic_capture: &TrafficCapture,
    capture_entity_resolver: Option<&(dyn CaptureEntityResolver + Send + Sync)>, peer_addr: &ConnectionAddress,
) -> Option<ProcessOrigin> {
    if !origin_detection_enabled && !traffic_capture.is_ongoing() {
        return None;
    }

    resolve_process_origin(capture_entity_resolver, peer_addr)
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

async fn dispatch_events(mut event_buffer: EventsBuffer, source_context: &SourceContext) {
    debug!(events_len = event_buffer.len(), "Forwarding events.");

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
            error!(error = %e, "Failed to dispatch eventd events.");

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
            error!(error = %e, "Failed to dispatch service check events.");

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
            error!(error = %e, "Failed to dispatch metric events.");

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
        time::{Duration, Instant},
    };

    use bytes::Buf as _;
    use bytes::{BufMut as _, Bytes};
    use bytesize::ByteSize;
    use metrics::{Key, Label};
    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use saluki_context::{ContextResolverBuilder, TagsResolverBuilder};
    use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
    use saluki_core::components::test_util::TestComponentSupervisor;
    use saluki_core::{
        components::{sources::SourceContext, ComponentContext},
        health::HealthRegistry,
        pooling::{helpers::get_pooled_object_via_builder, ObjectPool as _},
        runtime::state::DataspaceRegistry,
        support::SubsystemIdentifier,
        topology::{EventsBuffer, EventsDispatcher, OutputName, TopologyContext},
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
    use stringtheory::MetaString;
    #[cfg(unix)]
    use tokio::{
        io::AsyncWriteExt as _,
        net::{UnixDatagram, UnixStream},
    };
    use tokio::{
        net::UdpSocket,
        runtime::Handle,
        sync::{mpsc, Mutex},
        task::yield_now,
        time::timeout,
    };

    use super::OriginEnrichmentConfiguration;
    use super::{
        build_io_buffer_pool, capture_named_pipe_frame, default_decoder_worker_count,
        filters::EnablePayloadsFilter,
        forwarder::{
            ConnectedPacketForwarder, ForwardPacket, PacketForwarder, PacketForwarderTarget, FORWARDER_QUEUE_CAPACITY,
        },
        handle_frame, handle_metric_packet,
        metrics::build_metrics,
        origin_detection_failed_for_telemetry, resolve_process_origin, resolve_process_origin_if_needed,
        shutdown_listeners_and_drain_datagram_decoders, BufferDecodeContext, BufferDecodeMode, ContextResolvers,
        DatagramSocketContext, DecodeOutcome, DecoderContext, DogStatsDConfiguration, DogStatsDDecoder, ProcessOrigin,
        QueuedDatagram, ReceivedBuffer, TrafficCapture, TrafficCaptureReader,
    };
    #[cfg(unix)]
    use super::{receive_connected_stream, receive_connectionless_stream, received_payload};
    #[cfg(target_os = "linux")]
    use super::{DogStatsDOriginTagResolver, Listener};

    /// Windows named pipe SDDL the Datadog Agent schema defaults to.
    const TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR: &str = "D:AI(A;;GA;;;WD)";

    /// Packet receive buffer size the Datadog Agent schema defaults to.
    const TEST_BUFFER_SIZE: usize = 8192;

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
        resolution_count: AtomicUsize,
    }

    impl CaptureTestEntityResolver {
        fn with_pid_mapping(process_id: u32, entity_id: EntityId) -> Self {
            let mut pid_map = HashMap::new();
            pid_map.insert(process_id, entity_id);
            Self {
                pid_map: StdMutex::new(pid_map),
                resolution_count: AtomicUsize::new(0),
            }
        }

        fn resolution_count(&self) -> usize {
            self.resolution_count.load(Ordering::Relaxed)
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
            self.resolution_count.fetch_add(1, Ordering::Relaxed);
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

    /// Builds a source context bound to `supervisor`, plus the receiver for its `metrics` output.
    ///
    /// `supervisor` must be running: the DogStatsD source spawns its listeners, decoders, pool shrinker, and per-
    /// connection handlers as supervised children, so a spawner over a never-run supervisor fails with
    /// `SupervisorGone`.
    fn test_source_context(supervisor: &TestComponentSupervisor) -> (SourceContext, mpsc::Receiver<EventsBuffer>) {
        let component_context = test_component_context();
        let mut dispatcher = EventsDispatcher::new(component_context.clone());
        let metrics_output = OutputName::Given("metrics".into());
        dispatcher
            .add_output(metrics_output.clone())
            .expect("metrics output should be added");
        let (metrics_tx, metrics_rx) = mpsc::channel(4);
        dispatcher
            .attach_sender_to_output(&metrics_output, metrics_tx)
            .expect("metrics output should accept a sender");

        let health_registry = HealthRegistry::new();
        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            health_registry.clone(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = health_registry
            .register_component(&SubsystemIdentifier::from_dotted("test.decoder"))
            .expect("test decoder should have a health handle");
        let source_context = SourceContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            supervisor.spawner(),
        );

        (source_context, metrics_rx)
    }

    fn test_decoder_context(origin_detection_enabled: bool) -> DecoderContext {
        DecoderContext {
            codec: DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()),
            context_resolvers: test_context_resolvers(),
            default_hostname: MetaString::from_static("default-hostname"),
            enabled_filter: EnablePayloadsFilter::default(),
            origin_detection_enabled,
            stream_log_too_big: false,
            disable_verbose_logs: false,
            additional_tags: Vec::<String>::new().into(),
            traffic_capture: TrafficCapture::new(PathBuf::new(), 1),
        }
    }

    fn test_io_buffer(payload: &[u8], capacity: usize) -> BytesBuffer {
        let mut buffer: BytesBuffer = get_pooled_object_via_builder(|| FixedSizeVec::with_capacity(capacity));
        buffer.put_slice(payload);
        buffer
    }

    #[test]
    fn named_pipe_frames_are_written_to_an_active_capture() {
        let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
        let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
        let capture_path = capture
            .start_capture(None, Duration::from_secs(30), false)
            .expect("capture should start");

        capture_named_pipe_frame(&capture, b"captured.named_pipe.one:1|c");
        capture_named_pipe_frame(&capture, b"captured.named_pipe.two:1|c");
        capture.stop_capture();

        let deadline = Instant::now() + Duration::from_secs(2);
        while capture.is_ongoing() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!capture.is_ongoing(), "capture should stop after its sender is dropped");

        let mut reader = TrafficCaptureReader::from_path(&capture_path).expect("capture should be readable");
        let first_record = reader
            .read_next()
            .expect("first capture record should decode")
            .expect("first named-pipe frame should be captured");
        let second_record = reader
            .read_next()
            .expect("second capture record should decode")
            .expect("second named-pipe frame should be captured");
        assert_eq!(first_record.payload, b"captured.named_pipe.one:1|c");
        assert_eq!(first_record.pid, 0);
        assert_eq!(second_record.payload, b"captured.named_pipe.two:1|c");
        assert_eq!(second_record.pid, 0);
        assert!(reader.read_next().expect("capture should terminate cleanly").is_none());
    }

    #[tokio::test]
    async fn connectionless_decoder_dispatches_full_and_flushed_buffers_and_forwards_frames() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
        let metrics = build_metrics(&listen_addr, &test_component_context(), false);
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let (source_context, mut metrics_rx) = test_source_context(&supervisor);
        let mut decoder = DogStatsDDecoder::new(source_context, test_decoder_context(false));
        let event_buffer_capacity = EventsBuffer::default().capacity();
        let (packets_tx, mut packets_rx) = mpsc::channel(event_buffer_capacity + 1);
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics.clone());
        let mut buffer_decode_context = BufferDecodeContext::new(
            &listen_addr,
            false,
            &metrics,
            Some(&packet_forwarder),
            BufferDecodeMode::Connectionless,
        );

        let mut payload = b"decoder.metric:1|c\n".repeat(event_buffer_capacity);
        payload.extend_from_slice(b"decoder.metric:1|c");
        let io_buffer = test_io_buffer(&payload, payload.len());
        let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, payload.len(), peer_addr, None);
        let outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;

        assert_eq!(outcome, DecodeOutcome::Continue);
        let io_buffer = returned_buffer
            .await
            .expect("connectionless decoder should return the I/O buffer");
        assert_eq!(io_buffer.remaining(), 0);
        let full_buffer = timeout(Duration::from_secs(1), metrics_rx.recv())
            .await
            .expect("full event buffer dispatch should not time out")
            .expect("metrics output should remain connected");
        assert_eq!(full_buffer.len(), event_buffer_capacity);
        assert!(
            timeout(Duration::from_secs(1), packets_rx.recv())
                .await
                .expect("forwarded packet should not time out")
                .is_some(),
            "forwarded packet should be queued"
        );
        assert_eq!(packets_rx.len(), event_buffer_capacity);

        decoder.flush_events().await;
        let flushed_buffer = timeout(Duration::from_secs(1), metrics_rx.recv())
            .await
            .expect("partial event buffer flush should not time out")
            .expect("metrics output should remain connected");
        assert_eq!(flushed_buffer.len(), 1);
        decoder.flush_events().await;
        assert!(
            metrics_rx.try_recv().is_err(),
            "empty flush should not dispatch another buffer"
        );

        assert_eq!(
            recorder.counter((
                "component_packets_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unixgram"),
                    ("state", "ok"),
                ],
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unixgram"),
                ],
            )),
            Some(payload.len() as u64)
        );
        assert_eq!(
            recorder.counter(processed_metric_key("unixgram", None)),
            Some((event_buffer_capacity + 1) as u64)
        );
    }

    #[tokio::test]
    async fn connected_decoder_waits_for_complete_outer_frame_and_stops_on_eof() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
            saluki_io::net::ProcessCredentialsError::InvalidCredentials,
        ));
        let metrics = build_metrics(&listen_addr, &test_component_context(), false);
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let (source_context, mut metrics_rx) = test_source_context(&supervisor);
        let mut decoder = DogStatsDDecoder::new(source_context, test_decoder_context(true));
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let packet_forwarder = packet_forwarder_from_sender(9125, packets_tx, metrics.clone());
        let mut buffer_decode_context = BufferDecodeContext::new(
            &listen_addr,
            false,
            &metrics,
            Some(&packet_forwarder),
            BufferDecodeMode::Connected,
        );

        let frame = b"stream.metric:1|c\n";
        let mut payload = Vec::with_capacity(frame.len() + 4);
        payload.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        payload.extend_from_slice(frame);
        let io_buffer = test_io_buffer(&payload[..2], payload.len() + 16);
        let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, 2, peer_addr.clone(), None);

        let partial_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
        assert_eq!(partial_outcome, DecodeOutcome::Continue);
        let mut io_buffer = returned_buffer
            .await
            .expect("connected decoder should return the partial I/O buffer");
        assert_eq!(io_buffer.remaining(), 2);
        assert!(
            packets_rx.try_recv().is_err(),
            "partial outer frame should not be forwarded"
        );
        assert_eq!(
            recorder.counter((
                "component_packets_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unix"),
                    ("state", "ok"),
                ],
            )),
            Some(0)
        );
        assert_eq!(
            recorder.counter((
                "component_errors_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("error_type", "origin_detection"),
                ],
            )),
            Some(0)
        );

        io_buffer.put_slice(&payload[2..]);
        let (received, returned_buffer) =
            ReceivedBuffer::with_return(io_buffer, payload.len() - 2, peer_addr.clone(), None);
        let complete_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
        assert_eq!(complete_outcome, DecodeOutcome::Continue);
        let io_buffer = returned_buffer
            .await
            .expect("connected decoder should return the consumed I/O buffer");
        assert_eq!(io_buffer.remaining(), 0);
        assert!(
            timeout(Duration::from_secs(1), packets_rx.recv())
                .await
                .expect("forwarded stream packet should not time out")
                .is_some(),
            "complete outer frame should be forwarded"
        );

        let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, 0, peer_addr, None);
        let eof_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
        assert_eq!(eof_outcome, DecodeOutcome::Stop);
        let returned_buffer = returned_buffer
            .await
            .expect("stopped decoder should return the I/O buffer");
        assert!(!returned_buffer.has_remaining());

        decoder.flush_events().await;
        let flushed_buffer = timeout(Duration::from_secs(1), metrics_rx.recv())
            .await
            .expect("connected event buffer flush should not time out")
            .expect("metrics output should remain connected");
        assert_eq!(flushed_buffer.len(), 1);
        assert_eq!(
            recorder.counter((
                "component_packets_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unix"),
                    ("state", "ok"),
                ],
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unix"),
                ],
            )),
            Some(payload.len() as u64)
        );
        assert_eq!(
            recorder.counter((
                "component_errors_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("error_type", "origin_detection"),
                ],
            )),
            Some(1)
        );
        assert_eq!(recorder.counter(processed_metric_key("unix", None)), Some(1));
    }

    #[tokio::test]
    async fn decoder_continues_connectionless_but_stops_connected_on_framing_error() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let (source_context, _metrics_rx) = test_source_context(&supervisor);
        let mut decoder = DogStatsDDecoder::new(source_context, test_decoder_context(false));

        let datagram_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
        let datagram_metrics = build_metrics(&datagram_addr, &test_component_context(), false);
        let payload = b"missing.newline:1|c";
        let datagram_buffer = test_io_buffer(payload, payload.len());
        let mut datagram_context = BufferDecodeContext::new(
            &datagram_addr,
            true,
            &datagram_metrics,
            None,
            BufferDecodeMode::Connectionless,
        );
        let (received, _returned_datagram_buffer) =
            ReceivedBuffer::with_return(datagram_buffer, payload.len(), peer_addr.clone(), None);
        let datagram_outcome = decoder.decode_buffer(&mut datagram_context, received).await;
        assert_eq!(datagram_outcome, DecodeOutcome::Continue);

        let stream_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
        let stream_metrics = build_metrics(&stream_addr, &test_component_context(), false);
        let stream_buffer_capacity = 64;
        let oversized_frame = (stream_buffer_capacity as u32).to_le_bytes();
        let stream_buffer = test_io_buffer(&oversized_frame, stream_buffer_capacity);
        let mut stream_context =
            BufferDecodeContext::new(&stream_addr, false, &stream_metrics, None, BufferDecodeMode::Connected);
        let (received, _returned_stream_buffer) =
            ReceivedBuffer::with_return(stream_buffer, oversized_frame.len(), peer_addr, None);
        let stream_outcome = decoder.decode_buffer(&mut stream_context, received).await;
        assert_eq!(stream_outcome, DecodeOutcome::Stop);

        for listener_type in ["unixgram", "unix"] {
            assert_eq!(
                recorder.counter((
                    "component_errors_total",
                    &[
                        ("component_id", "dogstatsd_test"),
                        ("component_type", "source"),
                        ("listener_type", listener_type),
                        ("error_type", "framing"),
                    ],
                )),
                Some(1)
            );
        }
        assert_eq!(
            recorder.counter((
                "component_packets_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unixgram"),
                    ("state", "ok"),
                ],
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_packets_received_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", "unix"),
                    ("state", "ok"),
                ],
            )),
            Some(0)
        );
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
    fn additional_tags_are_sorted_and_deduplicated() {
        let config = DogStatsDConfiguration {
            additional_tags: vec![
                "dogstatsd:configured".to_string(),
                "env:prod".to_string(),
                "provider_kind:autopilot".to_string(),
                "kube_distribution:eks".to_string(),
                "env:prod".to_string(),
            ],
            ..DogStatsDConfiguration::for_test()
        };

        assert_eq!(
            config.additional_tags(),
            [
                "dogstatsd:configured",
                "env:prod",
                "kube_distribution:eks",
                "provider_kind:autopilot",
            ]
        );
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

    fn udp_listen_address() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn tcp_listen_address() -> ListenAddress {
        ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn named_pipe_listen_address() -> ListenAddress {
        ListenAddress::named_pipe_with_input_buffer_size(
            "datadog-dogstatsd",
            TEST_WINDOWS_PIPE_SECURITY_DESCRIPTOR,
            TEST_BUFFER_SIZE as u32,
        )
    }

    #[test]
    fn build_addresses_includes_named_pipe_when_configured() {
        let config = DogStatsDConfiguration {
            port: 0,
            pipe_name: Some("datadog-dogstatsd".to_string()),
            ..DogStatsDConfiguration::for_test()
        };

        let addresses = config.build_addresses(None);

        assert_eq!(addresses, vec![named_pipe_listen_address()]);
    }

    #[test]
    fn build_addresses_uses_buffer_size_for_named_pipe_input_buffer() {
        let config = DogStatsDConfiguration {
            port: 0,
            pipe_name: Some("datadog-dogstatsd".to_string()),
            buffer_size: 16384,
            ..DogStatsDConfiguration::for_test()
        };

        let addresses = config.build_addresses(None);

        let [ListenAddress::NamedPipe { input_buffer_size, .. }] = addresses.as_slice() else {
            panic!("expected only a named pipe listen address, got {addresses:?}");
        };
        assert_eq!(*input_buffer_size, Some(16_384));
    }

    #[test]
    fn eol_required_matches_named_pipe_listener_type() {
        let config = DogStatsDConfiguration {
            eol_required: vec!["named_pipe".to_string()],
            ..DogStatsDConfiguration::for_test()
        };
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&named_pipe_listen_address()));
        assert!(!eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));
    }

    #[test]
    fn statsd_forward_no_host_disabled() {
        let config = DogStatsDConfiguration {
            statsd_forward_port: 9125,
            ..DogStatsDConfiguration::for_test()
        };
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_zero_port_disabled() {
        let config = DogStatsDConfiguration {
            statsd_forward_host: Some(MetaString::from("127.0.0.1")),
            statsd_forward_port: 0,
            ..DogStatsDConfiguration::for_test()
        };
        assert!(config.statsd_forward_target().is_none());
    }

    #[test]
    fn statsd_forward_host_and_port_enabled() {
        let config = DogStatsDConfiguration {
            statsd_forward_host: Some(MetaString::from("127.0.0.1")),
            statsd_forward_port: 9125,
            ..DogStatsDConfiguration::for_test()
        };
        let (host, port) = config.statsd_forward_target().expect("forwarding should be enabled");
        assert_eq!(host.as_ref(), "127.0.0.1");
        assert_eq!(port, 9125);
    }

    #[test]
    fn statsd_forward_invalid_target_still_builds_forwarder_handle() {
        let config = DogStatsDConfiguration {
            statsd_forward_host: Some(MetaString::from("not a valid host")),
            statsd_forward_port: 9125,
            ..DogStatsDConfiguration::for_test()
        };
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
    fn autoscaling_disabled_yields_a_single_udp_stream() {
        let config = DogStatsDConfiguration {
            autoscale_udp_listeners: false,
            ..DogStatsDConfiguration::for_test()
        };
        assert!(config.udp_streams_to_yield().is_none());
    }

    #[test]
    fn effective_max_buffer_count_never_below_baseline() {
        fn buffer_counts(buffer_count: usize, buffer_count_max: usize) -> DogStatsDConfiguration {
            DogStatsDConfiguration {
                buffer_count,
                buffer_count_max,
                ..DogStatsDConfiguration::for_test()
            }
        }

        // A config that only raised the baseline keeps its full capacity rather than being capped to the maximum.
        assert_eq!(buffer_counts(65536, 32_768).effective_max_buffer_count(), 65536);

        // An explicit maximum above the baseline is honored as-is.
        assert_eq!(buffer_counts(128, 512).effective_max_buffer_count(), 512);

        // A maximum below the baseline is treated as equal to the baseline.
        assert_eq!(buffer_counts(200, 64).effective_max_buffer_count(), 200);
    }

    #[test]
    fn decoder_worker_count_matches_core_agent_defaults() {
        assert_eq!(default_decoder_worker_count(1), 2);
        assert_eq!(default_decoder_worker_count(4), 2);
        assert_eq!(default_decoder_worker_count(8), 6);
    }

    #[test]
    fn decoder_worker_count_honors_explicit_override() {
        let config = DogStatsDConfiguration {
            workers_count: 1,
            ..DogStatsDConfiguration::for_test()
        };

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

        // The decoder reports completion by dropping its handle, exactly as the supervised children do.
        let mut decoder_shutdown_coordinator = ShutdownCoordinator::default();
        let decoder_shutdown = decoder_shutdown_coordinator.register();
        let decoder_task = tokio::spawn(async move {
            let _decoder_shutdown = decoder_shutdown;
            while receiver.recv().await.is_some() {
                decoder_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        shutdown_listeners_and_drain_datagram_decoders(listener_shutdown_coordinator, decoder_shutdown_coordinator)
            .await;
        listener_task.await.expect("listener task should stop cleanly");
        decoder_task.await.expect("decoder task should stop cleanly");

        assert_eq!(decoded.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn dogstatsd_io_buffer_pool_grows_on_demand_until_limit() {
        let min_buffers = 2;
        let max_buffers = 3;
        let (pool, shrinker) = build_io_buffer_pool(min_buffers, max_buffers, TEST_BUFFER_SIZE);

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
        let (pool, shrinker) = build_io_buffer_pool(2, 2, TEST_BUFFER_SIZE);
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
            false,
            TrafficCapture::new(PathBuf::new(), 1),
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
        assert_eq!(received_payload(first.buffer(), first.bytes_read), payloads[0]);
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
            assert_eq!(received_payload(received.buffer(), received.bytes_read), *expected);
        }

        drop(packets_rx);
        sender
            .send_to(b"shutdown", &socket_path)
            .await
            .expect("shutdown payload should send");
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop after observing the closed queue")
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
        let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let reader = tokio::spawn(receive_connectionless_stream(
            stream,
            pool,
            MemoryLimiter::noop(),
            true,
            TrafficCapture::new(PathBuf::new(), 1),
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
        assert_eq!(capture_entity_resolver.resolution_count(), 1);

        // Simulate the sender exiting and its PID being reused before a decoder worker reaches the queued packet.
        capture_entity_resolver.set_pid_mapping(process_id, reused_entity.clone());

        let mut workload_provider = TestWorkloadProvider::new();
        workload_provider.add_entity(original_entity, &["container:original"]);
        workload_provider.add_entity(reused_entity, &["container:reused"]);
        let origin_config = OriginEnrichmentConfiguration {
            enabled: true,
            ..OriginEnrichmentConfiguration::for_test()
        };
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

        drop(received);
        drop(packets_rx);
        sender
            .send_to(b"shutdown", &socket_path)
            .await
            .expect("shutdown payload should send");
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop after observing the closed queue")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_oriented_reader_preserves_partial_frames() {
        let (mut sender, receiver) = UnixStream::pair().expect("stream pair should be created");
        let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let reader = tokio::spawn(receive_connected_stream(
            Stream::from(receiver),
            pool,
            MemoryLimiter::noop(),
            false,
            TrafficCapture::new(PathBuf::new(), 1),
            None,
            packets_tx,
        ));

        sender.write_all(b"partial").await.expect("first payload should send");
        let first = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("first read should finish")
            .expect("reader should remain active")
            .expect("first read should succeed");
        assert_eq!(first.buffer().chunk(), b"partial");
        drop(first);

        sender.write_all(b"-frame").await.expect("second payload should send");
        let second = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("second read should finish")
            .expect("reader should remain active")
            .expect("second read should succeed");
        assert_eq!(second.bytes_read, b"-frame".len());
        assert_eq!(second.buffer().chunk(), b"partial-frame");
        drop(second);

        drop(packets_rx);
        sender
            .write_all(b"shutdown")
            .await
            .expect("shutdown payload should send");
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop after observing the closed queue")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_oriented_reader_releases_drained_buffer_before_reacquiring() {
        let (mut sender, receiver) = UnixStream::pair().expect("stream pair should be created");
        let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
        let (packets_tx, mut packets_rx) = mpsc::channel(1);
        let reader = tokio::spawn(receive_connected_stream(
            Stream::from(receiver),
            pool,
            MemoryLimiter::noop(),
            false,
            TrafficCapture::new(PathBuf::new(), 1),
            None,
            packets_tx,
        ));

        sender.write_all(b"first").await.expect("first payload should send");
        let mut first = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("first read should finish")
            .expect("reader should remain active")
            .expect("first read should succeed");
        let bytes_read = first.bytes_read;
        first.buffer_mut().advance(bytes_read);
        drop(first);

        sender.write_all(b"second").await.expect("second payload should send");
        let mut second = timeout(Duration::from_secs(1), packets_rx.recv())
            .await
            .expect("reader should reacquire the released buffer")
            .expect("reader should remain active")
            .expect("second read should succeed");
        let bytes_read = second.bytes_read;
        assert_eq!(received_payload(second.buffer(), bytes_read), b"second");
        second.buffer_mut().advance(bytes_read);
        drop(second);

        drop(packets_rx);
        sender
            .write_all(b"shutdown")
            .await
            .expect("shutdown payload should send");
        timeout(Duration::from_secs(1), reader)
            .await
            .expect("reader should stop after observing the closed queue")
            .expect("reader task should not panic");
        drop(shrinker);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn autoscale_udp_listeners_yields_multiple_streams_on_linux() {
        let config = DogStatsDConfiguration {
            autoscale_udp_listeners: true,
            ..DogStatsDConfiguration::for_test()
        };

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
        let config = DogStatsDConfiguration {
            port: 0,
            socket_path: Some("/tmp/dsd.sock".to_string()),
            origin_enrichment: OriginEnrichmentConfiguration {
                enabled: true,
                ..OriginEnrichmentConfiguration::for_test()
            },
            ..DogStatsDConfiguration::for_test()
        };
        let addresses = config.build_addresses(None);

        assert!(config.uds_origin_detection_unsupported_on_platform(&addresses));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn does_not_warn_for_udp_origin_detection_on_non_linux() {
        let config = DogStatsDConfiguration {
            origin_enrichment: OriginEnrichmentConfiguration {
                enabled: true,
                ..OriginEnrichmentConfiguration::for_test()
            },
            ..DogStatsDConfiguration::for_test()
        };
        let addresses = config.build_addresses(None);

        assert!(!config.uds_origin_detection_unsupported_on_platform(&addresses));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn autoscale_udp_listeners_yields_a_single_stream_on_non_linux() {
        let config = DogStatsDConfiguration {
            autoscale_udp_listeners: true,
            ..DogStatsDConfiguration::for_test()
        };

        assert_eq!(None, config.udp_streams_to_yield());
    }

    #[test]
    fn no_eol_required_listener_types_requires_no_newline() {
        let config = DogStatsDConfiguration {
            eol_required: Vec::new(),
            ..DogStatsDConfiguration::for_test()
        };
        let eol_required = config.eol_required();

        assert!(!eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&tcp_listen_address()));
    }

    #[test]
    fn eol_required_matches_configured_listener_types() {
        let config = DogStatsDConfiguration {
            eol_required: vec!["udp".to_string(), "uds".to_string()],
            ..DogStatsDConfiguration::for_test()
        };
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
    fn eol_required_ignores_unrecognized_listener_types() {
        let config = DogStatsDConfiguration {
            eol_required: vec!["udp".to_string(), "carrier_pigeon".to_string()],
            ..DogStatsDConfiguration::for_test()
        };
        let eol_required = config.eol_required();

        assert!(eol_required.for_listener(&udp_listen_address()));
        assert!(!eol_required.for_listener(&named_pipe_listen_address()));
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
        let config = DogStatsDConfiguration {
            context_string_interner_entry_count: 4096,
            ..DogStatsDConfiguration::for_test()
        };
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
    }

    #[test]
    fn interner_size_from_explicit_bytes() {
        let config = DogStatsDConfiguration {
            context_string_interner_size_bytes: Some(ByteSize::b(4194304)),
            ..DogStatsDConfiguration::for_test()
        };
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(4194304));
    }

    #[test]
    fn interner_size_explicit_bytes_takes_priority() {
        let config = DogStatsDConfiguration {
            context_string_interner_entry_count: 4096,
            context_string_interner_size_bytes: Some(ByteSize::b(8388608)),
            ..DogStatsDConfiguration::for_test()
        };
        // The explicit byte size (8 MiB) takes priority over the entry count.
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::b(8388608));
    }

    #[test]
    fn interner_size_custom_entry_count() {
        let config = DogStatsDConfiguration {
            context_string_interner_entry_count: 8192,
            ..DogStatsDConfiguration::for_test()
        };
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
            ..DogStatsDConfiguration::for_test()
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
    fn resolve_process_origin_skips_live_lookup_when_unused() {
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        }));
        let traffic_capture = TrafficCapture::new(PathBuf::new(), 1);

        assert_eq!(
            resolve_process_origin_if_needed(false, &traffic_capture, Some(&capture_entity_resolver), &peer_addr),
            None
        );
        assert_eq!(capture_entity_resolver.resolution_count(), 0);
    }

    #[tokio::test]
    async fn resolve_process_origin_pins_live_entity_during_capture() {
        let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        }));
        let capture_dir = tempfile::tempdir().expect("capture directory should be created");
        let traffic_capture = TrafficCapture::new(capture_dir.path().to_path_buf(), 1);
        traffic_capture
            .start_capture(None, Duration::from_secs(30), false)
            .expect("capture should start");

        assert_eq!(
            resolve_process_origin_if_needed(false, &traffic_capture, Some(&capture_entity_resolver), &peer_addr),
            Some(ProcessOrigin::Pinned(Some(
                EntityId::from_local_data("ci-pid-container").expect("container entity")
            )))
        );
        assert_eq!(capture_entity_resolver.resolution_count(), 1);

        traffic_capture.stop_capture();
        timeout(Duration::from_secs(1), async {
            while traffic_capture.is_ongoing() {
                yield_now().await;
            }
        })
        .await
        .expect("capture should stop");
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

/// Tests covering the source's background work running as supervised children.
///
/// The `tests` module above exercises the decode path in isolation. What matters here is the wiring: that `run` puts
/// the pool shrinker, datagram decoders, listeners, and per-connection handlers under the component's supervisor, and
/// that the whole subtree comes down cleanly and without dropping queued work.
#[cfg(test)]
mod supervision {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use saluki_context::{ContextResolverBuilder, TagsResolverBuilder};
    use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
    use saluki_core::components::test_util::TestComponentSupervisor;
    use saluki_core::components::{
        sources::{Source as _, SourceContext},
        ComponentContext,
    };
    use saluki_core::health::HealthRegistry;
    use saluki_core::runtime::state::DataspaceRegistry;
    use saluki_core::support::SubsystemIdentifier;
    use saluki_core::topology::{EventsBuffer, EventsDispatcher, OutputName, TopologyContext};
    use saluki_io::net::listener::Listener;
    use saluki_io::net::ListenAddress;
    use stringtheory::MetaString;
    use tokio::net::UdpSocket;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;

    /// Bound on driving the source to completion, so a hang fails rather than stalling the suite.
    const RUN_TIMEOUT: Duration = Duration::from_secs(10);

    /// Decoder workers the test source runs with.
    ///
    /// More than one, so the drain covers the real case of several decoders sharing the queue.
    const DECODER_WORKERS: usize = 2;

    /// How long to let datagrams make their way through to the (single-slot) output before asserting on backpressure.
    const BACKPRESSURE_SETTLE: Duration = Duration::from_millis(250);

    /// Children a running source with one UDP listener should have.
    ///
    /// The pool shrinker, one child per decoder worker, the listener itself, and one stream handler: a connectionless
    /// listener's `accept` yields its socket straight away, so its handler exists from the start rather than only once
    /// a peer shows up.
    const fn udp_child_count() -> usize {
        1 + DECODER_WORKERS + 1 + 1
    }

    /// Everything a test needs to drive `DogStatsD::run` and observe the result.
    struct Harness {
        source: Box<DogStatsD>,
        context: SourceContext,
        /// Signals the source's global shutdown.
        shutdown_coordinator: ShutdownCoordinator,
        /// Events dispatched to the `metrics` output.
        metrics_rx: mpsc::Receiver<EventsBuffer>,
        /// The address the source is listening on.
        listen_addr: SocketAddr,
    }

    /// Builds a source with a single UDP listener bound to an ephemeral port.
    ///
    /// `health_registry` **MUST** outlive the run: `Health::live` resolves immediately once its registry is dropped,
    /// and the source's run loop polls liveness in a `select!`, so a dropped registry spins it instead of waiting for
    /// shutdown.
    /// Builds a source whose `metrics` output holds a single buffer.
    ///
    /// One slot makes backpressure trivial to arrange: one dispatched buffer fills it and the next blocks a decoder.
    async fn build_source(supervisor: &TestComponentSupervisor, health_registry: &HealthRegistry) -> Harness {
        build_source_with_output_capacity(supervisor, health_registry, 1).await
    }

    async fn build_source_with_output_capacity(
        supervisor: &TestComponentSupervisor, health_registry: &HealthRegistry, output_capacity: usize,
    ) -> Harness {
        build_source_with(supervisor, health_registry, output_capacity, 16).await
    }

    async fn build_source_with(
        supervisor: &TestComponentSupervisor, health_registry: &HealthRegistry, output_capacity: usize,
        io_buffer_count: usize,
    ) -> Harness {
        let component_context = ComponentContext::test_source("dogstatsd");

        // Bind an ephemeral port and read back what we got, so parallel tests don't collide on a fixed one.
        let probe = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("should bind a probe socket");
        let listen_addr = probe.local_addr().expect("probe should have an address");
        drop(probe);

        let listener = Listener::from_listen_address(ListenAddress::Udp(listen_addr), None)
            .await
            .expect("listener should bind");

        let (io_buffer_pool, io_buffer_pool_shrinker) = build_io_buffer_pool(
            io_buffer_count,
            io_buffer_count,
            DogStatsDConfiguration::for_test().buffer_size,
        );

        let tags_resolver = TagsResolverBuilder::for_tests().build();
        let context_resolver = ContextResolverBuilder::for_tests()
            .with_tags_resolver(Some(tags_resolver.clone()))
            .build();

        let source = Box::new(DogStatsD {
            listeners: vec![listener],
            decoder_worker_count: NonZeroUsize::new(DECODER_WORKERS).expect("decoder count should be non-zero"),
            io_buffer_pool,
            io_buffer_queue_capacity: 16,
            io_buffer_pool_shrinker: Box::pin(io_buffer_pool_shrinker),
            codec: DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()),
            context_resolvers: ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver),
            default_hostname: MetaString::from_static("test-host"),
            enabled_filter: EnablePayloadsFilter::default(),
            origin_detection_enabled: false,
            origin_telemetry_enabled: false,
            stream_log_too_big: false,
            disable_verbose_logs: false,
            eol_required: EolRequired::default(),
            additional_tags: Vec::<String>::new().into(),
            capture_entity_resolver: None,
            traffic_capture: TrafficCapture::new(PathBuf::new(), 1),
            packet_forwarder_target: None,
        });

        // Only the `metrics` output is wired up: these tests feed counters, so nothing reaches the other two.
        let mut dispatcher = EventsDispatcher::new(component_context.clone());
        let metrics_output = OutputName::Given("metrics".into());
        dispatcher
            .add_output(metrics_output.clone())
            .expect("metrics output should be added");
        let (metrics_tx, metrics_rx) = mpsc::channel(output_capacity);
        dispatcher
            .attach_sender_to_output(&metrics_output, metrics_tx)
            .expect("metrics output should accept a sender");

        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            health_registry.clone(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = health_registry
            .register_component(&SubsystemIdentifier::from_dotted("test.dogstatsd"))
            .expect("component was not previously registered");

        let mut context = SourceContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            supervisor.spawner(),
        );

        let mut shutdown_coordinator = ShutdownCoordinator::default();
        context.set_shutdown_handle_for_test(shutdown_coordinator.register());

        Harness {
            source,
            context,
            shutdown_coordinator,
            metrics_rx,
            listen_addr,
        }
    }

    #[tokio::test]
    async fn background_work_runs_as_supervised_children() {
        // The pool shrinker, each datagram decoder, and each listener are supervised children rather than detached
        // tasks, so they are all accounted for while the source runs and all gone once it stops.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let health_registry = HealthRegistry::new();
        let harness = build_source(&supervisor, &health_registry).await;
        let Harness {
            source,
            context,
            shutdown_coordinator,
            ..
        } = harness;

        let run = tokio::spawn(async move { source.run(context).await });

        supervisor.wait_for_children(udp_child_count()).await;

        shutdown_coordinator.shutdown();
        timeout(RUN_TIMEOUT, run)
            .await
            .expect("source should stop on shutdown")
            .expect("source task should not panic")
            .expect("source should stop cleanly");

        // Everything the source drains explicitly -- listener, stream handler, decoders -- is gone by the time `run`
        // returns. The pool shrinker is the one exception, and deliberately so: it is interruptible, so it runs until
        // the supervisor drops it rather than being waited on.
        supervisor.wait_for_children(1).await;

        // A clean result is the assertion: `ShutdownTimedOut` would mean a child ignored shutdown and was aborted.
        let result = supervisor.shutdown().await;
        assert!(result.is_ok(), "every child should have stopped on its own: {result:?}");
    }

    #[tokio::test]
    async fn run_does_not_return_until_the_decoders_have_drained() {
        // The drain guarantee that matters: work already queued when shutdown is signalled must still be decoded and
        // dispatched. The decoders deliberately ignore shutdown for exactly this reason, exiting only once the
        // listeners have dropped their senders and the queue is empty -- and `run` waits for that.
        //
        // Rather than race a datagram against shutdown, this pins a decoder open: the metrics output holds a single
        // buffer, so one dispatched buffer fills it and the next blocks a decoder mid-dispatch. `run` must not return
        // while that is true, no matter that shutdown has been signalled.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let health_registry = HealthRegistry::new();
        let harness = build_source(&supervisor, &health_registry).await;
        let Harness {
            source,
            context,
            shutdown_coordinator,
            mut metrics_rx,
            listen_addr,
        } = harness;

        let mut run = tokio::spawn(async move { source.run(context).await });
        supervisor.wait_for_children(udp_child_count()).await;

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client should bind");

        // Establish that the pipeline works, and leave the output empty again.
        client
            .send_to(b"first.metric:1|c", listen_addr)
            .await
            .expect("client should send");
        let buffer = timeout(RUN_TIMEOUT, metrics_rx.recv())
            .await
            .expect("the first datagram should be decoded and dispatched")
            .expect("the metrics output should stay open");
        assert_eq!(buffer.len(), 1);

        // Now back the output up: the first of these fills the single slot, the second blocks a decoder.
        for payload in [b"second.metric:1|c".as_slice(), b"third.metric:1|c".as_slice()] {
            client.send_to(payload, listen_addr).await.expect("client should send");
        }
        tokio::time::sleep(BACKPRESSURE_SETTLE).await;

        shutdown_coordinator.shutdown();

        // `run` must still be waiting on the blocked decoder. Without the drain it would return here.
        tokio::time::sleep(BACKPRESSURE_SETTLE).await;
        assert!(
            !run.is_finished(),
            "`run` returned while a decoder was still mid-dispatch"
        );

        // Draining the output unblocks the decoder, which lets the drain -- and so `run` -- complete.
        let mut drained = 1;
        let result = loop {
            select! {
                run_result = &mut run => break run_result,
                maybe_buffer = metrics_rx.recv() => match maybe_buffer {
                    Some(buffer) => drained += buffer.len(),
                    None => break (&mut run).await,
                },
            }
        };
        result
            .expect("source task should not panic")
            .expect("source should stop cleanly");

        // `run` returning and the last buffer arriving are concurrent, so collect whatever is still queued. The
        // source's dispatcher is gone by now, so this terminates as soon as the channel is empty.
        while let Some(buffer) = metrics_rx.recv().await {
            drained += buffer.len();
        }

        // Nothing accepted before shutdown should have been dropped on the way out.
        assert_eq!(drained, 3, "every queued datagram should have been dispatched");

        let supervisor_result = supervisor.shutdown().await;
        assert!(
            supervisor_result.is_ok(),
            "every child should have stopped on its own: {supervisor_result:?}"
        );
    }

    #[tokio::test]
    async fn supervisor_shutdown_stops_the_subtree_without_the_source_driving_it() {
        // The orderly path is the source's own `run` signalling its coordinators. This is the other one: the
        // supervisor tears the component down while `run` is still going, so nothing is driving those coordinators.
        //
        // The listener has to observe the supervisor's own signal for this to terminate. If it only watched the
        // source's coordinator it would keep accepting, keep holding a datagram sender, and so keep the decoders
        // waiting on a queue that never closes -- until the shutdown budget elapsed and aborted the lot.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let health_registry = HealthRegistry::new();
        let harness = build_source(&supervisor, &health_registry).await;
        let Harness {
            source,
            context,
            // Deliberately held, not signalled: `run` stays in its loop for the whole test.
            shutdown_coordinator: _shutdown_coordinator,
            ..
        } = harness;

        let _run = tokio::spawn(async move { source.run(context).await });
        supervisor.wait_for_children(udp_child_count()).await;

        let result = supervisor.shutdown().await;
        assert!(
            result.is_ok(),
            "the subtree should have stopped on the supervisor's signal alone: {result:?}"
        );
    }

    #[tokio::test]
    async fn stream_handlers_are_supervised_children() {
        // Per-connection handlers are supervised too, under one fixed name per listener type. Accepting a connection
        // adds a child, and tearing the source down takes it with everything else.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let health_registry = HealthRegistry::new();

        // A TCP listener, so there are real connections to accept.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind a probe socket");
        let listen_addr = probe.local_addr().expect("probe should have an address");
        drop(probe);

        let mut harness = build_source(&supervisor, &health_registry).await;
        harness.source.listeners = vec![Listener::from_listen_address(ListenAddress::Tcp(listen_addr), None)
            .await
            .expect("listener should bind")];

        let Harness {
            source,
            context,
            shutdown_coordinator,
            ..
        } = harness;

        let run = tokio::spawn(async move { source.run(context).await });

        let baseline = 1 + DECODER_WORKERS + 1;
        supervisor.wait_for_children(baseline).await;

        let _client = tokio::net::TcpStream::connect(listen_addr)
            .await
            .expect("client should connect");
        supervisor.wait_for_children(baseline + 1).await;

        shutdown_coordinator.shutdown();
        timeout(RUN_TIMEOUT, run)
            .await
            .expect("source should stop on shutdown")
            .expect("source task should not panic")
            .expect("source should stop cleanly");

        // As above, only the interruptible pool shrinker is left for the supervisor to drop.
        supervisor.wait_for_children(1).await;

        let result = supervisor.shutdown().await;
        assert!(result.is_ok(), "every child should have stopped on its own: {result:?}");
    }
}
