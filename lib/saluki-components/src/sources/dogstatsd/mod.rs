//! DogStatsD source.
//!
//! # Missing
//!
//! - Create a health handle for each listener.
//! - Handle UDS stream framing without treating EOF the same way as UDP and UDS datagram framing.
//! - Track dispatch failures without depending on whether all events were already iterated.

use std::{
    future::Future,
    io,
    num::NonZeroUsize,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use saluki_common::sync::{
    shutdown::ShutdownHandle,
    work_queue::{self, WorkQueueReceiver, WorkQueueSender},
};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter, UsageExpr},
    components::{sources::*, BuildContext},
    data_model::{
        event::{
            eventd::EventD,
            metric::{Metric, MetricMetadata, MetricOrigin},
            service_check::ServiceCheck,
            Event, EventType,
        },
        tags::{RawTags, SharedTagSet},
    },
    pooling::{ElasticObjectPool, ObjectPool as _},
    runtime::{
        self,
        state::{AcquireError, ResourceLease},
        ShutdownStrategy,
    },
    topology::{interconnect::EventBufferManager, EventsBuffer, OutputDefinition, DEFAULT_EVENTS_BUFFER_CAPACITY},
};
use saluki_env::{workload::CaptureEntityResolver, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _, CollapsibleReadWriteIoBuffer as _, FixedSizeVec, ReadIoBuffer as _},
    deser::{codec::dogstatsd::*, framing::FramingError},
    net::{
        dns::{resolve_host, ResolveHostError},
        listener::Listener,
        ConnectionAddress, ListenAddress, ProcessCredentialsError, ProcessIdentity, SocketSpecification, Stream,
    },
};
use snafu::Snafu;
use stringtheory::MetaString;
use tokio::{
    pin, select,
    time::{interval, Interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

mod forwarder;
use self::forwarder::{forwarder_queue_capacity, PacketForwarder, PacketForwarderTarget};

mod framer;
use self::framer::{get_framer, DsdFramer, Frame};
use crate::sources::dogstatsd::tags::WellKnownTags;

mod filters;
pub use self::filters::EnablePayloadsConfiguration;

mod metrics;
use self::metrics::{build_metrics, ForwarderMetrics, Metrics};
use crate::common::dogstatsd_capture::{
    CaptureRecord, DogStatsDCaptureControl, DogStatsDReplayControl, REPLAY_CREDENTIALS_GID,
};

mod origin;
pub use self::origin::OriginEnrichmentConfiguration;
use self::origin::{origin_from_packet, DogStatsDOriginTagResolver, ProcessOrigin};

mod resolver;
use self::resolver::ContextResolvers;

mod tags;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Failed to acquire {} listener: {}", listener_type, source))]
    FailedToAcquireListener {
        listener_type: &'static str,
        source: AcquireError,
    },

    #[snafu(display("No listeners could be created. Please specify a port (`dogstatsd_port`) or a socket path (`dogstatsd_socket` or `dogstatsd_stream_socket`) that can be listened on."))]
    NoListeners,

    #[snafu(context(false), display("Could not resolve bind_host: {}", source))]
    UnresolvableBindHost { source: ResolveHostError },
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

/// Which of the configured listeners the source requires.
///
/// This decides what happens when a listener can't be created, for example because its address is already in use.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListenerRequirement {
    /// Every configured listener must be created, and at least one must be configured.
    ///
    /// A listener that can't be created stops the source from starting.
    #[default]
    All,

    /// At least one listener must be created.
    ///
    /// A listener that can't be created is logged and skipped. The source fails to start if
    /// no listener remains.
    AtLeastOne,

    /// No listener is required.
    ///
    /// A listener that can't be created is logged and skipped. If no listener remains, the source runs without
    /// listeners until shutdown, and the rest of the server process keeps running.
    Optional,
}

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

    /// The size of the DogStatsD packet queue, from `dogstatsd_queue_size`.
    ///
    /// The source doesn't queue received packets by count. It sizes the packet forwarder's queue from this value
    /// instead: a quarter of it, and at least 64 packets.
    pub queue_size: usize,

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

    /// Which of the configured listeners the source requires.
    ///
    /// Defaults to [`ListenerRequirement::All`], where a listener that can't be created prevents the source from
    /// starting. With [`ListenerRequirement::AtLeastOne`] or [`ListenerRequirement::Optional`], such a listener
    /// is logged and skipped instead. If no listener remains, the source only fails to start under `AtLeastOne`;
    /// under `Optional` the source runs without listeners until shutdown.
    ///
    /// Keep the default when every configured address is needed. Relax it when the server process should keep
    /// running while some addresses are unavailable, and use `AtLeastOne` when a process supervisor should still
    /// detect and restart a process that can receive no traffic.
    pub listener_requirement: ListenerRequirement,

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

    /// Control surface that starts and stops DogStatsD traffic capture.
    ///
    /// This is runtime wiring rather than configuration: the caller keeps a clone to serve the capture API, and the
    /// source writes the traffic it receives to it.
    pub capture_control: DogStatsDCaptureControl,

    /// Control surface that starts and stops DogStatsD traffic replay.
    ///
    /// This is runtime wiring rather than configuration: the caller keeps a clone to serve the replay API, and the
    /// source resolves the origin of replayed traffic from the captured tagger state it holds.
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

#[cfg(test)]
impl DogStatsDConfiguration {
    /// Creates a fixture configuration, for tests that exercise source behavior rather than configuration.
    ///
    /// Values match the effective defaults the configuration system resolves, so a test only states the settings it
    /// cares about.
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
            queue_size: 1024,
            socket_path: None,
            socket_stream_path: None,
            stream_log_too_big: false,
            listener_requirement: ListenerRequirement::default(),
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
            enable_payloads: EnablePayloadsConfiguration::for_test(),
            origin_enrichment: OriginEnrichmentConfiguration::for_test(),
            origin_telemetry_enabled: false,
            workload_provider: None,
            capture_entity_resolver: None,
            additional_tags: Vec::new(),
            capture_control: DogStatsDCaptureControl::new(std::path::PathBuf::new(), 1),
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
        Some(PacketForwarderTarget::new(
            host.clone(),
            port,
            forwarder_queue_capacity(self.queue_size),
            get_adjusted_buffer_size(self.buffer_size),
        ))
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
    /// `resolve_bind_ip`). Precedence matches the Agent:
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

    /// Resolves the IP that UDP and TCP listeners bind to, if `bind_host` applies.
    ///
    /// The lookup is skipped when `non_local_traffic` is set, since `bind_host` is then unused, so an unresolvable
    /// hostname that wouldn't be used doesn't stop the source from starting.
    async fn resolve_bind_ip(&self) -> Result<Option<std::net::IpAddr>, Error> {
        match &self.bind_host {
            Some(host) if !self.non_local_traffic => Ok(Some(resolve_host(host).await?)),
            _ => Ok(None),
        }
    }

    /// Handles `error`, raised because `listeners` can't be created.
    ///
    /// The error is not propagated if the listener requirement skips failed listeners.
    fn handle_listener_failure(&self, error: Error, listeners: &str) -> Result<(), Error> {
        // A key registered as a different type of resource is a programming error, not a listener that couldn't be
        // created, so it's never skipped.
        let is_key_collision = matches!(
            error,
            Error::FailedToAcquireListener {
                source: AcquireError::TypeMismatch { .. },
                ..
            }
        );
        let fail = match self.listener_requirement {
            ListenerRequirement::All => true,
            ListenerRequirement::AtLeastOne | ListenerRequirement::Optional => false,
        };
        if is_key_collision || fail {
            return Err(error);
        }

        error!(error = %error, "Can't create DogStatsD {}.", listeners);
        Ok(())
    }

    /// Acquires the appropriate `Listener` objects from the resource registry.
    ///
    /// The listeners are leased, not owned: the registry keeps the bound sockets when this source is torn down, so a
    /// rebuilt source gets the same sockets back rather than racing to re-bind addresses it just released.
    ///
    /// The listener requirement decides whether a listener that can't be created is skipped, and whether ending up with
    /// no listener at all is an error.
    async fn build_listeners(&self, context: &BuildContext) -> Result<Vec<ResourceLease<Listener>>, Error> {
        let addresses = match self.resolve_bind_ip().await {
            Ok(bind_ip) => self.build_addresses(bind_ip),
            Err(e) => {
                // An unresolvable `bind_host` only prevents the UDP and TCP listeners.
                self.handle_listener_failure(e, "UDP and TCP listeners")?;
                self.build_addresses(None)
                    .into_iter()
                    .filter(|address| !matches!(address, ListenAddress::Udp(_) | ListenAddress::Tcp(_)))
                    .collect()
            }
        };

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
            let spec = SocketSpecification::new(address)
                .with_udp_streams(listener_streams)
                .with_receive_buffer_size(socket_receive_buffer_size);
            match context.acquire_resource(spec).await {
                Ok(listener) => listeners.push(listener),
                Err(source) => self.handle_listener_failure(
                    Error::FailedToAcquireListener { listener_type, source },
                    &format!("{listener_type} listener"),
                )?,
            }
        }

        if listeners.is_empty() {
            match self.listener_requirement {
                ListenerRequirement::All | ListenerRequirement::AtLeastOne => return Err(Error::NoListeners),
                // The server process keeps running. The source runs without listeners until shutdown.
                ListenerRequirement::Optional => error!(error = %Error::NoListeners, "Could not start DogStatsD."),
            }
        }
        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let listeners = self.build_listeners(&context).await?;

        // Every listener requires at least one I/O buffer to ensure that all listeners can be serviced without
        // deadlocking any of the others. Multi-socket connectionless listeners require one buffer per yielded socket.
        let min_buffers: usize = listeners.iter().map(|listener| listener.min_buffer_reservation()).sum();
        let max_buffers = self.effective_max_buffer_count();
        // Without listeners, `min_buffers` is zero, so the check below can't catch a capacity of zero. The datagram
        // queue needs room for at least one buffer either way.
        if max_buffers == 0 {
            return Err(generic_error!(
                "The maximum I/O buffer count must be greater than zero."
            ));
        }
        if max_buffers < min_buffers {
            return Err(generic_error!(
                "The maximum I/O buffer count ({}) must be at least {} to service all configured listeners.",
                max_buffers,
                min_buffers,
            ));
        }

        // The resolver reads the captured tagger state that the replay control's active session sets.
        let captured_tagger = self.replay_control.captured_tagger();
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

        let decoder_context = DecoderContext {
            codec: DogStatsDCodec::from_configuration(codec_config),
            context_resolvers,
            default_hostname: self.default_hostname.clone(),
            enable_payloads: self.enable_payloads,
            origin_detection_enabled: self.origin_enrichment.enabled,
            stream_log_too_big: self.stream_log_too_big,
            disable_verbose_logs: self.disable_verbose_logs,
            additional_tags: self.additional_tags().into(),
            traffic_capture: self.capture_control.clone(),
        };

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
            decoder_context,
            origin_telemetry_enabled: self.origin_telemetry_enabled,
            eol_required: self.eol_required(),
            capture_entity_resolver: self.capture_entity_resolver.clone(),
            packet_forwarder_target: self.packet_forwarder_target(),
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

        // Each packet forwarder queue slot keeps a buffer as large as the largest packet it has carried, up to a receive
        // buffer. This is the worst case: slots are usually much smaller.
        if self.statsd_forward_target().is_some() {
            builder.firm().with_expr(UsageExpr::product(
                "packet forwarder queue",
                UsageExpr::constant("forwarder_queue_capacity", forwarder_queue_capacity(self.queue_size)),
                UsageExpr::config("dogstatsd_buffer_size", adjusted_buffer_size),
            ));
        }
    }
}

