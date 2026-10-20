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

use super::{get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, RemapperRule, TelemetryProcessor};
use crate::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};

/// State shared by the telemetry API routes.
#[derive(Clone)]
pub struct InternalTelemetryState {
    metrics: Reflector<AggregatedMetricsProcessor>,
    raw: Arc<Mutex<TelemetryProcessor>>,
    compat: Arc<Mutex<TelemetryProcessor>>,
}

/// API handler that exposes the internal telemetry routes (`/metrics`, and optionally `/compat/metrics`) on the
/// unprivileged endpoint.
pub struct InternalTelemetryAPIHandler {
    state: InternalTelemetryState,
    compat_enabled: bool,
}

impl InternalTelemetryAPIHandler {
    fn new(metrics: Reflector<AggregatedMetricsProcessor>, compat_remapper_rules: Option<Vec<RemapperRule>>) -> Self {
        let compat_enabled = compat_remapper_rules.is_some();
        let compat_remapper_rules = compat_remapper_rules.unwrap_or_default();

        Self {
            state: InternalTelemetryState {
                metrics,
                raw: Arc::new(Mutex::new(TelemetryProcessor::new())),
                compat: Arc::new(Mutex::new(
                    TelemetryProcessor::new().with_remapper_rules(compat_remapper_rules),
                )),
            },
            compat_enabled,
        }
    }

    async fn metrics_handler(State(state): State<InternalTelemetryState>) -> impl IntoResponse {
        let mut processor = state.raw.lock().await;
        processor.process(state.metrics.state())
    }

    async fn compat_metrics_handler(State(state): State<InternalTelemetryState>) -> impl IntoResponse {
        let mut processor = state.compat.lock().await;
        processor.process(state.metrics.state())
    }
}

impl APIHandler for InternalTelemetryAPIHandler {
    type State = InternalTelemetryState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        let router = Router::new().route("/metrics", get(Self::metrics_handler));
        if self.compat_enabled {
            router.route("/compat/metrics", get(Self::compat_metrics_handler))
        } else {
            router
        }
    }
}

/// A worker that exposes the process's internal telemetry on the unprivileged API endpoint.
///
/// Asserts these routes on the unprivileged endpoint:
///
/// - `GET /metrics`: the full aggregated metrics state, rendered as Prometheus text exposition.
/// - `GET /compat/metrics`, when compatibility remapper rules are configured: the subset of metrics that match the
///   rules, rendered with their remapped names.
pub struct InternalTelemetryAPIWorker {
    compat_remapper_rules: Option<Vec<RemapperRule>>,
}

impl InternalTelemetryAPIWorker {
    /// Creates a new `InternalTelemetryAPIWorker`.
    ///
    /// When `compat_remapper_rules` is `Some`, the worker also serves `/compat/metrics` with those rules.
    pub fn new(compat_remapper_rules: Option<Vec<RemapperRule>>) -> Self {
        Self { compat_remapper_rules }
    }
}

#[async_trait]
impl Supervisable for InternalTelemetryAPIWorker {
    fn name(&self) -> &str {
        "internal-telemetry-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let metrics = get_shared_metrics_state();
        let handler = InternalTelemetryAPIHandler::new(metrics, self.compat_remapper_rules.clone());
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

    fn handler(compat_remapper_rules: Option<Vec<RemapperRule>>) -> InternalTelemetryAPIHandler {
        let (metrics, _worker) = Reflector::new(stream::empty::<Vec<MetricsSnapshot>>(), AggregatedMetricsProcessor);
        InternalTelemetryAPIHandler::new(metrics, compat_remapper_rules)
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
    async fn compat_route_is_only_served_with_compat_rules() {
        let without_compat = handler(None);
        assert_eq!(status(&without_compat, "/metrics").await, StatusCode::OK);
        assert_eq!(status(&without_compat, "/compat/metrics").await, StatusCode::NOT_FOUND);

        let with_compat = handler(Some(Vec::new()));
        assert_eq!(status(&with_compat, "/metrics").await, StatusCode::OK);
        assert_eq!(status(&with_compat, "/compat/metrics").await, StatusCode::OK);
    }
}
