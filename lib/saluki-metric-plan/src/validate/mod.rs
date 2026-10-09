//! Plan validation.
//!
//! [`Validator::validate`] checks a [`Plan`] against the [operator registry][OperatorRegistry] and a [`Catalog`] of the
//! outputs, configuration bindings, and scope inputs the host declares, and returns every finding: errors, which make
//! the plan invalid, and warnings, which do not. Each finding names the [rule][Rule] it reports and where in the plan
//! the problem is. The default program is validated by the same rules as any other plan.
//!
//! The rules, grouped as the specification groups them:
//!
//! - **Structure:** node IDs are unique; operators are registered and their parameters valid; edges refer to existing
//!   ports; every port has exactly one edge; the graph is acyclic, checked from every node; every node is reachable
//!   from a source; and every path ends in an `emit` or a `drop`.
//! - **Types:** port types are propagated from the sources through every operator. An operator that cannot receive its
//!   input, or an `emit` whose output does not accept it, is an error. A `split` whose predicate can never match its
//!   input, or a port that can never carry anything, is a warning.
//! - **References:** outputs, configuration bindings, and scope inputs must be declared by the catalog, and agree with
//!   it.
//! - **Duplicate contribution:** no structure may deliver one contribution twice to the same output, counting outputs
//!   that share a recipient as one (see [the analysis](#duplicate-analysis)).
//! - **Scopes:** every scope's inputs are declared by the catalog, and every scope is closed, so that an artifact
//!   replacing it changes nothing outside it.
//! - **Bounds:** the [limits][crate::limits] on artifacts apply to nodes with artifact provenance only, so that large
//!   native configurations stay valid.
//! - **Authority:** every lossy node has provenance, only native configuration places host-owned operators, nodes in
//!   the policy namespace come from artifacts, and every artifact revision a node names is one the plan lists.
//!
//! # Duplicate analysis
//!
//! For every group of outputs that share a recipient, every distinct path from a source to an `emit` of the group is
//! enumerated, up to the [work limit][crate::limits::WORK_PER_INPUT]. Two paths from different sources carry different
//! contributions. Two paths from the same source first differ at a node where they leave by different output ports:
//! that node is their divergence. If its ports are point-disjoint, as for a `split` or a `timestamp_split`, the paths
//! carry different points. If its ports are copies, as for a `tee`, the pair is rejected, unless after the divergence
//! one path passes the `true` port of a `split` and the other the `false` port of a `split` whose predicate has the
//! same canonical form, and no operator from the divergence up to either `split` writes a field the predicate reads.
//! Each rejection names both paths and the divergence.
//!
//! This analysis is conservative: it treats every series reaching an output as potentially overlapping, so it rejects
//! some plans that never deliver a contribution twice, such as a `tee` into two aggregations whose names differ.
//!
//! Its cost is bounded by the work limit whatever the plan: at most that many paths per group of outputs, and at most
//! its square in comparisons of pairs of paths in the whole plan, each a few word operations, since predicates are
//! compared by number rather than by text. A plan beyond either bound is reported as `duplicate_analysis_limit`.

use std::{collections::BTreeMap, fmt};

use crate::{
    ir::{ArtifactRevision, NodeId, OutputId, Plan, PortName, PortRef, ScopeName},
    model::PortType,
    operators::OperatorRegistry,
};

mod bounds;
mod catalog;
pub use self::catalog::{Catalog, CatalogBinding, CatalogOutput, Liveness, MemoryCatalog};

mod duplicates;
mod graph;
mod references;
pub(crate) use self::references::binding_uses;
mod types;
pub(crate) use self::types::describe as describe_port_type;

/// How serious a finding is.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    /// The plan is invalid.
    Error,

    /// The plan is valid, but probably not what its author meant.
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Error => "error",
            Self::Warning => "warning",
        })
    }
}

