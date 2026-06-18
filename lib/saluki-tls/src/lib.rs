//! Transport Layer Security (TLS) configuration and helpers.

#[cfg(all(unix, not(feature = "_fips")))]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(not(feature = "_fips"))]
use std::{
    fmt::{Debug, Formatter},
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

#[cfg(not(feature = "_fips"))]
use rustls::KeyLog;
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        Resumption,
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    version::{TLS12, TLS13},
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme, SupportedProtocolVersion,
};
#[cfg(not(feature = "_fips"))]
use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use tracing::debug;
#[cfg(not(feature = "_fips"))]
use tracing::warn;

/// Tracks if the default cryptography provider for `rustls` has been set.
static DEFAULT_CRYPTO_PROVIDER_SET: OnceLock<()> = OnceLock::new();

/// Default root certificate store to use for TLS when one isn't explicitly provided.
static DEFAULT_ROOT_CERT_STORE_MUTEX: Mutex<()> = Mutex::new(());
static DEFAULT_ROOT_CERT_STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();
#[cfg(not(feature = "_fips"))]
static KEY_LOG_FILES: OnceLock<Mutex<FastHashMap<PathBuf, Option<Arc<NssKeyLogFile>>>>> = OnceLock::new();

// Various defaults for TLS configuration.
const DEFAULT_MAX_TLS12_RESUMPTION_SESSIONS: usize = 8;
const TLS12_PLUS_PROTOCOL_VERSIONS: &[&SupportedProtocolVersion] = &[&TLS13, &TLS12];
const TLS13_PROTOCOL_VERSIONS: &[&SupportedProtocolVersion] = &[&TLS13];

/// Minimum TLS protocol version to use for client connections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TlsMinimumVersion {
    /// TLS 1.2 or newer.
    #[default]
    Tls12,

    /// TLS 1.3 or newer.
    Tls13,
}

impl TlsMinimumVersion {
    const fn protocol_versions(self) -> &'static [&'static SupportedProtocolVersion] {
        match self {
            Self::Tls12 => TLS12_PLUS_PROTOCOL_VERSIONS,
            Self::Tls13 => TLS13_PROTOCOL_VERSIONS,
        }
    }
}

/// A certificate verifier that accepts all server certificates without validation.
///
/// This is inherently insecure and should only be used for local/development connections where the
/// server's identity is already established through other means (for example, connecting via Unix domain socket
/// to a local process).
#[derive(Debug)]
struct AcceptAllServerCertVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAllServerCertVerifier {
    fn verify_server_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _server_name: &ServerName<'_>,
        _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
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

#[cfg(not(feature = "_fips"))]
struct NssKeyLogFile {
    path: PathBuf,
    file: Mutex<File>,
}

#[cfg(not(feature = "_fips"))]
impl NssKeyLogFile {
    fn open_shared<P: Into<PathBuf>>(path: P) -> Option<Arc<Self>> {
        let path = path.into();
        let mut key_log_files = match KEY_LOG_FILES.get_or_init(|| Mutex::new(FastHashMap::default())).lock() {
            Ok(key_log_files) => key_log_files,
            Err(_) => {
                warn!("Failed to acquire TLS key log file registry lock; TLS key logging disabled.");
                return None;
            }
        };

        // Open is attempted exactly once per path. Both success and failure are cached so that
        // repeated builds for the same path do not re-open the file or re-emit warnings.
        if let Some(cached) = key_log_files.get(&path) {
            return cached.clone();
        }

        let key_log_file = match open_key_log_file(&path) {
            Ok(file) => {
                warn!(
                    path = %path.display(),
                    "TLS key logging enabled; TLS session secrets will be written to disk."
                );
                Some(Arc::new(Self {
                    path: path.clone(),
                    file: Mutex::new(file),
                }))
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to open TLS key log file for appending; TLS key logging disabled."
                );
                None
            }
        };

        key_log_files.insert(path, key_log_file.clone());
        key_log_file
    }
}

#[cfg(not(feature = "_fips"))]
impl Debug for NssKeyLogFile {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NssKeyLogFile").field("path", &self.path).finish()
    }
}

#[cfg(feature = "_fips")]
static FIPS_KEY_LOG_WARNED_PATHS: OnceLock<Mutex<saluki_common::collections::FastHashSet<PathBuf>>> = OnceLock::new();

