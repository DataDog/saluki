//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use std::env::{self, VarError};

use saluki_common::logging::filter_from_env;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use socket2::SockRef;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, warn};
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

mod app;
use crate::app::{initialize_app_router, DogStatsDForwardingState};

/// The address of the HTTP intake, and the variable that overrides it.
const HTTP_LISTEN_ADDR: &str = "0.0.0.0:2049";
const HTTP_LISTEN_ADDR_VAR: &str = "INTAKE_HTTP_LISTEN_ADDR";

/// The address of the DogStatsD forwarding capture socket, and the variable that overrides it.
const DOGSTATSD_FORWARDING_LISTEN_ADDR: &str = "0.0.0.0:9125";
const DOGSTATSD_FORWARDING_LISTEN_ADDR_VAR: &str = "INTAKE_DOGSTATSD_LISTEN_ADDR";
const MAX_UDP_PACKET_SIZE: usize = 65535;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(true)
        .with_target(true)
        .finish()
        .with(filter_from_env(Targets::new().with_default(LevelFilter::INFO)))
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

    let http_listen_addr = listen_addr(HTTP_LISTEN_ADDR_VAR, HTTP_LISTEN_ADDR)?;
    let dogstatsd_forwarding_listen_addr =
        listen_addr(DOGSTATSD_FORWARDING_LISTEN_ADDR_VAR, DOGSTATSD_FORWARDING_LISTEN_ADDR)?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    spawn_signal_handlers(shutdown_tx).error_context("Failed to configure signal handlers.")?;

    let dogstatsd_forwarding_state = DogStatsDForwardingState::new();
    let dogstatsd_forwarding_socket = UdpSocket::bind(&dogstatsd_forwarding_listen_addr)
        .await
        .with_error_context(|| {
            format!(
                "Failed to bind DogStatsD forwarding capture socket on {dogstatsd_forwarding_listen_addr} \
                 ({DOGSTATSD_FORWARDING_LISTEN_ADDR_VAR})."
            )
        })?;

    if let Err(e) = SockRef::from(&dogstatsd_forwarding_socket).set_recv_buffer_size(1_677_720) {
        warn!(error = %e, "Failed to set UDP receive buffer size for forwarding capture socket.");
    }
    let _dogstatsd_forwarding_task = tokio::spawn(capture_dogstatsd_forwarded_packets(
        dogstatsd_forwarding_socket,
        dogstatsd_forwarding_state.clone(),
    ));

    info!("DogStatsD forwarding capture started: listening on {dogstatsd_forwarding_listen_addr}.");

    let listener = TcpListener::bind(&http_listen_addr).await.with_error_context(|| {
        format!("Failed to bind HTTP intake listener on {http_listen_addr} ({HTTP_LISTEN_ADDR_VAR}).")
    })?;
    info!("datadog-intake started: listening on {http_listen_addr}.");

    axum::serve(listener, initialize_app_router(dogstatsd_forwarding_state))
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}

/// Reads a listen address from the environment variable `var`, or returns `default` when it is unset.
fn listen_addr(var: &str, default: &str) -> Result<String, GenericError> {
    match env::var(var) {
        Ok(addr) => Ok(addr),
        Err(VarError::NotPresent) => Ok(default.to_string()),
        Err(e) => Err(generic_error!("Failed to read {var}: {e}")),
    }
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

async fn capture_dogstatsd_forwarded_packets(socket: UdpSocket, state: DogStatsDForwardingState) {
    let mut buffer = vec![0; MAX_UDP_PACKET_SIZE];

    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((bytes_read, _peer_addr)) => state.record_packet(&buffer[..bytes_read]),
            Err(e) => warn!(error = %e, "Failed to receive DogStatsD forwarded packet."),
        }
    }
}
