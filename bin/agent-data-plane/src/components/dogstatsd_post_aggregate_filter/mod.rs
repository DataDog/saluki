//! DogStatsD post-aggregate metric filter transform.
//!
//! Drops post-aggregation scalar metrics whose generated histogram aggregate names match the metric filterlist.
use agent_data_plane_config::{domains::dogstatsd::MetricFilter, Live};
use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{metric::Metric, EventType},
    observability::ComponentMetricsExt,
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::{
    matcher::SortedSetMatcher,
    params::HistogramParams,
    selection::{is_histogram_statistic_entry, is_scalar_series},
};
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::{debug, error};

pub(crate) mod telemetry;

use self::telemetry::Telemetry;

/// DogStatsD post-aggregate metric filter configuration.
///
/// This transform mirrors the Agent time-sampler metric filter for DogStatsD histogram aggregate series after the
/// aggregate transform has expanded histograms into scalar metrics. It matches only the filterlist entries that name
/// a histogram statistic's series, as `saluki_metric_kernels::selection::is_histogram_statistic_entry` decides.
pub struct DogStatsDPostAggregateFilterConfiguration {
    metric_filter: Live<MetricFilter>,
    suffixes: Vec<String>,
}

impl DogStatsDPostAggregateFilterConfiguration {
    /// Creates a new `DogStatsDPostAggregateFilterConfiguration`.
    ///
    /// Each aggregate name is the suffix of a series as it is written, whether or not aggregation computes it: the
    /// aggregation's own configuration rejects unknown names. Each percentile becomes the suffix `<n>percentile`, with
    /// the quantile rounded to a whole percent as aggregation rounds it.
    ///
    /// # Errors
    ///
    /// Returns an error when a percentile is not a number from `0.0` through `1.0`.
    pub fn new(
        metric_filter: Live<MetricFilter>, histogram_aggregates: &[String], histogram_percentiles: &[String],
    ) -> Result<Self, GenericError> {
        let percentiles = HistogramParams::from_names(&[] as &[&str], histogram_percentiles, false, String::new())
            .map_err(|error| generic_error!("{}", error))?;
        let suffixes = histogram_aggregates
            .iter()
            .cloned()
            .chain(percentiles.suffixes())
            .collect();
        Ok(Self {
            metric_filter,
            suffixes,
        })
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

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let mut filter = DogStatsDPostAggregateFilter {
            matcher: SortedSetMatcher::default(),
            metric_filter: self.metric_filter.clone(),
            suffixes: self.suffixes.clone(),
            telemetry: Telemetry::new(&metrics_builder),
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

struct DogStatsDPostAggregateFilter {
    matcher: SortedSetMatcher,
    metric_filter: Live<MetricFilter>,
    suffixes: Vec<String>,
    telemetry: Telemetry,
}

impl DogStatsDPostAggregateFilter {
    fn sync_matcher(&mut self) {
        // Post-aggregate filtering only owns entries shaped like `<metric>.<statistic>`. Other filterlist entries
        // remain the listener filter's responsibility in `dogstatsd_prefix_filter`.
        let histogram_values = self
            .metric_filter
            .values
            .iter()
            .filter(|value| is_histogram_statistic_entry(value, &self.suffixes));
        self.matcher = SortedSetMatcher::new(histogram_values, self.metric_filter.match_prefix);
    }

    fn should_filter_metric(&self, metric: &Metric) -> bool {
        is_scalar_series(metric.values()) && self.matcher.matches_intake_name(metric.context().name())
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
                metric_filter = self.metric_filter.changed() => {
                    self.sync_matcher();
                    debug!(?metric_filter, "Updated metric filter.");
                },
            }
        }

        debug!("DogStatsD post-aggregate filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use metrics::set_default_local_recorder;
    use saluki_core::{
        data_model::event::{
            metric::{context::Context, Metric},
            Event,
        },
        topology::EventsBuffer,
    };
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;
    use crate::components::dogstatsd_post_aggregate_filter::telemetry::FILTERED_METRICS_METRIC;

    fn filter_with(
        values: Vec<&str>, match_prefix: bool, histogram_aggregates: Vec<&str>, histogram_percentiles: Vec<&str>,
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
        let metric_filter = MetricFilter {
            values: values.into_iter().map(ToString::to_string).collect(),
            match_prefix,
        };
        let configuration = DogStatsDPostAggregateFilterConfiguration::new(
            Live::new_fixed(metric_filter),
            &histogram_aggregates,
            &histogram_percentiles,
        )
        .unwrap();

        let mut filter = DogStatsDPostAggregateFilter {
            matcher: SortedSetMatcher::default(),
            metric_filter: configuration.metric_filter,
            suffixes: configuration.suffixes,
            telemetry,
        };
        filter.sync_matcher();
        filter
    }

    fn noop_filter(values: Vec<&str>, match_prefix: bool) -> DogStatsDPostAggregateFilter {
        filter_with(
            values,
            match_prefix,
            vec!["max", "median", "avg", "count"],
            vec!["0.95"],
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
        let filter = noop_filter(vec!["request.duration.max"], false);

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
        let filter = noop_filter(vec!["request.duration", "db.query.max"], true);

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
        let filter = noop_filter(vec!["custom.metric"], false);

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
    fn invalid_percentiles_fail_construction_and_aggregates_are_taken_as_written() {
        // A value that isn't a number and a numeric value outside the [0.0, 1.0] range must each fail with the message
        // aggregation's own configuration gives. Aggregate names are suffixes as written, so an aggregate that
        // aggregation does not compute is no error here, even beside an invalid percentile: aggregation's own
        // configuration rejects it.
        let cases = [
            (&[][..], &["abc"][..], "Invalid percentile: abc"),
            (&[], &["1.1"], "Percentile out of range: 1.1"),
            (&[], &["-0.1"], "Percentile out of range: -0.1"),
            (&["p99"], &["abc"], "Invalid percentile: abc"),
        ];
        for (aggregates, percentiles, expected) in cases {
            let aggregates = aggregates.iter().map(ToString::to_string).collect::<Vec<_>>();
            let percentiles = percentiles.iter().map(ToString::to_string).collect::<Vec<_>>();
            let error = DogStatsDPostAggregateFilterConfiguration::new(
                Live::new_fixed(MetricFilter::default()),
                &aggregates,
                &percentiles,
            )
            .err()
            .expect("invalid statistics must be rejected");
            assert_eq!(error.to_string(), expected);
        }

        let filter = filter_with(
            vec!["request.duration.p99", "request.duration.max"],
            false,
            vec!["p99"],
            vec![],
            Telemetry::noop(),
        );
        let names = filter_metric_names(
            &filter,
            vec![
                Metric::gauge("request.duration.p99", 1.0),
                Metric::gauge("request.duration.max", 1.0),
            ],
        );
        assert_eq!(names, vec!["request.duration.max"]);
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
    // https://github.com/DataDog/datadog-agent/blob/12213fe95538f47d98d73bd945a87b3e24189285/pkg/aggregator/demultiplexer_agent_test.go#L390
    #[tokio::test]
    async fn typed_live_updates_rebuild_the_effective_matcher() {
        let mut initial = agent_data_plane_config::SalukiConfiguration::default();
        initial.domains.dogstatsd.metric_filter.values = vec!["request.duration.max".to_string()];
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(initial));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let metric_filter = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.metric_filter
        });
        let mut filter = noop_filter(vec![], false);
        filter.metric_filter = metric_filter;
        filter.sync_matcher();

        assert!(filter.should_filter_metric(&Metric::gauge("request.duration.max", 1.0)));
        assert!(!filter.should_filter_metric(&Metric::gauge("request.duration.avg", 1.0)));

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.metric_filter.values = vec!["request.duration.avg".to_string()];
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        tokio::time::timeout(Duration::from_secs(2), filter.metric_filter.changed())
            .await
            .expect("timed out waiting for typed metric filter update");
        filter.sync_matcher();

        assert!(!filter.should_filter_metric(&Metric::gauge("request.duration.max", 1.0)));
        assert!(filter.should_filter_metric(&Metric::gauge("request.duration.avg", 1.0)));
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
