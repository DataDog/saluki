//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use std::{env::VarError, net::SocketAddr};

use saluki_common::logging::filter_from_env;
use saluki_error::{ErrorContext as _, GenericError};
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

const HTTP_LISTEN_ADDR: &str = "0.0.0.0:2049";
const DOGSTATSD_FORWARDING_LISTEN_ADDR: &str = "0.0.0.0:9125";
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

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    spawn_signal_handlers(shutdown_tx).error_context("Failed to configure signal handlers.")?;

    let dogstatsd_address = listen_address(
        "INTAKE_DOGSTATSD_LISTEN_ADDR",
        std::env::var("INTAKE_DOGSTATSD_LISTEN_ADDR"),
        DOGSTATSD_FORWARDING_LISTEN_ADDR,
    )?;
    let http_address = listen_address(
        "INTAKE_HTTP_LISTEN_ADDR",
        std::env::var("INTAKE_HTTP_LISTEN_ADDR"),
        HTTP_LISTEN_ADDR,
    )?;

    let dogstatsd_forwarding_state = DogStatsDForwardingState::new();
    let dogstatsd_forwarding_socket = UdpSocket::bind(dogstatsd_address)
        .await
        .error_context("Failed to bind DogStatsD forwarding capture socket (INTAKE_DOGSTATSD_LISTEN_ADDR).")?;

    if let Err(e) = SockRef::from(&dogstatsd_forwarding_socket).set_recv_buffer_size(1_677_720) {
        warn!(error = %e, "Failed to set UDP receive buffer size for forwarding capture socket.");
    }
    let dogstatsd_address = dogstatsd_forwarding_socket
        .local_addr()
        .error_context("Failed to read bound DogStatsD forwarding address.")?;
    let _dogstatsd_forwarding_task = tokio::spawn(capture_dogstatsd_forwarded_packets(
        dogstatsd_forwarding_socket,
        dogstatsd_forwarding_state.clone(),
    ));

    info!("DogStatsD forwarding capture started: listening on {dogstatsd_address}.");

    let listener = TcpListener::bind(http_address)
        .await
        .error_context("Failed to bind HTTP intake listener (INTAKE_HTTP_LISTEN_ADDR).")?;
    let http_address = listener
        .local_addr()
        .error_context("Failed to read bound HTTP intake address.")?;
    info!("datadog-intake started: listening on {http_address}.");

    axum::serve(listener, initialize_app_router(dogstatsd_forwarding_state))
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap_or(()) })
        .await
        .map_err(Into::into)
}

fn listen_address(key: &str, value: Result<String, VarError>, default: &str) -> Result<SocketAddr, GenericError> {
    let value = match value {
        Ok(value) => value,
        Err(VarError::NotPresent) => default.into(),
        Err(error) => return Err(error).error_context(format!("Failed to read {key}.")),
    };
    value
        .parse()
        .error_context(format!("Invalid {key}='{value}': expected host:port."))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses_default_to_the_historical_constants() {
        for (key, default) in [
            ("INTAKE_HTTP_LISTEN_ADDR", HTTP_LISTEN_ADDR),
            ("INTAKE_DOGSTATSD_LISTEN_ADDR", DOGSTATSD_FORWARDING_LISTEN_ADDR),
        ] {
            assert_eq!(
                listen_address(key, Err(VarError::NotPresent), default).unwrap(),
                default.parse::<SocketAddr>().unwrap()
            );
            assert_eq!(
                listen_address(key, Ok("[::]:8080".into()), default).unwrap(),
                "[::]:8080".parse::<SocketAddr>().unwrap()
            );
            let error = listen_address(key, Ok("invalid".into()), default).unwrap_err();
            assert!(error.to_string().contains(key), "{error}");
        }
    }
}
