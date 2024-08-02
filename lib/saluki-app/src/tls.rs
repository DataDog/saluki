//! TLS.

use saluki_error::{generic_error, GenericError};

/// Initializes the TLS subsystem.
///
/// This ensures the correct cryptography provider is selected for use by the TLS subsystem, which is important for
/// ensuring, in certain cases, that a validated cryptographic provider is used (e.g., when operating in FIPS mode).
///
/// ## Errors
///
/// If the TLS subsystem was already initialized, an error will be returned.
pub fn initialize_tls() -> Result<(), GenericError> {
    rustls::crypto::aws_lc_rs::default_provider().install_default()
		.map_err(|_| generic_error!("Failed to install AWS-LC as default cryptography provider. This is likely due to a conflicting provider already being installed."))
}
