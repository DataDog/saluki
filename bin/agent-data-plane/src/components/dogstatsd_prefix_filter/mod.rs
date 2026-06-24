//! DogStatsD metric prefix and listener-side metric filter transform.

use async_trait::async_trait;
use metrics::{Counter, Gauge};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::dogstatsd::PrefixFilterConfig;
use saluki_component_config::DynamicConfig;
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

/// DogStatsD prefix filter transform builder.
///
/// Builds a [`DogStatsDPrefixFilter`] from a typed dynamic config handle. The builder holds no
/// raw-map references; it receives the initial and live prefix-filter configuration through a
/// [`DynamicConfig`] handle.
pub struct DogStatsDPrefixFilterBuilder {
    config: DynamicConfig<PrefixFilterConfig>,
}

impl DogStatsDPrefixFilterBuilder {
    /// Creates a new builder backed by the given typed config handle.
    pub fn new(config: DynamicConfig<PrefixFilterConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl TransformBuilder for DogStatsDPrefixFilterBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let snapshot = self.config.current();

        let mut metric_prefix = snapshot.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with('.') {
            metric_prefix.push('.');
        }

        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let effective_filterlist = EffectiveFilterlist::new(
            snapshot.metric_filterlist.clone(),
            snapshot.metric_filterlist_match_prefix,
            snapshot.metric_blocklist.clone(),
            snapshot.metric_blocklist_match_prefix,
        );
        let telemetry = FilterlistTelemetry::new(&metrics_builder);

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: snapshot.metric_prefix_blocklist.clone(),
            matcher: Blocklist::default(),
            effective_filterlist,
            telemetry,
            config: self.config.clone(),
        };
        filter.sync_effective_blocklist(false);

        Ok(Box::new(filter))
    }
}

impl MemoryBounds for DogStatsDPrefixFilterBuilder {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
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
    config: DynamicConfig<PrefixFilterConfig>,
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

    fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().as_ref();

        if self.metric_prefix.is_empty() {
            if self.matcher.contains(metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", metric_name);
                return false;
            }
        } else {
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
                    let new_config = self.config.current();
                    let new_filterlist = EffectiveFilterlist::new(
                        new_config.metric_filterlist,
                        new_config.metric_filterlist_match_prefix,
                        new_config.metric_blocklist,
                        new_config.metric_blocklist_match_prefix,
                    );
                    if new_filterlist != self.effective_filterlist {
                        self.effective_filterlist = new_filterlist;
                        self.sync_effective_blocklist(true);
                    }
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
    use saluki_component_config::DynamicConfig;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    fn fixed_filter(pf: PrefixFilterConfig) -> DogStatsDPrefixFilter {
        let effective_filterlist = EffectiveFilterlist::new(
            pf.metric_filterlist.clone(),
            pf.metric_filterlist_match_prefix,
            pf.metric_blocklist.clone(),
            pf.metric_blocklist_match_prefix,
        );
        DogStatsDPrefixFilter {
            metric_prefix: {
                let mut p = pf.metric_prefix.clone();
                if !p.is_empty() && !p.ends_with('.') {
                    p.push('.');
                }
                p
            },
            metric_prefix_blocklist: pf.metric_prefix_blocklist.clone(),
            matcher: effective_filterlist.to_matcher(),
            effective_filterlist,
            telemetry: FilterlistTelemetry::noop(),
            config: DynamicConfig::fixed(pf),
        }
    }

    #[test]
    fn test_metric_prefix_add() {
        let filter = fixed_filter(PrefixFilterConfig {
            metric_prefix: "foo".to_string(),
            ..Default::default()
        });

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blocklist() {
        let filter = fixed_filter(PrefixFilterConfig {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string(), "bar".to_string()],
            ..Default::default()
        });

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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string()],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn dynamic_update_changes_filterlist() {
        let (tx, rx) = tokio::sync::watch::channel(PrefixFilterConfig {
            metric_blocklist: vec!["foobar".to_string(), "test".to_string()],
            ..Default::default()
        });

        let mut handle = DynamicConfig::live(PrefixFilterConfig::default(), rx);
        handle.changed().await;
        let snap = handle.current();

        let effective_filterlist = EffectiveFilterlist::new(
            snap.metric_filterlist.clone(),
            snap.metric_filterlist_match_prefix,
            snap.metric_blocklist.clone(),
            snap.metric_blocklist_match_prefix,
        );

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: effective_filterlist.to_matcher(),
            effective_filterlist,
            telemetry: FilterlistTelemetry::noop(),
            config: handle,
        };

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        tx.send(PrefixFilterConfig {
            metric_blocklist: vec!["foo".to_string()],
            ..Default::default()
        })
        .unwrap();

        filter.config.changed().await;
        let new_config = filter.config.current();
        let new_filterlist = EffectiveFilterlist::new(
            new_config.metric_filterlist,
            new_config.metric_filterlist_match_prefix,
            new_config.metric_blocklist,
            new_config.metric_blocklist_match_prefix,
        );
        filter.effective_filterlist = new_filterlist;
        filter.sync_effective_blocklist(true);

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_only_counts_active_filterlist_updates() {
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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
        };

        filter.sync_effective_blocklist(false);

        let new_filterlist = EffectiveFilterlist::new(
            vec!["preferred".to_string()],
            false,
            vec!["ignored".to_string(), "still_ignored".to_string()],
            false,
        );
        // Blocklist update while filterlist is active: should not count as update.
        filter.effective_filterlist = new_filterlist;
        filter.sync_effective_blocklist(false);

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(0));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(1.0));

        let mut metric = Metric::gauge("preferred", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("ignored", 1.0);
        assert!(filter.process_metric(&mut metric));

        // Clear filterlist: blocklist becomes active, counts as update.
        let new_filterlist = EffectiveFilterlist::new(
            Vec::new(),
            false,
            vec!["ignored".to_string(), "still_ignored".to_string()],
            false,
        );
        filter.effective_filterlist = new_filterlist;
        filter.sync_effective_blocklist(true);

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(2.0));

        let mut metric = Metric::gauge("ignored", 1.0);
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
            config: DynamicConfig::fixed(PrefixFilterConfig::default()),
        };

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}
