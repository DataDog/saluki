//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use datadog_protos::stateful::stateful_logs_service_server::StatefulLogsServiceServer;
use saluki_error::{ErrorContext as _, GenericError};
use std::net::SocketAddr;
use tokio::{
    net::{TcpListener, UdpSocket},
    select,
    signal::unix::{signal, SignalKind},
    sync::broadcast,
};
use tracing::{error, info, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

mod app;
use crate::app::{initialize_app_router, DogStatsDForwardingState, StatefulLogsGrpcService, StatefulLogsState};

const HTTP_LISTEN_ADDR: &str = "0.0.0.0:2049";
const STATEFUL_LOGS_GRPC_LISTEN_ADDR: &str = "0.0.0.0:2050";
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

    let (shutdown_tx, _) = broadcast::channel(1);
    spawn_signal_handlers(shutdown_tx.clone()).error_context("Failed to configure signal handlers.")?;

    let dogstatsd_forwarding_state = DogStatsDForwardingState::new();
    let stateful_logs_state = StatefulLogsState::new();
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
    let grpc_addr: SocketAddr = STATEFUL_LOGS_GRPC_LISTEN_ADDR
        .parse()
        .error_context("Failed to parse stateful logs gRPC listen address.")?;
    info!("datadog-intake HTTP server started: listening on {HTTP_LISTEN_ADDR}.");
    info!("datadog-intake stateful logs gRPC server starting: listening on {STATEFUL_LOGS_GRPC_LISTEN_ADDR}.");

    let mut http_shutdown_rx = shutdown_tx.subscribe();
    let http_server = axum::serve(
        listener,
        initialize_app_router(dogstatsd_forwarding_state, stateful_logs_state.clone()),
    )
    .with_graceful_shutdown(async move {
        let _ = http_shutdown_rx.recv().await;
    });

    let mut grpc_shutdown_rx = shutdown_tx.subscribe();
    let grpc_server = tonic::transport::Server::builder()
        .add_service(StatefulLogsServiceServer::new(StatefulLogsGrpcService::new(
            stateful_logs_state,
        )))
        .serve_with_shutdown(grpc_addr, async move {
            let _ = grpc_shutdown_rx.recv().await;
        });

    select! {
        result = http_server => {
            result.error_context("HTTP intake server failed.")?;
        }
        result = grpc_server => {
            result.error_context("Stateful logs gRPC intake server failed.")?;
        }
    }

    Ok(())
}

fn spawn_signal_handlers(shutdown_tx: broadcast::Sender<()>) -> Result<(), GenericError> {
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

        if let Err(e) = shutdown_tx.send(()) {
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
