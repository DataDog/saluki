//! Single-region fused metric execution transform with live generation switching.
//!
//! Hosts a [`GenerationManager`] inside one Saluki [`Transform`] task with zero intermediate channels between metric
//! operators. Supports 0, 1, or N stateful aggregate operators natively, atomic stateless generation reloads with
//! live state transfer, exact bucket-boundary cuts for stateful changes, and live runtime-binding updates.

use std::{collections::BTreeMap, future::pending, sync::Arc, time::Duration};

use async_trait::async_trait;
use saluki_common::{sync::live::Live, time::get_unix_timestamp};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{
        metric::{context::Context, Metric},
        Event, EventType,
    },
    topology::{EventsBuffer, EventsDispatcher, OutputDefinition, OutputName},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::{
    aggregate::{AggregateContextSnapshotEntry, AggregatedMetric},
    routing_filter::MetricMirroring,
    tag_filter::DEFAULT_CONTEXT_CACHE_CAPACITY as TAG_FILTER_CONTEXT_CACHE_CAPACITY,
};
use saluki_metric_plan::{
    lowering::TARGET_PRIMARY, validate_plan, GenerationManager, InstallOutcome, MetricPlan, Op, RuntimeBindings,
    ValidatedPlan,
};
use tokio::{
    select,
    time::{interval_at, sleep_until, Instant},
};
use tracing::{debug, error, info, trace, warn};

use crate::transforms::aggregate::{
    send_context_snapshot_if_open, AggregateContextSnapshotReceiver, AggregateContextSnapshotRequest,
    AggregateContextSnapshotRequestReceiver,
};

/// Classifier mapping an incoming metric to a logical plan source stream name.
pub type StreamClassifier = Arc<dyn Fn(&Metric) -> &'static str + Send + Sync>;

/// Configuration for [`MetricRegion`], a single fused transform running compiled [`MetricPlan`] generations.
pub struct MetricRegionConfiguration {
    plan: ValidatedPlan,
    default_stream: String,
    stream_classifier: Option<StreamClassifier>,
    flush_interval: Duration,
    initial_bindings: RuntimeBindings,
    live_plans: Option<Live<MetricPlan>>,
    live_mirroring: Option<(String, Live<MetricMirroring>)>,
    host_tags_expiry: Option<(String, Duration)>,
    context_snapshot_receiver: Option<AggregateContextSnapshotReceiver>,
    outputs: Vec<OutputDefinition<EventType>>,
}

impl MetricRegionConfiguration {
    /// Creates a new `MetricRegionConfiguration` for `plan`.
    ///
    /// Metrics arriving on the transform's input enter at `default_stream` unless overridden with
    /// [`with_stream_classifier`][Self::with_stream_classifier]. The plan's `"primary"` emit target (when present)
    /// binds to the transform's default output; every other emit target binds to a named output of the same name.
    pub fn new(plan: ValidatedPlan, default_stream: impl Into<String>, flush_interval: Duration) -> Self {
        let mut outputs = Vec::new();
        for node in plan.plan().nodes.iter() {
            if let Op::Emit { target } = &node.op {
                if target == TARGET_PRIMARY {
                    outputs.push(OutputDefinition::default_output(EventType::Metric));
                } else {
                    outputs.push(OutputDefinition::named_output(target.clone(), EventType::Metric));
                }
            }
        }

        Self {
            plan,
            default_stream: default_stream.into(),
            stream_classifier: None,
            flush_interval,
            initial_bindings: RuntimeBindings::default(),
            live_plans: None,
            live_mirroring: None,
            host_tags_expiry: None,
            context_snapshot_receiver: None,
            outputs,
        }
    }

    /// Supplies initial runtime bindings (host tags and mirroring signals).
    pub fn with_bindings(mut self, bindings: RuntimeBindings) -> Self {
        self.initial_bindings = bindings;
        self
    }

