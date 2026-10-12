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
//! - [`Step::Decide`] runs one [decision](super::decision) over a register: it computes each event's exit set first,
//!   then moves the events to the registers its exits lead to, in batch order everywhere. A buffer whose events all
//!   take the same exits moves whole to the last of their registers, and every other one receives a copy. When one
//!   register receives every event that goes to any (the later of two such), the original buffer keeps those events,
//!   filtered in place with the order-preserving `retain`, and every other register receives a buffer of copies
//!   created with exact capacity. Otherwise every register receives a buffer of exactly its size, each event moving
//!   to the last register its set leads to and copied to the others. An exit that leads to a `drop` counts its events
//!   and points as policy loss of that `drop` node instead of holding them. A decision is a maximal group of `split`
//!   and `tee` nodes joined directly by edges, which runs as one function (see
//!   [decisions](super::decision#grouping)); the group's other nodes are no steps of their own.
//! - [`Step::Concat`] joins registers, as a `merge` does, in input port order.
//! - [`Step::Exit`] hands a register's buffers to one of the slot's exits, with the consumer's entry set in their
//!   header ([`EntrySource`]): the consumer region's entry when the exit leads to another slot, the lease of the
//!   output when it leads to the encode host of a bound output's delivery host, which the region resolves from its
//!   generation's [`LeaseTable`], and none otherwise: a fixed component or a fixed destination reads none. Exit buffers
//!   leave by value.
//! - [`Step::Drop`] discards a register's buffers, counting their events and points as policy loss of the node.
//! - [`Step::State`] hands a register's buffers to a state owner's kernel in an isolation slot, with the per-call
//!   parameters of the batch's generation. What the kernel emits later, at its flush ticks, runs the steps of its
//!   [`EntryKey::StateOutput`] entry.
//!
//! An entry's steps cover only what its batch can reach: a `merge` whose other inputs the entry cannot reach is no
//! step at all, and its output is its one reachable input's register.
//!
//! # Kernel reuse
//!
//! Each kernel is identified by a [`KernelKey`]: its node and a hash of its operator and parameters, resolved values
//! included. Two generations whose programs for one slot have a kernel with the same key share one instance, with its
//! caches, as the specification's plan diff requires; any parameter change makes a new key, and a new instance.
//!
//! A state owner's kernel also belongs to a **lineage** ([`LineageKey`]): its node and a hash of its operator and the
//! parameters its state contract depends on ([`OperatorDef::lineage_params`]). Its other parameters reach it per call,
//! from the program's [state parameter table][RegionProgram::state_params], whose entries the program's fingerprint
//! covers.
//!
//! [`OperatorDef::lineage_params`]: crate::operators::OperatorDef::lineage_params

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{BatchHeader, EntryId};
use saluki_metric_kernels::aggregate::EmissionFilter;
use serde_json::{json, Value};
use snafu::Snafu;

use super::{
    decision::{
        Branch, Decision, DecisionExit, DecisionKind, DecisionNode, ExitTarget, PredicateIndex, PredicateTable,
        EXIT_SET_CAPACITY, MEMO_COST_FLOOR,
    },
    frame::{ExitIndex, Frame, FrameTarget, SlotId},
    placement::{EntryKey, Placement},
    resolve,
    rewrite::r5b::PushedFilterSite,
    Role,
};
use crate::{
    canonical::PlanHash,
    ir::{Node, NodeId, OutputId, Plan, PortName, PortRef},
    operators::{names, ConfiguredOperator, KernelSpec, OperatorRegistry},
    predicate::{NoBindings, PredicateError},
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

    /// A state owner's kernel cannot be compiled.
    #[snafu(display("Cannot compile the state owner `{}`: {}.", node, message))]
    StateOwner {
        /// The state owner.
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
index!(
    /// The index of an entry in its program's state parameter table.
    StateParamsIndex
);

/// The identity of a kernel instance: its node, and a hash of its operator and parameters, resolved values included.
///
/// Programs of different generations share a kernel instance when their keys are equal. A state owner's kernel is
/// keyed by its lineage instead ([`LineageKey`]): its hash is the lineage's contract hash, so that generations whose
/// state owners share a lineage share its instance, whatever their other parameters, which reach it per call.
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

/// The identity of one lineage of a state owner's state: its node, and a hash of its operator and the parameters its
/// state contract depends on.
///
/// Generations whose state owners have the same lineage key can share one instance of the state: a `retain`
/// transition keeps the instance even when the node's other parameters change, since those reach the kernel per call.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LineageKey {
    /// The state owner's node.
    pub node: NodeId,

    /// The hash of `{"op": <operator>, "lineage": <lineage parameters>}`, with resolved values.
    pub contract: PlanHash,
}

impl LineageKey {
    /// Returns the lineage key of a state owner: its node, and the hash of its operator and the parameters its
    /// [`OperatorDef::lineage_params`][crate::operators::OperatorDef::lineage_params] returns.
    ///
    /// # Errors
    ///
    /// Returns an error if the lineage parameters have no canonical form.
    pub fn of(node: &NodeId, op: &ConfiguredOperator) -> Result<Self, CompileError> {
        let value = json!({"op": op.name(), "lineage": op.lineage_params()});
        let contract = PlanHash::of_value(&value).map_err(|error| CompileError::Unhashable {
            node: node.clone(),
            message: error.to_string(),
        })?;
        Ok(Self {
            node: node.clone(),
            contract,
        })
    }
}

impl fmt::Display for LineageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.node, self.contract)
    }
}

