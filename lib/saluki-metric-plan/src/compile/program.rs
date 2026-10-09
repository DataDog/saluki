//! Region programs: what each slot runs for one generation.
//!
//! A [`RegionProgram`] is plain data plus the [specifications][KernelSpec] of the kernels it runs, so that a region
//! host can build its kernels, rebuild them after a panic, and share them with other generations. It has one
//! [`EntryProgram`] per [entry](super::placement#entries) of the slot: a straight list of [steps][Step] that run a
//! batch arriving at that entry through every node it reaches in the slot.
//!
//! # Batch-at-a-time execution
//!
//! Steps work on **registers**, each holding a sequence of buffers that all carry the input batch's header. Register 0
//! holds the batch the entry received. Each step processes the whole content of its registers before the next step
//! starts, and steps are listed in topological order, so a step never runs before every step that feeds it:
//!
//! - [`Step::Map`] runs a single-port kernel over a register in place. What the kernel leaves in a buffer stays in
//!   that register, followed by anything it writes to its port.
//! - [`Step::Expand`] runs a kernel with several ports, routing each port's buffers to a register (or to a drop). A
//!   kernel's whole-buffer fast path (`route_all`) moves a buffer to its port untouched.
//! - [`Step::Decide`] runs one decision, a `split` or a `tee`, over a register: it computes each event's targets
//!   first, then filters the original buffer in place with the order-preserving `retain` and moves it to its last
//!   target, and gives every other target a buffer created with exact capacity (copies for a `tee`). When every event
//!   goes to one target, the buffer moves there whole. A target that leads to a `drop` counts its events and points
//!   as policy loss of that `drop` node instead of holding them.
//! - [`Step::Concat`] joins registers, as a `merge` does, in input port order.
//! - [`Step::Exit`] hands a register's buffers to one of the slot's exits, with the consumer's entry set in their
//!   header when the exit leads to another slot, and cleared otherwise. Exit buffers leave by value.
//! - [`Step::Drop`] discards a register's buffers, counting their events and points as policy loss of the node.
//!
//! An entry's steps cover only what its batch can reach: a `merge` whose other inputs the entry cannot reach is no
//! step at all, and its output is its one reachable input's register.
//!
//! # Kernel reuse
//!
//! Each kernel is identified by a [`KernelKey`]: its node and a hash of its operator and parameters, resolved values
//! included. Two generations whose programs for one slot have a kernel with the same key share one instance, with its
//! caches, as the specification's plan diff requires; any parameter change makes a new key, and a new instance.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use saluki_core::execution::{BatchHeader, EntryId};
use serde_json::{json, Value};
use snafu::Snafu;

use super::{
    frame::{ExitIndex, Frame, FrameTarget, SlotId},
    placement::{EntryKey, Placement},
    resolve, Role,
};
use crate::{
    canonical::PlanHash,
    ir::{Node, NodeId, Plan, PortName, PortRef},
    operators::{names, KernelSpec, OperatorRegistry},
    predicate::{CompiledPredicate, NoBindings, Predicate, PredicateError},
    validate::graph::{emit_output, Graph},
};

/// An error compiling a candidate plan.
#[derive(Clone, Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum CompileError {
    /// The candidate cannot be placed into the frame.
    #[snafu(display("{}", source), context(name(PlacementFailed)))]
    Placement {
        /// Why.
        source: super::placement::PlacementError,
    },

    /// A `split`'s predicate has a configuration binding without a resolved value.
    #[snafu(display("Cannot compile the predicate of node `{}`: {}", node, source))]
    UnresolvedBinding {
        /// The `split` node.
        node: NodeId,

        /// Why.
        source: PredicateError,
    },

    /// A node's parameters have no canonical form, so its kernel has no key.
    #[snafu(display(
        "The parameters of node `{}` cannot be hashed: {}. Write integers within ±(2^53 - 1).",
        node,
        message
    ))]
    Unhashable {
        /// The node.
        node: NodeId,

        /// Why.
        message: String,
    },

    /// A slot's program would be larger than a program can number.
    #[snafu(display(
        "The program of slot {} has more {} than a program supports ({}).",
        slot,
        what,
        limit
    ))]
    TooLarge {
        /// The slot.
        slot: SlotId,

        /// What there are too many of.
        what: &'static str,

        /// The largest number supported.
        limit: usize,
    },

    /// A validated, placed plan breaks an assumption of compilation, which is a bug.
    #[snafu(display("Internal error compiling the plan: {}", message))]
    Internal {
        /// What went wrong.
        message: String,
    },
}

