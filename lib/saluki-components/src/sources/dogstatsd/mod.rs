use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use metrics::{Counter, Gauge, Histogram};
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_context::TagsResolver;
use saluki_core::data_model::event::eventd::EventD;
use saluki_core::data_model::event::metric::{MetricMetadata, MetricOrigin};
use saluki_core::data_model::event::service_check::ServiceCheck;
use saluki_core::data_model::event::{metric::Metric, Event, EventType};
use saluki_core::topology::EventsBuffer;
use saluki_core::{
    components::{sources::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::FixedSizeObjectPool,
    topology::{
        interconnect::EventBufferManager,
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_env::WorkloadProvider;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::deser::codec::dogstatsd::{EventPacket, ServiceCheckPacket};
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec},
    deser::{
        codec::{
            dogstatsd::{parse_message_type, MessageType, MetricPacket, ParseError, ParsedPacket},
            DogstatsdCodec, DogstatsdCodecConfiguration,
        },
        framing::FramerExt as _,
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, Stream,
    },
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use tokio::{
    select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

mod framer;
use self::framer::{get_framer, DsdFramer};

mod filters;
use self::filters::EnablePayloadsFilter;

mod io_buffer;
use self::io_buffer::IoBufferManager;

mod origin;
use self::origin::{
    origin_from_event_packet, origin_from_metric_packet, origin_from_service_check_packet, DogStatsDOriginTagResolver,
    OriginEnrichmentConfiguration,
};

mod resolver;
use self::resolver::ContextResolvers;

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

const fn default_allow_context_heap_allocations() -> bool {
    true
}

const fn default_no_aggregation_pipeline_support() -> bool {
    true
}

const fn default_context_string_interner_size() -> ByteSize {
    ByteSize::mib(2)
}

const fn default_dogstatsd_permissive_decoding() -> bool {
    true
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

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
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

    /// The Unix domain socket path to listen on, in datagram mode.
    ///
    /// If not set, UDS (in datagram mode) is not used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_socket")]
    socket_path: Option<String>,

    /// The Unix domain socket path to listen on, in stream mode.
    ///
    /// If not set, UDS (in stream mode) is not used.
    ///
    /// Defaults to unset.
    #[serde(rename = "dogstatsd_stream_socket")]
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

    /// Total size of the string interner used for contexts.
    ///
    /// This controls the amount of memory that can be used to intern metric names and tags. If the interner is full,
    /// metrics with contexts that have not already been resolved may or may not be dropped, depending on the value of
    /// `allow_context_heap_allocations`.
    #[serde(
        rename = "dogstatsd_string_interner_size",
        default = "default_context_string_interner_size"
    )]
    context_string_interner_bytes: ByteSize,

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

    /// Additional tags to add to all metrics.
    #[serde(rename = "dogstatsd_tags", default)]
    additional_tags: Vec<String>,
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
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

        let maybe_origin_tags_resolver = self
            .workload_provider
            .clone()
            .map(|provider| DogStatsDOriginTagResolver::new(self.origin_enrichment.clone(), provider));
        let context_resolvers = ContextResolvers::new(self, &context, maybe_origin_tags_resolver)
            .error_context("Failed to create context resolvers.")?;

        let codec_config = DogstatsdCodecConfiguration::default()
            .with_timestamps(self.no_aggregation_pipeline_support)
            .with_permissive_mode(self.permissive_decoding);

        let codec = DogstatsdCodec::from_configuration(codec_config);

        let enable_payloads_filter = EnablePayloadsFilter::default()
            .with_allow_series(self.enable_payloads_series)
            .with_allow_sketches(self.enable_payloads_sketches)
            .with_allow_events(self.enable_payloads_events)
            .with_allow_service_checks(self.enable_payloads_service_checks);

        Ok(Box::new(DogStatsD {
            listeners,
            io_buffer_pool: FixedSizeObjectPool::with_builder("dsd_packet_bufs", self.buffer_count, || {
                FixedSizeVec::with_capacity(get_adjusted_buffer_size(self.buffer_size))
            }),
            codec,
            context_resolvers,
            enabled_filter: enable_payloads_filter,
            additional_tags: self.additional_tags.clone().into(),
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
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
                "dogstatsd_string_interner_size",
                self.context_string_interner_bytes.as_u64() as usize,
            ));
    }
}

pub struct DogStatsD {
    listeners: Vec<Listener>,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogstatsdCodec,
    context_resolvers: ContextResolvers,
    enabled_filter: EnablePayloadsFilter,
    additional_tags: Arc<[String]>,
}

struct ListenerContext {
    shutdown_handle: DynamicShutdownHandle,
    listener: Listener,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogstatsdCodec,
    context_resolvers: ContextResolvers,
    additional_tags: Arc<[String]>,
}

struct HandlerContext {
    listen_addr: ListenAddress,
    framer: DsdFramer,
    codec: DogstatsdCodec,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    metrics: Metrics,
    context_resolvers: ContextResolvers,
    additional_tags: Arc<[String]>,
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
        ListenAddress::Tcp(_) => unreachable!("TCP is not supported for DogStatsD"),
        ListenAddress::Udp(_) => "udp",
        ListenAddress::Unix(_) => "unix",
        ListenAddress::Unixgram(_) => "unixgram",
    };

    Metrics {
        metrics_received: builder.register_debug_counter_with_tags(
            "component_events_received_total",
            [("message_type", "metrics"), ("listener_type", listener_type)],
        ),
        events_received: builder.register_debug_counter_with_tags(
            "component_events_received_total",
            [("message_type", "events"), ("listener_type", listener_type)],
        ),
        service_checks_received: builder.register_debug_counter_with_tags(
            "component_events_received_total",
            [("message_type", "service_checks"), ("listener_type", listener_type)],
        ),
        bytes_received: builder
            .register_debug_counter_with_tags("component_bytes_received_total", [("listener_type", listener_type)]),
        bytes_received_size: builder
            .register_debug_histogram_with_tags("component_bytes_received_size", [("listener_type", listener_type)]),
        framing_errors: builder.register_debug_counter_with_tags(
            "component_errors_total",
            [("listener_type", listener_type), ("error_type", "framing")],
        ),
        metric_decoder_errors: builder.register_debug_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "metrics"),
            ],
        ),
        event_decoder_errors: builder.register_debug_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "events"),
            ],
        ),
        service_check_decoder_errors: builder.register_debug_counter_with_tags(
            "component_errors_total",
            [
                ("listener_type", listener_type),
                ("error_type", "decode"),
                ("message_type", "service_checks"),
            ],
        ),
        connections_active: builder
            .register_debug_gauge_with_tags("component_connections_active", [("listener_type", listener_type)]),
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
            };

            spawn_traced_named(
                task_name,
                process_listener(context.clone(), listener_context, self.enabled_filter),
            );
        }

        health.mark_ready();
        info!("DogStatsD source started.");

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

        info!("Stopping DogStatsD source...");

        listener_shutdown_coordinator.shutdown().await;

        info!("DogStatsD source stopped.");

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
    } = handler_context;

    debug!(%listen_addr, "Stream handler started.");

    if !stream.is_connectionless() {
        metrics.connections_active().increment(1);
    }

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
                Ok((bytes_read, peer_addr)) => {
                    if bytes_read == 0 {
                        eof = true;
                    }

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
                                    Err(e) => warn!(%listen_addr, %peer_addr, error = %e, "Failed to parse frame."),
                                }
                            }
                            Some(Err(e)) => {
                                metrics.framing_errors().increment(1);

                                if stream.is_connectionless() {
                                    // For connectionless streams, we don't want to shutdown the stream since we can just keep
                                    // reading more packets.
                                    warn!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Continuing stream.");
                                    continue 'read;
                                } else {
                                    warn!(%listen_addr, %peer_addr, error = %e, "Error decoding frame. Stopping stream.");
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

fn handle_frame(
    frame: &[u8], codec: &DogstatsdCodec, context_resolvers: &mut ContextResolvers, source_metrics: &Metrics,
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
    // Capture the origin from the packet, including any process ID information if we have it.
    let mut origin = origin_from_metric_packet(&packet);
    if let ConnectionAddress::ProcessLike(Some(creds)) = &peer_addr {
        origin.set_process_id(creds.pid as u32);
    }

    // Choose the right context resolver based on whether or not this metric is pre-aggregated.
    let context_resolver = if packet.timestamp.is_some() {
        context_resolvers.no_agg()
    } else {
        context_resolvers.primary()
    };

    // Chain the existing tags with the additional tags.
    let tags = packet
        .tags
        .clone()
        .into_iter()
        .chain(additional_tags.iter().map(|s| s.as_str()));

    // Try to resolve the context for this metric.
    match context_resolver.resolve(packet.metric_name, tags, Some(origin)) {
        Some(context) => {
            let metric_origin = packet
                .jmx_check_name
                .map(MetricOrigin::jmx_check)
                .unwrap_or_else(MetricOrigin::dogstatsd);
            let metadata = MetricMetadata::default().with_origin(metric_origin);

            Some(Metric::from_parts(context, packet.values, metadata))
        }
        // We failed to resolve the context, likely due to not having enough interner capacity.
        None => None,
    }
}

fn handle_event_packet(
    packet: EventPacket, tags_resolver: &mut TagsResolver, peer_addr: &ConnectionAddress, additional_tags: &[String],
) -> Option<EventD> {
    // Capture the origin from the packet, including any process ID information if we have it.
    let mut origin = origin_from_event_packet(&packet);
    if let ConnectionAddress::ProcessLike(Some(creds)) = &peer_addr {
        origin.set_process_id(creds.pid as u32);
    }
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    // Chain the existing tags with the additional tags.
    let tags = packet
        .tags
        .clone()
        .into_iter()
        .chain(additional_tags.iter().map(|s| s.as_str()));

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
    let mut origin = origin_from_service_check_packet(&packet);
    if let ConnectionAddress::ProcessLike(Some(creds)) = &peer_addr {
        origin.set_process_id(creds.pid as u32);
    }
    let origin_tags = tags_resolver.resolve_origin_tags(Some(origin));

    // Chain the existing tags with the additional tags.
    let tags = packet
        .tags
        .clone()
        .into_iter()
        .chain(additional_tags.iter().map(|s| s.as_str()))
        .chain(origin_tags.into_iter().map(|s| s.as_str()));

    let tags = tags_resolver.create_tag_set(tags)?;

    let service_check = ServiceCheck::new(packet.name, packet.status)
        .with_timestamp(packet.timestamp)
        .with_hostname(packet.hostname.map(|s| s.into()))
        .with_tags(tags)
        .with_message(packet.message.map(|s| s.into()));

    Some(service_check)
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
    use std::net::SocketAddr;

    use saluki_context::{ContextResolverBuilder, TagsResolverBuilder};
    use saluki_io::{
        deser::codec::{dogstatsd::ParsedPacket, DogstatsdCodec, DogstatsdCodecConfiguration},
        net::ConnectionAddress,
    };

    use super::{handle_metric_packet, ContextResolvers};

    #[test]
    fn no_metrics_when_interner_full_allocations_disallowed() {
        // We're specifically testing here that when we don't allow outside allocations, we should not be able to
        // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
        // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
        // _inlined_ either, since that will get around the interner being full.
        //
        // We set our metric name to be longer than 31 bytes (the inlining limit) to ensure this.

        let codec = DogstatsdCodec::from_configuration(DogstatsdCodecConfiguration::default());
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
        let codec = DogstatsdCodec::from_configuration(DogstatsdCodecConfiguration::default());
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
}