macro_rules! rules {
    ($($(#[$meta:meta])* $variant:ident => $name:literal, $severity:ident;)*) => {
        /// A validation rule.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum Rule {
            $($(#[$meta])* $variant,)*
        }

        impl Rule {
            /// Every rule, in the order validation checks them.
            pub const ALL: &'static [Rule] = &[$(Self::$variant,)*];

            /// Returns the rule's stable name.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Returns how serious a finding of this rule is.
            pub const fn severity(self) -> Severity {
                match self {
                    $(Self::$variant => Severity::$severity,)*
                }
            }
        }
    };
}

rules! {
    /// The plan is written in the IR version this validator checks.
    IrVersion => "ir_version", Error;
    /// No two nodes have the same ID.
    UniqueNodeIds => "unique_node_ids", Error;
    /// Every node's operator is registered.
    KnownOperator => "known_operator", Error;
    /// Every node's parameters match its operator's schema and rules.
    ValidParameters => "valid_parameters", Error;
    /// Every edge leaves an existing output port and enters an existing input port.
    EdgeReferences => "edge_references", Error;
    /// Every input port has exactly one edge, and no output port has more than one: fan-out and fan-in use `tee` and
    /// `merge`.
    OneEdgePerPort => "one_edge_per_port", Error;
    /// Every output port has an edge, so every path ends in an `emit` or a `drop`.
    PathsEndInEmitOrDrop => "paths_end_in_emit_or_drop", Error;
    /// The graph has no cycle.
    Acyclic => "acyclic", Error;
    /// Every node is reachable from a source.
    Reachable => "reachable", Error;
    /// No two sources bind the same host source, which enters the plan once.
    UniqueSources => "unique_sources", Error;
    /// Every node in the policy namespace has artifact provenance: only artifacts add policy-namespace nodes.
    ProvenanceNamespace => "provenance_namespace", Error;
    /// Every operator accepts the records it can receive.
    PortTypes => "port_types", Error;
    /// Every `emit`'s output accepts the records it can receive.
    OutputAccepts => "output_accepts", Error;
    /// A `split`'s predicate can match some record it can receive.
    PredicateCanMatch => "predicate_can_match", Warning;
    /// Every output port can carry some record.
    PortCanCarry => "port_can_carry", Warning;
    /// No two declared outputs have the same ID.
    UniqueOutputIds => "unique_output_ids", Error;
    /// Every `emit` delivers to an output the plan declares.
    DeclaredOutput => "declared_output", Error;
    /// Every declared output is declared by the catalog, with the same binding.
    CatalogOutput => "catalog_output", Error;
    /// Every declared output is emitted to.
    UnusedOutput => "unused_output", Warning;
    /// Every configuration binding is declared by the catalog.
    KnownBinding => "known_binding", Error;
    /// Every configuration binding's type is admitted by the parameter it supplies.
    BindingType => "binding_type", Error;
    /// Every use of one configuration binding has the same resolved value.
    ConsistentBinding => "consistent_binding", Error;
    /// A host-owned node is not bound to live configuration, which it only applies at restart.
    LiveBindingOnHost => "live_binding_on_host", Warning;
    /// Every scope input refers to an output port, is declared by the catalog, and carries what the catalog declares.
    ScopeInputs => "scope_inputs", Error;
    /// Every scope is closed: only its inputs feed it from outside, its exits lead out of it, and no two scopes share a
    /// node.
    ScopeClosed => "scope_closed", Error;
    /// No structure can deliver one contribution twice to outputs that share a recipient.
    DuplicateContribution => "duplicate_contribution", Error;
    /// The duplicate analysis completes within its work limit.
    DuplicateAnalysisLimit => "duplicate_analysis_limit", Error;
    /// An artifact has no more nodes than the limit.
    ArtifactNodes => "artifact_nodes", Error;
    /// An artifact's predicates have no more leaves than the limit.
    PredicateLeaves => "predicate_leaves", Error;
    /// An artifact's `tee`s have no more output ports than the limit.
    TeeFanOut => "tee_fan_out", Error;
    /// One record entering an artifact causes no more node visits than the limit.
    WorkPerInput => "work_per_input", Error;
    /// Every lossy node has provenance: native configuration or an approved artifact.
    LossAuthority => "loss_authority", Error;
    /// Host-owned operators, such as sources and opaque hosts, come only from native configuration.
    TrustedHost => "trusted_host", Error;
    /// Every node's artifact provenance names an artifact revision that the plan's provenance lists.
    ProvenanceConsistency => "provenance_consistency", Error;
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where in a plan a finding is.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Location {
    /// The plan as a whole.
    Plan,

    /// A node, and optionally one of its ports.
    Node {
        /// The node.
        id: NodeId,

        /// The port, if the finding concerns one.
        port: Option<PortName>,
    },

    /// An edge, by its position in the plan's edge list, counting from zero.
    Edge {
        /// The edge's position.
        index: usize,
    },

    /// A declared output.
    Output(OutputId),

    /// A scope.
    Scope(ScopeName),
}

impl Location {
    /// Returns the node the finding is at, if it is at a node.
    pub fn node(&self) -> Option<&NodeId> {
        match self {
            Self::Node { id, .. } => Some(id),
            _ => None,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan => f.write_str("plan"),
            Self::Node { id, port: None } => write!(f, "node `{}`", id),
            Self::Node { id, port: Some(port) } => write!(f, "port `{}.{}`", id, port),
            Self::Edge { index } => write!(f, "edge {}", index),
            Self::Output(id) => write!(f, "output `{}`", id),
            Self::Scope(name) => write!(f, "scope `{}`", name),
        }
    }
}

/// One validation finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    /// How serious the finding is: the severity of its rule.
    pub severity: Severity,

    /// The rule the finding reports.
    pub rule: Rule,

    /// Where the problem is.
    pub location: Location,

    /// What is wrong, and how to fix it.
    pub message: String,
}

impl Finding {
    fn new(rule: Rule, location: Location, message: impl Into<String>) -> Self {
        Self {
            severity: rule.severity(),
            rule,
            location,
            message: message.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}[{}] {}: {}",
            self.severity, self.rule, self.location, self.message
        )
    }
}

/// What a [bound][BoundMeasure] covers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BoundScope {
    /// Every node of the plan. Limits are not enforced on the plan as a whole: they apply to artifacts, so that large
    /// native configurations stay valid.
    Plan,

    /// The nodes added or changed by one artifact revision. Limits are enforced on these.
    Artifact(ArtifactRevision),
}

/// One resource bound of a plan, measured against its [limit][crate::limits].
///
/// Validation measures every bound of the whole plan, and of each artifact revision whose nodes the plan contains.
/// Only the measures of artifacts are enforced: one that exceeds its limit is also reported as a finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundMeasure {
    /// The name of the limit, as the [limits table][crate::limits::LIMITS] lists it.
    pub limit: &'static str,

    /// The limit's value.
    pub maximum: u64,

    /// What the measure covers.
    pub scope: BoundScope,

    /// The measured value, or `None` if it is unbounded, as it is downstream of an expansion without a declared
    /// maximum.
    pub value: Option<u64>,
}

