//! DogStatsD metric prefix and listener-side metric filter transform.

use agent_data_plane_config::{domains::dogstatsd::PrefixFilter, Live};
use async_trait::async_trait;
use metrics::{Counter, Gauge};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
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
/// Appends a prefix to every metric if specified.
///
/// Checks if a metric name should be allowed.
pub struct DogStatsDPrefixFilterConfiguration {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    live: Live<PrefixFilter>,
}

impl DogStatsDPrefixFilterConfiguration {
    /// Creates a new `DogStatsDPrefixFilterConfiguration` from the resolved prefix-filter settings.
    ///
    /// The metric namespace and its exemption list are read once; the filterlist and blocklist are
    /// tracked through `live` so the transform can rebuild its matcher on runtime updates.
    pub fn from_configuration(prefix_filter: &PrefixFilter, live: Live<PrefixFilter>) -> Result<Self, GenericError> {
        Ok(Self {
            metric_prefix: prefix_filter.metric_namespace.clone(),
            metric_prefix_blocklist: prefix_filter.metric_namespace_blocklist.clone(),
            live,
        })
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
        // Ensure our metric prefix has a trailing period so that we don't have to check for, and possibly add it, when we're
        // actually processing metrics.
        let mut metric_prefix = self.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let effective_filterlist = EffectiveFilterlist::from_prefix_filter(&self.live.current());
        let telemetry = FilterlistTelemetry::new(&metrics_builder);
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: self.metric_prefix_blocklist.clone(),
            matcher: Blocklist::default(),
            effective_filterlist,
            telemetry,
            live: self.live.clone(),
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
    live: Live<PrefixFilter>,
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

    /// Rebuilds the matcher from an updated prefix-filter slice.
    ///
    /// The update counter tracks reconfigurations of the *active* list, so it increments only when
    /// the effective values or match mode change; a change confined to the inactive list is a no-op.
    fn apply_update(&mut self, prefix_filter: &PrefixFilter) {
        let new_effective = EffectiveFilterlist::from_prefix_filter(prefix_filter);
        let count_update = new_effective.effective_values() != self.effective_filterlist.effective_values();
        self.effective_filterlist = new_effective;
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

        let mut live = self.live.clone();

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
                _ = live.changed() => {
                    let prefix_filter = live.current();
                    debug!(?prefix_filter, "Updated metric filterlist configuration.");
                    self.apply_update(&prefix_filter);
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

    use agent_data_plane_config::{domains::dogstatsd::PrefixFilter, Live, SalukiConfiguration};
    use arc_swap::ArcSwap;
    use metrics::set_default_local_recorder;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};
    use tokio::sync::watch;

    use super::*;

    /// Builds a runtime filter with a fixed live view, for the non-reactive matching tests.
    fn fixed_filter(
        metric_prefix: &str, metric_prefix_blocklist: Vec<String>, matcher: Blocklist,
        effective_filterlist: EffectiveFilterlist,
    ) -> DogStatsDPrefixFilter {
        DogStatsDPrefixFilter {
            metric_prefix: metric_prefix.to_string(),
            metric_prefix_blocklist,
            matcher,
            effective_filterlist,
            telemetry: FilterlistTelemetry::noop(),
            live: Live::fixed(PrefixFilter::default()),
        }
    }

    /// A [`Live`] view backed by a cell the test can flip, mirroring how the config system drives
    /// runtime updates to the prefix-filter slice.
    fn drivable_live(
        prefix_filter: PrefixFilter,
    ) -> (Arc<ArcSwap<SalukiConfiguration>>, watch::Sender<()>, Live<PrefixFilter>) {
        let cell = Arc::new(ArcSwap::from_pointee(config_with(prefix_filter)));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| &c.domains.dogstatsd.prefix_filter);
        (cell, tx, live)
    }

    fn config_with(prefix_filter: PrefixFilter) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.domains.dogstatsd.prefix_filter = prefix_filter;
        config
    }

    fn store_prefix_filter(cell: &ArcSwap<SalukiConfiguration>, tx: &watch::Sender<()>, prefix_filter: PrefixFilter) {
        cell.store(Arc::new(config_with(prefix_filter)));
        tx.send(()).expect("live cell should have a receiver");
    }

