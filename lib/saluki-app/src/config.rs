//! Configuration API handler.

use http::StatusCode;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use saluki_config::GenericConfiguration;
use serde_json::Value;

/// State for the configuration API handler.
#[derive(Clone)]
pub struct ConfigState {
    config: GenericConfiguration,
}

/// An API handler for exposing the current configuration.
pub struct ConfigAPIHandler {
    state: ConfigState,
}

impl ConfigAPIHandler {
    /// Creates a new `ConfigAPIHandler`.
    pub fn new(config: GenericConfiguration) -> Self {
        Self {
            state: ConfigState { config },
        }
    }

    async fn config_handler(State(state): State<ConfigState>) -> impl IntoResponse {
        match state.config.as_typed::<Value>() {
            Ok(config) => (StatusCode::OK, serde_json::to_string(&config).unwrap()).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get configuration: {}", e),
            )
                .into_response(),
        }
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