macro_rules! index {
    ($(#[$meta:meta])* $ty:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $ty(u16);

        impl $ty {
            /// Creates an index.
            pub const fn new(index: u16) -> Self {
                Self(index)
            }

            /// Returns this index as a `usize`, for indexing.
            pub const fn get(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

index!(
    /// The index of a kernel in its program's kernel table.
    KernelIndex
);
index!(
    /// The index of a decision in its program's decision table.
    DecisionIndex
);
index!(
    /// A register of an entry program: a place holding buffers between steps. Register 0 holds the entry's batch.
    RegisterId
);

/// The identity of a kernel instance: its node, and a hash of its operator and parameters, resolved values included.
///
/// Programs of different generations share a kernel instance when their keys are equal.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KernelKey {
    /// The node the kernel runs.
    pub node: NodeId,

    /// The hash of `{"op": <operator>, "params": <parameters>}`, with resolved values.
    pub params_hash: PlanHash,
}

impl KernelKey {
    /// Returns the key of a node's kernel.
    ///
    /// # Errors
    ///
    /// Returns an error if the node's parameters have no canonical form.
    pub fn of(node: &Node) -> Result<Self, CompileError> {
        let value = json!({"op": node.op.as_str(), "params": node.params});
        let params_hash = PlanHash::of_value(&value).map_err(|error| CompileError::Unhashable {
            node: node.id.clone(),
            message: error.to_string(),
        })?;
        Ok(Self {
            node: node.id.clone(),
            params_hash,
        })
    }
}

impl fmt::Display for KernelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.node, self.params_hash)
    }
}

/// One kernel of a program.
#[derive(Clone, Debug)]
pub struct ProgramKernel {
    /// The kernel's identity.
    pub key: KernelKey,

    /// The node's operator.
    pub operator: &'static str,

    /// The kernel's output ports, in port order, for telemetry that counts events per port.
    pub ports: Vec<PortName>,

    /// How to build the kernel.
    pub spec: KernelSpec,
}

/// Where one target of a decision or one port of a kernel leads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// Into a register, for the steps that follow.
    Register(RegisterId),

    /// Into a `drop` node: the records are counted as its policy loss and discarded.
    Drop(NodeId),
}

/// One step of an entry program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// Runs a single-port kernel over a register, in place.
    Map {
        /// The kernel.
        kernel: KernelIndex,

        /// The register it transforms.
        register: RegisterId,
    },

    /// Runs a kernel with several output ports, routing each port.
    Expand {
        /// The kernel.
        kernel: KernelIndex,

        /// The register it consumes.
        input: RegisterId,

        /// Where each of its ports leads, in port order.
        ports: Vec<Route>,
    },

    /// Runs a decision, routing each event to its targets.
    Decide {
        /// The decision.
        decision: DecisionIndex,

        /// The register it consumes.
        input: RegisterId,

        /// Where each of its targets leads, in the decision's port order. The original buffer moves to the last target
        /// that receives events.
        targets: Vec<Route>,
    },

    /// Joins registers into one, in order, as a `merge` does.
    Concat {
        /// The registers joined, in the `merge`'s input port order.
        inputs: Vec<RegisterId>,

        /// The register that receives them.
        output: RegisterId,
    },

    /// Hands a register's buffers to an exit.
    Exit {
        /// The register.
        register: RegisterId,

        /// The exit.
        exit: ExitIndex,

        /// The consumer's entry, set in each buffer's header when the exit leads to another slot.
        entry: Option<EntryId>,
    },

    /// Discards a register's buffers, as policy loss of a `drop` node.
    Drop {
        /// The register.
        register: RegisterId,

        /// The `drop` node.
        node: NodeId,
    },
}

/// What a decision does.
#[derive(Clone, Debug)]
pub enum DecisionKind {
    /// A `split`: each event goes to `true` if the predicate matches it, and to `false` otherwise.
    Split {
        /// The predicate, in canonical form, with resolved values.
        predicate: Predicate,

        /// The predicate, compiled, shared so that a region can hold it beside the program.
        compiled: Arc<CompiledPredicate>,
    },

    /// A `tee`: each event goes to every target.
    Tee,
}

/// One decision: a `split` or a `tee`.
///
/// From stage 4, a maximal group of them compiles to one decision; through stage 3 each is its own.
#[derive(Clone, Debug)]
pub struct Decision {
    /// The node.
    pub node: NodeId,

    /// The node's output ports, in target order, for telemetry that counts events per port.
    pub ports: Vec<PortName>,

