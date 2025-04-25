use saluki_core::{components::transforms::*, topology::interconnect::FixedSizeEventBuffer};
use saluki_error::GenericError;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use async_trait::async_trait;

/// PreaggregationFilter synchronous transform.
///
/// Filters out metrics that are sketches, allowing only non-sketch metrics to pass through.
#[derive(Default)]
pub struct PreaggregationFilterConfiguration {}

impl MemoryBounds for PreaggregationFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<PreaggregationFilter>("component struct");
    }
}

#[async_trait]
impl SynchronousTransformBuilder for PreaggregationFilterConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(PreaggregationFilter {}))
    }
}

#[derive(Default)]
pub struct PreaggregationFilter {}

impl SynchronousTransform for PreaggregationFilter {
    fn transform_buffer(&mut self, event_buffer: &mut FixedSizeEventBuffer) {
        // Extract and discard sketch metrics
        let _ = event_buffer.extract(|event| {
            if let Some(metric) = event.try_as_metric() {
                metric.values().is_sketch()
            } else {
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;
    use saluki_event::{metric::Metric, Event};

    use super::*;

    #[test]
    fn test_filter_sketches() {
        let mut filter = PreaggregationFilter::default();
        let mut buffer = FixedSizeEventBuffer::for_test(10);

        // Add a non-sketch metric
        let non_sketch_metric = Metric::gauge(
            Context::from_static_parts("test", &[]),
            1.0,
        );
        buffer.try_push(Event::Metric(non_sketch_metric));

        // Add a sketch metric
        let sketch_metric = Metric::distribution(
            Context::from_static_parts("test", &[]),
            &[1.0, 2.0, 3.0][..],
        );
        buffer.try_push(Event::Metric(sketch_metric));

        // Apply the filter
        filter.transform_buffer(&mut buffer);

        // Verify only the non-sketch metric remains
        assert_eq!(buffer.len(), 1);
        let remaining_event = buffer.into_iter().next().unwrap();
        let remaining_metric = remaining_event.try_as_metric().unwrap();
        assert!(!remaining_metric.values().is_sketch());
    }
} 