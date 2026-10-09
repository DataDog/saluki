//! Frames of region drivers, wired together in process, deliver what the reference interpreter delivers.
//!
//! [`FrameHarness`] runs every slot of a frame through a [`MetricRegionDriver`] and every fixed component through the
//! interpreter's opaque-host adapters, synchronously and without channels: a batch admitted at a source is stamped as
//! its admission point stamps it, runs through every region and host it reaches, and what reaches an output is
//! recorded. A host's output is re-admitted at its own admission point, with the generation the schedule holds then.
//! Generations are installed as the installer installs them: every region prepares the candidate's program, with the
//! kernels the installer would build, and then the commit replaces the generation that admission points stamp.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, ScalarPoints},
        Event,
    },
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, ExitSink, GenerationId, GenerationRef, GenerationToken,
        LossClass, PortIndex, Progress, RegionDriver, RegionTurn,
    },
    topology::EventsBuffer,
};
use saluki_metrics::Counter;
use serde_json::{json, Value};

use super::*;
use crate::{
    canonical::PlanHash,
    compile::{compile, CompiledPlan, Frame, FrameTarget, Placement, RegionProgram, Site},
    equivalence::{compare, OutputPoints},
    explain::loss_class_name,
    install::kernel_reuse,
    interpret::{
        adapters::AggregateAdapter, AdapterError, HostBatch, HostOutput, HostTick, Interpreter, OpaqueHostAdapter,
        Trace,
    },
    ir::{NativeComponentId, NodeId, OutputId, Plan, PortName, RecipientId},
    model::{Kind, PortType},
    operators::{names, ConfiguredOperator, KernelBuildContext, OperatorRegistry},
    predicate::Predicate,
    testing::{
        arb_metric, arb_predicate, default_program_catalog, predicate_json, DEFAULT_PROGRAM_JSON,
        NO_AGGREGATION_PIPELINE_OFF_JSON, PREFIX_VOCABULARY, TAG_KEY_VOCABULARY, TAG_VALUE_VOCABULARY,
    },
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
};

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn out(value: &str) -> OutputId {
    OutputId::new(value).unwrap()
}

/// Counters by component and tag.
type Counters = BTreeMap<(String, String), Arc<AtomicU64>>;

/// Telemetry that records into counters the test can read, keyed by component and tag.
#[derive(Clone, Default)]
pub(super) struct RecordingTelemetry {
    counters: Arc<Mutex<Counters>>,
}

impl RecordingTelemetry {
    fn counter(&self, component: String, tag: String) -> Counter {
        let mut counters = self.counters.lock().unwrap();
        Counter::from_arc(Arc::clone(counters.entry((component, tag)).or_default()))
    }

    /// Returns the value of the counter of `component` with `tag` (`port:<port>` or `class:<class>`), or 0 if nothing
    /// registered it.
    pub fn value(&self, component: &str, tag: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(&(component.to_string(), tag.to_string()))
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    }

    /// Returns the points `component` lost in `class`.
    pub fn loss(&self, component: &str, class: LossClass) -> u64 {
        self.value(component, &format!("class:{}", loss_class_name(class)))
    }

    /// Returns the events `node` sent on `port`.
    pub fn port_events(&self, node: &str, port: &str) -> u64 {
        self.value(node, &format!("port:{}", port))
    }
}

impl RegisterTelemetry for RecordingTelemetry {
    fn port_events(&self, node: &NodeId, _operator: &'static str, port: &PortName) -> Vec<Counter> {
        vec![self.counter(node.to_string(), format!("port:{}", port))]
    }

    fn loss(&self, component_id: &str, class: LossClass) -> Counter {
        self.counter(component_id.to_string(), format!("class:{}", loss_class_name(class)))
    }
}

/// An opaque host that forwards everything it admits unchanged, as an opaque native filter that keeps everything
/// would.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(PortIndex::PRIMARY, metric);
        }
    }
}

fn echo(_operator: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(Box::new(EchoHost))
}

/// Builds the adapter of a fixed component: native aggregation for `aggregate`, and [`EchoHost`] for opaque hosts.
fn host_for(operator: &ConfiguredOperator) -> Box<dyn OpaqueHostAdapter> {
    if operator.name() == names::AGGREGATE {
        Box::new(AggregateAdapter::from_operator(operator).unwrap())
    } else {
        Box::new(EchoHost)
    }
}

/// Names a slot after the first of its boot nodes.
fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

fn buffer_of(header: BatchHeader, metrics: Vec<Metric>) -> EventsBuffer {
    let mut buffer = EventsBuffer::with_capacity(metrics.len()).with_header(header);
    for metric in metrics {
        assert!(
            buffer.try_push(Event::Metric(metric)).is_none(),
            "test batches fit one buffer"
        );
    }
    buffer
}

/// A frame whose slots run through region drivers, wired together in process.
pub(super) struct FrameHarness {
    registry: OperatorRegistry,
    frame: Frame,
    placement: Placement,
    programs: Vec<Arc<RegionProgram>>,
    drivers: Vec<MetricRegionDriver>,
    hosts: BTreeMap<NodeId, (Box<dyn OpaqueHostAdapter>, usize)>,
    stamps: BTreeMap<AdmissionPointId, AdmissionTime>,
    active: GenerationRef,
    prepared: Option<(GenerationRef, CompiledPlan)>,
    next_generation: u64,
    last_tick: Option<AdmissionTime>,
    outputs: OutputPoints,
    telemetry: RecordingTelemetry,
    /// Whether buffers one slot sends another are held, as a queue between two regions holds them, rather than
    /// delivered at once.
    holding: bool,
    held: VecDeque<(FrameTarget, EventsBuffer, AdmissionTime)>,
    /// The generation hosts re-admit with while held buffers are delivered: the held batch's own.
    readmit_as: Option<GenerationRef>,
}

