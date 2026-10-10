//! Operators: the closed set of node types a plan can use.
//!
//! A plan names an operator for each node and passes it parameters; everything else about the node comes from the
//! operator's trusted definition, an [`OperatorDef`]: its parameter schema, the ports that follow from its
//! parameters, its [effect contract][EffectContract], the types of its output ports, how its state moves between
//! generations, its memory bounds, and, from the stage where it has one, how to build its kernel.
//!
//! [`OperatorRegistry::native`] is the closed set:
//!
//! | Operator | Boundary | Ports | Summary |
//! |---|---|---|---|
//! | `source` | I/O host | out `metrics` | Where a host source's metrics enter the plan. |
//! | `dogstatsd_mapper` | synchronous | `in`, `out` | Renames metrics and adds tags by mapping profile. |
//! | `namespace_prefix` | synchronous | `in`, `out` | Adds a namespace prefix to metric names. |
//! | `timestamp_split` | synchronous | `in`; `default`, `passthrough` | Separates timestamped points. |
//! | `tag_projection` | synchronous | `in`, `out` | Removes tags and restricts tag values, for counters and sketches. |
//! | `aggregate` | isolated | `in`, `out` | Native aggregation into time buckets, as a kernel in an isolation slot. |
//! | `host_enrichment` | synchronous | `in`, `out` | Leaves metrics unchanged. |
//! | `host_tags` | synchronous | `in`, `out` | Adds host tags until a deadline. |
//! | `opaque_host` | opaque host | `in`; named outputs | A native component without an extracted kernel. |
//! | `split` | synchronous | `in`; `true`, `false` | Routes each record by a predicate. |
//! | `tee` | synchronous | `in`; `0`..`n-1` | Copies each record to every output. |
//! | `merge` | synchronous | `0`..`n-1`; `out` | Joins several inputs. |
//! | `emit` | synchronous | `in` | Delivers to a declared output. |
//! | `drop` | synchronous | `in` | Discards records as policy loss. |
//!
//! Parameters whose values come from configuration are bindable: lowering writes them as configuration bindings with
//! their resolved values. Parameters that structure the plan, such as port counts, are never bindable.

use std::{any::Any, fmt, sync::Arc, time::Duration};

use saluki_common::resource_tracking::ResourceGroupToken;
use saluki_core::{
    accounting::MemoryBoundsBuilder, components::ComponentContext, execution::Kernel, support::SubsystemIdentifier,
    topology::ComponentId,
};
use saluki_metric_kernels::aggregate::{ContextBudget, EmissionFilter};
use snafu::Snafu;

use crate::{
    extension::TransitionClass,
    ir::{decode_params, DecodedParams, Namespace, NodeId, ParamError, ParamSpec, Params},
    model::PortType,
};

mod contract;
pub use self::contract::{
    Boundary, Cardinality, EffectContract, Identity, KeyedState, Loss, LossClass, LossDecl, OrderContract, PerKind,
    PortRelation, Reducer, ReducerKey, StateContract, StateTime, TimeContract,
};

mod ports;
pub use self::ports::{union_of, InputPort, KindMap, OutputPort, PortMapping, Ports, TimestampMap, IN, OUT};

mod aggregate;
pub use self::aggregate::{aggregate_params, histogram_params, Aggregate, HISTOGRAM_AGGREGATES, MAX_HISTOGRAM_SERIES};

mod dogstatsd;
pub use self::dogstatsd::{
    decode_mapper_profiles, decode_tag_filter_rules, decode_tag_value_rules, encode_mapper_profiles,
    encode_tag_filter_rules, encode_tag_value_rules, namespace_prefix_params, DogstatsdMapper, NamespacePrefix,
    TagProjection, TimestampSplit, DEFAULT_PORT, MAPPER_PROFILES, PASSTHROUGH_PORT, TAG_FILTER_RULES, TAG_VALUE_RULES,
};

mod enrich;
pub use self::enrich::{HostEnrichment, HostTags};

mod opaque;
pub use self::opaque::OpaqueHost;