    /// What the decision does.
    pub kind: DecisionKind,
}

/// The steps that run a batch arriving at one entry of a slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryProgram {
    /// How the slot recognizes the entry's batches.
    pub key: EntryKey,

    /// The logical input port the entry's batches enter.
    pub input: PortRef,

    /// The steps, in order. Register 0 holds the batch.
    pub steps: Vec<Step>,

    /// The number of registers the steps use.
    pub registers: usize,
}

/// What one slot runs for one generation.
#[derive(Clone, Debug)]
pub struct RegionProgram {
    slot: SlotId,
    fingerprint: PlanHash,
    entries: Vec<EntryProgram>,
    kernels: Vec<ProgramKernel>,
    decisions: Vec<Decision>,
}

impl RegionProgram {
    /// Returns the slot the program is for.
    pub fn slot(&self) -> SlotId {
        self.slot
    }

    /// Returns the program's fingerprint: a hash of its entries, steps, kernel keys, and decisions.
    ///
    /// Two programs with equal fingerprints do the same thing with the same kernels, so a region can keep running the
    /// one it has.
    pub fn fingerprint(&self) -> PlanHash {
        self.fingerprint
    }

    /// Returns `true` if the program has no entries: the generation places nothing in the slot.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the entry programs, in the order of the slot's entries.
    pub fn entries(&self) -> &[EntryProgram] {
        &self.entries
    }

    /// Returns the program of one entry.
    pub fn entry(&self, key: EntryKey) -> Option<&EntryProgram> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// Returns the program of the entry a batch belongs to: by the entry its header carries, set by the slot that sent
    /// it, or otherwise by the admission point that stamped it.
    ///
    /// Returns `None` for a batch the program has no entry for, which is a wiring fault.
    pub fn entry_for(&self, header: &BatchHeader) -> Option<&EntryProgram> {
        let key = match (header.entry, header.admission_point) {
            (Some(entry), _) => EntryKey::Region(entry),
            (None, Some(point)) => EntryKey::Admission(point),
            (None, None) => return None,
        };
        self.entry(key)
    }

    /// Returns the kernel table.
    pub fn kernels(&self) -> &[ProgramKernel] {
        &self.kernels
    }

    /// Returns one kernel.
    pub fn kernel(&self, index: KernelIndex) -> Option<&ProgramKernel> {
        self.kernels.get(index.get())
    }

    /// Returns the kernel with the given key, if the program has it.
    pub fn kernel_by_key(&self, key: &KernelKey) -> Option<&ProgramKernel> {
        self.kernels.iter().find(|kernel| &kernel.key == key)
    }

    /// Returns the decision table.
    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }

    /// Returns one decision.
    pub fn decision(&self, index: DecisionIndex) -> Option<&Decision> {
        self.decisions.get(index.get())
    }

    /// Returns the largest number of registers any entry uses.
    pub fn register_count(&self) -> usize {
        self.entries.iter().map(|entry| entry.registers).max().unwrap_or(0)
    }
}

/// Compiles the program of every slot of a frame for a placed candidate plan, in slot order.
///
/// # Errors
///
/// Returns an error if a `split`'s predicate has a binding without a resolved value, if a node's parameters cannot be
/// hashed, or if the plan does not match its placement.
pub fn compile_programs(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, placement: &Placement,
) -> Result<Vec<RegionProgram>, CompileError> {
    let graph = resolve(plan, registry).map_err(|problems| CompileError::Internal {
        message: format!("a placed plan has structural problems: {}", problems.join("; ")),
    })?;
    let roles = super::roles(&graph);
    let mut slot_of = vec![None; graph.nodes.len()];
    for (node, slot) in slot_of.iter_mut().enumerate() {
        if let Some(super::frame::Site::Slot(placed)) = placement.site(graph.id(node)) {
            *slot = Some(placed);
        }
    }
    let context = Context {
        graph: &graph,
        roles: &roles,
        slot_of: &slot_of,
        frame,
        placement,
    };
    frame.slots().iter().map(|slot| context.compile_slot(slot.id)).collect()
}

struct Context<'a, 'p> {
    graph: &'a Graph<'p>,
    roles: &'a [Role],
    slot_of: &'a [Option<SlotId>],
    frame: &'a Frame,
    placement: &'a Placement,
}