impl FrameHarness {
    /// Computes the frame of a valid boot plan, and creates a driver for each slot, prepared for generation 1.
    pub fn new(plan: &Plan, report: &ValidationReport, registry: OperatorRegistry) -> Self {
        let frame = Frame::compute(plan, &registry, &namer).unwrap();
        let compiled = compile(plan, report, &registry, &frame).unwrap();
        let active: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let telemetry = RecordingTelemetry::default();
        let drivers = frame
            .slots()
            .iter()
            .zip(&compiled.programs)
            .map(|(slot, program)| {
                let keys = program.kernels().iter().map(|kernel| &kernel.key);
                let kernels = build_kernels(program, keys, KernelBuildContext::detached).unwrap();
                let boot = PrepareRegion::new(
                    Arc::clone(&active),
                    Arc::clone(program),
                    kernels,
                    PanicBudget::DEFAULT_PROGRAM,
                    &telemetry,
                );
                MetricRegionDriver::new(RegionConfig::for_slot(slot, &telemetry), boot).unwrap()
            })
            .collect();
        let hosts = frame
            .fixed()
            .iter()
            .filter(|fixed| fixed.operator.as_str() != names::SOURCE)
            .map(|fixed| {
                let operator = registry.configure(fixed.operator.as_str(), &fixed.params).unwrap();
                let ports = operator.ports().outputs.len();
                (fixed.node.clone(), (host_for(&operator), ports))
            })
            .collect();
        Self {
            registry,
            frame,
            placement: compiled.placement,
            programs: compiled.programs,
            drivers,
            hosts,
            stamps: BTreeMap::new(),
            active,
            prepared: None,
            next_generation: 2,
            last_tick: None,
            outputs: OutputPoints::new(),
            telemetry,
            holding: false,
            held: VecDeque::new(),
            readmit_as: None,
        }
    }

    /// Holds every buffer one slot sends another from now on, until [`stop_holding`][Self::stop_holding]: what is in
    /// flight between two regions when a generation commits.
    pub fn hold_between_slots(&mut self) {
        self.holding = true;
    }

    /// Stops holding buffers between slots; what is held stays held until [`release`][Self::release].
    pub fn stop_holding(&mut self) {
        self.holding = false;
    }

    /// Delivers the buffers held between slots, in the order they were sent.
    ///
    /// A held batch runs whole in its own generation, as the reference interpreter runs it: a host it reaches
    /// re-admits it with that generation, as if the host had drained its queue before the commit. What the hold
    /// changes is when the regions below run it: after a later generation committed, and after later batches.
    pub fn release(&mut self) {
        while let Some((target, buffer, arrival)) = self.held.pop_front() {
            self.readmit_as = buffer.header().generation.clone();
            self.deliver(Some(target), buffer, arrival);
            self.readmit_as = None;
        }
    }

