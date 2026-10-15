//! Receiving a policy artifact, and composing it into the default program.
//!
//! An artifact from the channel goes through two steps before it becomes a candidate plan:
//!
//! 1. **Receipt** ([`receive_artifact`]): the document is checked against the artifact limits (its size, also with its
//!    blob references counted as copies of the blobs, node count, blob sizes, and predicate leaves), read in a
//!    supported IR version and migrated to the current one, and refused if any node or blob carries a resolved
//!    configuration value or its blob table is inconsistent. The result is a [`ReceivedArtifact`], with the hash of the
//!    document as received, which approval binds to.
//! 2. **Composition** ([`compose`], or [`compose_all`] for one artifact per scope): each artifact replaces its scope in
//!    the current default program, reaching the scope's inputs as `input.<name>` and its exits as `exit.<name>`. Each
//!    artifact node gets its plan ID by correspondence (see [`Correspondence`]): a reserved node the artifact keeps
//!    unmodified is the default program's node exactly, and every other node is the artifact's as written. Bindings
//!    resolve from the catalog, and the composed plan is validated, with the artifact limits applied to the program's
//!    nodes, every node of the scope the artifact replaces. The plan's provenance names each artifact and its scope:
//!    what a plan runs is a property of the plan, not of its nodes. Composition also reports whether each artifact's
//!    base no longer matches the default program (a stale base: it still installs) and whether it was approved against
//!    another active plan (a stale review: it still installs).
//!
//! Artifacts reference outputs by ID only, native ones and those a resources artifact declares, never declaring one.
//! An artifact may hold state owners, such as `aggregate`: whether the frame can run them where the artifact puts them
//! is placement's to decide, and installing a change to one is a cut.
//!
//! # Outputs that change under an artifact
//!
//! An artifact records the fingerprint of every output it emits to. Composition compares each with the host's
//! catalog, and what it does when they differ depends on whether the artifact is new or the one in effect, composed
//! again after the default program changed ([`Approval::in_effect`]):
//!
//! - **Declared with the recorded fingerprint:** it composes.
//! - **Declared with another fingerprint**, or, for a routing group, whose output IDs are positional, another routing
//!   group's output carries the recorded one (the groups were renumbered): a new artifact is refused
//!   ([`ComposeError::ReboundOutput`]); the artifact in effect composes with every `emit` to the output a counted drop
//!   ([`ComposeOutcome::Degraded`]).
//! - **Not declared:** a new artifact waits for it, a held state ([`ComposeError::PendingOutput`]); the artifact in
//!   effect composes with its `emit`s dropped, as above. Renumbering is for routing groups only: a declared output's
//!   fingerprint covers its recipient list, not its name, so another declared output with the same recipients carries
//!   the same fingerprint, and an artifact waiting for the first still waits.
//! - **Emitted to but not recorded**, and declared or named `resources_<name>`: refused
//!   ([`ComposeError::UnrecordedOutput`]); the artifact in effect cannot have such an `emit`, since it composed before.
//! - **Neither recorded nor declared:** refused ([`ComposeError::UndeclaredOutput`]).
//!
//! Removal by the resource authority fails closed: the artifact in effect keeps running, but nothing it sends an output
//! that was removed or rebound reaches that output, and nothing else it does is undone. A dropped `emit` keeps its node
//! ID and becomes a `drop`, so its loss is the program's policy loss at that node. When the output returns with the
//! recorded fingerprint, the next composition restores the `emit`.
//!
//! An artifact that records an output's fingerprint as an earlier fingerprint version computed it, such as one kept in
//! an old run directory, is an artifact whose output now delivers elsewhere: a new one is refused, asking for a rebase,
//! and one in effect runs degraded until it is republished.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{contains_resolved_values, to_canonical_string, PlanHash, BINDING_MEMBER, RESOLVED_MEMBER},
    export::{emitted_output, operation_params, unresolved, ScopeRegion},
    install::{
        correspondence::{CorrespondenceKind, NodeCorrespondence},
        Correspondence,
    },
    ir::{
        decode_params, ArtifactBase, ArtifactNodeId, ArtifactPortRef, BindingName, BlobError, Blobs, ComposedArtifact,
        Edge, HostId, IrError, Node, NodeId, OperatorName, OutputId, Params, Plan, PlanProvenance, PolicyArtifact,
        PortRef, ReceivedArtifact, RecipientId, RecipientName, ScopeDecl, ScopeName, Selector, BLOB_MEMBER,
    },
    limits::values::{ARTIFACT_NODES, ARTIFACT_SIZE, BLOB_SIZE, PREDICATE_LEAVES},
    operators::{names, OperatorRegistry, StateContract, EMIT_TO},
    validate::{predicate_leaves, BindingClass, Catalog, ValidationReport, Validator},
};

/// Reads an artifact document received from the channel, after checking it against the artifact limits.
///
/// The checks run on the document as received, before it is composed, in this order:
///
/// 1. its size, the length of its canonical form (which is what its hash covers), within [`ARTIFACT_SIZE`] bytes;
/// 2. it is read as [`ReceivedArtifact::from_value`] reads it: in an accepted IR version, migrated to the current one,
///    with no resolved configuration values in its nodes' parameters, and with a consistent blob table;
/// 3. its node count within [`ARTIFACT_NODES`];
/// 4. each blob's size, the length of its canonical form, within [`BLOB_SIZE`] bytes, and no resolved configuration
///    value in any blob's content;
/// 5. its inlined size, the size it would have written without blobs (every blob reference replaced by the blob's
///    content, and no blob table), within [`ARTIFACT_SIZE`] bytes. Composition replaces each reference with a copy of
///    the blob, so this bounds what the composed plan holds of the artifact: blobs never let an artifact cost the host
///    more than an artifact written without them may. It is computed from the sizes alone, without copying any blob;
/// 6. the leaves of all its predicates together, with blob references replaced by their content, within
///    [`PREDICATE_LEAVES`]. A node whose operator is not native, or whose parameters its operator does not accept,
///    counts none here: validation rejects it.
///
/// These bound what the host does with a document before it knows what the document does, so they count every node of
/// the artifact, kept from the default program or not. Validation of the composed plan applies the node and leaf limits
/// again to the program's nodes, with their bindings resolved.
///
/// # Errors
///
/// Returns the first check the document fails.
pub fn receive_artifact(document: Value) -> Result<ReceivedArtifact, ReceiptError> {
    // A document without a canonical form is refused by the read, which names the offending number.
    let canonical = to_canonical_string(&document);
    if let Ok(canonical) = &canonical {
        let size = canonical.len() as u64;
        if size > ARTIFACT_SIZE {
            return Err(ReceiptError::TooLarge {
                size,
                limit: ARTIFACT_SIZE,
            });
        }
    }
    // The canonical form holds the blob table as the member `"blobs":<table>` and the comma that separates it from
    // another member (an artifact has several), which an artifact written without blobs does not have.
    let table = document
        .get(BLOBS)
        .and_then(|table| to_canonical_string(table).ok())
        .map_or(0, |table| (BLOBS.len() + 3 + table.len() + 1) as u64);

    let received = ReceivedArtifact::from_value(document).map_err(|source| ReceiptError::Document { source })?;
    let artifact = &received.artifact;
    // Reading the artifact hashed the document, so it has a canonical form.
    let size = canonical.map_or(0, |canonical| canonical.len() as u64);

    let nodes = artifact.nodes.len() as u64;
    if nodes > ARTIFACT_NODES {
        return Err(ReceiptError::TooManyNodes {
            nodes,
            limit: ARTIFACT_NODES,
        });
    }

    let mut blob_sizes = BTreeMap::new();
    for (blob, content) in artifact.blobs.iter() {
        // Reading the artifact verified every blob's hash, so every blob has a canonical form.
        let size = to_canonical_string(content).map_or(0, |canonical| canonical.len() as u64);
        if size > BLOB_SIZE {
            return Err(ReceiptError::BlobTooLarge {
                blob: *blob,
                size,
                limit: BLOB_SIZE,
            });
        }
        if contains_resolved_values(content) {
            return Err(ReceiptError::ResolvedInBlob { blob: *blob });
        }
        blob_sizes.insert(*blob, size);
    }

    let inlined = inlined_size(size.saturating_sub(table), artifact, &blob_sizes);
    if inlined > ARTIFACT_SIZE {
        return Err(ReceiptError::TooLargeInlined {
            size: inlined,
            limit: ARTIFACT_SIZE,
        });
    }

    let leaves = artifact_predicate_leaves(artifact, &OperatorRegistry::native());
    if leaves > PREDICATE_LEAVES {
        return Err(ReceiptError::TooManyPredicateLeaves {
            leaves,
            limit: PREDICATE_LEAVES,
        });
    }

    Ok(received)
}