impl Context<'_, '_> {
    fn compile_slot(&self, slot: SlotId) -> Result<RegionProgram, CompileError> {
        let graph = self.graph;
        let members = (0..graph.nodes.len())
            .filter(|&node| self.slot_of[node] == Some(slot))
            .collect::<Vec<_>>();

        let too_large = |what| CompileError::TooLarge {
            slot,
            what,
            limit: usize::from(u16::MAX) + 1,
        };
        let mut kernels = Vec::new();
        let mut kernel_of = vec![None; graph.nodes.len()];
        let mut decisions = Vec::new();
        let mut decision_of = vec![None; graph.nodes.len()];
        for &node in &members {
            let op = graph.op(node).expect("a resolved plan configures every node");
            let id = graph.id(node);
            match op.name() {
                names::SPLIT => {
                    let predicate = op
                        .params()
                        .predicate("predicate")
                        .ok_or_else(|| CompileError::Internal {
                            message: format!("validated `split` node `{}` has no predicate", id),
                        })?;
                    let compiled =
                        predicate
                            .compile(&NoBindings)
                            .map_err(|source| CompileError::UnresolvedBinding {
                                node: id.clone(),
                                source,
                            })?;
                    decision_of[node] = Some(DecisionIndex(
                        u16::try_from(decisions.len()).map_err(|_| too_large("decisions"))?,
                    ));
                    decisions.push(Decision {
                        node: id.clone(),
                        ports: op.ports().outputs.iter().map(|port| port.name.clone()).collect(),
                        kind: DecisionKind::Split {
                            predicate: predicate.canonical(),
                            compiled: Arc::new(compiled),
                        },
                    });
                }
                names::TEE => {
                    decision_of[node] = Some(DecisionIndex(
                        u16::try_from(decisions.len()).map_err(|_| too_large("decisions"))?,
                    ));
                    decisions.push(Decision {
                        node: id.clone(),
                        ports: op.ports().outputs.iter().map(|port| port.name.clone()).collect(),
                        kind: DecisionKind::Tee,
                    });
                }
                names::MERGE | names::EMIT | names::DROP => {}
                _ => {
                    let spec = op.kernel().ok_or_else(|| CompileError::Internal {
                        message: format!(
                            "node `{}` is a `{}` node, a synchronous operator without a kernel",
                            id,
                            op.name()
                        ),
                    })?;
                    kernel_of[node] = Some(KernelIndex(
                        u16::try_from(kernels.len()).map_err(|_| too_large("kernels"))?,
                    ));
                    kernels.push(ProgramKernel {
                        key: KernelKey::of(graph.nodes[node].node)?,
                        operator: op.name(),
                        ports: op.ports().outputs.iter().map(|port| port.name.clone()).collect(),
                        spec,
                    });
                }
            }
        }

        let placed = self.placement.slot(slot).ok_or_else(|| CompileError::Internal {
            message: format!("the placement has no slot {}", slot),
        })?;
        let mut entries = Vec::with_capacity(placed.entries.len());
        for entry in &placed.entries {
            let (start, port) =
                graph
                    .input_port(&entry.input.node, &entry.input.port)
                    .ok_or_else(|| CompileError::Internal {
                        message: format!("entry input `{}` does not resolve", entry.input),
                    })?;
            let builder = EntryBuilder {
                context: self,
                slot,
                kernel_of: &kernel_of,
                decision_of: &decision_of,
                steps: Vec::new(),
                registers: 1,
                inputs: BTreeMap::from([((start, port), RegisterId(0))]),
                folded: vec![false; graph.nodes.len()],
            };
            let (steps, registers) = builder.build(start).map_err(|()| too_large("registers"))?;
            entries.push(EntryProgram {
                key: entry.key,
                input: entry.input.clone(),
                steps,
                registers,
            });
        }

        let fingerprint =
            fingerprint(slot, &entries, &kernels, &decisions).map_err(|message| CompileError::Internal {
                message: format!("the program of slot {} has no canonical form: {}", slot, message),
            })?;
        Ok(RegionProgram {
            slot,
            fingerprint,
            entries,
            kernels,
            decisions,
        })
    }