/// The lineage a program's state kernel belongs to, and when it ticks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineageInfo {
    /// The lineage.
    pub key: LineageKey,

    /// The interval between the kernel's flush ticks under this program's generation.
    pub flush_interval: Duration,
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

    /// The lineage of the kernel's state, for a state owner in an isolation slot, and `None` for a stateless kernel.
    pub lineage: Option<LineageInfo>,
}

/// One entry of a program's state parameter table: the per-call parameters of one state owner under the program's
/// generation, which the region passes to its kernel with every call.
#[derive(Clone)]
pub struct StateParamsEntry {
    /// The parameters, as the state owner's operator built them
    /// ([`OperatorDef::step_params`][crate::operators::OperatorDef::step_params]); the kernel reads them as its own
    /// type.
    pub params: Arc<dyn Any + Send + Sync>,

    /// The canonical hash of what the parameters were built from, which the program's fingerprint covers, so that a
    /// change of per-call parameters alone changes the fingerprint.
    pub hash: PlanHash,

    /// The filter R5b pushed into the state owner, if any, whose own series count the records the kernel suppresses
    /// at emission ([`PushedFilterSite`]).
    pub pushed: Option<PushedFilterSite>,
}

impl fmt::Debug for StateParamsEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateParamsEntry")
            .field("hash", &self.hash)
            .field("pushed", &self.pushed)
            .finish_non_exhaustive()
    }
}

/// Where one target of a decision or one port of a kernel leads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// Into a register, for the steps that follow.
    Register(RegisterId),

    /// Into a `drop` node: the records are counted as its policy loss and discarded.
    Drop(NodeId),
}

/// Where the entry an exit writes into each buffer's header comes from.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum EntrySource {
    /// No entry: the exit leads to a fixed component or a fixed destination, which reads none.
    None,

    /// The entry of the consumer region, which the program fixes.
    Region(EntryId),

    /// The lease of the output the exit delivers to, in the encode host the exit leads to.
    ///
    /// Leases are resources that outlive generations, so the program names the output, and the region resolves the
    /// lease from the [`LeaseTable`] its generation was prepared with: a rebinding changes only the lease table of an
    /// otherwise unchanged program.
    Lease(OutputId),
}

impl EntrySource {
    /// Returns the consumer region's entry, if the exit leads to another slot.
    pub fn region(&self) -> Option<EntryId> {
        match self {
            Self::Region(entry) => Some(*entry),
            Self::None | Self::Lease(_) => None,
        }
    }

    /// Returns the output whose lease the exit writes, if it leads to an encode host.
    pub fn lease(&self) -> Option<&OutputId> {
        match self {
            Self::Lease(output) => Some(output),
            Self::None | Self::Region(_) => None,
        }
    }
}

/// The lease of each output a generation's programs emit to: which logical encoder stream of an encode host a batch
/// leaving by an exit belongs to.
///
/// The installer assigns leases, which outlive generations: a generation that binds an output as another live
/// generation does reuses its lease. A region translates every [`EntrySource::Lease`] of a program through the table
/// its generation was prepared with.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LeaseTable {
    leases: BTreeMap<OutputId, EntryId>,
}

impl LeaseTable {
    /// Creates an empty lease table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the lease of an output, replacing any lease it had in the table.
    pub fn insert(&mut self, output: OutputId, lease: EntryId) {
        self.leases.insert(output, lease);
    }

    /// Returns the lease of an output, if the table has one.
    pub fn lease(&self, output: &OutputId) -> Option<EntryId> {
        self.leases.get(output).copied()
    }

    /// Returns every output's lease, in output order.
    pub fn iter(&self) -> impl Iterator<Item = (&OutputId, EntryId)> {
        self.leases.iter().map(|(output, lease)| (output, *lease))
    }

    /// Returns the number of leases in the table.
    pub fn len(&self) -> usize {
        self.leases.len()
    }

    /// Returns `true` if the table has no lease.
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }
}

impl FromIterator<(OutputId, EntryId)> for LeaseTable {
    fn from_iter<I: IntoIterator<Item = (OutputId, EntryId)>>(iter: I) -> Self {
        Self {
            leases: iter.into_iter().collect(),
        }
    }
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

