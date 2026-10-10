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
//!    artifact node gets its plan ID by correspondence (see [`Correspondence`]) and its provenance: native for a
//!    reserved node the artifact keeps unmodified (and, if it can lose records or runs in a host, fed as the default
//!    program feeds it), the artifact's revision otherwise. Bindings resolve from the catalog, and the composed plan is
//!    validated, with the artifact limits applied to artifact-provenance nodes. Composition also reports whether each
//!    artifact's base no longer matches the default program (a stale base: it still installs) and whether it was
//!    approved against another active plan (a stale review: it still installs).
//!
//! Artifacts reference native outputs only. An artifact may hold state owners, such as `aggregate`: whether the frame
//! can run them where the artifact puts them is placement's to decide, and installing a change to one is a cut.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{contains_resolved_values, to_canonical_string, PlanHash, BINDING_MEMBER, RESOLVED_MEMBER},
    export::{emitted_output, unresolved, ScopeRegion},
    install::{
        correspondence::{CorrespondenceKind, NodeCorrespondence},
        Correspondence,
    },
    ir::{
        decode_params, ArtifactBase, ArtifactNodeId, ArtifactPortRef, ArtifactRevision, BindingName, BlobError, Blobs,
        Edge, IrError, Node, NodeId, OperatorName, OutputId, Params, Plan, PlanProvenance, PolicyArtifact, PortName,
        PortRef, Provenance, ReceivedArtifact, ScopeDecl, ScopeName, BLOB_MEMBER,
    },
    limits::values::{ARTIFACT_NODES, ARTIFACT_SIZE, BLOB_SIZE, PREDICATE_LEAVES},
    operators::{names, OperatorRegistry},
    validate::{predicate_leaves, Catalog, ValidationReport, Validator},
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
/// again to the nodes that have the artifact's provenance.
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
}

