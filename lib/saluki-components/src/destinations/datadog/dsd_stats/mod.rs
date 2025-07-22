use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use saluki_config::GenericConfiguration;
use saluki_core::{components::destinations::DestinationBuilder, data_model::event::EventType};
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

impl DogStatsDAPIHandler {
    pub fn from_state(state: Arc<Mutex<DogStatsDHandlerState>>) -> Self {
        Self {
            state: state.lock().unwrap().clone(),
        }
    }

    async fn stats_handler(State(_state): State<DogStatsDHandlerState>) -> impl IntoResponse {
        info!("DogStatsD stats requested");
        "DogStatsD stats endpoint - TODO: implement actual statistics"
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
        // This is just a placeholder - the actual API handler is used in the control plane
        unimplemented!("DogStatsDStatisticsConfiguration is used for API handler, not as a destination")
    }
}

impl MemoryBounds for DogStatsDStatisticsConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {
        // No memory bounds needed for API handler configuration
    }
}
