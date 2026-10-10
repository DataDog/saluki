//! What a region records about the nodes it runs, and the handles it publishes to.
//!
//! A region counts in plain integers while it processes batches: events per output port of every kernel, events per
//! exit set of every decision, from which it derives each decision node's per-port counts, and points lost per node and
//! loss class. It adds them to shared counters only when the region host asks it to publish, off the processing path
//! of any batch. The counters are registered beforehand, by whoever creates the
//! region or prepares a generation in it, through [`RegisterTelemetry`]: a region never registers telemetry itself.

use saluki_core::execution::{LossClass, LossCount, TurnCounters};
use saluki_metrics::{Counter, Gauge, MetricsBuilder};

use crate::{
    compile::{RegionProgram, Route, Step},
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

/// The name of the gauge of how far an isolation slot's watermark lags its wall clock, in seconds, when the region
/// publishes, tagged `component_id` (the region).
pub const PLAN_STATE_WATERMARK_LAG_SECONDS: &str = "plan_state_watermark_lag_seconds";

/// The name of the gauge of the lineages an isolation slot holds: instances of its state owners' state that have not
/// retired, tagged `component_id` (the region).
pub const PLAN_STATE_LINEAGES: &str = "plan_state_lineages";

/// The name of the counter of clock resets an isolation slot ran, tagged `component_id` (the region).
pub const PLAN_CLOCK_RESETS_TOTAL: &str = "plan_clock_resets_total";

/// The name of the counter of flush ticks an isolation slot fired once their grace ended, at the watermark rather than
/// at the tick's wall-clock second, tagged `component_id` (the region).
pub const PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL: &str = "plan_state_tick_grace_expired_total";

/// The handles an isolation slot publishes its own telemetry to: its watermark lag, its lineages, its clock resets, and
/// its flush ticks whose grace ended.
#[derive(Clone, Debug)]
pub struct StateTelemetry {
    /// [`PLAN_STATE_WATERMARK_LAG_SECONDS`].
    pub watermark_lag: Gauge,

    /// [`PLAN_STATE_LINEAGES`].
    pub lineages: Gauge,

    /// [`PLAN_CLOCK_RESETS_TOTAL`].
    pub clock_resets: Counter,

    /// [`PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL`].
    pub tick_grace_expired: Counter,
}

impl StateTelemetry {
    /// Returns handles that record nothing.
    pub fn noop() -> Self {
        Self {
            watermark_lag: Gauge::noop(),
            lineages: Gauge::noop(),
            clock_resets: Counter::noop(),
            tick_grace_expired: Counter::noop(),
        }
    }
}

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

    /// Returns the handles the isolation slot `region` publishes its own telemetry to; see [`StateTelemetry`].
    ///
    /// The default records nothing.
    fn state(&self, _region: &str) -> StateTelemetry {
        StateTelemetry::noop()
    }
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

    fn state(&self, region: &str) -> StateTelemetry {
        let tags = || [("component_id", region.to_string())];
        StateTelemetry {
            watermark_lag: self
                .builder
                .register_gauge_with_tags(PLAN_STATE_WATERMARK_LAG_SECONDS, tags()),
            lineages: self.builder.register_gauge_with_tags(PLAN_STATE_LINEAGES, tags()),
            clock_resets: self.builder.register_counter_with_tags(PLAN_CLOCK_RESETS_TOTAL, tags()),
            tick_grace_expired: self
                .builder
                .register_counter_with_tags(PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL, tags()),
        }
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
    /// The index of each decision's first port, by decision index: the ports of its nodes, node by node.
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
            decision_port_count += decision.port_count();
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
                    Step::Map { .. } | Step::Concat { .. } | Step::Exit { .. } | Step::State { .. } => {}
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
    /// The series of the filter R5b pushed into each kernel, by kernel index.
    pushed: Vec<Option<PushedTelemetry>>,
}

/// The series of a filter R5b pushed into a state owner, which count the records the state owner suppresses at
/// emission as the filter would have counted them: the records on the filter's port to its `drop`, and their points
/// as the `drop`'s policy loss. Equal labels name the same series in every region, so the filter's series are what
/// they would be without the rewrite.
struct PushedTelemetry {
    records: Vec<Counter>,
    points: Counter,
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
            for node in &decision.nodes {
                for port in &node.ports {
                    decision_ports.push(telemetry.port_events(&node.node, node.operator(), port));
                }
            }
        }
        let drops = layout
            .drops
            .iter()
            .map(|node| telemetry.loss(&node.to_string(), LossClass::Policy))
            .collect();
        let mut pushed = (0..program.kernels().len()).map(|_| None).collect::<Vec<_>>();
        for entry in program.entries() {
            for step in &entry.steps {
                let Step::State { kernel, params, .. } = step else {
                    continue;
                };
                let Some(site) = program.state_param(*params).and_then(|entry| entry.pushed.as_ref()) else {
                    continue;
                };
                if let Some(slot) = pushed.get_mut(kernel.get()) {
                    *slot = Some(PushedTelemetry {
                        records: telemetry.port_events(&site.filter, names::SPLIT, &site.drop_port),
                        points: telemetry.loss(&site.drop.to_string(), LossClass::Policy),
                    });
                }
            }
        }
        Self {
            kernel_ports,
            decision_ports,
            drops,
            kernel_loss,
            pushed,
        }
    }

    /// Returns `true` if R5b pushed a filter into the kernel at `kernel`, whose suppressed records it counts.
    pub fn has_pushed_filter(&self, kernel: usize) -> bool {
        self.pushed.get(kernel).is_some_and(Option::is_some)
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
        for (pushed, suppressed) in self.pushed.iter().zip(counts.pushed.iter_mut()) {
            if let Some(pushed) = pushed {
                if suppressed.events > 0 {
                    for counter in &pushed.records {
                        counter.increment(suppressed.events);
                    }
                }
                if suppressed.points > 0 {
                    pushed.points.increment(suppressed.points);
                }
            }
            *suppressed = LossCount::default();
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
    /// Events per decision port, by the layout's decision port index, as derived from the decisions' counts per exit
    /// set.
    pub decision_ports: Vec<u64>,
    /// Policy loss per `drop` node, by the layout's drop index.
    pub drops: Vec<LossCount>,
    /// Loss each kernel reported, by kernel index.
    pub kernel_loss: Vec<TurnCounters>,
    /// The records, as events, and their points, that each kernel suppressed at emission for the filter R5b pushed into
    /// it, by kernel index.
    pub pushed: Vec<LossCount>,
}

impl ProgramCounts {
    pub fn new(layout: &Layout) -> Self {
        Self {
            kernel_ports: vec![0; layout.kernel_port_count],
            decision_ports: vec![0; layout.decision_port_count],
            drops: vec![LossCount::default(); layout.drops.len()],
            kernel_loss: vec![TurnCounters::default(); layout.kernel_ports.len()],
            pushed: vec![LossCount::default(); layout.kernel_ports.len()],
        }
    }
}

/// Adds a loss of `events` events carrying `points` points to `count`.
pub(super) fn add_loss(count: &mut LossCount, events: u64, points: u64) {
    count.events = count.events.saturating_add(events);
    count.points = count.points.saturating_add(points);
}
