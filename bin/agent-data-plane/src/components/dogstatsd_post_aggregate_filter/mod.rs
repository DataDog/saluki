//! DogStatsD post-aggregate metric filter transform.
//!
//! Drops post-aggregation scalar metrics whose generated histogram aggregate names match the metric filterlist.
use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::dogstatsd::DogStatsDPostAggregateFilterConfig;
use saluki_component_config::ScopedConfig;
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{
        metric::{Metric, MetricValues},
        EventType,
    },
    observability::ComponentMetricsExt,
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, error};

use crate::components::dogstatsd_filterlist::{Blocklist, EffectiveFilterlist};

mod telemetry;

use self::telemetry::Telemetry;

/// DogStatsD post-aggregate metric filter configuration.
///
/// This transform mirrors the Agent time-sampler metric filter for DogStatsD histogram aggregate series after the
/// aggregate transform has expanded histograms into scalar metrics. It uses `metric_filterlist` when non-empty,
/// otherwise it falls back to the legacy `statsd_metric_blocklist`.
///
/// The configuration arrives as a typed [`ScopedConfig<DogStatsDPostAggregateFilterConfig>`]; the
/// transform reacts to runtime updates published on that handle by rebuilding its effective matcher.
pub struct DogStatsDPostAggregateFilterConfiguration {
    config: ScopedConfig<DogStatsDPostAggregateFilterConfig>,
}

impl DogStatsDPostAggregateFilterConfiguration {
    /// Creates a new `DogStatsDPostAggregateFilterConfiguration` from the given native configuration handle.
    pub fn from_native(config: ScopedConfig<DogStatsDPostAggregateFilterConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl TransformBuilder for DogStatsDPostAggregateFilterConfiguration {
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
        let histogram_suffixes =
            HistogramSuffixes::from_histogram_config(&current.histogram_aggregates, &current.histogram_percentiles)?;
        let effective_filterlist = EffectiveFilterlist::new(
            current.metric_filterlist.clone(),
            current.metric_filterlist_match_prefix,
            current.metric_blocklist.clone(),
            current.metric_blocklist_match_prefix,
        );
        let mut filter = DogStatsDPostAggregateFilter {
            matcher: Blocklist::default(),
            effective_filterlist,
            histogram_suffixes,
            telemetry: Telemetry::new(&metrics_builder),
            config: self.config.clone(),
        };
        filter.sync_matcher();

        Ok(Box::new(filter))
    }
}

impl MemoryBounds for DogStatsDPostAggregateFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDPostAggregateFilter>("component struct");
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistogramSuffixes {
    values: Vec<MetaString>,
}

impl HistogramSuffixes {
    fn from_histogram_config(aggregates: &[String], percentiles: &[String]) -> Result<Self, GenericError> {
        let mut values = aggregates
            .iter()
            .map(|aggregate| MetaString::from(aggregate.as_str()))
            .collect::<Vec<_>>();

        for percentile in percentiles {
            let quantile = percentile
                .parse::<f64>()
                .map_err(|_| generic_error!("Invalid percentile: {}", percentile))?;
            if !(0.0..=1.0).contains(&quantile) {
                return Err(generic_error!("Percentile out of range: {}", percentile));
            }

            // Match the Agent histogram filterlist suffix generation:
            // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/comp/filterlist/impl/filterlist.go#L197-L217
            // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/metrics/histogram.go#L51-L69
            let suffix = format!("{}percentile", (quantile * 100.0 + 0.5) as u32);
            values.push(suffix.into());
        }

        Ok(Self { values })
    }