#[cfg(feature = "_fips")]
fn fips_key_log_warn_once(path: PathBuf) {
    let warned =
        FIPS_KEY_LOG_WARNED_PATHS.get_or_init(|| Mutex::new(saluki_common::collections::FastHashSet::default()));
    let Ok(mut warned) = warned.lock() else {
        return;
    };
    if warned.insert(path.clone()) {
        tracing::warn!(
            path = %path.display(),
            "FIPS build: TLS key logging is disabled because exporting TLS secrets is not FIPS-compliant."
        );
    }
}

#[cfg(not(feature = "_fips"))]
impl KeyLog for NssKeyLogFile {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        let line = match build_nss_key_log_line(label, client_random, secret) {
            Ok(line) => line,
            Err(e) => {
                debug!(path = %self.path.display(), error = %e, "Failed to format TLS key log line.");
                return;
            }
        };

        match self.file.lock() {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&line) {
                    debug!(path = %self.path.display(), error = %e, "Failed to write TLS key log line.");
                }
            }
            Err(_) => {
                debug!(path = %self.path.display(), "TLS key log file lock poisoned; dropping TLS key log line.");
            }
        }
    }
}

#[cfg(not(feature = "_fips"))]
fn open_key_log_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).append(true);

    #[cfg(unix)]
    options.mode(0o600);

    options.open(path)
}

#[cfg(not(feature = "_fips"))]
fn build_nss_key_log_line(label: &str, client_random: &[u8], secret: &[u8]) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    write!(line, "{label} ")?;
    write_hex(&mut line, client_random)?;
    write!(line, " ")?;
    write_hex(&mut line, secret)?;
    writeln!(line)?;

    Ok(line)
}

#[cfg(not(feature = "_fips"))]
fn write_hex(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    for byte in bytes {
        write!(writer, "{byte:02x}")?;
    }

    Ok(())
}

/// A TLS client configuration builder.
///
/// Exposes various options for configuring a client's TLS configuration that would otherwise be cumbersome to
/// configure, and provides sane defaults for many common options.
///
/// # Missing
///
/// - ability to configure client authentication
pub struct ClientTLSConfigBuilder {
    key_log_file_path: Option<PathBuf>,
    max_tls12_resumption_sessions: Option<usize>,
    min_tls_version: TlsMinimumVersion,
    root_cert_store: Option<RootCertStore>,
    danger_accept_invalid_certs: bool,
}

impl ClientTLSConfigBuilder {
    pub fn new() -> Self {
        Self {
            key_log_file_path: None,
            max_tls12_resumption_sessions: None,
            min_tls_version: TlsMinimumVersion::default(),
            root_cert_store: None,
            danger_accept_invalid_certs: false,
        }
    }

