//! Telemetry of the plan-driven metrics path, under the native identities of the nodes it runs.
//!
//! Regions and the installer register their metrics here, off the data path:
//!
//! - [`RegionTelemetry`] is what regions publish to: the plan-level counters, `plan_node_events_total` and
//!   `plan_loss_total`, and the native metrics that a node's [telemetry profile](super::telemetry_profiles) says the
//!   region host counts on a port of the node (`MetricSource::PortEvents`). It also names the component each kernel
//!   reports its own telemetry as, and the resource group each kernel's memory and CPU time are accounted to:
//!   `<region>.<node>`, under the region's own, as a native component's sub-transforms are. A state owner in an
//!   isolation slot named after it, as DogStatsD aggregation runs in `dsd_agg`, stands for the native component itself:
//!   its kernel is accounted in the region's own group, and draws on the host's budgets of state.
//! - [`InstallerTelemetry`] is what the installer publishes: the active generation, the active artifact's revision,
//!   the live generations, the installations by outcome, the cut installations by how they ended and the refused
//!   cancellations of cuts, and the rewrites applied in compiling the active generation, and the native metrics that
//!   telemetry profiles say the installer records from the plan a generation runs (`MetricSource::BindingLength`,
//!   `MatcherEntries`, `TagProjectionRules`, and `BindingChanges`).

use std::collections::BTreeMap;

use saluki_core::{
    accounting::ComponentRegistry,
    components::ComponentContext,
    execution::{GenerationId, LossClass},
    observability::ComponentMetricsExt as _,
    support::SubsystemIdentifier,
    topology::ComponentId,
};
use saluki_metric_kernels::{
    matcher::SortedSetMatcher,
    tag_projection::{TagProjectionRules, ValueRules},
};
use saluki_metric_plan::{
    compile::{Frame, RegionProgram, RewriteId, RewriteRecord, SlotId},
    ir::{BindingName, Namespace, Node, NodeId, Plan, PortName},
    operators::{decode_tag_filter_rules, decode_tag_value_rules, KernelBuildContext, OperatorRegistry},
    runtime::{
        kernel_accounting_name, kernel_accounting_path, PlanTelemetry, RegisterTelemetry, StateBudgets, StateTelemetry,
    },
};
use saluki_metrics::{Counter, Gauge, MetricsBuilder};
use serde_json::Value;

use super::{
    bindings,
    telemetry_profiles::{native_telemetry, MetricSource, NativeMetric, NativeMetricKind, NativeTelemetry},
};

/// The name of the gauge of the active generation's identifier.
pub const PLAN_ACTIVE_GENERATION: &str = "plan_active_generation";

/// The name of the gauge of the active artifact's revision: `0` while the default program runs.
pub const PLAN_ACTIVE_REVISION: &str = "plan_active_revision";

/// The name of the gauge of the generations alive: active, kept, preparing, and draining.
pub const PLAN_LIVE_GENERATIONS: &str = "plan_live_generations";

/// The name of the counter of installations, tagged `outcome`.
pub const PLAN_INSTALLS_TOTAL: &str = "plan_installs_total";

/// The name of the gauge of the rewrites applied in compiling the active generation, tagged `rewrite`.
pub const PLAN_REWRITES_APPLIED: &str = "plan_rewrites_applied";

/// The name of the counter of cut installations by how they ended, tagged `outcome`: each cut is counted once, when it
/// ends, so the counts add up to the cuts that ended.
pub const PLAN_CUTS_TOTAL: &str = "plan_cuts_total";

/// The name of the counter of cancellations of a cut that were refused, because an admission point had stamped the
/// cut's new generation or the locks were not free in time. A refusal ends nothing: the cut then completes, and
/// `plan_cuts_total` counts it as such.
pub const PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL: &str = "plan_cut_cancellations_refused_total";

/// How a cut installation ended, as `plan_cuts_total` counts it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutOutcome {
    /// Every affected admission point passed the cut, and the schedule holds the new generation alone.
    Completed,

    /// The cut was cancelled before any admission point stamped its new generation.
    Cancelled,

    /// The cut's generation was given up before the cut was ever armed: arming was put off too many times, the cut
    /// could not be armed, or a newer proposal replaced it first.
    NotArmed,
}

impl CutOutcome {
    /// Every outcome, in the order the installer registers their counters.
    pub const ALL: [Self; 3] = [Self::Completed, Self::Cancelled, Self::NotArmed];

