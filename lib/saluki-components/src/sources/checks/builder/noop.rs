//! A no-op implementation of CheckBuilder

use std::sync::Arc;

use saluki_env::autodiscovery::CheckConfig;

use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;

pub struct NoopCheckBuilder;

impl CheckBuilder for NoopCheckBuilder {
    fn build_check(&self, _check_id: &str, _check_request: &CheckConfig) -> Option<Arc<dyn Check + Send + Sync>> {
        None
    }
}
