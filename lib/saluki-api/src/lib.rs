pub use axum::response;
pub use axum::routing;
use axum::Router;
pub use http::StatusCode;
use tonic::service::Routes;

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

/// API endpoint type.
///
/// Identifies whether or not a route should be exposed on the unprivileged or privileged API endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

/// API endpoint protocol.
///
/// Identifies which application protocol the route should be exposed to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EndpointProtocol {
    /// HTTP.
    Http,

    /// gRPC.
    Grpc,
}

impl EndpointProtocol {
    /// Returns a human-readable name for this endpoint protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::Grpc => "gRPC",
        }
    }
}

/// A set of dynamic API routes.
///
/// Dynamic routes allow for processes to dynamically register/unregister themselves from running API endpoints,
/// adapting to changes in the process state and without requiring all routes to be known and declared upfront.
#[derive(Clone, Debug)]
pub struct DynamicRoute {
    /// Which API endpoint these routes target.
    endpoint_type: EndpointType,

    /// Which API protocol these routes target.
    endpoint_protocol: EndpointProtocol,

    /// The routes to serve.
    router: Router<()>,
}

impl DynamicRoute {
    /// Creates a dynamic HTTP route from the given API handler.
    pub fn http<T: APIHandler>(endpoint_type: EndpointType, handler: T) -> Self {
        let router = handler.generate_routes().with_state(handler.generate_initial_state());
        Self::new(endpoint_type, EndpointProtocol::Http, router)
    }

    /// Creates a dynamic gRPC route from the given Tonic routes.
    pub fn grpc(endpoint_type: EndpointType, routes: Routes) -> Self {
        let router = routes.prepare().into_axum_router();
        Self::new(endpoint_type, EndpointProtocol::Grpc, router)
    }

    fn new(endpoint_type: EndpointType, endpoint_protocol: EndpointProtocol, router: Router<()>) -> Self {
        Self {
            endpoint_type,
            endpoint_protocol,
            router,
        }
    }

    /// Returns the type of endpoint these routes target.
    pub fn endpoint_type(&self) -> EndpointType {
        self.endpoint_type
    }

    /// Returns the protocol of endpoint these routes target.
    pub fn endpoint_protocol(&self) -> EndpointProtocol {
        self.endpoint_protocol
    }

    /// Consumes this route and returns the underlying router.
    pub fn into_router(self) -> Router<()> {
        self.router
    }
}
