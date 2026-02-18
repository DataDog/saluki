use std::net::SocketAddr;

use async_trait::async_trait;
use axum::Router;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_core::components::{sources::*, ComponentContext};
use saluki_core::data_model::event::EventType;
use saluki_core::topology::OutputDefinition;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::server::http::{ErrorHandle, ShutdownHandle};
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use tokio::runtime::Handle;
use tokio::select;
use tonic::transport::Server;
use tracing::debug;

/// Checks IPC source.
#[derive(Default)]
pub struct ChecksIPCConfiguration {
    grpc_endpoint: ListenAddress,
}

#[async_trait]
impl SourceBuilder for ChecksIPCConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::all_bits())];
        OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ChecksIPC {
            grpc_endpoint: self.grpc_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for ChecksIPCConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<ChecksIPC>("checks_ipc");
    }
}

struct ChecksIPC {
    grpc_endpoint: ListenAddress,
}

#[async_trait]
impl Source for ChecksIPC {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let server = ChecksServer::new(service);
        let grpc_server = Server::builder().add_service(ChecksServer::new(service));

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("OTLP gRPC endpoint must be a TCP address.")),
        };
        context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("checks-ipc-grpc-server", grpc_server.serve(grpc_socket_addr));

        health.mark_ready();
        debug!("Checks IPC source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
            }
        }

        debug!("Checks IPC source stopped.");
        Ok(())
    }
}