/// Artifacts composed into the default program, one per scope.
#[derive(Clone, Debug)]
pub struct Composed {
    /// The composed plan, with every configuration binding resolved and every node's provenance set.
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
///   with its native provenance and resolved values. A node that can lose records or runs in a host must also be fed
///   what the default program feeds it, the same streams from the same places through nodes that are themselves
///   unchanged (a `tee` or `merge` passes its input on unchanged, whatever its port count), since native configuration
///   authorizes the loss it expresses on the records it expresses it on. Otherwise the node's provenance is the
///   artifact's revision. Every other artifact node is placed as `policy/<local>` with the artifact's provenance. A
///   local ID equal to a reserved ID outside the scope is rejected.
/// - Blob references are replaced with their content, and the configuration bindings of artifact-provenance nodes
///   resolve from `catalog`. A binding the catalog does not declare is left unresolved, for validation to report; one
///   it declares without a value is rejected.
/// - Every output an `emit` of an artifact delivers to must be recorded in the artifact's base, and declared by the
///   catalog with the fingerprint the base records; one that is not declared, or declared with another fingerprint,
///   is a removed output, and one that is neither recorded nor declared is an undeclared output.
/// - State owners, such as `aggregate`, compose like any node: placement decides whether the frame can run them where
///   the artifact puts them, and validation rejects host components in artifacts.
/// - The composed plan's provenance lists every artifact's revision, in scope order, and its nodes and edges are in the
///   default program's order, each artifact's taking the places of its region's in order (any left over follow the last
///   of them), so that composing an unmodified export gives the default program again, with the same hash.
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
        nodes = splice(&nodes, |node| members.contains(&node.id), part.nodes);
        edges = splice(
            &edges,
            |edge| members.contains(&edge.to.node) || members.contains(&edge.from.node),
            part.edges,
        );
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
        }),
        nodes,
        edges,
        outputs: default_plan.outputs.clone(),
        scopes: default_plan.scopes.clone(),
    };
    let report = Validator::new(registry, catalog).validate(&plan);
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

    /// The artifact's revision, as provenance names it.
    revision: ArtifactRevision,

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
    let revision = ArtifactRevision {
        revision: approval.revision,
        hash: received.hash,
    };

    let ids = node_ids(default_plan, artifact, &region)?;
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

    // Each artifact node with its plan ID, its parameters with blobs inlined, and the default program's node it does
    // the operation of, if any.
    let mut placed = Vec::with_capacity(artifact.nodes.len());
    for node in &artifact.nodes {
        let id = &ids[&node.id];
        let params = inline_blobs(&node.params, &artifact.blobs).map_err(|error| ComposeError::Malformed {
            message: format!("node `{}`: {}", node.id, error),
        })?;
        let same = region_nodes
            .get(id)
            .copied()
            .filter(|default| same_operation(default, &node.op, &params, registry));
        placed.push((node, id, params, same));
    }

    // What reaches each node of the region, in the default program and in the composition.
    let default_feeds = Feeds::new(
        region_nodes.iter().map(|(id, node)| (*id, node.op.as_str(), true)),
        &default_plan.edges,
    )
    .compute(None);
    let feeds = Feeds::new(
        placed
            .iter()
            .map(|(node, id, _, same)| (*id, node.op.as_str(), same.is_some())),
        &edges,
    )
    .compute(Some(&default_feeds));

    let mut correspondence = Correspondence::new();
    let mut nodes = Vec::with_capacity(artifact.nodes.len());
    for (node, id, params, same) in placed {
        let id = id.clone();
        // A node that can lose records, or runs in a host, keeps native authority only if it is fed what the default
        // program feeds it: native configuration authorizes the loss it expresses, on the records it expresses it on.
        let kept = same.filter(|default| {
            !guards_authority(default, registry) || {
                let feed = feeds.get(&id);
                feed.is_some_and(|feed| !feed.is_changed()) && feed == default_feeds.get(&id)
            }
        });
        let (composed, kind) = match kept {
            Some(default) => (default.clone(), CorrespondenceKind::Unmodified),
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
                    provenance: Some(Provenance::Artifact(revision.clone())),
                };
                (composed, kind)
            }
        };
        if composed.op.as_str() == names::EMIT {
            check_output(&node.id, &composed.params, &artifact.base, default_plan, catalog)?;
        }
        correspondence.insert(node.id.clone(), NodeCorrespondence { node: id, kind });
        nodes.push(composed);
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

    Ok(ScopePart {
        nodes,
        edges,
        members: region.members,
        revision,
        composition: ScopeComposition {
            correspondence,
            stale_base,
            stale_review,
        },
    })
}

