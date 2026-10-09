//! What a region records about the nodes it runs, and the handles it publishes to.
//!
//! A region counts in plain integers while it processes batches: events per output port of every kernel and decision,
//! and points lost per node and loss class. It adds them to shared counters only when the region host asks it to
//! publish, off the processing path of any batch. The counters are registered beforehand, by whoever creates the
//! region or prepares a generation in it, through [`RegisterTelemetry`]: a region never registers telemetry itself.

use saluki_core::execution::{LossClass, LossCount, TurnCounters};
use saluki_metrics::{Counter, MetricsBuilder};

use crate::{
    compile::{DecisionKind, RegionProgram, Route, Step},
    explain::loss_class_name,
    ir::{NodeId, PortName},
    operators::names,
};

/// The name of the counter of events leaving each output port of each node a region runs, tagged `component_id` (the
/// node) and `port`.
pub const PLAN_NODE_EVENTS_TOTAL: &str = "plan_node_events_total";

/// The name of the counter of points lost, tagged `component_id` (the node, or the region for loss outside any node)
/// and `class` (`policy`, `capacity`, `invalid`, or `shutdown`).
pub const PLAN_LOSS_TOTAL: &str = "plan_loss_total";

/// The loss classes, in the order a region keeps their counts.
pub(super) const LOSS_CLASSES: [LossClass; 4] = [
    LossClass::Policy,
    LossClass::Capacity,
    LossClass::Invalid,
    LossClass::Shutdown,
];

/// Registers the counters a region adds to when it publishes its telemetry.
///
/// A region's creator implements this, or uses [`PlanTelemetry`], and passes it where counters are registered: to
/// [`PrepareRegion::new`][super::PrepareRegion::new] for a generation's program and to
/// [`RegionConfig::for_slot`][super::RegionConfig::for_slot] for the region itself. Both run off the data path, such
/// as in the task that builds the topology or prepares a generation.
pub trait RegisterTelemetry {
    /// Returns the counters to add the number of events to that `node`, running `operator`, sends on its output port
    /// `port`.
    ///
    /// Besides the plan-level counter, a host can return counters that keep a native metric's name, such as one that
    /// counts the events a filter drops.
    fn port_events(&self, node: &NodeId, operator: &'static str, port: &PortName) -> Vec<Counter>;

    /// Returns the counter to add the number of points to that `component_id` loses in `class`.
    ///
    /// `component_id` is a node, or the region itself for loss outside any node, such as a batch no program can
    /// process.
    fn loss(&self, component_id: &str, class: LossClass) -> Counter;
}

/// The plan-level telemetry of a region: [`PLAN_NODE_EVENTS_TOTAL`] and [`PLAN_LOSS_TOTAL`], registered with a
/// metrics builder.
#[derive(Clone, Default)]
pub struct PlanTelemetry {
    builder: MetricsBuilder,
}

impl PlanTelemetry {
    /// Creates plan-level telemetry whose counters carry the builder's default tags.
    pub fn new(builder: MetricsBuilder) -> Self {
        Self { builder }
    }
}

impl RegisterTelemetry for PlanTelemetry {
    fn port_events(&self, node: &NodeId, _operator: &'static str, port: &PortName) -> Vec<Counter> {
        vec![self.builder.register_counter_with_tags(
            PLAN_NODE_EVENTS_TOTAL,
            [("component_id", node.to_string()), ("port", port.to_string())],
        )]
    }

    fn loss(&self, component_id: &str, class: LossClass) -> Counter {
        self.builder.register_counter_with_tags(
            PLAN_LOSS_TOTAL,
            [
                ("component_id", component_id.to_string()),
                ("class", loss_class_name(class).to_string()),
            ],
        )
    }
}

/// Registers no counters: everything a region counts is discarded when it publishes.
///
/// For benchmarks and tests that do not observe telemetry.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTelemetry;

impl RegisterTelemetry for NoTelemetry {
    fn port_events(&self, _node: &NodeId, _operator: &'static str, _port: &PortName) -> Vec<Counter> {
        Vec::new()
    }

    fn loss(&self, _component_id: &str, _class: LossClass) -> Counter {
        Counter::noop()
    }
}

/// Where a program's counts live: one place per output port of each kernel and decision, and one per `drop` node.
///
/// Every function that numbers a program's counts derives them from this layout, so the counts a region keeps and the
/// counters registered for them always line up.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    /// The index of each kernel's first port, by kernel index.
    pub kernel_ports: Vec<usize>,
    /// The number of kernel ports.
    pub kernel_port_count: usize,
    /// The index of each decision's first port, by decision index.
    pub decision_ports: Vec<usize>,
    /// The number of decision ports.
    pub decision_port_count: usize,
    /// The `drop` nodes the program's steps and routes count loss for, in the order they first appear.
    pub drops: Vec<NodeId>,
}