    /// Returns the number of buffers held between slots.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Prepares a candidate plan in every region, reusing the kernels the active programs have, as the installer
    /// does before its barrier. Admission points keep stamping the active generation until [`commit`][Self::commit].
    pub fn prepare(&mut self, plan: &Plan, report: &ValidationReport) {
        let compiled = compile(plan, report, &self.registry, &self.frame).unwrap();
        let reuse = kernel_reuse(Some(&self.programs), &compiled.programs);
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(self.next_generation)));
        self.next_generation += 1;
        for ((driver, program), kernels) in self.drivers.iter_mut().zip(&compiled.programs).zip(&reuse) {
            let built = build_kernels(program, &kernels.build, KernelBuildContext::detached).unwrap();
            let prepare = PrepareRegion::new(
                Arc::clone(&generation),
                Arc::clone(program),
                built,
                PanicBudget::DEFAULT_PROGRAM,
                &self.telemetry,
            );
            let mut budget = Budget::unlimited();
            let mut sink = ExitSink::new(0);
            driver
                .control(
                    &mut RegionTurn::new(Instant::now(), &mut budget),
                    RegionControl::Prepare(Box::new(prepare)),
                    &mut sink,
                )
                .unwrap();
        }
        self.prepared = Some((generation, compiled));
    }

    /// Commits the prepared generation: admission points stamp it from now on, and nothing in the harness holds the
    /// one it replaces any more.
    pub fn commit(&mut self) {
        let (generation, compiled) = self.prepared.take().expect("a generation is prepared");
        self.active = generation;
        self.programs = compiled.programs;
        self.placement = compiled.placement;
    }

    /// Admits a batch at a source's `metrics` output.
    pub fn admit(&mut self, source: &str, at: AdmissionTime, metrics: Vec<Metric>) {
        let point = self
            .frame
            .admission_point_at(&id(source), &"metrics".parse().unwrap())
            .unwrap()
            .clone();
        let header = BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(point.id),
            epoch: 0,
            generation: Some(Arc::clone(&self.active)),
            entry: None,
        };
        let arrival = self.last_tick.map_or(at, |tick| tick.max(at));
        self.deliver(point.route.clone(), buffer_of(header, metrics), arrival);
    }

    /// Runs a flush tick of every host.
    pub fn advance(&mut self, now: AdmissionTime) {
        self.last_tick = Some(now);
        let nodes = self.hosts.keys().cloned().collect::<Vec<_>>();
        for node in nodes {
            let (host, ports) = self.hosts.get_mut(&node).unwrap();
            let mut output = HostOutput::new(*ports);
            host.tick(HostTick { now, watermark: None }, &mut output);
            self.readmit(&node, output, now);
        }
    }

    /// Publishes every region's telemetry.
    pub fn publish(&mut self) {
        for driver in &mut self.drivers {
            driver.publish_telemetry();
        }
    }

    /// Re-admits what a host emitted, at its output's admission point, with the host's clock, which never moves back.
    fn readmit(&mut self, node: &NodeId, mut output: HostOutput, now: AdmissionTime) {
        let (ports, _) = output.take();
        let fixed = self.frame.fixed_component(node).unwrap().clone();
        for (port, metrics) in ports.into_iter().enumerate() {
            if metrics.is_empty() {
                continue;
            }
            let point = self
                .frame
                .admission_point(fixed.admission_points[port])
                .unwrap()
                .clone();
            let stamped = self.stamps.get(&point.id).map_or(now, |previous| (*previous).max(now));
            self.stamps.insert(point.id, stamped);
            let generation = self.readmit_as.clone().unwrap_or_else(|| Arc::clone(&self.active));
            let header = BatchHeader {
                admitted_at: Some(stamped),
                admission_point: Some(point.id),
                epoch: 0,
                generation: Some(generation),
                entry: None,
            };
            self.deliver(point.route.clone(), buffer_of(header, metrics), now);
        }
    }

    fn deliver(&mut self, target: Option<FrameTarget>, buffer: EventsBuffer, arrival: AdmissionTime) {
        let mut queue = VecDeque::from([(target, buffer)]);
        while let Some((target, buffer)) = queue.pop_front() {
            match target {
                None => {}
                Some(FrameTarget::Output(output)) => {
                    for event in buffer {
                        self.outputs.insert(&output, &event.try_into_metric().unwrap());
                    }
                }
                Some(FrameTarget::Slot(slot)) => {
                    let exits = &self.frame.slot(slot).unwrap().exits;
                    let mut sink = ExitSink::new(exits.len());
                    let mut budget = Budget::unlimited();
                    let progress = self.drivers[slot.get()].process(
                        &mut RegionTurn::new(Instant::now(), &mut budget),
                        buffer,
                        &mut sink,
                    );
                    assert_eq!(progress, Progress::Done);
                    for (exit, buffer) in sink.drain() {
                        let target = exits[exit.get()].target.clone();
                        // Every buffer leaves stamped, with an entry exactly when it goes to another slot.
                        assert!(buffer.header().generation.is_some(), "exit buffers keep the generation");
                        assert_eq!(
                            buffer.header().entry.is_some(),
                            matches!(target, FrameTarget::Slot(_)),
                            "exit to {:?} has entry {:?}",
                            target,
                            buffer.header().entry
                        );
                        if self.holding && matches!(target, FrameTarget::Slot(_)) {
                            self.held.push_back((target, buffer, arrival));
                        } else {
                            queue.push_back((Some(target), buffer));
                        }
                    }
                }
                Some(FrameTarget::Fixed(node)) => {
                    let admitted_at = buffer.header().admitted_at;
                    let events = buffer.into_iter().filter_map(Event::try_into_metric).collect();
                    let (host, ports) = self.hosts.get_mut(&node).expect("every fixed target has a host");
                    let mut output = HostOutput::new(*ports);
                    host.admit(
                        HostBatch {
                            admitted_at,
                            arrived_at: arrival,
                            events,
                        },
                        &mut output,
                    );
                    self.readmit(&node, output, arrival);
                }
            }
        }
    }

    /// Returns what every output received.
    pub fn outputs(&self) -> &OutputPoints {
        &self.outputs
    }

    /// Returns the recorded telemetry.
    pub fn telemetry(&self) -> &RecordingTelemetry {
        &self.telemetry
    }

    /// Returns the `drop` nodes that run in a slot, whose loss a region counts.
    pub fn drops_in_slots(&self, plan: &Plan) -> Vec<NodeId> {
        plan.nodes
            .iter()
            .filter(|node| node.op.as_str() == names::DROP)
            .filter(|node| matches!(self.placement.site(&node.id), Some(Site::Slot(_))))
            .map(|node| node.id.clone())
            .collect()
    }

    /// Returns the drivers.
    pub fn drivers(&self) -> &[MetricRegionDriver] {
        &self.drivers
    }
}

/// Checks that the harness's outputs and the policy loss its regions counted equal the interpreter's.
fn assert_matches_interpreter(harness: &mut FrameHarness, plan: &Plan, run: &crate::interpret::RunOutput) {
    harness.publish();
    if let Err(mismatch) = compare(&OutputPoints::from_run(run), harness.outputs()) {
        panic!("region drivers differ from the interpreter:\n{}", mismatch);
    }
    for node in harness.drops_in_slots(plan) {
        assert_eq!(
            harness.telemetry().loss(&node.to_string(), LossClass::Policy),
            run.loss(&node).loss(LossClass::Policy).points,
            "policy loss of `{}`",
            node
        );
    }
}

// The default program.

/// The base time, a multiple of the 10-second window.
const B: u64 = 1_700_000_000;

fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

fn counter_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), (B + relative, value))
}

