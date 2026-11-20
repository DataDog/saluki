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
    version::{TLS12, TLS13},
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
    SupportedProtocolVersion,
};
use rustls_pki_types::{pem::PemObject as _, PrivateKeyDer};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_tls::{ensure_client_config_fips_compliant, ensure_server_config_fips_compliant};

const DEFAULT_CERT_READ_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CERT_READ_INTERVAL: Duration = Duration::from_millis(100);
const IPC_TLS13_ONLY: &[&SupportedProtocolVersion] = &[&TLS13];
const IPC_TLS12_ONLY: &[&SupportedProtocolVersion] = &[&TLS12];

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
        // Exact leaf DER equality pins one server identity; certificate chains and CA semantics do not broaden trust.
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

/// Builds an exact shared-certificate mTLS client configuration for Datadog Agent IPC.
///
/// The client accepts only a server leaf certificate whose DER encoding exactly matches the configured IPC certificate
/// and verifies the handshake signature as proof that the server possesses its private key. The client presents the same
/// certificate and proves possession of its private key to satisfy mandatory server-side client authentication.
/// Certificate chains and CA trust do not broaden the accepted server identity.
///
/// # Errors
///
/// If the IPC TLS identity file cannot be read or does not contain a valid PEM-encoded certificate and private key, an
/// error is returned.
pub async fn build_ipc_client_ipc_tls_config<P: AsRef<Path>>(cert_path: P) -> Result<ClientConfig, GenericError> {
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

    build_ipc_client_tls_config_with_provider(parsed_cert, parsed_key, crypto_provider).with_error_context(|| {
        format!(
            "Failed to build client TLS configuration from certificate file '{}'.",
            cert_path.as_ref().display()
        )
    })
}

/// Builds the IPC client configuration from a parsed identity and an explicit cryptography provider.
fn build_ipc_client_tls_config_with_provider(
    cert: CertificateDer<'static>, key: PrivateKeyDer<'static>, provider: Arc<CryptoProvider>,
) -> Result<ClientConfig, GenericError> {
    // Create our custom certificate verifier to use the parsed certificate for server verification.
    let agent_cert_verifier = Arc::new(DatadogAgentServerCertVerifier::from_certificate_and_provider(
        cert.clone(),
        Arc::clone(&provider),
    ));

    let protocol_versions = ipc_client_protocol_versions(&provider);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(protocol_versions)
        .error_context("Failed to select TLS protocol versions.")?
        .dangerous()
        .with_custom_certificate_verifier(agent_cert_verifier)
        .with_client_auth_cert(vec![cert], key)
        .error_context("Failed to set the client certificate.")?;

    ensure_client_config_fips_compliant(&config)?;

    Ok(config)
}

/// Returns the TLS protocol versions that the IPC client offers with the given cryptography provider.
///
/// The client offers only TLS 1.3 when the provider has a TLS 1.3 cipher suite. Otherwise, it offers only TLS 1.2. For
/// example, the Windows CNG provider in FIPS mode has no TLS 1.3 cipher suites on Windows Server 2016, because CNG
/// there has no HKDF algorithm. Its TLS 1.2 cipher suites use the CNG TLS 1.2 key derivation function, which is
/// available in FIPS mode.
///
/// The TLS 1.2 fallback relies on the Datadog Agent's IPC server accepting TLS 1.2. Its server configuration sets no
/// minimum version, so Go's default minimum of TLS 1.2 applies:
/// <https://github.com/DataDog/datadog-agent/blob/a04e8aa6d8a46856d37cfbe064dd936d1e214701/pkg/api/security/cert/cert_getter.go#L179-L185>
fn ipc_client_protocol_versions(provider: &CryptoProvider) -> &'static [&'static SupportedProtocolVersion] {
    if provider.cipher_suites.iter().any(|suite| suite.tls13().is_some()) {
        IPC_TLS13_ONLY
    } else {
        IPC_TLS12_ONLY
    }
}

