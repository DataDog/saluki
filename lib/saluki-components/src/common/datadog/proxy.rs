use headers::Authorization;
use hyper_http_proxy::{Intercept, Proxy};
use saluki_error::GenericError;
use serde::{Deserialize, Deserializer};
use url::Url;

#[derive(Clone, Deserialize)]
#[allow(dead_code)]
pub struct ProxyConfiguration {
    /// The proxy server for HTTP requests.
    #[serde(rename = "proxy_http")]
    http_server: Option<String>,

    /// The proxy server for HTTPS requests.
    #[serde(rename = "proxy_https")]
    https_server: Option<String>,

    /// List of hosts that should bypass the proxy.
    ///
    /// In YAML this is a sequence under `proxy.no_proxy`. As an environment variable (`DD_PROXY_NO_PROXY`),
    /// values are space-separated.
    #[serde(default, rename = "proxy_no_proxy", deserialize_with = "deserialize_space_separated_or_seq")]
    no_proxy: Vec<String>,

    /// When true, hostname matching for `no_proxy` uses substring matching rather than exact matching.
    #[serde(default)]
    no_proxy_nonexact_match: bool,

    /// When true, proxy settings apply to requests for cloud provider metadata endpoints.
    #[serde(default)]
    use_proxy_for_cloud_metadata: bool,
}

impl ProxyConfiguration {
    /// Builds the configured proxies.
    ///
    /// # Errors
    ///
    /// If the configured proxy URLs are invalid, an error is returned.
    pub fn build(&self) -> Result<Vec<Proxy>, GenericError> {
        let mut proxies = Vec::new();
        if let Some(url) = &self.http_server {
            proxies.push(new_proxy(url, Intercept::Http)?);
        }
        if let Some(url) = &self.https_server {
            proxies.push(new_proxy(url, Intercept::Https)?);
        }
        Ok(proxies)
    }

    /// Returns the list of hosts that should bypass the proxy.
    #[allow(dead_code)]
    pub fn no_proxy(&self) -> &[String] {
        &self.no_proxy
    }

    /// Returns whether `no_proxy` hostname matching uses substring matching rather than exact matching.
    #[allow(dead_code)]
    pub fn no_proxy_nonexact_match(&self) -> bool {
        self.no_proxy_nonexact_match
    }

    /// Returns whether proxy settings apply to cloud provider metadata endpoint requests.
    #[allow(dead_code)]
    pub fn use_proxy_for_cloud_metadata(&self) -> bool {
        self.use_proxy_for_cloud_metadata
    }
}

fn new_proxy(proxy_url: &str, intercept: Intercept) -> Result<Proxy, GenericError> {
    let url = Url::parse(proxy_url)?;
    let mut proxy = Proxy::new(intercept, url.as_str().parse()?);
    if let Some(password) = url.password() {
        let username = url.username();
        proxy.set_authorization(Authorization::basic(username, password));
    }
    Ok(proxy)
}

/// Deserializes a `Vec<String>` from either a sequence or a space-separated string.
///
/// This handles the dual representation of `no_proxy`: a YAML sequence (`proxy.no_proxy`) and an
/// environment variable (`DD_PROXY_NO_PROXY`) where values are space-separated.
fn deserialize_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    use std::fmt;

    struct SpaceSeparatedOrSeq;

    impl<'de> Visitor<'de> for SpaceSeparatedOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence or a space-separated string")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(v.split_whitespace().map(str::to_owned).collect())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut values = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                values.push(v);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(SpaceSeparatedOrSeq)
}