    /// Runs a decision: computes each event's exit set, then moves each event to the target of every exit in it.
    Decide {
        /// The decision.
        decision: DecisionIndex,

        /// The register it consumes.
        input: RegisterId,

        /// Where each of the decision's exits leads, in its exit order. The original buffer moves whole to the last
        /// register when every event takes the same exits, and stays, filtered, at the register that receives every
        /// event going to any; otherwise each event moves to the last register its set leads to (see [the module
        /// documentation](self)).
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

        /// The consumer's entry, set in each buffer's header.
        entry: EntrySource,
    },

    /// Discards a register's buffers, as policy loss of a `drop` node.
    Drop {
        /// The register.
        register: RegisterId,

        /// The `drop` node.
        node: NodeId,
    },

    /// Hands a register's buffers to a state owner's kernel, with the batch's header and the per-call parameters of
    /// the batch's generation. The kernel emits nothing during the call: what it emits at its flush ticks runs the
    /// steps of its [`EntryKey::StateOutput`] entry.
    State {
        /// The state owner's kernel.
        kernel: KernelIndex,

        /// The register it consumes.
        register: RegisterId,

        /// The kernel's per-call parameters under the program's generation.
        params: StateParamsIndex,
    },
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
    state_params: Vec<StateParamsEntry>,
}

impl RegionProgram {
    /// Returns the slot the program is for.
    pub fn slot(&self) -> SlotId {
        self.slot
    }

    /// Returns the program's fingerprint: a hash of its entries, steps, kernel keys, decisions, and state parameters.
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

    /// Returns the state parameter table: the per-call parameters of the program's state owners, which its
    /// [`Step::State`] steps refer to.
    pub fn state_params(&self) -> &[StateParamsEntry] {
        &self.state_params
    }

    /// Returns one entry of the state parameter table.
    pub fn state_param(&self, index: StateParamsIndex) -> Option<&StateParamsEntry> {
        self.state_params.get(index.get())
    }

    /// Returns the largest number of registers any entry uses.
    pub fn register_count(&self) -> usize {
        self.entries.iter().map(|entry| entry.registers).max().unwrap_or(0)
    }
}

/// Which decision-time rewrites decision compilation applies.
///
/// Production compiles with [`ALL`][Self::ALL]; tests compare it with programs compiled without them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecisionRewrites {
    /// R4: memoize every decision worth memoizing.
    pub memoize: bool,

    /// R2 and R3: share equal predicates within a decision, and fold predicates constant given their resolved values.
    #[cfg(test)]
    pub predicates: bool,

    /// R5b: push filters into the emission of the state owners above them, which changes programs, not the plan.
    #[cfg(test)]
    pub pushed_filters: bool,
}

impl DecisionRewrites {
    /// Every decision-time rewrite.
    pub const ALL: Self = Self {
        memoize: true,
        #[cfg(test)]
        predicates: true,
        #[cfg(test)]
        pushed_filters: true,
    };

    /// No decision-time rewrite: each `split`'s predicate is evaluated as written, and nothing is memoized. Filters
    /// are still pushed into state owners.
    #[cfg(test)]
    pub const NONE: Self = Self {
        memoize: false,
        predicates: false,
        pushed_filters: true,
    };

    /// Every decision-time rewrite, but no filter pushed into a state owner: the original R5b's property tests compare
    /// with.
    #[cfg(test)]
    pub const WITHOUT_PUSHED_FILTERS: Self = Self {
        pushed_filters: false,
        ..Self::ALL
    };

    /// Returns whether R5b pushes filters into state owners: always, outside this crate's own tests.
    pub fn pushes_filters(&self) -> bool {
        #[cfg(test)]
        return self.pushed_filters;
        #[cfg(not(test))]
        return true;
    }
}

/// Compiles the program of every slot of a frame for a placed candidate plan, in slot order.
///
/// # Errors
///
/// Returns an error if a `split`'s predicate has a binding without a resolved value, if a node's parameters cannot be
/// hashed, if a `tee` has more output ports than a decision has exits ([`EXIT_SET_CAPACITY`]), or if the plan does not
/// match its placement.
pub fn compile_programs(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, placement: &Placement,
) -> Result<Vec<RegionProgram>, CompileError> {
    compile_programs_with(plan, registry, frame, placement, &[], DecisionRewrites::ALL)
}

/// A site R5b pushed a filter to, with the filter its state owner judges the records it emits with.
#[derive(Clone)]
pub(crate) struct SiteFilter {
    /// Where R5b pushed the filter.
    pub site: PushedFilterSite,

    /// What the state owner's kernel judges records with, from the parameters of its call
    /// ([`OperatorDef::step_params`][crate::operators::OperatorDef::step_params]).
    pub filter: Arc<dyn EmissionFilter>,
}

impl fmt::Debug for SiteFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SiteFilter")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

/// Compiles the program of every slot of a frame for a placed candidate plan, in slot order, with the filters R5b
/// pushed into its state owners, applying the given decision-time rewrites.
pub(crate) fn compile_programs_with(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, placement: &Placement, pushed: &[SiteFilter],
    rewrites: DecisionRewrites,
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
        plan,
        graph: &graph,
        roles: &roles,
        slot_of: &slot_of,
        frame,
        placement,
        pushed,
        rewrites,
    };
    frame.slots().iter().map(|slot| context.compile_slot(slot.id)).collect()
}