    /// Returns the outcome's tag value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::NotArmed => "not_armed",
        }
    }
}

/// What became of a candidate plan, as `plan_installs_total` counts it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    /// The candidate's generation became active.
    Committed,

    /// The candidate equals the plan that is active or being prepared: nothing was installed.
    Unchanged,

    /// A newer candidate replaced the candidate before it was installed.
    Superseded,

    /// The candidate's preparation failed, and it is tried again after a delay.
    Retried,

    /// The candidate was rejected, or the configuration change could not be lowered.
    Rejected,
}

impl InstallOutcome {
    /// Returns the outcome's tag value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Unchanged => "unchanged",
            Self::Superseded => "superseded",
            Self::Retried => "retried",
            Self::Rejected => "rejected",
        }
    }
}

/// Returns the component context a native identity reports as, in the given topology.
fn native_context(topology: &SubsystemIdentifier, native: &NativeTelemetry) -> Option<ComponentContext> {
    let component_id = ComponentId::try_from(native.component_id.as_str()).ok()?;
    Some(ComponentContext::new(topology, component_id, native.component_type))
}

/// Returns the metrics builder of a native identity, with its `component_id` and `component_type` labels.
fn native_builder(topology: &SubsystemIdentifier, native: &NativeTelemetry) -> Option<MetricsBuilder> {
    native_context(topology, native).map(|context| MetricsBuilder::from_component_context(&context))
}

/// Where the kernels of the regions are accounted: the component registry, and each region's identity, in slot order.
#[derive(Clone)]
struct Accounting {
    registry: ComponentRegistry,
    regions: Vec<SubsystemIdentifier>,
}

/// What the regions of the plan-driven topology publish to, the identities their kernels report as, and where their
/// kernels are accounted.
#[derive(Clone)]
pub struct RegionTelemetry {
    plan: PlanTelemetry,
    topology: SubsystemIdentifier,
    accounting: Option<Accounting>,
    state_budgets: StateBudgets,
    /// The isolation slots of the frame, by slot, with their region's name.
    isolation: BTreeMap<SlotId, String>,
}

impl RegionTelemetry {
    /// Creates the telemetry of the regions of the topology with the identifier `topology`.
    pub fn new(topology: SubsystemIdentifier) -> Self {
        Self {
            plan: PlanTelemetry::new(MetricsBuilder::default()),
            topology,
            accounting: None,
            state_budgets: StateBudgets::unlimited(),
            isolation: BTreeMap::new(),
        }
    }

    /// Accounts each kernel's memory and CPU time to a resource group of `registry` under its region: `regions` are
    /// the identities of the regions, in slot order.
    pub fn with_accounting(mut self, registry: ComponentRegistry, regions: Vec<SubsystemIdentifier>) -> Self {
        self.accounting = Some(Accounting { registry, regions });
        self
    }

    /// Builds the kernels of the isolation slots of `frame` with the budgets of state of `budgets`, which every lineage
    /// of every state owner shares, and accounts each in its region's own group when the region is named after it.
    pub fn with_state(mut self, budgets: StateBudgets, frame: &Frame) -> Self {
        self.state_budgets = budgets;
        self.isolation = frame
            .slots()
            .iter()
            .filter(|slot| slot.is_isolation())
            .map(|slot| (slot.id, slot.name.to_string()))
            .collect();
        self
    }

    /// Returns the component a node running `operator` reports its telemetry as: the native component it stands for,
    /// if its telemetry profile names one, and otherwise a transform named after the node (`<local>` for a reserved
    /// node, `policy_<local>` for a node in the policy namespace).
    pub fn component(&self, node: &NodeId, operator: &str) -> ComponentContext {
        if let Some(context) =
            native_telemetry(node, operator).and_then(|native| native_context(&self.topology, &native))
        {
            return context;
        }
        let name = match node.namespace() {
            Namespace::Reserved => node.local().to_string(),
            Namespace::Policy => format!("policy_{}", node.local()),
        };
        // Local identifiers start with a letter and never end with an underscore, so they are valid component IDs.
        let component_id = ComponentId::try_from(name.as_str()).expect("node identifiers are valid component IDs");
        ComponentContext::transform(&self.topology, component_id)
    }

