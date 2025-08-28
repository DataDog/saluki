//! Configuration API handler.

use std::sync::Arc;

use arc_swap::ArcSwap;
use http::StatusCode;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use serde_json::Value;

/// State for the configuration API handler.
#[derive(Clone)]
pub struct ConfigState {
    values: Arc<ArcSwap<Value>>,
}

/// An API handler for exposing the current configuration.
pub struct ConfigAPIHandler {
    state: ConfigState,
}

impl ConfigAPIHandler {
    /// Creates a new `ConfigAPIHandler`.
    pub fn from_state(values: Arc<ArcSwap<Value>>) -> Self {
        Self {
            state: ConfigState { values },
        }
    }

    async fn config_handler(State(state): State<ConfigState>) -> impl IntoResponse {
        let config = state.values.load();
        (StatusCode::OK, config.to_string())
    }
}

impl APIHandler for ConfigAPIHandler {
    type State = ConfigState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/config", get(Self::config_handler))
    }
}
