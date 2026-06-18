//! Configuration API handler.
//!
//! Serves the `/config` compatibility views over the privileged API. The worker is source-agnostic:
//! it holds a clone-able **view producer** -- a closure that returns the two already-scrubbed,
//! already-serialized view strings (`raw`, `internal`) on each call -- rather than any configuration
//! map. This keeps `saluki-app` free of any raw-configuration type while still serving live-on-request
//! views: the producer regenerates from the configuration system's current retained snapshot every
//! time a route is hit.

use std::sync::Arc;

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

/// A clone-able producer of the `/config` views.
///
/// Returns the `(raw, internal)` pair of already-scrubbed, already-serialized view strings. It is
/// invoked once per request so the served views reflect the latest accepted configuration.
pub type ConfigViewProducer = Arc<dyn Fn() -> ConfigViewStrings + Send + Sync>;

/// The pair of serialized `/config` view strings.
#[derive(Clone)]
pub struct ConfigViewStrings {
    /// The source-shaped view, served at `/config` and `/config/raw`.
    pub raw: String,

    /// The ADP-native (internal) view, served at `/config/internal`.
    pub internal: String,
}

/// State used for the config API handler.
#[derive(Clone)]
pub struct ConfigState {
    producer: ConfigViewProducer,
}

/// An API handler for returning the current configuration.
///
/// Exposes three routes -- `/config`, `/config/raw`, and `/config/internal`. `/config` is a
/// compatibility alias for `/config/raw` (the source-shaped, scrubbed effective configuration);
/// `/config/internal` returns the ADP-native Saluki shape. The handler never exposes a raw
/// configuration map.
pub struct ConfigAPIHandler {
    state: ConfigState,
}

impl ConfigAPIHandler {
    fn new(producer: ConfigViewProducer) -> Self {
        Self {
            state: ConfigState { producer },
        }
    }

    async fn raw_handler(State(state): State<ConfigState>) -> impl IntoResponse {
        (StatusCode::OK, (state.producer)().raw).into_response()
    }

    async fn internal_handler(State(state): State<ConfigState>) -> impl IntoResponse {
        (StatusCode::OK, (state.producer)().internal).into_response()
    }
}

impl APIHandler for ConfigAPIHandler {
    type State = ConfigState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/config", get(Self::raw_handler))
            .route("/config/raw", get(Self::raw_handler))
            .route("/config/internal", get(Self::internal_handler))
    }
}

/// A worker for exposing an endpoint that returns the current configuration.
///
/// When running, the worker asserts a set of routes (based on [`ConfigAPIHandler`]) that allow querying the current
/// configuration. As the configuration may contain sensitive data, these routes are only present on the privileged API
/// endpoint.
pub struct ConfigWorker {
    handler: ConfigAPIHandler,
}

impl ConfigWorker {
    /// Creates a new [`ConfigWorker`] driven by the given view producer.
    ///
    /// The producer is invoked once per request, so the served views reflect the configuration
    /// system's latest accepted snapshot (live-on-request).
    pub fn new(producer: ConfigViewProducer) -> Self {
        Self {
            handler: ConfigAPIHandler::new(producer),
        }
    }
}

#[async_trait]
impl Supervisable for ConfigWorker {
    fn name(&self) -> &str {
        "config-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let config_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(config_route, "config-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}