    /// Installs a per-metric stream classifier for regions accepting multiple logical source streams.
    pub fn with_stream_classifier(mut self, classifier: StreamClassifier) -> Self {
        self.stream_classifier = Some(classifier);
        self
    }

    /// Attaches a live stream of whole [`MetricPlan`] updates for atomic generation reloads and bucket-boundary cuts.
    pub fn with_live_plans(mut self, live_plans: Live<MetricPlan>) -> Self {
        self.live_plans = Some(live_plans);
        self
    }

    /// Attaches a live mirroring signal bound to `binding_name` inside the active plan.
    pub fn with_live_mirroring(mut self, binding_name: impl Into<String>, signal: Live<MetricMirroring>) -> Self {
        self.live_mirroring = Some((binding_name.into(), signal));
        self
    }

    /// Clears `binding_name` from runtime host-tags bindings after `duration` elapses from region startup.
    pub fn with_host_tags_expiry(mut self, binding_name: impl Into<String>, duration: Duration) -> Self {
        self.host_tags_expiry = Some((binding_name.into(), duration));
        self
    }

    /// Attaches a retained-context snapshot receiver so diagnostics (such as `dogstatsd top`) can query stateful
    /// contexts inside this region.
    pub fn with_context_snapshot_receiver(mut self, receiver: AggregateContextSnapshotReceiver) -> Self {
        self.context_snapshot_receiver = Some(receiver);
        self
    }
}

#[async_trait]
impl TransformBuilder for MetricRegionConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let mut bindings = self.initial_bindings.clone();
        if let Some((binding_name, signal)) = &self.live_mirroring {
            bindings
                .mirroring_signals
                .insert(binding_name.clone(), (**signal).clone());
        }

        let generations = GenerationManager::new(self.plan.clone(), bindings.clone())?;
        if generations.active_engine().stream_index(&self.default_stream).is_none() {
            return Err(generic_error!(
                "plan does not declare default stream `{}`",
                self.default_stream
            ));
        }

        let mut target_to_output = BTreeMap::new();
        for node in &self.plan.plan().nodes {
            if let Op::Emit { target } = &node.op {
                let output = if target == TARGET_PRIMARY {
                    OutputName::Default
                } else {
                    OutputName::Given(target.clone().into())
                };
                target_to_output.insert(target.clone(), output);
            }
        }

        let context_snapshot_requests = self
            .context_snapshot_receiver
            .as_ref()
            .map(AggregateContextSnapshotReceiver::take_receiver)
            .transpose()?;

        Ok(Box::new(MetricRegion {
            generations,
            bindings,
            default_stream: self.default_stream.clone(),
            stream_classifier: self.stream_classifier.clone(),
            stream_batches: BTreeMap::new(),
            flush_interval: self.flush_interval,
            live_plans: self.live_plans.clone(),
            live_mirroring: self.live_mirroring.clone(),
            host_tags_expiry: self.host_tags_expiry.clone(),
            context_snapshot_requests,
            target_to_output,
        }))
    }
}

impl MemoryBounds for MetricRegionConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<MetricRegion>("component struct");

        for node in &self.plan.plan().nodes {
            match &node.op {
                Op::Aggregate { context_limit, .. } => {
                    builder
                        .firm()
                        .with_expr(UsageExpr::product(
                            "aggregation state map",
                            UsageExpr::sum(
                                "context map entry",
                                UsageExpr::struct_size::<Context>("context"),
                                UsageExpr::struct_size::<AggregatedMetric<()>>("aggregated metric"),
                            ),
                            UsageExpr::config("aggregate_context_limit", *context_limit),
                        ))
                        .with_expr(UsageExpr::product(
                            "retained context snapshot",
                            UsageExpr::struct_size::<AggregateContextSnapshotEntry>("snapshot entry"),
                            UsageExpr::config("aggregate_context_limit", *context_limit),
                        ));
                }
                Op::TagFilter {
                    entries,
                    value_allowlists,
                } if !entries.is_empty() || !value_allowlists.is_empty() => {
                    builder
                        .firm()
                        .with_fixed_amount("tag filter context cache", TAG_FILTER_CONTEXT_CACHE_CAPACITY * 64);
                }
                _ => {}
            }
        }
    }
}

