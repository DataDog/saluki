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