mod routing;
pub(crate) use self::routing::decision_cost;
pub use self::routing::{DropOperator, Emit, Merge, Split, Tee, FALSE_PORT, TRUE_PORT};

mod source;
pub use self::source::{Source, SourceBinding, METRICS_PORT};

/// The names of the native operators.
pub mod names {
    /// The `source` operator.
    pub const SOURCE: &str = "source";
    /// The `dogstatsd_mapper` operator.
    pub const DOGSTATSD_MAPPER: &str = "dogstatsd_mapper";
    /// The `namespace_prefix` operator.
    pub const NAMESPACE_PREFIX: &str = "namespace_prefix";
    /// The `timestamp_split` operator.
    pub const TIMESTAMP_SPLIT: &str = "timestamp_split";
    /// The `tag_projection` operator.
    pub const TAG_PROJECTION: &str = "tag_projection";
    /// The `aggregate` operator.
    pub const AGGREGATE: &str = "aggregate";
    /// The `host_enrichment` operator.
    pub const HOST_ENRICHMENT: &str = "host_enrichment";
    /// The `host_tags` operator.
    pub const HOST_TAGS: &str = "host_tags";
    /// The `opaque_host` operator.
    pub const OPAQUE_HOST: &str = "opaque_host";
    /// The `split` operator.
    pub const SPLIT: &str = "split";
    /// The `tee` operator.
    pub const TEE: &str = "tee";
    /// The `merge` operator.
    pub const MERGE: &str = "merge";
    /// The `emit` operator.
    pub const EMIT: &str = "emit";
    /// The `drop` operator.
    pub const DROP: &str = "drop";
}

/// The definition of one operator.
///
/// Definitions are trusted: only this crate supplies them, and a plan can only choose among them. Every method other
/// than [`name`][Self::name], [`summary`][Self::summary], and [`params`][Self::params] takes the node's parameters
/// already decoded against [`params`][Self::params] and checked by [`check_params`][Self::check_params], which
/// [`OperatorRegistry::configure`] does.
pub trait OperatorDef: Send + Sync {
    /// Returns the operator's name, as plans refer to it.
    fn name(&self) -> &'static str;

    /// Returns a one-sentence description of what the operator does.
    fn summary(&self) -> &'static str;

    /// Returns the operator's parameter schema.
    fn params(&self) -> &'static [ParamSpec];

    /// Checks the rules between decoded parameters that their types alone do not express, returning every problem
    /// found.
    ///
    /// Values bound to configuration and not yet resolved are not known, and are not checked.
    fn check_params(&self, _params: &DecodedParams) -> Vec<ParamError> {
        Vec::new()
    }

    /// Returns the operator's ports.
    fn ports(&self, params: &DecodedParams) -> Ports;

    /// Returns the operator's effect contract.
    fn contract(&self, params: &DecodedParams) -> EffectContract;

    /// Returns the type of each output port, given the type of each input port.
    ///
    /// By default, each output port's [mapping][PortMapping] is applied to the union of the inputs.
    fn output_types(&self, _params: &DecodedParams, ports: &Ports, inputs: &[PortType]) -> Vec<PortType> {
        ports.map_types(inputs)
    }

    /// Refines how state moves when a node of this operator changes from the `old` parameters to the `new` ones.
    ///
    /// The class is decided by comparing the old and new [state contracts][StateContract] (see
    /// [`ConfiguredOperator::transition_to`]); this method can only make it stricter, for state the contract does not
    /// describe, such as an opaque host's. The default refines nothing: it returns [`TransitionClass::Retain`].
    fn transition(&self, _old: &DecodedParams, _new: &DecodedParams) -> TransitionClass {
        TransitionClass::Retain
    }

    /// Declares the memory the operator's kernel needs.
    ///
    /// Operators that run inside a host component, such as opaque hosts, declare nothing here: the host declares its
    /// own bounds. The memory of the state a state owner keeps in entries drawn from its host's state budget is not
    /// declared here, but in [`specify_state_bounds`][Self::specify_state_bounds]. The default declares nothing.
    fn specify_bounds(&self, _params: &DecodedParams, _builder: &mut MemoryBoundsBuilder) {}

