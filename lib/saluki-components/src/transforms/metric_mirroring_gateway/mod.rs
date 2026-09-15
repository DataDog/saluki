//! Selective metric-mirroring gateway transform.

use std::collections::HashSet;

use agent_data_plane_config::{domains::metric_mirroring::Routing as MetricMirroringRouting, Live};
use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{
        metric::{Metric, MetricValues},
        Event, EventType,
    },
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

/// Configuration for the metric-mirroring gateway transform.
///
/// This transform sits between the enrichment stage and a secondary encoder/forwarder, and owns the branch's routing
/// and filtering decisions:
///
/// - When the branch gate or live metric mirroring is off, all events are dropped.
/// - When both are on and no allowlist is configured, events are forwarded or dropped according to the configured
///   empty-allowlist behavior.
/// - When both are on and an allowlist is configured, only events whose metric name is in the allowlist are forwarded.
pub struct MetricMirroringGatewayConfiguration {
    enabled: bool,
    metric_mirroring: Live<MetricMirroringRouting>,
    empty_allowlist_behavior: EmptyAllowlistBehavior,
    metric_scope: MirroredMetricScope,
}

impl MetricMirroringGatewayConfiguration {
    /// Creates a new `MetricMirroringGatewayConfiguration`.
    ///
    /// `enabled` is the branch's static gate. It is read once when the topology is built because the secondary
    /// forwarder this transform feeds is wired at that point or not at all.
    ///
    /// `metric_mirroring` is whether metrics are mirrored to the secondary intake and which metric names are allowed to
    /// be mirrored. It is a live view: an operator can turn mirroring on or change the allowlist without restarting,
    /// and the transform rebuilds its routing state from the new value. The two settings arrive as one value, from one
    /// configuration version, so a rebuild cannot mix a fresh setting with a stale one.
    ///
    /// `metric_scope` determines which metric kinds are eligible for this branch. It is static because it expresses
    /// the contract of the branch rather than an operator-controlled routing setting.
    pub fn new(
        enabled: bool, metric_mirroring: Live<MetricMirroringRouting>,
        empty_allowlist_behavior: EmptyAllowlistBehavior, metric_scope: MirroredMetricScope,
    ) -> Self {
        Self {
            enabled,
            metric_mirroring,
            empty_allowlist_behavior,
            metric_scope,
        }
    }
}

/// Behavior of an active mirroring branch whose metric allowlist is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmptyAllowlistBehavior {
    /// Forward every metric.
    ForwardAll,
    /// Drop every metric.
    DropAll,
}

/// Metric kinds that a mirroring branch is permitted to forward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MirroredMetricScope {
    /// Permit both series and sketch metrics.
    AllMetrics,
    /// Permit only series metrics: counters, rates, gauges, and sets.
    SeriesOnly,
}

impl MirroredMetricScope {
    fn includes(self, metric: &Metric) -> bool {
        match self {
            Self::AllMetrics => true,
            Self::SeriesOnly => matches!(
                metric.values(),
                MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..)
            ),
        }
    }
}

/// Routing and filtering state for a metric-mirroring gateway.
#[derive(Debug)]
enum GatewayMode {
    /// The static branch gate or live metric mirroring is off; drop all events.
    Inactive,
    /// Mirroring is on and no allowlist is configured; forward all events.
    ForwardAll,
    /// Mirroring is on and an allowlist is configured; forward only matching events.
    FilteredForward { allowlist: HashSet<String> },
}

/// The current settings, and the routing state they imply.
///
/// This holds the mirroring settings by value rather than as a view. The transform's run loop awaits the view and hands
/// the new value here, so the routing state is always rebuilt from one configuration version: the two settings the mode
/// is derived from cannot be a fresh value and a stale one.
struct Routing {
    enabled: bool,
    metric_mirroring: MetricMirroringRouting,
    empty_allowlist_behavior: EmptyAllowlistBehavior,
    metric_scope: MirroredMetricScope,
    mode: GatewayMode,
}

impl Routing {
    fn new(
        enabled: bool, metric_mirroring: MetricMirroringRouting, empty_allowlist_behavior: EmptyAllowlistBehavior,
        metric_scope: MirroredMetricScope,
    ) -> Self {
        let mut routing = Self {
            enabled,
            metric_mirroring,
            empty_allowlist_behavior,
            metric_scope,
            mode: GatewayMode::Inactive,
        };
        routing.rebuild_mode();

        routing
    }

    fn set_metric_mirroring(&mut self, metric_mirroring: MetricMirroringRouting) {
        self.metric_mirroring = metric_mirroring;
        self.rebuild_mode();
        debug!(mode = ?self.mode, "Metric-mirroring gateway routing state rebuilt.");
    }

