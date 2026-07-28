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
    server::danger::{ClientCertVerified, ClientCertVerifier},
    version::TLS13,
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use rustls_pki_types::{pem::PemObject as _, PrivateKeyDer};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_tls::{ensure_client_config_fips_compliant, ensure_server_config_fips_compliant};

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

#[derive(Debug)]
struct DatadogAgentClientCertVerifier {
    cert: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
}

impl DatadogAgentClientCertVerifier {
    fn from_certificate_and_provider(cert: CertificateDer<'static>, provider: Arc<CryptoProvider>) -> Self {
        Self { cert, provider }
    }
}

impl ClientCertVerifier for DatadogAgentClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if end_entity != &self.cert {
            return Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer));
        }

        Ok(ClientCertVerified::assertion())
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

    let config = ClientConfig::builder_with_protocol_versions(&[&TLS13])
        .dangerous()
        .with_custom_certificate_verifier(agent_cert_verifier)
        .with_client_auth_cert(vec![parsed_cert], parsed_key)
        .with_error_context(|| {
            format!(
                "Failed to build client TLS configuration from certificate file '{}'.",
                cert_path.as_ref().display()
            )
        })?;

    ensure_client_config_fips_compliant(&config)?;

    Ok(config)
}

/// Builds a server TLS configuration suitable for IPC usage with the Datadog Agent.
///
/// The server requires every client to present a leaf certificate whose DER encoding exactly matches the configured IPC
/// certificate. This is exact-certificate mutual TLS rather than CA-based trust: the client and server identities must
/// be coordinated to use the same certificate, and no overlap between different certificates is accepted.
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

    let crypto_provider = rustls::crypto::CryptoProvider::get_default()
        .map(Arc::clone)
        .ok_or_else(|| generic_error!("Default cryptography provider not yet installed."))?;
    let agent_cert_verifier = Arc::new(DatadogAgentClientCertVerifier::from_certificate_and_provider(
        parsed_cert.clone(),
        crypto_provider,
    ));

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(agent_cert_verifier)
        .with_single_cert(vec![parsed_cert], parsed_key)
        .with_error_context(|| {
            format!(
                "Failed to build server TLS configuration from certificate file '{}'.",
                cert_path.as_ref().display()
            )
        })?;

    ensure_server_config_fips_compliant(&mut config)?;

    Ok(config)
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
        version::TLS13,
        ClientConfig, ServerConfig,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::{build_ipc_client_ipc_tls_config, build_ipc_server_tls_config, DatadogAgentServerCertVerifier};

    const REQUEST: &[u8] = b"GET /ipc HTTP/1.1\r\nHost: localhost\r\n\r\n";
    const RESPONSE: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    struct TestIdentity {
        _temp_dir: TempDir,
        cert_path: PathBuf,
        cert_der: CertificateDer<'static>,
        key_der: Vec<u8>,
    }

    impl TestIdentity {
        fn localhost() -> Self {
            let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()])
                .expect("self-signed localhost certificate should be generated");
            let temp_dir = tempfile::tempdir().expect("temporary certificate directory should be created");
            let cert_path = temp_dir.path().join("ipc-cert.pem");
            fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
                .expect("certificate and private key should be written");

            Self {
                _temp_dir: temp_dir,
                cert_path,
                cert_der: cert.der().clone(),
                key_der: signing_key.serialize_der(),
            }
        }

        fn private_key(&self) -> PrivateKeyDer<'static> {
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
        }
    }

    #[derive(Debug)]
    enum ClientOutcome {
        HandshakeRejected,
        RequestRejected,
        Response(Vec<u8>),
    }

    #[derive(Debug)]
    enum ServerOutcome {
        HandshakeRejected,
        RequestReadFailed,
        RequestHandled(Vec<u8>),
    }

    #[derive(Debug)]
    struct ExchangeOutcome {
        client: ClientOutcome,
        server: ServerOutcome,
        request_reached_handler: bool,
    }

    fn initialize_crypto_provider() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        assert!(
            CryptoProvider::get_default().is_some(),
            "default crypto provider should be installed"
        );
    }

    fn client_config_pinned_to(server_identity: &TestIdentity, client_identity: Option<&TestIdentity>) -> ClientConfig {
        let provider = CryptoProvider::get_default()
            .cloned()
            .expect("default crypto provider should be installed");
        let verifier = Arc::new(DatadogAgentServerCertVerifier::from_certificate_and_provider(
            server_identity.cert_der.clone(),
            provider,
        ));
        let builder = ClientConfig::builder_with_protocol_versions(&[&TLS13])
            .dangerous()
            .with_custom_certificate_verifier(verifier);

        match client_identity {
            Some(identity) => builder
                .with_client_auth_cert(vec![identity.cert_der.clone()], identity.private_key())
                .expect("client TLS config should accept generated identity"),
            None => builder.with_no_client_auth(),
        }
    }

    async fn exchange(server_config: ServerConfig, client_config: ClientConfig) -> ExchangeOutcome {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let request_reached_handler = Arc::new(AtomicBool::new(false));
        let server_request_reached_handler = Arc::clone(&request_reached_handler);

        let server_task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let mut stream = match acceptor.accept(server_io).await {
                Ok(stream) => stream,
                Err(_) => return ServerOutcome::HandshakeRejected,
            };

            let mut request = vec![0; REQUEST.len()];
            if stream.read_exact(&mut request).await.is_err() {
                return ServerOutcome::RequestReadFailed;
            }
            server_request_reached_handler.store(true, Ordering::SeqCst);

            if stream.write_all(RESPONSE).await.is_err() {
                return ServerOutcome::RequestReadFailed;
            }

            ServerOutcome::RequestHandled(request)
        });

        let client = tokio::time::timeout(Duration::from_secs(5), async move {
            let connector = TlsConnector::from(Arc::new(client_config));
            let server_name = ServerName::try_from("localhost").expect("localhost should be a valid server name");
            let mut stream = match connector.connect(server_name, client_io).await {
                Ok(stream) => stream,
                Err(_) => return ClientOutcome::HandshakeRejected,
            };

            if stream.write_all(REQUEST).await.is_err() {
                return ClientOutcome::RequestRejected;
            }
            if stream.flush().await.is_err() {
                return ClientOutcome::RequestRejected;
            }

            let mut response = vec![0; RESPONSE.len()];
            match stream.read_exact(&mut response).await {
                Ok(_) => ClientOutcome::Response(response),
                Err(_) => ClientOutcome::RequestRejected,
            }
        })
        .await
        .expect("client TLS exchange should not time out");

        let server = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server TLS exchange should not time out")
            .expect("server task should not panic");

        ExchangeOutcome {
            client,
            server,
            request_reached_handler: request_reached_handler.load(Ordering::SeqCst),
        }
    }

    #[tokio::test]
    async fn matching_ipc_identity_completes_mtls_and_delivers_request() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = build_ipc_client_ipc_tls_config(&identity_a.cert_path)
            .await
            .expect("production client TLS config should build");

        let outcome = exchange(server_config, client_config).await;

        assert!(
            matches!(&outcome.client, ClientOutcome::Response(response) if response == RESPONSE),
            "matching client should receive the response: {outcome:?}"
        );
        assert!(
            matches!(&outcome.server, ServerOutcome::RequestHandled(request) if request == REQUEST),
            "matching client request should reach the handler: {outcome:?}"
        );
        assert!(outcome.request_reached_handler);
    }

    #[tokio::test]
    async fn missing_client_certificate_is_rejected_before_request_handler() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = client_config_pinned_to(&identity_a, None);

        let outcome = exchange(server_config, client_config).await;

        assert!(
            matches!(outcome.server, ServerOutcome::HandshakeRejected),
            "server should reject a missing client identity during the handshake: {outcome:?}"
        );
        assert!(!outcome.request_reached_handler, "request must not reach the handler");
        assert!(
            !matches!(outcome.client, ClientOutcome::Response(_)),
            "anonymous client must not receive an application response: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn mismatched_client_certificate_is_rejected_before_request_handler() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let identity_b = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = client_config_pinned_to(&identity_a, Some(&identity_b));

        let outcome = exchange(server_config, client_config).await;

        assert!(
            matches!(outcome.server, ServerOutcome::HandshakeRejected),
            "server should reject a different client identity during the handshake: {outcome:?}"
        );
        assert!(!outcome.request_reached_handler, "request must not reach the handler");
        assert!(
            !matches!(outcome.client, ClientOutcome::Response(_)),
            "mismatched client must not receive an application response: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn production_client_rejects_wrong_server_identity() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let identity_b = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_b.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = build_ipc_client_ipc_tls_config(&identity_a.cert_path)
            .await
            .expect("production client TLS config should build");

        let outcome = exchange(server_config, client_config).await;

        assert!(
            matches!(outcome.client, ClientOutcome::HandshakeRejected),
            "production client pinned to A should reject server B: {outcome:?}"
        );
        assert!(!outcome.request_reached_handler, "request must not reach the handler");
    }
}
