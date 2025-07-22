use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use memory_accounting::UsageExpr;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::destinations::{Destination, DestinationBuilder},
    data_model::event::EventType,
};
use saluki_error::GenericError;
use serde::Deserialize;
use tracing::info;

/// Configuration for DogStatsD internal statistics API.
#[derive(Deserialize)]
#[allow(dead_code)] // brianna: remove this later on
pub struct DogStatsDStatisticsConfiguration {
    /// Whether to enable the internal statistics API.
    #[serde(rename = "dogstatsd_internal_statistics_enabled", default)]
    enabled: bool,
}
/// State for the DogStatsD API handler.
#[derive(Clone, Default)]
pub struct DogStatsDHandlerState {
    // TODO: Add actual state fields for DogStatsD statistics
}

/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDHandlerState,
}

/// DogStatsD destination that collects internal statistics.
pub struct DogStatsDDestination;

impl DogStatsDDestination {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Destination for DogStatsDDestination {
    async fn run(
        self: Box<Self>, _context: saluki_core::components::destinations::DestinationContext,
    ) -> Result<(), saluki_error::GenericError> {
        // Collect statistics from incoming metrics and store them in the API handler state
        // TODO: actual implementation would process statistics
        Ok(())
    }
}

impl DogStatsDAPIHandler {
    pub fn from_state(state: Arc<Mutex<DogStatsDHandlerState>>) -> Self {
        Self {
            state: state.lock().unwrap().clone(),
        }
    }

    async fn stats_handler(State(_state): State<DogStatsDHandlerState>) -> impl IntoResponse {
        info!("DogStatsD stats requested");
        // TODO: This is where the stats will be processed and returned
    }
}

impl APIHandler for DogStatsDAPIHandler {
    type State = DogStatsDHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/dogstatsd/stats", get(Self::stats_handler))
    }
}

impl DogStatsDStatisticsConfiguration {
    /// Creates a new 'DogStatsDStatisticsConfiguration' from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Returns an API handler for DogStatsD internal statistics.
    pub fn api_handler(&self) -> DogStatsDAPIHandler {
        DogStatsDAPIHandler::from_state(Arc::new(Mutex::new(DogStatsDHandlerState::default())))
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDStatisticsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(
        &self, _context: saluki_core::components::ComponentContext,
    ) -> Result<Box<dyn saluki_core::components::destinations::Destination + Send>, saluki_error::GenericError> {
        Ok(Box::new(DogStatsDDestination::new()))
    }
}

impl MemoryBounds for DogStatsDStatisticsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDStatisticsConfiguration>("configuration struct");

        builder.firm().with_expr(UsageExpr::constant("api handler state", 64)); // 64 bytes as a placeholder until state is implemented
    }
}