    /// Declares the memory of the state the operator's kernel keeps in entries drawn from its host's state budget
    /// ([`KernelBuildContext::state_budget`]), such as aggregation's contexts, at the node's own limit on them.
    ///
    /// Every state owner of a host draws on one budget, which bounds what they hold together whatever their own
    /// limits say. So a host's memory bounds hold this state once, for its boot program, whose state owners' limits
    /// set the budget's capacity; and the limit on the memory an artifact's nodes declare
    /// ([`ARTIFACT_MEMORY`][crate::limits::ARTIFACT_MEMORY]) leaves it out, counting only what
    /// [`specify_bounds`][Self::specify_bounds] declares. The default declares nothing.
    fn specify_state_bounds(&self, _params: &DecodedParams, _builder: &mut MemoryBoundsBuilder) {}

    /// Returns an estimate of the work the operator does per event, in nanoseconds, for rewrite R1's cost model.
    ///
    /// The estimates come from the stage 3 measurements of the default program's kernels running in a region over a
    /// DogStatsD-like batch, release build, and are only compared with one another: R1 fuses synchronous neighbors
    /// into one region as long as the region's estimated cost stays within a budget (see
    /// [`Frame`][crate::compile::Frame]). Operators that run in host components, or alone in an isolation slot, which
    /// R1 never fuses, are not priced: their cost counts nowhere. The default is zero, for those and for operators
    /// whose work is negligible next to a kernel's.
    fn cost(&self, _params: &DecodedParams) -> u32 {
        0
    }

    /// Returns how to build the operator's kernel, or `None` if it has none.
    ///
    /// The spec captures the parameters; the host supplies the rest when it builds the kernel, in a
    /// [`KernelBuildContext`]. Sources and opaque hosts never have kernels, and the routing operators (`split`, `tee`,
    /// `merge`, `emit`, `drop`) need none, since hosts run them themselves. Every other native operator has a kernel,
    /// extracted from its native component. A kernel whose parameters are bound to configuration and not resolved
    /// fails to build.
    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
        None
    }

    /// Returns the parameters a lineage of the operator's state is keyed by: those its state contract depends on, so
    /// that two generations whose nodes have equal lineage parameters can share one instance of the state.
    ///
    /// The installer keys a state owner's kernel instance by its node and a hash of its operator and these parameters,
    /// so that a `retain` transition keeps the instance even when other parameters change; those reach the kernel per
    /// call instead (see [`step_params`][Self::step_params]). The default is every parameter, as decoded: an operator
    /// without state, or whose every parameter belongs to its state contract, shares an instance only with equal
    /// parameters.
    fn lineage_params(&self, params: &DecodedParams) -> Params {
        params.encode()
    }

    /// Returns the parameters a state owner's kernel takes with each call, from the generation the call runs under,
    /// rather than from when the kernel was built: those a `retain` transition may change without a new lineage.
    ///
    /// A host passes them to the kernel through
    /// [`Turn::with_param`][saluki_core::execution::Turn::with_param]; a kernel called without them uses the
    /// parameters it was built with. `filter` is the emission filter a rewrite pushed into the state owner, if any,
    /// which the kernel applies to what it emits. The default is `None`: the operator takes no per-call parameters.
    fn step_params(
        &self, _params: &DecodedParams, _filter: Option<Arc<dyn EmissionFilter>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// Returns the interval between the flush ticks of a state owner's kernel, which the host of its isolation slot
    /// schedules, or `None` if the operator has no time-driven work or the interval is bound to configuration and not
    /// resolved.
    ///
    /// The host runs the kernel's [`advance_time`][saluki_core::execution::Kernel::advance_time] at each tick. After a
    /// tick, the next one is due this interval later, as the generation the tick ran under says. The default is
    /// `None`.
    fn flush_interval(&self, _params: &DecodedParams) -> Option<Duration> {
        None
    }
}