/// The member of an artifact document that holds its blob table.
const BLOBS: &str = "blobs";

/// Returns the size of an artifact with every blob reference replaced by the blob's content: `size`, the canonical
/// length of its document without its blob table, less the canonical length of each reference in its nodes'
/// parameters, plus the canonical length of the blob the reference names, from `blob_sizes`.
///
/// Nothing is copied, so measuring a document that references one blob many times costs no more than reading it.
fn inlined_size(size: u64, artifact: &PolicyArtifact, blob_sizes: &BTreeMap<PlanHash, u64>) -> u64 {
    fn measure(value: &Value, blob_sizes: &BTreeMap<PlanHash, u64>, size: &mut u64) {
        match value {
            Value::Object(members) if members.contains_key(BLOB_MEMBER) => {
                // Reading the artifact checked every reference: it names a blob the table holds, and blobs hold
                // values, never references.
                let reference = to_canonical_string(value).map_or(0, |canonical| canonical.len() as u64);
                let blob = members
                    .get(BLOB_MEMBER)
                    .and_then(Value::as_str)
                    .and_then(|hash| hash.parse::<PlanHash>().ok())
                    .and_then(|hash| blob_sizes.get(&hash).copied())
                    .unwrap_or(0);
                *size = size.saturating_sub(reference).saturating_add(blob);
            }
            Value::Object(members) => members.values().for_each(|member| measure(member, blob_sizes, size)),
            Value::Array(values) => values.iter().for_each(|element| measure(element, blob_sizes, size)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    let mut inlined = size;
    for node in &artifact.nodes {
        for (_, value) in node.params.iter() {
            measure(value, blob_sizes, &mut inlined);
        }
    }
    inlined
}

/// Returns the number of predicate leaves in an artifact's nodes, with blob references replaced by their content.
///
/// A node whose operator is not registered, or whose parameters do not decode against its operator's schema, counts
/// none.
fn artifact_predicate_leaves(artifact: &PolicyArtifact, registry: &OperatorRegistry) -> u64 {
    artifact
        .nodes
        .iter()
        .filter_map(|node| {
            let def = registry.get(node.op.as_str())?;
            let params = inline_blobs(&node.params, &artifact.blobs).ok()?;
            let decoded = decode_params(def.params(), &params).ok()?;
            Some(decoded.iter().map(|(_, value)| predicate_leaves(value)).sum::<u64>())
        })
        .fold(0u64, u64::saturating_add)
}

/// An artifact document refused on receipt.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ReceiptError {
    /// The artifact's canonical form is larger than the artifact size limit.
    #[snafu(display(
        "The artifact is {} bytes, more than the artifact size limit of {} bytes, which its blobs count toward. Split \
         the policy, or shorten its largest values.",
        size,
        limit
    ))]
    TooLarge {
        /// The artifact's size, in bytes.
        size: u64,

        /// The limit.
        limit: u64,
    },

    /// The artifact, written without blobs (each blob reference replaced by the blob's content), is larger than the
    /// artifact size limit.
    #[snafu(display(
        "The artifact is {} bytes with its blob references replaced by their content, more than the artifact size \
         limit of {} bytes. Each reference to a blob counts as a copy of it, since composition copies the blob into \
         every place that refers to it: refer to large blobs from fewer places, or split the policy.",
        size,
        limit
    ))]
    TooLargeInlined {
        /// The artifact's size written without blobs, in bytes.
        size: u64,

        /// The limit.
        limit: u64,
    },

    /// The artifact has more nodes than the artifact node limit.
    #[snafu(display("The artifact has {} nodes, more than the limit of {}.", nodes, limit))]
    TooManyNodes {
        /// The artifact's node count.
        nodes: u64,

        /// The limit.
        limit: u64,
    },

    /// The artifact's predicates have more leaves, together, than the predicate leaf limit.
    #[snafu(display(
        "The artifact's predicates have {} leaves together, more than the limit of {}.",
        leaves,
        limit
    ))]
    TooManyPredicateLeaves {
        /// The artifact's predicate leaves.
        leaves: u64,

        /// The limit.
        limit: u64,
    },

    /// A blob is larger than the blob size limit.
    #[snafu(display("Blob {} is {} bytes, more than the blob size limit of {} bytes.", blob, size, limit))]
    BlobTooLarge {
        /// The blob's hash.
        blob: PlanHash,

        /// The blob's size, in bytes.
        size: u64,

        /// The limit.
        limit: u64,
    },

    /// A blob's content carries the resolved value of a configuration binding.
    #[snafu(display(
        "Blob {} carries a resolved configuration value. Artifacts refer to configuration by binding only, as \
         {{\"config\": \"<binding>\"}}: remove the `resolved` member from the blob, or write the value as a literal.",
        blob
    ))]
    ResolvedInBlob {
        /// The blob's hash.
        blob: PlanHash,
    },

    /// The document is not an artifact this version reads: its IR version is not accepted or fails to migrate, it does
    /// not match the artifact's structure, a node carries a resolved value, or its blob table is inconsistent.
    #[snafu(display("{}", source))]
    Document {
        /// The underlying error.
        source: IrError,
    },
}

/// What an artifact was approved as, and what it is composed against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Approval {
    /// The artifact's revision, from its envelope.
    pub revision: u64,

    /// The active plan hash the approver reviewed the artifact against, from its envelope, if it says.
    pub approved_against: Option<PlanHash>,

    /// The host's active plan hash (with resolved values) when the artifact is composed, if a generation is active.
    ///
    /// When `approved_against` is given and differs, the composed artifact has a stale review.
    pub active_plan_hash: Option<PlanHash>,

    /// Whether the artifact is in effect, composed again because the default program changed, rather than composed to
    /// be installed for the first time.
    ///
    /// The artifact in effect composes with every `emit` to an output that was removed, or now delivers elsewhere,
    /// replaced by a counted drop, and runs degraded; a new artifact is refused instead, pending while its output is
    /// not declared (see [Outputs that change under an artifact](self#outputs-that-change-under-an-artifact)).
    pub in_effect: bool,
}

/// Artifacts composed into the default program, one per scope.
#[derive(Clone, Debug)]
pub struct Composed {
    /// The composed plan, with every configuration binding resolved, and the artifacts it composes in its provenance.
    pub plan: Plan,

    /// The composed plan's validation report. An artifact whose report has errors is rejected.
    pub report: ValidationReport,

    /// Which plan node each artifact node became, and which reserved nodes of the scope the artifact left out, when
    /// one artifact was composed. When several were, it is empty: each artifact's is in [`scopes`][Self::scopes], since
    /// artifacts of different scopes can name their nodes alike.
    pub correspondence: Correspondence,

    /// Whether an artifact's `base.program_hash` differs from the default program's current base of its scope: the
    /// scope changed structurally, or an output it emits to was rebound, since the artifact's export. The artifact
    /// still installs, reported stale.
    pub stale_base: bool,

    /// Whether an artifact was approved against an active plan other than the one it is composed against. The artifact
    /// still installs, with its review reported stale.
    pub stale_review: bool,

    /// How each scope was composed, by scope: one entry per artifact.
    pub scopes: BTreeMap<ScopeName, ScopeComposition>,
}

/// How one artifact was composed into its scope.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScopeComposition {
    /// Which plan node each artifact node became, and which reserved nodes of the scope the artifact left out.
    pub correspondence: Correspondence,

    /// Whether the artifact's `base.program_hash` differs from the default program's current base of the scope.
    pub stale_base: bool,

    /// Whether the artifact was approved against an active plan other than the one it is composed against.
    pub stale_review: bool,

    /// Whether the artifact's `emit`s were composed as written, or, for the artifact in effect, some were dropped.
    pub outcome: ComposeOutcome,
}

impl ScopeComposition {
    /// Returns what the artifact's acknowledgment says about its composition beside its state, in plain text: why it
    /// runs degraded, or `None` if there is nothing to say.
    pub fn notes(&self) -> Option<String> {
        match &self.outcome {
            ComposeOutcome::Degraded(dropped) => {
                Some(dropped.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
            }
            ComposeOutcome::Composed => None,
        }
    }
}

/// Whether an artifact's `emit`s were composed as written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ComposeOutcome {
    /// Every `emit` delivers to the output the artifact's base records.
    #[default]
    Composed,

