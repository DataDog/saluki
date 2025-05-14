//! A Python implementation of CheckBuilder

use std::sync::Arc;

use async_trait::async_trait;
use saluki_env::autodiscovery::CheckConfig;
use tokio::sync::OnceCell;

use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;

pub struct PythonCheckBuilder {
    able_to_run_checks: OnceCell<()>,
}

impl PythonCheckBuilder {
    pub fn new() -> Self {
        Self {
            able_to_run_checks: OnceCell::new(),
        }
    }

    async fn ensure_able_to_run_checks(&self) {
        self.able_to_run_checks
            .get_or_init(|| async {
                // TODO: Implement this
            })
            .await;
    }
}

#[async_trait]
impl CheckBuilder for PythonCheckBuilder {
    async fn build_check(&self, _check_id: &str, _check_request: &CheckConfig) -> Option<Arc<dyn Check + Send + Sync>> {
        self.ensure_able_to_run_checks().await;
        None
    }
}
