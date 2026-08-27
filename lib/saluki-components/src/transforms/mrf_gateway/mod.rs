//! MRF metrics gateway transform.

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

/// Configuration for the MRF metrics gateway transform.
///
/// This transform sits between the enrichment stage and the MRF-specific encoder/forwarder, and owns all routing and
/// filtering decisions for the MRF metrics pipeline:
///
/// - When multi-region failover is off, or metric mirroring is off, all events are dropped.
/// - When both are on and no allowlist is configured, all events are forwarded.
/// - When both are on and an allowlist is configured, only events whose metric name is in the allowlist are forwarded.
pub struct MrfMetricsGatewayConfiguration {
    enabled: bool,
    metric_mirroring: Live<MetricMirroring>,
}

impl MrfMetricsGatewayConfiguration {
    /// Creates a new `MrfMetricsGatewayConfiguration`.
    ///
    /// `enabled` is whether multi-region failover is on. It defaults to off in configuration and is read once, when
    /// the topology is built, because the failover forwarder this transform feeds is wired at that point or not at all.
    ///
    /// `metric_mirroring` is whether metrics are mirrored to the failover region and which metric names are allowed to
    /// be mirrored, defaulting to off and empty respectively. It is a live view: an operator can turn mirroring on, or
    /// change the allowlist, without restarting, and the transform rebuilds its routing state from the new value. The
    /// two settings arrive as one value, from one configuration version, so a rebuild cannot mix a fresh setting with a
    /// stale one.
    pub fn new(enabled: bool, metric_mirroring: Live<MetricMirroring>) -> Self {
        Self {
            enabled,
            metric_mirroring,
        }
    }
}

/// Routing and filtering state for the MRF metrics gateway.
#[derive(Debug)]
enum GatewayMode {
    /// Multi-region failover is off, or metric mirroring is off; drop all events.
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
    metric_mirroring: MetricMirroring,
    mode: GatewayMode,
}

impl Routing {
    fn new(enabled: bool, metric_mirroring: MetricMirroring) -> Self {
        let mut routing = Self {
            enabled,
            metric_mirroring,
            mode: GatewayMode::Inactive,
        };
        routing.rebuild_mode();

        routing
    }

    fn set_metric_mirroring(&mut self, metric_mirroring: MetricMirroring) {
        self.metric_mirroring = metric_mirroring;
        self.rebuild_mode();
        debug!(mode = ?self.mode, "MRF metrics gateway routing state rebuilt.");
    }

    fn rebuild_mode(&mut self) {
        self.mode = if !(self.enabled && self.metric_mirroring.enabled) {
            GatewayMode::Inactive
        } else if self.metric_mirroring.allowlist.is_empty() {
            GatewayMode::ForwardAll
        } else {
            GatewayMode::FilteredForward {
                allowlist: self.metric_mirroring.allowlist.iter().cloned().collect(),
            }
        };
    }

    fn should_forward(&self, event: &Event) -> bool {
        match &self.mode {
            GatewayMode::Inactive => false,
            GatewayMode::ForwardAll => true,
            GatewayMode::FilteredForward { allowlist } => {
                let Event::Metric(metric) = event else {
                    return false;
                };
                allowlist.contains(metric.context().name().as_ref())
            }
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
            "MRF metrics gateway processed event batch."
        );

        Ok(())
    }
}

/// MRF metrics gateway transform.
///
/// Forwards the metrics permitted to reach the failover region and drops the rest, following the live mirroring
/// settings it holds. It carries the view rather than a snapshot so that the run loop can await it.
pub struct MrfMetricsGateway {
    enabled: bool,
    metric_mirroring: Live<MetricMirroring>,
}

impl MrfMetricsGateway {
    fn new(config: &MrfMetricsGatewayConfiguration) -> Self {
        Self {
            enabled: config.enabled,
            metric_mirroring: config.metric_mirroring.clone(),
        }
    }
}

#[async_trait]
impl TransformBuilder for MrfMetricsGatewayConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MrfMetricsGateway::new(self)))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for MrfMetricsGatewayConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let allowlist = &self.metric_mirroring.allowlist;
        builder
            .minimum()
            .with_single_value::<MrfMetricsGateway>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                // Two copies: the live view's snapshot, and the copy the routing state is rebuilt from.
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
impl Transform for MrfMetricsGateway {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        // The view is moved out of the transform because `select!` awaits it while an arm body updates the routing
        // state; keeping the two apart is what lets both happen without borrowing the same value.
        let Self {
            enabled,
            mut metric_mirroring,
        } = *self;
        let mut routing = Routing::new(enabled, (*metric_mirroring).clone());

        health.mark_ready();
        debug!(mode = ?routing.mode, "MRF metrics gateway transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = routing.process_event_batch(events, &mut context).await {
                            error!(error = %e, "MRF metrics gateway failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down MRF metrics gateway transform.");
                        break;
                    }
                },
                new_metric_mirroring = metric_mirroring.changed() => {
                    routing.set_metric_mirroring(new_metric_mirroring);
                },
            }
        }

        debug!("MRF metrics gateway transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_data_plane_config::SalukiConfiguration;
    use arc_swap::ArcSwap;
    use saluki_core::data_model::event::{metric::Metric, Event};
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
    fn routing(enabled: bool, source: &LiveSource) -> Routing {
        let config = MrfMetricsGatewayConfiguration::new(enabled, source.metric_mirroring());
        let gateway = MrfMetricsGateway::new(&config);

        Routing::new(gateway.enabled, (*gateway.metric_mirroring).clone())
    }

    fn counter(name: &'static str) -> Event {
        Event::Metric(Metric::counter(name, 1.0))
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

    #[tokio::test]
    async fn failover_that_is_off_drops_everything() {
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
    async fn an_allowlist_forwards_only_matching_metrics() {
        let source = LiveSource::new(true, &["allowed.metric"]);
        let routing = routing(true, &source);

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(!routing.should_forward(&counter("blocked.metric")));
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

    #[tokio::test]
    async fn a_change_to_both_settings_arrives_as_one_update() {
        // The dangerous transition is mirroring turning off as the allowlist is cleared: taken one setting at a time,
        // the cleared allowlist alone reads as "forward everything", which no published configuration asked for. The
        // two settings share one view, so the transition arrives as a single value and the intermediate state cannot
        // be observed.
        let source = LiveSource::new(true, &["allowed.metric"]);
        let mut routing = routing(true, &source);
        let mut view = source.metric_mirroring();

        assert!(routing.should_forward(&counter("allowed.metric")));
        assert!(!routing.should_forward(&counter("other.metric")));

        source.publish(false, &[]);
        routing.set_metric_mirroring(await_update(&mut view).await);

        assert!(!routing.should_forward(&counter("allowed.metric")));
        assert!(!routing.should_forward(&counter("other.metric")));

        // Nothing is left to deliver, which is what makes the assertions above the only state the transform sees.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), view.changed())
                .await
                .is_err(),
            "both settings should have arrived in the update above"
        );
    }
}
