use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::data_model::event::EventType;
use saluki_core::{
    components::{sources::*, ComponentContext},
    observability::metrics::MetricsStream,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

/// Internal metrics source.
///
/// Collects all metrics that are emitted internally (via the `metrics` crate) and forwards them as-is.
pub struct InternalMetricsConfiguration;

#[async_trait]
impl SourceBuilder for InternalMetricsConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(InternalMetrics))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(EventType::Metric)];

        OUTPUTS
    }
}

impl MemoryBounds for InternalMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder
            .minimum()
            .with_single_value::<InternalMetrics>("component struct");
    }
}

pub struct InternalMetrics;

#[async_trait]
impl Source for InternalMetrics {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let mut metrics_stream = MetricsStream::register();

        health.mark_ready();
        debug!("Internal Metrics source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                maybe_metrics = metrics_stream.next() => match maybe_metrics {
                    Some(metrics) => {
                        debug!(metrics_len = metrics.len(), "Received internal metrics.");

                        let events = Arc::unwrap_or_clone(metrics);
                        if let Err(e) = context.forwarder().forward(events).await {
                            error!(error = %e, "Failed to forward events.");
                        }
                    },
                    None => {
                        error!("Internal metrics stream ended unexpectedly.");
                        break;
                    },
                },
            }
        }

        debug!("Internal Metrics source stopped.");

        Ok(())
    }
}