    fn rebuild_mode(&mut self) {
        self.mode = if !(self.enabled && self.metric_mirroring.enabled) {
            GatewayMode::Inactive
        } else if self.metric_mirroring.allowlist.is_empty() {
            match self.empty_allowlist_behavior {
                EmptyAllowlistBehavior::ForwardAll => GatewayMode::ForwardAll,
                EmptyAllowlistBehavior::DropAll => GatewayMode::Inactive,
            }
        } else {
            GatewayMode::FilteredForward {
                allowlist: self.metric_mirroring.allowlist.iter().cloned().collect(),
            }
        };
    }

    fn should_forward(&self, event: &Event) -> bool {
        let Event::Metric(metric) = event else {
            return false;
        };
        if !self.metric_scope.includes(metric) {
            return false;
        }

        match &self.mode {
            GatewayMode::Inactive => false,
            GatewayMode::ForwardAll => true,
            GatewayMode::FilteredForward { allowlist } => allowlist.contains(metric.context().name().as_ref()),
        }
    }

    async fn process_event_batch(
        &self, mut events: EventsBuffer, context: &mut TransformContext,
    ) -> Result<(), GenericError> {
        let input_count = events.len();
        events.remove_if(|event| !self.should_forward(event));
        let forwarded_count = events.len();
        let dropped_count = input_count.saturating_sub(forwarded_count);

        let sent_count = context.dispatcher().buffered()?.send_all(events).await?;
        debug!(
            forwarded_events = sent_count,
            dropped_events = dropped_count,
            "Metric-mirroring gateway processed event batch."
        );

        Ok(())
    }
}

/// Metric-mirroring gateway transform.
///
/// Forwards the metrics permitted to reach the secondary intake and drops the rest, following the live mirroring
/// settings it holds. It carries the view rather than a snapshot so that the run loop can await it.
pub struct MetricMirroringGateway {
    enabled: bool,
    metric_mirroring: Live<MetricMirroringRouting>,
    empty_allowlist_behavior: EmptyAllowlistBehavior,
    metric_scope: MirroredMetricScope,
}

impl MetricMirroringGateway {
    fn new(config: &MetricMirroringGatewayConfiguration) -> Self {
        Self {
            enabled: config.enabled,
            metric_mirroring: config.metric_mirroring.clone(),
            empty_allowlist_behavior: config.empty_allowlist_behavior,
            metric_scope: config.metric_scope,
        }
    }
}