    /// The artifact is in effect, and some outputs it records were removed or now deliver elsewhere: every `emit` to
    /// them was replaced by a counted drop. The artifact runs degraded, and its acknowledgment is `active` with the
    /// reasons, until the outputs return or it is rebased.
    Degraded(Vec<DroppedEmits>),
}

impl ComposeOutcome {
    /// Returns `true` if some `emit`s were dropped.
    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded(_))
    }
}

/// What dropped `emit`s delivered to: an output, by its ID, or recipients, by the names the artifact gives them.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DropTarget {
    /// An output the artifact's base records.
    Output(OutputId),

    /// The recipients an `emit` names.
    Recipients(Vec<RecipientName>),
}

/// The `emit`s of one output, or one set of recipients, that composition dropped, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroppedEmits {
    /// What they delivered to.
    pub target: DropTarget,

    /// Why its `emit`s were dropped.
    pub reason: DropReason,

    /// The plan nodes that were `emit`s to the output and are `drop`s now, in artifact order.
    pub nodes: Vec<NodeId>,
}

impl std::fmt::Display for DroppedEmits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output = match &self.target {
            DropTarget::Output(output) => output.to_string(),
            DropTarget::Recipients(names) => format!("to {}", describe_names(names)),
        };
        match &self.reason {
            DropReason::Removed => write!(
                f,
                "output `{}` is no longer declared; its emits are dropped until it returns",
                output
            ),
            DropReason::Rebound { renumbered: None } => write!(
                f,
                "output `{}` now delivers elsewhere; its emits are dropped; rebase",
                output
            ),
            DropReason::Rebound { renumbered: Some(now) } => write!(
                f,
                "output `{}` renumbered: the recorded output is now `{}`; its emits are dropped; rebase",
                output, now
            ),
            DropReason::Unavailable(why) => write!(
                f,
                "the emits {} are dropped until they are available again: {}",
                output, why
            ),
        }
    }
}

/// Why composition dropped the `emit`s to an output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DropReason {
    /// The host no longer declares the output, and no routing group's output carries the fingerprint the base records.
    Removed,

    /// The host declares the output with another fingerprint, so that it now delivers elsewhere; or another routing
    /// group's output carries the fingerprint the base records, as after the groups are renumbered.
    Rebound {
        /// The output that now carries the recorded fingerprint, if one does.
        renumbered: Option<OutputId>,
    },

    /// The recipients an `emit` names are no longer available, as the plain-text reason says: a name the directory no
    /// longer has, names of recipients on more than one delivery host, or no output for their recipients.
    Unavailable(String),
}

/// Writes recipient names as a program does, as `[primary, "https://intake.example.com"]`.
fn describe_names(names: &[RecipientName]) -> String {
    let names = names
        .iter()
        .map(|name| {
            if crate::ir::is_identifier(name.as_str()) {
                name.to_string()
            } else {
                // Quoted as a string literal, with its quotes and backslashes escaped.
                serde_json::to_string(name.as_str()).unwrap_or_else(|_| format!("\"{}\"", name))
            }
        })
        .collect::<Vec<_>>();
    format!("[{}]", names.join(", "))
}

