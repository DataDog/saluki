//! DogStatsD metric prefix and listener-side metric filter transform.

use agent_data_plane_config::{domains::dogstatsd::MetricFilter, Live};
use async_trait::async_trait;
use metrics::{Counter, Gauge};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::data_model::event::{metric::Metric, EventType};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    observability::ComponentMetricsExt as _,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::{debug, error};

use crate::components::dogstatsd_filterlist::Blocklist;

const METRIC_FILTERLIST_SIZE_METRIC: &str = "metric_filterlist_size";
const METRIC_FILTERLIST_UPDATES_METRIC: &str = "metric_filterlist_updates_total";
const LISTENER_FILTERED_POINTS_METRIC: &str = "dogstatsd_listener_filtered_points_total";

/// DogStatsD prefix filter transform.
///
/// Appends a prefix to every metric if specified.
///
/// Checks if a metric name should be allowed.
pub struct DogStatsDPrefixFilterConfiguration {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    metric_filter: Live<MetricFilter>,
}

impl DogStatsDPrefixFilterConfiguration {
    /// Creates a new `DogStatsDPrefixFilterConfiguration`.
    pub fn new(metric_prefix: String, metric_prefix_blocklist: Vec<String>, metric_filter: Live<MetricFilter>) -> Self {
        Self {
            metric_prefix,
            metric_prefix_blocklist,
            metric_filter,
        }
    }
}

#[async_trait]
impl TransformBuilder for DogStatsDPrefixFilterConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        // Ensure our metric prefix has a trailing period so that we don't have to check for, and possibly add it, when we're
        // actually processing metrics.
        let mut metric_prefix = self.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = FilterlistTelemetry::new(&metrics_builder);
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: self.metric_prefix_blocklist.clone(),
            matcher: Blocklist::default(),
            metric_filter: self.metric_filter.clone(),
            telemetry,
        };
        filter.sync_matcher(false);

        Ok(Box::new(filter))
    }
}

impl MemoryBounds for DogStatsDPrefixFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder
            .minimum()
            .with_single_value::<DogStatsDPrefixFilter>("component struct");
    }
}

#[derive(Clone)]
struct FilterlistTelemetry {
    filterlist_size: Gauge,
    filterlist_updates: Counter,
    listener_filtered_points: Counter,
}

impl FilterlistTelemetry {
    fn new(builder: &MetricsBuilder) -> Self {
        Self {
            filterlist_size: builder.register_gauge(METRIC_FILTERLIST_SIZE_METRIC),
            filterlist_updates: builder.register_counter(METRIC_FILTERLIST_UPDATES_METRIC),
            listener_filtered_points: builder.register_counter(LISTENER_FILTERED_POINTS_METRIC),
        }
    }

    #[cfg(test)]
    fn noop() -> Self {
        Self {
            filterlist_size: Gauge::noop(),
            filterlist_updates: Counter::noop(),
            listener_filtered_points: Counter::noop(),
        }
    }

    fn increment_filterlist_updates(&self) {
        self.filterlist_updates.increment(1);
    }

    fn increment_listener_filtered_points(&self) {
        self.listener_filtered_points.increment(1);
    }

    fn set_filterlist_size(&self, size: usize) {
        self.filterlist_size.set(size as f64);
    }
}

struct DogStatsDPrefixFilter {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    matcher: Blocklist,
    metric_filter: Live<MetricFilter>,
    telemetry: FilterlistTelemetry,
}

impl DogStatsDPrefixFilter {
    fn sync_matcher(&mut self, count_update: bool) {
        self.matcher = Blocklist::new(
            self.metric_filter.values.iter().map(String::as_str),
            self.metric_filter.match_prefix,
        );
        self.telemetry.set_filterlist_size(self.metric_filter.values.len());
        if count_update {
            self.telemetry.increment_filterlist_updates();
        }
    }

    fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().as_ref();

        if self.metric_prefix.is_empty() {
            if self.matcher.contains(metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", metric_name);
                return false;
            }
        } else {
            // We don't want to prefix the metric if it has a prefix that is on our _prefix_ blocklist,
            // which ensures we don't prefix metrics that are already prefixed.
            let new_metric_name = if self.has_excluded_prefix(metric_name) {
                metric.context().name().clone()
            } else {
                let mut prefixed_metric_name = self.metric_prefix.clone();
                prefixed_metric_name.push_str(metric_name);
                prefixed_metric_name.into()
            };

            if self.matcher.contains(&new_metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", new_metric_name);
                return false;
            }

            // Update metric with new name.
            let new_context = metric.context().with_name(new_metric_name);
            let existing_context = metric.context_mut();
            *existing_context = new_context;
        }

        true
    }

    fn has_excluded_prefix(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blocklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }
}

