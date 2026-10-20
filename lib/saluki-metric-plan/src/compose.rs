//! Receiving a policy artifact, and composing it with the host's sources and outputs.
//!
//! An artifact from the channel goes through two steps before it becomes a candidate plan:
//!
//! 1. **Receipt** ([`receive_artifact`]): the document is checked against the artifact limits (its size, also with its
//!    blob references counted as copies of the blobs, node count, blob sizes, and predicate leaves), read in a
//!    supported IR version and migrated to the current one, and refused if any node or blob carries a resolved
//!    configuration value or its blob table is inconsistent. The result is a [`ReceivedArtifact`], with the hash of the
//!    document as received, which approval binds to.
//! 2. **Composition** ([`compose`]): the artifact is a whole program, which [elaboration](crate::elaborate) resolves
//!    and writes in its core form: the default program's `source` nodes, then the artifact's nodes, with every edge
//!    from `input.<name>` leaving the source whose `binding` is `<name>` ([`sources`]); `emit`s to recipients addressed
//!    to the outputs that deliver to them; configuration bindings written as their values from the catalog; and the
//!    default program's outputs and resources. Nothing of the default program but its sources runs beside the artifact.
//!    The plan's provenance names the artifact's revision, and the plan is validated. Composition also reports whether
//!    the artifact was approved against another active plan (a stale review: it still installs).
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
    canonical::{contains_resolved_values, to_canonical_string, PlanHash},
    elaborate::{elaborate, Elaborated},
    ir::{
        decode_params, ArtifactNodeId, BindingName, BlobError, Blobs, HostId, InputName, IrError, NodeId, OutputId,
        Params, Plan, PolicyArtifact, PortName, PortRef, ReceivedArtifact, RecipientId, RecipientName, BLOB_MEMBER,
    },
    limits::values::{ARTIFACT_NODES, ARTIFACT_SIZE, BLOB_SIZE, PREDICATE_LEAVES},
    operators::{names, OperatorRegistry, EMIT_TO, METRICS_PORT},
    validate::{predicate_leaves, Catalog},
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

    /// The host's active plan hash when the artifact is composed, if a generation is active.
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

/// An artifact composed with the host's sources and outputs: its [elaboration](crate::elaborate).
pub type Composed = Elaborated;

/// Composes a received artifact, a whole program, with the host's sources and outputs: [`elaborate`] it.
///
/// # Errors
///
/// Returns an error if the artifact cannot be composed at all; see [`ComposeError`].
pub fn compose(
    default_plan: &Plan, received: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Composed, ComposeError> {
    elaborate(default_plan, received, approval, catalog, registry)
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
pub(crate) fn describe_names(names: &[RecipientName]) -> String {
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

/// Why the recipients an `emit` names have no output now.
pub(crate) enum Unavailable {
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
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Unknown(name) => format!("the host has no recipient `{}`", name),
            Self::TwoHosts(_) => "they are recipients of more than one delivery host".to_string(),
            Self::NoOutput => "no output delivers to exactly them".to_string(),
            Self::Refused(why) => format!("no output delivers to exactly them, and the host adds none: {}", why),
        }
    }
}

/// Returns the recipient names of an `emit`'s `to` parameter, or `None` if it has none.
///
/// # Errors
///
/// Returns an error if `to` is not a list of recipient names, or is given beside `output`.
pub(crate) fn recipient_names(
    node: &ArtifactNodeId, params: &Params,
) -> Result<Option<Vec<RecipientName>>, ComposeError> {
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
pub(crate) fn resolve_recipients(
    names: &[RecipientName], catalog: &dyn Catalog,
) -> Result<Vec<RecipientId>, Unavailable> {
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

/// Returns parameters with every blob reference replaced by the blob's content.
pub(crate) fn inline_blobs(params: &Params, blobs: &Blobs) -> Result<Params, BlobError> {
    params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            blobs.inline(&mut value)?;
            Ok((name.clone(), value))
        })
        .collect()
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
