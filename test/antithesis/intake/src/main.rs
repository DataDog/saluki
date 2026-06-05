//! A mock Datadog intake for the Antithesis harness.
//!
//! Simulates the real `/api/v2/series` intake and, on every payload the Agent
//! Data Plane delivers, fires Antithesis SDK assertions for the W properties
//! (W1-W22) ported from the `invariant-jig` checker. These observe whether the
//! Agent honoured its wire contract. Ingested metrics are also recorded for
//! `/metrics/dump`, which the `finally_verify_delivery` test command polls to
//! confirm end-to-end delivery.

#![deny(warnings)]
#![deny(missing_docs)]

use std::sync::Arc;

use antithesis_sdk::prelude::*;
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{net::TcpListener, sync::mpsc};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod handlers;
mod http;
mod intake;
mod predicates;
mod state;

use crate::state::{AppState, MetricsState};

const HTTP_LISTEN_ADDR: &str = "0.0.0.0:2049";

/// The Agent hostname W17 resolves each series against, when `DD_HOSTNAME` is
/// unset. Matches the default the harness writes into ADP's `datadog.yaml`.
const DEFAULT_HOSTNAME: &str = "antithesis-adp";

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
        .init();

    antithesis_init();

    match run().await {
        Ok(()) => info!("antithesis-intake stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), GenericError> {
    info!("antithesis-intake starting...");

    let expected_hostname: Arc<str> = std::env::var("DD_HOSTNAME")
        .unwrap_or_else(|_| DEFAULT_HOSTNAME.to_owned())
        .into();
    info!(hostname = %expected_hostname, "W17 resolves each series host against this hostname.");

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    spawn_signal_handlers(shutdown_tx).error_context("Failed to configure signal handlers.")?;

    let state = AppState {
        metrics: MetricsState::new(),
        expected_hostname,
    };

    let listener = TcpListener::bind(HTTP_LISTEN_ADDR)
        .await
        .error_context("Failed to bind HTTP intake listener.")?;
    info!("antithesis-intake started: listening on {HTTP_LISTEN_ADDR}.");

    axum::serve(listener, http::build_router(state))
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}

#[cfg(unix)]
fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<(), GenericError> {
    use tokio::{
        select,
        signal::unix::{signal, SignalKind},
    };

    let mut sigint_handler = signal(SignalKind::interrupt()).error_context("Failed to set up SIGINT handler.")?;
    let mut sigterm_handler = signal(SignalKind::terminate()).error_context("Failed to set up SIGTERM handler.")?;

    tokio::spawn(async move {
        select! {
            _ = sigint_handler.recv() => info!("Received SIGINT, shutting down..."),
            _ = sigterm_handler.recv() => info!("Received SIGTERM, shutting down..."),
        }

        if let Err(e) = shutdown_tx.send(()).await {
            error!("Failed to send shutdown signal: {:?}", e);
        }
    });

    Ok(())
}

#[cfg(windows)]
fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<(), GenericError> {
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => info!("Received Ctrl-C, shutting down..."),
            Err(e) => error!(error = %e, "Failed to receive Ctrl-C signal."),
        }

        if let Err(e) = shutdown_tx.send(()).await {
            error!("Failed to send shutdown signal: {:?}", e);
        }
    });

    Ok(())
}
