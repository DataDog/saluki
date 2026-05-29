use axum::{extract::State, Json};
use tracing::info;

use super::DogStatsDForwardingState;

pub async fn handle_dogstatsd_forwarding_dump(State(state): State<DogStatsDForwardingState>) -> Json<Vec<String>> {
    info!("Got request to dump forwarded DogStatsD packets.");
    Json(state.dump_packets())
}
