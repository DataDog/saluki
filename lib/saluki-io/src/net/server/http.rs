//! Basic HTTP server.

use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

use http::{Request, Response};
use http_body::Body;
use hyper::{body::Incoming, service::Service};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use saluki_core::spawn_traced;
use saluki_error::GenericError;
use tokio::{select, sync::oneshot};
use tracing::{debug, error, info};

use crate::net::listener::ConnectionOrientedListener;

/// A basic HTTP server that listens for incoming connections and serves them using a given service.
///
/// ## Missing
///
/// - Graceful shutdown (shutdown of the server itself can be triggered, but not individual connections)
pub struct HttpServer<S> {
    listener: ConnectionOrientedListener,
    conn_builder: Builder<TokioExecutor>,
    service: S,
}

impl<S> HttpServer<S> {
    /// Create a new `HttpServer` from the given listener and service.
    ///
    /// The service must be able to be cloned, as a copy is given to each incoming connection.
    pub fn from_listener(listener: ConnectionOrientedListener, service: S) -> Self {
        Self {
            listener,
            conn_builder: Builder::new(TokioExecutor::new()),
            service,
        }
    }

    /// Configures the HTTP/1 settings used for each connection.
    pub fn configure_http1_settings<F>(&mut self, f: F) -> &mut Self
    where
        F: FnOnce(&mut hyper_util::server::conn::auto::Http1Builder<'_, TokioExecutor>),
    {
        let mut http1_settings = self.conn_builder.http1();
        f(&mut http1_settings);
        self
    }

    /// Configures the HTTP/2 settings used for each connection.
    pub fn configure_http2_settings<F>(&mut self, f: F) -> &mut Self
    where
        F: FnOnce(&mut hyper_util::server::conn::auto::Http2Builder<'_, TokioExecutor>),
    {
        let mut http2_settings = self.conn_builder.http2();
        f(&mut http2_settings);
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
    pub fn listen(self) -> (ShutdownHandle, ErrorHandle) {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (error_tx, error_rx) = oneshot::channel();

        let Self {
            mut listener,
            conn_builder,
            service,
            ..
        } = self;

        spawn_traced(async move {
            info!(listen_addr = %listener.listen_address(), "HTTP server started.");

            loop {
                select! {
                    result = listener.accept() => match result {
                        Ok(stream) => {
                            let service = service.clone();
                            let conn_builder = conn_builder.clone();
                            let listen_addr = listener.listen_address().clone();

                            spawn_traced(async move {
                                if let Err(e) = conn_builder.serve_connection(TokioIo::new(stream), service).await {
                                    error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
                                }
                            });
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

pub struct ShutdownHandle(oneshot::Sender<()>);

impl ShutdownHandle {
    pub fn shutdown(self) {
        let _ = self.0.send(());
    }
}

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