    /// Returns whether the filterlist entry targets a generated histogram aggregate output.
    ///
    /// Post-aggregate filtering only owns entries shaped like `<metric>.<aggregate>`. Other filterlist entries remain
    /// the listener filter's responsibility in `dogstatsd_prefix_filter`.
    fn contains_filter_entry(&self, value: &str) -> bool {
        self.values.iter().any(|suffix| {
            let suffix: &str = suffix.as_ref();
            value
                .strip_suffix(suffix)
                .map(|prefix| prefix.ends_with('.'))
                .unwrap_or(false)
        })
    }
}

fn is_scalar_series_metric(metric: &Metric) -> bool {
    matches!(
        metric.values(),
        MetricValues::Counter(_) | MetricValues::Rate(_, _) | MetricValues::Gauge(_) | MetricValues::Set(_)
    )
}

struct DogStatsDPostAggregateFilter {
    matcher: Blocklist,
    effective_filterlist: EffectiveFilterlist,
    histogram_suffixes: HistogramSuffixes,
    telemetry: Telemetry,
    config: ScopedConfig<DogStatsDPostAggregateFilterConfig>,
}

impl DogStatsDPostAggregateFilter {
    fn sync_matcher(&mut self) {
        let (values, match_prefix) = self.effective_filterlist.effective_values();
        let histogram_values = values
            .iter()
            .filter(|value| self.histogram_suffixes.contains_filter_entry(value))
            .cloned()
            .collect::<Vec<_>>();

        self.matcher = Blocklist::new(histogram_values.iter().map(String::as_str), match_prefix);
    }

    /// Rebuilds local state from the latest published configuration slice.
    ///
    /// The dynamic filterlist/blocklist source keys are retranslated into the whole
    /// `DogStatsDPostAggregateFilterConfig` slice by the config-system; this rebuilds the effective
    /// filterlist, histogram suffixes, and matcher from it. A failed histogram-suffix rebuild
    /// (invalid percentile) is logged and the previous matcher is retained.
    fn apply_update(&mut self, updated: DogStatsDPostAggregateFilterConfig) {
        match HistogramSuffixes::from_histogram_config(&updated.histogram_aggregates, &updated.histogram_percentiles) {
            Ok(histogram_suffixes) => self.histogram_suffixes = histogram_suffixes,
            Err(e) => {
                error!(error = %e, "Failed to rebuild histogram suffixes from updated config; retaining previous matcher.");
                return;
            }
        }
        self.effective_filterlist = EffectiveFilterlist::new(
            updated.metric_filterlist,
            updated.metric_filterlist_match_prefix,
            updated.metric_blocklist,
            updated.metric_blocklist_match_prefix,
        );
        self.sync_matcher();
    }

    fn should_filter_metric(&self, metric: &Metric) -> bool {
        is_scalar_series_metric(metric) && self.matcher.contains(metric.context().name())
    }

