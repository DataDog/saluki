use std::sync::LazyLock;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{components::sources::*, topology::OutputDefinition};
use saluki_error::GenericError;
use saluki_event::DataType;
use saluki_io::net::{listener::ConnectionOrientedListener, GrpcListenAddress, ListenAddress};
use serde::Deserialize;
use tracing::info;

fn default_listen_addresses() -> Vec<GrpcListenAddress> {
    vec![GrpcListenAddress::Binary(
        ListenAddress::Tcp(([127, 0, 0, 1], 8080).into()),
    )]
}

/// OpenTelemetry source.
///
/// Accepts OpenTelemetry data (OTLP) over gRPC.
#[derive(Deserialize)]
pub struct OpenTelemetryConfiguration {
    /// Addresses to listen on.
    ///
    /// Multiple addresses can be specified to create multiple listeners, where the mode (HTTP vs gRPC) is determined by
    /// the address scheme. When HTTP/HTTPS is used, the listener will be a OTLP/HTTP-compatible endpoint. For TCP and
    /// UDS, the listener will be a OTLP/gRPC-compatible endpoint.
    ///
    /// Defaults to `grpc://127.0.0.1:4317`.
    #[serde(default = "default_listen_addresses")]
    listen_addresses: Vec<GrpcListenAddress>,
}

impl OpenTelemetryConfiguration {
    /// Creates a new `OpenTelemetryConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SourceBuilder for OpenTelemetryConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        let mut listeners = Vec::with_capacity(self.listen_addresses.len());
        for listen_address in &self.listen_addresses {
            let (service_type, address) = match listen_address {
                GrpcListenAddress::Binary(addr) => ("grpc", addr),
                GrpcListenAddress::Web(addr) => ("http", addr),
            };

            let listener = ConnectionOrientedListener::from_listen_address(address.clone()).await?;
            listeners.push((service_type, listener));
        }

        Ok(Box::new(OpenTelemetry { listeners }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("logs", DataType::Log),
                OutputDefinition::named_output("metrics", DataType::Metric),
                OutputDefinition::named_output("traces", DataType::Trace),
            ]
        });

        &OUTPUTS
    }
}

impl MemoryBounds for OpenTelemetryConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {}
}

pub struct OpenTelemetry {
    listeners: Vec<(&'static str, ConnectionOrientedListener)>,
}

#[async_trait]
impl Source for OpenTelemetry {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");

        info!("OpenTelemetry source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        global_shutdown.await;
        info!("Stopping OpenTelemetry source...");

        info!("OpenTelemetry source stopped.");

        Ok(())
    }
}
