//! Receiving a policy artifact, and composing it with the host's sources and outputs.
//!
//! An artifact from the channel goes through two steps before it becomes a candidate plan:
//!
//! 1. **Receipt** ([`receive_artifact`]): the document is checked against the artifact limits (its size, also with its
//!    blob references counted as copies of the blobs, node count, blob sizes, and predicate leaves), read in a
//!    supported IR version and migrated to the current one, and refused if any node or blob carries a resolved
//!    configuration value or its blob table is inconsistent. The result is a [`ReceivedArtifact`], with the hash of the
//!    document as received, which approval binds to.
//! 2. **Composition** ([`compose`]): the artifact is a whole program. Composition resolves its names and builds the
//!    plan: the default program's `source` nodes, then the artifact's nodes, with every edge from `input.<name>`
//!    leaving the source whose `binding` is `<name>` ([`sources`]); `emit`s to recipients addressed to the outputs
//!    that deliver to them; configuration bindings resolved from the catalog; and the default program's outputs and
//!    resources. Nothing of the default program but its sources runs beside the artifact. The plan's provenance names
//!    the artifact's revision, and the plan is validated. Composition also reports whether the artifact was approved
//!    against another active plan (a stale review: it still installs).
//!
//! # Outputs that change under an artifact
//!
//! An `emit` that names an output by ID must name one the host declares. A new artifact naming one it does not declare
//! is refused ([`ComposeError::UndeclaredOutput`]). The artifact in effect, composed again after the host's
//! configuration changed ([`Approval::in_effect`]), keeps running with every `emit` to an output that is gone turned
//! into a counted drop under its ID ([`ComposeOutcome::Degraded`]), until the output returns. Recipients an `emit`
//! addresses by name are followed by name: a new artifact waits for those the host does not have yet.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{contains_resolved_values, to_canonical_string, PlanHash, BINDING_MEMBER, RESOLVED_MEMBER},
    export::{emitted_output, non_source_nodes},
    ir::{
        decode_params, ArtifactNodeId, ArtifactPortRef, ArtifactRevision, BindingName, BlobError, Blobs, Edge, HostId,
        InputName, IrError, LocalId, Node, NodeId, OperatorName, OutputId, Params, Plan, PlanProvenance,
        PolicyArtifact, PortName, PortRef, ReceivedArtifact, RecipientId, RecipientName, BLOB_MEMBER,
    },
    limits::values::{ARTIFACT_NODES, ARTIFACT_SIZE, BLOB_SIZE, PREDICATE_LEAVES},
    normalize::normalize_artifact,
    operators::{names, OperatorRegistry, StateContract, EMIT_TO, METRICS_PORT},
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
    /// The artifact in effect composes with every `emit` to an output that was removed replaced by a counted drop, and
    /// runs degraded; a new artifact is refused instead (see [Outputs that change under an
    /// artifact](self#outputs-that-change-under-an-artifact)).
    pub in_effect: bool,
}

/// An artifact composed with the host's sources and outputs.
#[derive(Clone, Debug)]
pub struct Composed {
    /// The composed plan, with every configuration binding resolved, and the artifact it runs in its provenance.
    pub plan: Plan,

    /// The composed plan's validation report. An artifact whose report has errors is rejected.
    pub report: ValidationReport,

    /// Which plan node each artifact node became: its reserved ID, the policy namespace, or the default program's one
    /// aggregation's ID (amendment 182). A node composition adds, such as the nodes of a partly available `emit`, has
    /// none.
    pub ids: BTreeMap<ArtifactNodeId, NodeId>,

    /// Whether the artifact was approved against an active plan other than the one it is composed against. The artifact
    /// still installs, with its review reported stale.
    pub stale_review: bool,

    /// Whether the artifact's `emit`s were composed as written, or, for the artifact in effect, some were dropped.
    pub outcome: ComposeOutcome,
}

impl Composed {
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
    /// Every `emit` delivers to what the artifact names.
    #[default]
    Composed,

