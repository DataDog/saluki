//! The default program's regions run through `MetricRegionDriver`, against hand-written baselines that call the same
//! kernels in sequence.
//!
//! - **Pre-aggregation**: R1's cost model pipelines the stage 3 default program's DogStatsD chain over three regions:
//!   the mapper; the namespace prefix with the listener filterlist; and the timestamp split with the tag filterlist.
//!   The driver side runs a batch through the three drivers, handing each one's exit buffers to the next, as the
//!   topology does; the baseline calls the four kernels and the filter on the batch in place. Both deliver the same
//!   events, stamped with the batch's header, toward aggregation and toward the passthrough's merge. Measured over
//!   batches of 1,024 metrics with 256 and 4,096 contexts, and of 128 metrics.
//! - **Post-aggregation**: the post-aggregation filter's region (the filter and the client telemetry tap's copy) and
//!   the enrichment region (host enrichment, host tags, and the five-way fan-out to the primary output and the four
//!   gateways), against a baseline that filters with the compiled predicate, copies the buffer for the tap, calls both
//!   kernels, and copies the buffer for each gateway.
//!
//! - **Aggregation**: the default program's isolation slot `dsd_agg`, with aggregation as a stateful kernel behind its
//!   isolation fence: admitting a batch of 1,024 metrics over 256 contexts, and a flush tick that closes the buckets
//!   of 100,000 contexts, run in drain steps of the limits table's size, as the region host runs them, against the
//!   kernel called directly in one call.
//!
//! Setup checks that each side delivers the same events to each destination. The baselines are what hand-written
//! components would do, with no program, no registers, and no telemetry.

use std::{sync::Arc, time::Instant};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, ScalarPoints},
        Event,
    },
    data_model::tags::{Tag, TagSet},
    execution::{
        AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, Budget, ExitSink, GenerationId, GenerationRef,
        GenerationToken, Kernel, PortIndex, PortOutputs, Progress, RegionDriver, RegionTurn, Turn, TurnCounters,
        WallClock, WorkKind,
    },
    topology::EventsBuffer,
};
use saluki_metric_plan::{
    compile::{compile, CompiledPlan, EntryKey, Frame, FrameTarget, RegionProgram, SlotId, SlotKind, Step},
    ir::{NativeComponentId, NodeId, Plan},
    model::EventView,
    operators::{KernelBuildContext, OperatorRegistry},
    predicate::CompiledPredicate,
    runtime::{build_kernels, MetricRegionDriver, NoTelemetry, PanicBudget, PrepareRegion, RegionConfig},
    testing::{default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON},
    validate::Validator,
};

fn tags(tags: &[String]) -> TagSet {
    tags.iter().map(|tag| Tag::from(tag.as_str())).collect()
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    saluki_metric_plan::testing::default_program_slot_name(nodes)
}

/// The stage 3 default program's frame and its compiled programs.
fn default_program() -> (Frame, CompiledPlan) {
    let plan = Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    assert!(report.is_valid(), "{}", report);
    let frame = Frame::compute(&plan, &registry, &namer).unwrap();
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    (frame, compiled)
}

/// Returns the slot named `name`.
fn slot_named(frame: &Frame, name: &str) -> SlotId {
    frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == name)
        .unwrap_or_else(|| panic!("no slot `{}`", name))
        .id
}

/// A DogStatsD-like batch of `size` metrics: mapped request counters, blocked and excluded names, latency
/// distributions whose `user_id` tag the tag filterlist removes, gauges, and timestamped counters for the passthrough,
/// over `contexts` contexts.
fn dogstatsd_input(header: BatchHeader, size: usize, contexts: usize) -> EventsBuffer {
    let mut buffer = EventsBuffer::default().with_header(header);
    for i in 0..size {
        let context = i % contexts;
        let metric = match i % 8 {
            0 | 1 => Metric::counter(
                Context::from_parts(
                    format!("raw.requests.route{}", context % 16),
                    tags(&["env:prod".to_string(), format!("instance:{}", context)]),
                ),
                1.0,
            ),
            2 => Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
            3 | 4 => Metric::distribution(
                Context::from_parts(
                    "checkout.latency",
                    tags(&[format!("user_id:{}", context), "env:dev".to_string()]),
                ),
                10.0,
            ),
            5 => Metric::gauge(
                Context::from_parts(format!("system.disk{}", context % 8), TagSet::default()),
                9.0,
            ),
            6 => Metric::gauge(
                Context::from_parts(format!("app.queue{}", context), tags(&["team:core".to_string()])),
                7.0,
            ),
            _ => Metric::counter(
                Context::from_parts(format!("app.jobs{}", context % 32), TagSet::default()),
                ScalarPoints::from_iter([(std::num::NonZeroU64::new(1_700_000_000), 2.0)]),
            ),
        };
        assert!(buffer.try_push(Event::Metric(metric)).is_none());
    }
    buffer
}

