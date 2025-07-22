use std::sync::{Arc, Mutex};

use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use serde::Deserialize;
use tracing::info;

/// Configuration for DogStatsD internal statistics API.
#[derive(Deserialize)]
#[allow(dead_code)] // brianna: remove this later on
pub struct DogStatsDInternalStatisticsConfiguration {
    /// Whether to enable the internal statistics API.
    #[serde(rename = "dogstatsd_internal_statistics_enabled", default)]
    enabled: bool,
}

impl Default for DogStatsDInternalStatisticsConfiguration {
    fn default() -> Self {
        Self { enabled: false }
    }
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

impl DogStatsDInternalStatisticsConfiguration {
    /// Returns an API handler for DogStatsD internal statistics.
    pub fn api_handler(&self) -> DogStatsDAPIHandler {
        DogStatsDAPIHandler::from_state(Arc::new(Mutex::new(DogStatsDHandlerState::default())))
    }
}