struct Context<'a, 'p> {
    plan: &'p Plan,
    graph: &'a Graph<'p>,
    roles: &'a [Role],
    slot_of: &'a [Option<SlotId>],
    frame: &'a Frame,
    placement: &'a Placement,
    pushed: &'a [SiteFilter],
    rewrites: DecisionRewrites,
}

/// The decisions of one slot, and where their exits are.
struct SlotDecisions {
    decisions: Vec<Decision>,
    /// The output port, as `(node, port)` by graph position, behind each exit of each decision, in exit order.
    exits: Vec<Vec<(usize, usize)>>,
    /// The decision each node is the first node of, by graph position.
    decision_of: Vec<Option<DecisionIndex>>,
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
        let mut state_params = Vec::new();
        let mut params_of = vec![None; graph.nodes.len()];
        for &node in &members {
            let op = graph.op(node).expect("a resolved plan configures every node");
            let id = graph.id(node);
            match op.name() {
                names::SPLIT | names::TEE | names::MERGE | names::EMIT | names::DROP => {}
                _ => {
                    let spec = op.kernel().ok_or_else(|| CompileError::Internal {
                        message: format!(
                            "node `{}` is a `{}` node, a synchronous operator without a kernel",
                            id,
                            op.name()
                        ),
                    })?;
                    let index = KernelIndex(u16::try_from(kernels.len()).map_err(|_| too_large("kernels"))?);
                    kernel_of[node] = Some(index);
                    let (key, lineage) = if self.roles[node] == Role::Isolated {
                        let (lineage, params) = self.state_owner(node, op)?;
                        params_of[node] = Some(StateParamsIndex(
                            u16::try_from(state_params.len()).map_err(|_| too_large("state parameters"))?,
                        ));
                        state_params.push(params);
                        // A state owner's instance is shared by every generation whose state owner has its lineage.
                        let key = KernelKey {
                            node: id.clone(),
                            params_hash: lineage.key.contract,
                        };
                        (key, Some(lineage))
                    } else {
                        (KernelKey::of(graph.nodes[node].node)?, None)
                    };
                    kernels.push(ProgramKernel {
                        key,
                        operator: op.name(),
                        ports: op.ports().outputs.iter().map(|port| port.name.clone()).collect(),
                        spec,
                        lineage,
                    });
                }
            }
        }
        let SlotDecisions {
            decisions,
            exits: decision_exits,
            decision_of,
        } = self.decisions(slot, &members)?;

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
            if self.slot_of[start] != Some(slot) {
                // The slot passes the entry's batches through, unchanged, to the state owner they enter.
                let (exit, next) = self.exit_for(slot, start, port).ok_or_else(|| CompileError::Internal {
                    message: format!(
                        "slot {} passes batches through to `{}`, which none of its exits leads to",
                        slot, entry.input
                    ),
                })?;
                entries.push(EntryProgram {
                    key: entry.key,
                    input: entry.input.clone(),
                    steps: vec![Step::Exit {
                        register: RegisterId(0),
                        exit,
                        entry: next,
                    }],
                    registers: 1,
                });
                continue;
            }
            let builder = EntryBuilder {
                context: self,
                slot,
                kernel_of: &kernel_of,
                params_of: &params_of,
                decision_of: &decision_of,
                decision_exits: &decision_exits,
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

        // What each state owner emits at its flush ticks leaves the slot from its output port, as the node's edges say.
        for &node in &members {
            if self.roles[node] != Role::Isolated {
                continue;
            }
            let kernel = kernel_of[node].expect("every state owner has a kernel");
            entries.push(self.state_output(slot, node, kernel)?);
        }

        let fingerprint = fingerprint(slot, &entries, &kernels, &decisions, &state_params).map_err(|message| {
            CompileError::Internal {
                message: format!("the program of slot {} has no canonical form: {}", slot, message),
            }
        })?;
        Ok(RegionProgram {
            slot,
            fingerprint,
            entries,
            kernels,
            decisions,
            state_params,
        })
    }