#[test]
fn the_default_program_runs_through_a_frame_of_region_drivers_as_the_interpreter_runs_it() {
    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let report = Validator::new(&registry, &catalog).validate(&plan);

        // The trace: DogStatsD and OTLP batches, a flush tick, and batches after the host tags' deadline. Without the
        // no-aggregation pipeline, DogStatsD parses no timestamps, so its timestamped points are left out.
        let mut dogstatsd = vec![
            Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
            Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
            Metric::distribution(
                Context::from_static_parts("checkout.latency", &["user_id:42", "env:dev"]),
                10.0,
            ),
            Metric::gauge(Context::from_static_name("internal.queue"), 7.0),
            Metric::histogram(
                Context::from_static_parts("checkout.latency", &["env:prod"]),
                [1.0, 2.0, 3.0, 4.0, 5.0],
            ),
        ];
        let late_dogstatsd = if timestamped {
            dogstatsd.push(counter_at("checkout.requests", 1, 2.0));
            dogstatsd.push(Metric::gauge(
                Context::from_static_name("system.load"),
                ScalarPoints::from_iter([(None, 1.0), (std::num::NonZeroU64::new(B + 2), 2.0)]),
            ));
            gauge_at("system.disk", 302, 9.0)
        } else {
            Metric::gauge(Context::from_static_name("system.disk"), 9.0)
        };
        let otlp = vec![
            gauge_at("catalog.items", 4, 3.0),
            counter_at("requests.count", 4, 4.0),
            counter_at("checkout.requests", 4, 1.0),
        ];
        let trace = Trace::new()
            .install(plan.clone())
            .admit(id("dsd_in"), t(3), dogstatsd.clone())
            .admit(id("otlp_in"), t(4), otlp.clone())
            .advance(t(15))
            .admit(id("otlp_in"), t(301), vec![gauge_at("checkout.cpu", 301, 0.5)])
            .admit(id("dsd_in"), t(302), vec![late_dogstatsd.clone()]);
        let run = Interpreter::new(&registry, &catalog).run(&trace).unwrap();

        // Kernels with caches need a Tokio runtime to be built in, as the interpreter's are.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut harness = FrameHarness::new(&plan, &report, registry);
        // R1's cost model cuts the default program into eight slots: three before aggregation, the post-aggregation
        // filter's, enrichment with the Multi-Region Failover gateway, and one for each other gateway.
        assert_eq!(
            harness.drivers().len(),
            8,
            "the default program's frame has eight slots"
        );
        harness.admit("dsd_in", t(3), dogstatsd);
        harness.admit("otlp_in", t(4), otlp);
        harness.advance(t(15));
        harness.admit("otlp_in", t(301), vec![gauge_at("checkout.cpu", 301, 0.5)]);
        harness.admit("dsd_in", t(302), vec![late_dogstatsd]);

        assert!(
            OutputPoints::from_run(&run).count(&out("dd_metrics_encode")) > 10,
            "the trace delivers"
        );
        assert_matches_interpreter(&mut harness, &plan, &run);
        // The blocklist drops exactly `tenant.raw.blocked`, so its split counts one event on `true` and the rest of
        // the first batch, after the mapper and prefix, on `false`.
        let telemetry = harness.telemetry();
        assert_eq!(telemetry.port_events("dsd_prefix_filter_blocklist", "true"), 1);
        assert_eq!(
            telemetry.port_events("dsd_prefix_filter_blocklist", "false"),
            if timestamped { 6 + 1 } else { 5 },
            "every other DogStatsD event passes the blocklist"
        );
        assert_eq!(telemetry.loss("dsd_prefix_filter_blocklist_true", LossClass::Policy), 1);
    }
}

// Random plans.

/// The stateless kernels random plans use.
#[derive(Clone, Copy, Debug)]
enum KernelKind {
    NamespacePrefix,
    TagProjection,
    HostEnrichment,
    HostTags,
    Mapper,
}

const KERNEL_KINDS: [KernelKind; 5] = [
    KernelKind::NamespacePrefix,
    KernelKind::TagProjection,
    KernelKind::HostEnrichment,
    KernelKind::HostTags,
    KernelKind::Mapper,
];

/// The shape of a random plan below its sources' merge.
#[derive(Clone, Debug)]
enum Shape {
    Emit,
    Drop,
    Split(Box<Shape>, Box<Shape>),
    Tee(Vec<Shape>),
    /// A split whose ports join again in a merge, followed by the rest.
    Rejoin(Box<Shape>),
    /// A split whose `true` port goes through an opaque host into a merge that its `false` port joins directly: the
    /// merge is one fence deeper than the split, so they run in different slots, joined by an exit.
    Detour(Box<Shape>),
    /// A split whose `true` port goes through an opaque host into a copy to two merges, `m`, which emits to an output
    /// of its own, and `k`, followed by the rest; its `false` port leads to a second split, whose ports join `m` and
    /// `k`. Both merges are one fence deeper than the splits, so the second split's two edges reach two nodes of the
    /// slot below over one exit, as two entries.
    DoubleDetour(Box<Shape>),
    /// An opaque host, followed by the rest.
    Fence(Box<Shape>),
    /// A stateless kernel, followed by the rest.
    Kernel(KernelKind, Box<Shape>),
    /// A timestamp split, with a shape on each of its ports.
    TimestampSplit(Box<Shape>, Box<Shape>),
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![3 => Just(Shape::Emit), 1 => Just(Shape::Drop)];
    leaf.prop_recursive(8, 64, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(yes, no)| Shape::Split(Box::new(yes), Box::new(no))),
            vec(inner.clone(), 1..4).prop_map(Shape::Tee),
            inner.clone().prop_map(|rest| Shape::Rejoin(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::Detour(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::DoubleDetour(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::Fence(Box::new(rest))),
            (select(KERNEL_KINDS.to_vec()), inner.clone()).prop_map(|(kind, rest)| Shape::Kernel(kind, Box::new(rest))),
            (inner.clone(), inner)
                .prop_map(|(default, passthrough)| Shape::TimestampSplit(Box::new(default), Box::new(passthrough))),
        ]
    })
}