struct MetricRegion {
    generations: GenerationManager,
    bindings: RuntimeBindings,
    default_stream: String,
    stream_classifier: Option<StreamClassifier>,
    stream_batches: BTreeMap<&'static str, Vec<Metric>>,
    flush_interval: Duration,
    live_plans: Option<Live<MetricPlan>>,
    live_mirroring: Option<(String, Live<MetricMirroring>)>,
    host_tags_expiry: Option<(String, Duration)>,
    context_snapshot_requests: Option<AggregateContextSnapshotRequestReceiver>,
    target_to_output: BTreeMap<String, OutputName>,
}

#[inline]
fn push_emitted(emitted: &mut BTreeMap<String, Vec<Metric>>, target: &str, metric: Metric) {
    if let Some(buf) = emitted.get_mut(target) {
        buf.push(metric);
    } else {
        emitted.insert(target.to_string(), vec![metric]);
    }
}

impl MetricRegion {
    async fn dispatch_emitted(
        &self, emitted: &mut BTreeMap<String, Vec<Metric>>, dispatcher: &EventsDispatcher,
    ) -> Result<(), GenericError> {
        for (target, metrics) in emitted.iter_mut() {
            if metrics.is_empty() {
                continue;
            }
            let Some(output_name) = self.target_to_output.get(target) else {
                warn!(target = %target, count = metrics.len(), "Dropping metrics for undeclared region emit target.");
                metrics.clear();
                continue;
            };
            let mut buffered = match output_name {
                OutputName::Default => dispatcher.buffered()?,
                OutputName::Given(name) => dispatcher.buffered_named(name.as_ref())?,
            };
            for metric in metrics.drain(..) {
                buffered.push(Event::Metric(metric)).await?;
            }
            if metrics.capacity() > 256 {
                metrics.shrink_to(256);
            }
            buffered.flush().await?;
        }
        Ok(())
    }

    fn ingest_buffer(&mut self, events: EventsBuffer, now: u64, emitted: &mut BTreeMap<String, Vec<Metric>>) {
        if let Some(classifier) = self.stream_classifier.as_ref() {
            for metric in events.into_events::<Metric>() {
                let stream_name = classifier(&metric);
                self.stream_batches.entry(stream_name).or_default().push(metric);
            }
            for (&stream_name, batch) in self.stream_batches.iter_mut() {
                if batch.is_empty() {
                    continue;
                }
                let effective_stream = if self.generations.active_engine().stream_index(stream_name).is_some() {
                    stream_name
                } else {
                    self.default_stream.as_str()
                };
                if let Err(e) = self
                    .generations
                    .ingest(effective_stream, now, batch.drain(..), |target, metric| {
                        push_emitted(emitted, target, metric);
                    })
                {
                    error!(stream = effective_stream, error = %e, "Failed to ingest metrics into region.");
                }
            }
        } else if let Err(e) = self.generations.ingest(
            &self.default_stream,
            now,
            events.into_events::<Metric>(),
            |target, metric| {
                push_emitted(emitted, target, metric);
            },
        ) {
            error!(stream = %self.default_stream, error = %e, "Failed to ingest metrics into region.");
        }
    }

