//! Compiled region programs, run by a minimal executor, deliver what the reference interpreter delivers.
//!
//! The executor below runs a frame's programs literally, as the steps document them: one batch at a time, every step
//! over every buffer of its registers, with no budgets and no channels. Fixed components run through the interpreter's
//! opaque-host adapters. A state step hands its buffers to the state owner's kernel with the step's state parameters,
//! and every flush tick runs each state owner's time-driven work at the tick's whole second, as if every source
//! heartbeat at the tick, through the entry of its time-driven output. It is the smallest host that exercises the
//! compiled steps, entries, exits, and admission routes, so comparing it with the interpreter checks compilation, not
//! a region host.
//!
//! On lanes, each lane of a slot runs its own kernels, and each edge reaches the lanes its routing names, so comparing
//! a frame on lanes with the interpreter, which has none, checks that programs compiled for lanes run every lane's
//! batches, and that keyed exits keep each context on one state lane.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Instant,
};

use proptest::prelude::*;
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, ScalarPoints},
        Event,
    },
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, Kernel, LossClass, LossCount, PortIndex, PortOutputs,
        Turn, TurnCounters,
    },
    topology::EventsBuffer,
};

use super::{
    rewrite::harness::{self, arb_plan_params, arb_shape, arb_trace, Case, TraceParams},
    *,
};
use crate::{
    equivalence::{compare, OutputPoints},
    interpret::{HostBatch, HostOutput, HostTick, Interpreter, OpaqueHostAdapter, Trace, TraceEvent},
    ir::{NativeComponentId, NodeId, Plan},
    operators::{KernelBuildContext, OperatorRegistry},
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
    validate::Validator,
};

/// The base time, a multiple of the 10-second window.
const B: u64 = 1_700_000_000;

fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

/// One event of the trace both executions run: a batch admitted at one lane of a source (lane 0 for a source without
/// lanes; the interpreter has none), or a flush tick.
enum TraceStep {
    Admit(&'static str, u16, AdmissionTime, Vec<Metric>),
    Advance(AdmissionTime),
}

/// A minimal host for a frame's compiled programs.
///
/// A frame on lanes runs every lane of a slot with kernels of its own, and every batch reaches the lane its edge's
/// routing names: a producer lane's own lane, the lane owning each event's context key behind a keyed exit, the
/// sender's lane modulo the target's for affinity, and lanes in rotation for a spread. Batches still run one at a time,
/// to completion, in trace order, so a frame on lanes delivers in the order one lane does.
struct Executor {
    frame: Frame,
    programs: Vec<Arc<RegionProgram>>,
    /// Each slot's kernels, per lane.
    kernels: Vec<Vec<Vec<Box<dyn Kernel>>>>,
    hosts: BTreeMap<NodeId, Box<dyn OpaqueHostAdapter>>,
    host_ports: BTreeMap<NodeId, usize>,
    last_tick: Option<AdmissionTime>,
    outputs: OutputPoints,
    loss: BTreeMap<NodeId, LossCount>,
    /// The rotation of every spread edge, by the lanes it has sent to so far.
    rotation: u64,
    /// The lanes of each isolation slot each context key reached a state step on.
    state_lanes: BTreeMap<(SlotId, u64), BTreeSet<u16>>,
    /// The lanes of each slot that the batches stamped at each admission point reached.
    point_lanes: BTreeMap<(SlotId, AdmissionPointId), BTreeSet<u16>>,
}

impl Executor {
    /// Compiles a default program with the native registry: its aggregation runs as a kernel in an isolation slot.
    fn new(plan: &Plan) -> Self {
        Self::on_lanes(plan, LanePolicy::ONE, FrameOptions::default())
    }

    /// Compiles a default program with the native registry into its frame on `lanes`, laid out with `options`.
    fn on_lanes(plan: &Plan, lanes: LanePolicy, options: FrameOptions) -> Self {
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, &default_program_catalog()).validate(plan);
        let frame = Frame::compute_with(plan, &registry, &namer, &FrameHosts::new(), &lanes, options).unwrap();
        let compiled = compile(plan, &report, &registry, &frame).unwrap();
        assert!(
            frame.slots().iter().any(Slot::is_isolation),
            "aggregation runs in an isolation slot"
        );
        Self::of(frame, compiled, BTreeMap::new())
    }