/// The parameters of one node of a random plan; each kind of node reads the ones it needs.
#[derive(Clone, Debug)]
struct NodeParams {
    predicate: Predicate,
    namespace: &'static str,
    exclusions: Vec<&'static str>,
    tag_rules: Vec<(&'static str, bool, Vec<&'static str>)>,
    value_rule: Option<(&'static str, &'static str, Vec<&'static str>, bool)>,
    cache: bool,
    deadline: u64,
}

fn arb_node_params() -> impl Strategy<Value = NodeParams> {
    let names = [
        "app.requests",
        "app.errors",
        "my_metric_name",
        "demo.debug.trace",
        "ns.app.requests",
    ];
    (
        arb_predicate(),
        select(vec!["", "ns", "tenant."]),
        vec(select(PREFIX_VOCABULARY), 0..3),
        vec(
            (
                select(names.to_vec()),
                any::<bool>(),
                vec(select(TAG_KEY_VOCABULARY), 0..3),
            ),
            0..3,
        ),
        // A value rule needs a non-empty prefix and tag name.
        proptest::option::of((
            select(vec!["app.", "my"]),
            select(vec!["env", "team", "region"]),
            vec(select(TAG_VALUE_VOCABULARY), 0..3),
            any::<bool>(),
        )),
        any::<bool>(),
        1_000u64..1_010,
    )
        .prop_map(
            |(predicate, namespace, exclusions, tag_rules, value_rule, cache, deadline)| NodeParams {
                predicate,
                namespace,
                exclusions,
                tag_rules,
                value_rule,
                cache,
                deadline,
            },
        )
}

/// The parameters of every node of two plans of one shape: the second plan keeps each node's parameters from the
/// first unless `changed` says otherwise for its position, so that most of its kernels are reused, and swaps the
/// targets of each split whose position `swapped` says, so that its programs route, and enter regions, differently.
#[derive(Clone, Debug)]
struct PlanParams {
    first: Vec<NodeParams>,
    second: Vec<NodeParams>,
    changed: Vec<bool>,
    swapped: Vec<bool>,
}

impl PlanParams {
    fn get(&self, plan: usize, node: usize) -> &NodeParams {
        let position = node % self.first.len();
        if plan == 1 && self.changed[position] {
            &self.second[position]
        } else {
            &self.first[position]
        }
    }

    /// Returns the ports of a split at `node` in plan `plan`, as `(the port leading to the first branch, the port
    /// leading to the second)`.
    fn split_ports(&self, plan: usize, node: usize) -> (&'static str, &'static str) {
        if plan == 1 && self.swapped[node % self.swapped.len()] {
            ("false", "true")
        } else {
            ("true", "false")
        }
    }
}

fn arb_plan_params() -> impl Strategy<Value = PlanParams> {
    (
        vec(arb_node_params(), 8),
        vec(arb_node_params(), 8),
        vec(any::<bool>(), 8),
        vec(proptest::bool::weighted(0.25), 8),
    )
        .prop_map(|(first, second, changed, swapped)| PlanParams {
            first,
            second,
            changed,
            swapped,
        })
}

/// A node with native provenance of its own, so that lossy and host nodes have authority, and so that R1's cost model
/// may cut between any two nodes, as it cuts between native components.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn kernel_node(id: &str, kind: KernelKind, params: &NodeParams) -> Value {
    match kind {
        KernelKind::NamespacePrefix => node(
            id,
            names::NAMESPACE_PREFIX,
            json!({"namespace": params.namespace, "exclusions": params.exclusions}),
        ),
        KernelKind::TagProjection => {
            let rules = params
                .tag_rules
                .iter()
                .map(|(name, include, tags)| {
                    json!({"metric_name": name, "action": if *include { "include" } else { "exclude" }, "tags": tags})
                })
                .collect::<Vec<_>>();
            let value_rules = params
                .value_rule
                .iter()
                .map(|(prefix, tag, values, replace)| {
                    if *replace {
                        json!({"metric_prefix": prefix, "tag_name": tag, "values": values, "on_miss": "replace",
                               "replacement": "other"})
                    } else {
                        json!({"metric_prefix": prefix, "tag_name": tag, "values": values, "on_miss": "remove"})
                    }
                })
                .collect::<Vec<_>>();
            node(
                id,
                names::TAG_PROJECTION,
                json!({
                    "rules": rules,
                    "value_rules": value_rules,
                    "cache_capacity": if params.cache { 16 } else { 0 },
                }),
            )
        }
        KernelKind::HostEnrichment => node(id, names::HOST_ENRICHMENT, json!({})),
        KernelKind::HostTags => node(
            id,
            names::HOST_TAGS,
            json!({"tags": ["host_tag:a", "zone:b"], "deadline": format!("{}s", params.deadline)}),
        ),
        KernelKind::Mapper => node(
            id,
            names::DOGSTATSD_MAPPER,
            json!({
                "profiles": [{"name": "test", "prefix": "app.", "mappings": [{
                    "match": "app.*", "match_type": "wildcard", "name": "mapped",
                    "tags": [{"name": "tail", "value": "$1"}]
                }]}],
                "cache_size": if params.cache { 100 } else { 0 },
                "string_interner_bytes": 65_536
            }),
        ),
    }
}

fn opaque(id: &str) -> Value {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    node(
        id,
        names::OPAQUE_HOST,
        json!({"component": id, "input": any, "outputs": {"out": any}}),
    )
}

/// Lowers a shape below the port `from` into nodes and edges, numbering nodes and outputs as it goes.
struct Builder<'p> {
    params: &'p PlanParams,
    plan: usize,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    outputs: Vec<String>,
    opaque: Vec<String>,
}

