//! Programs: a validated plan prepared for the interpreter, with its operators configured and its graph resolved.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, GenerationRef, Kernel};
use saluki_metric_kernels::aggregate::ContextBudget;

use super::{host::OpaqueHostAdapter, InstallRejection, InterpretErrorKind};
use crate::{
    ir::{NodeId, OutputId, ParamValue, Plan, PortName, PortRef},
    model::PortType,
    operators::{
        names, Boundary, ConfiguredOperator, KernelBuildContext, KernelSpec, OperatorRegistry, StateContract,
        StateTime, FALSE_PORT, TRUE_PORT,
    },
    predicate::{BindingResolver, CompiledPredicate},
    validate::ValidationReport,
};

/// One port of one node, by position: `(node, port)`.
pub(super) type Endpoint = (usize, usize);

/// Why a plan could not be prepared.
pub(super) enum PrepareError {
    /// The installation is rejected, and the active plan stays.
    Rejected(InstallRejection),

    /// The plan cannot be interpreted at all.
    Fatal(InterpretErrorKind),
}

/// How the interpreter runs one node.
pub(super) enum Exec {
    /// A source: batches enter here.
    Source,

    /// A `split`, with its predicate compiled and the positions of its `true` and `false` ports.
    Split {
        predicate: CompiledPredicate,
        true_port: usize,
        false_port: usize,
    },

    /// A `tee`.
    Tee,

    /// A `merge`.
    Merge,

    /// An `emit` to the output.
    Emit(OutputId),

    /// A `drop`.
    Drop,

    /// An operator with a kernel. The instance is missing only while an installation is being prepared.
    Kernel {
        spec: KernelSpec,
        kernel: Option<Box<dyn Kernel>>,
    },

    /// An opaque host. The adapter is missing only while an installation is being prepared.
    Host(Option<Box<dyn OpaqueHostAdapter>>),

    /// A state owner behind an isolation fence, whose state lives in the run's instances: the index of the instance
    /// this generation binds, missing only while an installation is being prepared, and the per-call parameters its
    /// kernel takes from this generation.
    State {
        spec: KernelSpec,
        instance: Option<usize>,
        step: Option<Arc<dyn Any + Send + Sync>>,
    },
}

/// One node of a program.
pub(super) struct ProgramNode {
    pub id: NodeId,
    pub op: ConfiguredOperator,
    pub exec: Exec,
}

impl ProgramNode {
    /// Returns `true` if the node keeps state between batches: if it is a state owner.
    pub fn is_stateful(&self) -> bool {
        self.op.contract().state != StateContract::None
    }

    /// Returns `true` if the node runs in a host component.
    pub fn is_host(&self) -> bool {
        matches!(self.exec, Exec::Host(_))
    }

    /// Returns `true` if the node is a state owner behind an isolation fence, whose watermark and flush ticks follow
    /// the shared [time rules][crate::runtime::state::rules].
    pub fn is_isolated(&self) -> bool {
        matches!(self.exec, Exec::State { .. })
    }

    /// Returns `true` if the node keeps state of its own in this program: a state owner that is not isolated, such as
    /// an opaque host.
    pub fn is_local_state(&self) -> bool {
        self.is_stateful() && !self.is_isolated()
    }

    /// Returns the instance an isolated state owner binds, once bound.
    pub fn instance(&self) -> Option<usize> {
        match self.exec {
            Exec::State { instance, .. } => instance,
            _ => None,
        }
    }

    /// Returns the per-call parameters an isolated state owner's kernel takes in this generation.
    pub fn step(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        match &self.exec {
            Exec::State { step, .. } => step.clone(),
            _ => None,
        }
    }

    /// Returns the bucket width of a state owner that buckets by time, if it is known.
    pub fn bucket_width(&self) -> Option<Duration> {
        match &self.op.contract().state {
            StateContract::Keyed(state) => match state.time {
                StateTime::Buckets { width } => width,
            },
            _ => None,
        }
    }
}

/// One stamp of an admission point.
#[derive(Clone, Copy, Debug)]
pub(super) struct Stamp {
    /// The admission time.
    pub admitted_at: AdmissionTime,