#[async_trait]
impl TransformBuilder for MetricMirroringGatewayConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MetricMirroringGateway::new(self)))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for MetricMirroringGatewayConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let allowlist = &self.metric_mirroring.allowlist;
        builder
            .minimum()
            .with_single_value::<MetricMirroringGateway>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                // Three copies: the live view's snapshot, routing state, and hash set.
                "allowlist strings",
                allowlist
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>()
                    * 3,
            )
            .with_fixed_amount(
                "hashset buckets",
                allowlist.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for MetricMirroringGateway {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        // The view is moved out of the transform because `select!` awaits it while an arm body updates the routing
        // state; keeping the two apart is what lets both happen without borrowing the same value.
        let Self {
            enabled,
            mut metric_mirroring,
            empty_allowlist_behavior,
            metric_scope,
        } = *self;
        let mut routing = Routing::new(
            enabled,
            (*metric_mirroring).clone(),
            empty_allowlist_behavior,
            metric_scope,
        );

        health.mark_ready();
        debug!(mode = ?routing.mode, "Metric-mirroring gateway transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = routing.process_event_batch(events, &mut context).await {
                            error!(error = %e, "Metric-mirroring gateway failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down metric-mirroring gateway transform.");
                        break;
                    }
                },
                new_metric_mirroring = metric_mirroring.changed() => {
                    routing.set_metric_mirroring(new_metric_mirroring);
                },
            }
        }

        debug!("Metric-mirroring gateway transform stopped.");
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

        fn metric_mirroring(&self) -> Live<MetricMirroringRouting> {
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
    fn routing(enabled: bool, source: &LiveSource) -> Routing {
        routing_with_policy(
            enabled,
            source,
            EmptyAllowlistBehavior::ForwardAll,
            MirroredMetricScope::AllMetrics,
        )
    }

    fn routing_with_empty_behavior(
        enabled: bool, source: &LiveSource, empty_allowlist_behavior: EmptyAllowlistBehavior,
    ) -> Routing {
        routing_with_policy(
            enabled,
            source,
            empty_allowlist_behavior,
            MirroredMetricScope::AllMetrics,
        )
    }

    fn routing_with_policy(
        enabled: bool, source: &LiveSource, empty_allowlist_behavior: EmptyAllowlistBehavior,
        metric_scope: MirroredMetricScope,
    ) -> Routing {
        let config = MetricMirroringGatewayConfiguration::new(
            enabled,
            source.metric_mirroring(),
            empty_allowlist_behavior,
            metric_scope,
        );
        let gateway = MetricMirroringGateway::new(&config);

        Routing::new(
            gateway.enabled,
            (*gateway.metric_mirroring).clone(),
            gateway.empty_allowlist_behavior,
            gateway.metric_scope,
        )
    }

    fn counter(name: &'static str) -> Event {
        Event::Metric(Metric::counter(name, 1.0))
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

    /// Waits for `view` to process the published update, failing the test rather than hanging.
    async fn await_update<T>(view: &mut Live<T>) -> T
    where
        T: Clone + PartialEq + 'static,
    {
        tokio::time::timeout(std::time::Duration::from_secs(2), view.changed())
            .await
            .expect("the published update should reach the view")
    }

    #[test]
    fn memory_bounds_include_all_allowlist_copies() {
        let allowlist = ["allowed.metric", "also.allowed"];
        let source = LiveSource::new(true, &allowlist);
        let config = MetricMirroringGatewayConfiguration::new(
            true,
            source.metric_mirroring(),
            EmptyAllowlistBehavior::ForwardAll,
            MirroredMetricScope::AllMetrics,
        );

        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let bounds = registry.as_bounds();

        let allowlist_strings = allowlist
            .iter()
            .map(|name| name.len() + size_of::<String>())
            .sum::<usize>();
        let expected = size_of::<MetricMirroringGateway>()
            + size_of::<HashSet<String>>()
            + allowlist_strings * 3
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
    }

    #[tokio::test]
    async fn an_empty_allowlist_can_drop_everything() {
        let source = LiveSource::new(true, &[]);
        let routing = routing_with_empty_behavior(true, &source, EmptyAllowlistBehavior::DropAll);

        assert!(!routing.should_forward(&counter("any.metric")));
    }

    #[tokio::test]
    async fn an_allowlist_forwards_only_matching_metrics() {
        let source = LiveSource::new(true, &["allowed.metric"]);
        let routing = routing(true, &source);

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(!routing.should_forward(&counter("blocked.metric")));
    }

    #[test]
    fn series_only_scope_forwards_allowed_series_and_drops_sketches() {
        let source = LiveSource::new(
            true,
            &[
                "allowed.counter",
                "allowed.gauge",
                "allowed.rate",
                "allowed.set",
                "allowed.histogram",
                "allowed.distribution",
            ],
        );
        let routing = routing_with_policy(
            true,
            &source,
            EmptyAllowlistBehavior::DropAll,
            MirroredMetricScope::SeriesOnly,
        );

        assert!(routing.should_forward(&counter("allowed.counter")));
        assert!(routing.should_forward(&gauge("allowed.gauge")));
        assert!(routing.should_forward(&rate("allowed.rate")));
        assert!(routing.should_forward(&set("allowed.set")));
        assert!(!routing.should_forward(&histogram("allowed.histogram")));
        assert!(!routing.should_forward(&distribution("allowed.distribution")));
        assert!(!routing.should_forward(&counter("blocked.counter")));
    }

    #[test]
    fn all_metrics_scope_preserves_mrf_sketch_forwarding() {
        let source = LiveSource::new(true, &["allowed.histogram", "allowed.distribution"]);
        let routing = routing_with_policy(
            true,
            &source,
            EmptyAllowlistBehavior::ForwardAll,
            MirroredMetricScope::AllMetrics,
        );

        assert!(routing.should_forward(&histogram("allowed.histogram")));
        assert!(routing.should_forward(&distribution("allowed.distribution")));
    }

    #[tokio::test]
    async fn a_mirroring_update_toggles_forwarding() {
        let source = LiveSource::new(false, &[]);
        let mut routing = routing(true, &source);
        let mut view = source.metric_mirroring();

        assert!(!routing.should_forward(&counter("any.metric")));

        source.publish(true, &[]);
        routing.set_metric_mirroring(await_update(&mut view).await);
        assert!(routing.should_forward(&counter("any.metric")));

        source.publish(false, &[]);
        routing.set_metric_mirroring(await_update(&mut view).await);
        assert!(!routing.should_forward(&counter("any.metric")));
    }

    #[tokio::test]
    async fn an_allowlist_update_changes_filtering() {
        let source = LiveSource::new(true, &[]);
        let mut routing = routing(true, &source);
        let mut view = source.metric_mirroring();

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(routing.should_forward(&counter("also.allowed")));

        source.publish(true, &["also.allowed"]);
        routing.set_metric_mirroring(await_update(&mut view).await);

        assert!(!routing.should_forward(&counter("allowed.metric")));
        assert!(routing.should_forward(&counter("also.allowed")));
        assert!(!routing.should_forward(&counter("blocked.metric")));
    }
}
