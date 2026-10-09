use axum::{response::IntoResponse, routing::get, Router};
use saluki_api::APIHandler;
use saluki_core::{
    accounting::MemoryBounds,
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        BuildContext,
    },
    data_model::event::EventType,
    runtime,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_io::net::{server::http::HttpServer, ListenAddress};
use tokio::{pin, select};
use tonic::async_trait;
use tracing::debug;

/// TODO: doc
pub struct DatadogTracesAPIHandler {}

/// TODO: doc
pub struct DatadogTracesConfiguration {
    receiver_endpoint: ListenAddress,
}

struct DatadogTraces {
    receiver_endpoint: ListenAddress,
}

impl DatadogTracesConfiguration {
    /// Creates a new `DatadogTracesConfiguration` from the resolved configuration.
    pub fn from_configuration(receiver_endpoint: ListenAddress) -> Self {
        Self { receiver_endpoint }
    }
}

#[async_trait]
impl SourceBuilder for DatadogTracesConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Trace)];
        OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(DatadogTraces {
            receiver_endpoint: self.receiver_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for DatadogTracesConfiguration {
    fn specify_bounds(&self, _builder: &mut saluki_core::accounting::MemoryBoundsBuilder) {
        // builder.minimum().with_single_value::<DatadogTraces>("datadog_traces");
    }
}

#[async_trait]
impl Source for DatadogTraces {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        pin! {
            let global_shutdown = context.take_shutdown_handle();
        }

        let api_handler = DatadogTracesAPIHandler::new();
        let http_server =
            HttpServer::from_listen_address(self.receiver_endpoint).add_routes(api_handler.generate_routes());
        runtime::nested_supervisor(http_server.into_supervisor()).spawn();

        health.mark_ready();

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
            }
        }
        Ok(())
    }
}

impl DatadogTracesAPIHandler {
    fn new() -> Self {
        Self {}
    }

    async fn services_handler() -> impl IntoResponse {
        "OK\n"
    }
}

impl APIHandler for DatadogTracesAPIHandler {
    type State = ();

    fn generate_initial_state(&self) -> Self::State {}

    fn generate_routes(&self) -> axum::Router<Self::State> {
        Router::new()
            .route("/services", get(Self::services_handler))
            .route("/v0.1/services", get(Self::services_handler))
            .route("/v0.2/services", get(Self::services_handler))
            .route("/v0.3/services", get(Self::services_handler))
            .route("/v0.4/services", get(Self::services_handler))
    }
}
