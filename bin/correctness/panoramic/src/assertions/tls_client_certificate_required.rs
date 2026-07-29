use std::time::{Duration, Instant};

use reqwest::ClientBuilder;
use rustls::{AlertDescription, Error as RustlsError};

use crate::assertions::{http_check::resolve_endpoint, Assertion, AssertionContext, AssertionResult};

const INVALID_ENDPOINT: &str = "<invalid endpoint>";

fn sanitized_endpoint(endpoint: &str) -> String {
    let Ok(mut endpoint) = reqwest::Url::parse(endpoint) else {
        return INVALID_ENDPOINT.to_string();
    };
    if endpoint.host().is_none() || endpoint.set_username("").is_err() || endpoint.set_password(None).is_err() {
        return INVALID_ENDPOINT.to_string();
    }

    endpoint.set_query(None);
    endpoint.set_fragment(None);
    endpoint.to_string()
}

/// Assertion that verifies an HTTPS endpoint requires TLS client-certificate authentication.
pub struct TlsClientCertificateRequiredAssertion {
    endpoint: String,
    timeout: Duration,
}

impl TlsClientCertificateRequiredAssertion {
    /// Creates an assertion for an HTTPS endpoint and an overall request timeout.
    pub fn new(endpoint: String, timeout: Duration) -> Self {
        Self { endpoint, timeout }
    }

    fn result(&self, started: Instant, passed: bool, message: String) -> AssertionResult {
        AssertionResult {
            name: self.name().to_string(),
            passed,
            message,
            duration: started.elapsed(),
        }
    }
}

#[async_trait::async_trait]
impl Assertion for TlsClientCertificateRequiredAssertion {
    fn name(&self) -> &'static str {
        "tls_client_certificate_required"
    }

    fn description(&self) -> String {
        format!(
            "HTTPS endpoint '{}' rejects an anonymous client during TLS client-certificate authentication.",
            sanitized_endpoint(&self.endpoint)
        )
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();

        if ctx.target_is_windows() {
            return self.result(
                started,
                false,
                "The tls_client_certificate_required assertion uses a host-side HTTPS probe and is not supported for Windows container targets."
                    .to_string(),
            );
        }

        let configured_endpoint = match reqwest::Url::parse(&self.endpoint) {
            Ok(endpoint) if endpoint.host().is_some() => endpoint,
            _ => {
                return self.result(
                    started,
                    false,
                    "TLS client-certificate authentication cannot be checked because the configured endpoint is malformed; expected an absolute HTTPS URL."
                        .to_string(),
                );
            }
        };

        if configured_endpoint.scheme() != "https" {
            return self.result(
                started,
                false,
                format!(
                    "TLS client-certificate authentication can only be checked with an https:// endpoint; configured endpoint was '{}'.",
                    sanitized_endpoint(&self.endpoint)
                ),
            );
        }

        let endpoint = resolve_endpoint(&self.endpoint, ctx);
        let endpoint_display = sanitized_endpoint(&endpoint);
        let client = match ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.timeout)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                return self.result(
                    started,
                    false,
                    format!(
                        "Failed to build anonymous HTTPS client for '{}': {}.",
                        endpoint_display, error
                    ),
                );
            }
        };

        let request = client.get(&endpoint).send();
        tokio::pin!(request);

        tokio::select! {
            _ = ctx.cancel_token.cancelled() => self.result(
                started,
                false,
                format!(
                    "TLS client-certificate authentication check for '{}' was cancelled.",
                    endpoint_display
                ),
            ),
            _ = ctx.container_exit_token.cancelled() => self.result(
                started,
                false,
                format!(
                    "TLS client-certificate authentication check for '{}' stopped because the target exited.",
                    endpoint_display
                ),
            ),
            _ = tokio::time::sleep(self.timeout) => self.result(
                started,
                false,
                format!(
                    "Anonymous HTTPS request to '{}' did not produce a TLS client-certificate authentication rejection within {:?}.",
                    endpoint_display, self.timeout
                ),
            ),
            result = &mut request => match result {
                Ok(response) => self.result(
                    started,
                    false,
                    format!(
                        "Anonymous HTTPS request to '{}' returned HTTP status {}; the endpoint did not require a client certificate during the TLS handshake.",
                        endpoint_display,
                        response.status().as_u16()
                    ),
                ),
                Err(error) if is_certificate_required_alert(&error) => self.result(
                    started,
                    true,
                    format!(
                        "HTTPS endpoint '{}' rejected the anonymous client with the TLS CertificateRequired alert.",
                        endpoint_display
                    ),
                ),
                Err(error) => self.result(
                    started,
                    false,
                    format!(
                        "Anonymous HTTPS request to '{}' failed, but not because TLS client-certificate authentication was required: {}.",
                        endpoint_display,
                        error_chain(error)
                    ),
                ),
            },
        }
    }
}

fn error_chain(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut messages = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(source) = current {
        messages.push(source.to_string());
        current = source.source();
    }
    messages.dedup();
    messages.join(": ")
}

fn is_certificate_required_alert(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(source) = current {
        if rustls_error_is_certificate_required(source.downcast_ref::<RustlsError>()) {
            return true;
        }
        if let Some(io_error) = source.downcast_ref::<std::io::Error>() {
            if rustls_error_is_certificate_required(
                io_error.get_ref().and_then(|inner| inner.downcast_ref::<RustlsError>()),
            ) {
                return true;
            }
        }
        current = source.source();
    }

    false
}

fn rustls_error_is_certificate_required(error: Option<&RustlsError>) -> bool {
    matches!(
        error,
        Some(RustlsError::AlertReceived(AlertDescription::CertificateRequired))
    )
}
