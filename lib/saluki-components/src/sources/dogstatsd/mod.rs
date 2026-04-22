use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use metrics::{Counter, Gauge, Histogram};
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_context::{
    tags::{RawTags, RawTagsFilter},
    TagsResolver,
};
use saluki_core::data_model::event::metric::Metric;
use saluki_core::data_model::event::{
    eventd::EventD,
    metric::{MetricMetadata, MetricOrigin},
    service_check::ServiceCheck,
    Event, EventType,
};
use saluki_core::{
    components::{sources::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::FixedSizeObjectPool,
    topology::{
        interconnect::EventBufferManager,
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        EventsBuffer, OutputDefinition,
    },
};
use saluki_env::{workload::EntityId, WorkloadProvider};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec},
    deser::{
        codec::dogstatsd::*,
        framing::{Framer as _, FramerExt as _, LengthDelimitedFramer, NewlineFramer},
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, ReceiveResult, Stream,
    },
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use serde_with::{serde_as, NoneAsEmptyString};
use snafu::{ResultExt as _, Snafu};
use tokio::{
    select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

mod framer;
use self::framer::{get_framer, DsdFramer};
use crate::sources::dogstatsd::tags::{WellKnownTags, WellKnownTagsFilterPredicate};

mod filters;
use self::filters::EnablePayloadsFilter;

mod io_buffer;
use self::io_buffer::IoBufferManager;

mod replay;
use self::replay::{CaptureRecord, TrafficCapture, REPLAY_CREDENTIALS_GID};
pub use self::replay::{DogStatsDCaptureControl, DogStatsDReplayState};

mod origin;
use self::origin::{
    origin_from_event_packet, origin_from_metric_packet, origin_from_service_check_packet, DogStatsDOriginTagResolver,
    OriginEnrichmentConfiguration,
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
}

const fn default_buffer_size() -> usize {
    8192
}

const fn default_buffer_count() -> usize {
    128
}

const fn default_port() -> u16 {
    8125
}

const fn default_tcp_port() -> u16 {
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

/// Baseline byte cost per interner entry, used to convert the Core Agent's entry-count-based
/// `dogstatsd_string_interner_size` to a byte size.
///
/// 4096 entries × 512 bytes = 2 MiB, matching ADP's previous default.
const INTERNER_BASELINE_BYTES_PER_ENTRY: u64 = 512;

const fn default_cached_contexts_limit() -> usize {
    500_000
}

const fn default_cached_tagsets_limit() -> usize {
    500_000
}

const fn default_dogstatsd_permissive_decoding() -> bool {
    true
}

const fn default_dogstatsd_minimum_sample_rate() -> f64 {
    0.000000003845
}

const fn default_enable_payloads_series() -> bool {
    true
}

const fn default_enable_payloads_sketches() -> bool {
    true
}

const fn default_enable_payloads_events() -> bool {
    true
}

const fn default_enable_payloads_service_checks() -> bool {
    true
}

const fn default_capture_depth() -> usize {
    0
}

const DOGSTATSD_CAPTURE_DIR: &str = "dsd_capture";

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
#[serde_as]
#[derive(Deserialize)]
pub struct DogStatsDConfiguration {
    /// The size of the buffer used to receive messages into, in bytes.
    ///
    /// Payloads cannot exceed this size, or they will be truncated, leading to discarded messages.
    ///
    /// Defaults to 8192 bytes.
    #[serde(rename = "dogstatsd_buffer_size", default = "default_buffer_size")]
    buffer_size: usize,

    /// The number of message buffers to allocate overall.
    ///
    /// This represents the maximum number of message buffers available for processing incoming metrics, which loosely
    /// correlates with how many messages can be received per second. The default value should be suitable for the
    /// majority of workloads, but high-throughput workloads may consider increasing this value.
    ///
    /// Defaults to 128.
    #[serde(rename = "dogstatsd_buffer_count", default = "default_buffer_count")]
    buffer_count: usize,

    /// The port to listen on in UDP mode.
    ///
    /// If set to `0`, UDP is not used.
    ///
    /// Defaults to 8125.
    #[serde(rename = "dogstatsd_port", default = "default_port")]
    port: u16,

    /// The port to listen on in TCP mode.
    ///
    /// If set to `0`, TCP is not used.
    ///
    /// Defaults to 0.
    #[serde(rename = "dogstatsd_tcp_port", default = "default_tcp_port")]
    tcp_port: u16,

    /// The Unix domain socket path to listen on, in datagram mode.
    ///
    /// If not set, UDS (in datagram mode) is not used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_socket", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode.
    ///
    /// If not set, UDS (in stream mode) is not used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_stream_socket", default)]
    #[serde_as(as = "NoneAsEmptyString")]
    socket_stream_path: Option<String>,

    /// Whether or not to listen for non-local traffic in UDP mode.
    ///
    /// If set to `true`, the listener will accept packets from any interface/address. Otherwise, the source will only
    /// listen on `localhost`.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_non_local_traffic", default)]
    non_local_traffic: bool,

    /// Whether or not to allow heap allocations when resolving contexts.
    ///
    /// When resolving contexts during parsing, the metric name and tags are interned to reduce memory usage. The
    /// interner has a fixed size, however, which means some strings can fail to be interned if the interner is full.
    /// When set to `true`, we allow these strings to be allocated on the heap like normal, but this can lead to
    /// increased (unbounded) memory usage. When set to `false`, if the metric name and all of its tags cannot be
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
    /// metric timestamps are present, it is used as a signal to any aggregation transforms that the metric should not
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
    /// When `dogstatsd_string_interner_size_bytes` is not set, this value is multiplied by 512 bytes per entry to
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
    /// can be used to intern metric names and tags. If the interner is full, metrics with contexts that have not
    /// already been resolved may or may not be dropped, depending on the value of `allow_context_heap_allocations`.
    #[serde(rename = "dogstatsd_string_interner_size_bytes", default)]
    context_string_interner_size_bytes: Option<ByteSize>,

    /// The maximum number of cached contexts to allow.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit does not affect
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
    /// This is the maximum number of resolved tagsets that can be cached at any given time. This limit does not affect
    /// the total number of tagsets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "dogstatsd_cached_tagsets_limit", default = "default_cached_tagsets_limit")]
    cached_tagsets_limit: usize,

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

    /// Whether or not to enable sending serie payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_payloads_series")]
    enable_payloads_series: bool,

    /// Whether or not to enable sending sketch payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_payloads_sketches")]
    enable_payloads_sketches: bool,

    /// Whether or not to enable sending event payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_payloads_events")]
    enable_payloads_events: bool,

    /// Whether or not to enable sending service check payloads.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_enable_payloads_service_checks")]
    enable_payloads_service_checks: bool,

    /// Configuration related to origin detection and enrichment.
    #[serde(flatten, default)]
    origin_enrichment: OriginEnrichmentConfiguration,

    /// Workload provider to utilize for origin detection/enrichment.
    #[serde(skip)]
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,

    /// Shared replay state to use for replay-marked origin lookups.
    #[serde(skip, default)]
    replay_state: DogStatsDReplayState,

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
    /// This controls the depth of the in-process capture queue once the writer is fully implemented. A value of `0`
    /// matches the Core Agent default and indicates no extra buffering.
    ///
    /// Defaults to `0`.
    #[serde(rename = "dogstatsd_capture_depth", default = "default_capture_depth")]
    capture_depth: usize,

    #[serde(skip, default)]
    capture_control: DogStatsDCaptureControl,
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut dogstatsd_config: Self = config.as_typed()?;
        dogstatsd_config.fix_empty_capture_path(config);
        Ok(dogstatsd_config)
    }

    /// Returns the effective string interner size in bytes.
    ///
    /// If `dogstatsd_string_interner_size_bytes` is set, it is used directly. Otherwise,
    /// `dogstatsd_string_interner_size` (an entry count) is multiplied by 512 bytes per entry to derive the byte
    /// size.
    fn effective_context_string_interner_bytes(&self) -> ByteSize {
        match self.context_string_interner_size_bytes {
            Some(explicit_bytes) => explicit_bytes,
            None => ByteSize::b(self.context_string_interner_entry_count * INTERNER_BASELINE_BYTES_PER_ENTRY),
        }
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment will not be enabled.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }

    /// Sets the shared replay state to use for replay-marked DogStatsD traffic.
    pub fn with_replay_state(mut self, replay_state: DogStatsDReplayState) -> Self {
        self.replay_state = replay_state;
        self
    }

    /// Returns the shared control handle for DogStatsD traffic capture.
    pub fn capture_control(&self) -> DogStatsDCaptureControl {
        self.capture_control.clone()
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

    async fn build_listeners(&self) -> Result<Vec<Listener>, Error> {
        let mut listeners = Vec::new();

        if self.port != 0 {
            let address = if self.non_local_traffic {
                ListenAddress::Udp(([0, 0, 0, 0], self.port).into())
            } else {
                ListenAddress::Udp(([127, 0, 0, 1], self.port).into())
            };

            let listener = Listener::from_listen_address(address)
                .await
                .context(FailedToCreateListener { listener_type: "UDP" })?;
            listeners.push(listener);
        }

        if self.tcp_port != 0 {
            let address = if self.non_local_traffic {
                ListenAddress::Tcp(([0, 0, 0, 0], self.tcp_port).into())
            } else {
                ListenAddress::Tcp(([127, 0, 0, 1], self.tcp_port).into())
            };

            let listener = Listener::from_listen_address(address)
                .await
                .context(FailedToCreateListener { listener_type: "TCP" })?;
            listeners.push(listener);
        }

        if let Some(socket_path) = &self.socket_path {
            let address = ListenAddress::Unixgram(socket_path.into());
            let listener = Listener::from_listen_address(address)
                .await
                .context(FailedToCreateListener {
                    listener_type: "UDS (datagram)",
                })?;
            listeners.push(listener);
        }

        if let Some(socket_stream_path) = &self.socket_stream_path {
            let address = ListenAddress::Unix(socket_stream_path.into());
            let listener = Listener::from_listen_address(address)
                .await
                .context(FailedToCreateListener {
                    listener_type: "UDS (stream)",
                })?;
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
        // deadlocking any of the others.
        if self.buffer_count < listeners.len() {
            return Err(generic_error!(
                "Must have a minimum of {} I/O buffers based on the number of listeners configured.",
                listeners.len()
            ));
        }

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(|provider| {
            DogStatsDOriginTagResolver::new(self.origin_enrichment.clone(), provider, self.replay_state.clone())
        });
        let context_resolvers = ContextResolvers::new(self, &context, maybe_origin_tags_resolver)
            .error_context("Failed to create context resolvers.")?;

        let codec_config = DogStatsDCodecConfiguration::default()
            .with_timestamps(self.no_aggregation_pipeline_support)
            .with_permissive_mode(self.permissive_decoding)
            .with_minimum_sample_rate(self.minimum_sample_rate);

        let codec = DogStatsDCodec::from_configuration(codec_config);

        let enable_payloads_filter = EnablePayloadsFilter::default()
            .with_allow_series(self.enable_payloads_series)
            .with_allow_sketches(self.enable_payloads_sketches)
            .with_allow_events(self.enable_payloads_events)
            .with_allow_service_checks(self.enable_payloads_service_checks);
        let traffic_capture = TrafficCapture::new(self.capture_path.clone(), self.capture_depth);
        self.capture_control.bind(traffic_capture.clone());

        Ok(Box::new(DogStatsD {
            listeners,
            io_buffer_pool: FixedSizeObjectPool::with_builder("dsd_packet_bufs", self.buffer_count, || {
                FixedSizeVec::with_capacity(get_adjusted_buffer_size(self.buffer_size))
            }),
            codec,
            context_resolvers,
            enabled_filter: enable_payloads_filter,
            additional_tags: self.additional_tags.clone().into(),
            workload_provider: self.workload_provider.clone(),
            traffic_capture,
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
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DogStatsD>("source struct")
            // We allocate our I/O buffers entirely up front.
            .with_expr(UsageExpr::product(
                "buffers",
                UsageExpr::config("dogstatsd_buffer_count", self.buffer_count),
                UsageExpr::config("dogstatsd_buffer_size", get_adjusted_buffer_size(self.buffer_size)),
            ))
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_expr(UsageExpr::config(
                "dogstatsd_string_interner_size_bytes",
                self.effective_context_string_interner_bytes().as_u64() as usize,
            ));
    }
}

/// DogStatsD source.
pub struct DogStatsD {
    listeners: Vec<Listener>,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    enabled_filter: EnablePayloadsFilter,
    additional_tags: Arc<[String]>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    traffic_capture: TrafficCapture,
}

struct ListenerContext {
    shutdown_handle: DynamicShutdownHandle,
    listener: Listener,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogStatsDCodec,
    context_resolvers: ContextResolvers,
    additional_tags: Arc<[String]>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    traffic_capture: TrafficCapture,
}

struct HandlerContext {
    listen_addr: ListenAddress,
    framer: DsdFramer,
    codec: DogStatsDCodec,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    metrics: Metrics,
    context_resolvers: ContextResolvers,
    additional_tags: Arc<[String]>,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    traffic_capture: TrafficCapture,
}

#[derive(Default)]
struct StreamCapture {
    outer_framer: LengthDelimitedFramer,
    pending: VecDeque<u8>,
    last_pid: Option<i32>,
    last_ancillary_data: Vec<u8>,
}

impl StreamCapture {
    fn push_received(&mut self, peer_addr: &ConnectionAddress, ancillary_data: &[u8], payload: &[u8]) {
        if let Some(process_id) = process_id_from_peer_addr(peer_addr) {
            self.last_pid = Some(process_id);
        }
        if !ancillary_data.is_empty() {
            self.last_ancillary_data = ancillary_data.to_vec();
        }
        self.pending.extend(payload);
    }
}

struct Metrics {
    metrics_received: Counter,
    events_received: Counter,
    service_checks_received: Counter,
    bytes_received: Counter,
    bytes_received_size: Histogram,
    framing_errors: Counter,
    metric_decoder_errors: Counter,
    event_decoder_errors: Counter,
    service_check_decoder_errors: Counter,
    failed_context_resolve_total: Counter,
    connections_active: Gauge,
    packet_receive_success: Counter,
    packet_receive_failure: Counter,
}

impl Metrics {
    fn metrics_received(&self) -> &Counter {
        &self.metrics_received
    }

    fn events_received(&self) -> &Counter {
        &self.events_received
    }

    fn service_checks_received(&self) -> &Counter {
        &self.service_checks_received
    }

    fn bytes_received(&self) -> &Counter {
        &self.bytes_received
    }

    fn bytes_received_size(&self) -> &Histogram {
        &self.bytes_received_size
    }

    fn framing_errors(&self) -> &Counter {
        &self.framing_errors
    }

    fn metric_decode_failed(&self) -> &Counter {
        &self.metric_decoder_errors
    }

    fn event_decode_failed(&self) -> &Counter {
        &self.event_decoder_errors
    }

    fn service_check_decode_failed(&self) -> &Counter {
        &self.service_check_decoder_errors
    }

    fn failed_context_resolve_total(&self) -> &Counter {
        &self.failed_context_resolve_total
    }

    fn connections_active(&self) -> &Gauge {
        &self.connections_active
    }

    fn packet_receive_success(&self) -> &Counter {
        &self.packet_receive_success
    }

    fn packet_receive_failure(&self) -> &Counter {
        &self.packet_receive_failure
    }
}

fn build_metrics(listen_addr: &ListenAddress, component_context: &ComponentContext) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    let listener_type = match listen_addr {
        ListenAddress::Tcp(_) => "tcp",
        ListenAddress::Udp(_) => "udp",
        ListenAddress::Unix(_) => "unix",
        ListenAddress::Unixgram(_) => "unixgram",
    };

    Metrics {
        metrics_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "metrics"), ("listener_type", listener_type)],
        ),
        events_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "events"), ("listener_type", listener_type)],
        ),
        service_checks_received: builder.register_counter_with_tags(
            "component_events_received_total",
            [("message_type", "service_checks"), ("listener_type", listener_type)],
        ),
        bytes_received: builder
            .register_counter_with_tags("component_bytes_received_total", [("listener_type", listener_type)]),
        bytes_received_size: builder
            .register_trace_histogram_with_tags("component_bytes_received_size", [("listener_type", listener_type)]),
        framing_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [("listener_type", listener_type), ("error_type", "framing")],
        ),
        metric_decoder_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "metrics"),
            ],
        ),
        event_decoder_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "events"),
            ],
        ),
        service_check_decoder_errors: builder.register_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "service_checks"),
            ],
        ),
        connections_active: builder
            .register_gauge_with_tags("component_connections_active", [("listener_type", listener_type)]),
        packet_receive_success: builder.register_debug_counter_with_tags(
            "component_packets_received_total",
            [("listener_type", listener_type), ("state", "ok")],
        ),
        packet_receive_failure: builder.register_debug_counter_with_tags(
            "component_packets_received_total",
            [("listener_type", listener_type), ("state", "error")],
        ),
        failed_context_resolve_total: builder.register_debug_counter("component_failed_context_resolve_total"),
    }
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

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
                io_buffer_pool: self.io_buffer_pool.clone(),
                codec: self.codec.clone(),
                context_resolvers: self.context_resolvers.clone(),
                additional_tags: self.additional_tags.clone(),
                workload_provider: self.workload_provider.clone(),
                traffic_capture: self.traffic_capture.clone(),
            };

            spawn_traced_named(
                task_name,
                process_listener(context.clone(), listener_context, self.enabled_filter),
            );
        }

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

        listener_shutdown_coordinator.shutdown().await;

        debug!("DogStatsD source stopped.");

        Ok(())
    }
}

