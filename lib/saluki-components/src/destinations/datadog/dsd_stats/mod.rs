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
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::EventType,
};
use saluki_error::GenericError;
use serde::Deserialize;
use tracing::{info, debug};

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
pub struct DogStatsDStats {
    state: Arc<Mutex<DogStatsDHandlerState>>,
}

impl DogStatsDStats {
    pub fn new(state: Arc<Mutex<DogStatsDHandlerState>>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl Destination for DogStatsDStats {
    async fn run(
        mut self: Box<Self>, mut context: DestinationContext,
    ) -> Result<(), GenericError> {
        // Process incoming metrics and update statistics
        loop {
            match context.events().next().await {
                Some(events) => {
                    // TODO: Process metrics and update state
                    // For now, just log that we received events and update state
                    debug!("DogStatsD stats destination received {} events", events.len());
                    
                    // Update the shared state (placeholder for actual statistics collection)
                    if let Ok(_state) = self.state.lock() {
                        // TODO: Process events and update statistics in state
                    }
                }
                None => break,
            }
        }
        
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
        // Create shared state that will be used by both the destination and API handler
        let shared_state = Arc::new(Mutex::new(DogStatsDHandlerState::default()));
        DogStatsDAPIHandler::from_state(shared_state)
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDStatisticsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(
        &self, _context: ComponentContext,
    ) -> Result<Box<dyn Destination + Send>, GenericError> {
        // Create shared state that will be used by both the destination and API handler
        let shared_state = Arc::new(Mutex::new(DogStatsDHandlerState::default()));
        Ok(Box::new(DogStatsDStats::new(shared_state)))
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