    /// Returns the context to build the kernel of `node`, of `program`, in: reported as [`component`][Self::component]
    /// says, and accounted to the resource group `<region>.<node>` (`<region>.policy` for a node in the policy
    /// namespace), whose bounds `RegionBounds` declares under the same name.
    ///
    /// A kernel of an isolation slot draws on the budgets of state ([`with_state`][Self::with_state]): the global
    /// budget and its node's. If the slot is named after the kernel's node, as `dsd_agg` is after DogStatsD
    /// aggregation, the region stands for the node's native component, and the kernel is accounted in the region's own
    /// group (see `kernel_accounting_path`).
    pub fn kernel_context(&self, program: &RegionProgram, node: &NodeId) -> KernelBuildContext {
        let operator = program
            .kernels()
            .iter()
            .find(|kernel| &kernel.key.node == node)
            .map_or("", |kernel| kernel.operator);
        let context = KernelBuildContext::new(node.clone(), self.component(node, operator));
        let isolation = self.isolation.get(&program.slot());
        let context = match isolation {
            Some(_) => context.with_state_budget(self.state_budgets.context_budget(node)),
            None => context,
        };
        let group = self.accounting.as_ref().and_then(|accounting| {
            let region = accounting.regions.get(program.slot().get())?.clone();
            let name = match isolation {
                Some(region_name) => kernel_accounting_path(region_name, node),
                None => Some(kernel_accounting_name(node)),
            };
            let group = match name {
                Some(name) => region.child(name),
                None => region,
            };
            Some(accounting.registry.get_resource_group_token(&group))
        });
        match group {
            Some(token) => context.with_resource_group(token),
            None => context,
        }
    }
}

impl RegisterTelemetry for RegionTelemetry {
    fn port_events(&self, node: &NodeId, operator: &'static str, port: &PortName) -> Vec<Counter> {
        let mut counters = self.plan.port_events(node, operator, port);
        let Some(native) = native_telemetry(node, operator) else {
            return counters;
        };
        let Some(builder) = native_builder(&self.topology, &native) else {
            return counters;
        };
        for metric in native.profile.metrics() {
            if matches!(metric.source, MetricSource::PortEvents(counted) if counted == port.as_str()) {
                counters.push(builder.register_counter_with_tags(metric.name, metric.tags.iter().copied()));
            }
        }
        counters
    }

    fn loss(&self, component_id: &str, class: LossClass) -> Counter {
        self.plan.loss(component_id, class)
    }

    fn state(&self, region: &str) -> StateTelemetry {
        self.plan.state(region)
    }
}

/// The identity of one native metric the installer records: its name, and its labels and tags.
type NativeKey = (&'static str, String, &'static str, &'static [&'static str]);

/// What the installer publishes: plan-level gauges and counters, and the native metrics it records from bindings.
///
/// It keeps a handle to every metric it registers, as a native component does, so that an idle metric is still
/// reported.
pub struct InstallerTelemetry {
    topology: SubsystemIdentifier,
    active_generation: Gauge,
    active_revision: Gauge,
    live_generations: Gauge,
    installs: Vec<(InstallOutcome, Counter)>,
    cuts: Vec<(CutOutcome, Counter)>,
    cut_cancellations_refused: Counter,
    rewrites: Vec<(RewriteId, Gauge)>,
    native_gauges: BTreeMap<NativeKey, Gauge>,
    native_counters: BTreeMap<NativeKey, Counter>,
}

impl InstallerTelemetry {
    /// Creates the installer's telemetry, for the topology with the identifier `topology`.
    pub fn new(topology: SubsystemIdentifier) -> Self {
        let builder = MetricsBuilder::default();
        let installs = [
            InstallOutcome::Committed,
            InstallOutcome::Unchanged,
            InstallOutcome::Superseded,
            InstallOutcome::Retried,
            InstallOutcome::Rejected,
        ]
        .into_iter()
        .map(|outcome| {
            let counter = builder.register_counter_with_tags(PLAN_INSTALLS_TOTAL, [("outcome", outcome.as_str())]);
            (outcome, counter)
        })
        .collect();
        let cuts = CutOutcome::ALL
            .into_iter()
            .map(|outcome| {
                let counter = builder.register_counter_with_tags(PLAN_CUTS_TOTAL, [("outcome", outcome.as_str())]);
                (outcome, counter)
            })
            .collect();
        Self {
            active_generation: builder.register_gauge(PLAN_ACTIVE_GENERATION),
            active_revision: builder.register_gauge(PLAN_ACTIVE_REVISION),
            live_generations: builder.register_gauge(PLAN_LIVE_GENERATIONS),
            installs,
            cuts,
            cut_cancellations_refused: builder.register_counter(PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL),
            rewrites: Vec::new(),
            topology,
            native_gauges: BTreeMap::new(),
            native_counters: BTreeMap::new(),
        }
    }