async fn process_listener(
    source_context: SourceContext, listener_context: ListenerContext, enabled_filter: EnablePayloadsFilter,
) {
    let ListenerContext {
        shutdown_handle,
        mut listener,
        io_buffer_pool,
        codec,
        context_resolvers,
        additional_tags,
        workload_provider,
        traffic_capture,
    } = listener_context;
    tokio::pin!(shutdown_handle);

    let listen_addr = listener.listen_address().clone();
    let mut stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

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
                        framer: get_framer(&listen_addr),
                        codec: codec.clone(),
                        io_buffer_pool: io_buffer_pool.clone(),
                        metrics: build_metrics(&listen_addr, source_context.component_context()),
                        context_resolvers: context_resolvers.clone(),
                        additional_tags: additional_tags.clone(),
                        workload_provider: workload_provider.clone(),
                        traffic_capture: traffic_capture.clone(),
                    };

                    let task_name = format!("dogstatsd-stream-handler-{}", listen_addr.listener_type());
                    spawn_traced_named(task_name, process_stream(stream, source_context.clone(), handler_context, stream_shutdown_coordinator.register(), enabled_filter));
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!(%listen_addr, "DogStatsD listener stopped.");
}

async fn process_stream(
    stream: Stream, source_context: SourceContext, handler_context: HandlerContext,
    shutdown_handle: DynamicShutdownHandle, enabled_filter: EnablePayloadsFilter,
) {
    tokio::pin!(shutdown_handle);

    select! {
        _ = &mut shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, handler_context, enabled_filter) => {},
    }
}

