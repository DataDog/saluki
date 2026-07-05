//! MRF metrics gateway transform.

use std::collections::HashSet;

use agent_data_plane_config::{domains::multi_region_failover::Domain, Live};
use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

use crate::config::MrfConfiguration;

/// MRF metrics gateway transform configuration.
///
/// This transform sits between the enrichment stage and the MRF-specific encoder/forwarder. It owns
/// all routing and filtering decisions for the MRF metrics pipeline:
///
/// - When MRF is disabled, all events are dropped.
/// - When MRF is enabled with no allowlist, all events are forwarded.
/// - When MRF is enabled with an allowlist, only matching events are forwarded.
///
/// The transform reads static MRF configuration from a snapshot taken at build time, and watches the
/// multi-region failover domain for dynamic updates to `failover_metrics` and `metric_allowlist`.
pub struct MrfMetricsGatewayConfiguration {
    mrf_config: MrfConfiguration,
    live: Live<Domain>,
}

impl MrfMetricsGatewayConfiguration {
    /// Creates a new `MrfMetricsGatewayConfiguration` from the given [`MrfConfiguration`] and a live
    /// view of the multi-region failover domain.
    pub fn new(mrf_config: MrfConfiguration, live: Live<Domain>) -> Self {
        Self { mrf_config, live }
    }
}

/// Routing and filtering state for the MRF metrics gateway.
#[derive(Debug)]
enum GatewayMode {
    /// MRF is disabled or improperly configured; drop all events.
    Inactive,
    /// MRF is active and no allowlist is configured; forward all events.
    ForwardAll,
    /// MRF is active and an allowlist is configured; forward only matching events.
    FilteredForward { allowlist: HashSet<String> },
}

/// MRF metrics gateway transform.
pub struct MrfMetricsGateway {
    mrf_config: MrfConfiguration,
    mode: GatewayMode,
    live: Live<Domain>,
}

impl MrfMetricsGateway {
    fn new(mrf_config: MrfConfiguration, live: Live<Domain>) -> Self {
        let mode = Self::mode_for_config(&mrf_config);

        Self { mrf_config, mode, live }
    }

    fn mode_for_config(mrf_config: &MrfConfiguration) -> GatewayMode {
        if !mrf_config.is_metrics_forwarding_requested() {
            GatewayMode::Inactive
        } else if mrf_config.metric_allowlist().is_empty() {
            GatewayMode::ForwardAll
        } else {
            GatewayMode::FilteredForward {
                allowlist: mrf_config.metric_allowlist().iter().cloned().collect(),
            }
        }
    }

    /// Re-derives the routing mode from the current multi-region failover domain, tracking runtime
    /// changes to `failover_metrics` and `metric_allowlist`.
    fn apply_domain(&mut self, domain: &Domain) {
        self.mrf_config.set_failover_metrics(domain.failover_metrics);
        self.mrf_config.set_metric_allowlist(domain.metric_allowlist.clone());
        self.mode = Self::mode_for_config(&self.mrf_config);
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

#[async_trait]
impl TransformBuilder for MrfMetricsGatewayConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MrfMetricsGateway::new(
            self.mrf_config.clone(),
            self.live.clone(),
        )))
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
        let allowlist = self.mrf_config.metric_allowlist();
        builder
            .minimum()
            .with_single_value::<MrfMetricsGateway>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                "allowlist strings",
                allowlist
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>(),
            )
            .with_fixed_amount(
                "hashset buckets",
                allowlist.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for MrfMetricsGateway {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let mut live = self.live.clone();

        health.mark_ready();
        debug!(mode = ?self.mode, "MRF metrics gateway transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = self.process_event_batch(events, &mut context).await {
                            error!(error = %e, "MRF metrics gateway failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down MRF metrics gateway transform.");
                        break;
                    }
                },
                _ = live.changed() => {
                    let domain = live.current();
                    self.apply_domain(&domain);
                    debug!(mode = ?self.mode, "Updated MRF metrics gateway routing from live configuration.");
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
    use std::time::Duration;

    use agent_data_plane_config::{domains::multi_region_failover::Domain, Live, SalukiConfiguration};
    use arc_swap::ArcSwap;
    use saluki_core::data_model::event::{metric::Metric, Event};
    use tokio::sync::watch;

    use super::*;

    /// A [`Live`] view over the multi-region failover domain, backed by a cell the test can flip,
    /// mirroring how the config system drives runtime updates.
    fn drivable_live(domain: Domain) -> (Arc<ArcSwap<SalukiConfiguration>>, watch::Sender<()>, Live<Domain>) {
        let mut config = SalukiConfiguration::default();
        config.domains.multi_region_failover = domain;

        let cell = Arc::new(ArcSwap::from_pointee(config));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| &c.domains.multi_region_failover);
        (cell, tx, live)
    }

    fn store_domain(cell: &ArcSwap<SalukiConfiguration>, tx: &watch::Sender<()>, domain: Domain) {
        let mut config = SalukiConfiguration::default();
        config.domains.multi_region_failover = domain;
        cell.store(Arc::new(config));
        tx.send(()).expect("live cell should have a receiver");
    }

    fn gateway_from(domain: Domain, live: Live<Domain>) -> MrfMetricsGateway {
        let mrf_config = MrfConfiguration::from_configuration(&domain).expect("MRF configuration should build");
        MrfMetricsGateway::new(mrf_config, live)
    }

    #[tokio::test]
    async fn failover_metrics_dynamic_update_toggles_forwarding() {
        let base = Domain {
            enabled: true,
            failover_metrics: false,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        };
        let (cell, tx, live) = drivable_live(base.clone());
        let mut gw = gateway_from(base.clone(), live);

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));

        store_domain(
            &cell,
            &tx,
            Domain {
                failover_metrics: true,
                ..base.clone()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), gw.live.changed())
            .await
            .expect("live view should observe the enabled update");
        let domain = gw.live.current();
        gw.apply_domain(&domain);
        assert!(gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));

        store_domain(
            &cell,
            &tx,
            Domain {
                failover_metrics: false,
                ..base.clone()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), gw.live.changed())
            .await
            .expect("live view should observe the disabled update");
        let domain = gw.live.current();
        gw.apply_domain(&domain);
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));
    }

    #[tokio::test]
    async fn metric_allowlist_dynamic_update_changes_filtering() {
        let base = Domain {
            enabled: true,
            failover_metrics: true,
            api_key: Some("mrf-api-key".to_string()),
            dd_url: Some("https://mrf.example.com".to_string()),
            ..Default::default()
        };
        let (cell, tx, live) = drivable_live(base.clone());
        let mut gw = gateway_from(base.clone(), live);

        assert!(gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("also.allowed", 1.0))));

        store_domain(
            &cell,
            &tx,
            Domain {
                metric_allowlist: vec!["also.allowed".to_string()],
                ..base.clone()
            },
        );
        tokio::time::timeout(Duration::from_secs(2), gw.live.changed())
            .await
            .expect("live view should observe the allowlist update");
        let domain = gw.live.current();
        gw.apply_domain(&domain);

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("also.allowed", 1.0))));
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("blocked.metric", 1.0))));
    }
}
