use std::sync::Mutex;

use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use tracing::info;

static API_HANDLER: Mutex<Option<DogStatsDAPIHandler>> = Mutex::new(None);

/// Acquires the DogStatsD API handler.
pub fn acquire_dogstatsd_api_handler() -> Option<DogStatsDAPIHandler> {
    API_HANDLER.lock().unwrap().take()
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
    pub(crate) fn from_state(inner: Arc<Mutex<DogStatsDHandlerState>>) -> Self {
        Self {
            state: DogStatsDHandlerState { inner },
        }
    }

    async fn stats_handler(State(_state): State<DogStatsDHandlerState>) -> impl IntoResponse {
        // TODO: Implement actual statistics collection from DogStatsD source
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
