//! TLS.

use saluki_error::GenericError;
use saluki_tls::{initialize_default_crypto_provider, load_platform_root_certificates};

/// Initializes the TLS subsystem.
///
/// This ensures the correct cryptography provider is selected for use by the TLS subsystem, which is important for
/// ensuring, in certain cases, that a validated cryptographic provider is used (e.g., when operating in FIPS mode).
/// Additionally, it ensures that the platform's root certificate store can be loaded for validating connections.
///
/// # Errors
///
/// If the TLS subsystem was already initialized, or if there was an error loading the platform's native certificate
/// store, an error will be returned.
pub(crate) fn initialize_tls() -> Result<(), GenericError> {
    initialize_default_crypto_provider()?;
    load_platform_root_certificates()
}