/// Assigns every artifact node its plan ID: the reserved ID of a region node with the same local ID, or its local ID
/// in the policy namespace.
///
/// # Errors
///
/// Returns an error if two artifact nodes have the same ID, or one has the ID of a reserved node outside the region.
fn node_ids<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, region: &ScopeRegion,
) -> Result<BTreeMap<&'a ArtifactNodeId, NodeId>, ComposeError> {
    let mut ids = BTreeMap::new();
    for node in &artifact.nodes {
        let reserved = NodeId::reserved(node.id.to_local_id());
        let id = if region.members.contains(&reserved) {
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

/// Returns `true` if an artifact node with operator `op` and parameters `params` does what the default program's node
/// `default` does: the same operator, and the same parameters without resolved values, compared as written or, when
/// both decode, as their operator decodes them.
fn same_operation(default: &Node, op: &OperatorName, params: &Params, registry: &OperatorRegistry) -> bool {
    if default.op != *op {
        return false;
    }
    let default_params = unresolved(&default.params);
    if default_params == *params {
        return true;
    }
    match (
        registry.configure(op.as_str(), &default_params),
        registry.configure(op.as_str(), params),
    ) {
        (Ok(default), Ok(artifact)) => default.params().encode() == artifact.params().encode(),
        _ => false,
    }
}

/// Returns `true` if a node of the default program needs the authority its provenance gives for more than what it
/// does: a node that can lose records, whose loss its provenance authorizes, or a fence, which runs in a host that only
/// native configuration places. Such a node keeps its native provenance in a composition only if it is also fed what
/// the default program feeds it. A node whose operator does not configure is taken to need it.
fn guards_authority(node: &Node, registry: &OperatorRegistry) -> bool {
    registry
        .configure(node.op.as_str(), &node.params)
        .map_or(true, |configured| {
            let contract = configured.contract();
            contract.loss.is_lossy() || contract.boundary.is_fence()
        })
}

/// A stream of records, named by where it comes from in the default program's terms.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Stream {
    /// What a port outside the region produces: a scope input's port.
    Outside(PortRef),

    /// What an output port of a region node produces, where the node does what the default program's node of its ID
    /// does and is fed what the default program feeds it.
    Port(PortRef),

    /// Records the default program does not produce at this point.
    Changed,
}

/// The streams of records one port carries, counted.
type Streams = BTreeMap<Stream, u64>;

/// What reaches a node: for each of its input ports, the streams of records that enter it, counted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Feed(BTreeMap<(PortName, Stream), u64>);

impl Feed {
    /// Returns `true` if some of what reaches the node is not what the default program produces.
    fn is_changed(&self) -> bool {
        self.0.keys().any(|(_, stream)| *stream == Stream::Changed)
    }
}

/// Adds `count` copies of `key` to a counted set.
fn add_count<K: Ord>(counts: &mut BTreeMap<K, u64>, key: K, count: u64) {
    let total = counts.entry(key).or_insert(0);
    *total = total.saturating_add(count);
}

/// The nodes of a scope's region and the edges that enter them, in one plan, for working out what reaches each node.
///
/// A stream is named by the port it comes from: a port outside the region (a scope input), or the output port of a
/// region node that does what the default program's node of the same ID does and is fed as it is there. Any other node
/// produces records the default program does not, except `tee` and `merge`, which pass on what enters them unchanged:
/// a stream through them keeps its name, so a `tee` resized, or a branch moved to another of its ports, changes nothing
/// downstream. Streams are counted, so a node fed one stream twice is not fed what it was fed once, and a long chain of
/// copies costs no more than counting.
struct Feeds<'a> {
    /// Each region node's operator, and whether it does what the default program's node of the same ID does.
    nodes: BTreeMap<&'a NodeId, (&'a str, bool)>,

    /// The edges entering each region node.
    inbound: BTreeMap<&'a NodeId, Vec<&'a Edge>>,
}

impl<'a> Feeds<'a> {
    fn new(nodes: impl IntoIterator<Item = (&'a NodeId, &'a str, bool)>, edges: &'a [Edge]) -> Self {
        let nodes = nodes
            .into_iter()
            .map(|(id, op, same)| (id, (op, same)))
            .collect::<BTreeMap<_, _>>();
        let mut inbound = BTreeMap::<_, Vec<_>>::new();
        for edge in edges.iter().filter(|edge| nodes.contains_key(&edge.to.node)) {
            inbound.entry(&edge.to.node).or_default().push(edge);
        }
        Self { nodes, inbound }
    }