    /// Enables logging of TLS key material to the given file path.
    ///
    /// TLS key material will be logged to the given file path in the [NSS Key Log][nss_key_log]
    /// format, which can be used for debugging TLS issues, as well as decrypting captured
    /// TLS traffic in tools such as Wireshark.
    ///
    /// Newly created files are created with owner read/write permissions on Unix.
    /// Existing file permissions are preserved.
    ///
    /// [nss_key_log]: https://nss-crypto.org/reference/security/nss/legacy/key_log_format/index.html
    pub fn with_key_log_file<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.key_log_file_path = Some(path.into());
        self
    }

    /// Sets the maximum number of TLS 1.2 sessions to cache.
    ///
    /// Defaults to 8.
    pub fn with_max_tls12_resumption_sessions(mut self, max: usize) -> Self {
        self.max_tls12_resumption_sessions = Some(max);
        self
    }

    /// Sets the root certificate store to use for the client.
    ///
    /// Defaults to the "default" root certificate store initialized from the platform. (See [`load_platform_root_certificates`].)
    pub fn with_root_cert_store(mut self, store: RootCertStore) -> Self {
        self.root_cert_store = Some(store);
        self
    }

    /// Sets the minimum TLS protocol version to allow for client connections.
    ///
    /// Defaults to TLS 1.2.
    pub fn with_min_tls_version(mut self, version: TlsMinimumVersion) -> Self {
        self.min_tls_version = version;
        self
    }

    /// Disables server certificate verification entirely.
    ///
    /// This is inherently insecure and should only be used for local/development connections where
    /// the server's identity is already established through other means (for example, connecting via Unix
    /// domain socket to a local process).
    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.danger_accept_invalid_certs = true;
        self
    }

    /// Builds the client TLS configuration.
    ///
    /// # Errors
    ///
    /// If the default root cert store (see [`load_platform_root_certificates`]) hasn't been initialized, and a root
    /// cert store hasn't been provided, or if the resulting configuration isn't FIPS compliant, an error will be
    /// returned.
    pub fn build(self) -> Result<ClientConfig, GenericError> {
        let max_tls12_resumption_sessions = self
            .max_tls12_resumption_sessions
            .unwrap_or(DEFAULT_MAX_TLS12_RESUMPTION_SESSIONS);
        let protocol_versions = self.min_tls_version.protocol_versions();

        let mut config = if self.danger_accept_invalid_certs {
            let crypto_provider = CryptoProvider::get_default()
                .map(Arc::clone)
                .ok_or_else(|| generic_error!("Default cryptography provider not yet installed."))?;
            let verifier = Arc::new(AcceptAllServerCertVerifier {
                provider: crypto_provider,
            });

            ClientConfig::builder_with_protocol_versions(protocol_versions)
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth()
        } else {
            let root_cert_store = self.root_cert_store.map(Arc::new).map(Ok).unwrap_or_else(|| {
                DEFAULT_ROOT_CERT_STORE
                    .get()
                    .map(Arc::clone)
                    .ok_or(generic_error!("Default TLS root certificate store not initialized."))
            })?;

            ClientConfig::builder_with_protocol_versions(protocol_versions)
                .with_root_certificates(root_cert_store)
                .with_no_client_auth()
        };

        if let Some(path) = self.key_log_file_path {
            #[cfg(feature = "_fips")]
            fips_key_log_warn_once(path);

            #[cfg(not(feature = "_fips"))]
            if let Some(key_log) = NssKeyLogFile::open_shared(path) {
                config.key_log = key_log;
            }
        }

        // One unfortunate thing is that by creating `config` above, it assigns the default value for `Resumption` before
        // we reset it down here... which means the big, beefy default one gets allocated and then immediately thrown
        // away.
        config.resumption = Resumption::in_memory_sessions(max_tls12_resumption_sessions);

        // FIPS requires the Extended Master Secret extension (RFC 7627) for TLS 1.2. Both the AWS-LC and Windows CNG
        // FIPS providers expect it, and `config.fips()` below will not return true for a TLS 1.2-capable config without
        // it.
        #[cfg(feature = "_fips")]
        {
            config.require_ems = true;
        }

        // Do our final check that this configuration is FIPS compliant.
        #[cfg(feature = "_fips")]
        if !config.fips() {
            return Err(generic_error!("Client TLS configuration is not FIPS compliant."));
        }

        Ok(config)
    }
}

/// Initializes the default TLS cryptography provider used by `rustls`.
///
/// The provider is selected by target: Windows uses the OS-native [CNG][cng] provider (via `rustls-cng-crypto`), and
/// every other platform uses [AWS-LC][aws_lc]. Both support running in FIPS mode for FIPS-compliant builds. On Windows,
/// FIPS mode is governed by the host's system-wide FIPS policy, so the resulting configuration is only FIPS-compliant
/// when that policy is enabled.
///
/// # Errors
///
/// If the default cryptography provider has already been set, an error will be returned. In FIPS builds, an error is
/// also returned if the installed provider isn't operating in FIPS mode.
///
/// [aws_lc]: https://github.com/aws/aws-lc-rs
/// [cng]: https://learn.microsoft.com/en-us/windows/win32/seccng/cng-portal
pub fn initialize_default_crypto_provider() -> Result<(), GenericError> {
    if DEFAULT_CRYPTO_PROVIDER_SET.get().is_some() {
        return Err(generic_error!("Default TLS cryptography provider already initialized."));
    }

    // Install the process-wide default `CryptoProvider`. The provider is selected by target: Windows uses the
    // OS-native CNG provider (`rustls-cng-crypto`), and every other platform uses AWS-LC. This locks in the provider
    // for all future TLS configurations, regardless of whether they use the configuration builders here. (The main
    // caveat is that it's only relevant if `rustls` is being used.)
    #[cfg(windows)]
    let provider = rustls_cng_crypto::default_provider();
    #[cfg(not(windows))]
    let provider = rustls::crypto::aws_lc_rs::default_provider();

    provider
        .install_default()
        .map_err(|_| generic_error!("Failed to install the default TLS cryptography provider. This is likely due to a conflicting provider already being installed."))?;

    // In FIPS builds, fail fast if the installed provider isn't actually operating in FIPS mode. On Windows this most
    // commonly means the host's system-wide FIPS policy is disabled (CNG then exposes no FIPS-approved cryptography);
    // elsewhere it indicates a provider/build mismatch.
    #[cfg(feature = "_fips")]
    if !CryptoProvider::get_default()
        .ok_or_else(|| generic_error!("Default TLS cryptography provider missing immediately after installation."))?
        .fips()
    {
        return Err(generic_error!(
            "FIPS build is not operating in FIPS mode. On Windows, ensure the host's system-wide FIPS policy is enabled."
        ));
    }

    // With the process-wide default having been set, mark it as having been set.
    DEFAULT_CRYPTO_PROVIDER_SET
        .set(())
        .expect("should be impossible for DEFAULT_CRYPTO_PROVIDER_SET to be initialized twice");

    Ok(())
}

