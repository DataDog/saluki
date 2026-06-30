//! Basic HTTP server.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};

use http::{Request, Response};
use http_body::Body;
use hyper::{body::Incoming, service::Service};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use rustls::ServerConfig;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::{spawn_traced_named, HandleExt as _},
};
use saluki_error::GenericError;
use saluki_tls::ensure_server_config_fips_compliant;
use tokio::{pin, runtime::Handle, select, sync::oneshot};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};

use crate::net::listener::ConnectionOrientedListener;

/// An HTTP server.
pub struct HttpServer<S> {
    executor: Handle,
    listener: ConnectionOrientedListener,
    conn_builder: Builder<TokioExecutor>,
    service: S,
    tls_config: Option<ServerConfig>,
}

impl<S> HttpServer<S> {
    /// Creates a new `HttpServer` from the given listener and service.
    ///
    /// # Panics
    ///
    /// This will panic if called outside the context of a Tokio runtime.
    pub fn from_listener(listener: ConnectionOrientedListener, service: S) -> Self {
        Self {
            executor: Handle::current(),
            listener,
            conn_builder: Builder::new(TokioExecutor::new()),
            service,
            tls_config: None,
        }
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

    /// Sets the executor for the server.
    ///
    /// This executor will be used for spawning tasks to handle incoming connections, but _not_ for the spawn that accepts
    /// new connections.
    ///
    /// Defaults to the current Tokio runtime at the time `HttpServer::new` is called.
    pub fn with_executor(mut self, executor: Handle) -> Self {
        self.executor = executor;
        self
    }
}

impl<S, B> HttpServer<S>
where
    S: Service<Request<Incoming>, Response = Response<B>> + Send + Clone + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    /// Starts the server and listens for incoming connections.
    ///
    /// Returns two handles: one for shutting down the server, and one for receiving any errors that occur while the
    /// server is running.
    pub fn listen(self) -> (ShutdownCoordinator, ErrorHandle) {
        let (shutdown_coordinator, shutdown) = ShutdownHandle::paired();
        let (error_tx, error_rx) = oneshot::channel();

        let Self {
            executor,
            mut listener,
            conn_builder,
            service,
            tls_config,
            ..
        } = self;

        spawn_traced_named("http-server-acceptor", async move {
            let maybe_tls_acceptor = match tls_config {
                Some(mut config) => {
                    // Allow for HTTP/1.1 and HTTP/2.
                    config.alpn_protocols.push(b"h2".to_vec());
                    config.alpn_protocols.push(b"http/1.1".to_vec());

                    if let Err(e) = ensure_server_config_fips_compliant(&config) {
                        let _ = error_tx.send(e);
                        return;
                    }

                    Some(TlsAcceptor::from(Arc::new(config)))
                }
                None => None,
            };
            let tls_enabled = maybe_tls_acceptor.is_some();

            info!(listen_addr = %listener.listen_address(), tls_enabled, "HTTP server started.");

            pin!(shutdown);

            loop {
                select! {
                    result = listener.accept() => match result {
                        Ok(stream) => {
                            let service = service.clone();
                            let conn_builder = conn_builder.clone();
                            let listen_addr = listener.listen_address().clone();
                            match &maybe_tls_acceptor {
                                Some(acceptor) => {
                                    let tls_stream = match acceptor.accept(stream).await {
                                        Ok(stream) => stream,
                                        Err(e) => {
                                            error!(%listen_addr, error = %e, "Failed to complete TLS handshake.");
                                            continue
                                        },
                                    };

                                    executor.spawn_traced_named("http-server-tls-conn-handler", async move {
                                        if let Err(e) = conn_builder.serve_connection(TokioIo::new(tls_stream), service).await {
                                            error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
                                        }
                                    });
                                },
                                None => {
                                    executor.spawn_traced_named("http-server-conn-handler", async move {
                                        if let Err(e) = conn_builder.serve_connection(TokioIo::new(stream), service).await {
                                            error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
                                        }
                                    });
                                },
                            }
                        },
                        Err(e) => {
                            let _ = error_tx.send(e.into());
                            break;
                        }
                    },

                    _ = &mut shutdown => {
                        debug!(listen_addr = %listener.listen_address(), "Received shutdown signal.");
                        break;
                    }
                }
            }

            info!(listen_addr = %listener.listen_address(), "HTTP server stopped.");
        });

        (shutdown_coordinator, ErrorHandle(error_rx))
    }
}

/// A future that resolves when [`HttpServer`] encounters an unrecoverable error.
pub struct ErrorHandle(oneshot::Receiver<GenericError>);

impl Future for ErrorHandle {
    type Output = Option<GenericError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match ready!(Pin::new(&mut self.0).poll(cx)) {
            Ok(err) => Poll::Ready(Some(err)),
            Err(_) => Poll::Ready(None),
        }
    }
}
