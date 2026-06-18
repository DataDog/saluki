//! DogStatsD metric prefix and listener-side metric filter transform.

use async_trait::async_trait;
use metrics::{Counter, Gauge};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::dogstatsd::DogStatsDPrefixFilterConfig;
use saluki_component_config::ScopedConfig;
use saluki_core::data_model::event::{metric::Metric, EventType};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    observability::ComponentMetricsExt as _,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::{debug, error};

use crate::components::dogstatsd_filterlist::{Blocklist, EffectiveFilterlist};

const METRIC_FILTERLIST_SIZE_METRIC: &str = "metric_filterlist_size";
const METRIC_FILTERLIST_UPDATES_METRIC: &str = "metric_filterlist_updates_total";
const LISTENER_FILTERED_POINTS_METRIC: &str = "dogstatsd_listener_filtered_points_total";

/// DogStatsD prefix filter transform.
///
/// Appends a prefix to every metric if specified, and filters metric names against the (dynamic)
/// filterlist/blocklist.
///
/// The configuration arrives as a typed [`ScopedConfig<DogStatsDPrefixFilterConfig>`]; the transform
/// reacts to runtime updates published on that handle by rebuilding its effective filterlist.
pub struct DogStatsDPrefixFilterConfiguration {
    config: ScopedConfig<DogStatsDPrefixFilterConfig>,
}

/// Normalizes a metric prefix to end with a trailing period (so the processing path can concatenate
/// without re-checking).
fn normalize_metric_prefix(metric_prefix: &str) -> String {
    let mut metric_prefix = metric_prefix.to_string();
    if !metric_prefix.is_empty() && !metric_prefix.ends_with('.') {
        metric_prefix.push('.');
    }
    metric_prefix
}

impl DogStatsDPrefixFilterConfiguration {
    /// Creates a new `DogStatsDPrefixFilterConfiguration` from the given native configuration handle.
    pub fn from_native(config: ScopedConfig<DogStatsDPrefixFilterConfig>) -> Self {
        Self { config }
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

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let current = self.config.current();
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let effective_filterlist = EffectiveFilterlist::new(
            current.metric_filterlist.clone(),
            current.metric_filterlist_match_prefix,
            current.metric_blocklist.clone(),
            current.metric_blocklist_match_prefix,
        );
        let telemetry = FilterlistTelemetry::new(&metrics_builder);
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: normalize_metric_prefix(&current.metric_prefix),
            metric_prefix_blocklist: current.metric_prefix_blocklist.clone(),
            matcher: Blocklist::default(),
            effective_filterlist,
            telemetry,
            config: self.config.clone(),
        };
        filter.sync_effective_blocklist(false);

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
    effective_filterlist: EffectiveFilterlist,
    telemetry: FilterlistTelemetry,
    config: ScopedConfig<DogStatsDPrefixFilterConfig>,
}

impl DogStatsDPrefixFilter {
    fn sync_effective_blocklist(&mut self, count_update: bool) {
        self.matcher = self.effective_filterlist.to_matcher();
        self.telemetry
            .set_filterlist_size(self.effective_filterlist.effective_len());
        if count_update {
            self.telemetry.increment_filterlist_updates();
        }
    }

