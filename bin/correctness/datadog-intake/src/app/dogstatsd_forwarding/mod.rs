use axum::{routing::get, Router};

mod handlers;
use self::handlers::*;

mod state;
pub use self::state::DogStatsDForwardingState;

pub fn build_dogstatsd_forwarding_router(state: DogStatsDForwardingState) -> Router {
    Router::new()
        .route("/dogstatsd-forwarding/dump", get(handle_dogstatsd_forwarding_dump))
        .with_state(state)
}