    /// The epoch of the point's clock.
    pub epoch: u32,

    /// Whether the stamp reset the clock.
    pub reset: bool,
}

/// The progress of one admission point's clock.
#[derive(Clone, Copy, Debug, Default)]
struct PointClock {
    latest: Option<AdmissionTime>,
    epoch: u32,
}

/// The admission points the interpreter has seen, with each one's clock: its latest admission time and its epoch.
///
/// Admission points belong to the host, not to a generation: a source's output keeps its admission clock across
/// installations, as long as a node with the same ID and port admits there.
pub(super) struct AdmissionPoints {
    ids: BTreeMap<(NodeId, PortName), AdmissionPointId>,
    names: Vec<(NodeId, PortName)>,
    clocks: Vec<PointClock>,
    reset_threshold: Duration,
}

impl AdmissionPoints {
    /// Returns the ID of the admission point at a node's output port, assigning one when it is first seen.
    pub fn id(&mut self, node: &NodeId, port: &PortName) -> AdmissionPointId {
        let key = (node.clone(), port.clone());
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        // Plans have at most a few thousand ports, so the count fits.
        let id = AdmissionPointId::new(self.names.len() as u32);
        self.ids.insert(key.clone(), id);
        self.names.push(key);
        self.clocks.push(PointClock::default());
        id
    }

    /// Returns every admission point seen, in ID order.
    pub fn ids(&self) -> impl Iterator<Item = AdmissionPointId> + '_ {
        (0..self.clocks.len()).map(|index| AdmissionPointId::new(index as u32))
    }

    /// Returns the latest admission time stamped at a point in its current epoch, if it has stamped any.
    pub fn latest(&self, id: AdmissionPointId) -> Option<AdmissionTime> {
        self.clocks.get(id.get() as usize).and_then(|clock| clock.latest)
    }

    /// Returns the epoch of a point's clock: how many times it reset, wrapping.
    pub fn epoch(&self, id: AdmissionPointId) -> u32 {
        self.clocks.get(id.get() as usize).map_or(0, |clock| clock.epoch)
    }

    /// Returns the largest backward jump of an admission time that a point's clock absorbs: a stamp further back
    /// resets it.
    pub fn reset_threshold(&self) -> Duration {
        self.reset_threshold
    }

    /// Returns the point's name, as `node.port`.
    pub fn name(&self, id: AdmissionPointId) -> String {
        self.names
            .get(id.get() as usize)
            .map_or_else(|| "?".to_string(), |(node, port)| format!("{}.{}", node, port))
    }

    /// Records a stamp the trace states, at a source.
    ///
    /// An admission clock stamps the later of its latest time and the wall clock, so it moves back only when the wall
    /// clock jumps back by more than the reset threshold: a stamp that much earlier than the latest one resets the
    /// clock, which then continues in a new epoch. Returns the latest time if the stamp is earlier by no more than
    /// the threshold, which a clock never stamps: it would have stamped its latest time again.
    pub fn admit(&mut self, id: AdmissionPointId, at: AdmissionTime) -> Result<Stamp, AdmissionTime> {
        let threshold = self.reset_threshold;
        let Some(clock) = self.clocks.get_mut(id.get() as usize) else {
            return Ok(Stamp {
                admitted_at: at,
                epoch: 0,
                reset: false,
            });
        };
        let reset = match clock.latest {
            Some(previous) if at < previous => {
                let jump = Duration::from_nanos(previous.as_unix_nanos() - at.as_unix_nanos());
                if jump <= threshold {
                    return Err(previous);
                }
                true
            }
            _ => false,
        };
        if reset {
            clock.epoch = clock.epoch.wrapping_add(1);
        }
        clock.latest = Some(at);
        Ok(Stamp {
            admitted_at: at,
            epoch: clock.epoch,
            reset,
        })
    }

    /// Stamps at a host's output when its clock reads `now`: the admission clock never moves back.
    pub fn stamp(&mut self, id: AdmissionPointId, now: AdmissionTime) -> AdmissionTime {
        match self.clocks.get_mut(id.get() as usize) {
            Some(clock) => {
                let stamped = clock.latest.map_or(now, |previous| previous.max(now));
                clock.latest = Some(stamped);
                stamped
            }
            None => now,
        }
    }
}