/// An aggregation flush of 1,024 metrics: counters, gauges, distributions, and the maximum of a histogram, which the
/// post-aggregation filter blocks, over 256 contexts.
fn aggregated_input(header: BatchHeader) -> EventsBuffer {
    let mut buffer = EventsBuffer::default().with_header(header);
    for i in 0..1024 {
        let context = i % 256;
        let name = |base: &str| format!("tenant.{}{}", base, context % 32);
        let metric = match i % 4 {
            0 => Metric::counter(Context::from_parts(name("requests"), TagSet::default()), 3.0),
            1 => Metric::gauge(Context::from_parts(name("queue"), tags(&["env:prod".to_string()])), 7.0),
            2 => Metric::distribution(Context::from_parts(name("latency"), TagSet::default()), 10.0),
            _ => Metric::gauge(Context::from_static_name("tenant.checkout.latency.max"), 4.0),
        };
        assert!(buffer.try_push(Event::Metric(metric)).is_none());
    }
    buffer
}

fn header(generation: &GenerationRef, point: u32) -> BatchHeader {
    BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_secs(1_700_000_003)),
        admission_point: Some(AdmissionPointId::new(point)),
        epoch: 0,
        generation: Some(Arc::clone(generation)),
        entry: None,
    }
}

/// One region of a pipeline: its slot, its driver, and the sink its exits are written to.
struct Stage {
    slot: SlotId,
    driver: MetricRegionDriver,
    sink: ExitSink,
}

/// A chain of region drivers, one per slot of a pipeline: what one writes to an exit leading to the next is handed to
/// the next, as the topology delivers it, and everything else is delivered, by exit target.
struct Pipeline {
    frame: Frame,
    stages: Vec<Stage>,
}

impl Pipeline {
    fn new(frame: &Frame, compiled: &CompiledPlan, generation: &GenerationRef, slots: &[SlotId]) -> Self {
        let stages = slots
            .iter()
            .map(|&slot| {
                let program = Arc::clone(&compiled.programs[slot.get()]);
                let keys = program.kernels().iter().map(|kernel| &kernel.key);
                let kernels = build_kernels(&program, keys, KernelBuildContext::detached).unwrap();
                let boot = PrepareRegion::new(
                    Arc::clone(generation),
                    program,
                    kernels,
                    PanicBudget::DEFAULT_PROGRAM,
                    &NoTelemetry,
                );
                let slot_frame = frame.slot(slot).unwrap();
                Stage {
                    slot,
                    driver: MetricRegionDriver::new(RegionConfig::for_slot(slot_frame, &NoTelemetry), boot).unwrap(),
                    sink: ExitSink::new(slot_frame.exits.len()),
                }
            })
            .collect();
        Self {
            frame: frame.clone(),
            stages,
        }
    }

    /// Runs a batch through the pipeline, returning what left it, by target.
    fn run(&mut self, batch: EventsBuffer, now: Instant, budget: &mut Budget) -> Vec<(FrameTarget, EventsBuffer)> {
        let mut delivered = Vec::new();
        let mut pending = vec![(0, batch)];
        let slots = self.stages.iter().map(|stage| stage.slot).collect::<Vec<_>>();
        while let Some((index, batch)) = pending.pop() {
            let stage = &mut self.stages[index];
            stage
                .driver
                .process(&mut RegionTurn::new(now, budget), batch, &mut stage.sink);
            let exits = &self.frame.slot(stage.slot).unwrap().exits;
            for (exit, buffer) in stage.sink.drain() {
                let target = exits[exit.get()].target.clone();
                let next = match &target {
                    FrameTarget::Slot(next) => slots.iter().position(|slot| slot == next),
                    _ => None,
                };
                match next {
                    Some(next) => pending.push((next, buffer)),
                    None => delivered.push((target, buffer)),
                }
            }
        }
        delivered
    }
}