impl Builder<'_> {
    fn connect(&mut self, from: &str, to: String) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn build(&mut self, shape: &Shape, from: &str) {
        let position = self.nodes.len();
        let id = format!("n{}", position);
        let params = self.params.get(self.plan, position);
        let split = |id: &str| {
            node(
                id,
                names::SPLIT,
                json!({"predicate": predicate_json(&params.predicate)}),
            )
        };
        match shape {
            Shape::Emit => {
                let output = format!("o{}", self.outputs.len());
                self.nodes.push(node(&id, names::EMIT, json!({"output": output})));
                self.outputs.push(output);
                self.connect(from, format!("{}.in", id));
            }
            Shape::Drop => {
                self.nodes.push(node(&id, names::DROP, json!({})));
                self.connect(from, format!("{}.in", id));
            }
            Shape::Split(yes, no) => {
                let (first, second) = self.params.split_ports(self.plan, position);
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                self.build(yes, &format!("{}.{}", id, first));
                self.build(no, &format!("{}.{}", id, second));
            }
            Shape::Tee(branches) => {
                self.nodes.push(node(&id, names::TEE, json!({"ports": branches.len()})));
                self.connect(from, format!("{}.in", id));
                for (port, branch) in branches.iter().enumerate() {
                    self.build(branch, &format!("{}.{}", id, port));
                }
            }
            Shape::Rejoin(rest) => {
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                let merge = format!("{}_m", id);
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::Detour(rest) => {
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                let host = format!("{}_h", id);
                let merge = format!("{}_m", id);
                self.nodes.push(opaque(&host));
                self.opaque.push(host.clone());
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.in", host));
                self.connect(&format!("{}.out", host), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::DoubleDetour(rest) => {
                let host = format!("{}_h", id);
                let copies = format!("{}_t", id);
                let merge = format!("{}_m", id);
                let merged = format!("{}_m_out", id);
                let second = format!("{}_s", id);
                let last = format!("{}_k", id);
                let output = format!("o{}", self.outputs.len());
                self.outputs.push(output.clone());
                self.nodes.push(split(&id));
                self.nodes.push(opaque(&host));
                self.opaque.push(host.clone());
                self.nodes.push(node(&copies, names::TEE, json!({"ports": 2})));
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.nodes.push(node(&merged, names::EMIT, json!({"output": output})));
                self.nodes.push(split(&second));
                self.nodes.push(node(&last, names::MERGE, json!({"inputs": 2})));
                self.connect(from, format!("{}.in", id));
                self.connect(&format!("{}.true", id), format!("{}.in", host));
                self.connect(&format!("{}.out", host), format!("{}.in", copies));
                self.connect(&format!("{}.0", copies), format!("{}.0", merge));
                self.connect(&format!("{}.1", copies), format!("{}.0", last));
                self.connect(&format!("{}.out", merge), format!("{}.in", merged));
                self.connect(&format!("{}.false", id), format!("{}.in", second));
                // The second plan may swap which merge each port of the second split reaches, listing the edges in
                // that order, so that the same entry of the slot below, numbered in plan edge order, leads to the
                // other merge, and to the other output, in each generation.
                let (to_merge, to_last) = self.params.split_ports(self.plan, position);
                let mut edges = [
                    (format!("{}.{}", second, to_merge), format!("{}.1", merge)),
                    (format!("{}.{}", second, to_last), format!("{}.1", last)),
                ];
                if to_merge == "false" {
                    edges.swap(0, 1);
                }
                for (from, to) in edges {
                    self.connect(&from, to);
                }
                self.build(rest, &format!("{}.out", last));
            }
            Shape::Fence(rest) => {
                self.nodes.push(opaque(&id));
                self.opaque.push(id.clone());
                self.connect(from, format!("{}.in", id));
                self.build(rest, &format!("{}.out", id));
            }
            Shape::Kernel(kind, rest) => {
                self.nodes.push(kernel_node(&id, *kind, params));
                self.connect(from, format!("{}.in", id));
                self.build(rest, &format!("{}.out", id));
            }
            Shape::TimestampSplit(default, passthrough) => {
                self.nodes
                    .push(node(&id, names::TIMESTAMP_SPLIT, json!({"window": "10s"})));
                self.connect(from, format!("{}.in", id));
                self.build(default, &format!("{}.default", id));
                self.build(passthrough, &format!("{}.passthrough", id));
            }
        }
    }
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

/// The catalog random plans validate against: outputs `o0` to `o127`, each with its own recipient.
fn random_plan_catalog() -> MemoryCatalog {
    (0..128).fold(MemoryCatalog::new(), |catalog, index| {
        let output = format!("o{}", index);
        catalog.with_output(
            out(&output),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(&output),
                recipients: vec![RecipientId::new(format!("out/{}", output)).unwrap()],
            },
        )
    })
}

/// Builds plan `plan` (0 or 1) of a shape, fed by two sources that together admit every kind: DogStatsD, with client
/// timestamps, admits every kind but rates, and OTLP admits rates. Returns the plan and its opaque hosts.
fn random_plan(shape: &Shape, params: &PlanParams, plan: usize) -> (Plan, Vec<String>) {
    let mut builder = Builder {
        params,
        plan,
        nodes: vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            ),
            node("otlp", names::SOURCE, json!({"binding": "otlp"})),
            node("in", names::MERGE, json!({"inputs": 2})),
        ],
        edges: vec![
            json!({"from": "dsd.metrics", "to": "in.0"}),
            json!({"from": "otlp.metrics", "to": "in.1"}),
        ],
        outputs: Vec::new(),
        opaque: Vec::new(),
    };
    builder.build(shape, "in.out");
    let outputs = builder
        .outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    let plan =
        Plan::from_value(json!({"ir_version": 1, "nodes": builder.nodes, "edges": builder.edges, "outputs": outputs}))
            .unwrap();
    (plan, builder.opaque)
}

