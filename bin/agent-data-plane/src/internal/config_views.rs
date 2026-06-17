use agent_data_plane_config::ConfigViews;
use async_trait::async_trait;
use http::StatusCode;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;

/// HTTP handler for typed configuration views.
pub struct ConfigViewsAPIHandler {
    views: ConfigViews,
}

impl ConfigViewsAPIHandler {
    /// Creates a new handler from typed config views.
    pub fn new(views: ConfigViews) -> Self {
        Self { views }
    }

    async fn config_handler(State(views): State<ConfigViews>) -> impl IntoResponse {
        json_response(views.raw.as_json())
    }

    async fn raw_config_handler(State(views): State<ConfigViews>) -> impl IntoResponse {
        json_response(views.raw.as_json())
    }

    async fn internal_config_handler(State(views): State<ConfigViews>) -> impl IntoResponse {
        json_response(views.internal.as_json())
    }
}

impl APIHandler for ConfigViewsAPIHandler {
    type State = ConfigViews;

    fn generate_initial_state(&self) -> Self::State {
        self.views.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/config", get(Self::config_handler))
            .route("/config/raw", get(Self::raw_config_handler))
            .route("/config/internal", get(Self::internal_config_handler))
    }
}

/// Worker that exposes typed configuration views on the privileged API.
pub struct ConfigViewsWorker {
    handler: ConfigViewsAPIHandler,
}

impl ConfigViewsWorker {
    /// Creates a new config view worker.
    pub fn new(views: ConfigViews) -> Self {
        Self {
            handler: ConfigViewsAPIHandler::new(views),
        }
    }
}

#[async_trait]
impl Supervisable for ConfigViewsWorker {
    fn name(&self) -> &str {
        "config-views-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let config_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(config_route, "config-views-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}

fn json_response(value: serde_json::Value) -> impl IntoResponse {
    match serde_json::to_string(&value) {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to serialize configuration view: {error}"),
        )
            .into_response(),
    }
}