/// Runs one kernel on a buffer in place, with the batch's header.
fn in_place(
    kernel: &mut Box<dyn Kernel>, header: &BatchHeader, now: Instant, ports: &mut PortOutputs,
    counters: &mut TurnCounters, batch: &mut EventsBuffer,
) {
    let mut budget = Budget::unlimited();
    let mut turn = Turn::new(header, now, &mut budget, ports, counters);
    kernel.consume(&mut turn, batch);
}

fn kernel_of(program: &RegionProgram, node: &str) -> Box<dyn Kernel> {
    let kernel = program
        .kernels()
        .iter()
        .find(|kernel| kernel.key.node.to_string() == node)
        .unwrap_or_else(|| panic!("no kernel `{}`", node));
    kernel
        .spec
        .build(&KernelBuildContext::detached(&kernel.key.node))
        .unwrap()
}

fn split_of(program: &RegionProgram) -> Arc<CompiledPredicate> {
    program
        .decisions()
        .iter()
        .find_map(|decision| {
            decision
                .predicates
                .entries()
                .first()
                .map(|entry| Arc::clone(&entry.compiled))
        })
        .unwrap()
}

/// Keeps the events a predicate does not match, as a filter whose `true` port leads to a `drop` does.
fn filter(batch: &mut EventsBuffer, predicate: &CompiledPredicate) {
    let mut dropped = 0u64;
    batch.retain(|event| {
        let matched = event
            .try_as_metric()
            .is_some_and(|metric| predicate.evaluate(&mut EventView::new(metric)));
        dropped += u64::from(matched);
        !matched
    });
    std::hint::black_box(dropped);
}

/// The hand-written pre-aggregation baseline: the chain's kernels and filter, called in sequence.
struct PreAggregationBaseline {
    mapper: Box<dyn Kernel>,
    prefix: Box<dyn Kernel>,
    split: Box<dyn Kernel>,
    tags: Box<dyn Kernel>,
    blocklist: Arc<CompiledPredicate>,
    ports: PortOutputs,
    counters: TurnCounters,
}

impl PreAggregationBaseline {
    fn new(compiled: &CompiledPlan, frame: &Frame) -> Self {
        let program = |name: &str| &compiled.programs[slot_named(frame, name).get()];
        Self {
            mapper: kernel_of(program("dsd_pre_aggregation"), "dsd_enrich"),
            prefix: kernel_of(program("dsd_prefix_filter_region"), "dsd_prefix_filter"),
            split: kernel_of(program("dsd_no_agg_split_region"), "dsd_no_agg_split"),
            tags: kernel_of(program("dsd_no_agg_split_region"), "dsd_tag_filterlist"),
            blocklist: split_of(program("dsd_prefix_filter_region")),
            ports: PortOutputs::new(2),
            counters: TurnCounters::default(),
        }
    }

    /// Runs one batch, returning what leaves toward aggregation and toward the passthrough.
    fn run(&mut self, mut batch: EventsBuffer, now: Instant) -> (Vec<EventsBuffer>, Vec<EventsBuffer>) {
        let header = batch.take_header();
        let mut unused = PortOutputs::new(1);
        in_place(
            &mut self.mapper,
            &header,
            now,
            &mut unused,
            &mut self.counters,
            &mut batch,
        );
        in_place(
            &mut self.prefix,
            &header,
            now,
            &mut unused,
            &mut self.counters,
            &mut batch,
        );
        filter(&mut batch, &self.blocklist);
        in_place(
            &mut self.split,
            &header,
            now,
            &mut self.ports,
            &mut self.counters,
            &mut batch,
        );
        self.ports.complete(batch);
        let mut passthrough = self.ports.drain(PortIndex::new(1)).collect::<Vec<_>>();
        let mut aggregated = self.ports.drain(PortIndex::PRIMARY).collect::<Vec<_>>();
        for buffer in &mut aggregated {
            in_place(&mut self.tags, &header, now, &mut unused, &mut self.counters, buffer);
            buffer.set_header(header.clone());
        }
        for buffer in &mut passthrough {
            buffer.set_header(header.clone());
        }
        aggregated.retain(|buffer| !buffer.is_empty());
        (aggregated, passthrough)
    }
}