    /// Rebuilds the effective filterlist from the latest published configuration slice.
    ///
    /// The dynamic source keys (`metric_filterlist`, `metric_filterlist_match_prefix`,
    /// `statsd_metric_blocklist`, `statsd_metric_blocklist_match_prefix`) are retranslated into the
    /// whole `DogStatsDPrefixFilterConfig` slice by the config-system; this rebuilds local state from
    /// it. The static metric-prefix settings are also refreshed so a published update is fully
    /// reflected.
    fn apply_update(&mut self, updated: DogStatsDPrefixFilterConfig) {
        let filterlist_active_before = self.effective_filterlist.metric_filterlist_is_active();
        let filterlist_active_after = !updated.metric_filterlist.is_empty();

        self.metric_prefix = normalize_metric_prefix(&updated.metric_prefix);
        self.metric_prefix_blocklist = updated.metric_prefix_blocklist;
        self.effective_filterlist = EffectiveFilterlist::new(
            updated.metric_filterlist,
            updated.metric_filterlist_match_prefix,
            updated.metric_blocklist,
            updated.metric_blocklist_match_prefix,
        );

        // Count an update if the active (effective) filter changed identity or was reconfigured while
        // active, mirroring the previous per-key counting behavior at slice granularity.
        let count_update = filterlist_active_before || filterlist_active_after;
        self.sync_effective_blocklist(count_update);
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
                _ = self.config.changed() => {
                    let updated = self.config.current();
                    debug!("Updated DogStatsD prefix/metric filterlist.");
                    self.apply_update(updated);
                },
            }
        }

        debug!("DogStatsD Prefix Filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use metrics::set_default_local_recorder;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    fn fixed_config() -> ScopedConfig<DogStatsDPrefixFilterConfig> {
        ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default())
    }

    #[test]
    fn test_metric_prefix_add() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blocklist() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string(), "bar".to_string()],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        let mut metric = Metric::gauge("barbar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "barbar");
    }

    #[test]
    fn test_metric_blocklist() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foobar", "test"], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_blocklist_with_metric_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo.bar", "test"], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string()],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_match_prefix_without_added_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["b", "test"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        // match prefix is true, "bar" has prefix "b"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // match prefix is true, "test" has prefix "test"
        let mut metric = Metric::gauge("test", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn test_metric_match_prefix_with_added_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["fo", "test"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };

        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    fn prefix_config(
        metric_filterlist: Vec<&str>, metric_filterlist_match_prefix: bool, metric_blocklist: Vec<&str>,
    ) -> DogStatsDPrefixFilterConfig {
        DogStatsDPrefixFilterConfig {
            metric_prefix: String::new(),
            metric_prefix_blocklist: vec![],
            metric_filterlist: metric_filterlist.into_iter().map(ToString::to_string).collect(),
            metric_filterlist_match_prefix,
            metric_blocklist: metric_blocklist.into_iter().map(ToString::to_string).collect(),
            metric_blocklist_match_prefix: false,
        }
    }

    #[test]
    fn apply_update_rebuilds_effective_matcher() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(
                vec!["preferred".to_string()],
                false,
                vec!["legacy".to_string()],
                false,
            ),
            telemetry,
            config: fixed_config(),
        };

        filter.sync_effective_blocklist(false);
        // The filterlist is active, so `preferred` is the active matcher; `legacy` (blocklist) is not.
        let mut metric = Metric::gauge("preferred", 1.0);
        assert!(!filter.process_metric(&mut metric));
        let mut metric = Metric::gauge("legacy", 1.0);
        assert!(filter.process_metric(&mut metric));

        // Clearing the filterlist activates the legacy blocklist; this is a coarse per-slice update.
        filter.apply_update(prefix_config(vec![], false, vec!["legacy"]));
        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));

        let mut metric = Metric::gauge("legacy", 1.0);
        assert!(!filter.process_metric(&mut metric));
        let mut metric = Metric::gauge("preferred", 1.0);
        assert!(filter.process_metric(&mut metric));
    }

    #[test]
    fn apply_update_match_prefix_takes_effect() {
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(vec!["foo".to_string()], false, Vec::new(), false),
            telemetry: FilterlistTelemetry::noop(),
            config: fixed_config(),
        };
        filter.sync_effective_blocklist(false);

        // Exact match: `foo.bar` is not filtered until prefix matching is enabled.
        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(filter.process_metric(&mut metric));

        filter.apply_update(prefix_config(vec!["foo"], true, vec![]));

        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_counts_listener_filtered_points() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo", "bar"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry,
            config: fixed_config(),
        };

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}
