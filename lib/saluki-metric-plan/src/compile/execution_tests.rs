//! Compiled region programs, run by a minimal executor, deliver what the reference interpreter delivers.
//!
//! The executor below runs a frame's programs literally, as the steps document them: one batch at a time, every step
//! over every buffer of its registers, with no budgets and no channels. Fixed components run through the interpreter's
//! opaque-host adapters. It is the smallest host that exercises the compiled steps, entries, exits, and admission
//! routes, so comparing it with the interpreter checks compilation, not a region host.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Instant,
};

use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, ScalarPoints},
        Event,
    },
    execution::{
        AdmissionTime, BatchHeader, Budget, Kernel, LossClass, LossCount, PortIndex, PortOutputs, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};

use super::*;
use crate::{
    equivalence::{compare, OutputPoints},
    interpret::{adapters::AggregateAdapter, HostBatch, HostOutput, HostTick, Interpreter, OpaqueHostAdapter, Trace},
    ir::{NativeComponentId, NodeId, Plan},
    model::EventView,
    operators::{names, KernelBuildContext, OperatorRegistry},
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

/// One event of the trace both executions run: a batch admitted at a source, or a flush tick.
enum TraceStep {
    Admit(&'static str, AdmissionTime, Vec<Metric>),
    Advance(AdmissionTime),
}

/// A minimal host for a frame's compiled programs.
struct Executor {
    frame: Frame,
    programs: Vec<Arc<RegionProgram>>,
    kernels: Vec<Vec<Box<dyn Kernel>>>,
    hosts: BTreeMap<NodeId, Box<dyn OpaqueHostAdapter>>,
    host_ports: BTreeMap<NodeId, usize>,
    last_tick: Option<AdmissionTime>,
    outputs: OutputPoints,
    loss: BTreeMap<NodeId, LossCount>,
}

impl Executor {
    fn new(plan: &Plan) -> Self {
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, &default_program_catalog()).validate(plan);
        let frame = Frame::compute(plan, &registry, &namer).unwrap();
        let compiled = compile(plan, &report, &registry, &frame).unwrap();
        let kernels = compiled
            .programs
            .iter()
            .map(|program| {
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
            .collect();
        let mut hosts = BTreeMap::new();
        let mut host_ports = BTreeMap::new();
        for fixed in frame.fixed() {
            if fixed.operator.as_str() == names::AGGREGATE {
                let op = registry.configure(fixed.operator.as_str(), &fixed.params).unwrap();
                hosts.insert(
                    fixed.node.clone(),
                    Box::new(AggregateAdapter::from_operator(&op).unwrap()) as Box<dyn OpaqueHostAdapter>,
                );
                host_ports.insert(fixed.node.clone(), op.ports().outputs.len());
            }
        }
        Self {
            frame,
            programs: compiled.programs,
            kernels,
            hosts,
            host_ports,
            last_tick: None,
            outputs: OutputPoints::new(),
            loss: BTreeMap::new(),
        }
    }

    fn run(&mut self, events: Vec<TraceStep>) {
        for event in events {
            match event {
                TraceStep::Admit(source, at, metrics) => {
                    let point = self
                        .frame
                        .admission_point_at(&id(source), &"metrics".parse().unwrap())
                        .unwrap()
                        .clone();
                    let header = BatchHeader {
                        admitted_at: Some(at),
                        admission_point: Some(point.id),
                        ..BatchHeader::default()
                    };
                    let arrival = self.last_tick.map_or(at, |tick| tick.max(at));
                    self.deliver(point.route.clone(), buffer_of(header, metrics), arrival);
                }
                TraceStep::Advance(now) => {
                    self.last_tick = Some(now);
                    let nodes = self.hosts.keys().cloned().collect::<Vec<_>>();
                    for node in nodes {
                        let mut output = HostOutput::new(self.host_ports[&node]);
                        let host = self.hosts.get_mut(&node).unwrap();
                        host.tick(HostTick { now, watermark: None }, &mut output);
                        self.readmit(&node, output, now);
                    }
                }
            }
        }
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
            self.deliver(point.route.clone(), buffer_of(header, metrics), at);
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
                    for (target, buffer) in self.run_slot(slot, buffer) {
                        queue.push_back((Some(target), buffer));
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

    fn run_slot(&mut self, slot: SlotId, mut batch: EventsBuffer) -> Vec<(FrameTarget, EventsBuffer)> {
        let program = Arc::clone(&self.programs[slot.get()]);
        let entry = program
            .entry_for(batch.header())
            .expect("every batch reaching a slot has an entry");
        let header = batch.take_header();
        let mut registers: Vec<Vec<EventsBuffer>> = (0..entry.registers).map(|_| Vec::new()).collect();
        registers[0].push(batch);
        let mut exits = Vec::new();

        for step in &entry.steps {
            match step {
                Step::Map { kernel, register } => {
                    let buffers = std::mem::take(&mut registers[register.get()]);
                    for buffer in buffers {
                        let mut ports = self.consume(slot, *kernel, &header, buffer, 1);
                        registers[register.get()].append(&mut ports[0]);
                    }
                }
                Step::Expand { kernel, input, ports } => {
                    let buffers = std::mem::take(&mut registers[input.get()]);
                    for buffer in buffers {
                        let outputs = self.consume(slot, *kernel, &header, buffer, ports.len());
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
                    for buffer in buffers {
                        let mut routed = (0..targets.len()).map(|_| Vec::new()).collect::<Vec<Vec<Event>>>();
                        for event in buffer {
                            match &decision.kind {
                                DecisionKind::Split { compiled, .. } => {
                                    let matched =
                                        compiled.evaluate(&mut EventView::new(event.try_as_metric().unwrap()));
                                    routed[if matched { 0 } else { 1 }].push(event);
                                }
                                DecisionKind::Tee => {
                                    for target in routed.iter_mut() {
                                        target.push(event.clone());
                                    }
                                }
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
                    let target = self.frame.slot(slot).unwrap().exits[exit.get()].target.clone();
                    for mut buffer in std::mem::take(&mut registers[register.get()]) {
                        let mut stamped = header.clone();
                        stamped.entry = *entry;
                        buffer.set_header(stamped);
                        exits.push((target.clone(), buffer));
                    }
                }
                Step::Drop { register, node } => {
                    let buffers = std::mem::take(&mut registers[register.get()]);
                    self.count_loss(node, &buffers);
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

    /// Runs one kernel over one buffer, returning each port's buffers, stamped with the batch's header.
    fn consume(
        &mut self, slot: SlotId, kernel: KernelIndex, header: &BatchHeader, mut input: EventsBuffer, port_count: usize,
    ) -> Vec<Vec<EventsBuffer>> {
        let mut budget = Budget::unlimited();
        let mut ports = PortOutputs::new(port_count);
        let mut counters = TurnCounters::default();
        let instance = &mut self.kernels[slot.get()][kernel.get()];
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
        TraceStep::Admit("dsd_in", t(3), dogstatsd),
        TraceStep::Admit("otlp_in", t(4), otlp),
        TraceStep::Advance(t(15)),
        TraceStep::Admit("otlp_in", t(301), vec![gauge_at("checkout.cpu", 301, 0.5)]),
        TraceStep::Admit("dsd_in", t(302), vec![late_dogstatsd]),
    ]
}

fn trace(plan: Plan, events: &[TraceStep]) -> Trace {
    let mut trace = Trace::new().install(plan);
    for event in events {
        trace = match event {
            TraceStep::Admit(source, at, metrics) => trace.admit(id(source), *at, metrics.clone()),
            TraceStep::Advance(now) => trace.advance(*now),
        };
    }
    trace
}

#[test]
fn compiled_programs_deliver_what_the_interpreter_delivers() {
    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let run = Interpreter::new(&registry, &catalog)
            .run(&trace(plan.clone(), &events(timestamped)))
            .unwrap();

        // Kernels with caches need a Tokio runtime to be built in, as the interpreter's are.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut executor = Executor::new(&plan);
        executor.run(events(timestamped));

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