    /// Returns the lineage of a state owner's kernel and its entry of the state parameter table: its per-call
    /// parameters, with the filters R5b pushed into it, and their hash.
    fn state_owner(
        &self, node: usize, op: &ConfiguredOperator,
    ) -> Result<(LineageInfo, StateParamsEntry), CompileError> {
        let graph = self.graph;
        let id = graph.id(node);
        let key = LineageKey::of(id, op)?;
        let flush_interval = op.flush_interval().ok_or_else(|| CompileError::StateOwner {
            node: id.clone(),
            message: "it declares no flush interval, or its flush interval is bound to configuration and has no \
                      resolved value"
                .to_string(),
        })?;

        // A state owner judges the records it emits with at most one filter: the one below its output port.
        let mut sites = self.pushed.iter().filter(|pushed| &pushed.site.owner == id);
        let pushed = sites.next();
        if sites.next().is_some() {
            return Err(CompileError::Internal {
                message: format!(
                    "R5b pushed more than one filter into `{}`, which judges its records with one",
                    id
                ),
            });
        }

        // The hash covers the node's full parameters, with resolved values, and the filter pushed into it, so that a
        // change of per-call parameters alone changes the program's fingerprint.
        let site = pushed
            .map(|pushed| {
                let site = &pushed.site;
                let filter = graph
                    .nodes
                    .iter()
                    .find(|written| written.node.id == site.filter)
                    .ok_or_else(|| CompileError::Internal {
                        message: format!(
                            "R5b pushed filter `{}` into `{}`, but the plan has no such node",
                            site.filter, id
                        ),
                    })?;
                let filter = KernelKey::of(filter.node)?;
                Ok(json!({
                    "filter": site.filter.to_string(),
                    "params_hash": filter.params_hash.to_string(),
                    "drop_port": site.drop_port.to_string(),
                }))
            })
            .transpose()?;
        let params_hash = KernelKey::of(graph.nodes[node].node)?.params_hash;
        let pushed_sites = site.into_iter().collect::<Vec<_>>();
        let hash = PlanHash::of_value(&json!({"params_hash": params_hash.to_string(), "pushed": pushed_sites}))
            .map_err(|error| CompileError::Unhashable {
                node: id.clone(),
                message: error.to_string(),
            })?;
        let params = match op.step_params(pushed.map(|pushed| Arc::clone(&pushed.filter))) {
            Some(params) => params,
            // A filter the state owner's parameters cannot carry would suppress nothing while its site claims it does.
            None if pushed.is_some() => {
                return Err(CompileError::Internal {
                    message: format!(
                        "R5b pushed a filter into `{}`, whose operator takes no per-call parameters to carry it",
                        id
                    ),
                })
            }
            // Per-call parameters that do not build because a parameter has no value would leave a retained instance
            // running on the parameters it was built with, so the state owner is refused, as its kernel would be.
            None => {
                if let Some((name, _)) = op.params().iter().find(|(_, value)| value.known().is_none()) {
                    return Err(CompileError::StateOwner {
                        node: id.clone(),
                        message: format!(
                            "its `{}` parameter is bound to configuration and has no resolved value, so its per-call \
                             parameters cannot be built",
                            name
                        ),
                    });
                }
                // An operator without per-call parameters gets the unit value, which its kernel never reads.
                Arc::new(()) as Arc<dyn Any + Send + Sync>
            }
        };
        let pushed = pushed.map(|pushed| pushed.site.clone());
        Ok((
            LineageInfo { key, flush_interval },
            StateParamsEntry { params, hash, pushed },
        ))
    }

    /// Compiles the entry that runs what a state owner's kernel emits at its flush ticks: from register 0, through the
    /// state owner's output port, to the exit its edge leaves the slot by.
    fn state_output(&self, slot: SlotId, node: usize, kernel: KernelIndex) -> Result<EntryProgram, CompileError> {
        let graph = self.graph;
        let id = graph.id(node);
        let op = graph.op(node).expect("a resolved plan configures every node");
        let internal = |what: &str| CompileError::Internal {
            message: format!("state owner `{}` {}", id, what),
        };
        let [port] = op.ports().outputs.as_slice() else {
            return Err(internal("does not have exactly one output port"));
        };
        let (to, to_port) = graph
            .target(node, 0)
            .ok_or_else(|| internal("has an unconnected output port"))?;
        if self.slot_of[to] == Some(slot) && self.roles[to] != Role::Fixed {
            return Err(internal("feeds a node in its own isolation slot"));
        }
        let (exit, entry) = self
            .exit_for(slot, to, to_port)
            .ok_or_else(|| internal("feeds a node its isolation slot has no exit to"))?;
        Ok(EntryProgram {
            key: EntryKey::StateOutput(kernel),
            input: PortRef::new(id.clone(), port.name.clone()),
            steps: vec![Step::Exit {
                register: RegisterId(0),
                exit,
                entry,
            }],
            registers: 1,
        })
    }

