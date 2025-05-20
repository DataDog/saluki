use std::time::Duration;

use saluki_error::GenericError;

/// A check.
///
/// Checks run some arbitrary chunk of logic on a configured interval, potentially producing outputs
/// such as service checks, events, metrics, and logs.
#[allow(dead_code)]
pub trait Check {
    /// Run the check.
    /// # Errors
    ///
    /// If a problem occurs while running the check, an error is returned.
    fn run(&self) -> Result<(), GenericError>;
    /// Get the interval of the check.
    fn interval(&self) -> &Duration;
    /// Gets the identifier of the check.
    ///
    /// This is used to uniquely identify check instances.
    fn id(&self) -> &str;
    /// Get the version of the check.
    fn version(&self) -> &str;
    /// Get the source of the check.
    fn source(&self) -> &str;
}
