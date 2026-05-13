use axum::{
    routing::{get, post},
    Router,
};

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::ApmTelemetryState;

/// Builds the agent telemetry router.
///
/// Routes:
/// - `POST /api/v2/apmtelemetry` — receives agent telemetry payloads from the Datadog Agent's
///   `agenttelemetry` component and stores them for later analysis.
/// - `GET /agent-telemetry/dump` — returns all collected payloads as a JSON array.
pub fn build_agent_telemetry_router() -> Router {
    let state = ApmTelemetryState::new();
    Router::new()
        .route("/api/v2/apmtelemetry", post(handle_apmtelemetry))
        .route("/agent-telemetry/dump", get(handle_agent_telemetry_dump))
        .with_state(state)
}