/// The hand-written post-aggregation baseline: the filter, the tap's copy, enrichment, and the fan-out.
struct PostAggregationBaseline {
    enrichment: Box<dyn Kernel>,
    host_tags: Box<dyn Kernel>,
    post_aggregate: Arc<CompiledPredicate>,
    counters: TurnCounters,
}

impl PostAggregationBaseline {
    /// The gateways the fan-out copies to, besides the primary output.
    const GATEWAYS: usize = 4;

    fn new(compiled: &CompiledPlan, frame: &Frame) -> Self {
        let filter_program = &compiled.programs[slot_named(frame, "dsd_post_agg_filter_region").get()];
        let enrich_program = &compiled.programs[slot_named(frame, "metrics_enrich").get()];
        Self {
            enrichment: kernel_of(enrich_program, "metrics_enrich"),
            host_tags: kernel_of(enrich_program, "metrics_enrich_host_tags"),
            post_aggregate: split_of(filter_program),
            counters: TurnCounters::default(),
        }
    }

    /// Runs one batch, returning what leaves for the tap, the gateways, and the primary output.
    fn run(&mut self, mut batch: EventsBuffer, now: Instant) -> Vec<EventsBuffer> {
        let header = batch.take_header();
        filter(&mut batch, &self.post_aggregate);
        let mut tap = batch.clone();
        tap.set_header(header.clone());
        let mut unused = PortOutputs::new(1);
        in_place(
            &mut self.enrichment,
            &header,
            now,
            &mut unused,
            &mut self.counters,
            &mut batch,
        );
        in_place(
            &mut self.host_tags,
            &header,
            now,
            &mut unused,
            &mut self.counters,
            &mut batch,
        );
        let mut delivered = vec![tap];
        for _ in 0..Self::GATEWAYS {
            let mut copy = batch.clone();
            copy.set_header(header.clone());
            delivered.push(copy);
        }
        batch.set_header(header);
        delivered.push(batch);
        delivered
    }
}

/// Returns the names, tags, and point counts of the events in some buffers, sorted, for comparing deliveries.
fn delivered<'a>(buffers: impl IntoIterator<Item = &'a EventsBuffer>) -> Vec<(String, usize)> {
    let mut delivered = buffers
        .into_iter()
        .flat_map(|buffer| buffer.into_iter())
        .map(|event| {
            let metric = event.try_as_metric().unwrap();
            (metric.context().to_string(), metric.values().len())
        })
        .collect::<Vec<_>>();
    delivered.sort();
    delivered
}

