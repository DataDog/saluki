//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use saluki_error::{ErrorContext as _, GenericError};
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod app;
use crate::app::{initialize_app_router, DogStatsDForwardingState};

const HTTP_LISTEN_ADDR: &str = "0.0.0.0:2049";
const DOGSTATSD_FORWARDING_LISTEN_ADDR: &str = "0.0.0.0:9125";
const MAX_UDP_PACKET_SIZE: usize = 65535;

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

    let dogstatsd_forwarding_state = DogStatsDForwardingState::new();
    let dogstatsd_forwarding_socket = UdpSocket::bind(DOGSTATSD_FORWARDING_LISTEN_ADDR)
        .await
        .error_context("Failed to bind DogStatsD forwarding capture socket.")?;
    let _dogstatsd_forwarding_task = tokio::spawn(capture_dogstatsd_forwarded_packets(
        dogstatsd_forwarding_socket,
        dogstatsd_forwarding_state.clone(),
    ));

    info!("DogStatsD forwarding capture started: listening on {DOGSTATSD_FORWARDING_LISTEN_ADDR}.");

    let listener = TcpListener::bind(HTTP_LISTEN_ADDR)
        .await
        .error_context("Failed to bind HTTP intake listener.")?;
    info!("datadog-intake started: listening on {HTTP_LISTEN_ADDR}.");

    axum::serve(listener, initialize_app_router(dogstatsd_forwarding_state))
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}

#[cfg(unix)]
fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<(), GenericError> {
    let mut sigint_handler = signal(SignalKind::interrupt()).error_context("Failed to set up SIGINT handler.")?;
    let mut sigterm_handler = signal(SignalKind::terminate()).error_context("Failed to set up SIGTERM handler.")?;

    tokio::spawn(async move {
        tokio::select! {
            _ = sigint_handler.recv() => {
                info!("Received SIGINT, shutting down...");
            }
            _ = sigterm_handler.recv() => {
                info!("Received SIGTERM, shutting down...");
            }
        }

        send_shutdown_signal(shutdown_tx).await;
    });

    Ok(())
}

#[cfg(not(unix))]
fn spawn_signal_handlers(shutdown_tx: mpsc::Sender<()>) -> Result<(), GenericError> {
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Failed to wait for Ctrl-C: {:?}", e);
        } else {
            info!("Received Ctrl-C, shutting down...");
        }

        send_shutdown_signal(shutdown_tx).await;
    });

    Ok(())
}

async fn send_shutdown_signal(shutdown_tx: mpsc::Sender<()>) {
    if let Err(e) = shutdown_tx.send(()).await {
        error!("Failed to send shutdown signal: {:?}", e);
    }
}

async fn capture_dogstatsd_forwarded_packets(socket: UdpSocket, state: DogStatsDForwardingState) {
    let mut buffer = vec![0; MAX_UDP_PACKET_SIZE];

    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((bytes_read, _peer_addr)) => state.record_packet(&buffer[..bytes_read]),
            Err(e) => warn!(error = %e, "Failed to receive DogStatsD forwarded packet."),
        }
    }
}