#[async_trait]
impl Transform for DogStatsDPrefixFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("DogStatsD Prefix Filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        events.remove_if(|event| match event.try_as_metric_mut() {
                            // `process_metric` returns `true` if the metric should be kept, so we have to invert that
                            // here to match the predicate structure, which will _remove_ the event if `true` is returned.
                            Some(metric) => !self.process_metric(metric),
                            None => true,
                        });

                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                },
                metric_filter = self.metric_filter.changed() => {
                    self.sync_matcher(true);
                    debug!(?metric_filter, "Updated metric filter.");
                },
            }
        }

        debug!("DogStatsD Prefix Filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use metrics::set_default_local_recorder;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    /// Builds a [`DogStatsDPrefixFilter`] with defaults for fields a test does not vary.
    struct FilterBuilder {
        metric_prefix: String,
        metric_prefix_blocklist: Vec<String>,
        matcher: Blocklist,
        metric_filter: Live<MetricFilter>,
        telemetry: FilterlistTelemetry,
    }

    impl FilterBuilder {
        fn new() -> Self {
            Self {
                metric_prefix: String::new(),
                metric_prefix_blocklist: Vec::new(),
                matcher: Blocklist::default(),
                metric_filter: Live::new_fixed(MetricFilter::default()),
                telemetry: FilterlistTelemetry::noop(),
            }
        }

        fn prefix(mut self, prefix: &str) -> Self {
            self.metric_prefix = prefix.to_string();
            self
        }

        fn prefix_blocklist(mut self, entries: &[&str]) -> Self {
            self.metric_prefix_blocklist = entries.iter().map(|s| s.to_string()).collect();
            self
        }

        fn matcher(mut self, matcher: Blocklist) -> Self {
            self.matcher = matcher;
            self
        }

        fn metric_filter(mut self, metric_filter: Live<MetricFilter>) -> Self {
            self.metric_filter = metric_filter;
            self
        }

        fn telemetry(mut self, telemetry: FilterlistTelemetry) -> Self {
            self.telemetry = telemetry;
            self
        }

        fn build(self) -> DogStatsDPrefixFilter {
            DogStatsDPrefixFilter {
                metric_prefix: self.metric_prefix,
                metric_prefix_blocklist: self.metric_prefix_blocklist,
                matcher: self.matcher,
                metric_filter: self.metric_filter,
                telemetry: self.telemetry,
            }
        }
    }

    #[test]
    fn prefix_is_prepended_to_metric_name() {
        let filter = FilterBuilder::new().prefix("foo.").build();

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn metric_already_matching_prefix_blocklist_is_not_prefixed() {
        let filter = FilterBuilder::new()
            .prefix("foo")
            .prefix_blocklist(&["foo", "bar"])
            .build();

        let mut metric = Metric::gauge("barbar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "barbar");
    }

    #[test]
    fn metric_on_blocklist_is_dropped_and_others_pass() {
        let filter = FilterBuilder::new()
            .matcher(Blocklist::new(["foobar", "test"], false))
            .build();

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn blocklist_matches_against_the_prefixed_name() {
        let filter = FilterBuilder::new()
            .prefix("foo.")
            .matcher(Blocklist::new(["foo.bar", "test"], false))
            .build();

        // "bar" is prefixed to "foo.bar", which the blocklist matches.
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // A metric already on the prefix blocklist keeps its name, so the added prefix is skipped and it passes.
        let filter = FilterBuilder::new().prefix("foo.").prefix_blocklist(&["foo"]).build();

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn prefix_matching_blocklist_drops_metrics_by_prefix() {
        let filter = FilterBuilder::new()
            .matcher(Blocklist::new(["b", "test"], true))
            .build();

        // match prefix is true, "bar" has prefix "b"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // match prefix is true, "test" has prefix "test"
        let mut metric = Metric::gauge("test", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn prefix_matching_blocklist_applies_after_prefixing() {
        let filter = FilterBuilder::new()
            .prefix("foo")
            .matcher(Blocklist::new(["fo", "test"], true))
            .build();

        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn typed_live_updates_rebuild_the_matcher() {
        let mut initial = agent_data_plane_config::SalukiConfiguration::default();
        initial.domains.dogstatsd.metric_filter = MetricFilter {
            values: vec!["foobar".to_string(), "test".to_string()],
            match_prefix: false,
        };
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(initial));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let metric_filter = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.metric_filter
        });
        let mut filter = FilterBuilder::new().metric_filter(metric_filter).build();
        filter.sync_matcher(false);

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.metric_filter = MetricFilter {
            values: vec!["foo".to_string()],
            match_prefix: true,
        };
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        tokio::time::timeout(Duration::from_secs(2), filter.metric_filter.changed())
            .await
            .expect("timed out waiting for typed metric filter update");
        filter.sync_matcher(true);

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));
        assert_eq!(filter.matcher, Blocklist::new(["foo"], true));
    }

    #[test]
    fn telemetry_counts_typed_filter_updates() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let metric_filter = Live::new_fixed(MetricFilter {
            values: vec!["preferred".to_string()],
            match_prefix: false,
        });
        let mut filter = FilterBuilder::new()
            .metric_filter(metric_filter)
            .telemetry(telemetry)
            .build();

        filter.sync_matcher(false);
        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(0));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(1.0));

        filter.metric_filter = Live::new_fixed(MetricFilter {
            values: vec!["foo".to_string(), "foobar".to_string()],
            match_prefix: true,
        });
        filter.sync_matcher(true);

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(2.0));

        let mut metric = Metric::gauge("foobar.baz", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_counts_listener_filtered_points() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let filter = FilterBuilder::new()
            .matcher(Blocklist::new(["foo", "bar"], true))
            .telemetry(telemetry)
            .build();

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}
