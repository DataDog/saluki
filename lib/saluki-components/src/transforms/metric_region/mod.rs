//! Single-region fused metric execution transform.
//!
//! Hosts a compiled [`FusedMetricEngine`] inside one Saluki [`Transform`] task with zero intermediate channels
//! between metric operators. Supports 0, 1, or N stateful aggregate operators natively inside the single region
//! driver and exposes one topology output per terminal `Emit` target in the plan.

use std::{collections::BTreeMap, future::pending, sync::Arc, time::Duration};

use async_trait::async_trait;
use saluki_common::time::get_unix_timestamp;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{metric::Metric, Event, EventType},
    topology::{EventsBuffer, EventsDispatcher, OutputDefinition, OutputName},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_plan::{lowering::TARGET_PRIMARY, FusedMetricEngine, Op, RuntimeBindings, ValidatedPlan};
use tokio::{select, time::interval_at};
use tracing::{debug, error, trace};

/// Classifier mapping an incoming metric to a logical plan source stream name.
pub type StreamClassifier = Arc<dyn Fn(&Metric) -> &'static str + Send + Sync>;

/// Configuration for [`MetricRegion`], a single fused transform running a compiled [`saluki_metric_plan::MetricPlan`].
pub struct MetricRegionConfiguration {
    plan: ValidatedPlan,
    default_stream: String,
    stream_classifier: Option<StreamClassifier>,
    flush_interval: Duration,
    initial_bindings: RuntimeBindings,
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
        let engine = FusedMetricEngine::compile(&self.plan, self.initial_bindings.clone())?;
        let default_stream_idx = engine
            .stream_index(&self.default_stream)
            .ok_or_else(|| generic_error!("plan does not declare default stream `{}`", self.default_stream))?;

        let target_outputs: Vec<OutputName> = engine
            .target_names()
            .iter()
            .map(|name| {
                if name == TARGET_PRIMARY {
                    OutputName::Default
                } else {
                    OutputName::Given(name.clone().into())
                }
            })
            .collect();

        Ok(Box::new(MetricRegion {
            engine,
            default_stream_idx,
            stream_classifier: self.stream_classifier.clone(),
            flush_interval: self.flush_interval,
            target_outputs,
        }))
    }
}

impl MemoryBounds for MetricRegionConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<MetricRegion>("component struct");
    }
}

struct MetricRegion {
    engine: FusedMetricEngine,
    default_stream_idx: usize,
    stream_classifier: Option<StreamClassifier>,
    flush_interval: Duration,
    target_outputs: Vec<OutputName>,
}

impl MetricRegion {
    async fn dispatch_emitted(
        &self, target_buffers: &mut [Vec<Metric>], dispatcher: &EventsDispatcher,
    ) -> Result<(), GenericError> {
        for (target_idx, metrics) in target_buffers.iter_mut().enumerate() {
            if metrics.is_empty() {
                continue;
            }
            let output_name = &self.target_outputs[target_idx];
            let mut buffered = match output_name {
                OutputName::Default => dispatcher.buffered()?,
                OutputName::Given(name) => dispatcher.buffered_named(name.as_ref())?,
            };
            for metric in metrics.drain(..) {
                buffered.push(Event::Metric(metric)).await?;
            }
            buffered.flush().await?;
        }
        Ok(())
    }

    fn ingest_buffer(&mut self, events: EventsBuffer, now: u64, target_buffers: &mut [Vec<Metric>]) {
        if let Some(classifier) = self.stream_classifier.clone() {
            let mut by_stream: BTreeMap<usize, Vec<Metric>> = BTreeMap::new();
            for metric in events.into_events::<Metric>() {
                let stream_name = classifier(&metric);
                let stream_idx = self.engine.stream_index(stream_name).unwrap_or(self.default_stream_idx);
                by_stream.entry(stream_idx).or_default().push(metric);
            }
            for (stream_idx, batch) in by_stream {
                self.engine.ingest(stream_idx, now, batch, |target_idx, metric| {
                    target_buffers[target_idx].push(metric);
                });
            }
        } else {
            let stream_idx = self.default_stream_idx;
            self.engine
                .ingest(stream_idx, now, events.into_events::<Metric>(), |target_idx, metric| {
                    target_buffers[target_idx].push(metric);
                });
        }
    }
}

#[async_trait]
impl Transform for MetricRegion {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let has_stateful = self.engine.stateful_count() > 0;
        let mut flush_timer =
            has_stateful.then(|| interval_at(tokio::time::Instant::now() + self.flush_interval, self.flush_interval));
        let mut final_flush = false;
        let mut target_buffers: Vec<Vec<Metric>> = (0..self.target_outputs.len()).map(|_| Vec::new()).collect();

        health.mark_ready();
        debug!(
            steps = self.engine.step_count(),
            stateful = self.engine.stateful_count(),
            targets = self.target_outputs.len(),
            "Metric region transform started."
        );

        loop {
            select! {
                _ = health.live() => continue,
                _ = next_flush_tick(&mut flush_timer) => {
                    if self.engine.has_buffered_state() {
                        let now = get_unix_timestamp();
                        self.engine.flush(now, final_flush, |target_idx, metric| {
                            target_buffers[target_idx].push(metric);
                        });
                        if let Err(e) = self.dispatch_emitted(&mut target_buffers, context.dispatcher()).await {
                            error!(error = %e, "Failed to dispatch flushed metrics from region.");
                        }
                    }

                    if final_flush {
                        break;
                    }
                },
                maybe_events = context.events().next(), if !final_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Metric region received events.");
                        let now = get_unix_timestamp();
                        self.ingest_buffer(events, now, &mut target_buffers);
                        if let Err(e) = self.dispatch_emitted(&mut target_buffers, context.dispatcher()).await {
                            error!(error = %e, "Failed to dispatch metrics from region.");
                        }
                    }
                    None => {
                        if has_stateful {
                            final_flush = true;
                            if let Some(timer) = flush_timer.as_mut() {
                                timer.reset_immediately();
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use saluki_core::{
        components::test_util::TestComponentDriver,
        data_model::event::{
            metric::{context::Context, Metric, MetricValues},
            Event,
        },
        topology::EventsBuffer,
    };
    use saluki_metric_kernels::{
        histogram::HistogramConfiguration,
        routing_filter::MetricMirroring,
        tag_filter::{FilterAction, MetricTagFilterEntry},
    };
    use saluki_metric_plan::{
        lowering::{lower_native_pipeline, NativePipelineConfig, RoutingPolicySpec, STREAM_DOGSTATSD},
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
}