/// Initializes the default root certificate store from the platform's native certificate store.
///
/// ## Environment Variables
///
/// | Environment Variable | Description                                                                           |
/// |----------------------|---------------------------------------------------------------------------------------|
/// | SSL_CERT_FILE        | File containing an arbitrary number of certificates in PEM format.                    |
/// | SSL_CERT_DIR         | Directory utilizing the hierarchy and naming convention used by OpenSSL's `c_rehash`. |
///
/// If **either** (or **both**) are set, certificates are only loaded from the locations specified via environment
/// variables and not the platform- native certificate store.
///
/// ## Certificate Validity
///
/// All certificates are expected to be in PEM format. A file may contain multiple certificates.
///
/// Example:
///
/// ```text
/// -----BEGIN CERTIFICATE-----
/// MIICGzCCAaGgAwIBAgIQQdKd0XLq7qeAwSxs6S+HUjAKBggqhkjOPQQDAzBPMQsw
/// CQYDVQQGEwJVUzEpMCcGA1UEChMgSW50ZXJuZXQgU2VjdXJpdHkgUmVzZWFyY2gg
/// R3JvdXAxFTATBgNVBAMTDElTUkcgUm9vdCBYMjAeFw0yMDA5MDQwMDAwMDBaFw00
/// MDA5MTcxNjAwMDBaME8xCzAJBgNVBAYTAlVTMSkwJwYDVQQKEyBJbnRlcm5ldCBT
/// ZWN1cml0eSBSZXNlYXJjaCBHcm91cDEVMBMGA1UEAxMMSVNSRyBSb290IFgyMHYw
/// EAYHKoZIzj0CAQYFK4EEACIDYgAEzZvVn4CDCuwJSvMWSj5cz3es3mcFDR0HttwW
/// +1qLFNvicWDEukWVEYmO6gbf9yoWHKS5xcUy4APgHoIYOIvXRdgKam7mAHf7AlF9
/// ItgKbppbd9/w+kHsOdx1ymgHDB/qo0IwQDAOBgNVHQ8BAf8EBAMCAQYwDwYDVR0T
/// AQH/BAUwAwEB/zAdBgNVHQ4EFgQUfEKWrt5LSDv6kviejM9ti6lyN5UwCgYIKoZI
/// zj0EAwMDaAAwZQIwe3lORlCEwkSHRhtFcP9Ymd70/aTSVaYgLXTWNLxBo1BfASdW
/// tL4ndQavEi51mI38AjEAi/V3bNTIZargCyzuFJ0nN6T5U6VR5CmD1/iQMVtCnwr1
/// /q4AaOeMSQ+2b1tbFfLn
/// -----END CERTIFICATE-----
/// -----BEGIN CERTIFICATE-----
/// MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
/// MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
/// Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
/// A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
/// Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
/// ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
/// QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
/// ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
/// BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
/// YyRIHN8wfdVoOw==
/// -----END CERTIFICATE-----
///
/// ```
///
/// For reasons of compatibility, an attempt is made to skip invalid sections of a certificate file but this means it's
/// also possible for a malformed certificate to be skipped.
///
/// If a certificate isn't loaded, and no error is reported, check if:
///
/// 1. the certificate is in PEM format (see example above)
/// 2. *BEGIN CERTIFICATE* line starts with exactly five hyphens (`'-'`)
/// 3. *END CERTIFICATE* line ends with exactly five hyphens (`'-'`)
/// 4. there is a line break after the certificate.
///
/// ## Errors
///
/// If errors occur during certificate loading and no certificates were ultimately added to the store, an error is
/// returned. Missing files or directories referenced by `SSL_CERT_FILE`/`SSL_CERT_DIR` are tolerated and treated as
/// "no certificates available" rather than as a load failure.
///
/// [c_rehash]: https://www.openssl.org/docs/manmaster/man1/c_rehash.html
pub fn load_platform_root_certificates() -> Result<(), GenericError> {
    let _guard = DEFAULT_ROOT_CERT_STORE_MUTEX
        .lock()
        .map_err(|_| generic_error!("Default TLS root certificate store update lock poisoned."))?;
    if DEFAULT_ROOT_CERT_STORE.get().is_some() {
        return Err(generic_error!(
            "Default TLS root certificate store already initialized."
        ));
    }

    let root_cert_store = load_platform_root_certificates_inner()?;

    // The reason it should be impossible is that we intentionally only set it _here_, and we do so after acquiring the
    // mutex, and only then do we make sure that it hasn't been set before proceeding to try to set it.
    DEFAULT_ROOT_CERT_STORE
        .set(Arc::new(root_cert_store))
        .expect("should be impossible for DEFAULT_ROOT_CERT_STORE to be initialized twice");

    Ok(())
}

