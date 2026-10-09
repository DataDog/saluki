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
//! | `aggregate` | opaque host | `in`, `out` | Native aggregation into time buckets. |
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

use std::{fmt, sync::Arc};

use saluki_core::{
    accounting::MemoryBoundsBuilder, components::ComponentContext, execution::Kernel, support::SubsystemIdentifier,
    topology::ComponentId,
};
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
pub use self::aggregate::{histogram_params, Aggregate, HISTOGRAM_AGGREGATES};

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
    /// own bounds. The default declares nothing.
    fn specify_bounds(&self, _params: &DecodedParams, _builder: &mut MemoryBoundsBuilder) {}

    /// Returns how to build the operator's kernel, or `None` if it has none.
    ///
    /// The spec captures the parameters; the host supplies the rest when it builds the kernel, in a
    /// [`KernelBuildContext`]. Sources and opaque hosts never have kernels. No native operator has one yet: kernels
    /// are extracted from native components in a later stage.
    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
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
/// A kernel reports its telemetry and memory accounting, and names its internal caches, under its node's identity.
/// The context carries that identity: the node the kernel is built for, and the component context the host reports it
/// as. In a running topology the host supplies the component context of the native component the node stands for, so a
/// kernel extracted from a native component keeps that component's telemetry names and labels, whatever region runs
/// it. A host without a topology, such as the reference interpreter or a test, uses [`detached`][Self::detached].
///
/// # Missing
///
/// Stateful kernels will also find here the state budget they draw credits from, shared by every lineage of their
/// state owner, once state lineages exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelBuildContext {
    node: NodeId,
    component: ComponentContext,
}

impl KernelBuildContext {
    /// Creates a context for building the kernel of `node`, reported as the component `component`.
    pub fn new(node: NodeId, component: ComponentContext) -> Self {
        Self { node, component }
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
        Self {
            node: node.clone(),
            component: ComponentContext::transform(&topology, component_id),
        }
    }

    /// Returns the node the kernel is built for.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Returns the component context the kernel reports telemetry and memory accounting under.
    pub fn component(&self) -> &ComponentContext {
        &self.component
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

    /// Declares the memory the operator's kernel needs.
    pub fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.def.specify_bounds(&self.params, builder);
    }

    /// Returns how to build the operator's kernel, or `None` if it has none.
    pub fn kernel(&self) -> Option<KernelSpec> {
        self.def.kernel(&self.params)
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
    use std::time::Duration;

    use crate::ir::ParamError;

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
}

#[cfg(test)]
mod tests;