    /// Returns the exit an edge from `slot` to the input port `(to, port)` leaves by, and the consumer's entry when it
    /// leads to another slot.
    fn exit_for(&self, slot: SlotId, to: usize, port: usize) -> Option<(ExitIndex, Option<EntryId>)> {
        let graph = self.graph;
        let target = match (self.roles[to], self.slot_of[to]) {
            (Role::Fixed, _) => FrameTarget::Fixed(graph.id(to).clone()),
            (_, Some(other)) if other != slot => FrameTarget::Slot(other),
            _ => FrameTarget::Output(graph.op(to).and_then(emit_output)?.clone()),
        };
        let exit = self.frame.slot(slot)?.exit_to(&target)?.index;
        let entry = match target {
            FrameTarget::Slot(other) => {
                let input = PortRef::new(graph.id(to).clone(), graph.op(to)?.ports().inputs[port].name.clone());
                match self.placement.slot(other)?.entry_at(&input)?.key {
                    EntryKey::Region(entry) => Some(entry),
                    EntryKey::Admission(_) => return None,
                }
            }
            _ => None,
        };
        Some((exit, entry))
    }
}

/// Builds the steps of one entry.
struct EntryBuilder<'c, 'a, 'p> {
    context: &'c Context<'a, 'p>,
    slot: SlotId,
    kernel_of: &'c [Option<KernelIndex>],
    decision_of: &'c [Option<DecisionIndex>],
    steps: Vec<Step>,
    registers: usize,
    /// The register holding what reaches each input port of a node in the slot.
    inputs: BTreeMap<(usize, usize), RegisterId>,
    /// `drop` nodes whose records a route already counts.
    folded: Vec<bool>,
}

impl EntryBuilder<'_, '_, '_> {
    fn build(mut self, start: usize) -> Result<(Vec<Step>, usize), ()> {
        let graph = self.context.graph;
        let order = self.reachable_order(start);
        for node in order {
            if self.folded[node] {
                continue;
            }
            let op = graph.op(node).expect("configured");
            let id = graph.id(node).clone();
            let present = (0..op.ports().inputs.len())
                .filter_map(|port| self.inputs.get(&(node, port)).copied())
                .collect::<Vec<_>>();
            let Some(&first) = present.first() else { continue };

            // The register for each output port, where the records stay in the slot or leave by an exit.
            let mut outputs: Vec<Option<RegisterId>> = vec![None; op.ports().outputs.len()];
            match op.name() {
                names::MERGE => {
                    let output = if present.len() == 1 {
                        first
                    } else {
                        let output = self.register()?;
                        self.steps.push(Step::Concat {
                            inputs: present,
                            output,
                        });
                        output
                    };
                    outputs[0] = Some(output);
                }
                names::EMIT => {
                    if let Some((exit, entry)) = self.context.exit_for(self.slot, node, 0) {
                        self.steps.push(Step::Exit {
                            register: first,
                            exit,
                            entry,
                        });
                    }
                }
                names::DROP => self.steps.push(Step::Drop {
                    register: first,
                    node: id,
                }),
                names::SPLIT | names::TEE => {
                    let targets = self.routes(node, &mut outputs)?;
                    self.steps.push(Step::Decide {
                        decision: self.decision_of[node].expect("every decision node has a decision"),
                        input: first,
                        targets,
                    });
                }
                _ => {
                    let kernel = self.kernel_of[node].expect("every kernel node has a kernel");
                    if op.ports().outputs.len() == 1 {
                        self.steps.push(Step::Map {
                            kernel,
                            register: first,
                        });
                        outputs[0] = Some(first);
                    } else {
                        let ports = self.routes(node, &mut outputs)?;
                        self.steps.push(Step::Expand {
                            kernel,
                            input: first,
                            ports,
                        });
                    }
                }
            }

            for (port, register) in outputs.into_iter().enumerate() {
                let Some(register) = register else { continue };
                let Some((to, to_port)) = graph.target(node, port) else {
                    continue;
                };
                if self.context.slot_of[to] == Some(self.slot) && self.context.roles[to] != Role::Fixed {
                    self.inputs.insert((to, to_port), register);
                } else if let Some((exit, entry)) = self.context.exit_for(self.slot, to, to_port) {
                    self.steps.push(Step::Exit { register, exit, entry });
                }
            }
        }
        Ok((self.steps, self.registers))
    }

    /// Returns a route for each output port of `node`, folding `drop` targets in the slot, and records the register of
    /// every other port.
    fn routes(&mut self, node: usize, outputs: &mut [Option<RegisterId>]) -> Result<Vec<Route>, ()> {
        let graph = self.context.graph;
        let mut routes = Vec::with_capacity(outputs.len());
        for (port, output) in outputs.iter_mut().enumerate() {
            let target = graph.target(node, port);
            let drop = target.filter(|&(to, _)| {
                self.context.slot_of[to] == Some(self.slot) && graph.op(to).is_some_and(|op| op.name() == names::DROP)
            });
            match drop {
                Some((to, _)) => {
                    self.folded[to] = true;
                    routes.push(Route::Drop(graph.id(to).clone()));
                }
                None => {
                    let register = self.register()?;
                    *output = Some(register);
                    routes.push(Route::Register(register));
                }
            }
        }
        Ok(routes)
    }

    fn register(&mut self) -> Result<RegisterId, ()> {
        let register = RegisterId(u16::try_from(self.registers).map_err(|_| ())?);
        self.registers += 1;
        Ok(register)
    }

    /// Returns the nodes of the slot reachable from `start`, in topological order, choosing among ready nodes in plan
    /// order.
    fn reachable_order(&self, start: usize) -> Vec<usize> {
        let graph = self.context.graph;
        let in_slot =
            |node: usize| self.context.slot_of[node] == Some(self.slot) && self.context.roles[node] != Role::Fixed;
        let successors = |node: usize| {
            let ports = graph.op(node).map_or(0, |op| op.ports().outputs.len());
            (0..ports)
                .filter_map(move |port| graph.target(node, port).map(|(to, _)| to))
                .filter(move |&to| in_slot(to))
        };

        let mut reached = vec![false; graph.nodes.len()];
        let mut pending = vec![start];
        reached[start] = true;
        while let Some(node) = pending.pop() {
            for next in successors(node) {
                if !reached[next] {
                    reached[next] = true;
                    pending.push(next);
                }
            }
        }

        let mut in_degree = vec![0usize; graph.nodes.len()];
        for node in (0..graph.nodes.len()).filter(|&node| reached[node]) {
            for next in successors(node) {
                in_degree[next] += 1;
            }
        }
        let mut ready = BTreeSet::from([start]);
        let mut order = Vec::new();
        while let Some(node) = ready.pop_first() {
            order.push(node);
            for next in successors(node) {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    ready.insert(next);
                }
            }
        }
        order
    }
}