    #[test]
    fn test_metric_prefix_add() {
        let filter = fixed_filter("foo.", vec![], Blocklist::default(), EffectiveFilterlist::default());

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blocklist() {
        let filter = fixed_filter(
            "foo",
            vec!["foo".to_string(), "bar".to_string()],
            Blocklist::default(),
            EffectiveFilterlist::default(),
        );

        let mut metric = Metric::gauge("barbar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "barbar");
    }

    #[test]
    fn test_metric_blocklist() {
        let filter = fixed_filter(
            "",
            vec![],
            Blocklist::new(["foobar", "test"], false),
            EffectiveFilterlist::default(),
        );

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_blocklist_with_metric_prefix() {
        let filter = fixed_filter(
            "foo.",
            vec![],
            Blocklist::new(["foo.bar", "test"], false),
            EffectiveFilterlist::default(),
        );

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = fixed_filter(
            "foo.",
            vec!["foo".to_string()],
            Blocklist::default(),
            EffectiveFilterlist::default(),
        );

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_match_prefix_without_added_prefix() {
        let filter = fixed_filter(
            "",
            vec![],
            Blocklist::new(["b", "test"], true),
            EffectiveFilterlist::default(),
        );

        // match prefix is true, "bar" has prefix "b"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // match prefix is true, "test" has prefix "test"
        let mut metric = Metric::gauge("test", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn test_metric_match_prefix_with_added_prefix() {
        let filter = fixed_filter(
            "foo",
            vec![],
            Blocklist::new(["fo", "test"], true),
            EffectiveFilterlist::default(),
        );

        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn test_metric_blocklist_dynamic_update() {
        let base = PrefixFilter {
            metric_blocklist: vec!["foobar".to_string(), "test".to_string()],
            ..PrefixFilter::default()
        };
        let (cell, tx, live) = drivable_live(base.clone());

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: EffectiveFilterlist::from_prefix_filter(&base).to_matcher(),
            effective_filterlist: EffectiveFilterlist::from_prefix_filter(&base),
            telemetry: FilterlistTelemetry::noop(),
            live: live.clone(),
        };
        let mut live = live;

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");

        // Move "foo" onto the blocklist and drop the rest.
        store_prefix_filter(
            &cell,
            &tx,
            PrefixFilter {
                metric_blocklist: vec!["foo".to_string()],
                ..PrefixFilter::default()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), live.changed())
            .await
            .expect("timed out waiting for blocklist update");
        filter.apply_update(&live.current());

        // "foobar" is taken off the blocklist
        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foobar");

        // "foo" is added to the blocklist
        let mut metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // A non-empty filterlist takes precedence over the blocklist.
        store_prefix_filter(
            &cell,
            &tx,
            PrefixFilter {
                metric_filterlist: vec!["baz".to_string()],
                metric_blocklist: vec!["foo".to_string()],
                ..PrefixFilter::default()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), live.changed())
            .await
            .expect("timed out waiting for filterlist update");
        filter.apply_update(&live.current());

        // "baz" is added to the filterlist
        let mut metric = Metric::gauge("baz", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn test_metric_filterlist_match_prefix_dynamic_update_is_applied() {
        let base = PrefixFilter {
            metric_filterlist: vec!["foo".to_string()],
            metric_filterlist_match_prefix: false,
            ..PrefixFilter::default()
        };
        let (cell, tx, live) = drivable_live(base.clone());

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: EffectiveFilterlist::from_prefix_filter(&base).to_matcher(),
            effective_filterlist: EffectiveFilterlist::from_prefix_filter(&base),
            telemetry: FilterlistTelemetry::noop(),
            live: live.clone(),
        };
        let mut live = live;

        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(filter.process_metric(&mut metric));

        store_prefix_filter(
            &cell,
            &tx,
            PrefixFilter {
                metric_filterlist: vec!["foo".to_string()],
                metric_filterlist_match_prefix: true,
                ..PrefixFilter::default()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), live.changed())
            .await
            .expect("timed out waiting for metric_filterlist_match_prefix update");
        filter.apply_update(&live.current());

        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
        assert_eq!(filter.matcher, Blocklist::new(["foo"], true));
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
            live: Live::fixed(PrefixFilter::default()),
        };

        filter.sync_effective_blocklist(false);
        // Reconfiguring the inactive blocklist while the filterlist is active is not counted.
        filter.apply_update(&PrefixFilter {
            metric_filterlist: vec!["preferred".to_string()],
            metric_blocklist: vec!["ignored".to_string(), "still_ignored".to_string()],
            ..PrefixFilter::default()
        });

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(0));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(1.0));

        let mut metric = Metric::gauge("preferred", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("ignored", 1.0);
        assert!(filter.process_metric(&mut metric));

        // Clearing the filterlist activates the blocklist, which is a counted reconfiguration.
        filter.apply_update(&PrefixFilter {
            metric_filterlist: Vec::new(),
            metric_blocklist: vec!["ignored".to_string(), "still_ignored".to_string()],
            ..PrefixFilter::default()
        });

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(2.0));

        let mut metric = Metric::gauge("ignored", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_counts_active_reconfiguration_even_if_matcher_is_unchanged() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(vec!["foo".to_string()], true, Vec::new(), false),
            telemetry,
            live: Live::fixed(PrefixFilter::default()),
        };

        filter.sync_effective_blocklist(false);
        filter.apply_update(&PrefixFilter {
            metric_filterlist: vec!["foo".to_string(), "foobar".to_string()],
            metric_filterlist_match_prefix: true,
            ..PrefixFilter::default()
        });

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
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo", "bar"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry,
            live: Live::fixed(PrefixFilter::default()),
        };

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}