    /// Groups the `split` and `tee` nodes of a slot into decisions, and compiles each, with the decision-time rewrites.
    ///
    /// A group starts at each `split` or `tee` whose input does not come from another `split` or `tee` of the slot,
    /// and takes in, depth first, every `split` and `tee` an edge from the group leads to, as long as the group's exits
    /// stay within [`EXIT_SET_CAPACITY`] (and its nodes within what a branch numbers); a node that would take the group
    /// beyond either starts a group of its own.
    /// Decisions are numbered in plan order of their first node, then in the order cut groups were found.
    fn decisions(&self, slot: SlotId, members: &[usize]) -> Result<SlotDecisions, CompileError> {
        let graph = self.graph;
        let is_decision = |node: usize| {
            self.slot_of[node] == Some(slot)
                && graph
                    .op(node)
                    .is_some_and(|op| matches!(op.name(), names::SPLIT | names::TEE))
        };
        let ports = |node: usize| graph.op(node).map_or(0, |op| op.ports().outputs.len());
        let too_wide = || CompileError::TooLarge {
            slot,
            what: "output ports on one `tee`",
            limit: EXIT_SET_CAPACITY,
        };

        let mut roots = members
            .iter()
            .copied()
            .filter(|&node| is_decision(node))
            .filter(|&node| !graph.source(node, 0).is_some_and(|(from, _)| is_decision(from)))
            .collect::<std::collections::VecDeque<_>>();
        let mut slot_decisions = SlotDecisions {
            decisions: Vec::new(),
            exits: Vec::new(),
            decision_of: vec![None; graph.nodes.len()],
        };
        while let Some(root) = roots.pop_front() {
            // The group, depth first: every node after the node leading to it.
            if ports(root) > EXIT_SET_CAPACITY {
                return Err(too_wide());
            }
            let mut group = vec![root];
            let mut exits = ports(root);
            let mut stack = vec![(root, 0)];
            while let Some(&(node, port)) = stack.last() {
                if port == ports(node) {
                    stack.pop();
                    continue;
                }
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                let Some((next, _)) = graph.target(node, port).filter(|&(next, _)| is_decision(next)) else {
                    continue;
                };
                let width = ports(next);
                if width > EXIT_SET_CAPACITY {
                    return Err(too_wide());
                }
                // Taking `next` in turns the port leading to it into `next`'s own ports. A group also stays within
                // the nodes a branch numbers, which only a chain of one-port `tee`s could exceed.
                if exits - 1 + width <= EXIT_SET_CAPACITY && group.len() < usize::from(u16::MAX) {
                    exits = exits - 1 + width;
                    group.push(next);
                    stack.push((next, 0));
                } else {
                    roots.push_back(next);
                }
            }

            let index =
                DecisionIndex(
                    u16::try_from(slot_decisions.decisions.len()).map_err(|_| CompileError::TooLarge {
                        slot,
                        what: "decisions",
                        limit: usize::from(u16::MAX) + 1,
                    })?,
                );
            let (decision, exits) = self.decision(slot, &group)?;
            slot_decisions.decision_of[root] = Some(index);
            slot_decisions.decisions.push(decision);
            slot_decisions.exits.push(exits);
        }
        Ok(slot_decisions)
    }

    /// Compiles one group of `split` and `tee` nodes, by graph position with every node after the node leading to it,
    /// into a decision, returning it with the output port behind each of its exits.
    fn decision(&self, slot: SlotId, group: &[usize]) -> Result<(Decision, Vec<(usize, usize)>), CompileError> {
        let graph = self.graph;
        let position_in_group = |node: usize| group.iter().position(|&member| member == node);
        let mut nodes = Vec::with_capacity(group.len());
        let mut exits = Vec::new();
        let mut exit_ports = Vec::new();
        let mut predicates = PredicateTable::new();
        for &node in group {
            let op = graph.op(node).expect("a resolved plan configures every node");
            let id = graph.id(node);
            let port_names = op
                .ports()
                .outputs
                .iter()
                .map(|port| port.name.clone())
                .collect::<Vec<_>>();
            let mut branches = Vec::with_capacity(port_names.len());
            for (port, name) in port_names.iter().enumerate() {
                let next = graph.target(node, port).and_then(|(next, _)| position_in_group(next));
                match next {
                    // Grouping keeps a group's nodes within what a branch numbers.
                    Some(next) => branches.push(Branch::Node(next as u16)),
                    None => {
                        branches.push(Branch::Exit(DecisionExit::new(exits.len() as u16)));
                        exits.push(ExitTarget {
                            node: id.clone(),
                            port: name.clone(),
                        });
                        exit_ports.push((node, port));
                    }
                }
            }
            let kind = match op.name() {
                names::SPLIT => DecisionKind::Split {
                    predicate: self.predicate(slot, node, &mut predicates)?,
                },
                _ => DecisionKind::Tee,
            };
            nodes.push(DecisionNode {
                node: id.clone(),
                ports: port_names,
                kind,
                branches,
            });
        }

        let mut decision = Decision {
            nodes,
            exits,
            predicates,
            memoized: false,
            rewrites: Vec::new(),
        };
        let plan_position = group
            .iter()
            .map(|&node| (graph.id(node).clone(), node))
            .collect::<BTreeMap<_, _>>();
        // R4 where it pays: the `split`s the memo spares cost enough that a hit saves several times what a miss adds.
        let cost = decision.evaluation_cost(|node| {
            plan_position
                .get(node)
                .and_then(|&node| graph.op(node))
                .map_or(0, |op| op.cost())
        });
        decision.memoized = self.rewrites.memoize && decision.memoizable() && cost >= MEMO_COST_FLOOR;
        decision.rewrites =
            decision.rewrite_records(|node| plan_position.get(node).copied().unwrap_or(usize::MAX), cost);
        Ok((decision, exit_ports))
    }

