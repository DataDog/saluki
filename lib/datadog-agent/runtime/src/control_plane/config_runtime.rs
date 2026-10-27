//! Runtime configuration API handler.

use std::sync::Arc;

use async_trait::async_trait;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType, StatusCode,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;
use serde::Serialize;

/// Produces the current runtime configuration in its serialized JSON form.
#[derive(Clone)]
pub struct RuntimeConfigSnapshot {
    serialize: Arc<dyn Fn() -> Result<String, serde_json::Error> + Send + Sync>,
}

impl RuntimeConfigSnapshot {
    /// Creates a `RuntimeConfigSnapshot` that serializes the configuration `load` returns at the time of each request.
    pub fn from_loader<T, F>(load: F) -> Self
    where
        T: Serialize + 'static,
        F: Fn() -> Arc<T> + Send + Sync + 'static,
    {
        Self {
            serialize: Arc::new(move || serde_json::to_string(&*load())),
        }
    }

    fn to_json(&self) -> Result<String, serde_json::Error> {
        (self.serialize)()
    }
}

/// State used for the runtime configuration API handler.
#[derive(Clone)]
pub struct ConfigRuntimeState {
    snapshot: RuntimeConfigSnapshot,
}

/// An API handler for returning the translated runtime configuration.
///
/// This handler exposes a single route -- `/config/runtime` -- that returns the current
/// translated configuration in its serialized JSON form. It reflects any dynamic updates
/// applied to the configuration since startup.
pub struct ConfigRuntimeAPIHandler {
    state: ConfigRuntimeState,
}

impl ConfigRuntimeAPIHandler {
    fn new(snapshot: RuntimeConfigSnapshot) -> Self {
        Self {
            state: ConfigRuntimeState { snapshot },
        }
    }

    async fn config_handler(State(state): State<ConfigRuntimeState>) -> impl IntoResponse {
        match state.snapshot.to_json() {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize configuration: {}", e),
            )
                .into_response(),
        }
    }
}

impl APIHandler for ConfigRuntimeAPIHandler {
    type State = ConfigRuntimeState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/config/runtime", get(Self::config_handler))
    }
}

/// A worker for exposing an endpoint that returns the translated runtime configuration.
///
/// When running, the worker asserts a route (based on `ConfigRuntimeAPIHandler`) that returns
/// the current translated configuration. As the configuration may contain sensitive data, this
/// route is only present on the privileged API endpoint.
pub struct ConfigRuntimeWorker {
    handler: ConfigRuntimeAPIHandler,
}

impl ConfigRuntimeWorker {
    /// Creates a new [`ConfigRuntimeWorker`] serving the given configuration.
    pub fn new(snapshot: RuntimeConfigSnapshot) -> Self {
        Self {
            handler: ConfigRuntimeAPIHandler::new(snapshot),
        }
    }
}

#[async_trait]
impl Supervisable for ConfigRuntimeWorker {
    fn name(&self) -> &str {
        "config-runtime-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let config_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            dataspace.assert(config_route, "config-runtime-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;

    use super::*;

    #[derive(Serialize)]
    struct ExampleConfig {
        log_level: String,
    }

    #[test]
    fn snapshot_serializes_the_configuration_its_loader_returns_at_each_request() {
        let current = Arc::new(ArcSwap::from_pointee(ExampleConfig {
            log_level: "info".to_string(),
        }));
        let loaded = Arc::clone(&current);
        let snapshot = RuntimeConfigSnapshot::from_loader(move || loaded.load_full());
        assert_eq!(snapshot.to_json().unwrap(), r#"{"log_level":"info"}"#);

        current.store(Arc::new(ExampleConfig {
            log_level: "debug".to_string(),
        }));
        assert_eq!(snapshot.to_json().unwrap(), r#"{"log_level":"debug"}"#);
    }
}