    /// Runs a plan's compiled programs in its frame, with `hosts`: an adapter, and its number of output ports, for each
    /// fixed component the trace reaches.
    fn of(frame: Frame, compiled: CompiledPlan, hosts: BTreeMap<NodeId, (Box<dyn OpaqueHostAdapter>, usize)>) -> Self {
        let kernels = compiled
            .programs
            .iter()
            .zip(frame.slots())
            .map(|(program, slot)| {
                (0..slot.lanes)
                    .map(|_| {
                        program
                            .kernels()
                            .iter()
                            .map(|kernel| {
                                kernel
                                    .spec
                                    .build(&KernelBuildContext::detached(&kernel.key.node))
                                    .unwrap()
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let host_ports = hosts.iter().map(|(node, (_, ports))| (node.clone(), *ports)).collect();
        let hosts = hosts.into_iter().map(|(node, (adapter, _))| (node, adapter)).collect();
        Self {
            frame,
            programs: compiled.programs,
            kernels,
            hosts,
            host_ports,
            last_tick: None,
            outputs: OutputPoints::new(),
            loss: BTreeMap::new(),
            rotation: 0,
            state_lanes: BTreeMap::new(),
            point_lanes: BTreeMap::new(),
        }
    }

    fn run(&mut self, events: Vec<TraceStep>) {
        for event in events {
            match event {
                TraceStep::Admit(source, lane, at, metrics) => self.admit(&id(source), lane, at, metrics),
                TraceStep::Advance(now) => {
                    self.last_tick = Some(now);
                    let nodes = self.hosts.keys().cloned().collect::<Vec<_>>();
                    for node in nodes {
                        let mut output = HostOutput::new(self.host_ports[&node]);
                        let host = self.hosts.get_mut(&node).unwrap();
                        host.tick(HostTick { now, watermark: None }, &mut output);
                        self.readmit(&node, output, now);
                    }
                    self.tick_state_owners(now);
                }
            }
        }
    }

    /// Runs every state owner's flush, on every lane, at the whole second of `now`, and its output through its
    /// time-driven entry.
    fn tick_state_owners(&mut self, now: AdmissionTime) {
        let flush = AdmissionTime::from_unix_secs(now.as_unix_secs());
        let slots = self
            .frame
            .slots()
            .iter()
            .filter(|slot| slot.is_isolation())
            .map(|slot| (slot.id, slot.lanes))
            .collect::<Vec<_>>();
        for (slot, lanes) in slots {
            let program = Arc::clone(&self.programs[slot.get()]);
            for lane in 0..lanes {
                for (index, kernel) in program.kernels().iter().enumerate() {
                    if kernel.lineage.is_none() {
                        continue;
                    }
                    let kernel = KernelIndex::new(index as u16);
                    let header = BatchHeader {
                        admitted_at: Some(flush),
                        ..BatchHeader::default()
                    };
                    let params = program
                        .entries()
                        .iter()
                        .flat_map(|entry| &entry.steps)
                        .find_map(|step| match step {
                            Step::State {
                                kernel: fed, params, ..
                            } if *fed == kernel => Some(*params),
                            _ => None,
                        })
                        .and_then(|params| program.state_param(params))
                        .map(|entry| Arc::clone(&entry.params));
                    let mut budget = Budget::unlimited();
                    let mut ports = PortOutputs::new(1);
                    let mut counters = TurnCounters::default();
                    let instance = &mut self.kernels[slot.get()][usize::from(lane)][kernel.get()];
                    let mut turn = Turn::new(&header, Instant::now(), &mut budget, &mut ports, &mut counters)
                        .with_watermark(Some(flush));
                    if let Some(params) = &params {
                        turn = turn.with_param(params.as_ref());
                    }
                    instance.advance_time(&mut turn);
                    let output = ports.drain(PortIndex::PRIMARY).collect::<Vec<_>>();
                    let entry = program
                        .entry(EntryKey::StateOutput(kernel))
                        .expect("every state owner has a time-driven output entry");
                    for (exit, buffer) in self.run_entry(slot, lane, &program, entry, header.clone(), output) {
                        self.send(slot, lane, exit, buffer, now);
                    }
                }
            }
        }
    }

    /// Admits a batch at one lane of a source's admission point: the point of that lane, for a point with lanes.
    fn admit(&mut self, source: &NodeId, lane: u16, at: AdmissionTime, metrics: Vec<Metric>) {
        let point = self
            .frame
            .admission_point_at(source, &"metrics".parse().unwrap())
            .unwrap()
            .clone();
        let header = BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(point.lane_id(lane).expect("the source has the lane")),
            ..BatchHeader::default()
        };
        let arrival = self.last_tick.map_or(at, |tick| tick.max(at));
        let target_lane = self.point_lane(&point, lane);
        self.deliver(point.route.clone(), target_lane, buffer_of(header, metrics), arrival);
    }

    /// Returns the lane of its target that a batch of one lane of an admission point reaches.
    fn point_lane(&mut self, point: &AdmissionPointInfo, lane: u16) -> u16 {
        match (point.routing, &point.route) {
            (PointRouting::Fixed, _) => lane,
            (PointRouting::Spread, Some(FrameTarget::Slot(slot))) => {
                let lanes = self.frame.slot(*slot).unwrap().lanes;
                self.rotate(lanes)
            }
            _ => 0,
        }
    }

    fn rotate(&mut self, lanes: u16) -> u16 {
        self.rotation += 1;
        (self.rotation % u64::from(lanes)) as u16
    }

    /// Re-admits what a host emitted, stamped at its output's admission point with the host's clock.
    fn readmit(&mut self, node: &NodeId, mut output: HostOutput, at: AdmissionTime) {
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
            let header = BatchHeader {
                admitted_at: Some(at),
                admission_point: Some(point.id),
                ..BatchHeader::default()
            };
            let lane = self.point_lane(&point, 0);
            self.deliver(point.route.clone(), lane, buffer_of(header, metrics), at);
        }
    }

    /// Sends what one lane of a slot handed to one of its exits to the lanes of its target, as the exit's routing says.
    fn send(&mut self, slot: SlotId, lane: u16, exit: ExitIndex, buffer: EventsBuffer, arrival: AdmissionTime) {
        let exit = self.frame.slot(slot).unwrap().exits[exit.get()].clone();
        let lanes = match &exit.target {
            FrameTarget::Slot(next) => self.frame.slot(*next).unwrap().lanes,
            _ => 1,
        };
        match &exit.routing {
            ExitRouting::One => self.deliver(Some(exit.target), 0, buffer, arrival),
            ExitRouting::Affinity => self.deliver(Some(exit.target), lane % lanes, buffer, arrival),
            ExitRouting::Spread => {
                let lane = self.rotate(lanes);
                self.deliver(Some(exit.target), lane, buffer, arrival)
            }
            ExitRouting::Keyed(groups) => {
                // Each event to the lane that owns its context key, in order within each lane.
                let mut routed = (0..lanes).map(|_| Vec::new()).collect::<Vec<_>>();
                let header = buffer.header().clone();
                for event in buffer {
                    let metric = event.try_into_metric().unwrap();
                    routed[usize::from(groups.lane_of(metric.context().key().as_u64()))].push(metric);
                }
                for (lane, metrics) in routed.into_iter().enumerate() {
                    if !metrics.is_empty() {
                        let buffer = buffer_of(header.clone(), metrics);
                        self.deliver(Some(exit.target.clone()), lane as u16, buffer, arrival);
                    }
                }
            }
        }
    }

    /// Delivers a buffer to `target`, at `lane` of a slot with lanes.
    fn deliver(&mut self, target: Option<FrameTarget>, lane: u16, buffer: EventsBuffer, arrival: AdmissionTime) {
        match target {
            None => {}
            Some(FrameTarget::Destination(output)) => {
                for event in buffer {
                    self.outputs.insert(&output, &event.try_into_metric().unwrap());
                }
            }
            Some(FrameTarget::EncodeHost(host)) => {
                unreachable!("these plans bind no output to a delivery host, such as `{}`", host)
            }
            Some(FrameTarget::Slot(slot)) => {
                if let Some(point) = buffer.header().admission_point {
                    self.point_lanes.entry((slot, point)).or_default().insert(lane);
                }
                for (exit, buffer) in self.run_slot(slot, lane, buffer) {
                    self.send(slot, lane, exit, buffer, arrival);
                }
            }
            Some(FrameTarget::Fixed(node)) => {
                let admitted_at = buffer.header().admitted_at;
                let events = buffer.into_iter().filter_map(Event::try_into_metric).collect();
                let mut output = HostOutput::new(self.host_ports[&node]);
                let host = self
                    .hosts
                    .get_mut(&node)
                    .expect("every fixed component the trace reaches has a host");
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

    fn count_loss(&mut self, node: &NodeId, buffers: &[EventsBuffer]) {
        let mut counters = TurnCounters::default();
        for buffer in buffers {
            let points = buffer
                .into_iter()
                .filter_map(Event::try_as_metric)
                .map(|metric| metric.values().len() as u64)
                .sum();
            counters.record_loss(LossClass::Policy, buffer.len() as u64, points);
        }
        let loss = counters.loss(LossClass::Policy);
        let total = self.loss.entry(node.clone()).or_default();
        total.events += loss.events;
        total.points += loss.points;
    }

    fn run_slot(&mut self, slot: SlotId, lane: u16, mut batch: EventsBuffer) -> Vec<(ExitIndex, EventsBuffer)> {
        let program = Arc::clone(&self.programs[slot.get()]);
        let entry = program
            .entry_for(batch.header())
            .expect("every batch reaching a slot has an entry");
        let header = batch.take_header();
        self.run_entry(slot, lane, &program, entry, header, vec![batch])
    }

    /// Runs an entry's steps, on one lane of its slot, over the buffers it starts with, in register 0, returning what
    /// it hands to its exits.
    fn run_entry(
        &mut self, slot: SlotId, lane: u16, program: &RegionProgram, entry: &EntryProgram, header: BatchHeader,
        batches: Vec<EventsBuffer>,
    ) -> Vec<(ExitIndex, EventsBuffer)> {
        let mut registers: Vec<Vec<EventsBuffer>> = (0..entry.registers).map(|_| Vec::new()).collect();
        registers[0].extend(batches);
        let mut exits = Vec::new();

        for step in &entry.steps {
            match step {
                Step::Map { kernel, register } => {
                    let buffers = std::mem::take(&mut registers[register.get()]);
                    for buffer in buffers {
                        let mut ports = self.consume(slot, lane, *kernel, &header, buffer, 1);
                        registers[register.get()].append(&mut ports[0]);
                    }
                }
                Step::Expand { kernel, input, ports } => {
                    let buffers = std::mem::take(&mut registers[input.get()]);
                    for buffer in buffers {
                        let outputs = self.consume(slot, lane, *kernel, &header, buffer, ports.len());
                        for (route, buffers) in ports.iter().zip(outputs) {
                            self.route(route, buffers, &mut registers);
                        }
                    }
                }
                Step::Decide {
                    decision,
                    input,
                    targets,
                } => {
                    let decision = program.decision(*decision).unwrap();
                    let buffers = std::mem::take(&mut registers[input.get()]);
                    let mut scratch = DecisionScratch::default();
                    for buffer in buffers {
                        let mut routed = (0..targets.len()).map(|_| Vec::new()).collect::<Vec<Vec<Event>>>();
                        for event in buffer {
                            for exit in decision.evaluate(&event, &mut scratch).iter() {
                                routed[exit.get()].push(event.clone());
                            }
                        }
                        for (route, events) in targets.iter().zip(routed) {
                            if events.is_empty() {
                                continue;
                            }
                            let buffer = buffer_of(
                                header.clone(),
                                events
                                    .into_iter()
                                    .map(|event| event.try_into_metric().unwrap())
                                    .collect(),
                            );
                            self.route(route, vec![buffer], &mut registers);
                        }
                    }
                }
                Step::Concat { inputs, output } => {
                    for input in inputs {
                        let mut buffers = std::mem::take(&mut registers[input.get()]);
                        registers[output.get()].append(&mut buffers);
                    }
                }
                Step::Exit { register, exit, entry } => {
                    for mut buffer in std::mem::take(&mut registers[register.get()]) {
                        let mut stamped = header.clone();
                        stamped.entry = entry.region();
                        buffer.set_header(stamped);
                        exits.push((*exit, buffer));
                    }
                }
                Step::Drop { register, node } => {
                    let buffers = std::mem::take(&mut registers[register.get()]);
                    self.count_loss(node, &buffers);
                }
                Step::State {
                    kernel,
                    register,
                    params,
                } => {
                    let params = Arc::clone(
                        &program
                            .state_param(*params)
                            .expect("the step's state parameters")
                            .params,
                    );
                    for mut buffer in std::mem::take(&mut registers[register.get()]) {
                        for metric in (&buffer).into_iter().filter_map(Event::try_as_metric) {
                            self.state_lanes
                                .entry((slot, metric.context().key().as_u64()))
                                .or_default()
                                .insert(lane);
                        }
                        let mut budget = Budget::unlimited();
                        let mut ports = PortOutputs::new(1);
                        let mut counters = TurnCounters::default();
                        let instance = &mut self.kernels[slot.get()][usize::from(lane)][kernel.get()];
                        let mut turn = Turn::new(&header, Instant::now(), &mut budget, &mut ports, &mut counters)
                            .with_param(params.as_ref());
                        instance.consume(&mut turn, &mut buffer);
                        ports.complete(buffer);
                        assert!(ports.is_empty(), "a state owner emits nothing while it admits a batch");
                    }
                }
            }
        }
        assert!(registers.iter().all(Vec::is_empty), "every register is consumed");
        exits
    }

    fn route(&mut self, route: &Route, buffers: Vec<EventsBuffer>, registers: &mut [Vec<EventsBuffer>]) {
        match route {
            Route::Register(register) => registers[register.get()].extend(buffers),
            Route::Drop(node) => self.count_loss(node, &buffers),
        }
    }

    /// Runs one kernel of one lane over one buffer, returning each port's buffers, stamped with the batch's header.
    fn consume(
        &mut self, slot: SlotId, lane: u16, kernel: KernelIndex, header: &BatchHeader, mut input: EventsBuffer,
        port_count: usize,
    ) -> Vec<Vec<EventsBuffer>> {
        let mut budget = Budget::unlimited();
        let mut ports = PortOutputs::new(port_count);
        let mut counters = TurnCounters::default();
        let instance = &mut self.kernels[slot.get()][usize::from(lane)][kernel.get()];
        let mut turn = Turn::new(header, Instant::now(), &mut budget, &mut ports, &mut counters);
        instance.consume(&mut turn, &mut input);
        ports.complete(input);
        (0..port_count)
            .map(|port| {
                ports
                    .drain(PortIndex::new(port as u16))
                    .map(|mut buffer| {
                        buffer.set_header(header.clone());
                        buffer
                    })
                    .collect()
            })
            .collect()
    }
}

fn buffer_of(header: BatchHeader, metrics: Vec<Metric>) -> EventsBuffer {
    let mut buffer = EventsBuffer::default().with_header(header);
    for metric in metrics {
        assert!(
            buffer.try_push(Event::Metric(metric)).is_none(),
            "test batches fit one buffer"
        );
    }
    buffer
}

fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

fn counter_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), (B + relative, value))
}

/// The trace of the default-program interpreter fixture: DogStatsD and OTLP batches, a flush tick, and batches after
/// the host tags' deadline. Without the no-aggregation pipeline, DogStatsD parses no timestamps, so its timestamped
/// points are left out.
fn events(timestamped_dogstatsd: bool) -> Vec<TraceStep> {
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
    let late_dogstatsd = if timestamped_dogstatsd {
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
    vec![
        TraceStep::Admit("dsd_in", 0, t(3), dogstatsd),
        TraceStep::Admit("otlp_in", 0, t(4), otlp),
        TraceStep::Advance(t(15)),
        TraceStep::Admit("otlp_in", 0, t(301), vec![gauge_at("checkout.cpu", 301, 0.5)]),
        TraceStep::Admit("dsd_in", 0, t(302), vec![late_dogstatsd]),
    ]
}

fn trace(plan: Plan, events: &[TraceStep]) -> Trace {
    let mut trace = Trace::new().install(plan);
    for event in events {
        trace = match event {
            TraceStep::Admit(source, _, at, metrics) => trace.admit(id(source), *at, metrics.clone()),
            TraceStep::Advance(now) => trace.advance(*now),
        };
    }
    trace
}

#[test]
fn compiled_programs_deliver_what_the_interpreter_delivers() {
    // Aggregation runs as a kernel in its isolation slot: its batches are state steps, and its flushes run its
    // time-driven output entry. The interpreter's sources heartbeat at every tick, so its watermark has reached each
    // tick when it fires, as the executor's flushes assume; the last tick closes every bucket the trace filled.
    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let mut steps = events(timestamped);
        steps.push(TraceStep::Advance(t(315)));
        let run = Interpreter::new(&registry, &catalog)
            .with_heartbeats_at_ticks()
            .run(&trace(plan.clone(), &steps))
            .unwrap();

        // Kernels with caches need a Tokio runtime to be built in, as the interpreter's are.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut executor = Executor::new(&plan);
        executor.run(steps);

        let expected = OutputPoints::from_run(&run);
        assert!(
            expected.count(&"dd_metrics_encode".parse().unwrap()) > 10,
            "the trace delivers"
        );
        if let Err(mismatch) = compare(&expected, &executor.outputs) {
            panic!("compiled programs differ from the interpreter:\n{}", mismatch);
        }
        for node in [
            "dsd_prefix_filter_blocklist_true",
            "dsd_post_agg_filter_true",
            "metrics_routing_filter_0_false",
            "metrics_routing_filter_1_false",
            "mrf_metrics_gateway_false",
            "af_metrics_gateway_false",
        ] {
            assert_eq!(
                executor.loss.get(&id(node)).copied().unwrap_or_default(),
                run.loss(&id(node)).loss(LossClass::Policy),
                "{}",
                node
            );
        }
    }
}

// Programs on lanes.

/// The tags of the contexts the lane trace spreads over the state lanes.
const SHARDS: [&str; 24] = [
    "shard:0", "shard:1", "shard:2", "shard:3", "shard:4", "shard:5", "shard:6", "shard:7", "shard:8", "shard:9",
    "shard:10", "shard:11", "shard:12", "shard:13", "shard:14", "shard:15", "shard:16", "shard:17", "shard:18",
    "shard:19", "shard:20", "shard:21", "shard:22", "shard:23",
];

/// The default-program trace with DogStatsD batches of many contexts, each kind in its own name, admitted in turn at
/// each of `lanes` producer lanes, in both flush intervals; every batch on a lane is one its decoder could send.
fn lane_events(timestamped_dogstatsd: bool, lanes: u16) -> Vec<TraceStep> {
    let shards = |name: &'static str, kind: usize, at: u64| {
        SHARDS
            .iter()
            .enumerate()
            .filter(|(shard, _)| shard % 3 == kind)
            .map(|(shard, tag)| {
                let context = Context::from_static_parts(name, std::slice::from_ref(tag));
                match kind {
                    0 => Metric::counter(context, shard as f64),
                    1 => Metric::gauge(context, (at + shard as u64) as f64),
                    _ => Metric::distribution(context, shard as f64),
                }
            })
            .collect::<Vec<_>>()
    };
    let mut steps = events(timestamped_dogstatsd);
    let mut batches = Vec::new();
    for (round, at) in [5u64, 6, 8, 16, 18].into_iter().enumerate() {
        for (kind, name) in ["lanes.requests", "lanes.queue", "lanes.latency"]
            .into_iter()
            .enumerate()
        {
            batches.push((round * 3 + kind, at, shards(name, kind, at)));
        }
    }
    // The first flush tick is the third step: the first rounds go before it, the others after.
    let (before, after): (Vec<_>, Vec<_>) = batches.into_iter().partition(|(_, at, _)| *at < 15);
    let lane = |index: usize| (index % usize::from(lanes)) as u16;
    let late = steps.split_off(2);
    steps.extend(
        before
            .into_iter()
            .map(|(index, at, metrics)| TraceStep::Admit("dsd_in", lane(index), t(at), metrics)),
    );
    steps.extend(late);
    let advance = steps
        .iter()
        .position(|step| matches!(step, TraceStep::Advance(_)))
        .unwrap();
    let rest = steps.split_off(advance + 1);
    steps.extend(
        after
            .into_iter()
            .map(|(index, at, metrics)| TraceStep::Admit("dsd_in", lane(index), t(at), metrics)),
    );
    steps.push(TraceStep::Advance(t(30)));
    steps.extend(rest);
    steps.push(TraceStep::Advance(t(315)));
    // The trace's own DogStatsD batches go to the last producer lane.
    for step in &mut steps {
        if let TraceStep::Admit("dsd_in", lane, at, _) = step {
            if *at == t(3) || *at == t(302) {
                *lane = lanes - 1;
            }
        }
    }
    steps
}

#[test]
fn compiled_programs_on_lanes_deliver_what_the_interpreter_delivers_and_keep_each_context_on_one_state_lane() {
    // Each lane runs kernels of its own, DogStatsD's batches arrive at every producer lane, the keyed exit splits them
    // by context over the state lanes, and the other edges route by affinity or spread: what the outputs receive is
    // what the interpreter, which has no lanes, delivers, at every policy, and without pipelining on one lane.
    let policies = [
        LanePolicy::ONE,
        LanePolicy {
            dogstatsd: 2,
            state: 2,
            post: 2,
        },
        LanePolicy {
            dogstatsd: 3,
            state: 4,
            post: 2,
        },
        LanePolicy {
            dogstatsd: 1,
            state: 3,
            post: 1,
        },
        LanePolicy {
            dogstatsd: 4,
            state: 1,
            post: 3,
        },
        // One producer lane keeps its passthrough on one post-aggregation lane.
        LanePolicy {
            dogstatsd: 1,
            state: 2,
            post: 3,
        },
    ];
    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let run = Interpreter::new(&registry, &catalog)
            .with_heartbeats_at_ticks()
            .run(&trace(plan.clone(), &lane_events(timestamped, 1)))
            .unwrap();
        let expected = OutputPoints::from_run(&run);
        assert!(
            expected.count(&"dd_metrics_encode".parse().unwrap()) > 40,
            "the trace delivers"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        for lanes in policies {
            let mut executor = Executor::on_lanes(&plan, lanes, FrameOptions { pipelining: false });
            executor.run(lane_events(timestamped, lanes.dogstatsd));
            if let Err(mismatch) = compare(&expected, &executor.outputs) {
                panic!(
                    "compiled programs on {:?} differ from the interpreter:\n{}",
                    lanes, mismatch
                );
            }
            for node in ["dsd_prefix_filter_blocklist_true", "dsd_post_agg_filter_true"] {
                assert_eq!(
                    executor.loss.get(&id(node)).copied().unwrap_or_default(),
                    run.loss(&id(node)).loss(LossClass::Policy),
                    "{} on {:?}",
                    node,
                    lanes
                );
            }

            // Every context reaches one state lane, and with several, the contexts spread over all of them.
            assert!(
                executor.state_lanes.values().all(|lanes| lanes.len() == 1),
                "{:?}",
                executor.state_lanes
            );
            let used = executor.state_lanes.values().flatten().collect::<BTreeSet<_>>();
            assert_eq!(used.len(), usize::from(lanes.state), "state lanes used on {:?}", lanes);

            // Each producer lane's batches, and so each stream connection's, reach one lane of every stateless slot
            // downstream, so the passthrough keeps their order (the keyed exit keeps each context's).
            let slot_named = |name: &str| {
                executor
                    .frame
                    .slots()
                    .iter()
                    .find(|slot| slot.name.as_str() == name)
                    .unwrap()
                    .id
            };
            let (agg, post) = (slot_named("dsd_agg"), slot_named("metrics_enrich"));
            let dogstatsd = executor.frame.admission_points()[0].clone();
            let reached = executor
                .point_lanes
                .iter()
                .filter(|((slot, point), _)| *slot != agg && dogstatsd.lane_of(*point).is_some())
                .collect::<Vec<_>>();
            assert!(
                reached.iter().all(|(_, lanes)| lanes.len() == 1),
                "the producer lanes' batches on {:?}: {:?}",
                lanes,
                reached
            );
            assert_eq!(
                reached.iter().any(|((slot, _), _)| *slot == post),
                timestamped,
                "the passthrough reaches the post-aggregation slot on {:?}",
                lanes
            );
        }
    }
}

// Rewritten plans.

/// Compiles a random plan of the rewrite harness, with every logical rewrite, and runs its programs over the trace
/// without capacity bounds; returns what its outputs received and whether a logical rewrite applied.
fn run_compiled(case: &Case, trace: &TraceParams) -> Result<(OutputPoints, bool), String> {
    let compiled = compile(&case.plan, &case.report, harness::registry(), &case.frame)
        .map_err(|error| format!("the plan does not compile: {}", error))?;
    let rewritten = compiled.rewrites.iter().any(|record| record.is_applied());
    // Kernels with caches need a Tokio runtime to be built in, as the interpreter's are.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|error| error.to_string())?;
    let _guard = runtime.enter();
    let hosts = case
        .opaque
        .iter()
        .map(|host| (host.clone(), (harness::echo_host(), 1)))
        .collect();
    let mut executor = Executor::of(case.frame.clone(), compiled, hosts);
    for event in trace.events(&case.outputs(), false) {
        if let TraceEvent::Admit {
            source,
            lane,
            admitted_at,
            batch,
        } = event
        {
            executor.admit(&source, lane, admitted_at, batch);
        }
    }
    Ok((executor.outputs, rewritten))
}

/// Requires the compiled, rewritten programs of a random plan to deliver what the reference interpreter delivers
/// running the plan as written; returns whether a logical rewrite applied.
fn check_compiled(case: &Case, trace: &TraceParams) -> Result<bool, String> {
    let (delivered, rewritten) = run_compiled(case, trace)?;
    let expected = OutputPoints::from_run(&case.run(&case.plan, &trace.events(&case.outputs(), false))?);
    compare(&expected, &delivered)
        .map_err(|mismatch| format!("the compiled programs deliver something else:\n{}", mismatch))?;
    Ok(rewritten)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// R5a's copies, partial push-downs, and whole moves, and R6's hoists, run as compiled steps of region programs,
    /// not only in the interpreter.
    #[test]
    fn property_test_compiled_programs_of_rewritten_plans_deliver_what_the_interpreter_delivers(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let checked = check_compiled(&case, &trace);
        prop_assert!(checked.is_ok(), "{:?}", checked.err());
    }
}

#[test]
fn many_random_plans_run_compiled_after_a_rewrite() {
    // The property test would hold vacuously if the pipeline rewrote nothing: count the compiled plans it rewrote.
    let rewritten = harness::sample(64)
        .iter()
        .filter(|(case, trace)| check_compiled(case, trace).unwrap())
        .count();
    eprintln!("{} of 64 plans rewritten", rewritten);
    assert!(rewritten > 24, "only {} of 64 plans were rewritten", rewritten);
}