    /// Compiles the predicate of a `split` node into a decision's predicate table, returning its index.
    fn predicate(
        &self, slot: SlotId, node: usize, predicates: &mut PredicateTable,
    ) -> Result<PredicateIndex, CompileError> {
        let graph = self.graph;
        let id = graph.id(node);
        let op = graph.op(node).expect("a resolved plan configures every node");
        let predicate = op
            .params()
            .predicate("predicate")
            .ok_or_else(|| CompileError::Internal {
                message: format!("validated `split` node `{}` has no predicate", id),
            })?;
        let compiled = Arc::new(
            predicate
                .compile(&NoBindings)
                .map_err(|source| CompileError::UnresolvedBinding {
                    node: id.clone(),
                    source,
                })?,
        );
        #[cfg(test)]
        if !self.rewrites.predicates {
            return predicates
                .append(id, predicate, compiled)
                .ok_or(CompileError::TooLarge {
                    slot,
                    what: "predicates",
                    limit: usize::from(u16::MAX) + 1,
                });
        }
        predicates
            .intern(id, predicate, compiled)
            .ok_or(CompileError::TooLarge {
                slot,
                what: "predicates",
                limit: usize::from(u16::MAX) + 1,
            })
    }

    /// Returns the exit an edge from `slot` to the input port `(to, port)` leaves by, and where the consumer's entry
    /// comes from: the consumer region's entry when it leads to another slot, and the output's lease when it leads to
    /// the encode host of a bound output's delivery host. A fixed component or destination reads no entry.
    fn exit_for(&self, slot: SlotId, to: usize, port: usize) -> Option<(ExitIndex, EntrySource)> {
        let graph = self.graph;
        let mut lease = None;
        let target = match (self.roles[to], self.slot_of[to]) {
            (Role::Fixed, _) => FrameTarget::Fixed(graph.id(to).clone()),
            (_, Some(other)) if other != slot => FrameTarget::Slot(other),
            _ => {
                let output = graph.op(to).and_then(emit_output)?;
                lease = Some(output.clone());
                FrameTarget::of_emit(self.plan, output)
            }
        };
        let exit = self.frame.slot(slot)?.exit_to(&target)?.index;
        let entry = match target {
            FrameTarget::Slot(other) => {
                let input = PortRef::new(graph.id(to).clone(), graph.op(to)?.ports().inputs[port].name.clone());
                match self.placement.slot(other)?.entry_at(&input)?.key {
                    EntryKey::Region(entry) => EntrySource::Region(entry),
                    EntryKey::Admission(_) | EntryKey::StateOutput(_) => return None,
                }
            }
            FrameTarget::EncodeHost(_) => EntrySource::Lease(lease?),
            FrameTarget::Fixed(_) | FrameTarget::Destination(_) => EntrySource::None,
        };
        Some((exit, entry))
    }
}