/// Returns the fingerprint of a program: the hash of a JSON description of everything it does.
fn fingerprint(
    slot: SlotId, entries: &[EntryProgram], kernels: &[ProgramKernel], decisions: &[Decision],
) -> Result<PlanHash, String> {
    let route = |route: &Route| match route {
        Route::Register(register) => json!({"register": register.0}),
        Route::Drop(node) => json!({"drop": node.to_string()}),
    };
    let step = |step: &Step| match step {
        Step::Map { kernel, register } => json!({"map": {"kernel": kernel.0, "register": register.0}}),
        Step::Expand { kernel, input, ports } => json!({"expand": {
            "kernel": kernel.0, "input": input.0, "ports": ports.iter().map(route).collect::<Vec<_>>()
        }}),
        Step::Decide {
            decision,
            input,
            targets,
        } => json!({"decide": {
            "decision": decision.0, "input": input.0, "targets": targets.iter().map(route).collect::<Vec<_>>()
        }}),
        Step::Concat { inputs, output } => json!({"concat": {
            "inputs": inputs.iter().map(|register| register.0).collect::<Vec<_>>(), "output": output.0
        }}),
        Step::Exit { register, exit, entry } => json!({"exit": {
            "register": register.0, "exit": exit.get(), "entry": entry.map(|entry| entry.get())
        }}),
        Step::Drop { register, node } => json!({"drop": {"register": register.0, "node": node.to_string()}}),
    };
    let entry_key = |key: &EntryKey| match key {
        EntryKey::Admission(point) => json!({"admission": point.get()}),
        EntryKey::Region(entry) => json!({"region": entry.get()}),
    };
    let decisions = decisions
        .iter()
        .map(|decision| {
            let kind = match &decision.kind {
                DecisionKind::Split { predicate, .. } => {
                    json!({"split": serde_json::to_value(predicate).map_err(|error| error.to_string())?})
                }
                DecisionKind::Tee => json!({"tee": decision.ports.len()}),
            };
            Ok(json!({"node": decision.node.to_string(), "kind": kind}))
        })
        .collect::<Result<Vec<Value>, String>>()?;
    let description = json!({
        "slot": slot.get(),
        "kernels": kernels.iter().map(|kernel| json!({
            "node": kernel.key.node.to_string(), "params_hash": kernel.key.params_hash.to_string()
        })).collect::<Vec<_>>(),
        "decisions": decisions,
        "entries": entries.iter().map(|entry| json!({
            "key": entry_key(&entry.key),
            "input": entry.input.to_string(),
            "steps": entry.steps.iter().map(step).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    PlanHash::of_value(&description).map_err(|error| error.to_string())
}
