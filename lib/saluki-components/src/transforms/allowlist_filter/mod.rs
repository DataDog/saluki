//! Metric allowlist filter transform.

use std::collections::HashSet;

use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use serde::Deserialize;
use tokio::select;
use tracing::{debug, error};

/// Allowlist filter transform configuration.
#[derive(Clone, Deserialize)]
pub struct AllowlistFilterConfiguration {
    /// Metric names to retain.
    ///
    /// When empty, all metric events pass through unchanged.
    pub metric_names: Vec<String>,
}

/// Filters metrics by exact metric name.
#[derive(Debug)]
pub struct AllowlistFilter {
    metric_names: HashSet<String>,
}

impl AllowlistFilter {
    fn new(config: AllowlistFilterConfiguration) -> Self {
        Self {
            metric_names: config.metric_names.into_iter().collect(),
        }
    }

    fn should_retain_event(&self, event: &Event) -> bool {
        let Event::Metric(metric) = event else {
            return false;
        };

        self.metric_names.is_empty() || self.metric_names.contains(metric.context().name().as_ref())
    }

    async fn process_event_batch(
        &self, mut events: EventsBuffer, context: &mut TransformContext,
    ) -> Result<(), GenericError> {
        let input_count = events.len();
        events.remove_if(|event| !self.should_retain_event(event));
        let retained_count = events.len();
        let dropped_count = input_count.saturating_sub(retained_count);

        let sent_count = context.dispatcher().buffered()?.send_all(events).await?;
        debug!(
            retained_events = sent_count,
            dropped_events = dropped_count,
            "Successfully processed event batch."
        );

        Ok(())
    }
}

#[async_trait]
impl TransformBuilder for AllowlistFilterConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(AllowlistFilter::new(self.clone())))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for AllowlistFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<AllowlistFilter>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                "metric names strings",
                self.metric_names
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>(),
            )
            .with_fixed_amount(
                "hashset buckets",
                self.metric_names.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for AllowlistFilter {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!(metric_names = ?self.metric_names, "Allowlist filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = self.process_event_batch(events, &mut context).await {
                            error!(error = %e, "Failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down allowlist filter transform.");
                        break;
                    }
                }
            }
        }

        debug!("Allowlist filter transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::{metric::Metric, Event};

    use super::*;

    fn filter(metric_names: &[&str]) -> AllowlistFilter {
        AllowlistFilter::new(AllowlistFilterConfiguration {
            metric_names: metric_names.iter().map(|name| name.to_string()).collect(),
        })
    }

    #[test]
    fn empty_allowlist_retains_all_metrics() {
        let filter = filter(&[]);

        assert!(filter.should_retain_event(&Event::Metric(Metric::counter("first.metric", 1.0))));
        assert!(filter.should_retain_event(&Event::Metric(Metric::counter("second.metric", 1.0))));
    }

    #[test]
    fn non_empty_allowlist_retains_only_matching_metrics() {
        let filter = filter(&["allowed.metric", "also.allowed"]);

        assert!(filter.should_retain_event(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(filter.should_retain_event(&Event::Metric(Metric::counter("also.allowed", 1.0))));
        assert!(!filter.should_retain_event(&Event::Metric(Metric::counter("blocked.metric", 1.0))));
        assert!(!filter.should_retain_event(&Event::Metric(Metric::counter("allowed.metric.extra", 1.0))));
    }
}
