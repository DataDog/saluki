use std::time::Duration;

use saluki_error::GenericError;

/// Check trait
///
/// This trait allow us to have different check implementations.
pub trait Check {
    /// Run the check
    fn run(&self) -> Result<(), GenericError>;
    /// Get the interval of the check
    fn interval(&self) -> Duration;
    /// Get the id of the check
    fn id(&self) -> &str;
}