impl Layout {
    pub fn of(program: &RegionProgram) -> Self {
        let mut kernel_ports = Vec::with_capacity(program.kernels().len());
        let mut kernel_port_count = 0;
        for kernel in program.kernels() {
            kernel_ports.push(kernel_port_count);
            kernel_port_count += kernel.ports.len();
        }
        let mut decision_ports = Vec::with_capacity(program.decisions().len());
        let mut decision_port_count = 0;
        for decision in program.decisions() {
            decision_ports.push(decision_port_count);
            decision_port_count += decision.ports.len();
        }

        let mut drops = Vec::new();
        let mut note = |node: &NodeId| {
            if !drops.contains(node) {
                drops.push(node.clone());
            }
        };
        for entry in program.entries() {
            for step in &entry.steps {
                match step {
                    Step::Drop { node, .. } => note(node),
                    Step::Expand { ports: routes, .. } | Step::Decide { targets: routes, .. } => {
                        for route in routes {
                            if let Route::Drop(node) = route {
                                note(node);
                            }
                        }
                    }
                    Step::Map { .. } | Step::Concat { .. } | Step::Exit { .. } => {}
                }
            }
        }

        Self {
            kernel_ports,
            kernel_port_count,
            decision_ports,
            decision_port_count,
            drops,
        }
    }

    /// Returns the index of a `drop` node's counts.
    pub fn drop_index(&self, node: &NodeId) -> Option<usize> {
        self.drops.iter().position(|drop| drop == node)
    }
}

/// The counters one generation's program publishes to, registered off the data path.
pub(super) struct ProgramTelemetry {
    kernel_ports: Vec<Vec<Counter>>,
    decision_ports: Vec<Vec<Counter>>,
    drops: Vec<Counter>,
    kernel_loss: Vec<[Counter; 4]>,
}

impl ProgramTelemetry {
    pub fn register(program: &RegionProgram, layout: &Layout, telemetry: &dyn RegisterTelemetry) -> Self {
        let mut kernel_ports = Vec::with_capacity(layout.kernel_port_count);
        let mut kernel_loss = Vec::with_capacity(program.kernels().len());
        for kernel in program.kernels() {
            for port in &kernel.ports {
                kernel_ports.push(telemetry.port_events(&kernel.key.node, kernel.operator, port));
            }
            let node = kernel.key.node.to_string();
            kernel_loss.push(LOSS_CLASSES.map(|class| telemetry.loss(&node, class)));
        }
        let mut decision_ports = Vec::with_capacity(layout.decision_port_count);
        for decision in program.decisions() {
            let operator = match decision.kind {
                DecisionKind::Split { .. } => names::SPLIT,
                DecisionKind::Tee => names::TEE,
            };
            for port in &decision.ports {
                decision_ports.push(telemetry.port_events(&decision.node, operator, port));
            }
        }
        let drops = layout
            .drops
            .iter()
            .map(|node| telemetry.loss(&node.to_string(), LossClass::Policy))
            .collect();
        Self {
            kernel_ports,
            decision_ports,
            drops,
            kernel_loss,
        }
    }

    /// Adds `counts` to the counters and resets them.
    pub fn publish(&self, counts: &mut ProgramCounts) {
        publish_events(&self.kernel_ports, &mut counts.kernel_ports);
        publish_events(&self.decision_ports, &mut counts.decision_ports);
        for (counter, loss) in self.drops.iter().zip(counts.drops.iter_mut()) {
            if loss.points > 0 {
                counter.increment(loss.points);
            }
            *loss = LossCount::default();
        }
        for (counters, loss) in self.kernel_loss.iter().zip(counts.kernel_loss.iter_mut()) {
            for (counter, class) in counters.iter().zip(LOSS_CLASSES) {
                let points = loss.loss(class).points;
                if points > 0 {
                    counter.increment(points);
                }
            }
            *loss = TurnCounters::default();
        }
    }
}

fn publish_events(counters: &[Vec<Counter>], counts: &mut [u64]) {
    for (counters, count) in counters.iter().zip(counts.iter_mut()) {
        if *count > 0 {
            for counter in counters {
                counter.increment(*count);
            }
            *count = 0;
        }
    }
}

/// What one generation's program counted since the region last published, in plain integers.
#[derive(Clone, Debug)]
pub(super) struct ProgramCounts {
    /// Events per kernel port, by the layout's kernel port index.
    pub kernel_ports: Vec<u64>,
    /// Events per decision port, by the layout's decision port index.
    pub decision_ports: Vec<u64>,
    /// Policy loss per `drop` node, by the layout's drop index.
    pub drops: Vec<LossCount>,
    /// Loss each kernel reported, by kernel index.
    pub kernel_loss: Vec<TurnCounters>,
}

impl ProgramCounts {
    pub fn new(layout: &Layout) -> Self {
        Self {
            kernel_ports: vec![0; layout.kernel_port_count],
            decision_ports: vec![0; layout.decision_port_count],
            drops: vec![LossCount::default(); layout.drops.len()],
            kernel_loss: vec![TurnCounters::default(); layout.kernel_ports.len()],
        }
    }
}

/// Adds a loss of `events` events carrying `points` points to `count`.
pub(super) fn add_loss(count: &mut LossCount, events: u64, points: u64) {
    count.events = count.events.saturating_add(events);
    count.points = count.points.saturating_add(points);
}