    fn handle_plan_update(&mut self, candidate: MetricPlan, now: u64) {
        let validated = match validate_plan(&candidate) {
            Ok(validated) => validated,
            Err(errors) => {
                error!(
                    error_count = errors.len(),
                    first_error = %errors[0],
                    "Rejected invalid live metric plan update; keeping active generation."
                );
                return;
            }
        };

        match self.generations.install(validated, now) {
            Ok(InstallOutcome::SwitchedImmediately {
                generation,
                reused_stateful_nodes,
            }) => {
                info!(
                    generation = generation.0,
                    reused_stateful_nodes, "Installed live metric plan generation atomically."
                );
            }
            Ok(InstallOutcome::ScheduledCut {
                generation,
                cut_timestamp_secs,
            }) => {
                info!(
                    generation = generation.0,
                    cut_timestamp_secs, "Scheduled stateful metric plan cut at shared bucket boundary."
                );
            }
            Err(e) => {
                error!(error = %e, "Failed to compile live metric plan update; keeping active generation.");
            }
        }
    }
}

#[async_trait]
impl Transform for MetricRegion {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let mut flush_timer = self
            .generations
            .has_stateful_operators()
            .then(|| interval_at(tokio::time::Instant::now() + self.flush_interval, self.flush_interval));
        let mut host_tags_deadline = self
            .host_tags_expiry
            .take()
            .map(|(binding, duration)| (binding, Instant::now() + duration));
        let mut final_flush = false;
        let mut emitted: BTreeMap<String, Vec<Metric>> = BTreeMap::new();

        health.mark_ready();
        debug!(
            generation = self.generations.active_generation().0,
            steps = self.generations.active_engine().step_count(),
            stateful = self.generations.active_engine().stateful_count(),
            targets = self.target_to_output.len(),
            "Metric region transform started."
        );

        loop {
            select! {
                _ = health.live() => continue,
                _ = next_flush_tick(&mut flush_timer) => {
                    if self.generations.has_buffered_state() || self.generations.pending_cut_timestamp().is_some() {
                        let now = get_unix_timestamp();
                        self.generations.flush(now, final_flush, |target, metric| {
                            push_emitted(&mut emitted, target, metric);
                        });
                        if let Err(e) = self.dispatch_emitted(&mut emitted, context.dispatcher()).await {
                            error!(error = %e, "Failed to dispatch flushed metrics from region.");
                        }
                    }

                    if final_flush {
                        break;
                    }
                },
                expired_binding = next_host_tags_expiry(&mut host_tags_deadline) => {
                    if let Some(binding) = expired_binding {
                        self.bindings.host_tags.remove(&binding);
                        self.generations.set_bindings(self.bindings.clone());
                        debug!(binding = %binding, "Host tags enrichment duration elapsed; cleared host tags binding.");
                    }
                },
                snapshot_request = receive_context_snapshot_request(&mut self.context_snapshot_requests) => {
                    match snapshot_request {
                        Some(response) => {
                            send_context_snapshot_if_open(response, || self.generations.snapshot_contexts());
                        }
                        None => self.context_snapshot_requests = None,
                    }
                },
                maybe_plan = next_live_plan(&mut self.live_plans), if !final_flush => {
                    if let Some(candidate) = maybe_plan {
                        self.handle_plan_update(candidate, get_unix_timestamp());
                        if flush_timer.is_none() && self.generations.has_stateful_operators() {
                            flush_timer = Some(interval_at(
                                tokio::time::Instant::now() + self.flush_interval,
                                self.flush_interval,
                            ));
                        }
                    }
                },
                maybe_mirroring = next_live_mirroring(&mut self.live_mirroring), if !final_flush => {
                    if let Some((binding_name, mirroring)) = maybe_mirroring {
                        self.bindings.mirroring_signals.insert(binding_name, mirroring);
                        self.generations.set_bindings(self.bindings.clone());
                        debug!("Updated live mirroring signal on metric region.");
                    }
                },
                maybe_events = context.events().next(), if !final_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Metric region received events.");
                        let now = get_unix_timestamp();
                        self.ingest_buffer(events, now, &mut emitted);
                        if let Err(e) = self.dispatch_emitted(&mut emitted, context.dispatcher()).await {
                            error!(error = %e, "Failed to dispatch metrics from region.");
                        }
                    }
                    None => {
                        if self.generations.has_stateful_operators() || self.generations.has_buffered_state() {
                            final_flush = true;
                            if let Some(timer) = flush_timer.as_mut() {
                                timer.reset_immediately();
                            } else {
                                flush_timer = Some(interval_at(tokio::time::Instant::now(), self.flush_interval));
                            }
                        } else {
                            break;
                        }
                    }
                },
            }
        }

        debug!("Metric region transform stopped.");
        Ok(())
    }
}

