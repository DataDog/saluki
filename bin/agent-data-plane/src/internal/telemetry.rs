use std::sync::Arc;

use async_trait::async_trait;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::{
    observability::metrics::{get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, TelemetryProcessor},
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
};
use saluki_error::generic_error;
use tokio::sync::Mutex;

use crate::state::metrics::{emitter_tag, get_compat_remappings};

/// State shared by the telemetry API routes.
#[derive(Clone)]
pub struct InternalTelemetryState {
    metrics: Reflector<AggregatedMetricsProcessor>,
    raw: Arc<Mutex<TelemetryProcessor>>,
    compat: Arc<Mutex<TelemetryProcessor>>,
}

/// API handler that exposes the internal telemetry routes (`/metrics`, `/compat/metrics`) on the
/// unprivileged endpoint.
pub struct InternalTelemetryAPIHandler {
    state: InternalTelemetryState,
}

impl InternalTelemetryAPIHandler {
    fn new(metrics: Reflector<AggregatedMetricsProcessor>) -> Self {
        Self {
            state: InternalTelemetryState {
                metrics,
                raw: Arc::new(Mutex::new(TelemetryProcessor::new())),
                compat: Arc::new(Mutex::new(
                    TelemetryProcessor::new()
                        .with_remapper_rules(get_compat_remappings())
                        .with_injected_tags([emitter_tag()]),
                )),
            },
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
        Router::new()
            .route("/metrics", get(Self::metrics_handler))
            .route("/compat/metrics", get(Self::compat_metrics_handler))
    }
}

/// A worker that exposes ADP's internal telemetry on the unprivileged API endpoint.
///
/// Asserts two routes on the unprivileged endpoint:
///
/// - `GET /metrics`: the full aggregated metrics state, rendered as Prometheus text exposition.
/// - `GET /compat/metrics`: the subset of metrics that match the Datadog Agent compatibility
///   remapper rules, rendered with the remapped names so they line up with Core Agent telemetry.
pub struct InternalTelemetryAPIWorker;

impl InternalTelemetryAPIWorker {
    /// Creates a new `InternalTelemetryAPIWorker`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Supervisable for InternalTelemetryAPIWorker {
    fn name(&self) -> &str {
        "internal-telemetry-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let metrics = get_shared_metrics_state().await;
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
