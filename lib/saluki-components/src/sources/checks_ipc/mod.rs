use std::sync::LazyLock;

use async_trait::async_trait;
use datadog_protos::checks::checks_server::{Checks, ChecksServer};
use datadog_protos::checks::{SendCheckPayloadRequest, SendCheckPayloadResponse};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::components::{sources::*, ComponentContext};
use saluki_core::data_model::event::EventType;
use saluki_core::topology::OutputDefinition;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use tokio::select;
use tonic::transport::Server;
use tonic::{Response, Status};
use tracing::{debug, info};

const fn default_grpc_endpoint() -> ListenAddress {
    ListenAddress::any_tcp(5105)
}

/// Checks IPC source.
#[derive(Debug, Deserialize)]
pub struct ChecksIPCConfiguration {
    #[serde(default = "default_grpc_endpoint")]
    grpc_endpoint: ListenAddress,
}

impl ChecksIPCConfiguration {
    /// Creates a new `ChecksIPCConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SourceBuilder for ChecksIPCConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("metrics", EventType::Metric)]);

        &OUTPUTS
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

        let grpc_server = Server::builder().add_service(ChecksServer::new(ChecksService));

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

struct ChecksService;

#[async_trait]
impl Checks for ChecksService {
    async fn send_check_payload(
        &self, _request: tonic::Request<SendCheckPayloadRequest>,
    ) -> Result<Response<SendCheckPayloadResponse>, Status> {
        // command for testing locally:
        //
        // DD_DATA_PLANE_CHECKS_ENABLED=true make run-adp-standalone
        // grpcurl -plaintext -proto lib/protos/datadog/proto/checks/checks.proto localhost:5105 datadog.checks.Checks/SendCheckPayload

        info!("Received check payload.");
        Ok(Response::new(SendCheckPayloadResponse {}))
    }
}
