//! Transport Layer Security (TLS) configuration and helpers.

use std::sync::{Arc, Mutex, OnceLock};

use rustls::{client::Resumption, ClientConfig, RootCertStore};
use saluki_error::{generic_error, GenericError};
use tracing::debug;

/// Tracks if the default cryptography provider for `rustls` has been set.
static DEFAULT_CRYPTO_PROVIDER_SET: OnceLock<()> = OnceLock::new();

/// Default root certificate store to use for TLS when one isn't explicitly provided.
static DEFAULT_ROOT_CERT_STORE_MUTEX: Mutex<()> = Mutex::new(());
static DEFAULT_ROOT_CERT_STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();

// Various defaults for TLS configuration.
const DEFAULT_MAX_TLS12_RESUMPTION_SESSIONS: usize = 8;

/// A TLS client configuration builder.
///
/// Exposes various options for configuring a client's TLS configuration that would otherwise be cumbersome to
/// configure, and provides sane defaults for many common options.
///
/// ## Missing
///
/// - ability to configure client authentication
#[derive(Clone)]
pub struct ClientTLSConfigBuilder {
    max_tls12_resumption_sessions: Option<usize>,
    root_cert_store: Option<RootCertStore>,
}

impl ClientTLSConfigBuilder {
    pub fn new() -> Self {
        Self {
            max_tls12_resumption_sessions: None,
            root_cert_store: None,
        }
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

    /// Builds the client TLS configuration.
    ///
    /// ## Errors
    ///
    /// If the default root cert store (see [`load_platform_root_certificates`]) has not been initialized, and a root
    /// cert store has not been provided, or if the resulting configuration is not FIPS compliant, an error will be
    /// returned.
    pub fn build(self) -> Result<ClientConfig, GenericError> {
        let max_tls12_resumption_sessions = self
            .max_tls12_resumption_sessions
            .unwrap_or(DEFAULT_MAX_TLS12_RESUMPTION_SESSIONS);

        let root_cert_store = self.root_cert_store.map(Arc::new).map(Ok).unwrap_or_else(|| {
            DEFAULT_ROOT_CERT_STORE
                .get()
                .map(Arc::clone)
                .ok_or(generic_error!("Default TLS root certificate store not initialized."))
        })?;

        let mut config = ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();

        // One unfortunate thing is that by creating `config` above, it assigns the default value for `Resumption` before
        // we reset it down here... which means the big, beefy default one gets allocated and then immediately thrown
        // away.
        config.resumption = Resumption::in_memory_sessions(max_tls12_resumption_sessions);

        // Do our final check that this configuration is FIPS compliant.
        #[cfg(feature = "fips")]
        if !config.fips() {
            return Err(generic_error!("Client TLS configuration is not FIPS compliant."));
        }

        Ok(config)
    }
}

/// Initializes the default TLS cryptography provider used by `rustls`.
///
/// This explicitly sets the [AWS-LC][aws_lc] provider as the default provider for all future TLS configurations, which
/// provides the ability to run in FIPS mode for FIPS-compliant builds.
///
/// This is the only supported cryptography provider in Saluki.
///
/// ## Errors
///
/// If the default cryptography provider has already been set, an error will be returned.
///
/// [aws_lc]: https://github.com/aws/aws-lc-rs
pub fn initialize_default_crypto_provider() -> Result<(), GenericError> {
    if DEFAULT_CRYPTO_PROVIDER_SET.get().is_some() {
        return Err(generic_error!("Default TLS cryptography provider already initialized."));
    }

    // Set the process-wide default `CryptoProvider` to AWS-LC.
    //
    // This locks in AWS-LC as the default provider for all future TLS configurations, regardless of whether they use
    // the configuration builders here or not. (The main caveat is that it's only relevant if `rustls` is being used.)
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| generic_error!("Failed to install AWS-LC as default cryptography provider. This is likely due to a conflicting provider already being installed."))?;

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
/// | SSL_CERT_FILE        | File containing an arbitrary number of certificates in PEM format.                     |
/// | SSL_CERT_DIR         | Directory utilizing the hierarchy and naming convention used by OpenSSL's [c_rehash]. |
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
/// If any error occurs during the locating or loading the platform's native certificate store, an error will be returned.
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

    let mut root_cert_store = RootCertStore::empty();

    let result = rustls_native_certs::load_native_certs();
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

    let (added, failed) = root_cert_store.add_parsable_certificates(result.certs);
    if failed == 0 && added > 0 {
        debug!(
            "Added {} certificates from environment to the default root certificate store.",
            added
        );
    } else if failed > 0 && added > 0 {
        debug!("Added {} certificates from environment to the default root certificate store, but failed to add {} certificates.", added, failed);
    } else {
        return Err(generic_error!(
            "Failed to add any certificates from environment to the default root certificate store."
        ));
    }

    // The reason it should be impossible is that we intentionally only set it _here_, and we do so after acquiring the
    // mutex, and only then do we make sure that it hasn't been set before proceeding to try to set it.
    DEFAULT_ROOT_CERT_STORE
        .set(Arc::new(root_cert_store))
        .expect("should be impossible for DEFAULT_ROOT_CERT_STORE to be initialized twice");

    Ok(())
}
