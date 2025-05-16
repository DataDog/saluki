//! A no-op implementation of CheckBuilder

use std::sync::Arc;

use async_trait::async_trait;
use saluki_env::autodiscovery::{Data, Instance};
use stringtheory::MetaString;

use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;

pub struct NoopCheckBuilder;

#[async_trait]
impl CheckBuilder for NoopCheckBuilder {
    async fn build_check(
        &self, _name: &str, _instance: &Instance, _init_config: &Data, _source: &MetaString,
    ) -> Option<Arc<dyn Check + Send + Sync>> {
        None
    }
}
