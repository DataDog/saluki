use std::sync::Arc;
mod noop;
use saluki_env::autodiscovery::CheckConfig;

pub use self::noop::NoopCheckBuilder;
use crate::sources::checks::check::Check;

/// Check builder trait
///
/// We use this trait to build checks.
/// Based on the system, some check would use different runtimes.
/// For example, a check might be written in Python, and another in Rust.
///
/// This trait allow us to have a unified way to build checks, and have different implementations
/// for different runtimes.
pub trait CheckBuilder {
    /// Build a check
    fn build_check(&self, check_id: &str, check_request: &CheckConfig) -> Option<Arc<dyn Check + Send + Sync>>;
}
