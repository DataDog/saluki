use std::{
    fmt,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use hyper_util::client::legacy::connect::{
    dns::{GaiAddrs, GaiResolver, Name},
    HttpConnector,
};
use metrics::Counter;
use tower::Service;

/// An [`HttpConnector`] that uses [`SystemResolver`].
pub type SystemHttpConnector = HttpConnector<SystemResolver>;

/// A DNS resolver for `hyper` backed by the operating system resolver.
///
/// Lookups are handled in the default Tokio blocking thread pool as calls to `getaddrinfo` are synchronous.
#[derive(Clone)]
pub struct SystemResolver {
    inner: GaiResolver,
    lookup_errors: Option<Counter>,
}

impl SystemResolver {
    /// Creates a new [`SystemResolver`].
    pub fn new() -> Self {
        Self {
            inner: GaiResolver::new(),
            lookup_errors: None,
        }
    }

    /// Sets a counter that's incremented when DNS lookup fails.
    pub fn with_lookup_errors_counter(mut self, counter: Counter) -> Self {
        self.lookup_errors = Some(counter);
        self
    }

    /// Consumes `self` and creates a new [`SystemHttpConnector`] with this resolver.
    pub fn into_http_connector(self) -> SystemHttpConnector {
        SystemHttpConnector::new_with_resolver(self)
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// An error returned when a DNS lookup fails.
#[derive(Debug)]
pub struct DnsError(io::Error);

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to resolve host: {}", self.0)
    }
}

impl std::error::Error for DnsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl Service<Name> for SystemResolver {
    type Response = GaiAddrs;
    type Error = DnsError;

    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `GaiResolver` is stateless and always ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let fut = self.inner.call(name);
        let lookup_errors = self.lookup_errors.clone();

        Box::pin(async move {
            match fut.await {
                Ok(addrs) => Ok(addrs),
                Err(error) => {
                    if let Some(lookup_errors) = lookup_errors {
                        lookup_errors.increment(1);
                    }
                    Err(DnsError(error))
                }
            }
        })
    }
}
