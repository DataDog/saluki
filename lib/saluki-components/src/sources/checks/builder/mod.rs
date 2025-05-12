use std::sync::Arc;
mod noop;
use saluki_env::autodiscovery::Config;

pub use self::noop::NoopCheckBuilder;
use crate::sources::checks::check::Check;

pub trait CheckBuilder {
    fn build_check(&self, check_id: &str, check_request: &Config) -> Option<Arc<dyn Check + Send + Sync>>;
}
