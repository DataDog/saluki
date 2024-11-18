use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::destinations::*;
use saluki_error::GenericError;
use saluki_event::DataType;
use tokio::select;
use tracing::debug;

/// Flare destination.
///
/// Accepts all different event types, and selectively keeps track of ones relevant to the health of the data plane. The
/// data is kept in memory in order to be able to service flare requests from the Datadog Agent, where this data will be
/// exposed in a number of different files that are added to the flare.
///
/// ## Metrics
///
/// In order to provide a consistent format and structure for the metrics, metrics are handled in the following way:
/// - a maximum number of metrics are kept in memory (additional metrics beyond this number are dropped)
/// - metrics are kept for a maximum duration (metrics older than this duration are dropped)
/// - rates are normalized to counters
/// - gauges are kept as is
/// - histograms are converted to distributions
/// - sets are ignored entirely
/// - distributions (including the ones converted from histograms) are denormalized to the following aggregates:
///   - count
///   - sum
///   - minimum
///   - maximum
///   - average
///   - multiple percentiles: 0.5, 0.95, 0.99, 0.999
///
/// ## Bounds
///
/// The memory bounds for the Flare destination are based on the number of metrics stored.
///
/// Metrics have a minimum timestamp granularity of one second, which means that that the maximum number of data points
/// held at any given point in time is `max_metrics * metrics_history_secs`. For each data point, we hold the timestamp
/// (8 bytes) and the value. The value is generally 8 bytes (counters and gauges) but could be up to 72 bytes for
/// distributions (nine 8-byte floats). The maximum memory usage is therefore: `max_metrics * metrics_history_secs *
/// (8 + 72)`.
///
/// For example, with a maximum of 500 metrics and a maximum history of 300 seconds (5 minutes), the maximum memory
/// usage would be 12MB (500 * 300 * (8 + 72) => 12,000,000).
#[derive(Default)]
pub struct FlareConfiguration {
    /// Maximum number of metrics to hold on to.
    ///
    /// Any additional metrics will be dropped.
    max_metrics: usize,

    /// Maximum duration to keep metrics for.
    ///
    /// Metrics whose last data point is older than `metrics_history_secs` ago will be removed.
    metrics_history_secs: usize,
}

#[async_trait]
impl DestinationBuilder for FlareConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::all_bits()
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Flare {
            max_metrics: self.max_metrics,
            metrics_history_secs: self.metrics_history_secs,
        }))
    }
}

impl MemoryBounds for FlareConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<Flare>();
    }
}

struct Flare {
    max_metrics: usize,
    metrics_history_secs: usize,
}

#[async_trait]
impl Destination for Flare {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("Flare destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                result = context.events().next() => match result {
                    Some(_events) => {},
                    None => break,
                },
            }
        }

        debug!("Flare destination stopped.");

        Ok(())
    }
}
