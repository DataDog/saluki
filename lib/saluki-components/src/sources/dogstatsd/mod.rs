use std::num::NonZeroUsize;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::ContextResolver;
use saluki_core::{
    buffers::FixedSizeBufferPool,
    components::{metrics::MetricsBuilder, sources::*},
    topology::{
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_error::GenericError;
use saluki_event::{metric::OriginEntity, DataType, Event};
use saluki_io::{
    buf::{get_fixed_bytes_buffer_pool, BytesBuffer},
    deser::{codec::DogstatsdCodec, Deserializer, DeserializerBuilder, DeserializerError},
    net::{
        addr::{ConnectionAddress, ListenAddress},
        listener::{Listener, ListenerError},
    },
};
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use stringtheory::interning::FixedSizeInterner;
use tokio::select;
use tracing::{debug, error, info, trace};

mod framer;
use self::framer::{get_framer, DogStatsDMultiFraming};

// Intern up to 1MB of metric names/tags.
const DEFAULT_CONTEXT_INTERNER_SIZE_BYTES: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(2 * 1024 * 1024) };

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

        Ok(Box::new(DogStatsD {
            listeners,
            io_buffer_pool: get_fixed_bytes_buffer_pool(self.buffer_count, self.buffer_size),
            context_resolver: ContextResolver::from_interner(
                "dogstatsd",
                FixedSizeInterner::new(DEFAULT_CONTEXT_INTERNER_SIZE_BYTES),
            ),
            origin_detection: self.origin_detection,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

impl MemoryBounds for DogStatsDConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // We allocate our I/O buffers entirely up front.
            .with_fixed_amount(self.buffer_count * self.buffer_size)
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount(DEFAULT_CONTEXT_INTERNER_SIZE_BYTES.get());
    }
}

struct HandlerContext {
    shutdown_handle: DynamicShutdownHandle,
    listen_addr: String,
    origin_detection: bool,
    deserializer: Deserializer<DogStatsDMultiFraming, FixedSizeBufferPool<BytesBuffer>>,
}

struct ListenerContext {
    shutdown_handle: DynamicShutdownHandle,
    listener: Listener,
    io_buffer_pool: FixedSizeBufferPool<BytesBuffer>,
    context_resolver: ContextResolver,
    origin_detection: bool,
}

pub struct DogStatsD {
    listeners: Vec<Listener>,
    io_buffer_pool: FixedSizeBufferPool<BytesBuffer>,
    context_resolver: ContextResolver,
    origin_detection: bool,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
            let listener_context = ListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
                io_buffer_pool: self.io_buffer_pool.clone(),
                context_resolver: self.context_resolver.clone(),
                origin_detection: self.origin_detection,
            };

            tokio::spawn(process_listener(context.clone(), listener_context));
        }

        info!("DogStatsD source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        global_shutdown.await;
        info!("Stopping DogStatsd source...");

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
        context_resolver,
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
                        shutdown_handle: stream_shutdown_coordinator.register(),
                        listen_addr: listen_addr.to_string(),
                        origin_detection,
                        deserializer: DeserializerBuilder::new()
                            .with_framer_and_decoder(get_framer(&listen_addr), DogstatsdCodec::from_context_resolver(context_resolver.clone()))
                            .with_buffer_pool(io_buffer_pool.clone())
                            .with_metrics_builder(MetricsBuilder::from_component_context(source_context.component_context()))
                            .into_deserializer(stream),
                    };
                    tokio::spawn(process_stream(source_context.clone(), handler_context));
                }
                Err(e) => {
                    error!(%listen_addr, error = %e, "Failed to accept connection. Stopping listener.");
                    break
                }
            },
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!(%listen_addr, "DogStatsD listener stopped.");
}

async fn process_stream(source_context: SourceContext, handler_context: HandlerContext) {
    let HandlerContext {
        shutdown_handle,
        listen_addr,
        origin_detection,
        mut deserializer,
    } = handler_context;
    tokio::pin!(shutdown_handle);

    loop {
        let mut event_buffer = source_context.event_buffer_pool().acquire().await;
        select! {
            _ = &mut shutdown_handle => {
                debug!("Stream handler received shutdown signal.");
                break;
            },
            result = deserializer.decode(&mut event_buffer) => match result {
                // No events decoded. Connection is done.
                Ok((0, peer_addr)) => {
                    trace!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                    break;
                }
                // Got events. Forward them.
                Ok((n, peer_addr)) => {
                    // We do one optional enrichment step here, which is to add the client's socket credentials as a tag
                    // on each metric, if they came over UDS. This would then be utilized downstream in the pipeline by
                    // origin enrichment, if present.
                    if origin_detection {
                        if let ConnectionAddress::ProcessLike(Some(creds)) = &peer_addr {
                            for event in &mut event_buffer {
                                match event {
                                    Event::Metric(metric) => {
                                        if metric.metadata.origin_entity.is_none() {
                                            metric.metadata.origin_entity = Some(OriginEntity::ProcessId(creds.pid as u32));
                                        }
                                    },
                                }
                            }
                        }
                    }

                    trace!(%listen_addr, %peer_addr, events_len = n, "Forwarding events.");

                    if let Err(e) = source_context.forwarder().forward(event_buffer).await {
                        error!(%listen_addr, %peer_addr, error = %e, "Failed to forward events.");
                    }
                }
                Err(e) => match e {
                    DeserializerError::Io { source } => {
                        error!(%listen_addr, error = %source, "I/O error while decoding. Stopping stream.");
                        break;
                    }
                    other => error!(%listen_addr, error = %other, "Failed to decode events."),
                }
            }
        }
    }
}