impl fmt::Debug for dyn OperatorDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorDef").field("name", &self.name()).finish()
    }
}

/// An error building a kernel.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display("Failed to build the kernel: {}", message))]
pub struct KernelBuildError {
    /// What went wrong.
    pub message: String,
}

/// The topology name that [`KernelBuildContext::detached`] places components in.
const DETACHED_TOPOLOGY: &str = "plan";

/// What a host supplies when it builds a kernel, besides the parameters the kernel's [`KernelSpec`] captured.
///
/// A kernel reports its telemetry, and names its internal caches, under its node's identity. The context carries that
/// identity: the node the kernel is built for, and the component context the host reports it as. In a running topology
/// the host supplies the component context of the native component the node stands for, so a kernel extracted from a
/// native component keeps that component's telemetry names and labels, whatever region runs it. A host without a
/// topology, such as the reference interpreter or a test, uses [`detached`][Self::detached].
///
/// The context can also carry the resource group the kernel's memory and CPU time are accounted to
/// ([`with_resource_group`][Self::with_resource_group]). A host that runs the kernel enters that group whenever it
/// builds or calls the kernel, so that the kernel is accounted for separately from the component that runs it, as a
/// native component's sub-transforms are. Without one, the kernel is accounted to whatever group is current.
///
/// A stateful kernel finds here the state budget it draws credits from
/// ([`with_state_budget`][Self::with_state_budget]), shared by every lineage of its state owner and, through the
/// global budget, by every state owner of the host.
#[derive(Clone)]
pub struct KernelBuildContext {
    node: NodeId,
    component: ComponentContext,
    resource_group: Option<ResourceGroupToken>,
    state_budget: Option<ContextBudget>,
}

impl fmt::Debug for KernelBuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelBuildContext")
            .field("node", &self.node)
            .field("component", &self.component)
            .field("resource_group", &self.resource_group.is_some())
            .field("state_budget", &self.state_budget)
            .finish()
    }
}

impl KernelBuildContext {
    /// Creates a context for building the kernel of `node`, reported as the component `component`.
    pub fn new(node: NodeId, component: ComponentContext) -> Self {
        Self {
            node,
            component,
            resource_group: None,
            state_budget: None,
        }
    }

    /// Accounts the kernel's memory and CPU time to the resource group of `token`.
    pub fn with_resource_group(mut self, token: ResourceGroupToken) -> Self {
        self.resource_group = Some(token);
        self
    }

    /// Gives a stateful kernel the budgets its state draws on: the host's global budget and its node's.
    pub fn with_state_budget(mut self, budget: ContextBudget) -> Self {
        self.state_budget = Some(budget);
        self
    }

    /// Returns the budgets the host gave a stateful kernel's state, if it gave any.
    ///
    /// A host that runs state owners gives every one a budget; a kernel built without one, outside any topology, owns
    /// its state alone.
    pub fn state_budget(&self) -> Option<&ContextBudget> {
        self.state_budget.as_ref()
    }

    /// Creates a context for building the kernel of `node` outside any topology.
    ///
    /// The kernel is reported as a transform named after the node, `<local>` for a reserved node and `policy_<local>`
    /// for a node in the policy namespace, in a topology named `plan`.
    pub fn detached(node: &NodeId) -> Self {
        let name = match node.namespace() {
            Namespace::Reserved => node.local().to_string(),
            Namespace::Policy => format!("policy_{}", node.local()),
        };
        // Local identifiers start with a letter and never end with an underscore, so they are valid component IDs.
        let component_id = ComponentId::try_from(name.as_str()).expect("node identifiers are valid component IDs");
        let topology = SubsystemIdentifier::from_segments([DETACHED_TOPOLOGY]);
        Self::new(node.clone(), ComponentContext::transform(&topology, component_id))
    }

    /// Returns the node the kernel is built for.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Returns the component context the kernel reports telemetry under.
    pub fn component(&self) -> &ComponentContext {
        &self.component
    }

