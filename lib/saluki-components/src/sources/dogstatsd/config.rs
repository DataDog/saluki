//! DogStatsD source configuration, and building the source from it.

use std::{
    num::NonZeroUsize,
    sync::{Arc, LazyLock},
};

use async_trait::async_trait;
use bytesize::ByteSize;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{sources::*, BuildContext},
    data_model::event::EventType,
    runtime::state::{AcquireError, ResourceLease},
    topology::OutputDefinition,
};
use saluki_env::{workload::CaptureEntityResolver, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    deser::codec::dogstatsd::*,
    net::{
        dns::{resolve_host, ResolveHostError},
        listener::Listener,
        ListenAddress, SocketSpecification,
    },
};
use snafu::Snafu;
use stringtheory::MetaString;
use tracing::{error, warn};

use super::{
    build_io_buffer_pool, forwarder_queue_capacity, ContextResolvers, DecoderContext, DogStatsD,
    DogStatsDCaptureControl, DogStatsDOriginTagResolver, DogStatsDReplayControl, EnablePayloadsConfiguration,
    OriginEnrichmentConfiguration, PacketForwarderTarget,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub(super) enum Error {
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

pub(super) fn default_decoder_worker_count(vcpus: usize) -> usize {
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
pub(super) struct EolRequired {
    udp: bool,
    uds: bool,
    pub(super) named_pipe: bool,
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

    pub(super) fn for_listener(&self, listen_addr: &ListenAddress) -> bool {
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
    pub(super) fn for_test() -> Self {
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
    pub(super) fn additional_tags(&self) -> Vec<String> {
        let mut tags = self.additional_tags.clone();
        tags.sort_unstable();
        tags.dedup();
        tags
    }

    /// Returns the effective string interner size in bytes.
    ///
    /// If `context_string_interner_size_bytes` is set, it's used directly. Otherwise,
    /// `context_string_interner_entry_count` is multiplied by 512 bytes per entry to derive the byte size.
    pub(super) fn effective_context_string_interner_bytes(&self) -> ByteSize {
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

    pub(super) fn eol_required(&self) -> EolRequired {
        EolRequired::from_config_values(&self.eol_required)
    }

    pub(super) fn statsd_forward_target(&self) -> Option<(&MetaString, u16)> {
        let host = self.statsd_forward_host.as_ref()?;
        if self.statsd_forward_port == 0 {
            return None;
        }

        Some((host, self.statsd_forward_port))
    }

    pub(super) fn packet_forwarder_target(&self) -> Option<PacketForwarderTarget> {
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
    pub(super) fn udp_streams_to_yield(&self) -> Option<NonZeroUsize> {
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
    pub(super) fn effective_max_buffer_count(&self) -> usize {
        self.buffer_count_max.max(self.buffer_count)
    }

    pub(super) fn decoder_worker_count(&self) -> NonZeroUsize {
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
    pub(super) fn build_addresses(&self, bind_host: Option<std::net::IpAddr>) -> Vec<ListenAddress> {
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

    pub(super) fn uds_origin_detection_unsupported_on_platform(&self, addresses: &[ListenAddress]) -> bool {
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

pub(super) const fn get_adjusted_buffer_size(buffer_size: usize) -> usize {
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
