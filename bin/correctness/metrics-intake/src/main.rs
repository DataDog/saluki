use axum::{
    routing::{get, post},
    Router,
};
use saluki_app::prelude::*;
use saluki_error::GenericError;
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::{error, info};

mod handlers;
use self::handlers::*;

mod state;
use self::state::*;

#[tokio::main]
async fn main() {
    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

    match run().await {
        Ok(()) => info!("metrics-intake stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), GenericError> {
    info!("metrics-intake starting...");

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    let intake_state = IntakeState::new(shutdown_tx);

    let app = Router::new()
        // Management routes.
        .route("/shutdown", post(handle_shutdown))
        .route("/metrics/dump", get(handle_metrics_dump))
        // Routes to handle simulated intake endpoints.
        .route("/api/v1/validate", get(handle_validate_v1))
        .route("/api/v1/metadata", post(handle_metadata_v1))
        .route("/api/v1/check_run", post(handle_check_run_v1))
        .route("/intake/", post(handle_intake))
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .route_layer(RequestDecompressionLayer::new())
        .route_layer(CompressionLayer::new().zstd(true))
        .with_state(intake_state);

    let listener = TcpListener::bind("0.0.0.0:2049").await.unwrap();

    info!("metrics-intake started: listening on 0.0.0.0:2049");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}
