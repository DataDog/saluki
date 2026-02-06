use std::sync::Arc;

use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::data_model::event::Event;
use saluki_env::autodiscovery::{Data, Instance};

use async_trait::async_trait;
use stringtheory::MetaString;
use tokio::sync::mpsc::Sender;
use tracing::info;

use crate::sources::checks::builder::native::native_check::{NativeCheck, NativeSink};
use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;
use crate::sources::checks::execution_context::ExecutionContext;

use core_checks::http_check::HttpCheck;

pub struct NativeCheckBuilder {
    check_events_tx: Sender<Event>,
    execution_context: Arc<ExecutionContext>,
}

impl NativeCheckBuilder {
    pub fn new(
        check_events_tx: Sender<Event>, _: Option<Vec<String>>, execution_context: Arc<ExecutionContext>,
    ) -> Self {
        Self {
            check_events_tx,
            execution_context,
        }
    }
}

impl MemoryBounds for NativeCheckBuilder {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<NativeCheckBuilder>("source struct");
    }
}

#[async_trait]
impl CheckBuilder for NativeCheckBuilder {
    async fn build_check(
        &self, name: &str, instance: &Instance, init_config: &Data, source: &MetaString,
    ) -> Option<Arc<dyn Check + Send + Sync>> {
        let build_fn = match name {
            "http-check" => NativeCheck::build::<HttpCheck<NativeSink>>,
            _ => {
                info!(check.name = name, "Unknown check.");
                return None;
            }
        };

        build_fn(
            self.check_events_tx.clone(),
            MetaString::from(name),
            init_config.clone(),
            instance.clone(),
            source.clone(),
            self.execution_context.clone(),
        )
        .await
    }
}
