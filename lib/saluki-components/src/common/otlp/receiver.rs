//! OTLP receiver settings, shared by the OTLP source and relay.

use std::time::Duration;

use saluki_error::{generic_error, GenericError};
use saluki_io::net::server::http::Http2Config;

use super::{CorsConfiguration, OtlpTlsConfiguration};

/// Settings of the OTLP receiver's gRPC and HTTP servers.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpReceiver {
    /// Whether OTLP logs are accepted.
    pub logs_enabled: bool,

    /// Whether OTLP metrics are accepted.
    pub metrics_enabled: bool,

    /// gRPC server settings.
    pub grpc: OtlpGrpcReceiver,

    /// HTTP server settings.
    pub http: OtlpHttpReceiver,
}

/// Settings of the OTLP receiver's gRPC server.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpGrpcReceiver {
    /// Address the server listens on, without a scheme.
    pub endpoint: String,

    /// Maximum size of a received message, in MiB.
    pub max_recv_msg_size_mib: u64,

    /// Transport the server listens with.
    pub transport: GrpcTransport,

    /// Maximum number of concurrent streams per connection. If set to `0`, the number is not limited.
    pub max_concurrent_streams: u32,

    /// Keepalive and connection age settings.
    pub keepalive: OtlpKeepalive,

    /// TLS settings.
    pub tls: OtlpReceiverTls,
}

/// Transport of the OTLP receiver's gRPC server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrpcTransport {
    /// TCP socket.
    Tcp,

    /// Unix domain socket.
    Unix,
}

impl GrpcTransport {
    /// Returns the listen address scheme of this transport.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Unix => "unix",
        }
    }
}

/// Settings of the OTLP receiver's HTTP server.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpHttpReceiver {
    /// Address the server listens on, without a scheme.
    pub endpoint: String,

    /// Listen address scheme the relay uses with `endpoint`, such as `tcp`.
    pub transport: String,

    /// CORS settings.
    pub cors: CorsConfiguration,

    /// TLS settings.
    pub tls: OtlpReceiverTls,

    /// Maximum size of a request body, in bytes. If set to `0`, a default of 20 MiB applies.
    pub max_request_body_size: u64,
}

/// Keepalive and connection age settings of the OTLP receiver's gRPC server.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpKeepalive {
    /// Maximum age of a connection. If set to zero, connection age is not limited.
    pub max_connection_age: Duration,

    /// Grace period after `max_connection_age` for in-flight requests. If set to zero, the grace period is not
    /// limited.
    pub max_connection_age_grace: Duration,

    /// Interval between keepalive pings.
    pub time: Duration,

    /// Time to wait for a keepalive ping acknowledgement.
    pub timeout: Duration,
}

/// TLS file paths of an OTLP receiver server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtlpReceiverTls {
    /// Server certificate file. Empty if unset.
    pub cert_file: String,

    /// Server private key file. Empty if unset.
    pub key_file: String,

    /// CA certificate file used to verify client certificates. Empty if unset.
    pub ca_file: String,
}

impl OtlpReceiverTls {
    /// Builds a server TLS configuration, if TLS is enabled.
    ///
    /// TLS is enabled when both `cert_file` and `key_file` are non-empty. When `ca_file` is also non-empty, the server
    /// requests client certificates and verifies them against the CA certificates in that file, but does not require a
    /// client certificate (optional verification).
    ///
    /// # Errors
    ///
    /// Returns an error if any TLS field is set without the others required to form a valid TLS configuration. Both
    /// `cert_file` and `key_file` must be provided together to enable TLS, and `ca_file` must not be set without them.
    /// Setting only a subset is treated as a configuration error rather than silently downgrading to plaintext.
    pub(crate) fn build(&self) -> Result<Option<OtlpTlsConfiguration>, GenericError> {
        match (self.cert_file.is_empty(), self.key_file.is_empty()) {
            (true, true) => {
                if !self.ca_file.is_empty() {
                    Err(generic_error!(
                        "OTLP receiver TLS `ca_file` is set but `cert_file` and `key_file` are empty. All three must \
                         be provided together, or `ca_file` must be omitted when TLS is disabled."
                    ))
                } else {
                    Ok(None)
                }
            }
            (false, false) => {
                let mut config = OtlpTlsConfiguration::new(self.cert_file.clone().into(), self.key_file.clone().into());
                if !self.ca_file.is_empty() {
                    config = config.with_ca_file(self.ca_file.clone().into());
                }
                Ok(Some(config))
            }
            (true, false) => Err(generic_error!(
                "OTLP receiver TLS `key_file` is set but `cert_file` is empty. Both must be provided to enable TLS."
            )),
            (false, true) => Err(generic_error!(
                "OTLP receiver TLS `cert_file` is set but `key_file` is empty. Both must be provided to enable TLS."
            )),
        }
    }
}

