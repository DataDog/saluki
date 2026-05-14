//! A mock intake that simulates real Datadog intake APIs and allows for the dumping of ingested telemetry data in a simplified form.

#![deny(warnings)]
#![deny(missing_docs)]

use std::sync::Arc;

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::{
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    ServerConfig,
};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    select,
    signal::unix::{signal, SignalKind},
    sync::mpsc,
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};
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

    // Initialize the AWS-LC crypto provider required by rustls.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| saluki_error::generic_error!("Failed to install rustls crypto provider."))?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    spawn_signal_handlers(shutdown_tx).error_context("Failed to configure signal handlers.")?;

    // Build a self-signed TLS cert for the HTTPS proxy listener.
    //
    // The agent telemetry component always sends to an HTTPS endpoint (hardcoded in the agent),
    // so we listen on port 2050 with TLS and proxy connections through to the plain HTTP intake
    // on port 2049. The agent must be configured with `skip_ssl_validation: true` to accept the
    // self-signed cert.
    let tls_acceptor = build_tls_acceptor().error_context("Failed to build TLS acceptor for HTTPS proxy.")?;
    let https_listener = TcpListener::bind("0.0.0.0:2050")
        .await
        .error_context("Failed to bind HTTPS proxy listener on port 2050.")?;
    info!("datadog-intake HTTPS proxy: listening on 0.0.0.0:2050 (proxying to 0.0.0.0:2049)");

    // Spawn the TLS-terminating proxy as a background task.
    //
    // Each accepted TLS connection is forwarded to the plain HTTP intake on localhost:2049 via
    // a bidirectional byte-copy, so the existing HTTP handlers receive all agent telemetry
    // payloads without any changes.
    let (proxy_shutdown_tx, proxy_shutdown_rx) = mpsc::channel::<()>(1);
    tokio::spawn(run_tls_proxy(https_listener, tls_acceptor, proxy_shutdown_rx));

    info!("datadog-intake HTTP: listening on 0.0.0.0:2049");

    let listener = TcpListener::bind("0.0.0.0:2049").await.unwrap();
    axum::serve(listener, initialize_app_router())
        .with_graceful_shutdown(async move {
            shutdown_rx.recv().await.unwrap_or(());
            // Also stop the TLS proxy.
            let _ = proxy_shutdown_tx.send(()).await;
        })
        .await
        .map_err(Into::into)
}

/// Builds a [`TlsAcceptor`] backed by a freshly generated self-signed certificate.
///
/// The certificate covers the `datadog-intake` and `localhost` hostnames so that either
/// address works as a target in the agent configuration.
fn build_tls_acceptor() -> Result<TlsAcceptor, GenericError> {
    let subject_alt_names = vec!["datadog-intake".to_string(), "localhost".to_string()];
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(subject_alt_names)
        .map_err(|e| saluki_error::generic_error!("Failed to generate self-signed certificate: {}", e))?;

    let cert_chain = vec![cert.der().clone()];
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| saluki_error::generic_error!("Failed to build TLS server config: {}", e))?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Accepts TLS connections on `listener`, decrypts them, and pipes each connection to the plain
/// HTTP intake on `127.0.0.1:2049` via a bidirectional byte-copy.
async fn run_tls_proxy(listener: TcpListener, acceptor: TlsAcceptor, mut shutdown_rx: mpsc::Receiver<()>) {
    loop {
        select! {
            result = listener.accept() => {
                let (tcp_stream, peer_addr) = match result {
                    Ok(s) => s,
                    Err(e) => {
                        error!(error = %e, "TLS proxy: failed to accept connection.");
                        continue;
                    }
                };
                debug!(%peer_addr, "TLS proxy: accepted connection.");

                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = match acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            debug!(error = %e, "TLS proxy: TLS handshake failed.");
                            return;
                        }
                    };

                    let plain_stream = match TcpStream::connect("127.0.0.1:2049").await {
                        Ok(s) => s,
                        Err(e) => {
                            error!(error = %e, "TLS proxy: failed to connect to HTTP intake.");
                            return;
                        }
                    };

                    let (mut tls_read, mut tls_write) = io::split(tls_stream);
                    let (mut plain_read, mut plain_write) = io::split(plain_stream);

                    // Bidirectional byte-copy: client <-> plain HTTP intake.
                    let _ = tokio::join!(
                        io::copy(&mut tls_read, &mut plain_write),
                        io::copy(&mut plain_read, &mut tls_write),
                    );
                });
            }

            _ = shutdown_rx.recv() => {
                debug!("TLS proxy: received shutdown signal.");
                break;
            }
        }
    }
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