/// DogStatsD source.
pub struct DogStatsD {
    listeners: Vec<ResourceLease<Listener>>,
    decoder_worker_count: NonZeroUsize,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    io_buffer_queue_capacity: usize,
    io_buffer_pool_shrinker: Pin<Box<dyn Future<Output = ()> + Send>>,
    decoder_context: DecoderContext,
    origin_telemetry_enabled: bool,
    eol_required: EolRequired,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    packet_forwarder_target: Option<PacketForwarderTarget>,
}

/// State shared by every stream of one listener, and by the datagrams queued from them.
struct ListenerState {
    listen_addr: ListenAddress,
    eol_required: bool,
    metrics: Metrics,
    packet_forwarder: Option<PacketForwarder>,
    io_buffer_pool: ElasticObjectPool<BytesBuffer>,
    decoder_context: DecoderContext,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
}

#[derive(Clone)]
struct DecoderContext {
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    default_hostname: MetaString,
    enable_payloads: EnablePayloadsConfiguration,
    origin_detection_enabled: bool,
    stream_log_too_big: bool,
    disable_verbose_logs: bool,
    additional_tags: Arc<[String]>,
    traffic_capture: DogStatsDCaptureControl,
}

/// A datagram read by a listener, along with the origin pinned at read time.
struct ReceivedDatagram {
    buffer: BytesBuffer,
    bytes_read: usize,
    peer_addr: ConnectionAddress,
    process_origin: Option<ProcessOrigin>,
}

