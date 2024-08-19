use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::sources::*, observability::metrics::MetricsReceiver, pooling::ObjectPool as _,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_event::DataType;
use tokio::select;
use tracing::{debug, error};

/// Internal metrics source.
///
/// Collects all metrics that are emitted internally (via the `metrics` crate) and forwards them as-is.
pub struct InternalMetricsConfiguration;

#[async_trait]
impl SourceBuilder for InternalMetricsConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(InternalMetrics))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

impl MemoryBounds for InternalMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<InternalMetrics>();
    }
}

pub struct InternalMetrics;

#[async_trait]
impl Source for InternalMetrics {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let mut receiver = MetricsReceiver::register();

        health.mark_ready();
        debug!("Internal Metrics source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                metrics = receiver.next() => {
                    debug!(metrics_len = metrics.len(), "Received internal metrics.");

                    let mut event_buffer = context.event_buffer_pool().acquire().await;
                    event_buffer.extend(Arc::unwrap_or_clone(metrics));

                    if let Err(e) = context.forwarder().forward(event_buffer).await {
                        error!(error = %e, "Failed to forward events.");
                    }
                },
            }
        }

        debug!("Internal Metrics source stopped.");

        Ok(())
    }
}