    /// Counts a candidate's outcome.
    pub fn installed(&self, outcome: InstallOutcome) {
        if let Some((_, counter)) = self.installs.iter().find(|(counted, _)| *counted == outcome) {
            counter.increment(1);
        }
    }

    /// Counts how a cut installation ended. Call it once per cut.
    pub fn cut(&self, outcome: CutOutcome) {
        if let Some((_, counter)) = self.cuts.iter().find(|(counted, _)| *counted == outcome) {
            counter.increment(1);
        }
    }

    /// Counts a cancellation of a cut that was refused: the cut continues.
    pub fn cut_cancellation_refused(&self) {
        self.cut_cancellations_refused.increment(1);
    }

    /// Sets the active generation and the number of live generations.
    pub fn generations(&self, active: Option<GenerationId>, live: usize) {
        // Generation identifiers are small: one per installation.
        self.active_generation
            .set(active.map_or(0.0, |generation| generation.get() as f64));
        self.live_generations.set(live as f64);
    }

    /// Sets the number of applications of each rewrite among `records`, the rewrites compilation considered for the
    /// active generation (none without one); skipped applications are not counted.
    ///
    /// The first call registers a gauge for every rewrite ([`RewriteId::ALL`]), so the installer reports them all from
    /// boot, where it first calls this, and keeps reporting those it no longer applies, at zero.
    pub fn rewrites(&mut self, records: Option<&[RewriteRecord]>) {
        if self.rewrites.is_empty() {
            let builder = MetricsBuilder::default();
            self.rewrites = RewriteId::ALL
                .into_iter()
                .map(|rewrite| {
                    let gauge =
                        builder.register_gauge_with_tags(PLAN_REWRITES_APPLIED, [("rewrite", rewrite.as_str())]);
                    (rewrite, gauge)
                })
                .collect();
        }
        let records = records.unwrap_or_default();
        for (rewrite, gauge) in &self.rewrites {
            let applied = records
                .iter()
                .filter(|record| record.id == *rewrite && record.is_applied())
                .count();
            gauge.set(applied as f64);
        }
    }

    /// Sets the revision of the active artifact, or `None` while the default program runs.
    pub fn active_revision(&self, revision: Option<u64>) {
        // Revisions are safe integers: the IR keeps every number within 2^53.
        self.active_revision
            .set(revision.map_or(0.0, |revision| revision as f64));
    }

    /// Sets the native gauges the installer records from the plan of a generation, as the generation is installed:
    /// the length of a bound list (`BindingLength`), the entries a name matcher keeps for it (`MatcherEntries`), and
    /// the rules a tag filterlist puts in effect (`TagProjectionRules`).
    pub fn set_binding_gauges(&mut self, plan: &Plan) {
        for node in &plan.nodes {
            let Some(native) = native_telemetry(&node.id, node.op.as_str()) else {
                continue;
            };
            for metric in native.profile.metrics() {
                let value = match metric.source {
                    MetricSource::BindingLength(binding) => {
                        bound_list(node, |name| name == binding).map(|list| list.len())
                    }
                    MetricSource::MatcherEntries { binding, prefixes } => {
                        bound_list(node, |name| instance_of(name, binding)).map(|list| {
                            let entries = list.iter().filter_map(Value::as_str);
                            SortedSetMatcher::new(entries, prefixes).len()
                        })
                    }
                    MetricSource::TagProjectionRules => tag_projection_rule_count(node),
                    _ => None,
                };
                if let (Some(value), NativeMetricKind::Gauge) = (value, metric.kind) {
                    if let Some(gauge) = self.native_gauge(&native, metric) {
                        gauge.set(value as f64);
                    }
                }
            }
        }
    }

    /// Registers, at zero, every counter of configuration changes (`BindingChanges`) the nodes of the boot plan keep,
    /// as the native components register theirs when they are built: the series are reported before the first change.
    pub fn register_change_counters(&mut self, plan: &Plan) {
        for node in &plan.nodes {
            let Some(native) = native_telemetry(&node.id, node.op.as_str()) else {
                continue;
            };
            for metric in native.profile.metrics() {
                if matches!(metric.source, MetricSource::BindingChanges(_)) && metric.kind == NativeMetricKind::Counter
                {
                    self.native_counter(&native, metric);
                }
            }
        }
    }

