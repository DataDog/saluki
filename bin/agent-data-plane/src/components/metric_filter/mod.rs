//! Metric filtering shared by MRF and endpoint allow lists.

use std::collections::HashSet;

use agent_data_plane_config::{domains::multi_region_failover::MetricMirroring, Live};
use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

/// Configuration for a metric filter between enrichment and encoding.
///
/// MRF follows live settings for activation and its allowlist. Endpoint routing uses a fixed metric allowlist.
pub struct MetricFilterConfiguration {
    source: FilterSource,
}

impl MetricFilterConfiguration {
    /// Creates the MRF filter. An enabled MRF branch with an empty allowlist forwards all metrics.
    ///
    /// `enabled` is the startup-only MRF gate. The live view supplies mirroring activation and the allowlist together,
    /// so each update rebuilds the filter from one configuration version. Both activation flags default to off.
    pub fn for_mrf(enabled: bool, routing: Live<MetricMirroring>) -> Self {
        Self {
            source: FilterSource::Mrf { enabled, routing },
        }
    }

    /// Creates a fixed metric filter. Unlisted names are dropped; an empty list drops everything.
    pub fn for_allowlist(allowlist: Vec<String>) -> Self {
        Self {
            source: FilterSource::Allowlist(allowlist),
        }
    }
}

/// Keeps live MRF settings separate from the startup-only endpoint policy.
#[derive(Clone)]
enum FilterSource {
    Mrf {
        enabled: bool,
        routing: Live<MetricMirroring>,
    },
    Allowlist(Vec<String>),
}

impl FilterSource {
    fn filter(&self) -> Filter {
        match self {
            Self::Mrf { enabled, routing } => Filter::for_mrf(*enabled, routing),
            Self::Allowlist(names) => Filter::Allowlist(names.iter().cloned().collect()),
        }
    }

    fn allowlist(&self) -> &[String] {
        match self {
            Self::Mrf { routing, .. } => &routing.allowlist,
            Self::Allowlist(names) => names,
        }
    }

    async fn changed(&mut self) -> Filter {
        match self {
            Self::Mrf { enabled, routing } => Filter::for_mrf(*enabled, &routing.changed().await),
            Self::Allowlist(_) => std::future::pending().await,
        }
    }
}

/// The matching rule used by both sources. An empty set naturally matches nothing.
enum Filter {
    DropAll,
    All,
    Allowlist(HashSet<String>),
}

impl Filter {
    fn for_mrf(enabled: bool, routing: &MetricMirroring) -> Self {
        if !enabled || !routing.enabled {
            Self::DropAll
        } else if routing.allowlist.is_empty() {
            Self::All
        } else {
            Self::Allowlist(routing.allowlist.iter().cloned().collect())
        }
    }

    fn should_forward(&self, event: &Event) -> bool {
        let Event::Metric(metric) = event else {
            return false;
        };
        match self {
            Self::DropAll => false,
            Self::All => true,
            Self::Allowlist(names) => names.contains(metric.context().name().as_ref()),
        }
    }

    async fn process_event_batch(
        &self, mut events: EventsBuffer, context: &mut TransformContext,
    ) -> Result<(), GenericError> {
        let input_count = events.len();
        events.remove_if(|event| !self.should_forward(event));
        let dropped_count = input_count.saturating_sub(events.len());
        let sent_count = context.dispatcher().buffered()?.send_all(events).await?;
        debug!(
            forwarded_events = sent_count,
            dropped_events = dropped_count,
            "Metric filter processed event batch."
        );
        Ok(())
    }
}

/// Metric filter shared by live MRF routing and fixed endpoint allow lists.
struct MetricFilter {
    source: FilterSource,
}