struct QueuedDatagram {
    result: io::Result<ReceivedDatagram>,
    listener: Arc<ListenerState>,
}

struct DogStatsDDecoder {
    source_context: SourceContext,
    context: DecoderContext,
    event_buffer: EventBufferManager<DEFAULT_EVENTS_BUFFER_CAPACITY>,
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

/// Decode state of one connection, or of one datagram.
struct BufferDecodeContext<'a> {
    listener: &'a ListenerState,
    mode: BufferDecodeMode,
    framer: DsdFramer,

    /// The last PID seen on this connection, so that captured frames keep it on reads that carry no credentials.
    last_pid: Option<i32>,
}

impl<'a> BufferDecodeContext<'a> {
    fn new(listener: &'a ListenerState, mode: BufferDecodeMode) -> Self {
        Self {
            listener,
            mode,
            framer: get_framer(&listener.listen_addr, listener.eol_required),
            last_pid: None,
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

        // This source spawns everything below as its own children. Thus, the source does not finish until they finish.
        // When the body of the source returns, the children receive the shutdown signal.
        //
        // Brutal: the shrinker loops forever with no terminal condition of its own, so waiting for it would hold
        // every shutdown open until its timeout expired.
        runtime::worker("io_buffer_pool_shrinker", self.io_buffer_pool_shrinker)
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();

        // Each decoder gets its own queue. Sharing one receiver between decoders serializes decoding: handing the
        // receiver to the next decoder puts it behind the previous decoder's decode work on the same runtime thread.
        let (datagram_sender, datagram_receivers) =
            work_queue::bounded(self.decoder_worker_count, self.io_buffer_queue_capacity);
        let decoder_context = self.decoder_context;

        // Decoders must drain their queue to completion, so they deliberately ignore the shutdown signal and stop only
        // once their queue closes -- which happens when every listener and stream handler has dropped its datagram
        // sender. This makes shutdown lossless. The listeners stop when the body of this source returns. The decoders
        // can see the close only after the listeners and their stream handlers stop. Then the decoders drain the
        // datagrams that remain.
        //
        // When dispatch is backpressured, the drain can take longer than five seconds, the default timeout of a
        // closure-based child. Thus, the decoders have no timeout of their own, and are halted with the source if the
        // component's shutdown timeout expires.
        for (worker_id, datagram_receiver) in datagram_receivers.into_iter().enumerate() {
            let decoder_source_context = context.clone();
            let decoder_context = decoder_context.clone();

            runtime::worker(
                format!("datagram_decoder_{worker_id}"),
                drive_datagram_decoder(datagram_receiver, decoder_source_context, decoder_context),
            )
            .with_shutdown_timeout(Duration::MAX)
            .spawn();
        }

        // One packet forwarder serves every listener. Its forwarding loop stops once every listener, stream handler, and
        // decoder has dropped its handle, after sending whatever is still queued.
        let packet_forwarder = self
            .packet_forwarder_target
            .as_ref()
            .map(|target| target.to_forwarder(ForwarderMetrics::new(context.component_context())));
        if let Some(packet_forwarder) = &packet_forwarder {
            packet_forwarder.spawn();
        }

        // For each listener, spawn a dedicated task to run it. There may be none, when no listener could be created and
        // the listener requirement is `Optional`. The source then only waits for shutdown.
        for listener in self.listeners {
            let task_name = format!("listener_{}", listener.listen_address().listener_type());
            let listener_source_context = context.clone();

            // TODO: Create a health handle for each listener.
            //
            // We need to rework `HealthRegistry` to look a little more like `ComponentRegistry` so that we can have it
            // already be scoped properly, otherwise all we can do here at present is either have a relative name, like
            // `uds-stream`, or try and hardcode the full component name, which we will inevitably forget to update if
            // we tweak the topology configuration, etc.
            let listen_addr = listener.listen_address().clone();
            let metrics = build_metrics(&listen_addr, context.component_context(), self.origin_telemetry_enabled);
            let listener_state = Arc::new(ListenerState {
                eol_required: self.eol_required.for_listener(&listen_addr),
                listen_addr,
                metrics,
                packet_forwarder: packet_forwarder.clone(),
                io_buffer_pool: self.io_buffer_pool.clone(),
                decoder_context: decoder_context.clone(),
                capture_entity_resolver: self.capture_entity_resolver.clone(),
            });
            let datagram_sender = datagram_sender.clone();

            // An accept loop has no terminal condition of its own, so the listener needs the shutdown signal. It has no
            // shutdown timeout of its own: the topology shutdown timeout applies to the source and everything it spawned.
            //
            // Each listener is needed. A fatal accept error stops a listener without a request to stop. Then the source
            // has nothing to listen on, so the listener stops the source. The source does not continue to run.
            runtime::worker_with_shutdown(task_name, move |process_shutdown| {
                process_listener(
                    listener_source_context,
                    listener,
                    listener_state,
                    datagram_sender,
                    process_shutdown,
                )
            })
            .with_shutdown_timeout(Duration::MAX)
            .needed()
            .spawn();
        }
        drop(datagram_sender);
        drop(packet_forwarder);

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

        // This return stops the listeners, and after they stop, the decoders drain. The source finishes when all of its
        // children finish.
        debug!("Stopping DogStatsD source...");

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

async fn process_listener(
    source_context: SourceContext, mut listener: ResourceLease<Listener>, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>, process_shutdown: ShutdownHandle,
) {
    pin!(process_shutdown);

    let listen_addr = &listener_state.listen_addr;

    info!(%listen_addr, "DogStatsD listener started.");

    loop {
        select! {
            // This signal fires when the body of the source returns. That return closes the scope that this listener
            // belongs to.
            _ = &mut process_shutdown => {
                debug!(%listen_addr, "Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            result = listener.accept() => match result {
                Ok(stream) => {
                    debug!(%listen_addr, "Spawning new stream handler.");

                    let task_name = format!("conn_{}", listen_addr.listener_type());

                    // Each handler is a child of the listener. Thus, the listener does not finish until its stream
                    // handlers finish. The handlers receive the shutdown signal when the listener no longer accepts
                    // connections.
                    let handler_source_context = source_context.clone();
                    let handler_state = Arc::clone(&listener_state);
                    let handler_datagram_sender = datagram_sender.clone();
                    runtime::worker_with_shutdown(task_name, move |shutdown| {
                        process_stream(stream, handler_source_context, handler_state, handler_datagram_sender, shutdown)
                    })
                    .spawn();
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            }
        }
    }

    info!(%listen_addr, "DogStatsD listener stopped accepting connections.");
}

async fn process_stream(
    stream: Stream, source_context: SourceContext, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>, shutdown_handle: ShutdownHandle,
) {
    select! {
        _ = shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, listener_state, datagram_sender) => {},
    }
}

fn origin_detection_error_for_telemetry(
    origin_detection_enabled: bool, bytes_read: usize, peer_addr: &ConnectionAddress,
) -> Option<ProcessCredentialsError> {
    match peer_addr {
        ConnectionAddress::ProcessLike(identity @ ProcessIdentity::Error(error))
            if origin_detection_enabled && bytes_read > 0 && identity.is_telemetry_error() =>
        {
            Some(*error)
        }
        _ => None,
    }
}

async fn receive_connectionless_stream(
    mut stream: Stream, memory_limiter: MemoryLimiter, mut datagram_sender: WorkQueueSender<QueuedDatagram>,
    listener_state: Arc<ListenerState>,
) {
    let ListenerState {
        listen_addr,
        io_buffer_pool,
        decoder_context,
        capture_entity_resolver,
        ..
    } = listener_state.as_ref();

    debug!(%listen_addr, "Datagram reader started.");

    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = acquire_io_buffer(io_buffer_pool).await;
        let result = match stream.receive(&mut buffer).await {
            Ok((bytes_read, peer_addr)) => {
                // Pin the origin now: the sender's PID may be reused before a decoder reaches this datagram.
                let process_origin = resolve_process_origin_if_needed(
                    decoder_context.origin_detection_enabled,
                    &decoder_context.traffic_capture,
                    capture_entity_resolver.as_deref(),
                    &peer_addr,
                );
                Ok(ReceivedDatagram {
                    buffer,
                    bytes_read,
                    peer_addr,
                    process_origin,
                })
            }
            Err(error) => Err(error),
        };

        let queued = QueuedDatagram {
            result,
            listener: Arc::clone(&listener_state),
        };
        if datagram_sender.send(queued).await.is_err() {
            debug!(%listen_addr, "Failed to enqueue DogStatsD packet for decoding: receiver dropped.");
            break;
        }
    }

    debug!(%listen_addr, "Datagram reader stopped.");
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

impl DogStatsDDecoder {
    fn new(source_context: SourceContext, context: DecoderContext) -> Self {
        Self {
            source_context,
            context,
            event_buffer: EventBufferManager::default(),
        }
    }

    async fn decode_buffer(
        &mut self, context: &mut BufferDecodeContext<'_>, io_buffer: &mut BytesBuffer, bytes_read: usize,
        peer_addr: &ConnectionAddress, process_origin: Option<&ProcessOrigin>,
    ) -> DecodeOutcome {
        let listener = context.listener;
        let listen_addr = &listener.listen_addr;
        let metrics = &listener.metrics;
        let packet_forwarder = listener.packet_forwarder.as_ref();
        let mode = context.mode;

        let payload = received_payload(io_buffer, bytes_read);
        let traffic_capture = &self.context.traffic_capture;
        capture_unixgram_traffic(listen_addr, traffic_capture, peer_addr, process_origin, payload);

        let capture_stream_frames = matches!(listen_addr, ListenAddress::Unix(_)) && traffic_capture.is_ongoing();
        if capture_stream_frames {
            if let Some(process_id) = process_id_from_peer_addr(peer_addr) {
                context.last_pid = Some(process_id);
            }
        }

        metrics.bytes_received().increment(bytes_read as u64);
        metrics.bytes_received_size().record(bytes_read as f64);
        let origin_detection_errors =
            origin_detection_error_for_telemetry(self.context.origin_detection_enabled, bytes_read, peer_addr)
                .and_then(|error| metrics.origin_detection_errors(error));

        if matches!(mode, BufferDecodeMode::Connectionless) {
            metrics.packet_receive_success().increment(1);
            if let Some(origin_detection_errors) = origin_detection_errors {
                origin_detection_errors.increment(1);
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

        let mut frames = context.framer.frames(io_buffer.cursor(), eof, capture_stream_frames);
        loop {
            let frame_result = frames.next();
            let completed_outer_frames = frames.take_completed_outer_frames();
            if completed_outer_frames > 0 {
                metrics
                    .packet_receive_success()
                    .increment(completed_outer_frames as u64);
                if let Some(origin_detection_errors) = origin_detection_errors {
                    origin_detection_errors.increment(completed_outer_frames as u64);
                }
            }

            match frame_result {
                Some(Ok(Frame::Outer(payload))) => {
                    let _ = self.context.traffic_capture.enqueue(build_capture_record(
                        context.last_pid,
                        process_origin,
                        payload,
                    ));
                }
                Some(Ok(Frame::Message(frame))) => {
                    if matches!(listen_addr, ListenAddress::NamedPipe { .. }) {
                        capture_named_pipe_frame(&self.context.traffic_capture, frame);
                        metrics.packet_receive_success().increment(1);
                    }
                    self.decode_frame(frame, listen_addr, peer_addr, process_origin, metrics, packet_forwarder)
                        .await;
                }
                None => {
                    if mode.should_stop_on_eof(eof) {
                        debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                        return DecodeOutcome::Stop;
                    }
                    return DecodeOutcome::Continue;
                }
                Some(Err(error)) => {
                    metrics.framing_errors().increment(1);
                    if should_warn_stream_log_too_big(listen_addr, &error, self.context.stream_log_too_big) {
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
        &mut self, frame: &[u8], listen_addr: &ListenAddress, peer_addr: &ConnectionAddress,
        process_origin: Option<&ProcessOrigin>, metrics: &Metrics, packet_forwarder: Option<&PacketForwarder>,
    ) {
        trace!(%listen_addr, %peer_addr, frame = %frame.escape_ascii(), "Decoded frame.");
        if let Some(forwarder) = packet_forwarder {
            forwarder.forward(frame).await;
        }

        match self.handle_frame(frame, metrics, process_origin) {
            Ok(Some(event)) => {
                if let Some(event_buffer) = self.event_buffer.try_push(event) {
                    debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                    dispatch_events(event_buffer, &self.source_context).await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                log_parse_failure(self.context.disable_verbose_logs, listen_addr, peer_addr, frame, &error);
            }
        }
    }

    async fn flush_events(&mut self) {
        if let Some(event_buffer) = self.event_buffer.consume() {
            dispatch_events(event_buffer, &self.source_context).await;
        }
    }

    fn handle_frame(
        &mut self, frame: &[u8], source_metrics: &Metrics, process_origin: Option<&ProcessOrigin>,
    ) -> Result<Option<Event>, ParseError> {
        let DecoderContext {
            codec,
            context_resolvers,
            default_hostname,
            enable_payloads,
            origin_detection_enabled,
            additional_tags,
            ..
        } = &mut self.context;

        let resolve_telemetry_origin = || {
            (source_metrics.origin_telemetry_enabled() && *origin_detection_enabled)
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
                if !enable_payloads.allow_metric(&metric_packet) {
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
                if !enable_payloads.allow_event(&event) {
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
                if !enable_payloads.allow_service_check(&service_check) {
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
}

/// Builds the interval that flushes buffered events at least every 100 milliseconds while a decoder is otherwise idle.
fn buffer_flush_interval() -> Interval {
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
    buffer_flush
}

async fn drive_datagram_decoder(
    mut datagram_receiver: WorkQueueReceiver<QueuedDatagram>, source_context: SourceContext,
    decoder_context: DecoderContext,
) {
    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context);
    let mut buffer_flush = buffer_flush_interval();

    loop {
        select! {
            maybe_datagram = datagram_receiver.recv() => {
                let Some(QueuedDatagram { result, listener }) = maybe_datagram else {
                    break;
                };

                let mut received = match result {
                    Ok(received) => received,
                    Err(error) => {
                        listener.metrics.packet_receive_failure().increment(1);
                        warn!(listen_addr = %listener.listen_addr, %error, "I/O error while reading datagram. Continuing listener.");
                        continue;
                    }
                };

                let mut buffer_decode_context = BufferDecodeContext::new(&listener, BufferDecodeMode::Connectionless);
                let outcome = decoder
                    .decode_buffer(
                        &mut buffer_decode_context,
                        &mut received.buffer,
                        received.bytes_read,
                        &received.peer_addr,
                        received.process_origin.as_ref(),
                    )
                    .await;
                debug_assert_eq!(outcome, DecodeOutcome::Continue);
            }
            _ = buffer_flush.tick() => {
                decoder.flush_events().await;
            }
        }
    }

    decoder.flush_events().await;
    debug!("Datagram decoder drained its queue.");
}

async fn drive_stream(
    stream: Stream, source_context: SourceContext, listener_state: Arc<ListenerState>,
    datagram_sender: WorkQueueSender<QueuedDatagram>,
) {
    let memory_limiter = source_context.topology_context().memory_limiter().clone();
    if stream.is_connectionless() {
        receive_connectionless_stream(stream, memory_limiter, datagram_sender, listener_state).await;
    } else {
        drive_connected_stream(stream, source_context, memory_limiter, &listener_state).await;
    }
}

/// Reads and decodes a connected stream until it ends or fails.
///
/// Reads and decodes alternate on this one task: a stream is ordered, so there is nothing to decode until the read
/// before it is done.
async fn drive_connected_stream(
    mut stream: Stream, source_context: SourceContext, memory_limiter: MemoryLimiter, listener_state: &ListenerState,
) {
    let ListenerState {
        listen_addr,
        metrics,
        io_buffer_pool,
        decoder_context,
        capture_entity_resolver,
        ..
    } = listener_state;

    let mut decoder = DogStatsDDecoder::new(source_context, decoder_context.clone());
    let mut buffer_decode_context = BufferDecodeContext::new(listener_state, BufferDecodeMode::Connected);
    let mut buffer_flush = buffer_flush_interval();

    debug!(%listen_addr, "Stream handler started.");
    metrics.connections_active().increment(1);

    // Holds a partial frame between reads, and the buffer acquired for the next read across flush ticks.
    let mut retained_buffer: Option<BytesBuffer> = None;
    loop {
        memory_limiter.wait_for_capacity().await;

        let mut buffer = match retained_buffer.take() {
            Some(mut buffer) => {
                buffer.collapse();
                buffer
            }
            None => acquire_io_buffer(io_buffer_pool).await,
        };

        // `Stream::receive` is cancel-safe, so a flush tick that wins the race loses no bytes.
        select! {
            result = stream.receive(&mut buffer) => match result {
                Ok((bytes_read, peer_addr)) => {
                    let process_origin = resolve_process_origin_if_needed(
                        decoder_context.origin_detection_enabled,
                        &decoder_context.traffic_capture,
                        capture_entity_resolver.as_deref(),
                        &peer_addr,
                    );
                    let outcome = decoder
                        .decode_buffer(
                            &mut buffer_decode_context,
                            &mut buffer,
                            bytes_read,
                            &peer_addr,
                            process_origin.as_ref(),
                        )
                        .await;
                    if outcome == DecodeOutcome::Stop {
                        break;
                    }

                    // Return a drained buffer to the pool rather than holding it while the connection idles.
                    if !buffer.has_remaining() {
                        continue;
                    }
                }
                Err(e) => {
                    metrics.packet_receive_failure().increment(1);
                    warn!(%listen_addr, error = %e, "I/O error while decoding. Stopping stream.");
                    break;
                }
            },
            _ = buffer_flush.tick() => {
                decoder.flush_events().await;
            },
        }

        retained_buffer = Some(buffer);
    }

    decoder.flush_events().await;
    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
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

fn capture_named_pipe_frame(traffic_capture: &DogStatsDCaptureControl, frame: &[u8]) {
    if !frame.is_empty() && traffic_capture.is_ongoing() {
        let _ = traffic_capture.enqueue(build_capture_record(None, None, frame));
    }
}

fn capture_unixgram_traffic(
    listen_addr: &ListenAddress, traffic_capture: &DogStatsDCaptureControl, peer_addr: &ConnectionAddress,
    process_origin: Option<&ProcessOrigin>, payload: &[u8],
) {
    if payload.is_empty() || !matches!(listen_addr, ListenAddress::Unixgram(_)) || !traffic_capture.is_ongoing() {
        return;
    }

    let _ = traffic_capture.enqueue(build_capture_record(
        process_id_from_peer_addr(peer_addr),
        process_origin,
        payload,
    ));
}

fn build_capture_record(
    process_id: Option<i32>, process_origin: Option<&ProcessOrigin>, payload: &[u8],
) -> CaptureRecord {
    CaptureRecord {
        timestamp_ns: capture_timestamp_ns(),
        payload: payload.to_vec(),
        pid: process_id,
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
    origin_detection_enabled: bool, traffic_capture: &DogStatsDCaptureControl,
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

fn handle_metric_packet(
    packet: MetricPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String], default_hostname: &MetaString,
) -> Option<Metric> {
    let (well_known_tags, mut tags) = WellKnownTags::split_raw_tags(&packet.tags);
    tags.extend(additional_tags.iter().map(String::as_str));

    let origin = origin_from_packet(&packet.origin, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    let hostname = well_known_tags.hostname.unwrap_or(default_hostname);

    // Try to resolve the context for this metric. This fails if the interner is out of capacity.
    let context =
        context_resolver.resolve_with_host_and_origin_tags(packet.metric_name, hostname, &tags, origin_tags)?;

    let metric_origin = well_known_tags
        .jmx_check_name
        .map(MetricOrigin::jmx_check)
        .unwrap_or_else(MetricOrigin::dogstatsd);
    let metadata = MetricMetadata::default()
        .with_origin(metric_origin)
        .with_unit(packet.unit.map_or_else(MetaString::empty, MetaString::from_static));

    Some(Metric::from_parts(context, packet.values, metadata))
}

/// Resolves the tags and origin tags of an event or service check, along with the value of any `host:` tag.
///
/// Returns `None` if the tags can't be resolved.
fn resolve_tags_and_origin_tags<'a>(
    context_resolvers: &mut ContextResolvers, raw_tags: &'a RawTags<'_>, client_origin: &ClientOrigin<'_>,
    process_origin: Option<&ProcessOrigin>, additional_tags: &'a [String],
) -> Option<(SharedTagSet, SharedTagSet, Option<&'a str>)> {
    let (well_known_tags, mut tags) = WellKnownTags::split_raw_tags(raw_tags);
    tags.extend(additional_tags.iter().map(String::as_str));

    let origin = origin_from_packet(client_origin, &well_known_tags);
    let origin_tags = context_resolvers.resolve_origin_tags(origin, process_origin);

    let tags = context_resolvers.tags().create_tag_set(&tags)?;
    Some((tags, origin_tags, well_known_tags.hostname))
}

fn handle_event_packet(
    packet: EventPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<EventD> {
    let (tags, origin_tags, tagged_hostname) = resolve_tags_and_origin_tags(
        context_resolvers,
        &packet.tags,
        &packet.origin,
        process_origin,
        additional_tags,
    )?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior in pkg/aggregator/aggregator.go (addEvent), which sets e.Ts = time.Now().Unix()
    // for any event with Ts == 0.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let eventd = EventD::new(packet.title, packet.text)
        .with_timestamp(timestamp)
        // An `h:` field takes precedence; otherwise, fall back to a `host:` tag, matching the reference DogStatsD
        // server. An empty `host:` tag gives an explicitly empty hostname, which host enrichment leaves alone, as
        // the reference DogStatsD server does.
        .with_hostname(packet.hostname.or(tagged_hostname).map(|s| s.into()))
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
        .with_tags(tags)
        .with_origin_tags(origin_tags);

    Some(eventd)
}

fn handle_service_check_packet(
    packet: ServiceCheckPacket, context_resolvers: &mut ContextResolvers, process_origin: Option<&ProcessOrigin>,
    additional_tags: &[String],
) -> Option<ServiceCheck> {
    let (tags, origin_tags, tagged_hostname) = resolve_tags_and_origin_tags(
        context_resolvers,
        &packet.tags,
        &packet.origin,
        process_origin,
        additional_tags,
    )?;

    // When no d: field is present, backfill the current time—matching the stock Datadog Agent's
    // behavior, which sets the timestamp to time.Now().Unix() for any service check with a zero
    // timestamp.
    let timestamp = packet
        .timestamp
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(timestamp)
        // An `h:` field takes precedence; otherwise, fall back to a `host:` tag, matching the reference DogStatsD
        // server. An empty `host:` tag gives an explicitly empty hostname, which host enrichment leaves alone, as
        // the reference DogStatsD server does.
        .with_hostname(packet.hostname.or(tagged_hostname).map(|s| s.into()))
        .with_tags(tags)
        .with_origin_tags(origin_tags)
        .with_message(packet.message.map(|s| s.into()));

    Some(service_check)
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
        let eventd_events = event_buffer.take_events::<EventD>().map(Event::EventD);
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
        let service_check_events = event_buffer.take_events::<ServiceCheck>().map(Event::ServiceCheck);
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
mod tests;