    /// Returns what reaches every node of the region.
    ///
    /// `default` is what reaches each node of the default program's region, against which a node's own feed is
    /// compared to tell whether it is fed as there; it is `None` for the default program itself, each of whose nodes
    /// is.
    fn compute(&self, default: Option<&BTreeMap<&NodeId, Feed>>) -> BTreeMap<&'a NodeId, Feed> {
        let mut memo = BTreeMap::new();
        for &id in self.nodes.keys() {
            self.feed(id, default, &mut memo);
        }
        memo.into_iter().filter_map(|(id, feed)| Some((id, feed?))).collect()
    }

    /// Returns what reaches one node, recording it, and every feed it depends on, in `memo`.
    ///
    /// A node whose feed is being worked out is recorded as `None`, so that a cycle is a changed stream.
    fn feed(
        &self, id: &'a NodeId, default: Option<&BTreeMap<&NodeId, Feed>>, memo: &mut BTreeMap<&'a NodeId, Option<Feed>>,
    ) -> Feed {
        if let Some(Some(feed)) = memo.get(id) {
            return feed.clone();
        }
        memo.insert(id, None);
        let mut feed = Feed::default();
        for edge in self.inbound.get(id).into_iter().flatten() {
            for (stream, count) in self.streams(&edge.from, default, memo) {
                add_count(&mut feed.0, (edge.to.port.clone(), stream), count);
            }
        }
        memo.insert(id, Some(feed.clone()));
        feed
    }

    /// Returns the streams an output port carries.
    fn streams(
        &self, port: &'a PortRef, default: Option<&BTreeMap<&NodeId, Feed>>,
        memo: &mut BTreeMap<&'a NodeId, Option<Feed>>,
    ) -> Streams {
        let Some(&(op, same)) = self.nodes.get(&port.node) else {
            return Streams::from([(Stream::Outside(port.clone()), 1)]);
        };
        if matches!(memo.get(&port.node), Some(None)) {
            return Streams::from([(Stream::Changed, 1)]);
        }
        let feed = self.feed(&port.node, default, memo);
        if op == names::TEE || op == names::MERGE {
            let mut streams = Streams::new();
            for ((_, stream), count) in feed.0 {
                add_count(&mut streams, stream, count);
            }
            return streams;
        }
        let as_default = match default {
            None => true,
            Some(default) => same && !feed.is_changed() && default.get(&port.node) == Some(&feed),
        };
        if as_default {
            Streams::from([(Stream::Port(port.clone()), 1)])
        } else {
            Streams::from([(Stream::Changed, 1)])
        }
    }
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

/// Checks the output an `emit` node of the artifact delivers to against the artifact's base and the catalog.
///
/// An `emit` that names no valid output is left to validation, which reports its parameters.
///
/// # Errors
///
/// Returns an error if the base does not record the output, or the catalog does not declare it with the recorded
/// fingerprint. An output that neither records nor declares is undeclared: the error lists the outputs the default
/// program declares, which the artifact can emit to.
fn check_output(
    node: &ArtifactNodeId, params: &Params, base: &ArtifactBase, default_plan: &Plan, catalog: &dyn Catalog,
) -> Result<(), ComposeError> {
    let Some(output) = emitted_output(params) else {
        return Ok(());
    };
    let Some(recorded) = base.outputs.get(&output) else {
        if catalog.output(&output).is_none() {
            let declared = default_plan
                .outputs
                .iter()
                .map(|declared| declared.id.clone())
                .filter(|id| catalog.output(id).is_some())
                .collect();
            return Err(ComposeError::UndeclaredOutput {
                node: node.clone(),
                output,
                declared,
            });
        }
        return Err(ComposeError::UnrecordedOutput {
            node: node.clone(),
            output,
        });
    };
    match catalog.output(&output) {
        None => Err(ComposeError::RemovedOutput {
            output,
            declared: false,
        }),
        Some(declared) if declared.fingerprint != *recorded => {
            Err(ComposeError::RemovedOutput { output, declared: true })
        }
        Some(_) => Ok(()),
    }
}

/// Replaces the items of `default` for which `in_region` holds with `replacement`, keeping the default's order.
///
/// The replacement's items take the places of the region's items in order, and any left over follow the last of them;
/// with no region item, they follow everything. Replacing a region with its own items, in its own order, gives the
/// default back.
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

    /// The artifact emits to an output the host no longer declares, or declares with another fingerprint than the
    /// artifact's base records.
    #[snafu(display(
        "The artifact emits to output `{}`, which {}. Rebase the artifact on the current export.",
        output,
        if *declared { "the host now declares with another binding" } else { "the host no longer declares" }
    ))]
    RemovedOutput {
        /// The output.
        output: OutputId,

        /// Whether the host still declares the output, with another fingerprint.
        declared: bool,
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
mod tests;