/// Builds the steps of one entry.
struct EntryBuilder<'c, 'a, 'p> {
    context: &'c Context<'a, 'p>,
    slot: SlotId,
    kernel_of: &'c [Option<KernelIndex>],
    /// The state parameter table entry of each state owner, by graph position.
    params_of: &'c [Option<StateParamsIndex>],
    /// The decision each node is the first node of.
    decision_of: &'c [Option<DecisionIndex>],
    /// The output port behind each exit of each decision.
    decision_exits: &'c [Vec<(usize, usize)>],
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

            // The register behind each output port, as `(node, port, register)`, where the records stay in the slot or
            // leave by an exit: the node's own ports, or for a decision the ports behind its exits.
            let mut outputs: Vec<(usize, usize, RegisterId)> = Vec::new();
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
                    outputs.push((node, 0, output));
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
                    // Only a decision's first node has an input register; its other nodes are part of its step.
                    let Some(decision) = self.decision_of[node] else {
                        continue;
                    };
                    let decision_exits = &self.decision_exits[decision.get()];
                    let mut targets = Vec::with_capacity(decision_exits.len());
                    for &(from, port) in decision_exits {
                        targets.push(self.route(from, port, &mut outputs)?);
                    }
                    self.steps.push(Step::Decide {
                        decision,
                        input: first,
                        targets,
                    });
                }
                _ if self.context.roles[node] == Role::Isolated => {
                    // A state owner's kernel takes the batch into its state; what it emits later runs its own entry.
                    let kernel = self.kernel_of[node].expect("every state owner has a kernel");
                    let params = self.params_of[node].expect("every state owner has state parameters");
                    self.steps.push(Step::State {
                        kernel,
                        register: first,
                        params,
                    });
                }
                _ => {
                    let kernel = self.kernel_of[node].expect("every kernel node has a kernel");
                    if op.ports().outputs.len() == 1 {
                        self.steps.push(Step::Map {
                            kernel,
                            register: first,
                        });
                        outputs.push((node, 0, first));
                    } else {
                        let mut ports = Vec::with_capacity(op.ports().outputs.len());
                        for port in 0..op.ports().outputs.len() {
                            ports.push(self.route(node, port, &mut outputs)?);
                        }
                        self.steps.push(Step::Expand {
                            kernel,
                            input: first,
                            ports,
                        });
                    }
                }
            }

            for (from, port, register) in outputs {
                let Some((to, to_port)) = graph.target(from, port) else {
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

    /// Returns the route of the output port `port` of `node`, folding a `drop` target in the slot, and otherwise
    /// records the port's new register in `outputs`.
    fn route(&mut self, node: usize, port: usize, outputs: &mut Vec<(usize, usize, RegisterId)>) -> Result<Route, ()> {
        let graph = self.context.graph;
        let drop = graph.target(node, port).filter(|&(to, _)| {
            self.context.slot_of[to] == Some(self.slot) && graph.op(to).is_some_and(|op| op.name() == names::DROP)
        });
        match drop {
            Some((to, _)) => {
                self.folded[to] = true;
                Ok(Route::Drop(graph.id(to).clone()))
            }
            None => {
                let register = self.register()?;
                outputs.push((node, port, register));
                Ok(Route::Register(register))
            }
        }
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
    state_params: &[StateParamsEntry],
) -> Result<PlanHash, String> {
    let params_hash = |index: StateParamsIndex| {
        state_params
            .get(index.get())
            .map_or(Value::Null, |entry| json!(entry.hash.to_string()))
    };
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
        Step::Exit { register, exit, entry } => {
            // A lease is named by its output, never by its identifier, so a rebinding leaves the fingerprint as it is.
            let entry = match entry {
                EntrySource::None => Value::Null,
                EntrySource::Region(entry) => json!(entry.get()),
                EntrySource::Lease(output) => json!({"lease": output.as_str()}),
            };
            json!({"exit": {"register": register.0, "exit": exit.get(), "entry": entry}})
        }
        Step::Drop { register, node } => json!({"drop": {"register": register.0, "node": node.to_string()}}),
        Step::State {
            kernel,
            register,
            params,
        } => json!({"state": {"kernel": kernel.0, "register": register.0, "params": params_hash(*params)}}),
    };
    let entry_key = |key: &EntryKey| match key {
        EntryKey::Admission(point) => json!({"admission": point.get()}),
        EntryKey::Region(entry) => json!({"region": entry.get()}),
        EntryKey::StateOutput(kernel) => json!({"state_output": kernel.0}),
    };
    let decisions = decisions
        .iter()
        .map(|decision| {
            let predicates = decision
                .predicates
                .entries()
                .iter()
                .map(|entry| {
                    Ok(json!({
                        "predicate": serde_json::to_value(&entry.predicate).map_err(|error| error.to_string())?,
                        // The canonical form sorts operands; what runs evaluates them in the order written (R7).
                        "written": serde_json::to_value(&entry.written).map_err(|error| error.to_string())?,
                        "folded": entry.folded,
                    }))
                })
                .collect::<Result<Vec<Value>, String>>()?;
            let nodes = decision
                .nodes
                .iter()
                .map(|node| {
                    let kind = match node.kind {
                        DecisionKind::Split { predicate } => json!({"split": predicate.get()}),
                        DecisionKind::Tee => json!("tee"),
                    };
                    let branches = node
                        .branches
                        .iter()
                        .map(|branch| match branch {
                            Branch::Node(next) => json!({"node": next}),
                            Branch::Exit(exit) => json!({"exit": exit.get()}),
                        })
                        .collect::<Vec<_>>();
                    json!({"node": node.node.to_string(), "kind": kind, "branches": branches})
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "nodes": nodes, "predicates": predicates, "exits": decision.exits.len(), "memoized": decision.memoized
            }))
        })
        .collect::<Result<Vec<Value>, String>>()?;
    let kernels = kernels
        .iter()
        .map(|kernel| {
            let mut description = json!({
                "node": kernel.key.node.to_string(), "params_hash": kernel.key.params_hash.to_string()
            });
            // Stateless kernels have no lineage, and their description stays what it was before lineages existed.
            if let Some(lineage) = &kernel.lineage {
                description["lineage"] = json!({
                    "contract": lineage.key.contract.to_string(),
                    "flush_interval_nanos": u64::try_from(lineage.flush_interval.as_nanos()).unwrap_or(u64::MAX),
                });
            }
            description
        })
        .collect::<Vec<_>>();
    let mut description = json!({
        "slot": slot.get(),
        "kernels": kernels,
        "decisions": decisions,
        "entries": entries.iter().map(|entry| json!({
            "key": entry_key(&entry.key),
            "input": entry.input.to_string(),
            "steps": entry.steps.iter().map(step).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    // Programs without state parameters keep the description they had before state parameters existed.
    if !state_params.is_empty() {
        description["state_params"] = state_params
            .iter()
            .map(|entry| json!(entry.hash.to_string()))
            .collect::<Vec<_>>()
            .into();
    }
    PlanHash::of_value(&description).map_err(|error| error.to_string())
}
