//! API server.

use std::{future::Future, io::BufReader};

use axum::Router;
use rustls::ServerConfig;
use saluki_api::APIHandler;
use saluki_error::GenericError;
use saluki_io::net::{
    listener::ConnectionOrientedListener, server::http::HttpServer, util::hyper::TowerToHyperService, ListenAddress,
};
use tokio::select;
use tracing::error;

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls_pemfile::{certs, pkcs8_private_keys};

/// An API builder.
///
/// `APIBuilder` provides a simple and ergonomic builder pattern for constructing an API server from multiple handlers.
/// This allows composing portions of an API from individual building blocks.
///
/// ## Missing
///
/// - TLS support
/// - API-wide authentication support (can be added at the per-handler level)
/// - graceful shutdown (shutdown stops new connections, but does not wait for existing connections to close)
#[derive(Default)]
pub struct APIBuilder {
    router: Router,
    tls_config: Option<ServerConfig>,
}

impl APIBuilder {
    /// Create a new `APIBuilder` with an empty router.
    ///
    /// A fallback route will be provided that returns a 404 Not Found response for any route that isn't explicitly handled.
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            tls_config: None,
        }
    }

    /// Adds the given handler to this builder.
    ///
    /// The initial state and routes provided by the handler will be merged into this builder.
    pub fn with_handler<H>(mut self, handler: H) -> Self
    where
        H: APIHandler,
    {
        let handler_router = handler.generate_routes();
        let handler_state = handler.generate_initial_state();
        self.router = self.router.merge(handler_router.with_state(handler_state));

        self
    }

    /// Adds the given tls configuration to this builder.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Adds a self signed certificate to this builder.
    pub fn with_self_signed_tls(self) -> Self {
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(["localhost".to_owned()]).unwrap();
        let cert_file = cert.pem();
        let key_file = key_pair.serialize_pem();

        let cert_file = &mut BufReader::new(cert_file.as_bytes());
        let key_file = &mut BufReader::new(key_file.as_bytes());

        let cert_chain = certs(cert_file).collect::<Result<Vec<_>, _>>().unwrap();
        let mut keys = pkcs8_private_keys(key_file).collect::<Result<Vec<_>, _>>().unwrap();

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0)))
            .unwrap();

        self.with_tls_config(config)
    }

    /// Serves the API on the given listen address until `shutdown` resolves.
    ///
    /// The listen address must be a connection-oriented address (TCP or Unix domain socket in SOCK_STREAM mode).
    ///
    /// ## Errors
    ///
    /// If the given listen address is not connection-oriented, or if the server fails to bind to the address, or if
    /// there is an error while accepting for new connections, an error will be returned.
    pub async fn serve<F>(self, listen_address: ListenAddress, shutdown: F) -> Result<(), GenericError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = ConnectionOrientedListener::from_listen_address(listen_address).await?;

        // We have to convert this Tower-based `Service` to a Hyper-based `Service` to use it with `HttpServer`, since
        // the two traits are different from a semver perspective.
        let service = TowerToHyperService::new(self.router);

        // Create and spawn the HTTP server.
        let mut http_server = HttpServer::from_listener(listener, service);
        if let Some(tls_config) = self.tls_config {
            http_server = http_server.with_tls_config(tls_config);
        }
        let (shutdown_handle, error_handle) = http_server.listen();

        // Wait for our shutdown signal, which we'll forward to the listener to stop accepting new connections... or
        // capture any errors thrown by the listener itself.
        tokio::spawn(async move {
            select! {
                _ = shutdown => shutdown_handle.shutdown(),
                maybe_err = error_handle => if let Some(err) = maybe_err {
                    error!(error = ?err, "Failed to serve API connection.");
                },
            }
        });

        Ok(())
    }
}