impl OtlpGrpcReceiver {
    /// Converts the connection settings into HTTP/2 server settings.
    ///
    /// The keepalive interval and timeout map across directly. The connection age limits and `max_concurrent_streams`
    /// use a zero value to mean "no limit," which is translated here into the absence of a limit.
    pub(crate) fn http2_config(&self) -> Http2Config {
        let keepalive = &self.keepalive;
        let mut config = Http2Config::default().with_keepalive(keepalive.time, keepalive.timeout);

        if !keepalive.max_connection_age.is_zero() {
            let grace = (!keepalive.max_connection_age_grace.is_zero()).then_some(keepalive.max_connection_age_grace);
            config = config.with_max_connection_age(keepalive.max_connection_age, grace);
        }

        if self.max_concurrent_streams > 0 {
            config = config.with_max_concurrent_streams(self.max_concurrent_streams);
        }

        config
    }
}

/// Returns receiver settings with every server unset and no signal enabled.
#[cfg(test)]
pub(crate) fn receiver() -> OtlpReceiver {
    let tls = OtlpReceiverTls {
        cert_file: String::new(),
        key_file: String::new(),
        ca_file: String::new(),
    };
    OtlpReceiver {
        logs_enabled: false,
        metrics_enabled: false,
        grpc: OtlpGrpcReceiver {
            endpoint: String::new(),
            max_recv_msg_size_mib: 0,
            transport: GrpcTransport::Tcp,
            max_concurrent_streams: 0,
            keepalive: OtlpKeepalive {
                max_connection_age: Duration::ZERO,
                max_connection_age_grace: Duration::ZERO,
                time: Duration::ZERO,
                timeout: Duration::ZERO,
            },
            tls: tls.clone(),
        },
        http: OtlpHttpReceiver {
            endpoint: String::new(),
            transport: "tcp".to_string(),
            cors: CorsConfiguration::default(),
            tls,
            max_request_body_size: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tls(cert_file: &str, key_file: &str, ca_file: &str) -> OtlpReceiverTls {
        OtlpReceiverTls {
            cert_file: cert_file.to_string(),
            key_file: key_file.to_string(),
            ca_file: ca_file.to_string(),
        }
    }

    #[test]
    fn tls_is_enabled_by_a_certificate_and_key_with_an_optional_ca() {
        assert!(tls("", "", "").build().expect("unset TLS is valid").is_none());

        let config = tls("/cert.pem", "/key.pem", "")
            .build()
            .expect("a certificate and key are valid")
            .expect("a certificate and key enable TLS");
        assert_eq!(PathBuf::from("/cert.pem"), config.cert_file);
        assert_eq!(PathBuf::from("/key.pem"), config.key_file);
        assert_eq!(None, config.ca_file);

        let config = tls("/cert.pem", "/key.pem", "/ca.pem")
            .build()
            .expect("a certificate, key, and CA are valid")
            .expect("a certificate and key enable TLS");
        assert_eq!(Some(PathBuf::from("/ca.pem")), config.ca_file);
    }

    #[test]
    fn partial_tls_settings_are_rejected() {
        for (cert_file, key_file, ca_file, expected) in [
            ("", "", "/ca.pem", "`ca_file` is set"),
            ("", "/key.pem", "", "`key_file` is set"),
            ("", "/key.pem", "/ca.pem", "`key_file` is set"),
            ("/cert.pem", "", "", "`cert_file` is set"),
            ("/cert.pem", "", "/ca.pem", "`cert_file` is set"),
        ] {
            let error = tls(cert_file, key_file, ca_file)
                .build()
                .expect_err("partial TLS settings must not downgrade to plaintext");
            assert!(
                error.to_string().contains(expected),
                "{cert_file:?} {key_file:?} {ca_file:?}: {error}"
            );
        }
    }
}
