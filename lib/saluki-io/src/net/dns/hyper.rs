use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use hickory_resolver::{net::NetError, TokioResolver};
use hyper_util::client::legacy::connect::{dns::Name, HttpConnector};
use metrics::Counter;
use saluki_error::{ErrorContext as _, GenericError};
use tower::Service;

/// An [`HttpConnector`] that uses [`HickoryResolver`].
pub type HickoryHttpConnector = HttpConnector<HickoryResolver>;

/// A DNS resolver for `hyper` based on `hickory`.
#[derive(Clone)]
pub struct HickoryResolver {
    resolver: Arc<TokioResolver>,
    lookup_errors: Option<Counter>,
}

impl HickoryResolver {
    /// Creates a new [`HickoryResolver`] based on the system's resolver configuration.
    ///
    /// # Panics
    ///
    /// This will panic if called outside the context of a Tokio runtime.
    ///
    /// # Errors
    ///
    /// If there is an issue loading the system configuration, an error is returned.
    pub fn from_system_conf() -> Result<Self, GenericError> {
        let resolver = TokioResolver::builder_tokio()
            .error_context("Failed to load the system resolver configuration.")?
            .build()
            .error_context("Failed to build the resolver.")?;

        Ok(Self {
            resolver: Arc::new(resolver),
            lookup_errors: None,
        })
    }

    /// Creates a placeholder [`HickoryResolver`] with no configured nameservers.
    ///
    /// This resolver will never successfully resolve any names. It exists solely as a
    /// placeholder for code paths where DNS resolution is never needed (e.g., vsock
    /// transport), avoiding initialization failures in environments without system DNS
    /// configuration such as Nitro Enclaves.
    pub fn noop() -> Self {
        use hickory_resolver::{config::ResolverConfig, net::runtime::TokioRuntimeProvider};
        let config = ResolverConfig::from_parts(None, vec![], vec![]);
        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .build()
            .expect("noop resolver with empty config should always succeed");
        Self {
            resolver: Arc::new(resolver),
            lookup_errors: None,
        }
    }

    /// Sets a counter that's incremented when DNS lookup fails.
    pub fn with_lookup_errors_counter(mut self, counter: Counter) -> Self {
        self.lookup_errors = Some(counter);
        self
    }

    /// Consumes `self` and creates a new [`HickoryHttpConnector`] with this resolver.
    pub fn into_http_connector(self) -> HickoryHttpConnector {
        HickoryHttpConnector::new_with_resolver(self)
    }
}

impl Service<Name> for HickoryResolver {
    type Response = SocketAddrs;
    type Error = NetError;

    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let resolver = self.resolver.clone();
        let lookup_errors = self.lookup_errors.clone();

        Box::pin(async move {
            let response = match resolver.lookup_ip(name.as_str()).await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(lookup_errors) = lookup_errors {
                        lookup_errors.increment(1);
                    }
                    return Err(error);
                }
            };
            Ok(response.iter().collect())
        })
    }
}

pub struct SocketAddrs(Vec<IpAddr>);

impl Iterator for SocketAddrs {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop().map(|ip| SocketAddr::new(ip, 0))
    }
}

impl FromIterator<IpAddr> for SocketAddrs {
    fn from_iter<I: IntoIterator<Item = IpAddr>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
