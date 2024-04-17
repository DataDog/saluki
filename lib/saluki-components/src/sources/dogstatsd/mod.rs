use std::sync::Arc;

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
    buf::{get_fixed_bytes_buffer_pool, ReadWriteIoBuffer},
    deser::{codec::DogstatsdCodec, Deserializer, DeserializerBuilder, DeserializerError},
    net::{addr::ListenAddress, listener::Listener},
};

mod framer;
use self::framer::{get_framer, DogStatsDMultiFraming};

/// DogStatsD source.
///
/// Accepts metrics over TCP, UDP, or Unix Domain Sockets in the StatsD/DogStatsD format.
///
/// ## Missing
///
/// - UDS origin detection support (no exposed way to also receive ancillary/out-of-band data in UDS receive calls)
pub struct DogStatsDConfiguration {
    listen_address: ListenAddress,
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

        Ok(Self { listen_address })
    }
}

#[async_trait]
impl SourceBuilder for DogStatsDConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, Box<dyn std::error::Error + Send + Sync>> {
        let listen_address = self.listen_address.clone();
        let listener = Listener::from_listen_address(self.listen_address.clone()).await?;

        Ok(Box::new(DogStatsD {
            listen_address,
            listener,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct DogStatsD {
    listen_address: ListenAddress,
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
        let local_addr: Arc<str> = Arc::from(self.listen_address.to_string());

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
                        let deserializer = DeserializerBuilder::new()
                            .with_framer_and_decoder(get_framer(&self.listen_address), DogstatsdCodec)
                            .with_buffer_pool(io_buffer_pool.clone())
                            .into_deserializer(stream);
                        let local_addr = Arc::clone(&local_addr);
                        tokio::spawn(process_stream(context.clone(), local_addr, handler_shutdown_handle, deserializer));
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

async fn process_stream<B>(
    context: SourceContext, local_addr: Arc<str>, shutdown_handle: DynamicShutdownHandle,
    mut deserializer: Deserializer<DogStatsDMultiFraming, B>,
) where
    B: BufferPool,
    B::Buffer: ReadWriteIoBuffer,
{
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
                    trace!(%local_addr, peer_addr = %addr, events_len = n, "Forwarding events.");

                    if let Err(e) = context.forwarder().forward(event_buffer).await {
                        error!(error = %e, "Failed to forward events.");
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
