//! TLS helpers for client- and server-side IPC usage.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    version::TLS13,
    CertificateError, ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme,
};
use rustls_pki_types::{pem::PemObject as _, PrivateKeyDer};
use saluki_error::{generic_error, ErrorContext as _, GenericError};

const DEFAULT_CERT_READ_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CERT_READ_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct DatadogAgentServerCertVerifier {
    cert: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
}

impl DatadogAgentServerCertVerifier {
    fn from_certificate_and_provider(cert: CertificateDer<'static>, provider: Arc<CryptoProvider>) -> Self {
        Self { cert, provider }
    }
}

impl ServerCertVerifier for DatadogAgentServerCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _server_name: &ServerName<'_>,
        _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // We only care about if the server certificate matches the one we have.
        //
        // This explicitly ignores things like the server using a CA certificate as an end-entity certificate and all of
        // that. We just want to verify that the server certificate is the one we expect.
        if end_entity != &self.cert {
            return Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer));
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Builds a client TLS configuration suitable for IPC usage with the Datadog Agent.
///
/// All IPC for the Datadog Agent uses mutual TLS, where both client _and_ server verify each other's certificate, but
/// crucially, use the _same_ certificate on both sides.
///
/// ## Errors
///
/// If there is an issue reading the IPC TLS certificate file, or if the file isn't a valid PEM-encoded certificate, an
/// error is returned.
pub async fn build_ipc_client_ipc_tls_config<P: AsRef<Path>>(cert_path: P) -> Result<ClientConfig, GenericError> {
    // Read the certificate file, and extract the certificate and private key from it.
    let (parsed_cert, parsed_key) = read_and_parse_certificate_file(
        cert_path.as_ref(),
        DEFAULT_CERT_READ_TIMEOUT,
        DEFAULT_CERT_READ_INTERVAL,
    )
    .await?;

    // Create our custom certificate verifier to use the parsed certificate for server verification.
    let crypto_provider = rustls::crypto::CryptoProvider::get_default()
        .map(Arc::clone)
        .ok_or_else(|| generic_error!("Default cryptography provider not yet installed."))?;
    let agent_cert_verifier = Arc::new(DatadogAgentServerCertVerifier::from_certificate_and_provider(
        parsed_cert.clone(),
        crypto_provider,
    ));

    ClientConfig::builder_with_protocol_versions(&[&TLS13])
        .dangerous()
        .with_custom_certificate_verifier(agent_cert_verifier)
        .with_client_auth_cert(vec![parsed_cert], parsed_key)
        .with_error_context(|| {
            format!(
                "Failed to build client TLS configuration from certificate file '{}'.",
                cert_path.as_ref().display()
            )
        })
}

/// Builds a server TLS configuration suitable for IPC usage with the Datadog Agent.
///
/// All IPC for the Datadog Agent uses mutual TLS, where both client _and_ server verify each other's certificate, but
/// crucially, use the _same_ certificate on both sides.
///
/// ## Errors
///
/// If there is an issue reading the IPC TLS certificate file, or if the file isn't a valid PEM-encoded certificate, an
/// error is returned.
pub async fn build_ipc_server_tls_config<P: AsRef<Path>>(cert_path: P) -> Result<ServerConfig, GenericError> {
    // Read the certificate file, and extract the certificate and private key from it.
    let (parsed_cert, parsed_key) = read_and_parse_certificate_file(
        cert_path.as_ref(),
        DEFAULT_CERT_READ_TIMEOUT,
        DEFAULT_CERT_READ_INTERVAL,
    )
    .await?;

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![parsed_cert], parsed_key)
        .with_error_context(|| {
            format!(
                "Failed to build server TLS configuration from certificate file '{}'.",
                cert_path.as_ref().display()
            )
        })
}

/// Reads and parses a certificate file from the given path with retry behavior.
///
/// If reading the file fails, it will retry reading it for up to `timeout` total, waiting `interval` between attempts,
/// until it succeeds or the timeout is reached.
///
/// ## Errors
///
/// If the file can't be read after the maximum number of retries, or if the file isn't a valid certificate,
/// an error will be returned.
async fn read_and_parse_certificate_file(
    cert_path: &Path, timeout: Duration, interval: Duration,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), GenericError> {
    if timeout < interval {
        return Err(generic_error!(
            "Timeout is less than interval ({} <  {}).",
            timeout.as_secs(),
            interval.as_secs()
        ));
    }

    let start_time = Instant::now();
    let mut last_error = String::new();
    while start_time.elapsed() < timeout {
        match tokio::fs::read(cert_path).await {
            Ok(raw_cert_data) => {
                let parsed_cert = CertificateDer::from_pem_slice(&raw_cert_data[..])
                    .with_error_context(|| format!("Failed to parse certificate file '{}'.", cert_path.display()))?
                    .into_owned();

                let parsed_key = PrivateKeyDer::from_pem_slice(&raw_cert_data[..])
                    .with_error_context(|| format!("Failed to parse private key file '{}'.", cert_path.display()))?
                    .clone_key();

                return Ok((parsed_cert, parsed_key));
            }
            Err(e) => {
                last_error = e.to_string();
                tokio::time::sleep(interval).await;
            }
        }
    }

    Err(generic_error!(
        "Failed to read certificate file '{}' after {} seconds: {}",
        cert_path.display(),
        timeout.as_secs(),
        last_error
    ))
}
