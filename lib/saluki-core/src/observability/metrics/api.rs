//! Internal telemetry API handler.

use std::sync::Arc;

use async_trait::async_trait;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::generic_error;
use tokio::sync::Mutex;

use super::{get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, TelemetryProcessor};
use crate::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};

/// State shared by the telemetry API route.
#[derive(Clone)]
pub struct InternalTelemetryState {
    metrics: Reflector<AggregatedMetricsProcessor>,
    processor: Arc<Mutex<TelemetryProcessor>>,
}

/// API handler that exposes `/metrics` on the unprivileged endpoint.
pub struct InternalTelemetryAPIHandler {
    state: InternalTelemetryState,
}

impl InternalTelemetryAPIHandler {
    fn new(metrics: Reflector<AggregatedMetricsProcessor>) -> Self {
        Self {
            state: InternalTelemetryState {
                metrics,
                processor: Arc::new(Mutex::new(TelemetryProcessor::new())),
            },
        }
    }

    async fn metrics_handler(State(state): State<InternalTelemetryState>) -> impl IntoResponse {
        let mut processor = state.processor.lock().await;
        processor.process(state.metrics.state())
    }
}

impl APIHandler for InternalTelemetryAPIHandler {
    type State = InternalTelemetryState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/metrics", get(Self::metrics_handler))
    }
}

/// A worker that exposes the process's internal telemetry on the unprivileged API endpoint.
///
/// Asserts `GET /metrics`, serving the full aggregated metrics state as Prometheus text exposition.
pub struct InternalTelemetryAPIWorker;

#[async_trait]
impl Supervisable for InternalTelemetryAPIWorker {
    fn name(&self) -> &str {
        "internal-telemetry-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let metrics = get_shared_metrics_state();
        let handler = InternalTelemetryAPIHandler::new(metrics);
        let route = DynamicRoute::http(EndpointType::Unprivileged, &handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(route, "internal-telemetry-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use futures::stream;
    use http::{Request, StatusCode};
    use http_body_util::Empty;
    use hyper::body::Bytes;
    use tower::ServiceExt as _;

    use super::*;
    use crate::observability::metrics::MetricsSnapshot;

    fn handler() -> InternalTelemetryAPIHandler {
        let (metrics, _worker) = Reflector::new(stream::empty::<Vec<MetricsSnapshot>>(), AggregatedMetricsProcessor);
        InternalTelemetryAPIHandler::new(metrics)
    }

    async fn status(handler: &InternalTelemetryAPIHandler, path: &str) -> StatusCode {
        let request = Request::get(path).body(Empty::<Bytes>::new()).unwrap();
        handler
            .generate_routes()
            .with_state(handler.generate_initial_state())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn serves_only_metrics_route() {
        let handler = handler();
        assert_eq!(status(&handler, "/metrics").await, StatusCode::OK);
        assert_eq!(status(&handler, "/compat/metrics").await, StatusCode::NOT_FOUND);
    }
}
