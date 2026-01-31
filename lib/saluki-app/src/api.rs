//! API server.

use std::{convert::Infallible, error::Error, future::Future};

use axum::Router;
use http::{Request, Response};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::{pki_types::PrivateKeyDer, ServerConfig};
use rustls_pki_types::PrivatePkcs8KeyDer;
use saluki_api::APIHandler;
use saluki_error::GenericError;
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::{http::HttpServer, multiplex_service::MultiplexService},
    util::hyper::TowerToHyperService,
    ListenAddress,
};
use tokio::select;
use tonic::{body::Body, server::NamedService, service::RoutesBuilder};
use tower::Service;

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
    http_router: Router,
    grpc_router: RoutesBuilder,
    tls_config: Option<ServerConfig>,
}

impl APIBuilder {
    /// Create a new `APIBuilder` with an empty router.
    ///
    /// A fallback route will be provided that returns a 404 Not Found response for any route that isn't explicitly handled.
    pub fn new() -> Self {
        Self {
            http_router: Router::new(),
            grpc_router: RoutesBuilder::default(),
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
        self.http_router = self.http_router.merge(handler_router.with_state(handler_state));

        self
    }

    /// Adds the given optional handler to this builder.
    ///
    /// If the handler is `Some`, the initial state and routes provided by the handler will be merged into this builder.
    /// Otherwise, this builder will be returned unchanged.
    pub fn with_optional_handler<H>(self, handler: Option<H>) -> Self
    where
        H: APIHandler,
    {
        if let Some(handler) = handler {
            self.with_handler(handler)
        } else {
            self
        }
    }

    /// Add the given gRPC service to this builder.
    pub fn with_grpc_service<S>(mut self, svc: S) -> Self
    where
        S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
            + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Box<dyn Error + Send + Sync>> + Send,
    {
        self.grpc_router.add_service(svc);
        self
    }

    /// Sets the TLS configuration for the server.
    ///
    /// This will enable TLS for the server, and the server will only accept connections that are encrypted with TLS.
    ///
    /// Defaults to TLS being disabled.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Sets the TLS configuration for the server based on a dynamically generated self-signed certificate.
    ///
    /// This will enable TLS for the server, and the server will only accept connections that are encrypted with TLS.
    pub fn with_self_signed_tls(self) -> Self {
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(["localhost".to_owned()]).unwrap();
        let cert_chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .unwrap();

        self.with_tls_config(config)
    }

    /// Serves the API on the given listen address until `shutdown` resolves.
    ///
    /// The listen address must be a connection-oriented address (TCP or Unix domain socket in SOCK_STREAM mode).
    ///
    /// # Errors
    ///
    /// If the given listen address is not connection-oriented, or if the server fails to bind to the address, or if
    /// there is an error while accepting for new connections, an error will be returned.
    pub async fn serve<F>(self, listen_address: ListenAddress, shutdown: F) -> Result<(), GenericError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = ConnectionOrientedListener::from_listen_address(listen_address).await?;

        // Wrap up our HTTP and gRPC routers in a multiplexed service, allowing us to handle both types of requests on
        // the same port. Additionally, we have to wrap the service to translate from `tower::Service` to `hyper::Service`.
        let multiplexed_service = TowerToHyperService::new(MultiplexService::new(
            self.http_router,
            self.grpc_router.routes().into_axum_router(),
        ));

        // Create and spawn the HTTP server.
        let mut http_server = HttpServer::from_listener(listener, multiplexed_service);
        if let Some(tls_config) = self.tls_config {
            http_server = http_server.with_tls_config(tls_config);
        }
        let (shutdown_handle, error_handle) = http_server.listen();

        // Wait for our shutdown signal, which we'll forward to the listener to stop accepting new connections... or
        // capture any errors thrown by the listener itself.
        select! {
            _ = shutdown =>  shutdown_handle.shutdown(),
            maybe_err = error_handle => if let Some(e) = maybe_err {
                return Err(GenericError::from(e))
            },
        }

        Ok(())
    }
}
