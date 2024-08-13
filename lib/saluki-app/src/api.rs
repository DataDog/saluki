//! API server.

use std::future::Future;

use axum::Router;
use hyper_util::service::TowerToHyperService;
use saluki_api::APIHandler;
use saluki_error::GenericError;
use saluki_io::net::{listener::ConnectionOrientedListener, server::http::HttpServer, ListenAddress};
use tokio::select;
use tracing::error;

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
}

impl APIBuilder {
    /// Create a new `APIBuilder` with an empty router.
    ///
    /// A fallback route will be provided that returns a 404 Not Found response for any route that isn't explicitly handled.
    pub fn new() -> Self {
        Self { router: Router::new() }
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
        let http_server = HttpServer::from_listener(listener, service);
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