/// Splits `metrics` into batches at the given cut points, each batch's rates for the OTLP source and everything else
/// for the DogStatsD source, so that every record is one its source can produce.
fn batches(metrics: &[Metric], cuts: &[usize]) -> Vec<(&'static str, Vec<Metric>)> {
    let mut batches = Vec::new();
    let mut start = 0;
    for end in cuts.iter().copied().chain([metrics.len()]) {
        let (rates, others): (Vec<_>, Vec<_>) = metrics[start..end]
            .iter()
            .cloned()
            .partition(|metric| Kind::of(metric.values()) == Kind::Rate);
        batches.push(("dsd", others));
        batches.push(("otlp", rates));
        start = end;
    }
    batches
}

#[test]
fn random_plans_of_every_shape_are_valid_and_span_several_slots() {
    // The property test generates plans from these shapes; each must validate, or the property would hold vacuously,
    // and together they must exercise slot-to-slot exits.
    let shapes = [
        Shape::Emit,
        Shape::Drop,
        Shape::Split(Box::new(Shape::Emit), Box::new(Shape::Drop)),
        Shape::Tee(vec![Shape::Emit, Shape::Fence(Box::new(Shape::Emit)), Shape::Drop]),
        Shape::Rejoin(Box::new(Shape::Kernel(KernelKind::Mapper, Box::new(Shape::Emit)))),
        Shape::Detour(Box::new(Shape::Kernel(
            KernelKind::TagProjection,
            Box::new(Shape::Emit),
        ))),
        Shape::Fence(Box::new(Shape::Fence(Box::new(Shape::Drop)))),
        Shape::TimestampSplit(
            Box::new(Shape::Kernel(KernelKind::HostTags, Box::new(Shape::Emit))),
            Box::new(Shape::Kernel(KernelKind::NamespacePrefix, Box::new(Shape::Drop))),
        ),
        Shape::Kernel(KernelKind::HostEnrichment, Box::new(Shape::Emit)),
        Shape::DoubleDetour(Box::new(Shape::Emit)),
        Shape::Kernel(
            KernelKind::Mapper,
            Box::new(Shape::Kernel(KernelKind::Mapper, Box::new(Shape::Emit))),
        ),
    ];
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let mut slot_counts = Vec::new();
    for shape in shapes {
        for plan_index in 0..2 {
            let (plan, _) = random_plan(&shape, &params, plan_index);
            let report = Validator::new(&registry, &catalog).validate(&plan);
            assert!(report.is_valid(), "{:?}:\n{}", shape, report);
            let frame = Frame::compute(&plan, &registry, &namer).unwrap();
            compile(&plan, &report, &registry, &frame).unwrap();
            slot_counts.push(frame.slots().len());
        }
    }
    // The detour separates its split and its merge into two slots, and so does the double detour, whose second split
    // reaches the slot below by two edges over one exit. Two mappers, of 65 ns per event each, pipeline.
    assert_eq!(slot_counts[10], 2);
    assert_eq!(slot_counts[18], 2);
    assert_eq!(slot_counts[20], 2);
    assert!(slot_counts.iter().all(|count| *count >= 1));
}

#[test]
fn a_slot_reached_by_two_edges_over_one_exit_has_an_entry_for_each() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    params.swapped = vec![true; 8];
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let shape = Shape::DoubleDetour(Box::new(Shape::Emit));
    let entries = |plan_index: usize| {
        let (plan, _) = random_plan(&shape, &params, plan_index);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        let frame = Frame::compute(&random_plan(&shape, &params, 0).0, &registry, &namer).unwrap();
        let compiled = compile(&plan, &report, &registry, &frame).unwrap();
        compiled.placement.slots()[1]
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.input.to_string()))
            .collect::<Vec<_>>()
    };
    let region = |entry: u16| crate::compile::EntryKey::Region(saluki_core::execution::EntryId::new(entry));
    let exit = crate::compile::FrameSource::Exit {
        slot: crate::compile::SlotId::new(0),
        exit: crate::compile::ExitIndex::new(1),
    };
    // The second split's two edges share the first slot's exit to the second, as entries 0 and 1, in plan edge order;
    // the second plan swaps the merges they reach, so the same entries lead to the other merge.
    let host_entry = |entries: &[(crate::compile::EntryKey, crate::compile::FrameSource, String)]| entries[0].clone();
    let first = entries(0);
    let second = entries(1);
    assert_eq!(first.len(), 3);
    assert_eq!(host_entry(&first), host_entry(&second));
    assert_eq!(first[1], (region(0), exit, "n3_m.1".to_string()));
    assert_eq!(first[2], (region(1), exit, "n3_k.1".to_string()));
    assert_eq!(second[1], (region(0), exit, "n3_k.1".to_string()));
    assert_eq!(second[2], (region(1), exit, "n3_m.1".to_string()));
}

