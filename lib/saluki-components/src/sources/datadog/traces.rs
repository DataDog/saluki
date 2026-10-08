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
use tonic::async_trait;

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
        &[]
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(DatadogTraces {
            receiver_endpoint: self.receiver_endpoint.clone(),
        }))
    }
}

impl MemoryBounds for DatadogTracesConfiguration {
    fn specify_bounds(&self, builder: &mut saluki_core::accounting::MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<DatadogTraces>("datadog_traces");
    }
}

#[async_trait]
impl Source for DatadogTraces {
    async fn run(self: Box<Self>, _context: SourceContext) -> Result<(), GenericError> {
        let api_handler = DatadogTracesAPIHandler::new();
        let http_server =
            HttpServer::from_listen_address(self.receiver_endpoint).add_routes(api_handler.generate_routes());
        runtime::nested_supervisor(http_server.into_supervisor()).spawn();
        Ok(())
    }
}

impl DatadogTracesAPIHandler {
    fn new() -> Self {
        Self {}
    }

    async fn services_handler() -> impl IntoResponse {
        "OK"
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