fn bench_pre_aggregation(c: &mut Criterion) {
    // Kernels with caches need a Tokio runtime to be built in.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();

    let (frame, compiled) = default_program();
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let slots = [
        slot_named(&frame, "dsd_pre_aggregation"),
        slot_named(&frame, "dsd_prefix_filter_region"),
        slot_named(&frame, "dsd_no_agg_split_region"),
    ];
    let aggregation = FrameTarget::Slot(slot_named(&frame, "dsd_agg"));
    let merge = FrameTarget::Slot(slot_named(&frame, "dsd_post_agg_filter_region"));
    let now = Instant::now();

    let mut group = c.benchmark_group("pre_aggregation_pipeline");
    for (size, contexts) in [(1024, 256), (1024, 4096), (128, 256)] {
        let mut pipeline = Pipeline::new(&frame, &compiled, &generation, &slots);
        let mut baseline = PreAggregationBaseline::new(&compiled, &frame);
        let batch = dogstatsd_input(header(&generation, 0), size, contexts);

        // Equal delivered work: the same events leave toward aggregation and toward the passthrough's merge.
        let written = pipeline.run(batch.clone(), now, &mut Budget::unlimited());
        let to = |target: &FrameTarget| {
            written
                .iter()
                .filter(|(to, _)| to == target)
                .map(|(_, buffer)| buffer)
                .collect::<Vec<_>>()
        };
        let (aggregated, passthrough) = baseline.run(batch.clone(), now);
        assert_eq!(delivered(to(&aggregation)), delivered(&aggregated));
        assert_eq!(delivered(to(&merge)), delivered(&passthrough));
        assert!(written.iter().all(|(to, _)| *to == aggregation || *to == merge));
        drop(written);

        let parameter = format!("{} metrics, {} contexts", size, contexts);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_function(BenchmarkId::new("region_drivers", &parameter), |b| {
            let mut budget = Budget::unlimited();
            b.iter_batched(
                || batch.clone(),
                |batch| pipeline.run(batch, now, &mut budget).len(),
                BatchSize::SmallInput,
            )
        });
        group.bench_function(BenchmarkId::new("hand_written_baseline", &parameter), |b| {
            b.iter_batched(
                || batch.clone(),
                |batch| {
                    let (aggregated, passthrough) = baseline.run(batch, now);
                    aggregated.len() + passthrough.len()
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn bench_post_aggregation(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();

    let (frame, compiled) = default_program();
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let slots = [
        slot_named(&frame, "dsd_post_agg_filter_region"),
        slot_named(&frame, "metrics_enrich"),
    ];
    let mut pipeline = Pipeline::new(&frame, &compiled, &generation, &slots);
    let mut baseline = PostAggregationBaseline::new(&compiled, &frame);
    // Batches from aggregation carry its isolation slot's output point, which stamps its time-driven output, and the
    // entry of the post-aggregation filter's region that its exit sets.
    let aggregation = slot_named(&frame, "dsd_agg");
    let SlotKind::Isolation { output_point, .. } = frame.slot(aggregation).unwrap().kind else {
        panic!("aggregation runs in an isolation slot");
    };
    let entry = compiled.programs[aggregation.get()]
        .entries()
        .iter()
        .filter(|entry| matches!(entry.key, EntryKey::StateOutput(_)))
        .flat_map(|entry| &entry.steps)
        .find_map(|step| match step {
            Step::Exit { entry, .. } => entry.region(),
            _ => None,
        })
        .expect("aggregation's output leads to the post-aggregation filter's region");
    let mut batch = aggregated_input(header(&generation, output_point.get()));
    let mut stamped = batch.take_header();
    stamped.entry = Some(entry);
    batch.set_header(stamped);
    let now = Instant::now();

    // Equal delivered work: the same events leave toward the tap, the gateways, and the primary output.
    let written = pipeline.run(batch.clone(), now, &mut Budget::unlimited());
    let expected = baseline.run(batch.clone(), now);
    assert_eq!(written.len(), 6, "the tap, four gateways, and the primary output");
    assert_eq!(
        delivered(written.iter().map(|(_, buffer)| buffer)),
        delivered(&expected)
    );
    drop(written);

    let mut group = c.benchmark_group("post_aggregation_pipeline");
    group.throughput(Throughput::Elements(1024));
    group.bench_function("region_drivers", |b| {
        let mut budget = Budget::unlimited();
        b.iter_batched(
            || batch.clone(),
            |batch| pipeline.run(batch, now, &mut budget).len(),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("hand_written_baseline", |b| {
        b.iter_batched(
            || batch.clone(),
            |batch| baseline.run(batch, now).len(),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

/// The default program's isolation slot `dsd_agg`: its driver, the generation and point
/// its batches carry, and its number of exits.
struct Aggregation {
    driver: MetricRegionDriver,
    generation: GenerationRef,
    point: AdmissionPointId,
    exits: usize,
    wall: Arc<BenchClock>,
}

/// A wall clock the benchmark sets.
#[derive(Debug, Default)]
struct BenchClock(std::sync::atomic::AtomicU64);

impl WallClock for BenchClock {
    fn now(&self) -> AdmissionTime {
        AdmissionTime::from_unix_nanos(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }
}

impl Aggregation {
    fn new(now: Instant) -> Self {
        let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
        assert!(report.is_valid(), "{}", report);
        let frame = Frame::compute(&plan, &registry, &namer).unwrap();
        let compiled = compile(&plan, &report, &registry, &frame).unwrap();
        let slot = frame.slot(slot_named(&frame, "dsd_agg")).unwrap().clone();
        let program = Arc::clone(&compiled.programs[slot.id.get()]);
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(&program, keys, KernelBuildContext::detached).unwrap();
        let boot = PrepareRegion::new(
            Arc::clone(&generation),
            program,
            kernels,
            PanicBudget::DEFAULT_PROGRAM,
            &NoTelemetry,
        );
        let wall = Arc::new(BenchClock::default());
        let config = RegionConfig::for_slot(&slot, &NoTelemetry)
            .with_schedule(AdmissionSchedule::Steady(Arc::clone(&generation)).into_handle())
            .with_wall_clock(Arc::clone(&wall) as Arc<dyn WallClock>);
        let mut driver = MetricRegionDriver::new(config, boot).unwrap();
        driver.start(&mut RegionTurn::new(now, &mut Budget::unlimited()));
        Self {
            driver,
            generation,
            // DogStatsD's point is the one upstream of aggregation.
            point: AdmissionPointId::new(0),
            exits: slot.exits.len(),
            wall,
        }
    }

    fn header(&self, at: AdmissionTime) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(self.point),
            epoch: 0,
            generation: Some(Arc::clone(&self.generation)),
            entry: Some(saluki_core::execution::EntryId::new(0)),
        }
    }

    /// Admits 100,000 contexts, a counter and a gauge per 2, at `at`.
    fn hold_contexts(&mut self, at: AdmissionTime, now: Instant) {
        for buffer in held_contexts(self.header(at)) {
            let mut sink = ExitSink::new(self.exits);
            self.driver
                .process(&mut RegionTurn::new(now, &mut Budget::unlimited()), buffer, &mut sink);
        }
    }

    /// Raises the watermark to `at` with a heartbeat, and runs the flush tick due at `due`, with the wall clock at
    /// `at`, in drain steps of the limits table's size; returns the events it emitted.
    fn flush(&mut self, at: AdmissionTime, due: Instant) -> usize {
        self.wall
            .0
            .store(at.as_unix_nanos(), std::sync::atomic::Ordering::Relaxed);
        let mut sink = ExitSink::new(self.exits);
        let heartbeat = EventsBuffer::with_capacity(0).with_header(self.header(at));
        self.driver.process(
            &mut RegionTurn::new(due, &mut Budget::unlimited()),
            heartbeat,
            &mut sink,
        );
        let step = || {
            Budget::unlimited()
                .with_limit(WorkKind::Events, saluki_metric_plan::limits::values::DRAIN_STEP_EVENTS)
                .with_limit(
                    WorkKind::StateWork,
                    saluki_metric_plan::limits::values::DRAIN_STEP_CONTEXTS,
                )
        };
        let mut emitted = 0;
        let mut budget = step();
        let mut progress = self
            .driver
            .advance_time(&mut RegionTurn::new(due, &mut budget), &mut sink);
        emitted += sink.event_count();
        sink.clear();
        while progress == Progress::Yielded {
            let mut budget = step();
            progress = self.driver.resume(&mut RegionTurn::new(due, &mut budget), &mut sink);
            emitted += sink.event_count();
            sink.clear();
        }
        emitted
    }
}

/// Returns 100,000 contexts, a counter and a gauge per 2, in batches of 1,000 with `header`.
fn held_contexts(header: BatchHeader) -> Vec<EventsBuffer> {
    (0..(100_000 / 1_000))
        .map(|chunk| {
            let mut buffer = EventsBuffer::default().with_header(header.clone());
            for index in 0..1_000 {
                let context = Context::from_parts(format!("held.{}.{}", chunk, index), TagSet::default());
                let metric = if index % 2 == 0 {
                    Metric::counter(context, 1.0)
                } else {
                    Metric::gauge(context, 1.0)
                };
                assert!(buffer.try_push(Event::Metric(metric)).is_none());
            }
            buffer
        })
        .collect()
}

/// The default program's aggregation kernel, built outside any region, as the baseline calls it.
fn aggregation_kernel() -> Box<dyn Kernel> {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let node = plan.node(&"dsd_agg".parse().unwrap()).unwrap();
    let operator = OperatorRegistry::native()
        .configure(node.op.as_str(), &node.params)
        .unwrap();
    operator
        .kernel()
        .unwrap()
        .build(&KernelBuildContext::detached(&node.id))
        .unwrap()
}

/// The aggregation kernel called directly, holding 100,000 contexts admitted with `header`.
struct HeldKernel {
    kernel: Box<dyn Kernel>,
    ports: PortOutputs,
    counters: TurnCounters,
}

impl HeldKernel {
    fn new(header: &BatchHeader, now: Instant) -> Self {
        let mut held = Self {
            kernel: aggregation_kernel(),
            ports: PortOutputs::new(1),
            counters: TurnCounters::default(),
        };
        for mut buffer in held_contexts(header.clone()) {
            in_place(
                &mut held.kernel,
                header,
                now,
                &mut held.ports,
                &mut held.counters,
                &mut buffer,
            );
        }
        held
    }

    /// Runs one flush at `at` in one call with no budget, as the kernel runs outside any region; returns the events it
    /// emitted.
    fn flush(&mut self, header: &BatchHeader, at: AdmissionTime, now: Instant) -> usize {
        let mut budget = Budget::unlimited();
        let progress = {
            let mut turn =
                Turn::new(header, now, &mut budget, &mut self.ports, &mut self.counters).with_watermark(Some(at));
            self.kernel.advance_time(&mut turn)
        };
        assert_eq!(
            progress,
            Progress::Done,
            "a flush without a budget completes in one call"
        );
        self.ports.drain(PortIndex::PRIMARY).map(|buffer| buffer.len()).sum()
    }
}

fn bench_aggregation(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let start = Instant::now();
    let admitted = AdmissionTime::from_unix_secs(1_700_000_003);
    let flushed = AdmissionTime::from_unix_secs(1_700_000_015);
    let due = start + std::time::Duration::from_secs(15);
    let mut group = c.benchmark_group("aggregation_isolation_slot");

    // Admitting 1,024 metrics over 256 contexts, through the slot's state step and directly into the kernel.
    let mut aggregation = Aggregation::new(start);
    let batch = aggregated_input(aggregation.header(admitted));
    let mut kernel = aggregation_kernel();
    group.throughput(Throughput::Elements(1024));
    group.bench_function("admit_1024/region_driver", |b| {
        b.iter_batched(
            || batch.clone(),
            |batch| {
                let mut sink = ExitSink::new(aggregation.exits);
                aggregation
                    .driver
                    .process(&mut RegionTurn::new(start, &mut Budget::unlimited()), batch, &mut sink)
            },
            BatchSize::SmallInput,
        )
    });
    group.bench_function("admit_1024/kernel", |b| {
        let header = aggregation.header(admitted);
        let mut ports = PortOutputs::new(1);
        let mut counters = TurnCounters::default();
        b.iter_batched(
            || batch.clone(),
            |mut batch| in_place(&mut kernel, &header, start, &mut ports, &mut counters, &mut batch),
            BatchSize::SmallInput,
        )
    });

    // A flush tick over 100,000 contexts: the drain runs in bounded steps; every bucket closes, and every gauge leaves.
    // The kernel called directly flushes them all in one call.
    let mut check = Aggregation::new(start);
    check.hold_contexts(admitted, start);
    assert_eq!(
        check.flush(flushed, due),
        100_000,
        "every context's bucket is emitted once"
    );
    let held_header = check.header(admitted);
    assert_eq!(
        HeldKernel::new(&held_header, start).flush(&held_header, flushed, due),
        100_000,
        "the kernel emits what the region does"
    );
    group.throughput(Throughput::Elements(100_000));
    group.sample_size(10);
    group.bench_function("flush_100000_contexts/region_driver", |b| {
        b.iter_batched(
            || {
                let mut aggregation = Aggregation::new(start);
                aggregation.hold_contexts(admitted, start);
                aggregation
            },
            |mut aggregation| aggregation.flush(flushed, due),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("flush_100000_contexts/kernel", |b| {
        b.iter_batched(
            || HeldKernel::new(&held_header, start),
            |mut held| held.flush(&held_header, flushed, due),
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_pre_aggregation,
    bench_post_aggregation,
    bench_aggregation
);
criterion_main!(benches);