impl Default for AdmissionPoints {
    /// Returns no admission points, whose clocks reset at the limits table's
    /// [`CLOCK_RESET_THRESHOLD`][crate::limits::values::CLOCK_RESET_THRESHOLD].
    fn default() -> Self {
        Self {
            ids: BTreeMap::new(),
            names: Vec::new(),
            clocks: Vec::new(),
            reset_threshold: crate::limits::values::CLOCK_RESET_THRESHOLD,
        }
    }
}

/// A plan prepared for execution in one generation.
pub(super) struct Program {
    /// The generation that batches processed by this program are stamped with.
    pub generation: GenerationRef,

    /// The position in the trace of the installation that produced this program.
    pub install: usize,

    pub nodes: Vec<ProgramNode>,
    pub index: HashMap<NodeId, usize>,

    /// The order a pass runs the nodes in: topological order, choosing among ready nodes in plan order, with the
    /// `emit`s last, in plan order.
    pub order: Vec<usize>,

    /// For each node and output port, the input port its edge enters.
    pub targets: Vec<Vec<Option<Endpoint>>>,

    /// For each node and output port, the admission point it is, for sources and opaque hosts.
    pub points: Vec<Vec<Option<AdmissionPointId>>>,

    /// For each node and output port, the port type validation computed for it.
    pub port_types: Vec<Vec<Option<PortType>>>,

    /// For each state owner, the admission points upstream of it within its segment, in ID order.
    pub upstream: Vec<Vec<AdmissionPointId>>,

    /// The outputs the plan declares.
    pub outputs: Vec<OutputId>,
}

impl Program {
    /// Returns the position of the node that binds an instance of an isolated state owner, if one does.
    pub fn binding(&self, instance: usize) -> Option<usize> {
        self.nodes.iter().position(|node| node.instance() == Some(instance))
    }

    /// Returns the positions of the sources, in plan order.
    pub fn sources(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nodes.len()).filter(|&node| matches!(self.nodes[node].exec, Exec::Source))
    }
}