#[test]
fn a_batch_held_between_regions_across_a_commit_runs_its_own_generations_program_below() {
    // The double detour's second split sends to two merges of the slot below, over one exit, as entries 0 and 1; the
    // second plan swaps which merge each entry leads to, and the merges lead to different outputs. A batch of the first
    // generation held between the slots while the second commits, and while a batch of the second runs, must still
    // reach the merges its own program says.
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    params.swapped = vec![true; 8];
    params.changed = vec![false; 8];
    let shape = Shape::DoubleDetour(Box::new(Shape::Emit));
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let (first, opaque) = random_plan(&shape, &params, 0);
    let (second, _) = random_plan(&shape, &params, 1);
    let validator = Validator::new(&registry, &catalog);
    let (first_report, second_report) = (validator.validate(&first), validator.validate(&second));
    // Every kind, so that whatever the splits' predicates, both of the second split's ports carry events.
    let metrics = (0..16)
        .map(|index| match index % 4 {
            0 => Metric::counter(Context::from_static_name("app.requests"), 1.0),
            1 => Metric::gauge(Context::from_static_name("app.errors"), 2.0),
            2 => Metric::distribution(Context::from_static_name("my_metric_name"), 3.0),
            _ => Metric::counter(Context::from_static_name("demo.debug.trace"), 4.0),
        })
        .collect::<Vec<_>>();
    let trace = Trace::new()
        .install(first.clone())
        .admit(id("dsd"), AdmissionTime::from_unix_secs(1_000), metrics.clone())
        .install(second.clone())
        .admit(id("dsd"), AdmissionTime::from_unix_secs(1_001), metrics.clone());
    let interpreter = opaque
        .iter()
        .fold(Interpreter::new(&registry, &catalog), |interpreter, host| {
            interpreter.with_opaque_host(id(host), echo)
        });
    let run = interpreter.run(&trace).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut harness = FrameHarness::new(&first, &first_report, registry);
    harness.prepare(&second, &second_report);
    harness.hold_between_slots();
    harness.admit("dsd", AdmissionTime::from_unix_secs(1_000), metrics.clone());
    assert!(harness.held() > 0, "the first batch is held between the slots");
    harness.commit();
    harness.stop_holding();
    // The second batch runs through every region before the first one's held buffers reach the slot below.
    harness.admit("dsd", AdmissionTime::from_unix_secs(1_001), metrics);
    harness.release();
    assert_eq!(harness.held(), 0);
    assert_matches_interpreter(&mut harness, &second, &run);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_frames_of_region_drivers_deliver_what_the_interpreter_delivers(
        shape in arb_shape(),
        params in arb_plan_params(),
        metrics in vec(arb_metric(), 0..40),
        cuts in vec(any::<proptest::sample::Index>(), 0..4),
        install in proptest::option::of(any::<proptest::sample::Index>()),
        hold in any::<bool>(),
    ) {
        let registry = OperatorRegistry::native();
        let catalog = random_plan_catalog();
        let (first, opaque) = random_plan(&shape, &params, 0);
        let (second, _) = random_plan(&shape, &params, 1);
        let validator = Validator::new(&registry, &catalog);
        let (first_report, second_report) = (validator.validate(&first), validator.validate(&second));
        prop_assert!(first_report.is_valid(), "the first plan is invalid:\n{}", first_report);
        prop_assert!(second_report.is_valid(), "the second plan is invalid:\n{}", second_report);

        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let batches = batches(&metrics, &cuts);
        let install = install.map(|index| index.index(batches.len()));

        // The interpreter: the second plan installed after batch `install`, if any.
        let mut trace = Trace::new().install(first.clone());
        for (position, (source, batch)) in batches.iter().enumerate() {
            trace = trace.admit(id(source), AdmissionTime::from_unix_secs(1_000 + position as u64), batch.clone());
            if install == Some(position) {
                trace = trace.install(second.clone());
            }
        }
        let interpreter = opaque.iter().fold(Interpreter::new(&registry, &catalog), |interpreter, host| {
            interpreter.with_opaque_host(id(host), echo)
        });
        let run = interpreter.run(&trace).unwrap();

        // The same trace through a frame of region drivers. The second plan is prepared before batch `install`, which
        // is still stamped with the first generation and must run its programs although every region holds the second
        // one's, and committed after it. With `hold`, what batch `install` sends from one region to another is held
        // between them until the next batch has run, so that a region below runs a batch of the first generation after
        // the second committed and ran, and must find its entries in the first generation's program.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let mut harness = FrameHarness::new(&first, &first_report, registry);
        let mut plan = &first;
        for (position, (source, batch)) in batches.into_iter().enumerate() {
            if install == Some(position) {
                harness.prepare(&second, &second_report);
            }
            if install == Some(position) && hold {
                harness.hold_between_slots();
            }
            harness.admit(source, AdmissionTime::from_unix_secs(1_000 + position as u64), batch);
            if install.is_some_and(|install| install + 1 == position) {
                harness.release();
            }
            if install == Some(position) {
                prop_assert_eq!(harness.drivers()[0].generations().len(), 2, "both generations are live");
                harness.commit();
                harness.stop_holding();
                plan = &second;
            }
        }
        harness.release();
        harness.publish();

        if let Err(mismatch) = compare(&OutputPoints::from_run(&run), harness.outputs()) {
            prop_assert!(false, "region drivers differ from the interpreter:\n{}", mismatch);
        }
        // Without an install, every drop's policy loss matches; with one, the counters of a drop add up over both
        // generations, as the interpreter's do.
        for node in harness.drops_in_slots(plan) {
            prop_assert_eq!(
                harness.telemetry().loss(&node.to_string(), LossClass::Policy),
                run.loss(&node).loss(LossClass::Policy).points,
                "policy loss of `{}`", node
            );
        }
        // Retired generations are released: only the active one is left in every region.
        for driver in harness.drivers() {
            prop_assert_eq!(driver.generations().len(), 1);
        }
    }
}
