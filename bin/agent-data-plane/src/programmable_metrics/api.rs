//! The programmable-metrics control surface, on the privileged API (mutual TLS with the IPC certificate).
//!
//! | Route | Body |
//! |---|---|
//! | `GET /programmable-metrics/status` | the status summary and `config_states`, as the stub protocol writes them |
//! | `GET /programmable-metrics/resources` | the delivery hosts, outputs, recipients, and resource budget |
//! | `GET /programmable-metrics/export` | the export bundle of the default program; `?form=bound` for its bound form |
//! | `GET /programmable-metrics/plan` | the active composed logical plan, resolved values included |
//! | `GET /programmable-metrics/default-program` | the default program last lowered, resolved values included |
//! | `GET /programmable-metrics/explain` | the explain document of the active plan, as JSON; `?format=text` for text |
//!
//! The contract is specified in `docs/design/programmable-metrics-stub-protocol.md`, section 3; the demo server reads
//! it. Every body is read from the installer's published [`PlanStatus`], never from the installer itself, so a request
//! never waits on an installation.
//!
//! The status route's body also lists, as `recipients`, the delivery hosts' recipient incarnations: those retired
//! lately, each with its drain report as `last_drain` (see [`StatusDocument`]). A poll's status does not carry them.
//! The resources route says what the default program delivers through: the outputs and recipients the configuration
//! declares, credentials by reference only.
//!
//! A request the routes cannot answer gets a plain-text error: `400 Bad Request` for a missing or unknown query
//! parameter value, `500 Internal Server Error` for a default program that cannot be exported, and `503 Service
//! Unavailable` while no generation is active.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use http::{header::CONTENT_TYPE, StatusCode};
use saluki_api::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;
use serde::{Deserialize, Serialize};

use super::installer::{status::StatusDocument, PlanStatus};

/// The control surface's routes.
pub mod routes {
    /// The status summary, every config's acknowledgment, and the recipient incarnations.
    pub const STATUS: &str = "/programmable-metrics/status";

    /// What the default program delivers through: hosts, outputs, recipients, and the resource budget.
    pub const RESOURCES: &str = "/programmable-metrics/resources";

    /// The export bundle of the default program, in the form the `form` query parameter names.
    pub const EXPORT: &str = "/programmable-metrics/export";

    /// The active composed logical plan.
    pub const PLAN: &str = "/programmable-metrics/plan";

    /// The default program last lowered: the plan whose sources, outputs, and resources artifacts are composed with.
    pub const DEFAULT_PROGRAM: &str = "/programmable-metrics/default-program";

    /// The explain document of the active plan; `format=text` for its text form.
    pub const EXPLAIN: &str = "/programmable-metrics/explain";
}

/// What the control surface's routes read.
#[derive(Clone)]
pub struct ControlSurfaceState {
    status: Arc<ArcSwap<PlanStatus>>,
}

/// The query parameters of the export route.
#[derive(Deserialize)]
struct ExportQuery {
    /// The form to export it in: `detached`, the default, which leaves out what does nothing as configured and
    /// addresses emits to recipients, as a published program starts from, or `bound`, every node as the default program
    /// has it.
    form: Option<String>,
}

/// The query parameters of the explain route.
#[derive(Deserialize)]
struct ExplainQuery {
    /// `json`, the default, or `text`.
    format: Option<String>,
}

/// The API handler of the control surface.
pub struct ControlSurfaceAPIHandler {
    state: ControlSurfaceState,
}

impl ControlSurfaceAPIHandler {
    /// Creates the handler over the installer's published status.
    pub fn new(status: Arc<ArcSwap<PlanStatus>>) -> Self {
        Self {
            state: ControlSurfaceState { status },
        }
    }

    async fn status_handler(State(state): State<ControlSurfaceState>) -> Response {
        json(&StatusDocument::of(&state.status.load()))
    }

    async fn resources_handler(State(state): State<ControlSurfaceState>) -> Response {
        json(&state.status.load().resources)
    }

    async fn export_handler(State(state): State<ControlSurfaceState>, Query(query): Query<ExportQuery>) -> Response {
        let status = state.status.load();
        let export = match query.form.as_deref() {
            None | Some("detached") => &status.export,
            Some("bound") => &status.bound_export,
            Some(other) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "The `form` query parameter is `detached`, the default, or `bound`, not `{}`.",
                        other
                    ),
                )
            }
        };
        match export {
            Ok(bundle) => json(&**bundle),
            Err(reason) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The default program cannot be exported: {}", reason),
            ),
        }
    }

    async fn plan_handler(State(state): State<ControlSurfaceState>) -> Response {
        match &state.status.load().active {
            Some(active) => json(&*active.plan),
            None => not_active(),
        }
    }

    async fn default_program_handler(State(state): State<ControlSurfaceState>) -> Response {
        json(&*state.status.load().default_program)
    }

    async fn explain_handler(State(state): State<ControlSurfaceState>, Query(query): Query<ExplainQuery>) -> Response {
        let status = state.status.load();
        let Some(active) = &status.active else {
            return not_active();
        };
        match query.format.as_deref() {
            None | Some("json") => match active.explain.to_json() {
                Ok(body) => (StatusCode::OK, [(CONTENT_TYPE, "application/json")], body).into_response(),
                Err(reason) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to write the explain document as JSON: {}", reason),
                ),
            },
            Some("text") => (
                StatusCode::OK,
                [(CONTENT_TYPE, "text/plain; charset=utf-8")],
                active.explain.to_text(),
            )
                .into_response(),
            Some(other) => error(
                StatusCode::BAD_REQUEST,
                format!(
                    "Unknown explain format `{}`: the formats are `json`, the default, and `text`.",
                    other
                ),
            ),
        }
    }
}

/// Answers `200 OK` with `value` as JSON.
fn json<T: Serialize + ?Sized>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (StatusCode::OK, [(CONTENT_TYPE, "application/json")], body).into_response(),
        Err(reason) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write the response as JSON: {}", reason),
        ),
    }
}

/// Answers with `status` and a plain-text message.
fn error(status: StatusCode, message: String) -> Response {
    (status, [(CONTENT_TYPE, "text/plain; charset=utf-8")], message).into_response()
}

/// Answers that no generation is active, so there is no plan to describe.
fn not_active() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "No generation of the metrics plan is active yet.".to_string(),
    )
}

impl APIHandler for ControlSurfaceAPIHandler {
    type State = ControlSurfaceState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route(routes::STATUS, get(Self::status_handler))
            .route(routes::RESOURCES, get(Self::resources_handler))
            .route(routes::EXPORT, get(Self::export_handler))
            .route(routes::PLAN, get(Self::plan_handler))
            .route(routes::DEFAULT_PROGRAM, get(Self::default_program_handler))
            .route(routes::EXPLAIN, get(Self::explain_handler))
    }
}

/// The worker that asserts the control surface's routes on the privileged API endpoint.
pub struct ControlSurfaceWorker {
    handler: ControlSurfaceAPIHandler,
}

impl ControlSurfaceWorker {
    /// Creates the worker over the installer's published status.
    pub fn new(status: Arc<ArcSwap<PlanStatus>>) -> Self {
        Self {
            handler: ControlSurfaceAPIHandler::new(status),
        }
    }
}

#[async_trait]
impl Supervisable for ControlSurfaceWorker {
    fn name(&self) -> &str {
        "programmable-metrics-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let route = DynamicRoute::http(EndpointType::Privileged, &self.handler);
        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;
            dataspace.assert(route, "programmable-metrics-api");
            process_shutdown.await;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests;
