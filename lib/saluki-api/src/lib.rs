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

/// Describes which API endpoint a dynamic handler targets.
///
/// Publishers assert this value in the [`DataspaceRegistry`] alongside a `Router<()>` resource published to the
/// [`ResourceRegistry`] under the same [`Handle`]. The dynamic API server observes assertions and retractions via a
/// wildcard subscription, using the handle to look up the corresponding router resource.
#[derive(Clone, Debug)]
pub struct APIEndpointInterest {
    /// Which API endpoint this interest targets.
    pub endpoint: EndpointType,
}
