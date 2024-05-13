use async_trait::async_trait;
use saluki_core::{components::sources::*, observability::metrics::MetricsReceiver, topology::OutputDefinition};
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
        Ok(Box::new(InternalMetrics {}))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct InternalMetrics;

#[async_trait]
impl Source for InternalMetrics {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");
        tokio::pin!(global_shutdown);

        let mut receiver = MetricsReceiver::register();

        debug!("Internal Metrics source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                }
                maybe_metrics = receiver.next() => match maybe_metrics {
                    Some(metrics) => {
                        debug!(metrics_len = metrics.len(), "Received internal metrics.");

                        let mut event_buffer = context.event_buffer_pool().acquire().await;
                        event_buffer.extend(metrics.iter().cloned());

                        if let Err(e) = context.forwarder().forward(event_buffer).await {
                            error!(error = %e, "Failed to forward events.");
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Internal Metrics source stopped.");

        Ok(())
    }
}