async fn next_flush_tick(timer: &mut Option<tokio::time::Interval>) {
    match timer {
        Some(interval) => {
            interval.tick().await;
        }
        None => pending::<()>().await,
    }
}

async fn next_live_plan(plans: &mut Option<Live<MetricPlan>>) -> Option<MetricPlan> {
    match plans {
        Some(live) => Some(live.changed().await),
        None => pending().await,
    }
}

async fn next_live_mirroring(
    mirroring: &mut Option<(String, Live<MetricMirroring>)>,
) -> Option<(String, MetricMirroring)> {
    match mirroring {
        Some((name, live)) => Some((name.clone(), live.changed().await)),
        None => pending().await,
    }
}

async fn next_host_tags_expiry(deadline: &mut Option<(String, Instant)>) -> Option<String> {
    match deadline {
        Some((_, at)) => {
            sleep_until(*at).await;
            deadline.take().map(|(binding, _)| binding)
        }
        None => pending().await,
    }
}

async fn receive_context_snapshot_request(
    receiver: &mut Option<AggregateContextSnapshotRequestReceiver>,
) -> Option<AggregateContextSnapshotRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use saluki_common::sync::live;
    use saluki_core::{
        components::test_util::TestComponentDriver,
        data_model::event::{
            metric::{context::Context, Metric, MetricValues},
            Event,
        },
        topology::EventsBuffer,
    };
    use saluki_metric_kernels::{
        filterlist::MetricNameFilter,
        histogram::HistogramConfiguration,
        routing_filter::MetricMirroring,
        tag_filter::{FilterAction, MetricTagFilterEntry},
    };
    use saluki_metric_plan::{
        lowering::{
            lower_native_pipeline, NativePipelineConfig, RoutingPolicySpec, MRF_SIGNAL_BINDING, STREAM_DOGSTATSD,
        },
        validate_plan, MetricPlan, Op,
    };

    use super::*;

    fn events_from(metrics: Vec<Metric>) -> EventsBuffer {
        let mut buf = EventsBuffer::default();
        for metric in metrics {
            assert!(buf.try_push(Event::Metric(metric)).is_none());
        }
        buf
    }

    #[tokio::test]
    async fn single_region_executes_lowered_dogstatsd_plan_with_named_routing_outputs() {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            metric_prefix: "acme".to_string(),
            tag_filter_entries: vec![MetricTagFilterEntry {
                metric_name: "acme.hits".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["drop".to_string()],
            }],
            aggregate_flush_open_windows: true,
            aggregate_counter_expiry_secs: 0,
            routing_policies: vec![RoutingPolicySpec {
                target: "routing_0".to_string(),
                metric_allowlist: vec!["acme.hits".to_string()],
                metric_prefix_allowlist: vec![],
            }],
            mrf_branch_configured: true,
            mrf_enabled: true,
            mrf_mirroring: MetricMirroring {
                enabled: true,
                allowlist: vec!["acme.hits".to_string()],
            },
            ..NativePipelineConfig::default()
        };

        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let region_cfg = MetricRegionConfiguration::new(validated, STREAM_DOGSTATSD, Duration::from_secs(60));

        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("metrics_region")
            .transform(region_cfg)
            .await
            .expect("region should build");

        control
            .send(events_from(vec![
                Metric::counter(Context::from_static_parts("hits", &["drop:1", "env:prod"]), 3.0),
                Metric::counter(Context::from_static_parts("hits", &["drop:2", "env:prod"]), 5.0),
                Metric::gauge("ts.value", (1000, 9.0)),
            ]))
            .await;
        control.shutdown().await.expect("region shutdown");

        let primary: Vec<Metric> = outputs
            .default_output()
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();
        let routed: Vec<Metric> = outputs
            .named_output("routing_0")
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();
        let mrf: Vec<Metric> = outputs
            .named_output("mrf")
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();

        // Primary receives both the immediate timestamped gauge and the merged aggregated counter (as rate).
        assert_eq!(primary.len(), 2);
        assert!(primary.iter().any(|m| m.context().name() == "acme.ts.value"));
        let hits = primary
            .iter()
            .find(|m| m.context().name() == "acme.hits")
            .expect("merged hits");
        assert!(matches!(hits.values(), MetricValues::Rate(_, _)));

        // Routing and MRF branches receive only `acme.hits`.
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].context().name(), "acme.hits");
        assert_eq!(mrf.len(), 1);
        assert_eq!(mrf[0].context().name(), "acme.hits");
    }

    #[tokio::test]
    async fn single_region_runs_chained_two_aggregate_plan_to_completion() {
        let plan = MetricPlan::builder()
            .node(
                "in",
                Op::Source {
                    stream: "dsd".to_string(),
                },
            )
            .node(
                "agg1",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(10).unwrap(),
                    context_limit: 100,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "agg2",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(60).unwrap(),
                    context_limit: 100,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "out",
                Op::Emit {
                    target: TARGET_PRIMARY.to_string(),
                },
            )
            .edge("in", "agg1")
            .edge("agg1", "agg2")
            .edge("agg2", "out")
            .build();

        let validated = validate_plan(&plan).unwrap();
        let region_cfg = MetricRegionConfiguration::new(validated, "dsd", Duration::from_secs(60));
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("chained_region")
            .transform(region_cfg)
            .await
            .expect("build");

        control.send(events_from(vec![Metric::gauge("temp", 42.0)])).await;
        control.shutdown().await.expect("shutdown");

        let emitted: Vec<Metric> = outputs
            .default_output()
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].context().name(), "temp");
    }

    #[tokio::test]
    async fn live_plan_reload_preserves_state_and_live_mirroring_toggles_mrf() {
        let base_config = NativePipelineConfig {
            dogstatsd_enabled: true,
            aggregate_flush_open_windows: true,
            aggregate_counter_expiry_secs: 0,
            mrf_branch_configured: true,
            mrf_enabled: true,
            mrf_mirroring: MetricMirroring {
                enabled: false,
                allowlist: vec![],
            },
            ..NativePipelineConfig::default()
        };
        let initial_plan = lower_native_pipeline(&base_config);
        let validated = validate_plan(&initial_plan).unwrap();

        let (mut plan_publisher, plan_source) = live::channel(initial_plan);
        let (mut mrf_publisher, mrf_source) = live::channel(MetricMirroring {
            enabled: false,
            allowlist: vec![],
        });

        let region_cfg = MetricRegionConfiguration::new(validated, STREAM_DOGSTATSD, Duration::from_secs(60))
            .with_live_plans(plan_source.live(|p| p))
            .with_live_mirroring(MRF_SIGNAL_BINDING, mrf_source.live(|m| m));

        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("live_region")
            .transform(region_cfg)
            .await
            .expect("region should build");

        // 1. Send a counter before any reload (retained in AggregateCore).
        control.send(events_from(vec![Metric::counter("keep.me", 4.0)])).await;
        control.wait_until_input_drained().await;

        // 2. Publish a live filterlist update dropping `block.me`, and enable live MRF mirroring!
        let mut updated_config = base_config;
        updated_config.metric_filter = MetricNameFilter {
            values: vec!["block.me".to_string()],
            match_prefix: false,
            prefix_rules: vec![],
        };
        plan_publisher.publish(lower_native_pipeline(&updated_config));
        mrf_publisher.publish(MetricMirroring {
            enabled: true,
            allowlist: vec!["keep.me".to_string()],
        });

        // Yield briefly so the region task processes both live updates before admitting the next batch.
        tokio::time::sleep(Duration::from_millis(25)).await;

        // 3. Admit another `keep.me` sample (must merge with pre-reload 4.0 -> 10.0) and a now-blocked `block.me`.
        control
            .send(events_from(vec![
                Metric::counter("keep.me", 6.0),
                Metric::counter("block.me", 99.0),
            ]))
            .await;
        control.shutdown().await.expect("shutdown");

        let primary: Vec<Metric> = outputs
            .default_output()
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();
        let mrf: Vec<Metric> = outputs
            .named_output("mrf")
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();

        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].context().name(), "keep.me");
        let total: f64 = match primary[0].values() {
            MetricValues::Rate(points, _) => points.into_iter().map(|(_, v)| v).sum(),
            other => panic!("expected rate, got {other:?}"),
        };
        assert_eq!(total, 10.0, "pre-reload and post-reload samples must both be preserved");

        // MRF received `keep.me` after live mirroring was enabled.
        assert_eq!(mrf.len(), 1);
        assert_eq!(mrf[0].context().name(), "keep.me");
    }

    #[tokio::test]
    async fn region_serves_context_snapshots_and_expires_host_tags() {
        use saluki_core::data_model::tags::{SharedTagSet, Tag};
        use saluki_metric_plan::lowering::HOST_TAGS_BINDING;

        use crate::transforms::aggregate_context_snapshot_channel;

        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            host_tags_enabled: true,
            aggregate_flush_open_windows: true,
            aggregate_counter_expiry_secs: 0,
            ..NativePipelineConfig::default()
        };
        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let (snapshot_handle, snapshot_rx) = aggregate_context_snapshot_channel();
        let bindings = RuntimeBindings::default()
            .with_host_tags(HOST_TAGS_BINDING, SharedTagSet::from_iter([Tag::from("host_tag:v1")]));

        let region_cfg = MetricRegionConfiguration::new(validated, STREAM_DOGSTATSD, Duration::from_secs(60))
            .with_bindings(bindings)
            .with_host_tags_expiry(HOST_TAGS_BINDING, Duration::from_millis(40))
            .with_context_snapshot_receiver(snapshot_rx);

        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("snapshot_and_expiry_region")
            .transform(region_cfg)
            .await
            .expect("region should build");

        // Send a non-timestamped metric (retained in AggregateCore) and a timestamped gauge (emitted immediately with host tags).
        control
            .send(events_from(vec![
                Metric::counter("retained.counter", 1.0),
                Metric::gauge("before.expiry", (1000, 1.0)),
            ]))
            .await;
        control.wait_until_input_drained().await;

        // Context snapshot must return the retained counter context.
        let snapshot = snapshot_handle.snapshot().await.expect("snapshot succeeds");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].context().name(), "retained.counter");

        // Wait past `expected_tags_duration` and send another timestamped gauge; it must NOT get `host_tag:v1`.
        tokio::time::sleep(Duration::from_millis(60)).await;
        control
            .send(events_from(vec![Metric::gauge("after.expiry", (1001, 2.0))]))
            .await;
        control.shutdown().await.expect("shutdown");

        let emitted: Vec<Metric> = outputs
            .default_output()
            .collect_events()
            .await
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect();

        let before = emitted
            .iter()
            .find(|m| m.context().name() == "before.expiry")
            .expect("before.expiry emitted");
        assert!(before.context().tags().has_tag(Tag::from("host_tag:v1")));

        let after = emitted
            .iter()
            .find(|m| m.context().name() == "after.expiry")
            .expect("after.expiry emitted");
        assert!(!after.context().tags().has_tag(Tag::from("host_tag:v1")));
    }
}