    /// Returns the token of the resource group the kernel is accounted to, if the host gave it one.
    pub fn resource_group(&self) -> Option<ResourceGroupToken> {
        self.resource_group
    }

    /// Runs `f` inside the kernel's resource group, if it has one, so that what `f` allocates and the CPU time it
    /// takes are accounted to the kernel.
    pub fn accounted<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.resource_group.as_ref().map(ResourceGroupToken::enter);
        f()
    }
}

type KernelFactory = dyn Fn(&KernelBuildContext) -> Result<Box<dyn Kernel>, KernelBuildError> + Send + Sync;

/// How to build one node's kernel: a factory that has captured the node's resolved parameters.
///
/// Hosts keep a node's spec so that they can rebuild its kernel, for example after a panic.
#[derive(Clone)]
pub struct KernelSpec {
    factory: Arc<KernelFactory>,
}

impl KernelSpec {
    /// Creates a spec from a kernel factory.
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn(&KernelBuildContext) -> Result<Box<dyn Kernel>, KernelBuildError> + Send + Sync + 'static,
    {
        Self {
            factory: Arc::new(factory),
        }
    }

    /// Builds a new instance of the kernel, in the context the host supplies.
    ///
    /// # Errors
    ///
    /// Returns an error if the kernel cannot be built from its parameters.
    pub fn build(&self, context: &KernelBuildContext) -> Result<Box<dyn Kernel>, KernelBuildError> {
        (self.factory)(context)
    }
}

impl fmt::Debug for KernelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelSpec").finish_non_exhaustive()
    }
}

/// An error configuring an operator for a node.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ConfigureError {
    /// The plan names an operator that is not registered.
    #[snafu(display("Unknown operator `{}`; known operators are: {}.", name, known))]
    UnknownOperator {
        /// The operator name.
        name: String,

        /// The registered operator names, comma-separated.
        known: String,
    },

    /// The parameters do not match the operator's schema, or break one of its rules.
    #[snafu(display("Invalid parameters for operator `{}`.", operator))]
    InvalidParams {
        /// The operator.
        operator: &'static str,

        /// Every problem found.
        errors: Vec<ParamError>,
    },
}

/// An operator configured with one node's parameters: its definition, decoded parameters, ports, and contract.
#[derive(Clone, Debug)]
pub struct ConfiguredOperator {
    def: &'static dyn OperatorDef,
    params: DecodedParams,
    ports: Ports,
    contract: EffectContract,
}

