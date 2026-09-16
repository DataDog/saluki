//! Configuration API handler.

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
use saluki_core::{
    diagnostic::DiagnosticsEmitter,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
    support::SubsystemIdentifier,
};
use saluki_error::{generic_error, GenericError};
use serde_json::Value;

/// Produces a fresh serialized configuration snapshot per call.
pub type ConfigSnapshotFn = Arc<dyn Fn() -> Result<Value, GenericError> + Send + Sync>;

/// State used for the config API handler.
#[derive(Clone)]
pub struct ConfigState {
    snapshot: ConfigSnapshotFn,
}

/// An API handler for returning the current configuration.
///
/// This handler exposes a single route -- `/config` -- that returns the current configuration in its serialized JSON
/// form. This allows determining exactly how the process' configuration looks based on the various providers being
/// used, including any dynamic changes being applied.
pub struct ConfigAPIHandler {
    state: ConfigState,
}

impl ConfigAPIHandler {
    fn new(snapshot: ConfigSnapshotFn) -> Self {
        Self {
            state: ConfigState { snapshot },
        }
    }

    async fn config_handler(State(state): State<ConfigState>) -> impl IntoResponse {
        match (state.snapshot)() {
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

/// A worker for exposing an endpoint that returns the current configuration.
///
/// When running, the worker asserts a set of routes (based on [`ConfigAPIHandler`]) that allow querying the current
/// configuration. As the configuration may contain sensitive data, these routes are only present on the privileged API
/// endpoint.
pub struct ConfigWorker {
    handler: ConfigAPIHandler,
}

impl ConfigWorker {
    /// Creates a new [`ConfigWorker`] that serves the snapshots produced by the given closure.
    pub fn new(snapshot: ConfigSnapshotFn) -> Self {
        Self {
            handler: ConfigAPIHandler::new(snapshot),
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

        let snapshot = self.handler.state.snapshot.clone();

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            dataspace.assert(config_route, "config-api");

            let diagnostics =
                DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments(["config-api"]), dataspace);
            diagnostics.register_collector("runtime_config_dump.yaml", move || {
                snapshot()
                    .map(|v| serde_json::to_vec_pretty(&v).unwrap_or_default())
                    .unwrap_or_default()
            });

            process_shutdown.await;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http_body_util::BodyExt as _;
    use saluki_error::generic_error;
    use serde_json::json;

    use super::*;

    async fn response_parts(handler: &ConfigAPIHandler) -> (StatusCode, String) {
        let response = ConfigAPIHandler::config_handler(State(handler.state.clone()))
            .await
            .into_response();
        let status = response.status();
        let body = response.into_body().collect().await.expect("body collects").to_bytes();

        (status, String::from_utf8(body.to_vec()).expect("body is UTF-8"))
    }

    #[tokio::test]
    async fn config_endpoint_serves_a_fresh_snapshot_per_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot_calls = Arc::clone(&calls);
        let handler = ConfigAPIHandler::new(Arc::new(move || {
            Ok(json!({ "revision": snapshot_calls.fetch_add(1, Ordering::Relaxed) }))
        }));

        let (status, body) = response_parts(&handler).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"revision":0}"#);

        let (status, body) = response_parts(&handler).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"revision":1}"#);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn config_endpoint_reports_a_failed_snapshot() {
        let handler = ConfigAPIHandler::new(Arc::new(|| Err(generic_error!("cannot serialize"))));

        let (status, body) = response_parts(&handler).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("cannot serialize"), "unexpected body: {body}");
    }
}