impl Program {
    /// Prepares a validated plan: configures every operator, decides how each node runs, compiles predicates, builds
    /// the kernels of stateless nodes, and resolves the graph.
    ///
    /// Stateful kernels and opaque hosts are left without instances: the installation either carries them over from
    /// the active program or builds them.
    pub fn prepare<R>(
        plan: &Plan, registry: &OperatorRegistry, report: &ValidationReport, resolver: &R,
        points: &mut AdmissionPoints, install: usize, generation: GenerationRef,
    ) -> Result<Self, PrepareError>
    where
        R: BindingResolver + ?Sized,
    {
        let mut nodes = Vec::with_capacity(plan.nodes.len());
        for node in &plan.nodes {
            let op = registry.configure(node.op.as_str(), &node.params).map_err(|error| {
                PrepareError::Fatal(InterpretErrorKind::Internal {
                    message: format!("validated node `{}` does not configure: {}", node.id, error),
                })
            })?;
            let exec = exec_for(&node.id, &op, resolver)?;
            nodes.push(ProgramNode {
                id: node.id.clone(),
                op,
                exec,
            });
        }

        let index = nodes
            .iter()
            .enumerate()
            .map(|(position, node)| (node.id.clone(), position))
            .collect::<HashMap<_, _>>();
        let mut targets = nodes
            .iter()
            .map(|node| vec![None; node.op.ports().outputs.len()])
            .collect::<Vec<_>>();
        let mut feeders = nodes
            .iter()
            .map(|node| vec![None; node.op.ports().inputs.len()])
            .collect::<Vec<Vec<Option<Endpoint>>>>();
        for edge in &plan.edges {
            let endpoint = |port: &crate::ir::PortRef, input: bool| -> Option<Endpoint> {
                let node = *index.get(&port.node)?;
                let ports = nodes[node].op.ports();
                let position = if input {
                    ports.input(&port.port)?
                } else {
                    ports.output(&port.port)?
                };
                Some((node, position))
            };
            let (Some(from), Some(to)) = (endpoint(&edge.from, false), endpoint(&edge.to, true)) else {
                return Err(PrepareError::Fatal(InterpretErrorKind::Internal {
                    message: format!("validated edge `{}` -> `{}` does not resolve", edge.from, edge.to),
                }));
            };
            targets[from.0][from.1] = Some(to);
            feeders[to.0][to.1] = Some(from);
        }

        let order = topological_order(&targets);
        if order.len() != nodes.len() {
            return Err(PrepareError::Fatal(InterpretErrorKind::Internal {
                message: "a validated plan has a cycle".to_string(),
            }));
        }
        // An `emit` feeds nothing, so running every `emit` last keeps the order topological, and makes the order in
        // which records from different `emit`s reach one output their plan order.
        let is_emit = |node: &usize| matches!(nodes[*node].exec, Exec::Emit(_));
        let order = order
            .iter()
            .copied()
            .filter(|node| !is_emit(node))
            .chain((0..nodes.len()).filter(is_emit))
            .collect::<Vec<_>>();

        let points = nodes
            .iter()
            .map(|node| {
                let admits = matches!(node.exec, Exec::Source | Exec::Host(_));
                node.op
                    .ports()
                    .outputs
                    .iter()
                    .map(|port| admits.then(|| points.id(&node.id, &port.name)))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let port_types = nodes
            .iter()
            .map(|node| {
                node.op
                    .ports()
                    .outputs
                    .iter()
                    .map(|port| report.port_type(&PortRef::new(node.id.clone(), port.name.clone())))
                    .collect()
            })
            .collect();

        let upstream = (0..nodes.len())
            .map(|node| {
                if nodes[node].is_stateful() {
                    upstream_points(node, &feeders, &points)
                } else {
                    Vec::new()
                }
            })
            .collect();

        Ok(Self {
            generation,
            install,
            nodes,
            index,
            order,
            targets,
            points,
            port_types,
            upstream,
            outputs: plan.outputs.iter().map(|output| output.id.clone()).collect(),
        })
    }
}

/// Decides how the interpreter runs a node, and builds what a stateless node needs.
///
/// A `split`'s operands come from the plan's literals and resolved values first, and from `resolver` for bindings the
/// plan leaves unresolved. An isolated state owner whose kernel takes no per-call parameters from this generation is
/// rejected unless its kernel builds from the generation's parameters.
fn exec_for<R>(id: &NodeId, op: &ConfiguredOperator, resolver: &R) -> Result<Exec, PrepareError>
where
    R: BindingResolver + ?Sized,
{
    if let Some(signal) = op.contract().signals.first() {
        return Err(PrepareError::Fatal(InterpretErrorKind::Unsupported {
            capability: format!(
                "running operators that read host signals, as node `{}` reads `{}`,",
                id, signal.name
            ),
            stage: 10,
        }));
    }

    let exec = match op.name() {
        names::SOURCE => Exec::Source,
        names::SPLIT => {
            let Some(predicate) = op.params().predicate("predicate") else {
                return Err(internal(id, "a `split` without a predicate"));
            };
            let predicate = predicate.compile(resolver).map_err(|source| {
                PrepareError::Rejected(InstallRejection::UnresolvedBinding {
                    node: id.clone(),
                    source,
                })
            })?;
            let ports = op.ports();
            let (Some(true_port), Some(false_port)) = (
                ports.output(&PortName::from_static(TRUE_PORT)),
                ports.output(&PortName::from_static(FALSE_PORT)),
            ) else {
                return Err(internal(id, "a `split` without `true` and `false` ports"));
            };
            Exec::Split {
                predicate,
                true_port,
                false_port,
            }
        }
        names::TEE => Exec::Tee,
        names::MERGE => Exec::Merge,
        names::EMIT => match op.params().get("output") {
            Some(ParamValue::Output(output)) => Exec::Emit(output.clone()),
            _ => return Err(internal(id, "an `emit` without an output")),
        },
        names::DROP => Exec::Drop,
        _ if op.contract().boundary == Boundary::OpaqueHost => Exec::Host(None),
        _ if op.contract().boundary == Boundary::Isolated => match op.kernel() {
            Some(spec) => {
                let step = op.step_params(None);
                // Without per-call parameters, a retained lineage runs on the parameters its kernel was built with, so
                // per-call parameters that do not decode, such as a binding without a value, would leave it on another
                // generation's. Whether or not its lineage is retained, the state owner is rejected as a first
                // installation rejects it: by its kernel, which these parameters do not build either.
                if step.is_none() {
                    build_state_kernel(id, &spec, ContextBudget::exclusive())?;
                }
                Exec::State {
                    spec,
                    instance: None,
                    step,
                }
            }
            None => {
                return Err(internal(
                    id,
                    &format!("a `{}` node behind an isolation fence without a kernel", op.name()),
                ))
            }
        },
        _ => match op.kernel() {
            Some(spec) => {
                let kernel = if op.contract().state == StateContract::None {
                    Some(build_kernel(id, &spec)?)
                } else {
                    None
                };
                Exec::Kernel { spec, kernel }
            }
            // Every synchronous native operator has a kernel, so only an operator registered for tests can lack one.
            None => {
                return Err(internal(
                    id,
                    &format!("a `{}` node, a synchronous operator without a kernel", op.name()),
                ))
            }
        },
    };
    Ok(exec)
}

/// Builds a kernel from its spec, outside any topology.
pub(super) fn build_kernel(id: &NodeId, spec: &KernelSpec) -> Result<Box<dyn Kernel>, PrepareError> {
    build_kernel_in(id, spec, KernelBuildContext::detached(id))
}

/// Builds the kernel of an isolated state owner's lineage, drawing its state from `budget`.
pub(super) fn build_state_kernel(
    id: &NodeId, spec: &KernelSpec, budget: ContextBudget,
) -> Result<Box<dyn Kernel>, PrepareError> {
    build_kernel_in(id, spec, KernelBuildContext::detached(id).with_state_budget(budget))
}

fn build_kernel_in(
    id: &NodeId, spec: &KernelSpec, context: KernelBuildContext,
) -> Result<Box<dyn Kernel>, PrepareError> {
    spec.build(&context).map_err(|source| {
        PrepareError::Rejected(InstallRejection::KernelBuild {
            node: id.clone(),
            source,
        })
    })
}

fn internal(id: &NodeId, what: &str) -> PrepareError {
    PrepareError::Fatal(InterpretErrorKind::Internal {
        message: format!("validated node `{}` is {}", id, what),
    })
}

/// Returns the nodes in topological order, choosing among ready nodes in plan order.
fn topological_order(targets: &[Vec<Option<Endpoint>>]) -> Vec<usize> {
    let mut in_degree = vec![0usize; targets.len()];
    for ports in targets {
        for (next, _) in ports.iter().flatten() {
            in_degree[*next] += 1;
        }
    }
    let mut ready = (0..targets.len())
        .filter(|&node| in_degree[node] == 0)
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(targets.len());
    while let Some(node) = ready.pop_first() {
        order.push(node);
        for (next, _) in targets[node].iter().flatten() {
            in_degree[*next] -= 1;
            if in_degree[*next] == 0 {
                ready.insert(*next);
            }
        }
    }
    order
}

/// Returns the admission points upstream of `owner`: every source or opaque host output from which a path reaches it
/// without crossing another opaque host. An opaque host re-admits, so points beyond it belong to another segment.
fn upstream_points(
    owner: usize, feeders: &[Vec<Option<Endpoint>>], points: &[Vec<Option<AdmissionPointId>>],
) -> Vec<AdmissionPointId> {
    let mut found = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending = feeders[owner].iter().flatten().copied().collect::<Vec<_>>();
    while let Some((node, port)) = pending.pop() {
        if let Some(point) = points[node][port] {
            found.insert(point);
        } else if visited.insert(node) {
            pending.extend(feeders[node].iter().flatten().copied());
        }
    }
    found.into_iter().collect()
}
