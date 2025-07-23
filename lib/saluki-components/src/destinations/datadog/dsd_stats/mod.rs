use std::sync::{Arc, Mutex, RwLock};

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
use tracing::{debug, info};

/// Configuration for DogStatsD internal statistics API.
#[derive(Deserialize)]
#[allow(dead_code)] // brianna: remove this later on
pub struct DogStatsDStatisticsConfiguration {
    /// Whether to enable the internal statistics API.
    #[serde(rename = "dogstatsd_internal_statistics_enabled", default)]
    enabled: bool,

    /// Shared state for statistics (created lazily)
    #[serde(skip)]
    shared_state: Arc<RwLock<Option<Arc<Mutex<DogStatsDAPIHandlerState>>>>>,
}
/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDAPIHandlerState,
}
/// State for the DogStatsD API handler.
#[derive(Clone, Default)]
pub struct DogStatsDAPIHandlerState {
    // TODO: Add actual state fields for DogStatsD statistics
    metrics_received: u64,
}

/// DogStatsD destination that collects internal statistics.
pub struct DogStatsDStats {
    state: Arc<Mutex<DogStatsDAPIHandlerState>>,
}

impl DogStatsDStats {
    pub fn new(state: Arc<Mutex<DogStatsDAPIHandlerState>>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl Destination for DogStatsDStats {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        // Process incoming metrics and update statistics
        while let Some(events) = context.events().next().await {
            // TODO: use switch statement so either receives events or request from API handler
            // TODO: Process metrics and update state
            // For now, just log that we received events and update state
            debug!("DogStatsD stats destination received {} events", events.len());

            // Update the shared state (placeholder for actual statistics collection)
            if let Ok(mut state) = self.state.lock() {
                state.metrics_received += events.len() as u64;
                // TODO: Process events and update statistics in state
            }
        }

        Ok(())
    }
}

impl DogStatsDAPIHandler {
    pub fn from_state(state: Arc<Mutex<DogStatsDAPIHandlerState>>) -> Self {
        Self {
            state: state.lock().unwrap().clone(),
        }
    }

    async fn stats_handler(State(state): State<DogStatsDAPIHandlerState>) -> impl IntoResponse {
        info!("DogStatsD stats requested");
        format!("{{\"metrics_received\": {}}}", state.metrics_received)
    }
}

impl APIHandler for DogStatsDAPIHandler {
    type State = DogStatsDAPIHandlerState;

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
        let mut typed_config: Self = config.as_typed()?;
        typed_config.shared_state = Arc::new(RwLock::new(None));
        Ok(typed_config)
    }

    /// Returns an API handler for DogStatsD statistics.
    pub fn api_handler(&self) -> DogStatsDAPIHandler {
        let shared_state = self.get_or_create_shared_state();
        DogStatsDAPIHandler::from_state(shared_state)
    }

    /// Gets the shared state if it exists.
    fn get_shared_state(&self) -> Option<Arc<Mutex<DogStatsDAPIHandlerState>>> {
        self.shared_state.read().unwrap().clone()
    }

    /// Creates a new shared state and stores it in the configuration.
    fn create_shared_state(&self) -> Arc<Mutex<DogStatsDAPIHandlerState>> {
        let state = Arc::new(Mutex::new(DogStatsDAPIHandlerState::default()));
        *self.shared_state.write().unwrap() = Some(state.clone());
        state
    }

    /// Gets existing shared state or creates and stores a new one.
    fn get_or_create_shared_state(&self) -> Arc<Mutex<DogStatsDAPIHandlerState>> {
        self.get_shared_state().unwrap_or_else(|| self.create_shared_state())
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDStatisticsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let shared_state = self.get_or_create_shared_state();
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
