use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use headers::Authorization;
use hyper_http_proxy::{Custom, Intercept, Proxy};
use ipnet::IpNet;
use saluki_error::GenericError;
use serde::Deserialize;
use url::Url;
use wildcard::Wildcard;

#[derive(Clone, Deserialize)]
pub struct ProxyConfiguration {
    /// The proxy server for HTTP requests.
    #[serde(rename = "proxy_http")]
    http_server: Option<String>,

    /// The proxy server for HTTPS requests.
    #[serde(rename = "proxy_https")]
    https_server: Option<String>,

    /// Optional list of hosts or CIDR ranges to bypass the proxy.
    #[serde(default)]
    no_proxy: Option<Vec<String>>,
}

enum Bypass {
    Domain(String),
    IpNet(IpNet),
}

impl Bypass {
    fn matches(&self, host: &str) -> bool {
        match self {
            Bypass::Domain(pattern) => Wildcard::new(pattern.as_bytes()).map_or(false, |w| w.is_match(host.as_bytes())),
            Bypass::IpNet(n) => {
                if let Ok(addr) = host.parse::<IpAddr>() {
                    n.contains(&addr)
                } else {
                    false
                }
            }
        }
    }
}

impl ProxyConfiguration {
    /// Builds the configured proxies.
    ///
    /// # Errors
    ///
    /// If the configured proxy URLs are invalid, an error is returned.
    pub fn build(&self) -> Result<Vec<Proxy>, GenericError> {
        let bypass_rules: Arc<Vec<Bypass>> = Arc::new(
            self.no_proxy
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|p| {
                    if let Ok(net) = IpNet::from_str(p) {
                        Bypass::IpNet(net)
                    } else {
                        let pattern = if p.starts_with('.') {
                            format!("*{}", p)
                        } else {
                            p.to_string()
                        };
                        Bypass::Domain(pattern)
                    }
                })
                .collect(),
        );

        let mut proxies = Vec::new();
        if let Some(url) = &self.https_server {
            proxies.push(new_proxy(url, Intercept::Https, bypass_rules.clone())?);
        }
        if let Some(url) = &self.http_server {
            proxies.push(new_proxy(url, Intercept::Http, bypass_rules)?);
        }
        Ok(proxies)
    }
}

fn new_proxy(
    proxy_url: &str, intercept_scheme: Intercept, bypass_rules: Arc<Vec<Bypass>>,
) -> Result<Proxy, GenericError> {
    let url = Url::parse(proxy_url)?;

    let intercept = if bypass_rules.is_empty() {
        intercept_scheme
    } else {
        let interceptor = move |_scheme: Option<&str>, host: Option<&str>, _port: Option<u16>| {
            if let Some(host) = host {
                if bypass_rules.iter().any(|r| r.matches(host)) {
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
