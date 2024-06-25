use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use datadog_protos::metrics as protos;
use ddsketch_agent::DDSketch;
use intake_data_store::DataStore;
use protobuf::Message;
use tokio::net::TcpListener;
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::error;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let shared_data_store = Arc::new(Mutex::new(DataStore::new()));

    let app = Router::new()
        .route("/api/v2/series", post(handle_post_series_v2))
        .route("/api/beta/sketches", post(handle_post_sketches_beta))
        .route("/internal/dump_state", get(handle_get_dump_state))
        .layer(CompressionLayer::new().zstd(true))
        .layer(RequestDecompressionLayer::new().deflate(true).zstd(true))
        .with_state(shared_data_store);

    let listener = TcpListener::bind("0.0.0.0:9292").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_post_series_v2(State(data_store): State<Arc<Mutex<DataStore>>>, body: Bytes) -> StatusCode {
    // Decode the payload, or return an error if it's invalid.
    let mut decoded = match protos::MetricPayload::parse_from_tokio_bytes(&body) {
        Ok(decoded) => decoded,
        Err(e) => {
            error!(error = %e, "Failed to decode series v2 payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    let mut data_store_guard = data_store.lock().unwrap();

    // Add each series in the payload to the data store.
    for mut serie in decoded.take_series() {
        let name = serie.take_metric().to_string();
        let tags = serie.take_tags().into_iter().map(|tag| tag.to_string()).collect();
        let context = data_store_guard
            .metrics_mut()
            .context_store_mut()
            .resolve_context(name, tags);

        let interval = serie.interval();
        for point in serie.points {
            data_store_guard
                .metrics_mut()
                .add_point(context, point.timestamp(), interval, point.value());
        }
    }

    StatusCode::ACCEPTED
}

async fn handle_post_sketches_beta(State(data_store): State<Arc<Mutex<DataStore>>>, body: Bytes) -> StatusCode {
    // Decode the payload, or return an error if it's invalid.
    let mut decoded = match protos::SketchPayload::parse_from_tokio_bytes(&body) {
        Ok(decoded) => decoded,
        Err(e) => {
            error!(error = %e, "Failed to decode sketches payload.");
            return StatusCode::BAD_REQUEST;
        }
    };

    let mut data_store_guard = data_store.lock().unwrap();

    // Add each sketch in the payload to the data store.
    for mut sketch in decoded.take_sketches() {
        let name = sketch.take_metric().to_string();
        let tags = sketch.take_tags().into_iter().map(|tag| tag.to_string()).collect();
        let context = data_store_guard
            .metrics_mut()
            .context_store_mut()
            .resolve_context(name, tags);

        for dogsketch in sketch.dogsketches() {
            let ts = dogsketch.ts();
            let ddsketch = DDSketch::from(dogsketch);
            data_store_guard.metrics_mut().add_distribution(context, ts, ddsketch);
        }
    }

    StatusCode::ACCEPTED
}

async fn handle_get_dump_state(State(data_store): State<Arc<Mutex<DataStore>>>) -> Json<DataStore> {
    let data_store_guard = data_store.lock().unwrap();
    Json(data_store_guard.clone())
}
