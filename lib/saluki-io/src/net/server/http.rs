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
use saluki_core::task::spawn_traced;
use saluki_error::GenericError;
use tokio::{select, sync::oneshot};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};

use crate::net::listener::ConnectionOrientedListener;

/// An HTTP server.
pub struct HttpServer<S> {
    listener: ConnectionOrientedListener,
    conn_builder: Builder<TokioExecutor>,
    service: S,
    tls_config: Option<ServerConfig>,
}

impl<S> HttpServer<S> {
    /// Creates a new `HttpServer` from the given listener and service.
    pub fn from_listener(listener: ConnectionOrientedListener, service: S) -> Self {
        Self {
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
    pub fn listen(self) -> (ShutdownHandle, ErrorHandle) {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (error_tx, error_rx) = oneshot::channel();

        let Self {
            mut listener,
            conn_builder,
            service,
            tls_config,
            ..
        } = self;

        spawn_traced(async move {
            let tls_enabled = tls_config.is_some();
            let maybe_tls_acceptor = tls_config.map(|mut config| {
                // Allow for HTTP/1.1 and HTTP/2.
                config.alpn_protocols.push(b"h2".to_vec());
                config.alpn_protocols.push(b"http/1.1".to_vec());
                TlsAcceptor::from(Arc::new(config))
            });

            info!(listen_addr = %listener.listen_address(), ?tls_enabled, "HTTP server started.");

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

                                    spawn_traced(async move {
                                        if let Err(e) = conn_builder.serve_connection(TokioIo::new(tls_stream), service).await {
                                            error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
                                        }
                                    });
                                },
                                None => {
                                    spawn_traced(async move {
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

                    _ = &mut shutdown_rx => {
                        debug!(listen_addr = %listener.listen_address(), "Received shutdown signal.");
                        break;
                    }
                }
            }

            info!(listen_addr = %listener.listen_address(), "HTTP server stopped.");
        });

        (ShutdownHandle(shutdown_tx), ErrorHandle(error_rx))
    }
}

/// A handle for shutting down an [`HttpServer`].
pub struct ShutdownHandle(oneshot::Sender<()>);

impl ShutdownHandle {
    /// Triggers the server to shutdown.
    ///
    /// This method does not wait for shutdown to occur.
    pub fn shutdown(self) {
        let _ = self.0.send(());
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
