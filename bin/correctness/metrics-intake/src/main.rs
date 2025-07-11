//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested metrics in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use saluki_app::{logging::LoggingConfiguration, prelude::*};
use saluki_error::GenericError;
use tokio::{
    net::TcpListener,
    select,
    signal::unix::{signal, SignalKind},
    sync::mpsc,
};
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing::{error, info};

mod handlers;
use self::handlers::*;

mod state;
use self::state::*;

#[tokio::main]
async fn main() {
    if let Err(e) = initialize_logging(&LoggingConfiguration::default()) {
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
    let intake_state = IntakeState::new();

    let app = Router::new()
        // Management routes.
        .route("/metrics/dump", get(handle_metrics_dump))
        // Routes to handle simulated intake endpoints.
        .route("/api/v1/validate", get(handle_validate_v1))
        .route("/api/v1/metadata", post(handle_metadata_v1))
        .route("/api/v1/check_run", post(handle_check_run_v1))
        .route("/intake/", post(handle_intake))
        .route("/api/v2/series", post(handle_series_v2))
        .route("/api/beta/sketches", post(handle_sketch_beta))
        .route_layer(RequestDecompressionLayer::new().deflate(true).zstd(true))
        .route_layer(CompressionLayer::new().zstd(true))
        // Decompressed metrics payloads can be large (~62MB for sketches).
        .route_layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(intake_state);

    let listener = TcpListener::bind("0.0.0.0:2049").await.unwrap();

    info!("metrics-intake started: listening on 0.0.0.0:2049");

    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(sig) => sig,
            Err(e) => {
                error!("Failed to set up SIGINT handler: {:?}", e);
                return;
            }
        };

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(e) => {
                error!("Failed to set up SIGTERM handler: {:?}", e);
                return;
            }
        };

        select! {
            _ = sigint.recv() => {
                info!("Received SIGINT, shutting down...");
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
            }
        }

        if let Err(e) = shutdown_tx.send(()).await {
            error!("Failed to send shutdown signal: {:?}", e);
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}