    /// The artifact is in effect, and some outputs or recipients it names are gone: every `emit` to them was replaced by
    /// a counted drop. The artifact runs degraded, and its acknowledgment is `active` with the reasons, until they
    /// return or the artifact is replaced.
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
    /// An output the artifact names by ID.
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
    /// The host no longer declares the output.
    Removed,

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

/// Composes a received artifact, a whole program, with the host's sources and outputs.
///
/// `default_plan` is the default program as lowered, with its bindings resolved, and `catalog` the host's catalog of
/// the same configuration. Of the default program, only its `source` nodes, its outputs, and its resources are used.
///
/// - The artifact composes in [normal form](crate::normalize), as native lowering writes the default program.
/// - Each artifact node's plan ID is the reserved ID of the default program's non-source node with the same local ID,
///   or its local ID in the policy namespace; where the catalog runs at most one aggregation per program
///   ([`Catalog::single_aggregation`]), the artifact's one state owner takes the reserved ID of the default program's
///   one state owner, whatever the artifact calls it (amendment 182, until V10). A local ID equal to a source node's
///   is refused ([`ComposeError::ReservedSource`]).
/// - An edge from `input.<name>` leaves the port of the source whose `binding` is `<name>` ([`sources`]); a name no
///   source has is refused ([`ComposeError::UnknownSource`]).
/// - Blob references are replaced with their content, and configuration bindings resolve from `catalog`.
/// - An `emit` to recipients is addressed to the output with exactly their recipients; an `emit` to an output must name
///   one the catalog declares (see [Outputs that change under an artifact](self#outputs-that-change-under-an-artifact)).
/// - The plan is the default program's `source` nodes, in its order, then the artifact's nodes, in artifact order; its
///   edges are the artifact's, in artifact order; its outputs and resources are the default program's; and its
///   provenance names the artifact's revision. It is validated with `registry` and `catalog`; the report is returned,
///   not judged.
///
/// # Errors
///
/// Returns an error if the artifact cannot be composed at all; see [`ComposeError`].
pub fn compose(
    default_plan: &Plan, received: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Composed, ComposeError> {
    // The artifact composes in normal form, as the default program is lowered in it, so that a program that differs
    // from the default program only in how it is written composes to the default program.
    let artifact = &normalize_artifact(&received.artifact);
    let sources = sources(default_plan);
    if let Some(binding) = shared_source_binding(default_plan) {
        return Err(ComposeError::DefaultProgram {
            message: format!("more than one of its sources has the binding `{}`", binding),
        });
    }
    let program = non_source_nodes(default_plan);
    let stale_review = approval
        .approved_against
        .is_some_and(|approved| approval.active_plan_hash != Some(approved));

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
    let aggregation = catalog
        .single_aggregation()
        .then(|| the_aggregation(default_plan, artifact, &inlined, &program, catalog, registry))
        .flatten();
    let ids = node_ids(default_plan, artifact, &program, aggregation)?;
    let mut edges = artifact
        .edges
        .iter()
        .enumerate()
        .map(|(index, edge)| {
            Ok(Edge {
                from: plan_port(&edge.from, index, End::From, &sources, &ids)?,
                to: plan_port(&edge.to, index, End::To, &sources, &ids)?,
            })
        })
        .collect::<Result<Vec<_>, ComposeError>>()?;

    // Each artifact node with its plan ID, its parameters with blobs inlined and any recipients it emits to resolved
    // to their output, and how it is addressed.
    let mut placed = Vec::with_capacity(artifact.nodes.len());
    let mut outputs = EmittedOutputs::default();
    for (node, params) in artifact.nodes.iter().zip(inlined) {
        let (params, addressed) = address(&node.id, &node.op, params, catalog)?;
        // A new artifact waits for every recipient it names; only the artifact in effect delivers to those that remain.
        if let (Addressed::Partial { missing, .. }, false) = (&addressed, approval.in_effect) {
            outputs
                .pending_recipients
                .get_or_insert(ComposeError::UnknownRecipient {
                    node: node.id.clone(),
                    recipient: missing[0].clone(),
                });
        }
        if let (Addressed::Unavailable { recipients, reason }, false) = (&addressed, approval.in_effect) {
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
        placed.push((node, ids[&node.id].clone(), params, addressed));
    }

    // The default program's sources, in its order, then the artifact's nodes, in artifact order.
    let mut nodes = default_plan
        .nodes
        .iter()
        .filter(|node| !program.contains(&node.id))
        .cloned()
        .collect::<Vec<_>>();
    // The plan IDs taken so far, which the nodes a partly available `emit` adds must not take.
    let mut taken = ids.values().cloned().collect::<BTreeSet<_>>();
    taken.extend(default_plan.nodes.iter().map(|node| node.id.clone()));
    let counted_drop = |id: &NodeId| Node {
        id: id.clone(),
        op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
        params: Params::new(),
    };
    for (node, id, params, addressed) in placed {
        match addressed {
            Addressed::Partial {
                output,
                available,
                missing,
            } => {
                // The artifact in effect names recipients some of which are no longer available: the `emit` keeps
                // delivering to those that remain, through the output with exactly their recipients, and what it would
                // have sent the others is counted as dropped. The node becomes a `tee` whose first branch emits to the
                // remaining recipients and whose second is a counted drop.
                let (delivered, dropped) = partial_emit(&id, output, &mut taken);
                let reason = format!(
                    "{}; the `emit` still delivers to {}",
                    Unavailable::Unknown(missing[0].clone()).describe(),
                    describe_names(&available)
                );
                let entry = outputs
                    .dropped
                    .entry(DropTarget::Recipients(missing))
                    .or_insert_with(|| (DropReason::Unavailable(reason), Vec::new()));
                entry.1.push(dropped.id.clone());
                for (port, target) in [("0", &delivered.id), ("1", &dropped.id)] {
                    edges.push(Edge {
                        from: PortRef::new(id.clone(), port.parse().expect("a tee's port is a port name")),
                        to: PortRef::new(target.clone(), "in".parse().expect("`in` is a port name")),
                    });
                }
                let mut ports = Params::new();
                ports.insert("ports".parse().expect("`ports` is a parameter name"), Value::from(2));
                nodes.push(Node {
                    id,
                    op: OperatorName::new(names::TEE).expect("`tee` is an operator name"),
                    params: ports,
                });
                nodes.push(delivered);
                nodes.push(dropped);
            }
            Addressed::Unavailable { recipients, reason } => {
                // The artifact in effect keeps running, but nothing it emits reaches recipients that are no longer
                // available, or no longer on one host: the `emit` becomes a counted drop, under its ID.
                let entry = outputs
                    .dropped
                    .entry(DropTarget::Recipients(recipients))
                    .or_insert_with(|| (DropReason::Unavailable(reason.describe()), Vec::new()));
                entry.1.push(id.clone());
                nodes.push(counted_drop(&id));
            }
            Addressed::Recipients => nodes.push(Node {
                id,
                op: node.op.clone(),
                params,
            }),
            Addressed::Output => {
                let params = resolve_bindings(params, catalog, &node.id)?;
                let undeclared = (node.op.as_str() == names::EMIT)
                    .then(|| emitted_output(&params))
                    .flatten()
                    .filter(|output| catalog.output(output).is_none());
                match undeclared {
                    // The artifact in effect keeps running, but nothing it emits reaches an output that was removed:
                    // the `emit` becomes a counted drop, under its ID.
                    Some(output) if approval.in_effect => {
                        let entry = outputs
                            .dropped
                            .entry(DropTarget::Output(output))
                            .or_insert_with(|| (DropReason::Removed, Vec::new()));
                        entry.1.push(id.clone());
                        nodes.push(counted_drop(&id));
                    }
                    Some(output) => {
                        let declared = default_plan
                            .outputs
                            .iter()
                            .map(|declared| declared.id.clone())
                            .filter(|id| catalog.output(id).is_some())
                            .collect();
                        return Err(ComposeError::UndeclaredOutput {
                            node: node.id.clone(),
                            output,
                            declared,
                        });
                    }
                    None => nodes.push(Node {
                        id,
                        op: node.op.clone(),
                        params,
                    }),
                }
            }
        }
    }
    // A new artifact otherwise fine waits for the first recipients it names that have no output: it is refused with
    // any other error first, since that one would not go away when the recipients are declared.
    if let Some(pending) = outputs.pending_recipients {
        return Err(pending);
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
    let plan = Plan {
        ir_version: default_plan.ir_version,
        provenance: Some(PlanProvenance {
            config_change: default_plan
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.config_change),
            artifact: Some(ArtifactRevision {
                revision: approval.revision,
                hash: received.hash,
            }),
            // A composed plan is never a boot plan: the native components describe the default program's layout only.
            native_components: BTreeMap::new(),
        }),
        nodes,
        edges,
        outputs: default_plan.outputs.clone(),
        // Resources, like output declarations, come from the default program only: an artifact cannot declare them.
        resources: default_plan.resources.clone(),
    };
    let report = Validator::new(registry, catalog)
        .with_baseline(default_plan)
        .validate(&plan);
    let ids = ids.into_iter().map(|(local, id)| (local.clone(), id)).collect();
    Ok(Composed {
        plan,
        report,
        ids,
        stale_review,
        outcome,
    })
}

/// Returns a binding two of the `source` nodes of `plan` share, if any do: a host names each source once.
fn shared_source_binding(plan: &Plan) -> Option<&str> {
    let mut seen = BTreeSet::new();
    plan.nodes
        .iter()
        .filter(|node| node.op.as_str() == names::SOURCE)
        .filter_map(|node| node.params.get("binding").and_then(Value::as_str))
        .find(|binding| !seen.insert(*binding))
}

/// Which end of an edge a port is at.
#[derive(Clone, Copy)]
enum End {
    From,
    To,
}

/// Returns the plan port an artifact edge's end refers to: the port of the source of that name, or a port of an
/// artifact node, under its plan ID.
///
/// # Errors
///
/// Returns an error if the end is a source the host does not have, a source the edge enters, or a port of a node the
/// artifact does not have.
fn plan_port(
    reference: &ArtifactPortRef, index: usize, end: End, sources: &BTreeMap<InputName, PortRef>,
    ids: &BTreeMap<&ArtifactNodeId, NodeId>,
) -> Result<PortRef, ComposeError> {
    match (reference, end) {
        (ArtifactPortRef::Input(input), End::From) => {
            sources.get(input).cloned().ok_or_else(|| ComposeError::UnknownSource {
                name: input.clone(),
                known: sources.keys().cloned().collect(),
            })
        }
        (ArtifactPortRef::Input(_), End::To) => Err(ComposeError::Malformed {
            message: format!(
                "edge {} enters `{}`, which is a source; edges leave the host's sources",
                index, reference
            ),
        }),
        (ArtifactPortRef::Node { node, port }, end) => ids
            .get(node)
            .map(|id| PortRef::new(id.clone(), port.clone()))
            .ok_or_else(|| ComposeError::Malformed {
                message: format!(
                    "edge {} {} `{}`, but the artifact has no node `{}`",
                    index,
                    match end {
                        End::From => "leaves",
                        End::To => "enters",
                    },
                    reference,
                    node
                ),
            }),
    }
}

/// Returns the host's sources in `plan`, by name: for each `source` node, its `binding` parameter, mapped to its
/// output port. An artifact edge from `input.<name>` leaves that port.
///
/// A `source` node without a valid `binding` is left out, for validation to report; of two with one binding, the
/// first in plan order is kept.
pub fn sources(plan: &Plan) -> BTreeMap<InputName, PortRef> {
    let mut sources = BTreeMap::new();
    for node in plan.nodes.iter().filter(|node| node.op.as_str() == names::SOURCE) {
        let Some(name) = node
            .params
            .get("binding")
            .and_then(Value::as_str)
            .and_then(|binding| InputName::new(binding).ok())
        else {
            continue;
        };
        let port = PortRef::new(
            node.id.clone(),
            PortName::new(METRICS_PORT).expect("a source's port is a port name"),
        );
        sources.entry(name).or_insert(port);
    }
    sources
}

/// What composing one artifact's `emit`s found about the outputs they deliver to.
#[derive(Default)]
struct EmittedOutputs {
    /// The outputs, or recipients, whose `emit`s were dropped, with why and the nodes, for the artifact in effect.
    dropped: BTreeMap<DropTarget, (DropReason, Vec<NodeId>)>,

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

    /// An `emit` of recipients some of which the directory no longer names: the others, `available`, resolve to
    /// `output`, which the artifact in effect goes on delivering to, while a new artifact waits for `missing`.
    Partial {
        /// The output with exactly the available recipients.
        output: OutputId,

        /// The names the directory has, as the artifact writes them.
        available: Vec<RecipientName>,

        /// The names it does not have.
        missing: Vec<RecipientName>,
    },

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
        Err(reason) => {
            // Some names the directory no longer has: the others may still resolve to an output.
            if let Unavailable::Unknown(_) = reason {
                let (available, missing): (Vec<_>, Vec<_>) = names
                    .iter()
                    .cloned()
                    .partition(|name| catalog.recipient(name).is_some());
                let output = (!available.is_empty())
                    .then(|| resolve_recipients(&available, catalog).ok())
                    .flatten()
                    .and_then(|recipients| catalog.output_for(&recipients));
                if let Some(output) = output {
                    return Ok((
                        params,
                        Addressed::Partial {
                            output,
                            available,
                            missing,
                        },
                    ));
                }
            }
            Ok((
                params,
                Addressed::Unavailable {
                    recipients: names,
                    reason,
                },
            ))
        }
    }
}

/// Returns the two nodes a partly available `emit` with plan ID `id` adds after it, once it is a `tee`: an `emit` to
/// `output`, the output of its remaining recipients, and a counted drop, which counts what it would have sent the
/// recipients that are gone. Their IDs are `id`'s with `_delivered` and `_missing` after its local part, cut to fit an
/// identifier and numbered when taken; each is added to `taken`.
fn partial_emit(id: &NodeId, output: OutputId, taken: &mut BTreeSet<NodeId>) -> (Node, Node) {
    let mut derive = |suffix: &str| {
        let local = id.local().as_str();
        let base = &local[..local.len().min(crate::ir::MAX_IDENTIFIER_LEN - suffix.len() - 3)];
        let base = base.trim_end_matches('_');
        let mut attempt = 1usize;
        loop {
            let candidate = if attempt == 1 {
                format!("{}{}", base, suffix)
            } else {
                format!("{}{}_{}", base, suffix, attempt)
            };
            let node = LocalId::new(candidate)
                .map(NodeId::policy)
                .expect("a derived local ID is an identifier");
            if taken.insert(node.clone()) {
                return node;
            }
            attempt += 1;
        }
    };
    let delivered = Node {
        id: derive("_delivered"),
        op: OperatorName::new(names::EMIT).expect("`emit` is an operator name"),
        params: std::iter::once((
            "output".parse().expect("`output` is a parameter name"),
            Value::String(output.to_string()),
        ))
        .collect(),
    };
    let dropped = Node {
        id: derive("_missing"),
        op: OperatorName::new(names::DROP).expect("`drop` is an operator name"),
        params: Params::new(),
    };
    (delivered, dropped)
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
        // An `emit` whose names the directory has only in part delivers, in effect, to those it has.
        let names = names
            .into_iter()
            .filter(|name| catalog.recipient(name).is_some())
            .collect::<Vec<_>>();
        let Ok(recipients) = resolve_recipients(&names, catalog) else {
            continue;
        };
        if recipients.is_empty() {
            continue;
        }
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
/// per program: when the artifact has exactly one state owner, the default program has exactly one among `program`, its
/// non-source nodes, and no artifact node has that one's local ID, the artifact's state owner is the default program's,
/// whatever it is called.
///
/// So a program's one aggregation keeps the default program's ID (`dsd_agg`, say), and with it its lineage, its
/// isolation slot's series, and its telemetry, when an author renames it or writes the program from scratch. Whether
/// its state is kept is then decided as for any node with that ID: by its operation and its lineage parameters.
///
/// A node whose operator does not configure, with its bindings resolved from the catalog, is not counted: validation
/// reports it.
fn the_aggregation<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, inlined: &[Params], program: &BTreeSet<NodeId>,
    catalog: &dyn Catalog, registry: &OperatorRegistry,
) -> Option<(&'a ArtifactNodeId, NodeId)> {
    let owns_state = |op: &OperatorName, params: &Params| {
        registry
            .configure(op.as_str(), params)
            .is_ok_and(|configured| matches!(configured.contract().state, StateContract::Keyed(_)))
    };
    let region_owners = default_plan
        .nodes
        .iter()
        .filter(|node| program.contains(&node.id) && owns_state(&node.op, &node.params))
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

/// Assigns every artifact node its plan ID: the reserved ID of a node of `program`, the default program's non-source
/// nodes, with the same local ID, or its local ID in the policy namespace. `aggregation`, if given, is the artifact's
/// one state owner and the reserved ID it takes instead ([`the_aggregation`]).
///
/// # Errors
///
/// Returns an error if two artifact nodes have the same ID, or one has the ID of one of the default program's sources.
fn node_ids<'a>(
    default_plan: &Plan, artifact: &'a PolicyArtifact, program: &BTreeSet<NodeId>,
    aggregation: Option<(&'a ArtifactNodeId, NodeId)>,
) -> Result<BTreeMap<&'a ArtifactNodeId, NodeId>, ComposeError> {
    let mut ids = BTreeMap::new();
    for node in &artifact.nodes {
        let reserved = NodeId::reserved(node.id.to_local_id());
        // A source's ID is refused even for the adopted aggregation, whose plan ID would not collide: the artifact's
        // ID names the source, whatever plan ID the node takes.
        if !program.contains(&reserved) && default_plan.node(&reserved).is_some() {
            return Err(ComposeError::ReservedSource {
                node: node.id.clone(),
                reserved,
            });
        }
        let id = if let Some((_, id)) = aggregation.as_ref().filter(|(owner, _)| **owner == node.id) {
            id.clone()
        } else if program.contains(&reserved) {
            reserved
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

/// Composes artifacts with the host's current sources and outputs.
///
/// The installer core composes the artifact it accepts through this, without knowing the default program or the
/// catalog: the host implements it over [`compose`], with the default program it last lowered and its catalog.
pub trait Composer {
    /// Composes one received artifact with the host's current sources and outputs.
    ///
    /// # Errors
    ///
    /// Returns an error if the artifact cannot be composed; see [`compose`].
    fn compose(&self, artifact: &ReceivedArtifact, approval: &Approval) -> Result<Composed, ComposeError>;
}

/// An artifact that cannot be composed into the default program.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ComposeError {
    /// The default program cannot be composed with, such as one with two sources of one binding.
    #[snafu(display("The default program cannot be composed with: {}", message))]
    DefaultProgram {
        /// Why.
        message: String,
    },

    /// An artifact node's local ID is the ID of one of the default program's sources.
    #[snafu(display(
        "Artifact node `{}` has the ID of source `{}`, which the host places. Rename the node.",
        node,
        reserved
    ))]
    ReservedSource {
        /// The artifact node.
        node: ArtifactNodeId,

        /// The source node it collides with.
        reserved: NodeId,
    },

    /// An artifact edge leaves `input.<name>`, and the host has no source of that name.
    #[snafu(display(
        "The artifact reads `input.{}`, and the host has no source `{}`. {}",
        name,
        name,
        known_sources(known)
    ))]
    UnknownSource {
        /// The name, as the artifact writes it.
        name: InputName,

        /// The sources the host has, in name order.
        known: Vec<InputName>,
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

    /// An `emit` of a new artifact delivers to an output the host does not declare.
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
    /// enters a source, or a blob reference cannot be inlined.
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

/// Says which sources the host has, for an error message about one it does not.
fn known_sources(sources: &[InputName]) -> String {
    if sources.is_empty() {
        return "The host has no sources.".to_string();
    }
    let listed = sources
        .iter()
        .map(|source| format!("`input.{}`", source))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Read one of its sources: {}.", listed)
}

#[cfg(test)]
mod change_tests;
#[cfg(test)]
pub(crate) mod tests;
