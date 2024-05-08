use std::sync::Arc;

use async_trait::async_trait;
use tokio::select;
use tracing::{debug, error, trace};

use saluki_core::{
    buffers::FixedSizeBufferPool,
    components::sources::*,
    constants::internal::ORIGIN_PID_TAG_KEY,
    topology::{
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_event::{DataType, Event};
use saluki_io::{
    buf::{get_fixed_bytes_buffer_pool, BytesBuffer},
    deser::{codec::DogstatsdCodec, Deserializer, DeserializerBuilder, DeserializerError},
    net::{
        addr::{ConnectionAddress, ListenAddress},
        listener::Listener,
        stream::Stream,
    },
};

mod framer;
use self::framer::{get_framer, DogStatsDMultiFraming};

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
pub struct DogStatsDConfiguration {
    listen_address: ListenAddress,
    origin_detection: bool,
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` with the given listen address.
    pub fn from_listen_address<L: Into<ListenAddress>>(
        listen_address: L,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listen_address = listen_address.into();
        if matches!(listen_address, ListenAddress::Tcp(_)) {
            return Err("TCP mode not supported for DogStatsD source".into());
        }

        Ok(Self {
            listen_address,
            origin_detection: false,
        })
    }

    pub fn with_origin_detection(mut self, origin_detection: bool) -> Self {
        self.origin_detection = origin_detection;
        self
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, Box<dyn std::error::Error + Send + Sync>> {
        let listen_address = self.listen_address.clone();
        let listener = Listener::from_listen_address(self.listen_address.clone()).await?;

        Ok(Box::new(DogStatsD {
            listener,
            listen_address_str: listen_address.to_string().into(),
            listen_address,
            shutdown_coordinator: DynamicShutdownCoordinator::default(),
            io_buffer_pool: get_fixed_bytes_buffer_pool(128, 8192),
            origin_detection: self.origin_detection,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

struct HandlerContext {
    shutdown_handle: DynamicShutdownHandle,
    listen_addr: Arc<str>,
    origin_detection: bool,
    deserializer: Deserializer<DogStatsDMultiFraming, FixedSizeBufferPool<BytesBuffer>>,
}

pub struct DogStatsD {
    listener: Listener,
    listen_address: ListenAddress,
    listen_address_str: Arc<str>,
    shutdown_coordinator: DynamicShutdownCoordinator,
    io_buffer_pool: FixedSizeBufferPool<BytesBuffer>,
    origin_detection: bool,
}

impl DogStatsD {
    fn create_handler_context(&mut self, stream: Stream) -> HandlerContext {
        HandlerContext {
            shutdown_handle: self.shutdown_coordinator.register(),
            listen_addr: Arc::clone(&self.listen_address_str),
            origin_detection: self.origin_detection,
            deserializer: DeserializerBuilder::new()
                .with_framer_and_decoder(get_framer(&self.listen_address), DogstatsdCodec)
                .with_buffer_pool(self.io_buffer_pool.clone())
                .into_deserializer(stream),
        }
    }
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");
        tokio::pin!(global_shutdown);

        debug!("DogStatsD source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal. Waiting for existing stream handlers to finish...");
                    break;
                }
                result = self.listener.accept() => match result {
                    Ok(stream) => {
                        debug!("Spawning new stream handler.");

                        let handler_context = self.create_handler_context(stream);
                        tokio::spawn(process_stream(context.clone(), handler_context));
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to accept new connection.");
                        return Err(());
                    }
                },
            }
        }

        self.shutdown_coordinator.shutdown();

        debug!("DogStatsD source stopped.");

        Ok(())
    }
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
                Ok((0, addr)) => {
                    debug!(%listen_addr, peer_addr = %addr, "Stream received EOF. Shutting down handler.");
                    break;
                }
                // Got events. Forward them.
                Ok((n, addr)) => {
                    // We do one optional enrichment step here, which is to add the client's socket credentials as a tag
                    // on each metric, if they came over UDS. This would then be utilized downstream in the pipeline by
                    // origin enrichment, if present.
                    if origin_detection {
                        if let ConnectionAddress::ProcessLike(Some(creds)) = &addr {
                            for event in &mut event_buffer {
                                match event {
                                    Event::Metric(metric) => {
                                        metric.context.tags.insert_tag((ORIGIN_PID_TAG_KEY, creds.pid.to_string()));
                                    },
                                }
                            }
                        }
                    }

                    trace!(%listen_addr, peer_addr = %addr, events_len = n, "Forwarding events.");

                    if let Err(e) = source_context.forwarder().forward(event_buffer).await {
                        error!(%listen_addr, error = %e, "Failed to forward events.");
                    }
                }
                Err(e) => match e {
                    DeserializerError::Io { source } => {
                        error!(error = %source, "I/O error while decoding. Closing stream.");
                        break;
                    }
                    other => error!(error = %other, "Failed to decode events."),
                }
            }
        }
    }
}
