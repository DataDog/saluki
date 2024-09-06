use std::num::NonZeroUsize;
use std::sync::LazyLock;

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use metrics::{Counter, Histogram};
use saluki_config::GenericConfiguration;
use saluki_context::ContextResolver;
use saluki_core::{
    components::{sources::*, MetricsBuilder},
    pooling::{FixedSizeObjectPool, ObjectPool as _},
    spawn_traced,
    topology::{
        interconnect::EventBuffer,
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_event::{DataType, Event};
use saluki_io::{
    buf::{get_fixed_bytes_buffer_pool, BytesBuffer},
    deser::{
        codec::{dogstatsd::ParseError, DogstatsdCodec, DogstatsdCodecConfiguration},
        framing::FramerExt as _,
    },
    net::{
        listener::{Listener, ListenerError},
        ConnectionAddress, ListenAddress, Stream,
    },
};
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use stringtheory::interning::FixedSizeInterner;
use tokio::select;
use tracing::{debug, error, info, trace};

mod framer;
use self::framer::{get_framer, DsdFramer};

mod interceptor;
use self::interceptor::AgentLikeTagMetadataInterceptor;

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

    /// Whether or not to enable origin detection.
    ///
    /// Origin detection attempts to extract the sending process ID from the socket credentials of the client, which is
    /// later used to map a metric to the container that sent it.
    ///
    /// Only relevant in UDS mode.
    ///
    /// Defaults to `false`.
    #[serde(rename = "dogstatsd_origin_detection", default)]
    origin_detection: bool,

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
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
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
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        let listeners = self.build_listeners().await?;
        if listeners.is_empty() {
            return Err(Error::NoListenersConfigured.into());
        }

        let context_string_interner_size = NonZeroUsize::new(self.context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;
        let context_interner = FixedSizeInterner::new(context_string_interner_size);
        let context_resolver = ContextResolver::from_interner("dogstatsd", context_interner)
            .with_heap_allocations(self.allow_context_heap_allocations);

        let codec_config = DogstatsdCodecConfiguration::default().with_timestamps(self.no_aggregation_pipeline_support);
        let codec = DogstatsdCodec::from_context_resolver(context_resolver)
            .with_configuration(codec_config)
            .with_tag_metadata_interceptor(AgentLikeTagMetadataInterceptor);

        Ok(Box::new(DogStatsD {
            listeners,
            io_buffer_pool: get_fixed_bytes_buffer_pool(self.buffer_count, self.buffer_size),
            codec,
            origin_detection: self.origin_detection,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", DataType::Metric),
                OutputDefinition::named_output("events", DataType::EventD),
                OutputDefinition::named_output("service_checks", DataType::ServiceCheck),
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
            .with_single_value::<DogStatsD>()
            // We allocate our I/O buffers entirely up front.
            .with_fixed_amount(self.buffer_count * self.buffer_size)
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount(self.context_string_interner_bytes.as_u64() as usize);
    }
}

struct HandlerContext {
    listen_addr: String,
    origin_detection: bool,
    framer: DsdFramer,
    codec: DogstatsdCodec<AgentLikeTagMetadataInterceptor>,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    metrics: Metrics,
}

struct ListenerContext {
    shutdown_handle: DynamicShutdownHandle,
    listener: Listener,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogstatsdCodec<AgentLikeTagMetadataInterceptor>,
    origin_detection: bool,
}

pub struct DogStatsD {
    listeners: Vec<Listener>,
    io_buffer_pool: FixedSizeObjectPool<BytesBuffer>,
    codec: DogstatsdCodec<AgentLikeTagMetadataInterceptor>,
    origin_detection: bool,
}

struct Metrics {
    events_received: Counter,
    bytes_received: Counter,
    bytes_received_size: Histogram,
    decoder_errors: Counter,
}

impl Metrics {
    fn events_received(&self) -> &Counter {
        &self.events_received
    }

    fn bytes_received(&self) -> &Counter {
        &self.bytes_received
    }

    fn bytes_received_size(&self) -> &Histogram {
        &self.bytes_received_size
    }

    fn decoder_errors(&self) -> &Counter {
        &self.decoder_errors
    }
}

fn build_metrics(builder: MetricsBuilder) -> Metrics {
    Metrics {
        events_received: builder.register_counter("component_events_received_total"),
        bytes_received: builder.register_counter("component_bytes_received_total"),
        bytes_received_size: builder.register_histogram("component_bytes_received_size"),
        decoder_errors: builder.register_counter_with_labels("component_errors_total", &[("error_type", "decode")]),
    }
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
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
                origin_detection: self.origin_detection,
            };

            spawn_traced(process_listener(context.clone(), listener_context));
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

async fn process_listener(source_context: SourceContext, listener_context: ListenerContext) {
    let ListenerContext {
        shutdown_handle,
        mut listener,
        io_buffer_pool,
        codec,
        origin_detection,
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
                        listen_addr: listen_addr.to_string(),
                        origin_detection,
                        framer: get_framer(&listen_addr),
                        codec: codec.clone(),
                        io_buffer_pool: io_buffer_pool.clone(),
                        metrics: build_metrics(MetricsBuilder::from_component_context(source_context.component_context())),
                    };
                    spawn_traced(process_stream(stream, source_context.clone(), handler_context, stream_shutdown_coordinator.register()));
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
    shutdown_handle: DynamicShutdownHandle,
) {
    tokio::pin!(shutdown_handle);

    select! {
        _ = &mut shutdown_handle => {
            debug!("Stream handler received shutdown signal.");
        },
        _ = drive_stream(stream, source_context, handler_context) => {},
    }
}

async fn drive_stream(mut stream: Stream, source_context: SourceContext, handler_context: HandlerContext) {
    let HandlerContext {
        listen_addr,
        origin_detection,
        mut framer,
        mut codec,
        io_buffer_pool,
        metrics,
        ..
    } = handler_context;

    loop {
        let mut eof = false;
        // let mut eof_addr = None;

        source_context.memory_limiter().wait_for_capacity().await;
        let mut event_buffer = source_context.event_buffer_pool().acquire().await;

        let mut buffer = io_buffer_pool.acquire().await;
        debug!(capacity = buffer.remaining_mut(), "Acquired buffer for decoding.");

        // If our buffer is full, we can't do any reads.
        if !buffer.has_remaining_mut() {
            // try to get a new buffer on the next iteration?
            error!("Newly acquired buffer has no capacity. This should never happen.");
            continue;
        }

        // Try filling our buffer from the underlying reader first.
        debug!("About to receive data from the stream.");
        let (bytes_read, peer_addr) = match stream.receive(&mut buffer).await {
            Ok((bytes_read, peer_addr)) => (bytes_read, peer_addr),
            Err(error) => {
                error!(%listen_addr, %error, "I/O error while decoding. Stopping stream.");
                break;
            }
        };

        if bytes_read == 0 {
            eof = true;
        }

        metrics.bytes_received().increment(bytes_read as u64);
        metrics.bytes_received_size().record(bytes_read as f64);

        // When we're actually at EOF, or we're dealing with a connectionless stream, we try to decode in EOF mode.
        //
        // For connectionless streams, we always try to decode the buffer as if it's EOF, since it effectively _is_
        // always the end of file after a receive. For connection-oriented streams, we only want to do this once we've
        // actually hit true EOF.
        let reached_eof = eof || stream.is_connectionless();

        debug!(
            chunk_len = buffer.chunk().len(),
            chunk_cap = buffer.chunk_mut().len(),
            buffer_len = buffer.remaining(),
            buffer_cap = buffer.remaining_mut(),
            eof = reached_eof,
            "Received {} bytes from stream.",
            bytes_read
        );

        let mut frames = buffer.framed(&mut framer, reached_eof);
        loop {
            match frames.next() {
                Some(Ok(frame)) => {
                    trace!(?frame, "Decoded frame.");

                    if let Err(e) =
                        handle_frame(&frame[..], &mut codec, &mut event_buffer, origin_detection, &peer_addr)
                    {
                        error!(%listen_addr, error = %e, "Failed to parse frame.");
                    }
                }
                Some(Err(e)) => {
                    error!(error = %e, "Error decoding frame.");
                    metrics.decoder_errors().increment(1);
                    break;
                }
                None => {
                    debug!("Not enough data to decode another frame.");
                    if eof && !stream.is_connectionless() {
                        trace!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                        return;
                    } else {
                        break;
                    }
                }
            }
        }
        metrics.events_received().increment(event_buffer.len() as u64);
        forward_events(event_buffer, &source_context, &peer_addr, &listen_addr).await;
    }
}

fn handle_frame(
    frame: &[u8], codec: &mut DogstatsdCodec<AgentLikeTagMetadataInterceptor>, event_buffer: &mut EventBuffer,
    origin_detection: bool, peer_addr: &ConnectionAddress,
) -> Result<(), ParseError> {
    codec.decode_packet(frame, event_buffer)?;

    // We do one optional enrichment step here, which is to add the client's socket credentials as a tag
    // on each metric, if they came over UDS. This would then be utilized downstream in the pipeline by
    // origin enrichment, if present.
    if origin_detection {
        if let ConnectionAddress::ProcessLike(Some(creds)) = &peer_addr {
            for event in event_buffer {
                if let Some(metric) = event.try_as_metric_mut() {
                    metric
                        .metadata_mut()
                        .origin_entity_mut()
                        .set_process_id(creds.pid as u32);
                }
            }
        }
    }

    Ok(())
}

async fn forward_events(
    mut event_buffer: EventBuffer, source_context: &SourceContext, peer_addr: &ConnectionAddress, listen_addr: &str,
) {
    let n = event_buffer.len();

    // Extract eventd events only if at least one is present in the event buffer.
    let maybe_eventd_event_buffer = match event_buffer.has_data_type(DataType::EventD) {
        true => {
            let mut eventd_event_buffer = source_context.event_buffer_pool().acquire().await;
            eventd_event_buffer.extend(event_buffer.extract(Event::is_eventd));
            Some(eventd_event_buffer)
        }
        false => None,
    };

    // Extract service check events only if at least one is present in the event buffer.
    let maybe_service_checks_event_buffer = match event_buffer.has_data_type(DataType::ServiceCheck) {
        true => {
            let mut service_check_event_buffer = source_context.event_buffer_pool().acquire().await;
            service_check_event_buffer.extend(event_buffer.extract(Event::is_service_check));
            Some(service_check_event_buffer)
        }
        false => None,
    };

    trace!(%listen_addr, %peer_addr, events_len = n, "Forwarding events.");

    if let Err(e) = source_context.forwarder().forward_named("metrics", event_buffer).await {
        error!(%listen_addr, %peer_addr, error = %e, "Failed to forward metric events.");
    }

    if let Some(eventd_event_buffer) = maybe_eventd_event_buffer {
        if let Err(e) = source_context
            .forwarder()
            .forward_named("events", eventd_event_buffer)
            .await
        {
            error!(%listen_addr, %peer_addr, error = %e, "Failed to forward eventd events.");
        }
    }

    if let Some(service_checks_event_buffer) = maybe_service_checks_event_buffer {
        if let Err(e) = source_context
            .forwarder()
            .forward_named("service_checks", service_checks_event_buffer)
            .await
        {
            error!(%listen_addr, %peer_addr, error = %e, "Failed to forward service checks events.");
        }
    }
}