#[async_trait]
impl TransformBuilder for MetricFilterConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MetricFilter {
            source: self.source.clone(),
        }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for MetricFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let allowlist = self.source.allowlist();
        builder
            .minimum()
            .with_single_value::<MetricFilter>("component struct")
            .with_single_value::<Filter>("matching rule")
            .with_fixed_amount(
                // Two copies: the source's snapshot and the matching set.
                "allowlist strings",
                allowlist
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>()
                    * 2,
            )
            .with_fixed_amount(
                "hashset buckets",
                allowlist.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for MetricFilter {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let Self { mut source } = *self;
        let mut filter = source.filter();
        health.mark_ready();

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = filter.process_event_batch(events, &mut context).await {
                            error!(error = %e, "Metric filter failed to process event batch.");
                        }
                    }
                    None => break,
                },
                updated = source.changed() => filter = updated,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::size_of, sync::Arc, time::Duration};

    use agent_data_plane_config::SalukiConfiguration;
    use arc_swap::ArcSwap;
    use saluki_core::{
        accounting::ComponentRegistry,
        data_model::event::{metric::Metric, Event},
        support::SubsystemIdentifier,
    };
    use tokio::sync::watch;

    use super::*;

    /// A configuration the tests can replace, plus the live view projected out of it.
    ///
    /// This stands in for the configuration system: replacing the configuration and ticking the notification is what
    /// the system does once it has translated an update.
    struct LiveSource {
        cell: Arc<ArcSwap<SalukiConfiguration>>,
        tick: watch::Sender<()>,
    }

    impl LiveSource {
        fn new(failover_metrics: bool, metric_allowlist: &[&str]) -> Self {
            let (tick, _) = watch::channel(());

            Self {
                cell: Arc::new(ArcSwap::from_pointee(configuration(failover_metrics, metric_allowlist))),
                tick,
            }
        }

        fn metric_mirroring(&self) -> Live<MetricMirroring> {
            Live::new_dynamic(Arc::clone(&self.cell), self.tick.subscribe(), |config| {
                &config.domains.multi_region_failover.metric_mirroring
            })
        }

        fn publish(&self, failover_metrics: bool, metric_allowlist: &[&str]) {
            self.cell
                .store(Arc::new(configuration(failover_metrics, metric_allowlist)));
            self.tick.send(()).expect("a view still holds the receiver");
        }
    }

    fn configuration(failover_metrics: bool, metric_allowlist: &[&str]) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        let mirroring = &mut config.domains.multi_region_failover.metric_mirroring;
        mirroring.enabled = failover_metrics;
        mirroring.allowlist = metric_allowlist.iter().map(|name| (*name).to_string()).collect();

        config
    }

    /// Builds the routing state the transform starts from, the way `run` does.
    fn routing(enabled: bool, source: &LiveSource) -> Filter {
        MetricFilterConfiguration::for_mrf(enabled, source.metric_mirroring())
            .source
            .filter()
    }

    fn counter(name: &'static str) -> Event {
        Event::Metric(Metric::counter(name, 1.0))
    }

    #[test]
    fn static_constructor_filters_and_fails_closed_for_an_empty_list() {
        for allowlist in [vec![], vec!["allowed.metric".to_string()]] {
            let config = MetricFilterConfiguration::for_allowlist(allowlist.clone());
            let routing = config.source.filter();
            assert_eq!(
                routing.should_forward(&counter("allowed.metric")),
                !allowlist.is_empty()
            );
            assert!(!routing.should_forward(&counter("blocked.metric")));
            assert_eq!(
                routing.should_forward(&distribution("allowed.metric")),
                !allowlist.is_empty()
            );
            assert!(!routing.should_forward(&distribution("blocked.metric")));
        }
    }

    fn gauge(name: &'static str) -> Event {
        Event::Metric(Metric::gauge(name, 1.0))
    }

    fn rate(name: &'static str) -> Event {
        Event::Metric(Metric::rate(name, 1.0, Duration::from_secs(10)))
    }

    fn set(name: &'static str) -> Event {
        Event::Metric(Metric::set(name, "value".to_string()))
    }

    fn histogram(name: &'static str) -> Event {
        Event::Metric(Metric::histogram(name, 1.0))
    }

    fn distribution(name: &'static str) -> Event {
        Event::Metric(Metric::distribution(name, 1.0))
    }

    /// Waits for the transform's source to process an update, failing rather than hanging.
    async fn await_update(source: &mut FilterSource) -> Filter {
        tokio::time::timeout(Duration::from_secs(2), source.changed())
            .await
            .expect("the published update should reach the view")
    }

    #[test]
    fn memory_bounds_include_all_allowlist_copies() {
        let allowlist = ["allowed.metric", "also.allowed"];
        let source = LiveSource::new(true, &allowlist);
        let config = MetricFilterConfiguration::for_mrf(true, source.metric_mirroring());

        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let bounds = registry.as_bounds();

        let allowlist_strings = allowlist
            .iter()
            .map(|name| name.len() + size_of::<String>())
            .sum::<usize>();
        let expected = size_of::<MetricFilter>()
            + size_of::<Filter>()
            + allowlist_strings * 2
            + allowlist.len() * size_of::<Option<String>>() * 2;

        assert_eq!(bounds.total_minimum_required_bytes(), expected);
        assert_eq!(bounds.total_firm_limit_bytes(), expected);
    }

    #[tokio::test]
    async fn a_static_branch_gate_that_is_off_drops_everything() {
        let source = LiveSource::new(true, &[]);
        let routing = routing(false, &source);

        assert!(!routing.should_forward(&counter("any.metric")));
    }

    #[tokio::test]
    async fn mirroring_that_is_off_drops_everything() {
        let source = LiveSource::new(false, &[]);
        let routing = routing(true, &source);

        assert!(!routing.should_forward(&counter("any.metric")));
    }

    #[tokio::test]
    async fn an_empty_allowlist_forwards_everything() {
        let source = LiveSource::new(true, &[]);
        let routing = routing(true, &source);

        assert!(routing.should_forward(&counter("any.metric")));
        assert!(routing.should_forward(&histogram("any.metric")));
        assert!(routing.should_forward(&distribution("any.metric")));
    }

    #[tokio::test]
    async fn an_allowlist_forwards_only_matching_metrics() {
        let source = LiveSource::new(true, &["allowed.metric"]);
        let routing = routing(true, &source);

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(!routing.should_forward(&counter("blocked.metric")));
    }

    #[test]
    fn fixed_allowlist_filters_series_and_sketches_by_name() {
        let routing = MetricFilterConfiguration::for_allowlist(vec!["allowed".to_string()])
            .source
            .filter();

        assert!(routing.should_forward(&counter("allowed")));
        assert!(routing.should_forward(&gauge("allowed")));
        assert!(routing.should_forward(&rate("allowed")));
        assert!(routing.should_forward(&set("allowed")));
        assert!(routing.should_forward(&histogram("allowed")));
        assert!(routing.should_forward(&distribution("allowed")));
        assert!(!routing.should_forward(&counter("blocked.counter")));
        assert!(!routing.should_forward(&histogram("blocked.histogram")));
        assert!(!routing.should_forward(&distribution("blocked.distribution")));
    }

    #[test]
    fn all_metrics_scope_preserves_mrf_sketch_forwarding() {
        let source = LiveSource::new(true, &["allowed.histogram", "allowed.distribution"]);
        let routing = routing(true, &source);

        assert!(routing.should_forward(&histogram("allowed.histogram")));
        assert!(routing.should_forward(&distribution("allowed.distribution")));
    }

    #[tokio::test]
    async fn a_mirroring_update_toggles_forwarding() {
        let source = LiveSource::new(false, &[]);
        let mut routing = routing(true, &source);
        let mut filter_source = MetricFilterConfiguration::for_mrf(true, source.metric_mirroring()).source;

        assert!(!routing.should_forward(&counter("any.metric")));

        source.publish(true, &[]);
        routing = await_update(&mut filter_source).await;
        assert!(routing.should_forward(&counter("any.metric")));

        source.publish(false, &[]);
        routing = await_update(&mut filter_source).await;
        assert!(!routing.should_forward(&counter("any.metric")));
    }

    #[tokio::test]
    async fn an_allowlist_update_changes_filtering() {
        let source = LiveSource::new(true, &[]);
        let mut routing = routing(true, &source);
        let mut filter_source = MetricFilterConfiguration::for_mrf(true, source.metric_mirroring()).source;

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(routing.should_forward(&counter("also.allowed")));

        source.publish(true, &["also.allowed"]);
        routing = await_update(&mut filter_source).await;

        assert!(!routing.should_forward(&counter("allowed.metric")));
        assert!(routing.should_forward(&counter("also.allowed")));
        assert!(!routing.should_forward(&counter("blocked.metric")));
    }
}
