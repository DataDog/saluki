use std::{collections::HashSet, sync::LazyLock};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::select;
use tracing::{debug, error};

/// Metric router transform configuration.
#[derive(Deserialize, Clone)]
pub struct MetricRouterConfiguration {
    /// List of metric names to match against.
    pub metric_names: Vec<String>,
}

/// Routes incoming events based on exact metric name matching to one of two
/// named outputs: "matched" for metrics with names that exactly match the
/// configured list, and "unmatched" for all other events. This transform
/// demonstrates the use of named outputs without a default output.
#[derive(Debug)]
pub struct MetricRouter {
    metric_names: HashSet<String>,
}

impl MetricRouter {
    fn new(config: MetricRouterConfiguration) -> Result<Self, GenericError> {
        for name in &config.metric_names {
            if name.trim().is_empty() {
                return Err(generic_error!("Metric names cannot be empty or whitespace-only"));
            }
        }

        Ok(Self {
            metric_names: config.metric_names.into_iter().collect(),
        })
    }

    /// Evaluates whether the event should be routed to the matched output.
    ///
    /// Returns true if the event is a metric with a name that exactly matches
    /// one of the configured metric names, false otherwise.
    fn should_route_to_matched(&self, event: &Event) -> bool {
        match event {
            Event::Metric(metric) => {
                // Check if the metric name exactly matches any of our configured names
                let metric_name = metric.context().name();
                self.metric_names.contains(metric_name.as_ref())
            }
            _ => {
                // Non-metric events are always routed to unmatched
                false
            }
        }
    }

    /// Processes a batch of events by routing them to matched or unmatched outputs.
    async fn process_event_batch(
        &self, mut events: EventsBuffer, context: &mut TransformContext,
    ) -> Result<(), GenericError> {
        // Extract matched events from the buffer, leaving unmatched ones behind
        let matched_events = events.extract(|event| self.should_route_to_matched(event));

        let matched_count = context
            .dispatcher()
            .buffered_named("matched")?
            .send_all(matched_events)
            .await?;

        let unmatched_count = context
            .dispatcher()
            .buffered_named("unmatched")?
            .send_all(events)
            .await?;

        debug!(
            matched_events = matched_count,
            unmatched_events = unmatched_count,
            "Successfully processed event batch."
        );

        Ok(())
    }
}

#[async_trait]
impl TransformBuilder for MetricRouterConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MetricRouter::new(self.clone())?))
    }

    fn input_event_type(&self) -> EventType {
        EventType::all_bits()
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("matched", EventType::all_bits()),
                OutputDefinition::named_output("unmatched", EventType::all_bits()),
            ]
        });
        &OUTPUTS
    }
}

impl MemoryBounds for MetricRouterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Account for the MetricRouter struct itself when heap-allocated
            .with_single_value::<MetricRouter>("component struct")
            // Account for the HashSet<String> that stores metric names
            // HashSet has overhead beyond just the strings themselves
            .with_fixed_amount(
                "hashset overhead",
                std::mem::size_of::<std::collections::HashSet<String>>(),
            )
            // Account for the String allocations for each metric name
            // This approximates the heap usage of the strings themselves
            .with_fixed_amount(
                "metric names strings",
                self.metric_names
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>(),
            )
            // Account for HashSet bucket overhead (rough approximation)
            // HashSet typically allocates more buckets than items for good performance
            .with_fixed_amount(
                "hashset buckets",
                self.metric_names.len() * std::mem::size_of::<Option<String>>() * 2, // Rough 2x bucket overhead
            );
    }
}

#[async_trait]
impl Transform for MetricRouter {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!(
            metric_names = ?self.metric_names,
            "Metric Router transform started."
        );

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        // Process the batch of events with proper error handling
                        if let Err(e) = self.process_event_batch(events, &mut context).await {
                            error!(error = %e, "Failed to process event batch.");
                            // Continue processing subsequent batches even if one fails
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down metric router transform");
                        break;
                    }
                }
            }
        }

        debug!("Metric Router transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::{metric::Metric, Event};

    use super::*;

    #[test]
    fn test_exact_metric_name_matching() {
        let router = MetricRouter::new(MetricRouterConfiguration {
            metric_names: vec!["test.counter".to_string(), "another.metric".to_string()],
        })
        .expect("Should create router successfully");

        // Test metric event with matching name
        let matching_metric = Event::Metric(Metric::counter("test.counter", 1.0));
        assert!(router.should_route_to_matched(&matching_metric));

        // Test metric event with another matching name
        let another_matching_metric = Event::Metric(Metric::counter("another.metric", 5.0));
        assert!(router.should_route_to_matched(&another_matching_metric));

        // Test metric event with non-matching name
        let non_matching_metric = Event::Metric(Metric::counter("different.metric", 1.0));
        assert!(!router.should_route_to_matched(&non_matching_metric));

        // Test partial match (should fail - we want exact matching)
        let partial_match_metric = Event::Metric(Metric::counter("test.counter.extra", 1.0));
        assert!(!router.should_route_to_matched(&partial_match_metric));
    }

    #[test]
    fn test_empty_metric_names_list() {
        let router = MetricRouter::new(MetricRouterConfiguration { metric_names: vec![] })
            .expect("Should create router successfully");

        // No metrics should be routed to matched when the list is empty
        let metric_event = Event::Metric(Metric::counter("any.metric", 1.0));
        assert!(!router.should_route_to_matched(&metric_event));
    }

    #[test]
    fn test_input_validation() {
        // Test empty string validation
        let result = MetricRouter::new(MetricRouterConfiguration {
            metric_names: vec!["valid.metric".to_string(), "".to_string()],
        });
        assert!(result.is_err());
        if let Err(error) = result {
            assert!(error.to_string().contains("empty"));
        }

        // Test whitespace-only string validation
        let result = MetricRouter::new(MetricRouterConfiguration {
            metric_names: vec!["valid.metric".to_string(), "   ".to_string()],
        });
        assert!(result.is_err());
        if let Err(error) = result {
            assert!(error.to_string().contains("empty"));
        }

        // Test valid configuration
        let result = MetricRouter::new(MetricRouterConfiguration {
            metric_names: vec!["valid.metric".to_string(), "another.valid".to_string()],
        });
        assert!(result.is_ok());
    }
}
