//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use saluki_error::{ErrorContext as _, GenericError};
use tokio::{
    net::TcpListener,
    select,
    signal::unix::{signal, SignalKind},
    sync::mpsc,
};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod app;
use crate::app::initialize_app_router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    match run().await {
        Ok(()) => info!("datadog-intake stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), GenericError> {
    info!("datadog-intake starting...");

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    spawn_signal_handlers(shutdown_tx).error_context("Failed to configure signal handlers.")?;

    info!("datadog-intake started: listening on 0.0.0.0:2049");

    let listener = TcpListener::bind("0.0.0.0:2049").await.unwrap();
    axum::serve(listener, initialize_app_router())
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}

fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<(), GenericError> {
    let mut sigint_handler = signal(SignalKind::interrupt()).error_context("Failed to set up SIGINT handler.")?;
    let mut sigterm_handler = signal(SignalKind::terminate()).error_context("Failed to set up SIGTERM handler.")?;

    tokio::spawn(async move {
        select! {
            _ = sigint_handler.recv() => {
                info!("Received SIGINT, shutting down...");
            }
            _ = sigterm_handler.recv() => {
                info!("Received SIGTERM, shutting down...");
            }
        }

        if let Err(e) = shutdown_tx.send(()).await {
            error!("Failed to send shutdown signal: {:?}", e);
        }
    });

    Ok(())
}