/// Builds an exact shared-certificate mTLS server configuration for Datadog Agent IPC.
///
/// The server requires every client to present a leaf certificate whose DER encoding exactly matches the configured IPC
/// certificate and to prove possession of its private key with the handshake signature. The server presents the same
/// certificate as its identity. Certificate chains and CA trust do not broaden the accepted client identity, and no
/// overlap between different certificates is accepted.
///
/// # Errors
///
/// If the IPC TLS identity file cannot be read or does not contain a valid PEM-encoded certificate and private key, an
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
    use std::{fs, io, path::PathBuf, sync::Arc, time::Duration};

    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
        version::{TLS12, TLS13},
        ClientConfig, ProtocolVersion, ServerConfig,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::{
        build_ipc_client_ipc_tls_config, build_ipc_client_tls_config_with_provider, build_ipc_server_tls_config,
        ipc_client_protocol_versions, DatadogAgentServerCertVerifier,
    };

    const APPLICATION_BYTE: u8 = 42;
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

    async fn accept_client(server_config: ServerConfig, client_config: ClientConfig) -> io::Result<u8> {
        tokio::time::timeout(TEST_TIMEOUT, async move {
            let (client_io, server_io) = tokio::io::duplex(4096);
            let client = async move {
                let connector = TlsConnector::from(Arc::new(client_config));
                let server_name = ServerName::try_from("localhost").expect("localhost should be a valid server name");
                let mut stream = connector.connect(server_name, client_io).await?;
                stream.write_u8(APPLICATION_BYTE).await?;
                Ok::<_, io::Error>(stream)
            };
            let server = async move {
                let acceptor = TlsAcceptor::from(Arc::new(server_config));
                let mut stream = acceptor.accept(server_io).await?;
                Ok(stream
                    .read_u8()
                    .await
                    .expect("accepted client should send an application byte"))
            };

            let (_, server_result) = tokio::join!(client, server);
            server_result
        })
        .await
        .expect("TLS handshake should not time out")
    }

    #[tokio::test]
    async fn matching_ipc_identity_completes_mtls_and_delivers_application_byte() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = build_ipc_client_ipc_tls_config(&identity_a.cert_path)
            .await
            .expect("production client TLS config should build");

        assert_eq!(
            accept_client(server_config, client_config)
                .await
                .expect("server should accept the matching client identity"),
            APPLICATION_BYTE
        );
    }

    /// Returns the default provider without its TLS 1.3 cipher suites, like the Windows CNG provider in FIPS mode on a
    /// host whose CNG has no HKDF algorithm.
    fn provider_without_tls13_suites() -> Arc<CryptoProvider> {
        let mut provider = CryptoProvider::get_default()
            .map(|provider| provider.as_ref().clone())
            .expect("default crypto provider should be installed");
        provider.cipher_suites.retain(|suite| suite.tls13().is_none());
        assert!(
            !provider.cipher_suites.is_empty(),
            "default crypto provider should have TLS 1.2 cipher suites"
        );
        Arc::new(provider)
    }

    #[test]
    fn ipc_client_offers_only_tls13_when_provider_has_tls13_suites() {
        initialize_crypto_provider();
        let provider = CryptoProvider::get_default().expect("default crypto provider should be installed");

        assert_eq!(ipc_client_protocol_versions(provider), &[&TLS13]);
    }

    #[test]
    fn ipc_client_offers_only_tls12_when_provider_has_no_tls13_suites() {
        initialize_crypto_provider();

        assert_eq!(
            ipc_client_protocol_versions(&provider_without_tls13_suites()),
            &[&TLS12]
        );
    }

    #[tokio::test]
    async fn ipc_client_without_tls13_suites_completes_mtls_over_tls12() {
        initialize_crypto_provider();
        let identity = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = build_ipc_client_tls_config_with_provider(
            identity.cert_der.clone(),
            identity.private_key(),
            provider_without_tls13_suites(),
        )
        .expect("client TLS config should build without TLS 1.3 cipher suites");

        // FIPS 140-3 requires the Extended Master Secret extension for TLS 1.2. Rustls requires it whenever the
        // provider reports FIPS, and the FIPS check that the builder runs depends on it.
        assert_eq!(
            client_config.require_ems,
            client_config.crypto_provider().fips(),
            "client should require Extended Master Secret exactly when the provider is FIPS"
        );
        assert_eq!(
            server_config.require_ems,
            server_config.crypto_provider().fips(),
            "server should require Extended Master Secret exactly when the provider is FIPS"
        );

        let negotiated = tokio::time::timeout(TEST_TIMEOUT, async move {
            let (client_io, server_io) = tokio::io::duplex(4096);
            let client = async move {
                let connector = TlsConnector::from(Arc::new(client_config));
                let server_name = ServerName::try_from("localhost").expect("localhost should be a valid server name");
                let mut stream = connector.connect(server_name, client_io).await?;
                stream.write_u8(APPLICATION_BYTE).await?;
                Ok::<_, io::Error>(stream.get_ref().1.protocol_version())
            };
            let server = async move {
                let acceptor = TlsAcceptor::from(Arc::new(server_config));
                let mut stream = acceptor.accept(server_io).await?;
                stream.read_u8().await
            };

            let (client_result, server_result) = tokio::join!(client, server);
            assert_eq!(
                server_result.expect("server should accept the client over TLS 1.2"),
                APPLICATION_BYTE
            );
            client_result.expect("client should connect over TLS 1.2")
        })
        .await
        .expect("TLS handshake should not time out");

        assert_eq!(negotiated, Some(ProtocolVersion::TLSv1_2));
    }

    #[tokio::test]
    async fn missing_or_mismatched_client_certificate_makes_server_accept_fail() {
        initialize_crypto_provider();
        let identity_a = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = client_config_pinned_to(&identity_a, None);

        accept_client(server_config, client_config)
            .await
            .expect_err("server accept should reject a missing client certificate");

        let identity_b = TestIdentity::localhost();
        let server_config = build_ipc_server_tls_config(&identity_a.cert_path)
            .await
            .expect("production server TLS config should build");
        let client_config = client_config_pinned_to(&identity_a, Some(&identity_b));

        accept_client(server_config, client_config)
            .await
            .expect_err("server accept should reject a different client certificate");
    }
}