async fn drive_stream(
    mut stream: Stream, source_context: SourceContext, handler_context: HandlerContext,
    enabled_filter: EnablePayloadsFilter,
) {
    let HandlerContext {
        listen_addr,
        mut framer,
        codec,
        io_buffer_pool,
        metrics,
        mut context_resolvers,
        additional_tags,
        workload_provider,
        traffic_capture,
    } = handler_context;

    debug!(%listen_addr, "Stream handler started.");

    if !stream.is_connectionless() {
        metrics.connections_active().increment(1);
    }

    let mut stream_capture = match listen_addr {
        ListenAddress::Unix(_) => Some(StreamCapture::default()),
        _ => None,
    };
    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();
    let mut io_buffer_manager = IoBufferManager::new(&io_buffer_pool, &stream);
    let memory_limiter = source_context.topology_context().memory_limiter();

    'read: loop {
        let mut eof = false;

        let mut io_buffer = io_buffer_manager.get_buffer_mut().await;

        memory_limiter.wait_for_capacity().await;

        select! {
            // We read from the stream.
            read_result = stream.receive(&mut io_buffer) => match read_result {
                Ok(ReceiveResult {
                    bytes_read,
                    address: peer_addr,
                    ancillary_data,
                }) => {
                    if bytes_read == 0 {
                        eof = true;
                    }

                    capture_uds_traffic(
                        &listen_addr,
                        &traffic_capture,
                        workload_provider.as_deref(),
                        &codec,
                        &peer_addr,
                        &ancillary_data,
                        received_payload(&io_buffer, bytes_read),
                        stream_capture.as_mut(),
                    );

                    // TODO: This is correct for UDP and UDS in SOCK_DGRAM mode, but not for UDS in SOCK_STREAM mode...
                    // because to match the Datadog Agent, we would only want to increment the number of successful
                    // packets for each length-delimited frame, but this is obviously being incremented before we do any
                    // framing... and even further, with the nested framer, we don't have access to the signal that
                    // we've gotten a full length-delimited outer frame, only each individual newline-delimited inner
                    // frame.
                    //
                    // As such, we'll potentially be over-reporting this metric for UDS in SOCK_STREAM mode compared to
                    // the Datadog Agent.
                    metrics.packet_receive_success().increment(1);
                    metrics.bytes_received().increment(bytes_read as u64);
                    metrics.bytes_received_size().record(bytes_read as f64);

                    // When we're actually at EOF, or we're dealing with a connectionless stream, we try to decode in EOF mode.
                    //
                    // For connectionless streams, we always try to decode the buffer as if it's EOF, since it effectively _is_
                    // always the end of file after a receive. For connection-oriented streams, we only want to do this once we've
                    // actually hit true EOF.
                    let reached_eof = eof || stream.is_connectionless();

                    trace!(
                        buffer_len = io_buffer.remaining(),
                        buffer_cap = io_buffer.remaining_mut(),
                        eof = reached_eof,
                        %listen_addr,
                        %peer_addr,
                        "Received {} bytes from stream.",
                        bytes_read
                    );

                    let mut frames = io_buffer.framed(&mut framer, reached_eof);
                    'frame: loop {
                        match frames.next() {
                            Some(Ok(frame)) => {
                                trace!(%listen_addr, %peer_addr, ?frame, "Decoded frame.");
                                match handle_frame(&frame[..], &codec, &mut context_resolvers, &metrics, &peer_addr, enabled_filter, &additional_tags) {
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
                                        let frame_lossy_str = String::from_utf8_lossy(&frame);
                                        warn!(%listen_addr, %peer_addr, frame = %frame_lossy_str, error = %e, "Failed to parse frame.");
                                    },
                                }
                            }
                            Some(Err(e)) => {
                                metrics.framing_errors().increment(1);

                                if stream.is_connectionless() {
                                    // For connectionless streams, we don't want to shutdown the stream since we can just keep
                                    // reading more packets.
                                    debug!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Continuing stream.");
                                    continue 'read;
                                } else {
                                    debug!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Stopping stream.");
                                    break 'read;
                                }
                            }
                            None => {
                                trace!(%listen_addr, %peer_addr, "Not enough data to decode another frame.");
                                if eof && !stream.is_connectionless() {
                                    debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                                    break 'read;
                                } else {
                                    break 'frame;
                                }
                            }
                        }
                    }
                },
                Err(e) => {
                    metrics.packet_receive_failure().increment(1);

                    if stream.is_connectionless() {
                        // For connectionless streams, we don't want to shutdown the stream since we can just keep
                        // reading more packets.
                        warn!(%listen_addr, error = %e, "I/O error while decoding. Continuing stream.");
                        continue 'read;
                    } else {
                        warn!(%listen_addr, error = %e, "I/O error while decoding. Stopping stream.");
                        break 'read;
                    }
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

    metrics.connections_active().decrement(1);

    debug!(%listen_addr, "Stream handler stopped.");
}

fn capture_uds_traffic(
    listen_addr: &ListenAddress, traffic_capture: &TrafficCapture,
    workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>, codec: &DogStatsDCodec,
    peer_addr: &ConnectionAddress, ancillary_data: &[u8], payload: &[u8], stream_capture: Option<&mut StreamCapture>,
) {
    if payload.is_empty() || !traffic_capture.is_ongoing() {
        return;
    }

    match listen_addr {
        ListenAddress::Unixgram(_) => {
            let _ = traffic_capture.enqueue(build_capture_record(
                codec,
                workload_provider,
                process_id_from_peer_addr(peer_addr),
                ancillary_data,
                payload,
            ));
        }
        ListenAddress::Unix(_) => {
            let Some(stream_capture) = stream_capture else {
                return;
            };

            stream_capture.push_received(peer_addr, ancillary_data, payload);

            while let Ok(Some(outer_payload)) = stream_capture
                .outer_framer
                .next_frame(&mut stream_capture.pending, false)
            {
                let _ = traffic_capture.enqueue(build_capture_record(
                    codec,
                    workload_provider,
                    stream_capture.last_pid,
                    &stream_capture.last_ancillary_data,
                    &outer_payload,
                ));
            }
        }
        _ => {}
    }
}

fn build_capture_record(
    codec: &DogStatsDCodec, workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>, process_id: Option<i32>,
    ancillary_data: &[u8], payload: &[u8],
) -> CaptureRecord {
    CaptureRecord {
        timestamp_ns: capture_timestamp_ns(),
        payload: payload.to_vec(),
        pid: process_id,
        ancillary: ancillary_data.to_vec(),
        container_id: resolve_capture_container_id(codec, workload_provider, process_id, payload),
    }
}

fn resolve_capture_container_id(
    codec: &DogStatsDCodec, workload_provider: Option<&(dyn WorkloadProvider + Send + Sync)>, process_id: Option<i32>,
    payload: &[u8],
) -> Option<String> {
    payload_local_container_id(codec, payload).or_else(|| {
        let process_id = u32::try_from(process_id?).ok()?;
        workload_provider
            .and_then(|provider| provider.resolve_container_entity_for_pid(process_id))
            .and_then(EntityId::try_into_container)
            .map(|container_id| container_id.to_string())
    })
}

fn payload_local_container_id(codec: &DogStatsDCodec, payload: &[u8]) -> Option<String> {
    let mut framer = NewlineFramer::default().required_on_eof(false);
    let mut remaining = payload;

    while let Ok(Some(frame)) = framer.next_frame(&mut remaining, true) {
        if let Ok(parsed) = codec.decode_packet(&frame) {
            if let Some(container_id) = parsed_packet_local_container_id(parsed) {
                return Some(container_id);
            }
        }
    }

    None
}

fn parsed_packet_local_container_id(parsed: ParsedPacket<'_>) -> Option<String> {
    let maybe_local_data = match parsed {
        ParsedPacket::Metric(packet) => packet.local_data,
        ParsedPacket::Event(packet) => packet.local_data,
        ParsedPacket::ServiceCheck(packet) => packet.local_data,
    };

    maybe_local_data
        .and_then(EntityId::from_local_data)
        .and_then(EntityId::try_into_container)
        .map(|container_id| container_id.to_string())
}

fn process_id_from_peer_addr(peer_addr: &ConnectionAddress) -> Option<i32> {
    match peer_addr {
        ConnectionAddress::ProcessLike(Some(creds)) if creds.gid == REPLAY_CREDENTIALS_GID => {
            i32::try_from(creds.uid).ok()
        }
        ConnectionAddress::ProcessLike(Some(creds)) => Some(creds.pid),
        _ => None,
    }
}

fn process_id_for_origin(peer_addr: &ConnectionAddress) -> Option<u32> {
    match peer_addr {
        ConnectionAddress::ProcessLike(Some(creds)) if creds.gid == REPLAY_CREDENTIALS_GID => Some(creds.uid),
        ConnectionAddress::ProcessLike(Some(creds)) => u32::try_from(creds.pid).ok(),
        _ => None,
    }
}

fn is_replay_peer_addr(peer_addr: &ConnectionAddress) -> bool {
    matches!(peer_addr, ConnectionAddress::ProcessLike(Some(creds)) if creds.gid == REPLAY_CREDENTIALS_GID)
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

fn handle_frame(
    frame: &[u8], codec: &DogStatsDCodec, context_resolvers: &mut ContextResolvers, source_metrics: &Metrics,
    peer_addr: &ConnectionAddress, enabled_filter: EnablePayloadsFilter, additional_tags: &[String],
) -> Result<Option<Event>, ParseError> {
    let parsed = match codec.decode_packet(frame) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Try and determine what the message type was, if possible, to increment the correct error counter.
            match parse_message_type(frame) {
                MessageType::MetricSample => source_metrics.metric_decode_failed().increment(1),
                MessageType::Event => source_metrics.event_decode_failed().increment(1),
                MessageType::ServiceCheck => source_metrics.service_check_decode_failed().increment(1),
            }

            return Err(e);
        }
    };

    let event = match parsed {
        ParsedPacket::Metric(metric_packet) => {
            let events_len = metric_packet.num_points;
            if !enabled_filter.allow_metric(&metric_packet) {
                trace!(
                    metric.name = metric_packet.metric_name,
                    "Skipping metric due to filter configuration."
                );
                return Ok(None);
            }

            match handle_metric_packet(metric_packet, context_resolvers, peer_addr, additional_tags) {
                Some(metric) => {
                    source_metrics.metrics_received().increment(events_len);
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
            let tags_resolver = context_resolvers.tags();
            match handle_event_packet(event, tags_resolver, peer_addr, additional_tags) {
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
            let tags_resolver = context_resolvers.tags();
            match handle_service_check_packet(service_check, tags_resolver, peer_addr, additional_tags) {
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
    packet: MetricPacket, context_resolvers: &mut ContextResolvers, peer_addr: &ConnectionAddress,
    additional_tags: &[String],
) -> Option<Metric> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_metric_packet(&packet, &well_known_tags);
    if let Some(process_id) = process_id_for_origin(peer_addr) {
        origin.set_process_id(process_id);
    }
    origin.set_replay(is_replay_peer_addr(peer_addr));

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);

    // Try to resolve the context for this metric.
    match context_resolver.resolve(packet.metric_name, tags, Some(origin)) {
        Some(context) => {
            let metric_origin = well_known_tags
                .jmx_check_name
                .map(MetricOrigin::jmx_check)
                .unwrap_or_else(MetricOrigin::dogstatsd);
            let metadata = MetricMetadata::default()
                .with_origin(metric_origin)
                .with_hostname(well_known_tags.hostname.map(Arc::from));

            Some(Metric::from_parts(context, packet.values, metadata))
        }
        // We failed to resolve the context, likely due to not having enough interner capacity.
        None => None,
    }
}

fn handle_event_packet(
    packet: EventPacket, tags_resolver: &mut TagsResolver, peer_addr: &ConnectionAddress, additional_tags: &[String],
) -> Option<EventD> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_event_packet(&packet, &well_known_tags);
    if let Some(process_id) = process_id_for_origin(peer_addr) {
        origin.set_process_id(process_id);
    }
    origin.set_replay(is_replay_peer_addr(peer_addr));
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags = tags_resolver.create_tag_set(tags)?;

    let eventd = EventD::new(packet.title, packet.text)
        .with_timestamp(packet.timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_aggregation_key(packet.aggregation_key.map(|s| s.into()))
        .with_alert_type(packet.alert_type)
        .with_priority(packet.priority)
        .with_source_type_name(packet.source_type_name.map(|s| s.into()))
        .with_alert_type(packet.alert_type)
        .with_tags(tags)
        .with_origin_tags(origin_tags);

    Some(eventd)
}

fn handle_service_check_packet(
    packet: ServiceCheckPacket, tags_resolver: &mut TagsResolver, peer_addr: &ConnectionAddress,
    additional_tags: &[String],
) -> Option<ServiceCheck> {
    let well_known_tags = WellKnownTags::from_raw_tags(packet.tags.clone());

    let mut origin = origin_from_service_check_packet(&packet, &well_known_tags);
    if let Some(process_id) = process_id_for_origin(peer_addr) {
        origin.set_process_id(process_id);
    }
    origin.set_replay(is_replay_peer_addr(peer_addr));
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    let tags = get_filtered_tags_iterator(packet.tags, additional_tags);
    let tags = tags_resolver.create_tag_set(tags)?;

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(packet.timestamp)
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
        if let Err(e) = source_context
            .dispatcher()
            .buffered_named("events")
            .expect("events output should always exist")
            .send_all(eventd_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch eventd events.");
        }
    }

    // Dispatch any service check events, if present.
    if event_buffer.has_event_type(EventType::ServiceCheck) {
        let service_check_events = event_buffer.extract(Event::is_service_check);
        if let Err(e) = source_context
            .dispatcher()
            .buffered_named("service_checks")
            .expect("service checks output should always exist")
            .send_all(service_check_events)
            .await
        {
            error!(%listen_addr, error = %e, "Failed to dispatch service check events.");
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
    use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

    use bytesize::ByteSize;
    use saluki_config::ConfigurationLoader;
    use saluki_context::tags::SharedTagSet;
    use saluki_context::{ContextResolverBuilder, TagsResolverBuilder};
    use saluki_env::{
        workload::{origin::ResolvedOrigin, EntityId},
        WorkloadProvider,
    };
    use saluki_io::{
        deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration, ParsedPacket},
        net::ConnectionAddress,
    };
    use serde_json::json;

    use super::{
        handle_metric_packet, is_replay_peer_addr, payload_local_container_id, process_id_for_origin,
        process_id_from_peer_addr, resolve_capture_container_id, ContextResolvers, DogStatsDConfiguration,
        DOGSTATSD_CAPTURE_DIR, REPLAY_CREDENTIALS_GID,
    };

    #[derive(Default)]
    struct CaptureTestWorkloadProvider {
        pid_map: HashMap<u32, EntityId>,
    }

    impl CaptureTestWorkloadProvider {
        fn with_pid_mapping(process_id: u32, entity_id: EntityId) -> Self {
            let mut pid_map = HashMap::new();
            pid_map.insert(process_id, entity_id);
            Self { pid_map }
        }
    }

    impl WorkloadProvider for CaptureTestWorkloadProvider {
        fn get_tags_for_entity(
            &self, _entity_id: &EntityId, _cardinality: saluki_context::origin::OriginTagCardinality,
        ) -> Option<SharedTagSet> {
            None
        }

        fn get_resolved_origin(&self, _origin: saluki_context::origin::RawOrigin<'_>) -> Option<ResolvedOrigin> {
            None
        }

        fn resolve_container_entity_for_pid(&self, process_id: u32) -> Option<EntityId> {
            self.pid_map.get(&process_id).cloned()
        }
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
        let peer_addr = ConnectionAddress::from("1.1.1.1:1234".parse::<SocketAddr>().unwrap());

        let input = "big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
            panic!("Failed to parse packet.");
        };

        let maybe_metric = handle_metric_packet(packet, &mut context_resolvers, &peer_addr, &[]);
        assert!(maybe_metric.is_none());
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
        let peer_addr = ConnectionAddress::from("1.1.1.1:1234".parse::<SocketAddr>().unwrap());

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
        let maybe_metric = handle_metric_packet(packet, &mut context_resolvers, &peer_addr, &additional_tags);
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

    #[test]
    fn interner_size_defaults_to_2mib() {
        let config = deser_config("{}");
        assert_eq!(config.effective_context_string_interner_bytes(), ByteSize::mib(2));
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

    #[test]
    fn payload_local_container_id_reads_local_data() {
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let payload = b"test.metric:1|c|c:ci-local-container\n";

        assert_eq!(
            payload_local_container_id(&codec, payload),
            Some("local-container".to_string())
        );
    }

    #[test]
    fn resolve_capture_container_id_falls_back_to_pid_mapping() {
        let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
        let workload_provider = CaptureTestWorkloadProvider::with_pid_mapping(
            42,
            EntityId::from_local_data("ci-pid-container").expect("container entity"),
        );

        assert_eq!(
            resolve_capture_container_id(&codec, Some(&workload_provider), Some(42), b"test.metric:1|c\n"),
            Some("pid-container".to_string())
        );
    }

    #[test]
    fn stream_capture_preserves_last_pid_without_new_creds() {
        let mut stream_capture = super::StreamCapture::default();

        stream_capture.push_received(
            &ConnectionAddress::ProcessLike(Some(saluki_io::net::ProcessCredentials {
                pid: 42,
                uid: 0,
                gid: 0,
            })),
            &[],
            b"first",
        );
        stream_capture.push_received(&ConnectionAddress::ProcessLike(None), &[], b"second");

        assert_eq!(stream_capture.last_pid, Some(42));
    }

    #[test]
    fn process_id_from_replay_peer_addr_uses_uid_as_captured_pid() {
        let peer_addr = ConnectionAddress::ProcessLike(Some(saluki_io::net::ProcessCredentials {
            pid: 999,
            uid: 42,
            gid: REPLAY_CREDENTIALS_GID,
        }));

        assert_eq!(process_id_from_peer_addr(&peer_addr), Some(42));
        assert_eq!(process_id_for_origin(&peer_addr), Some(42));
        assert!(is_replay_peer_addr(&peer_addr));
    }
}