impl ConfiguredOperator {
    /// Returns the operator's definition.
    pub fn def(&self) -> &'static dyn OperatorDef {
        self.def
    }

    /// Returns the operator's name.
    pub fn name(&self) -> &'static str {
        self.def.name()
    }

    /// Returns the decoded parameters.
    pub fn params(&self) -> &DecodedParams {
        &self.params
    }

    /// Returns the ports.
    pub fn ports(&self) -> &Ports {
        &self.ports
    }

    /// Returns the effect contract.
    pub fn contract(&self) -> &EffectContract {
        &self.contract
    }

    /// Returns the type of each output port, given the type of each input port.
    pub fn output_types(&self, inputs: &[PortType]) -> Vec<PortType> {
        self.def.output_types(&self.params, &self.ports, inputs)
    }

    /// Classifies how state moves when this node is replaced by `new`, with the same identifier, in a later
    /// generation.
    ///
    /// The class is decided by the old and new [state contracts][StateContract], as the specification's compatibility
    /// rule requires, and then refined by the operator's [`OperatorDef::transition`], which can only make it stricter:
    ///
    /// - Two stateless contracts are [`TransitionClass::Retain`].
    /// - Two keyed contracts with a different reducer key or reducer are [`TransitionClass::Reject`]: the state cannot
    ///   be carried. Otherwise, a different time contract or admission policy is [`TransitionClass::Cut`], as is a
    ///   time contract not known on either side, such as a bucket width bound to configuration and not yet resolved.
    ///   Equal contracts are [`TransitionClass::Retain`].
    /// - Two opaque contracts are [`TransitionClass::Retain`] before refinement: the operator decides.
    /// - Any other pair, including a change of operator that keeps or adds state, is [`TransitionClass::Reject`].
    pub fn transition_to(&self, new: &ConfiguredOperator) -> TransitionClass {
        let by_contract = match (&self.contract.state, &new.contract.state) {
            (StateContract::None, StateContract::None) => TransitionClass::Retain,
            (StateContract::Keyed(old), StateContract::Keyed(new)) => {
                if old.key != new.key || old.reducer != new.reducer {
                    TransitionClass::Reject
                } else if old.time != new.time || !old.time.is_known() || old.admission != new.admission {
                    TransitionClass::Cut
                } else {
                    TransitionClass::Retain
                }
            }
            (StateContract::Opaque, StateContract::Opaque) if self.name() == new.name() => TransitionClass::Retain,
            _ => TransitionClass::Reject,
        };
        if self.name() == new.name() {
            by_contract.max(self.def.transition(&self.params, &new.params))
        } else {
            by_contract
        }
    }

    /// Declares the memory the operator's kernel needs, apart from the state it draws from its host's state budget;
    /// see [`OperatorDef::specify_bounds`].
    pub fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.def.specify_bounds(&self.params, builder);
    }

    /// Declares the memory of the state the operator's kernel keeps in entries drawn from its host's state budget;
    /// see [`OperatorDef::specify_state_bounds`].
    pub fn specify_state_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.def.specify_state_bounds(&self.params, builder);
    }

    /// Returns the estimate of the work the operator does per event, in nanoseconds, for R1's cost model; see
    /// [`OperatorDef::cost`].
    pub fn cost(&self) -> u32 {
        self.def.cost(&self.params)
    }

    /// Returns how to build the operator's kernel, or `None` if it has none.
    pub fn kernel(&self) -> Option<KernelSpec> {
        self.def.kernel(&self.params)
    }

    /// Returns the parameters a lineage of the operator's state is keyed by; see [`OperatorDef::lineage_params`].
    pub fn lineage_params(&self) -> Params {
        self.def.lineage_params(&self.params)
    }

    /// Returns the parameters the operator's kernel takes with each call, with the emission filter a rewrite pushed
    /// into it, if any; see [`OperatorDef::step_params`].
    pub fn step_params(&self, filter: Option<Arc<dyn EmissionFilter>>) -> Option<Arc<dyn Any + Send + Sync>> {
        self.def.step_params(&self.params, filter)
    }

    /// Returns the interval between the flush ticks of the operator's kernel, if it has time-driven work; see
    /// [`OperatorDef::flush_interval`].
    pub fn flush_interval(&self) -> Option<Duration> {
        self.def.flush_interval(&self.params)
    }
}

const NATIVE_OPERATORS: &[&dyn OperatorDef] = &[
    &Source,
    &DogstatsdMapper,
    &NamespacePrefix,
    &TimestampSplit,
    &TagProjection,
    &Aggregate,
    &HostEnrichment,
    &HostTags,
    &OpaqueHost,
    &Split,
    &Tee,
    &Merge,
    &Emit,
    &DropOperator,
];

/// The closed set of operators that plans can use.
#[derive(Clone, Copy, Debug)]
pub struct OperatorRegistry {
    operators: &'static [&'static dyn OperatorDef],
}

impl OperatorRegistry {
    /// Returns the registry of native operators: every operator this crate defines.
    pub const fn native() -> Self {
        Self {
            operators: NATIVE_OPERATORS,
        }
    }

