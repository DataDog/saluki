use async_trait::async_trait;
use tokio::select;
use tracing::{debug, error, trace};

use saluki_core::{
    buffers::BufferPool,
    components::sources::*,
    topology::{
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_event::DataType;
use saluki_io::{
    buf::{get_fixed_bytes_buffer_pool, IoBuffer},
    deser::{codec::DogstatsdCodec, framing::NewlineFramer, DeserializerBuilder},
    net::{addr::ListenAddress, listener::Listener, stream::Stream},
};

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
///
/// ## Missing
///
/// - multi-value support
/// - UDS origin detection support (no exposed way to also receive ancillary/out-of-band data in UDS receive calls)
pub struct DogStatsDConfiguration {
    listen_address: ListenAddress,
}

impl DogStatsDConfiguration {
    /// Creates a new `DogStatsDConfiguration` with the given listen address.
    pub fn from_listen_address<L: Into<ListenAddress>>(listen_address: L) -> Self {
        Self {
            listen_address: listen_address.into(),
        }
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, Box<dyn std::error::Error + Send + Sync>> {
        let listener = Listener::from_listen_address(self.listen_address.clone()).await?;

        Ok(Box::new(DogStatsD { listener }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct DogStatsD {
    listener: Listener,
}

#[async_trait]
impl Source for DogStatsD {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let io_buffer_pool = get_fixed_bytes_buffer_pool(128, 8192);
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");
        tokio::pin!(global_shutdown);

        let mut handler_shutdown_coordinator = DynamicShutdownCoordinator::default();

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

                        let handler_shutdown_handle = handler_shutdown_coordinator.register();
                        let handler = StreamHandler::new(context.clone(), handler_shutdown_handle, stream, io_buffer_pool.clone());
                        tokio::spawn(handler.process());
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to accept new connection.");
                        return Err(());
                    }
                },
            }
        }

        handler_shutdown_coordinator.shutdown();

        debug!("DogStatsD source stopped.");

        Ok(())
    }
}

struct StreamHandler<B> {
    context: SourceContext,
    shutdown_handle: DynamicShutdownHandle,
    stream: Stream,
    buffer_pool: B,
}

impl<B> StreamHandler<B>
where
    B: BufferPool,
    B::Buffer: IoBuffer,
{
    fn new(context: SourceContext, shutdown_handle: DynamicShutdownHandle, stream: Stream, buffer_pool: B) -> Self {
        Self {
            context,
            shutdown_handle,
            stream,
            buffer_pool,
        }
    }

    async fn process(self) {
        let Self {
            context,
            shutdown_handle,
            stream,
            buffer_pool,
        } = self;

        let mut deserializer = DeserializerBuilder::new()
            .with_framer_and_decoder(NewlineFramer::default().required_on_eof(false), DogstatsdCodec)
            .with_buffer_pool(buffer_pool)
            .into_deserializer(stream);

        tokio::pin!(shutdown_handle);

        loop {
            let mut event_buffer = context.event_buffer_pool().acquire().await;
            select! {
                _ = &mut shutdown_handle => {
                    debug!("Stream handler received shutdown signal.");
                    break;
                },
                result = deserializer.decode(&mut event_buffer) => match result {
                    // No events decoded. Connection is done.
                    Ok((0, addr)) => {
                        debug!(peer_addr = %addr, "Stream received EOF. Shutting down handler.");
                        break;
                    }
                    // Got events. Forward them.
                    Ok((n, addr)) => {
                        trace!(peer_addr = %addr, events_len = n, "Forwarding events.");

                        if let Err(e) = context.forwarder().forward(event_buffer).await {
                            error!(error = %e, "Failed to forward events.");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to decode events.");
                        break;
                    }
                }
            }
        }
    }
}