    /// Counts a configuration change, after which `current` was lowered where `previous` was before: for each node of
    /// `current` whose profile counts changes of some bindings (`BindingChanges`), once if the change changed the
    /// value of any of them.
    pub fn count_binding_changes(&mut self, previous: &Plan, current: &Plan) {
        for node in &current.nodes {
            let Some(native) = native_telemetry(&node.id, node.op.as_str()) else {
                continue;
            };
            let before = previous.node(&node.id).map(bound_values).unwrap_or_default();
            let after = bound_values(node);
            for metric in native.profile.metrics() {
                let MetricSource::BindingChanges(watched) = metric.source else {
                    continue;
                };
                let changed = watched
                    .iter()
                    .any(|binding| before.get(*binding) != after.get(*binding));
                if changed && metric.kind == NativeMetricKind::Counter {
                    if let Some(counter) = self.native_counter(&native, metric) {
                        counter.increment(1);
                    }
                }
            }
        }
    }

    fn native_key(native: &NativeTelemetry, metric: &'static NativeMetric) -> NativeKey {
        (
            metric.name,
            native.component_id.clone(),
            native.component_type.as_str(),
            metric.tags,
        )
    }

    /// Returns the gauge of a native metric under a node's native identity, registering it the first time.
    fn native_gauge(&mut self, native: &NativeTelemetry, metric: &'static NativeMetric) -> Option<&Gauge> {
        let key = Self::native_key(native, metric);
        if !self.native_gauges.contains_key(&key) {
            let builder = native_builder(&self.topology, native)?;
            let gauge = builder.register_gauge_with_tags(metric.name, metric.tags.iter().copied());
            self.native_gauges.insert(key.clone(), gauge);
        }
        self.native_gauges.get(&key)
    }

    /// Returns the counter of a native metric under a node's native identity, registering it the first time.
    fn native_counter(&mut self, native: &NativeTelemetry, metric: &'static NativeMetric) -> Option<&Counter> {
        let key = Self::native_key(native, metric);
        if !self.native_counters.contains_key(&key) {
            let builder = native_builder(&self.topology, native)?;
            let counter = builder.register_counter_with_tags(metric.name, metric.tags.iter().copied());
            self.native_counters.insert(key.clone(), counter);
        }
        self.native_counters.get(&key)
    }
}

/// Returns the number of rules a `tag_projection` node's resolved parameters put in effect, as its kernel counts them,
/// or `None` if its parameters are not resolved or not valid.
fn tag_projection_rule_count(node: &Node) -> Option<usize> {
    let operator = OperatorRegistry::native()
        .configure(node.op.as_str(), &node.params)
        .ok()?;
    let rules = decode_tag_filter_rules(operator.params().get("rules")?)?;
    let value_rules = decode_tag_value_rules(operator.params().get("value_rules")?)?;
    let value_rules = ValueRules::new(&value_rules).ok()?;
    Some(TagProjectionRules::new(&rules, value_rules).rule_count())
}

/// Returns the resolved value of every binding a node's parameters name, by binding name.
fn bound_values(node: &Node) -> BTreeMap<String, Value> {
    fn visit(value: &Value, found: &mut BTreeMap<String, Value>) {
        match value {
            Value::Object(members) => match (members.get("config"), members.get("resolved")) {
                (Some(Value::String(binding)), Some(resolved)) => {
                    found.insert(binding.clone(), resolved.clone());
                }
                _ => members.values().for_each(|value| visit(value, found)),
            },
            Value::Array(values) => values.iter().for_each(|value| visit(value, found)),
            _ => {}
        }
    }
    let mut found = BTreeMap::new();
    node.params.iter().for_each(|(_, value)| visit(value, &mut found));
    found
}

/// Returns the resolved list of the first binding of a node's parameters whose name `matches`.
fn bound_list(node: &Node, matches: impl Fn(&str) -> bool) -> Option<Vec<Value>> {
    bound_values(node)
        .into_iter()
        .find(|(name, _)| matches(name))
        .and_then(|(_, value)| match value {
            Value::Array(values) => Some(values),
            _ => None,
        })
}

/// Returns `true` if the binding `name` is an instance of the registry binding `pattern`, which may be keyed.
fn instance_of(name: &str, pattern: &str) -> bool {
    name.parse::<BindingName>()
        .ok()
        .and_then(|name| bindings::lookup(&name))
        .is_some_and(|(spec, _)| spec.name == pattern)
}

#[cfg(test)]
mod tests;