    /// Returns a registry of the given operators, for tests that need operators outside the native set, such as one
    /// with a kernel before native kernels exist, or one that fails on purpose.
    ///
    /// Production code always uses [`native`][Self::native]: only the native set is trusted.
    #[cfg(any(test, feature = "test-util"))]
    pub const fn with_operators(operators: &'static [&'static dyn OperatorDef]) -> Self {
        Self { operators }
    }

    /// Returns the operator with the given name, if it is registered.
    pub fn get(&self, name: &str) -> Option<&'static dyn OperatorDef> {
        self.operators.iter().copied().find(|def| def.name() == name)
    }

    /// Returns an iterator over the registered operators.
    pub fn iter(&self) -> impl Iterator<Item = &'static dyn OperatorDef> {
        self.operators.iter().copied()
    }

    /// Configures the operator named `name` with a node's parameters.
    ///
    /// # Errors
    ///
    /// Returns an error if the operator is not registered, or if the parameters do not match its schema or break one of
    /// its rules. Every parameter problem is reported.
    pub fn configure(&self, name: &str, params: &Params) -> Result<ConfiguredOperator, ConfigureError> {
        let def = self.get(name).ok_or_else(|| ConfigureError::UnknownOperator {
            name: name.to_string(),
            known: self.iter().map(|def| def.name()).collect::<Vec<_>>().join(", "),
        })?;
        let params = decode_params(def.params(), params).map_err(|errors| ConfigureError::InvalidParams {
            operator: def.name(),
            errors,
        })?;
        let errors = def.check_params(&params);
        if !errors.is_empty() {
            return Err(ConfigureError::InvalidParams {
                operator: def.name(),
                errors,
            });
        }
        let ports = def.ports(&params);
        let contract = def.contract(&params);
        Ok(ConfiguredOperator {
            def,
            params,
            ports,
            contract,
        })
    }
}

impl Default for OperatorRegistry {
    fn default() -> Self {
        Self::native()
    }
}

/// Helpers shared by the operator definitions.
mod support {
    use std::{error::Error, time::Duration};

    use super::KernelBuildError;
    use crate::ir::ParamError;

    /// Returns an error's message followed by the messages of its sources, separated by `: `.
    pub(super) fn error_chain(error: &dyn Error) -> String {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        message
    }

    /// Returns the error of a kernel built from a parameter that is bound to configuration and has no resolved value.
    pub(super) fn unresolved(param: &str) -> KernelBuildError {
        KernelBuildError {
            message: format!(
                "the `{}` parameter is bound to configuration and has no resolved value; resolve the plan's bindings \
                 before building its kernels",
                param
            ),
        }
    }

    /// Returns an error at `path` with `message`.
    pub(super) fn error(path: &str, message: impl Into<String>) -> ParamError {
        ParamError {
            path: path.to_string(),
            message: message.into(),
        }
    }

    /// Checks that a known duration parameter is a whole number of seconds and, unless `allow_zero`, at least one
    /// second.
    pub(super) fn check_whole_seconds(
        name: &str, value: Option<Duration>, allow_zero: bool, errors: &mut Vec<ParamError>,
    ) {
        let Some(value) = value else { return };
        if value.subsec_nanos() != 0 {
            errors.push(error(name, "must be a whole number of seconds, such as \"10s\""));
        } else if value.is_zero() && !allow_zero {
            errors.push(error(name, "must be at least one second"));
        }
    }

    /// Checks that a known bucket width is a whole number of seconds and one of the allowed bucket widths
    /// ([`ALLOWED_BUCKET_WIDTHS`][crate::limits::ALLOWED_BUCKET_WIDTHS]).
    ///
    /// Buckets are aligned to Unix time, and the allowed widths line up at a common boundary every minute, or every 2,
    /// 3, or 5 minutes, which is where a change of width takes effect.
    pub(super) fn check_bucket_width(name: &str, value: Option<Duration>, errors: &mut Vec<ParamError>) {
        let before = errors.len();
        check_whole_seconds(name, value, false, errors);
        let Some(value) = value.filter(|_| errors.len() == before) else {
            return;
        };
        if !crate::limits::is_allowed_bucket_width(value) {
            errors.push(error(
                name,
                format!(
                    "must be one of {}, but is {} seconds: aggregation buckets must line up at a common boundary \
                     every minute (or every 2, 3, or 5 minutes) so that aggregation changes can switch at a bucket \
                     boundary",
                    saluki_metric_kernels::params::describe_allowed_widths(),
                    value.as_secs()
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests;
