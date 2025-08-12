use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics as OtlpResourceMetrics;
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::topology::interconnect::EventBufferManager;
use saluki_core::topology::shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle};
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::EventType,
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::{debug, error};

mod cache;
mod config;
mod translator;
use self::config::OtlpTranslatorConfig;
use self::translator::OtlpTranslator;

/// Configuration for the OTLP source.
#[derive(Deserialize)]
pub struct OtlpConfiguration {
    /// The port for the OTLP gRPC server.
    ///
    /// The gRPC server is responsible for accepting OTLP metrics payloads.
    ///
    /// Defaults to 4317.
    #[serde(rename = "otlp_port", default = "default_port")]
    port: u16,
}

fn default_port() -> u16 {
    4317
}

impl OtlpConfiguration {
    /// Creates a new `OTLPConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SourceBuilder for OtlpConfiguration {
    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("metrics", EventType::Metric)]);

        &OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let context_resolver = ContextResolverBuilder::from_name(format!("{}/otlp", context.component_id()))?.build();
        let translator_config = OtlpTranslatorConfig::default();

        Ok(Box::new(Otlp {
            context_resolver,
            port: self.port,
            translator_config,
        }))
    }
}

impl MemoryBounds for OtlpConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<Otlp>("source struct")
            .with_single_value::<GrpcService>("gRPC service");
    }
}

pub struct Otlp {
    context_resolver: ContextResolver,
    port: u16,
    translator_config: OtlpTranslatorConfig,
}

#[async_trait]
impl Source for Otlp {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        // Create the internal channel for decoupling the gRPC service from the converter.
        let (tx, rx) = mpsc::channel(1024);
        let mut converter_shutdown_coordinator = DynamicShutdownCoordinator::default();

        // Spawn the converter task.
        spawn_traced_named(
            "otlp-metric-converter",
            run_converter(
                rx,
                self.translator_config,
                self.context_resolver.clone(),
                context.clone(),
                converter_shutdown_coordinator.register(),
            ),
        );

        // Create and spawn the gRPC server.
        let service = GrpcService::new(tx);
        let service = MetricsServiceServer::new(service);
        let address = ListenAddress::Tcp(([0, 0, 0, 0], self.port).into());

        let socket_address = address
            .as_local_connect_addr()
            .ok_or_else(|| generic_error!("Failed to get local address to bind to OTLP gRPC server."))?;

        let server = Server::builder().add_service(service);
        spawn_traced_named("otlp-grpc-server", server.serve(socket_address));

        health.mark_ready();
        debug!("OTLP source started.");

        // Wait for the global shutdown signal, then notify converter to shutdown.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping OTLP source...");

        converter_shutdown_coordinator.shutdown().await;

        debug!("OTLP source stopped.");

        Ok(())
    }
}

struct GrpcService {
    sender: mpsc::Sender<OtlpResourceMetrics>,
}

impl GrpcService {
    fn new(sender: mpsc::Sender<OtlpResourceMetrics>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl MetricsService for GrpcService {
    async fn export(
        &self, request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let request = request.into_inner();

        for resource_metrics in request.resource_metrics {
            if self.sender.send(resource_metrics).await.is_err() {
                error!("Failed to send resource metrics to converter; channel is closed.");
                return Err(Status::internal("Internal processing channel closed."));
            }
        }

        Ok(Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

async fn dispatch_events(events: EventsBuffer, source_context: &SourceContext) {
    if events.is_empty() {
        return;
    }

    let len = events.len();
    if let Err(e) = source_context
        .dispatcher()
        .buffered_named("metrics")
        .expect("metrics output should always exist")
        .send_all(events)
        .await
    {
        error!(error = %e, "Failed to dispatch metric events.");
    } else {
        debug!(events_len = len, "Dispatched metric events.");
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpResourceMetrics>, config: OtlpTranslatorConfig, context_resolver: ContextResolver,
    source_context: SourceContext, shutdown_handle: DynamicShutdownHandle,
) {
    tokio::pin!(shutdown_handle);
    debug!("OTLP metric converter task started.");

    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();
    let memory_limiter = source_context.topology_context().memory_limiter();
    let mut translator = OtlpTranslator::new(config, context_resolver);

    loop {
        memory_limiter.wait_for_capacity().await;
        select! {
            Some(resource_metrics) = receiver.recv() => {
                match translator.map_metrics(resource_metrics) {
                    Ok(events) => {
                        for event in events {
                            if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                dispatch_events(event_buffer, &source_context).await;
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to handle resource metrics.");
                    }
                }
            },
            _ = buffer_flush.tick() => {
                if let Some(event_buffer) = event_buffer_manager.consume() {
                    dispatch_events(event_buffer, &source_context).await;
                }
            },
            _ = &mut shutdown_handle => {
                debug!("Converter task received shutdown signal.");
                break;
            }
        }
    }

    if let Some(event_buffer) = event_buffer_manager.consume() {
        dispatch_events(event_buffer, &source_context).await;
    }

    debug!("OTLP metric converter task stopped.");
}