    fn transform_buffer(&self, buffer: &mut EventsBuffer) {
        buffer.remove_if(|event| {
            let should_filter = event
                .try_as_metric()
                .map(|metric| self.should_filter_metric(metric))
                .unwrap_or(false);

            if should_filter {
                self.telemetry.increment_filtered_metrics();
            }

            should_filter
        });
    }
}

#[async_trait]
impl Transform for DogStatsDPostAggregateFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("DogStatsD post-aggregate filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        self.transform_buffer(&mut events);

                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                },
                _ = self.config.changed() => {
                    let updated = self.config.current();
                    debug!("Updated DogStatsD post-aggregate metric filterlist.");
                    self.apply_update(updated);
                },
            }
        }

        debug!("DogStatsD post-aggregate filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use metrics::set_default_local_recorder;
    use saluki_component_config::dogstatsd::DogStatsDPostAggregateFilterConfig;
    use saluki_context::Context;
    use saluki_core::{
        data_model::event::{metric::Metric, Event},
        topology::EventsBuffer,
    };
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;
    use crate::components::dogstatsd_post_aggregate_filter::telemetry::FILTERED_METRICS_METRIC;

    fn default_histogram_aggregates() -> Vec<String> {
        ["max", "median", "avg", "count"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn default_histogram_percentiles() -> Vec<String> {
        ["0.95"].iter().map(|s| s.to_string()).collect()
    }

    fn filter_with(
        metric_filterlist: Vec<&str>, metric_filterlist_match_prefix: bool, metric_blocklist: Vec<&str>,
        metric_blocklist_match_prefix: bool, histogram_aggregates: Vec<&str>, histogram_percentiles: Vec<&str>,
        telemetry: Telemetry,
    ) -> DogStatsDPostAggregateFilter {
        let histogram_aggregates = histogram_aggregates
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let histogram_percentiles = histogram_percentiles
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let histogram_suffixes =
            HistogramSuffixes::from_histogram_config(&histogram_aggregates, &histogram_percentiles).unwrap();

        let mut filter = DogStatsDPostAggregateFilter {
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(
                metric_filterlist.into_iter().map(ToString::to_string).collect(),
                metric_filterlist_match_prefix,
                metric_blocklist.into_iter().map(ToString::to_string).collect(),
                metric_blocklist_match_prefix,
            ),
            histogram_suffixes,
            telemetry,
            config: ScopedConfig::fixed(DogStatsDPostAggregateFilterConfig::default()),
        };
        filter.sync_matcher();
        filter
    }

    fn noop_filter(
        metric_filterlist: Vec<&str>, metric_filterlist_match_prefix: bool, metric_blocklist: Vec<&str>,
        metric_blocklist_match_prefix: bool,
    ) -> DogStatsDPostAggregateFilter {
        filter_with(
            metric_filterlist,
            metric_filterlist_match_prefix,
            metric_blocklist,
            metric_blocklist_match_prefix,
            default_histogram_aggregates().iter().map(String::as_str).collect(),
            default_histogram_percentiles().iter().map(String::as_str).collect(),
            Telemetry::noop(),
        )
    }

    fn filter_metric_names(filter: &DogStatsDPostAggregateFilter, metrics: Vec<Metric>) -> Vec<String> {
        let mut buffer = EventsBuffer::default();
        for metric in metrics {
            assert!(buffer.try_push(Event::Metric(metric)).is_none());
        }

        filter.transform_buffer(&mut buffer);

        let mut names = buffer
            .into_iter()
            .map(|event| event.try_into_metric().unwrap().context().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    // Mirrors Datadog Agent time-sampler filtering of generated histogram series:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/aggregator/time_sampler_test.go#L546
    #[test]
    fn exact_match_filters_only_configured_histogram_aggregate_names() {
        let filter = noop_filter(vec!["request.duration.max"], false, vec![], false);

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("request.duration.max", 1.0),
                Metric::gauge("request.duration.avg", 1.0),
                Metric::gauge("request.duration", 1.0),
            ],
        );

        assert_eq!(names, vec!["request.duration", "request.duration.avg"]);
    }

    // Mirrors Datadog Agent histogram-specific filterlist derivation:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/comp/filterlist/impl/filterlist_test.go#L19
    #[test]
    fn prefix_match_uses_only_histogram_specific_filter_entries() {
        let filter = noop_filter(vec!["request.duration", "db.query.max"], true, vec![], false);

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("request.duration.max", 1.0),
                Metric::gauge("db.query.max", 1.0),
                Metric::gauge("db.query.max.extra", 1.0),
            ],
        );

        assert_eq!(names, vec!["request.duration.max"]);
    }

    // Mirrors Datadog Agent histogram-specific filterlist derivation:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/comp/filterlist/impl/filterlist_test.go#L19
    #[test]
    fn non_histogram_filterlist_entries_are_ignored() {
        let filter = noop_filter(vec!["custom.metric"], false, vec![], false);

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("custom.metric", 1.0),
                Metric::gauge("custom.metric.max", 1.0),
            ],
        );

        assert_eq!(names, vec!["custom.metric", "custom.metric.max"]);
    }

    // Mirrors Datadog Agent histogram-specific filterlist derivation:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/comp/filterlist/impl/filterlist_test.go#L19
    #[test]
    fn histogram_filter_subset_matches_agent_suffix_selection() {
        let filter = filter_with(
            vec![
                "foo",
                "bar",
                "baz",
                "foomax",
                "foo.avg",
                "foo.max",
                "foo.count",
                "baz.73percentile",
                "bar.50percentile",
                "bar.22percentile",
                "count",
            ],
            false,
            vec![],
            false,
            vec!["avg", "max", "median"],
            vec!["0.73", "0.22"],
            Telemetry::noop(),
        );

        assert!(filter.should_filter_metric(&Metric::gauge("foo.avg", 1.0)));
        assert!(filter.should_filter_metric(&Metric::gauge("foo.max", 1.0)));
        assert!(filter.should_filter_metric(&Metric::gauge("baz.73percentile", 1.0)));
        assert!(filter.should_filter_metric(&Metric::gauge("bar.22percentile", 1.0)));
        assert!(!filter.should_filter_metric(&Metric::gauge("foo.count", 1.0)));
        assert!(!filter.should_filter_metric(&Metric::gauge("bar.50percentile", 1.0)));
        assert!(!filter.should_filter_metric(&Metric::gauge("foomax", 1.0)));
    }

    // Mirrors Datadog Agent percentile suffix generation:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/metrics/histogram.go#L53
    #[test]
    fn filters_percentile_suffixes_like_aggregate_configuration() {
        let filter = filter_with(
            vec!["request.duration.95percentile", "request.duration.30percentile"],
            false,
            vec![],
            false,
            vec![],
            vec!["0.95", "0.299"],
            Telemetry::noop(),
        );

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("request.duration.95percentile", 1.0),
                Metric::gauge("request.duration.30percentile", 1.0),
                Metric::gauge("request.duration.29percentile", 1.0),
            ],
        );

        assert_eq!(names, vec!["request.duration.29percentile"]);
    }

    #[test]
    fn invalid_percentiles_are_rejected() {
        let histogram_aggregates = Vec::new();
        let histogram_percentiles = vec!["1.1".to_string()];

        let result = HistogramSuffixes::from_histogram_config(&histogram_aggregates, &histogram_percentiles);

        assert!(result.is_err());
    }

    // Mirrors Datadog Agent time-sampler filtering, which filters series while keeping sketches:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/aggregator/time_sampler_test.go#L546
    #[test]
    fn sketch_metrics_are_not_filtered() {
        let filter = noop_filter(
            vec![
                "distribution.duration.max",
                "histogram.duration.max",
                "gauge.duration.max",
            ],
            false,
            vec![],
            false,
        );

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::distribution("distribution.duration.max", [1.0, 2.0, 3.0]),
                Metric::histogram("histogram.duration.max", [1.0, 2.0, 3.0]),
                Metric::gauge("gauge.duration.max", 1.0),
            ],
        );

        assert_eq!(names, vec!["distribution.duration.max", "histogram.duration.max"]);
    }

    // Mirrors Datadog Agent runtime metric filterlist update behavior:

    #[test]
    fn falls_back_to_legacy_blocklist_only_when_filterlist_is_empty() {
        let filter = noop_filter(vec![], false, vec!["legacy.duration.max"], false);

        assert!(filter.should_filter_metric(&Metric::gauge("legacy.duration.max", 1.0)));

        let filter = noop_filter(
            vec!["preferred.duration.max"],
            false,
            vec!["legacy.duration.max"],
            false,
        );

        assert!(filter.should_filter_metric(&Metric::gauge("preferred.duration.max", 1.0)));
        assert!(!filter.should_filter_metric(&Metric::gauge("legacy.duration.max", 1.0)));
    }

    // Mirrors Datadog Agent filtered-metrics telemetry increment in the time sampler:
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/aggregator/time_sampler.go#L201
    #[test]
    fn telemetry_counts_filtered_metrics() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = Telemetry::new(&MetricsBuilder::default());
        let filter = filter_with(
            vec!["request.duration.max", "request.duration.avg"],
            false,
            vec![],
            false,
            vec!["max", "avg"],
            vec![],
            telemetry,
        );

        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("request.duration.max", 1.0),
                Metric::gauge("request.duration.avg", 1.0),
                Metric::gauge(Context::from_static_parts("request.duration.count", &[]), 1.0),
            ],
        );

        assert_eq!(names, vec!["request.duration.count"]);
        assert_eq!(recorder.counter(FILTERED_METRICS_METRIC), Some(2));
    }
}