/// Builds a `RootCertStore` from the platform's native certificate store.
///
/// Behaves identically to [`load_platform_root_certificates`] with respect to which certificates are loaded, but
/// returns the constructed store instead of writing it into the process-wide default.
///
/// # Errors
///
/// If errors occur during certificate loading and no certificates were ultimately added to the store, an error is
/// returned. Otherwise, even if some certificates failed to parse, the store is returned with whatever certificates were
/// successfully added. Missing files or directories referenced by `SSL_CERT_FILE`/`SSL_CERT_DIR` are tolerated and do
/// not produce an error.
pub fn load_platform_root_certificates_inner() -> Result<RootCertStore, GenericError> {
    let mut root_cert_store = RootCertStore::empty();

    let mut result = rustls_native_certs::load_native_certs();

    // Drop "not found" IO errors before evaluating success or failure: a missing `SSL_CERT_FILE` or `SSL_CERT_DIR`
    // should look like "no certificates available" rather than a load failure, since callers may simply not have set
    // those env vars on this host.
    result.errors.retain(|err| {
        !matches!(
            &err.kind,
            rustls_native_certs::ErrorKind::Io { inner, .. } if inner.kind() == std::io::ErrorKind::NotFound,
        )
    });

    // For whatever certificates we _did_ get back, try and add them to the root certificate store.
    let (added, failed) = root_cert_store.add_parsable_certificates(result.certs);
    if failed == 0 && added > 0 {
        debug!(
            "Added {} certificates from environment to the default root certificate store.",
            added
        );
    } else if failed > 0 && added > 0 {
        debug!("Added {} certificates from environment to the default root certificate store, but failed to add {} certificates.", added, failed);
    } else {
        // When we don't manage to add any certificates, it either means that:
        // - we found no certificates to add
        // - we hit an error when loading the certificates
        // - we hit an error when trying to add the certificates to our root certificate store
        //
        // We only consider this operation to have truly failed if there were errors during the initial loading of the
        // certificates.
        if !result.errors.is_empty() {
            let joined_errors = result
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ");

            return Err(generic_error!(
                "Failed to load certificates from platform's native certificate store: {}",
                joined_errors
            ));
        }
    }

    Ok(root_cert_store)
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "_fips"))]
    use std::fs;
    #[cfg(all(unix, not(feature = "_fips")))]
    use std::os::unix::fs::PermissionsExt;

    use rustls::{ProtocolVersion, RootCertStore};

    #[cfg(not(feature = "_fips"))]
    use super::{build_nss_key_log_line, open_key_log_file};
    use super::{ClientTLSConfigBuilder, TlsMinimumVersion};

    #[test]
    fn tls12_minimum_enables_tls12_and_tls13() {
        let versions = TlsMinimumVersion::Tls12.protocol_versions();

        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, ProtocolVersion::TLSv1_3);
        assert_eq!(versions[1].version, ProtocolVersion::TLSv1_2);
    }

    #[test]
    fn tls13_minimum_enables_tls13_only() {
        let versions = TlsMinimumVersion::Tls13.protocol_versions();

        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, ProtocolVersion::TLSv1_3);
    }

    #[test]
    #[cfg(not(feature = "_fips"))]
    fn nss_key_log_lines_are_written_in_hex_format() {
        let output =
            build_nss_key_log_line("CLIENT_RANDOM", &[0xab, 0xcd], &[0x01, 0x23]).expect("key log line should build");

        assert_eq!(output, b"CLIENT_RANDOM abcd 0123\n");
    }

    #[test]
    #[cfg(not(feature = "_fips"))]
    fn client_config_uses_configured_key_log_file() {
        let _ = super::initialize_default_crypto_provider();
        let tempdir = tempfile::tempdir().expect("temporary directory should be created");
        let key_log_path = tempdir.path().join("sslkeylogfile");

        let config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(RootCertStore::empty())
            .with_key_log_file(&key_log_path)
            .build()
            .expect("client TLS config should build");

        config.key_log.log("CLIENT_RANDOM", &[0xab, 0xcd], &[0x01, 0x23]);

        let contents = fs::read_to_string(&key_log_path).expect("key log file should be readable");
        assert_eq!(contents, "CLIENT_RANDOM abcd 0123\n");
    }

    #[test]
    #[cfg(not(feature = "_fips"))]
    fn client_config_ignores_unwritable_key_log_file() {
        let _ = super::initialize_default_crypto_provider();
        let tempdir = tempfile::tempdir().expect("temporary directory should be created");
        let key_log_path = tempdir.path().join("missing").join("sslkeylogfile");

        let config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(RootCertStore::empty())
            .with_key_log_file(&key_log_path)
            .build()
            .expect("client TLS config should build even when the key log file cannot be opened");

        config.key_log.log("CLIENT_RANDOM", &[0xab, 0xcd], &[0x01, 0x23]);

        assert!(!key_log_path.exists());
    }

    #[test]
    #[cfg(not(feature = "_fips"))]
    fn client_configs_append_to_shared_key_log_file() {
        let _ = super::initialize_default_crypto_provider();
        let tempdir = tempfile::tempdir().expect("temporary directory should be created");
        let key_log_path = tempdir.path().join("shared-sslkeylogfile");

        let first_config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(RootCertStore::empty())
            .with_key_log_file(&key_log_path)
            .build()
            .expect("first client TLS config should build");
        let second_config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(RootCertStore::empty())
            .with_key_log_file(&key_log_path)
            .build()
            .expect("second client TLS config should build");

        first_config.key_log.log("CLIENT_RANDOM", &[0xab, 0xcd], &[0x01, 0x23]);
        second_config.key_log.log("CLIENT_RANDOM", &[0xef, 0x01], &[0x45, 0x67]);

        let contents = fs::read_to_string(&key_log_path).expect("key log file should be readable");
        assert_eq!(contents, "CLIENT_RANDOM abcd 0123\nCLIENT_RANDOM ef01 4567\n");
    }

    #[cfg(all(unix, not(feature = "_fips")))]
    #[test]
    fn key_log_file_is_created_with_owner_only_permissions() {
        let tempdir = tempfile::tempdir().expect("temporary directory should be created");
        let key_log_path = tempdir.path().join("sslkeylogfile");

        let file = open_key_log_file(&key_log_path).expect("key log file should open");
        drop(file);

        let mode = fs::metadata(&key_log_path)
            .expect("key log file metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(feature = "_fips")]
    fn key_log_file_ignored_in_fips_mode() {
        let _ = super::initialize_default_crypto_provider();
        let tempdir = tempfile::tempdir().expect("temporary directory should be created");
        let key_log_path = tempdir.path().join("fips-sslkeylogfile");

        // FIPS builds soft-skip TLS key logging instead of failing, so a leftover key log file path does not
        // prevent TLS client construction.
        let config = ClientTLSConfigBuilder::new()
            .with_root_cert_store(RootCertStore::empty())
            .with_key_log_file(&key_log_path)
            .build()
            .expect("TLS config should build in FIPS mode even when a key log file is configured");

        // The default no-op `KeyLog` remains in place: invoking it must not panic and must not produce a file.
        config.key_log.log("CLIENT_RANDOM", &[0xab, 0xcd], &[0x01, 0x23]);

        assert!(!key_log_path.exists(), "FIPS builds must not create a TLS key log file");
    }
}