impl BoundMeasure {
    /// Returns `true` if the limit is enforced on this measure: if it covers an artifact.
    pub fn is_enforced(&self) -> bool {
        matches!(self.scope, BoundScope::Artifact(_))
    }

    /// Returns `true` if the measured value is bounded and at most the limit.
    pub fn is_within(&self) -> bool {
        self.value.is_some_and(|value| value <= self.maximum)
    }
}

/// The findings of validating one plan, and what validation computed: port types and resource bounds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ValidationReport {
    findings: Vec<Finding>,
    port_types: BTreeMap<PortRef, PortType>,
    bounds: Vec<BoundMeasure>,
}

impl ValidationReport {
    /// Returns the type of an output port, if validation could compute it.
    ///
    /// Types are computed from the sources downstream; a port downstream of a structural problem, such as a cycle or
    /// an unknown operator, has no type.
    pub fn port_type(&self, port: &PortRef) -> Option<PortType> {
        self.port_types.get(port).copied()
    }

    /// Returns every output port whose type validation computed, with its type, in port order.
    pub fn port_types(&self) -> impl Iterator<Item = (&PortRef, &PortType)> {
        self.port_types.iter()
    }

    /// Returns the resource bounds validation measured: first those of the whole plan, then those of each artifact
    /// revision, in revision order.
    ///
    /// The bounds are the number of nodes, the number of predicate leaves, the largest number of copies one node
    /// makes, and the worst-case number of node visits one record entering the scope causes. The last is missing when
    /// the plan has a cycle.
    pub fn bounds(&self) -> &[BoundMeasure] {
        &self.bounds
    }

    /// Returns every finding, in the order validation found them.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Returns the errors.
    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
    }

    /// Returns the warnings.
    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
    }

    /// Returns `true` if the plan has no errors.
    pub fn is_valid(&self) -> bool {
        self.errors().next().is_none()
    }

    fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for finding in &self.findings {
            writeln!(f, "{}", finding)?;
        }
        Ok(())
    }
}

/// Validates plans against an operator registry and a catalog.
#[derive(Clone, Copy)]
pub struct Validator<'a> {
    registry: &'a OperatorRegistry,
    catalog: &'a dyn Catalog,
}

impl<'a> Validator<'a> {
    /// Creates a validator.
    pub fn new(registry: &'a OperatorRegistry, catalog: &'a dyn Catalog) -> Self {
        Self { registry, catalog }
    }

    /// Validates a plan, returning every finding.
    pub fn validate(&self, plan: &Plan) -> ValidationReport {
        let mut report = ValidationReport::default();

        if plan.ir_version != crate::ir::IR_VERSION {
            report.push(Finding::new(
                Rule::IrVersion,
                Location::Plan,
                format!(
                    "the plan is written in IR version {}, but this validator checks version {}; read it with \
                     `Plan::from_json`, which migrates accepted older versions",
                    plan.ir_version,
                    crate::ir::IR_VERSION
                ),
            ));
        }

        let graph = graph::Graph::build(plan, self.registry, &mut report);
        let types = types::check(&graph, plan, &mut report);
        for (node, outputs) in types.outputs.iter().enumerate() {
            let Some(op) = graph.op(node) else { continue };
            for (port, port_type) in op.ports().outputs.iter().zip(outputs) {
                if let Some(port_type) = port_type {
                    let port = PortRef::new(graph.id(node).clone(), port.name.clone());
                    report.port_types.insert(port, *port_type);
                }
            }
        }
        references::check(&graph, &types, plan, self.catalog, &mut report);
        duplicates::check(&graph, &types, plan, &mut report);
        bounds::check(&graph, plan, &mut report);

        report
    }
}

#[cfg(test)]
mod default_program_tests;
#[cfg(test)]
mod tests;
