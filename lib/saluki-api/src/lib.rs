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

impl EndpointType {
    /// Returns a human-readable name for this endpoint type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unprivileged => "unprivileged",
            Self::Privileged => "privileged",
        }
    }
}

/// A dynamically-registered HTTP route set, asserted into the dataspace registry.
///
/// Publishers assert a `DynamicHttpRoute` under a [`Handle`] to register HTTP routes with a dynamic API server. The
/// server observes assertions and retractions via a wildcard subscription and hot-swaps its inner HTTP router.
#[derive(Clone, Debug)]
pub struct DynamicHttpRoute {
    /// Which API endpoint these routes target.
    pub endpoint: EndpointType,
    /// The HTTP routes to serve.
    pub router: Router<()>,
}

/// A dynamically-registered gRPC route set, asserted into the dataspace registry.
///
/// Publishers assert a `DynamicGrpcRoute` under a [`Handle`] to register gRPC routes with a dynamic API server. The
/// router should be pre-converted from tonic routes (via [`tonic::Routes::into_axum_router`]).
#[derive(Clone, Debug)]
pub struct DynamicGrpcRoute {
    /// Which API endpoint these routes target.
    pub endpoint: EndpointType,
    /// The gRPC routes to serve, as an axum router.
    pub router: Router<()>,
}
