use std::sync::Arc;

use headers::Authorization;
use hyper_http_proxy::{Custom, Intercept, Proxy};
use saluki_error::GenericError;
use serde::Deserialize;
use url::Url;

#[derive(Clone, Deserialize)]
pub struct ProxyConfiguration {
    /// The proxy server for HTTP requests.
    #[serde(rename = "proxy_http")]
    http_server: Option<String>,

    /// The proxy server for HTTPS requests.
    #[serde(rename = "proxy_https")]
    https_server: Option<String>,

    /// Optional list of hosts or CIDR ranges to bypass the proxy.
    #[serde(rename = "proxy_no_proxy")]
    no_proxy: Option<Vec<String>>,
}

impl ProxyConfiguration {
    /// Builds the configured proxies.
    ///
    /// # Errors
    ///
    /// If the configured proxy URLs are invalid, an error is returned.
    pub fn build(&self) -> Result<Vec<Proxy>, GenericError> {
        let no_proxy_hosts: Arc<Vec<String>> = Arc::new(self.no_proxy.clone().unwrap_or_default());

        let mut proxies = Vec::new();
        if let Some(url) = &self.https_server {
            proxies.push(new_proxy(url, Intercept::Https, no_proxy_hosts.clone())?);
        }
        if let Some(url) = &self.http_server {
            proxies.push(new_proxy(url, Intercept::Http, no_proxy_hosts)?);
        }
        Ok(proxies)
    }
}

fn new_proxy(
    proxy_url: &str, intercept_scheme: Intercept, no_proxy_hosts: Arc<Vec<String>>,
) -> Result<Proxy, GenericError> {
    let url = Url::parse(proxy_url)?;

    let intercept = if no_proxy_hosts.is_empty() {
        intercept_scheme
    } else {
        let interceptor = move |_scheme: Option<&str>, host: Option<&str>, _port: Option<u16>| {
            if let Some(host) = host {
                if no_proxy_hosts.iter().any(|r| r == host) {
                    return false; // Bypass the proxy.
                }
            }

            // Otherwise, proxy based on the original scheme.
            match intercept_scheme {
                Intercept::Http => _scheme == Some("http"),
                Intercept::Https => _scheme == Some("https"),
                _ => false, // Should not happen with current usage.
            }
        };
        Intercept::Custom(Custom::from(interceptor))
    };

    let mut proxy = Proxy::new(intercept, url.as_str().parse()?);
    if let Some(password) = url.password() {
        let username = url.username();
        proxy.set_authorization(Authorization::basic(username, password));
    }
    Ok(proxy)
}