/// Composes a received artifact into the default program, replacing the artifact's scope.
///
/// This is [`compose_all`] of one artifact; see there.
///
/// # Errors
///
/// Returns an error if the artifact cannot be composed at all; see [`ComposeError`].
pub fn compose(
    default_plan: &Plan, artifact: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Composed, ComposeError> {
    compose_all(default_plan, &[(artifact, approval)], catalog, registry)
}

/// Composes received artifacts, at most one per scope, each with its approval, into the default program, each
/// replacing its scope.
///
/// `default_plan` is the default program as lowered, with its bindings resolved, and `catalog` the host's catalog of
/// the same configuration. Each scope is composed on its own, against the default program, with its own correspondence
/// and base check; scopes never share a node, so the order does not matter, and they are composed in scope order. Then
/// the plan is validated once.
///
/// - An artifact must replace a scope the default program declares, and reach its inputs as `input.<name>` and its
///   exits as `exit.<name>` (only as an edge's target). Its nodes replace the scope's region (every node reachable from
///   the scope's inputs without entering one of its exits' ports), and its edges replace the edges that enter the
///   region and those that leave it into an exit; the rest of the default program, its outputs, and its scopes stay as
///   they are.
/// - An artifact node whose local ID equals a reserved ID inside the scope keeps that ID. If its operator and
///   parameters are the default program's (parameters compared without resolved values, as their operator decodes
///   them, so that a set written in another order is the same set), the node is the default program's node exactly,
///   with its resolved values. Otherwise it is the artifact's node as written. Every other artifact node is placed as
///   `policy/<local>`. A local ID equal to a reserved ID outside the scope is rejected. Every node of the scope is the
///   program's either way: what a node may do, and who answers for its loss, never depends on whether the author kept
///   it, so keeping a node unmodified only spares the installer rebuilding it.
/// - Where the catalog runs at most one aggregation per program ([`Catalog::single_aggregation`]), an artifact's one
///   state owner takes the reserved ID of the region's one state owner, whatever the artifact calls it, unless another
///   artifact node has that ID: one aggregation is the aggregation. It is then composed as any node with that ID is.
/// - Blob references are replaced with their content, and the configuration bindings of the nodes the artifact modifies
///   or adds resolve from `catalog`. A binding the catalog does not declare is left unresolved, for validation to
///   report; one it declares without a value is rejected.
/// - Every output an `emit` of an artifact delivers to must be recorded in the artifact's base, and declared by the
///   catalog with the fingerprint the base records. One that is not declared, or declared with another fingerprint,
///   refuses a new artifact (pending, or rebound), and turns the `emit`s of the artifact in effect into counted drops
///   ([`ComposeOutcome::Degraded`]); see [Outputs that change under an
///   artifact](self#outputs-that-change-under-an-artifact).
/// - State owners, such as `aggregate`, compose like any node: placement decides whether the frame can run them where
///   the artifact puts them, and validation rejects host components in artifacts.
/// - The composed plan's provenance lists every artifact's revision and scope, in scope order, and its nodes and edges
///   are in the default program's order, each artifact's taking the places of its region's in order (any left over
///   follow the last of them). An artifact's edges are first put in the default program's order: an edge the region has
///   takes that edge's place, and a new edge follows the region's edge the artifact lists before it. So composing an
///   unmodified export gives the default program again, with the same hash, whatever order its edges are written in, as
///   a printed and parsed export writes them.
/// - The composed plan is validated with `registry` and `catalog`; the report is returned, not judged, so the caller
///   rejects an artifact with errors as it does any candidate.
///
/// # Errors
///
/// Returns an error if no artifact, or more than one for a scope, is given, or if an artifact cannot be composed at
/// all; see [`ComposeError`].
pub fn compose_all(
    default_plan: &Plan, artifacts: &[(&ReceivedArtifact, &Approval)], catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Composed, ComposeError> {
    let mut by_scope = BTreeMap::new();
    for &(artifact, approval) in artifacts {
        let scope = &artifact.artifact.scope;
        if by_scope.insert(scope, (artifact, approval)).is_some() {
            return Err(ComposeError::Malformed {
                message: format!(
                    "more than one artifact replaces scope `{}`; compose one artifact per scope",
                    scope
                ),
            });
        }
    }
    if by_scope.is_empty() {
        return Err(ComposeError::Malformed {
            message: "no artifact was given to compose".to_string(),
        });
    }

    let mut nodes = default_plan.nodes.clone();
    let mut edges = default_plan.edges.clone();
    let mut revisions = Vec::with_capacity(by_scope.len());
    let mut scopes = BTreeMap::new();
    for (scope, (artifact, approval)) in by_scope {
        let part = compose_scope(default_plan, artifact, approval, catalog, registry)?;
        let members = &part.members;
        let in_region = |edge: &Edge| members.contains(&edge.to.node) || members.contains(&edge.from.node);
        nodes = splice(&nodes, |node| members.contains(&node.id), part.nodes);
        let ordered = in_default_order(edges.iter().filter(|edge| in_region(edge)), part.edges);
        edges = splice(&edges, in_region, ordered);
        revisions.push(part.revision);
        scopes.insert(scope.clone(), part.composition);
    }

    let plan = Plan {
        ir_version: default_plan.ir_version,
        provenance: Some(PlanProvenance {
            config_change: default_plan
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.config_change),
            artifacts: revisions,
            // A composed plan is never a boot plan: the native components describe the default program's layout only.
            native_components: BTreeMap::new(),
        }),
        nodes,
        edges,
        outputs: default_plan.outputs.clone(),
        scopes: default_plan.scopes.clone(),
        // Resources, like output declarations, come from the default program only: an artifact cannot declare them.
        resources: default_plan.resources.clone(),
    };
    let report = Validator::new(registry, catalog)
        .with_baseline(default_plan)
        .validate(&plan);
    let stale_base = scopes.values().any(|scope| scope.stale_base);
    let stale_review = scopes.values().any(|scope| scope.stale_review);
    let correspondence = match scopes.values().collect::<Vec<_>>()[..] {
        [only] => only.correspondence.clone(),
        _ => Correspondence::new(),
    };
    Ok(Composed {
        plan,
        report,
        correspondence,
        stale_base,
        stale_review,
        scopes,
    })
}

/// One artifact composed into its scope, before the composition is spliced into the plan.
struct ScopePart {
    /// The artifact's nodes, as composed, in artifact order.
    nodes: Vec<Node>,

    /// The artifact's edges, with plan ports, in artifact order.
    edges: Vec<Edge>,

    /// The default program's nodes of the scope's region, which the artifact's replace.
    members: BTreeSet<NodeId>,

    /// The artifact's revision and scope, as the plan's provenance names it.
    revision: ComposedArtifact,

    composition: ScopeComposition,
}

/// Composes one artifact against the default program's region of its scope.
fn compose_scope(
    default_plan: &Plan, received: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<ScopePart, ComposeError> {
    let artifact = &received.artifact;
    let scope = &artifact.scope;
    let declaration = default_plan
        .scopes
        .get(scope)
        .ok_or_else(|| ComposeError::UnknownScope { scope: scope.clone() })?;
    let default_program = |error: crate::export::ExportError| ComposeError::DefaultProgram {
        message: error.to_string(),
    };
    let region = ScopeRegion::of(default_plan, scope).map_err(default_program)?;
    let current = region.base(default_plan, scope).map_err(default_program)?;
    let stale_base = artifact.base.program_hash != current.program_hash;
    let stale_review = approval
        .approved_against
        .is_some_and(|approved| approval.active_plan_hash != Some(approved));
    let revision = ComposedArtifact {
        scope: scope.clone(),
        revision: approval.revision,
        hash: received.hash,
    };

    // Every artifact node's parameters with blobs inlined, in artifact order.
    let inlined = artifact
        .nodes
        .iter()
        .map(|node| {
            inline_blobs(&node.params, &artifact.blobs).map_err(|error| ComposeError::Malformed {
                message: format!("node `{}`: {}", node.id, error),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // A published program owns its policy: it binds facts and signals only. The rule judges what the author wrote, so
    // it covers every node, kept from the default program or not, before anything is resolved.
    if catalog.detached_programs() {
        for (node, params) in artifact.nodes.iter().zip(&inlined) {
            check_policy_bindings(&node.id, params, catalog)?;
        }
    }
    let aggregation = catalog
        .single_aggregation()
        .then(|| the_aggregation(default_plan, artifact, &inlined, &region, catalog, registry))
        .flatten();
    let ids = node_ids(default_plan, artifact, &region, aggregation)?;
    let edges = artifact
        .edges
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            Ok(Edge {
                from: plan_port(&edge.from, index, End::From, declaration, &ids)?,
                to: plan_port(&edge.to, index, End::To, declaration, &ids)?,
            })
        })
        .collect::<Result<Vec<_>, ComposeError>>()?;

    let region_nodes = default_plan
        .nodes
        .iter()
        .filter(|node| region.members.contains(&node.id))
        .map(|node| (&node.id, node))
        .collect::<BTreeMap<_, _>>();

    // Each artifact node with its plan ID, its parameters with blobs inlined and any recipients it emits to resolved
    // to their output, how it is addressed, and the default program's node it does the operation of, if any.
    let mut placed = Vec::with_capacity(artifact.nodes.len());
    let mut outputs = EmittedOutputs::default();
    for (node, params) in artifact.nodes.iter().zip(inlined) {
        let id = &ids[&node.id];
        let (params, addressed) = address(&node.id, &node.op, params, catalog)?;
        if let Addressed::Unavailable { recipients, reason } = &addressed {
            if !approval.in_effect {
                // A new artifact is refused for recipients on two hosts, and waits for unknown recipients, and for an
                // output for its recipients, unless another error refuses it first.
                let pending = match reason {
                    Unavailable::TwoHosts(hosts) => {
                        return Err(ComposeError::RecipientsOnTwoHosts {
                            node: node.id.clone(),
                            recipients: hosts.clone(),
                        })
                    }
                    Unavailable::Unknown(recipient) => ComposeError::UnknownRecipient {
                        node: node.id.clone(),
                        recipient: recipient.clone(),
                    },
                    Unavailable::NoOutput => ComposeError::PendingRecipients {
                        node: node.id.clone(),
                        recipients: recipients.clone(),
                    },
                    Unavailable::Refused(why) => {
                        return Err(ComposeError::NoOutputFor {
                            node: node.id.clone(),
                            recipients: recipients.clone(),
                            reason: why.clone(),
                        })
                    }
                };
                outputs.pending_recipients.get_or_insert(pending);
            }
        }
        let same = region_nodes
            .get(id)
            .copied()
            .filter(|default| same_operation(default, &node.op, &params, &node.id, catalog, registry));
        placed.push((node, id, params, addressed, same));
    }

    let mut correspondence = Correspondence::new();
    let mut nodes = Vec::with_capacity(artifact.nodes.len());
    for (node, id, params, addressed, same) in placed {
        let id = id.clone();
        if let Addressed::Unavailable { recipients, reason } = addressed {
            // The artifact in effect keeps running, but nothing it emits reaches recipients that are no longer
            // available, or no longer on one host: the `emit` becomes a counted drop, under its ID.
            let entry = outputs
                .dropped
                .entry(DropTarget::Recipients(recipients))
                .or_insert_with(|| (DropReason::Unavailable(reason.describe()), Vec::new()));
            entry.1.push(id.clone());
            let kind = if region_nodes.contains_key(&id) {
                CorrespondenceKind::Modified
            } else {
                CorrespondenceKind::Added
            };
            let dropped = Node {
                id: id.clone(),
                op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
                params: Params::new(),
            };
            correspondence.insert(node.id.clone(), NodeCorrespondence { node: id, kind });
            nodes.push(dropped);
            continue;
        }
        let by_name = matches!(addressed, Addressed::Recipients);
        let (composed, kind) = match same {
            // Written as the default program writes it, the node is the default program's own, bindings and all, and
            // follows native configuration.
            Some(default) if unresolved(&default.params) == params => (default.clone(), CorrespondenceKind::Unmodified),
            // The same only by value: the node is unmodified, but it runs what the artifact wrote, so that a detached
            // program never follows native policy. The plan does the same as the default program's, so the installer,
            // which compares plans by what they do, adopts or keeps the running generation, and builds nothing.
            Some(_) => (
                Node {
                    id: id.clone(),
                    op: node.op.clone(),
                    params: resolve_bindings(params, catalog, &node.id)?,
                },
                CorrespondenceKind::Unmodified,
            ),
            None => {
                let params = resolve_bindings(params, catalog, &node.id)?;
                let kind = if region_nodes.contains_key(&id) {
                    CorrespondenceKind::Modified
                } else {
                    CorrespondenceKind::Added
                };
                let composed = Node {
                    id: id.clone(),
                    op: node.op.clone(),
                    params,
                };
                (composed, kind)
            }
        };
        let (composed, kind) = if composed.op.as_str() == names::EMIT && !by_name {
            // An emit addressed to recipients follows them by name, so no fingerprint is recorded for it.
            let checked = check_output(&node.id, &composed.params, &artifact.base, default_plan, catalog);
            match checked {
                OutputCheck::Fine => (composed, kind),
                OutputCheck::Changed { output, reason } if approval.in_effect => {
                    // The artifact in effect keeps running, but nothing it emits reaches an output that was removed
                    // or now delivers elsewhere: the `emit` becomes a counted drop, under its ID.
                    let entry = outputs
                        .dropped
                        .entry(DropTarget::Output(output))
                        .or_insert_with(|| (reason, Vec::new()));
                    entry.1.push(id.clone());
                    let kind = if region_nodes.contains_key(&id) {
                        CorrespondenceKind::Modified
                    } else {
                        CorrespondenceKind::Added
                    };
                    let dropped = Node {
                        id: id.clone(),
                        op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
                        params: Params::new(),
                    };
                    (dropped, kind)
                }
                OutputCheck::Changed {
                    output,
                    reason: DropReason::Removed,
                } => {
                    outputs.pending.get_or_insert(output);
                    (composed, kind)
                }
                OutputCheck::Changed {
                    output,
                    reason: DropReason::Rebound { renumbered },
                } => return Err(ComposeError::ReboundOutput { output, renumbered }),
                // Only an emit addressed to recipients is unavailable, and those are not checked here.
                OutputCheck::Changed {
                    reason: DropReason::Unavailable(_),
                    ..
                } => (composed, kind),
                OutputCheck::Refused(error) => return Err(error),
            }
        } else {
            (composed, kind)
        };
        correspondence.insert(node.id.clone(), NodeCorrespondence { node: id, kind });
        nodes.push(composed);
    }
    // A new artifact whose outputs are otherwise fine waits for the first output it records that is not declared: it
    // is refused with any other error first, since that one would not go away when the output is declared.
    if let Some(output) = outputs.pending {
        return Err(ComposeError::PendingOutput { output });
    }
    if let Some(pending) = outputs.pending_recipients {
        return Err(pending);
    }
    for node in default_plan
        .nodes
        .iter()
        .filter(|node| region.members.contains(&node.id))
    {
        if correspondence.artifact_node(&node.id).is_none() {
            correspondence.insert_removed(node.id.clone());
        }
    }

    let outcome = if outputs.dropped.is_empty() {
        ComposeOutcome::Composed
    } else {
        ComposeOutcome::Degraded(
            outputs
                .dropped
                .into_iter()
                .map(|(target, (reason, nodes))| DroppedEmits { target, reason, nodes })
                .collect(),
        )
    };
    Ok(ScopePart {
        nodes,
        edges,
        members: region.members,
        revision,
        composition: ScopeComposition {
            correspondence,
            stale_base,
            stale_review,
            outcome,
        },
    })
}

/// What composing one artifact's `emit`s found about the outputs they deliver to.
#[derive(Default)]
struct EmittedOutputs {
    /// The outputs, or recipients, whose `emit`s were dropped, with why and the nodes, for the artifact in effect.
    dropped: BTreeMap<DropTarget, (DropReason, Vec<NodeId>)>,

    /// The first output a new artifact records that the host does not declare.
    pending: Option<OutputId>,

    /// Why a new artifact waits for the recipients an `emit` names, for the first such `emit`: a name the directory
    /// does not have, or no output with exactly their recipients yet.
    pending_recipients: Option<ComposeError>,
}

/// How an artifact node delivers, if it is an `emit`.
enum Addressed {
    /// Not an `emit` addressed to recipients: another operator, or an `emit` naming its output.
    Output,

    /// An `emit` addressed to recipients, resolved to the output whose recipients they are.
    Recipients,

    /// An `emit` addressed to recipients that have no output now.
    Unavailable {
        /// The names, as the artifact writes them.
        recipients: Vec<RecipientName>,

        /// Why.
        reason: Unavailable,
    },
}

/// Why the recipients an `emit` names have no output now.
enum Unavailable {
    /// The directory has no recipient of this name.
    Unknown(RecipientName),

    /// The names stand for recipients of more than one delivery host, each with its host.
    TwoHosts(Vec<(RecipientName, HostId)>),

    /// No output delivers to exactly their recipients yet: the host adds one, an implied output, once it lowers with
    /// the artifact's demand.
    NoOutput,

    /// No output delivers to exactly their recipients, and the host does not add one, for the plain-text reason.
    Refused(String),
}

impl Unavailable {
    fn describe(&self) -> String {
        match self {
            Self::Unknown(name) => format!("the host has no recipient `{}`", name),
            Self::TwoHosts(_) => "they are recipients of more than one delivery host".to_string(),
            Self::NoOutput => "no output delivers to exactly them".to_string(),
            Self::Refused(why) => format!("no output delivers to exactly them, and the host adds none: {}", why),
        }
    }
}

/// Resolves the recipients an `emit` node `node` names in its `to` parameter, if it has one, into the output whose
/// recipients are exactly theirs, which `params` then name in place of them.
///
/// Each name resolves through the host's recipient directory to the recipients it stands for. The names must stand for
/// recipients of one delivery host, since an output is one encoder stream on one host. Their recipients, sorted and
/// without repeats, are matched to an output with exactly those recipients ([`Catalog::output_for`]): a native output,
/// a declared one, or one the host added for a program's demand ([`demanded_outputs`]). The names' order and spelling
/// never matter, only the recipients they stand for.
///
/// # Errors
///
/// Returns an error if `to` is not a list of recipient names, or is given beside `output`, for which validation would
/// report the `emit` anyway.
fn address(
    node: &ArtifactNodeId, op: &OperatorName, mut params: Params, catalog: &dyn Catalog,
) -> Result<(Params, Addressed), ComposeError> {
    if op.as_str() != names::EMIT {
        return Ok((params, Addressed::Output));
    }
    let Some(names) = recipient_names(node, &params)? else {
        return Ok((params, Addressed::Output));
    };
    match resolve_recipients(&names, catalog) {
        Ok(recipients) => match catalog.output_for(&recipients) {
            Some(output) => {
                params = std::iter::once((
                    "output".parse().expect("`output` is a parameter name"),
                    Value::String(output.to_string()),
                ))
                .collect();
                Ok((params, Addressed::Recipients))
            }
            None => {
                let reason = match catalog.implied_output_refused(&recipients) {
                    Some(why) => Unavailable::Refused(why),
                    None => Unavailable::NoOutput,
                };
                Ok((
                    params,
                    Addressed::Unavailable {
                        recipients: names,
                        reason,
                    },
                ))
            }
        },
        Err(reason) => Ok((
            params,
            Addressed::Unavailable {
                recipients: names,
                reason,
            },
        )),
    }
}

/// Returns the recipient names of an `emit`'s `to` parameter, or `None` if it has none.
///
/// # Errors
///
/// Returns an error if `to` is not a list of recipient names, or is given beside `output`.
fn recipient_names(node: &ArtifactNodeId, params: &Params) -> Result<Option<Vec<RecipientName>>, ComposeError> {
    let Some(to) = params.get(EMIT_TO) else {
        return Ok(None);
    };
    let malformed = |what: &str| ComposeError::Malformed {
        message: format!("node `{}`: {}", node, what),
    };
    if params.get("output").is_some() {
        return Err(malformed(
            "an `emit` names an output or recipients, not both: write `emit <output>` or `emit to [<recipient>, ...]`",
        ));
    }
    let names = to
        .as_array()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| malformed("an `emit` to recipients names at least one recipient, in a list"))?
        .iter()
        .map(|name| {
            name.as_str()
                .and_then(|name| RecipientName::new(name).ok())
                .ok_or_else(|| malformed(&format!("`{}` is not a recipient name", name)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(names))
}

/// Resolves recipient names through the host's directory into the recipients they stand for, sorted and without
/// repeats, all on one delivery host.
fn resolve_recipients(names: &[RecipientName], catalog: &dyn Catalog) -> Result<Vec<RecipientId>, Unavailable> {
    let mut hosts = Vec::new();
    let mut recipients = BTreeSet::new();
    for name in names {
        let entry = catalog
            .recipient(name)
            .ok_or_else(|| Unavailable::Unknown(name.clone()))?;
        hosts.push((name.clone(), entry.host.clone()));
        recipients.extend(entry.recipients.iter().cloned());
    }
    if hosts.iter().any(|(_, host)| *host != hosts[0].1) {
        return Err(Unavailable::TwoHosts(hosts));
    }
    Ok(recipients.into_iter().collect())
}

/// The recipients one output of a program's demand delivers to: on one delivery host, sorted and without repeats.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Demand {
    /// The delivery host the recipients belong to.
    pub host: HostId,

    /// The recipients, sorted and without repeats.
    pub recipients: Vec<RecipientId>,

    /// The local ID of the first `emit` of the artifact, in artifact node ID order, that addresses these recipients,
    /// which a host may name the output it adds for them after.
    pub emit: ArtifactNodeId,
}

/// Returns the outputs an artifact's `emit`s addressed to recipients need: one per distinct set of recipients, on its
/// delivery host, in recipient order, whether or not the host has an output for the set yet.
///
/// A host that adds an output for each set no output serves (an **implied output**) before composing the artifact lets
/// a program route to any recipient its directory names, with one encoder per distinct set, as native routing groups
/// share one per set of endpoints. A name the directory does not have, or names that stand for recipients of more than
/// one host, are left out: composition reports them.
///
/// # Errors
///
/// Returns an error if an `emit`'s `to` is not a list of recipient names, or is given beside `output`.
pub fn demanded_outputs(artifact: &PolicyArtifact, catalog: &dyn Catalog) -> Result<Vec<Demand>, ComposeError> {
    let mut demand: BTreeMap<(HostId, Vec<RecipientId>), ArtifactNodeId> = BTreeMap::new();
    for node in artifact.nodes.iter().filter(|node| node.op.as_str() == names::EMIT) {
        let params = inline_blobs(&node.params, &artifact.blobs).map_err(|error| ComposeError::Malformed {
            message: format!("node `{}`: {}", node.id, error),
        })?;
        let Some(names) = recipient_names(&node.id, &params)? else {
            continue;
        };
        let Ok(recipients) = resolve_recipients(&names, catalog) else {
            continue;
        };
        let Some(host) = names
            .first()
            .and_then(|name| catalog.recipient(name))
            .map(|entry| entry.host.clone())
        else {
            continue;
        };
        let emit = demand.entry((host, recipients)).or_insert_with(|| node.id.clone());
        if node.id < *emit {
            *emit = node.id.clone();
        }
    }
    Ok(demand
        .into_iter()
        .map(|((host, recipients), emit)| Demand { host, recipients, emit })
        .collect())
}

/// Returns the artifact's one state owner and the reserved ID it takes, for a host that runs at most one aggregation
/// per program: when the artifact has exactly one state owner, the scope's region has exactly one, and no artifact node
/// has that one's local ID, the artifact's state owner is the region's, whatever it is called.
///
/// So a program's one aggregation keeps the default program's ID (`dsd_agg`, say), and with it its lineage, its
/// isolation slot's series, and its telemetry, when an author renames it or writes the program from scratch. Whether
/// its state is kept is then decided as for any node with that ID: by its operation and its lineage parameters.
///
/// A node whose operator does not configure, with its bindings resolved from the catalog, is not counted: validation
/// reports it.
fn the_aggregation<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, inlined: &[Params], region: &ScopeRegion, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Option<(&'a ArtifactNodeId, NodeId)> {
    let owns_state = |op: &OperatorName, params: &Params| {
        registry
            .configure(op.as_str(), params)
            .is_ok_and(|configured| matches!(configured.contract().state, StateContract::Keyed(_)))
    };
    let region_owners = default_plan
        .nodes
        .iter()
        .filter(|node| region.members.contains(&node.id) && owns_state(&node.op, &node.params))
        .collect::<Vec<_>>();
    let artifact_owners = artifact
        .nodes
        .iter()
        .zip(inlined)
        .filter(|(node, params)| {
            resolve_bindings((*params).clone(), catalog, &node.id).is_ok_and(|params| owns_state(&node.op, &params))
        })
        .collect::<Vec<_>>();
    let ([region_owner], [(artifact_owner, _)]) = (&region_owners[..], &artifact_owners[..]) else {
        return None;
    };
    let taken = artifact
        .nodes
        .iter()
        .any(|node| node.id.as_str() == region_owner.id.local().as_str());
    (!taken).then(|| (&artifact_owner.id, region_owner.id.clone()))
}

/// Assigns every artifact node its plan ID: the reserved ID of a region node with the same local ID, or its local ID
/// in the policy namespace. `aggregation`, if given, is the artifact's one state owner and the reserved ID it takes
/// instead ([`the_aggregation`]).
///
/// # Errors
///
/// Returns an error if two artifact nodes have the same ID, or one has the ID of a reserved node outside the region.
fn node_ids<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, region: &ScopeRegion,
    aggregation: Option<(&'a ArtifactNodeId, NodeId)>,
) -> Result<BTreeMap<&'a ArtifactNodeId, NodeId>, ComposeError> {
    let mut ids = BTreeMap::new();
    for node in &artifact.nodes {
        let reserved = NodeId::reserved(node.id.to_local_id());
        let id = if let Some((_, id)) = aggregation.as_ref().filter(|(owner, _)| **owner == node.id) {
            id.clone()
        } else if region.members.contains(&reserved) {
            reserved
        } else if default_plan.node(&reserved).is_some() {
            return Err(ComposeError::ReservedOutsideScope {
                node: node.id.clone(),
                reserved,
                scope: artifact.scope.clone(),
            });
        } else {
            NodeId::policy(node.id.to_local_id())
        };
        if ids.insert(&node.id, id).is_some() {
            return Err(ComposeError::Malformed {
                message: format!("more than one node has the ID `{}`; give each node its own ID", node.id),
            });
        }
    }
    Ok(ids)
}

/// Which end of an edge a port is at.
#[derive(Clone, Copy)]
enum End {
    From,
    To,
}

/// Returns the plan port an artifact edge's end refers to: a scope input's port, a scope exit's port, or a port of an
/// artifact node, under its plan ID.
///
/// # Errors
///
/// Returns an error if the end is a scope input or exit the scope does not declare, a scope input the edge enters, a
/// scope exit the edge leaves, or a port of a node the artifact does not have.
fn plan_port(
    reference: &ArtifactPortRef, index: usize, end: End, scope: &ScopeDecl, ids: &BTreeMap<&ArtifactNodeId, NodeId>,
) -> Result<PortRef, ComposeError> {
    let verb = match end {
        End::From => "leaves",
        End::To => "enters",
    };
    match (reference, end) {
        (ArtifactPortRef::Input(input), End::From) => {
            scope.inputs.get(input).cloned().ok_or_else(|| ComposeError::Malformed {
                message: format!(
                    "edge {} leaves `{}`, but the scope has no input `{}`",
                    index, reference, input
                ),
            })
        }
        (ArtifactPortRef::Input(_), End::To) => Err(ComposeError::Malformed {
            message: format!(
                "edge {} enters `{}`, which is a scope input; edges leave a scope's inputs",
                index, reference
            ),
        }),
        (ArtifactPortRef::Exit(exit), End::To) => {
            scope.exits.get(exit).cloned().ok_or_else(|| ComposeError::Malformed {
                message: format!(
                    "edge {} enters `{}`, but the scope has no exit `{}`",
                    index, reference, exit
                ),
            })
        }
        (ArtifactPortRef::Exit(_), End::From) => Err(ComposeError::Malformed {
            message: format!(
                "edge {} leaves `{}`, which is a scope exit; edges enter a scope's exits",
                index, reference
            ),
        }),
        (ArtifactPortRef::Node { node, port }, _) => ids
            .get(node)
            .map(|id| PortRef::new(id.clone(), port.clone()))
            .ok_or_else(|| ComposeError::Malformed {
                message: format!(
                    "edge {} {} `{}`, but the artifact has no node `{}`",
                    index, verb, reference, node
                ),
            }),
    }
}

/// Returns parameters with every blob reference replaced by the blob's content.
fn inline_blobs(params: &Params, blobs: &Blobs) -> Result<Params, BlobError> {
    params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            blobs.inline(&mut value)?;
            Ok((name.clone(), value))
        })
        .collect()
}

/// Returns `true` if an artifact node `node` with operator `op` and parameters `params` does what the default
/// program's node `default` does: the same operator, and the same parameters, compared by the values they have.
///
/// Parameters written as the default program writes them are the same. Otherwise both sides are compared with every
/// configuration binding written as the value it has now (the artifact's resolved from `catalog`), and every literal
/// `flag` and `is_empty` folded, as the detached export writes them ([`Predicate::fold_literals`]), as written or, when
/// both decode, as their operator decodes them, so that a set written in another order is the same set. So a literal
/// equal to a binding's value is the same operation, and the detached export composes to the default program: an
/// operation is what it does with the values it has, not where they come from. Such a node composes as the artifact
/// writes it, so a later change of the binding's value leaves it as it is: the next composition, which follows every
/// configuration change, finds it modified, still with the artifact's value.
///
/// [`Predicate::fold_literals`]: crate::predicate::Predicate::fold_literals
fn same_operation(
    default: &Node, op: &OperatorName, params: &Params, node: &ArtifactNodeId, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> bool {
    if default.op != *op {
        return false;
    }
    if unresolved(&default.params) == *params {
        return true;
    }
    let Ok(resolved) = resolve_bindings(params.clone(), catalog, node) else {
        return false;
    };
    let (default_params, artifact_params) = (operation_params(op, &default.params), operation_params(op, &resolved));
    if default_params == artifact_params {
        return true;
    }
    match (
        registry.configure(op.as_str(), &default_params),
        registry.configure(op.as_str(), &artifact_params),
    ) {
        (Ok(default), Ok(artifact)) => default.params().encode() == artifact.params().encode(),
        _ => false,
    }
}

/// Refuses a node of an artifact for a host whose programs bind facts and signals only
/// ([`Catalog::detached_programs`]) if its parameters bind a binding the catalog declares a policy setting.
///
/// # Errors
///
/// Returns [`ComposeError::PolicyBinding`] for the first such binding, with its current value and the facts and
/// signals the catalog declares.
fn check_policy_bindings(node: &ArtifactNodeId, params: &Params, catalog: &dyn Catalog) -> Result<(), ComposeError> {
    fn find(value: &Value, found: &mut Vec<BindingName>) {
        match value {
            Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
                if let Some(binding) = members
                    .get(BINDING_MEMBER)
                    .and_then(Value::as_str)
                    .and_then(|name| name.parse::<BindingName>().ok())
                {
                    found.push(binding);
                }
            }
            Value::Object(members) => members.values().for_each(|member| find(member, found)),
            Value::Array(values) => values.iter().for_each(|element| find(element, found)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    let mut found = Vec::new();
    params.iter().for_each(|(_, value)| find(value, &mut found));
    for binding in found {
        let Some(declared) = catalog.binding(&binding) else {
            continue;
        };
        if declared.class == BindingClass::Policy {
            return Err(ComposeError::PolicyBinding {
                node: node.clone(),
                binding,
                value: declared.value.clone(),
                allowed: catalog.facts_and_signals(),
            });
        }
    }
    Ok(())
}

/// Fills in the resolved value of every configuration binding in `params` that the catalog declares.
///
/// # Errors
///
/// Returns an error for the first binding the catalog declares without a value.
fn resolve_bindings(params: Params, catalog: &dyn Catalog, node: &ArtifactNodeId) -> Result<Params, ComposeError> {
    params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            resolve_value(&mut value, catalog, node)?;
            Ok((name.clone(), value))
        })
        .collect()
}

/// Fills in the resolved value of every configuration binding in one parameter value.
///
/// A binding is an object with a `config` member, which parameters reserve for bindings. A binding whose name is
/// invalid, or that the catalog does not declare, is left as it is, for validation to report.
fn resolve_value(value: &mut Value, catalog: &dyn Catalog, node: &ArtifactNodeId) -> Result<(), ComposeError> {
    match value {
        Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
            let Some(binding) = members
                .get(BINDING_MEMBER)
                .and_then(Value::as_str)
                .and_then(|name| name.parse::<BindingName>().ok())
            else {
                return Ok(());
            };
            let Some(declared) = catalog.binding(&binding) else {
                return Ok(());
            };
            let resolved = declared.value.clone().ok_or_else(|| ComposeError::UnresolvedBinding {
                node: node.clone(),
                binding,
            })?;
            members.insert(RESOLVED_MEMBER.to_string(), resolved);
            Ok(())
        }
        Value::Object(members) => members
            .values_mut()
            .try_for_each(|member| resolve_value(member, catalog, node)),
        Value::Array(values) => values
            .iter_mut()
            .try_for_each(|element| resolve_value(element, catalog, node)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// What checking the output of one `emit` against the artifact's base and the catalog found.
enum OutputCheck {
    /// The output is declared with the fingerprint the base records, or the `emit` names no valid output, which
    /// validation reports.
    Fine,

    /// The output the base records was removed, or now delivers elsewhere.
    Changed {
        /// The output.
        output: OutputId,

        /// Whether it was removed or rebound.
        reason: DropReason,
    },

    /// The `emit` cannot be composed whatever the artifact's state: its output is not recorded.
    Refused(ComposeError),
}

/// Checks the output an `emit` node of the artifact delivers to against the artifact's base and the catalog.
///
/// An output the base does not record is refused: as unrecorded if the catalog declares it or it is named as a
/// resources artifact names its outputs, and as undeclared otherwise, listing the outputs the default program declares,
/// which the artifact can emit to. A recorded output is fine when the catalog declares it with the recorded
/// fingerprint. Otherwise it changed: rebound when the catalog declares it with another fingerprint, or when another
/// routing group's output carries the recorded fingerprint (it is named then), and removed otherwise.
fn check_output(
    node: &ArtifactNodeId, params: &Params, base: &ArtifactBase, default_plan: &Plan, catalog: &dyn Catalog,
) -> OutputCheck {
    let Some(output) = emitted_output(params) else {
        return OutputCheck::Fine;
    };
    let Some(recorded) = base.outputs.get(&output) else {
        if catalog.output(&output).is_none() && !output.is_declared() {
            let declared = default_plan
                .outputs
                .iter()
                .map(|declared| declared.id.clone())
                .filter(|id| catalog.output(id).is_some())
                .collect();
            return OutputCheck::Refused(ComposeError::UndeclaredOutput {
                node: node.clone(),
                output,
                declared,
            });
        }
        return OutputCheck::Refused(ComposeError::UnrecordedOutput {
            node: node.clone(),
            output,
        });
    };
    let declared = catalog.output(&output);
    if declared.is_some_and(|declared| declared.fingerprint == *recorded) {
        return OutputCheck::Fine;
    }
    // After routing groups are renumbered, another group's output carries the fingerprint the base records: the
    // recorded output now delivers elsewhere, or is gone, and the artifact is to be rebased rather than wait for it.
    // Only routing groups have positional IDs: a declared output's fingerprint covers its recipients, not its name, so
    // two declared outputs with the same recipients carry one fingerprint, and neither was renumbered.
    let renumbered = default_plan
        .outputs
        .iter()
        .filter(|other| other.id != output)
        .filter(|other| {
            other
                .binding
                .as_ref()
                .is_some_and(|binding| matches!(binding.selector, Selector::Group(_)))
        })
        .find(|other| {
            catalog
                .output(&other.id)
                .is_some_and(|declared| declared.fingerprint == *recorded)
        })
        .map(|other| other.id.clone());
    let reason = match (declared, renumbered) {
        (None, None) => DropReason::Removed,
        (_, renumbered) => DropReason::Rebound { renumbered },
    };
    OutputCheck::Changed { output, reason }
}

/// Replaces the items of `default` for which `in_region` holds with `replacement`, keeping the default's order.
///
/// The replacement's items take the places of the region's items in order, and any left over follow the last of them;
/// with no region item, they follow everything. Replacing a region with its own items, in its own order, gives the
/// default back.
/// Returns an artifact's edges, with plan ports, in the default program's order: each edge that the default program's
/// region has (`region`, in plan order) takes that edge's position, and each other edge follows the edge of the region
/// that the artifact lists before it (or comes first, if the artifact lists none before it), in artifact order.
///
/// The order of a plan's edges means nothing to how it runs, but the plan's hash covers it. A tool may write an
/// artifact's edges in another order than the export, as the surface syntax's printed form does with the edges from
/// several scope inputs; ordered so, an artifact that is the export but for the order of its edges composes to the
/// default program, with its hash. Edges the artifact lists in the export's order keep the order they have.
fn in_default_order<'a>(region: impl Iterator<Item = &'a Edge>, edges: Vec<Edge>) -> Vec<Edge> {
    let positions = region
        .enumerate()
        .map(|(position, edge)| ((&edge.from, &edge.to), position))
        .collect::<BTreeMap<_, _>>();
    // (the position of the region's edge it is or follows, whether it is new, its position in the artifact)
    let mut anchor = None;
    let keys = edges
        .iter()
        .enumerate()
        .map(|(index, edge)| match positions.get(&(&edge.from, &edge.to)) {
            Some(&position) => {
                anchor = Some(position);
                (Some(position), false, index)
            }
            None => (anchor, true, index),
        })
        .collect::<Vec<_>>();
    let mut keyed = keys.into_iter().zip(edges).collect::<Vec<_>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, edge)| edge).collect()
}

fn splice<T: Clone>(default: &[T], in_region: impl Fn(&T) -> bool, replacement: Vec<T>) -> Vec<T> {
    let places = default.iter().filter(|item| in_region(item)).count();
    let mut replacement = replacement.into_iter();
    let mut spliced = Vec::with_capacity(default.len());
    let mut seen = 0;
    for item in default {
        if !in_region(item) {
            spliced.push(item.clone());
            continue;
        }
        seen += 1;
        if seen < places {
            spliced.extend(replacement.next());
        } else {
            spliced.extend(replacement.by_ref());
        }
    }
    spliced.extend(replacement);
    spliced
}

/// Composes artifacts into the host's current default program.
///
/// The installer core composes the artifacts it accepts through this, without knowing the default program or the
/// catalog: the host implements it over [`compose_all`], with the default program it last lowered and its catalog.
pub trait Composer {
    /// Composes received artifacts, at most one per scope, each with its approval, into the current default program:
    /// each scope is composed on its own, with its own correspondence and base check, and the plan is validated once.
    ///
    /// # Errors
    ///
    /// Returns an error if any artifact cannot be composed; see [`compose_all`].
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError>;

    /// Composes one received artifact into the current default program.
    ///
    /// # Errors
    ///
    /// Returns an error if the artifact cannot be composed; see [`compose_all`].
    fn compose(&self, artifact: &ReceivedArtifact, approval: &Approval) -> Result<Composed, ComposeError> {
        self.compose_all(&[(artifact, approval)])
    }
}

/// An artifact that cannot be composed into the default program.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ComposeError {
    /// The artifact replaces a scope the default program does not declare.
    #[snafu(display(
        "The artifact replaces scope `{}`, which the default program does not declare. Export the scope to replace \
         and edit that.",
        scope
    ))]
    UnknownScope {
        /// The scope.
        scope: ScopeName,
    },

    /// The default program's scope cannot be replaced, because it cannot be written as an artifact.
    #[snafu(display("The default program's scope cannot be replaced: {}", message))]
    DefaultProgram {
        /// Why, as the export says it.
        message: String,
    },

    /// An artifact node's local ID is a reserved ID outside the artifact's scope.
    #[snafu(display(
        "Artifact node `{}` has the ID of reserved node `{}`, which is outside scope `{}`. Rename the node.",
        node,
        reserved,
        scope
    ))]
    ReservedOutsideScope {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The reserved node it collides with.
        reserved: NodeId,

        /// The artifact's scope.
        scope: ScopeName,
    },

    /// An artifact node is bound to a configuration binding that the host declares without a value.
    #[snafu(display(
        "Artifact node `{}` is bound to `{}`, which has no value: the host's configuration has no valid value for it. \
         Fix the configuration, or write the value in the artifact.",
        node,
        binding
    ))]
    UnresolvedBinding {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The binding.
        binding: BindingName,
    },

    /// An `emit` of the artifact delivers to an output the host does not declare, and the artifact's base does not
    /// record either: no export has it.
    #[snafu(display(
        "Artifact node `{}` emits to output `{}`, which the host does not declare. {}",
        node,
        output,
        declared_outputs(declared)
    ))]
    UndeclaredOutput {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The output.
        output: OutputId,

        /// The outputs the host declares, in the default program's order.
        declared: Vec<OutputId>,
    },

    /// An `emit` of the artifact delivers to an output the host declares, but whose fingerprint the artifact's base
    /// does not record.
    #[snafu(display(
        "Artifact node `{}` emits to output `{}`, which the artifact's base does not record. Add the output's \
         fingerprint from the export to `base.outputs`.",
        node,
        output
    ))]
    UnrecordedOutput {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The output.
        output: OutputId,
    },

    /// A new artifact emits to an output its base records that the host does not declare: it waits, a held state,
    /// until native configuration or a resources artifact declares the output.
    #[snafu(display(
        "Waiting for output `{}`: the artifact's base records it, and the host does not declare it. The artifact \
         installs once native configuration or a resources artifact declares the output; whatever runs keeps running.",
        output
    ))]
    PendingOutput {
        /// The output.
        output: OutputId,
    },

    /// A new artifact emits to an output that now delivers elsewhere than its base records: the host declares it with
    /// another fingerprint, or another output carries the recorded one.
    #[snafu(display(
        "The artifact emits to output `{}`, which now delivers elsewhere than the artifact's base records{}. Rebase \
         the artifact on the current export.",
        output,
        renumbered.as_ref().map_or_else(String::new, |now| format!(
            ": the outputs were renumbered, and the recorded one is now `{}`",
            now
        ))
    ))]
    ReboundOutput {
        /// The output.
        output: OutputId,

        /// The output that now carries the recorded fingerprint, if one does.
        renumbered: Option<OutputId>,
    },

    /// An artifact node binds a policy setting, which a published program owns and writes as a literal, on a host whose
    /// programs bind facts and signals only ([`Catalog::detached_programs`]).
    #[snafu(display(
        "`{}` binds `{}`, a native policy setting. A published program owns its policy: write the value itself ({}; the \
         detached export writes current values). Only facts and signals can be bound{}.",
        node,
        binding,
        value.as_ref().map_or_else(
            || "it has no value now".to_string(),
            |value| format!("now `{}`", value)
        ),
        if allowed.is_empty() {
            String::new()
        } else {
            format!(
                ": {}",
                allowed.iter().map(|name| format!("`{}`", name)).collect::<Vec<_>>().join(", ")
            )
        }
    ))]
    PolicyBinding {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The binding.
        binding: BindingName,

        /// The binding's current value, if the catalog carries it: what the author writes in its place.
        value: Option<Value>,

        /// The facts and signals the host declares, which a program may bind, in name order.
        allowed: Vec<BindingName>,
    },

    /// A new artifact emits to a recipient name the host's recipient directory does not have: it waits, a held state,
    /// until configuration or a resources artifact declares the recipient.
    #[snafu(display(
        "Waiting for recipient `{}`: artifact node `{}` emits to it, and the host's recipient directory does not name \
         it. The artifact installs once configuration or a resources artifact declares the recipient; whatever runs \
         keeps running.",
        recipient,
        node
    ))]
    UnknownRecipient {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The recipient name.
        recipient: RecipientName,
    },

    /// A new artifact emits to recipients that no output delivers to yet: it waits, a held state, until the host adds
    /// an output for them, which it does when it lowers with the artifact's demand.
    #[snafu(display(
        "Waiting for an output to {}: artifact node `{}` emits to these recipients, and no output delivers to exactly \
         them yet. The host adds one; whatever runs keeps running.",
        describe_names(recipients),
        node
    ))]
    PendingRecipients {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The recipient names, as the artifact writes them.
        recipients: Vec<RecipientName>,
    },

    /// An `emit` of the artifact names recipients that no output delivers to exactly, and for which the host adds none.
    #[snafu(display(
        "Artifact node `{}` emits to {}, and no output delivers to exactly these recipients: {}.",
        node,
        describe_names(recipients),
        reason
    ))]
    NoOutputFor {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The recipient names, as the artifact writes them.
        recipients: Vec<RecipientName>,

        /// Why the host adds no output for them.
        reason: String,
    },

    /// An `emit` of the artifact names recipients of more than one delivery host, which no one output reaches.
    #[snafu(display(
        "Artifact node `{}` emits to {}, recipients of more than one delivery host ({}). An output is one encoder stream \
         on one host: emit to each host's recipients from a branch of its own.",
        node,
        describe_names(&recipients.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>()),
        recipients
            .iter()
            .map(|(name, host)| format!("`{}` on `{}`", name, host))
            .collect::<Vec<_>>()
            .join(", ")
    ))]
    RecipientsOnTwoHosts {
        /// The artifact node.
        node: ArtifactNodeId,

        /// Each recipient name, as the artifact writes them, with its delivery host.
        recipients: Vec<(RecipientName, HostId)>,
    },

    /// The artifact's structure cannot be placed into a plan: two nodes have one ID, an edge names an unknown node or
    /// scope input, or a blob reference cannot be inlined.
    #[snafu(display("The artifact cannot be composed: {}", message))]
    Malformed {
        /// What is wrong.
        message: String,
    },
}

/// Says which outputs the host declares, for an error message about an output it does not.
fn declared_outputs(outputs: &[OutputId]) -> String {
    if outputs.is_empty() {
        return "The host declares no outputs.".to_string();
    }
    let listed = outputs
        .iter()
        .map(|output| format!("`{}`", output))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Emit to one of the outputs it declares: {}.", listed)
}

#[cfg(test)]
mod change_tests;
#[cfg(test)]
pub(crate) mod tests;
