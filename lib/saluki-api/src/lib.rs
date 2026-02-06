pub use axum::response;
pub use axum::routing;
use axum::Router;
pub use http::StatusCode;

pub mod extract {
    pub use axum::extract::*;
    pub use axum_extra::extract::Query;
}

// An API handler.
//
// API handlers define the initial state and routes for a portion of an API.
pub trait APIHandler {
    type State: Clone + Send + Sync + 'static;

    fn generate_initial_state(&self) -> Self::State;
    fn generate_routes(&self) -> Router<Self::State>;
}

/// Identifies which API endpoint a handler should be bound to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EndpointType {
    /// The unprivileged (plain HTTP) API endpoint.
    Unprivileged,
    /// The privileged (TLS-protected) API endpoint.
    Privileged,
}

/// Describes whether a handler is being registered or withdrawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterestType {
    /// Register a new set of routes.
    Register,
    /// Withdraw a previously-registered set of routes.
    Withdraw,
}

/// A notification that an API handler wants to register or withdraw routes for a specific endpoint.
///
/// The `identifier` must match the resource registry identifier under which the corresponding `Router<()>` (with state
/// already applied) has been published.
#[derive(Clone, Debug)]
pub struct APIEndpointInterest {
    /// Which API endpoint this interest targets.
    pub endpoint: EndpointType,
    /// Whether routes are being added or removed.
    pub interest: InterestType,
    /// The resource registry identifier for the `Router<()>` resource.
    pub identifier: String,
}
