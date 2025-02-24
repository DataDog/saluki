use headers::Authorization;
use hyper_proxy2::{Intercept, Proxy};
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
}

impl ProxyConfiguration {
    /// Creates the list of proxies.
    pub fn build(&self) -> Result<Vec<Proxy>, GenericError> {
        let mut proxies = Vec::new();
        if let Some(url) = &self.http_server {
            proxies.push(self.new_proxy(url, Intercept::Http)?);
        }
        if let Some(url) = &self.https_server {
            proxies.push(self.new_proxy(url, Intercept::Https)?);
        }
        Ok(proxies)
    }

    fn new_proxy(&self, proxy_url: &str, intercept: Intercept) -> Result<Proxy, GenericError> {
        let url = Url::parse(proxy_url)?;
        let mut proxy = Proxy::new(intercept, url.as_str().parse()?);
        if let Some(password) = url.password() {
            let username = url.username();
            proxy.set_authorization(Authorization::basic(username, password));
        }
        Ok(proxy)
    }
}
