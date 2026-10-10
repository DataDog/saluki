use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::{ResultExt as _, Snafu};

use super::{
    document::parse_document,
    id::{
        ArtifactNodeId, ArtifactPortRef, ExitName, InputName, NativeComponentId, NodeId, OperatorName, OutputId,
        PortRef, RecipientId, ScopeName,
    },
    params::Params,
    version::{migrate, BlobTable, EdgeFromExit, Invalid, IrError, Parse, ResolvedInArtifact, Unhashable},
};
use crate::{
    canonical::{contains_resolved_values, strip_resolved_values, CanonicalError, PlanHash, ResolvedValues},
    extension::DeliveryPolicy,
    model::PortType,
};

/// A logical plan: a directed acyclic graph of operator nodes connected by edges between their ports.
///
/// Every input and output port of every node has exactly one edge; fan-out and fan-in exist only through the routing
/// operators `tee` and `merge`, and every path ends in an `emit` to a declared output or in a `drop`. Validation checks
/// these rules and more; deserializing a plan checks only its shape.
///
/// A plan's nodes and edges are kept in the order they are written, and are hashed in that order. Producers of plans,
/// such as lowering, emit them in a deterministic order.
///
/// # Normalized form
///
/// Serializing a plan writes its **normalized form**: members whose value is their default, such as an empty list of
/// outputs or recipients and the native delivery policy, are left out. A plan read from its normalized form writes the
/// same document back, and its [hash][Self::hash] is the RFC 8785 digest of that document. A document that writes a
/// default explicitly reads as the same plan, and has the same hash, although its own digest differs.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The IR version the plan is written in.
    pub ir_version: u32,

    /// Where the plan as a whole came from.
    ///
    /// Plan-level provenance describes the plan's origin, not what it does, so it is not part of the plan's
    /// [hash][Self::hash].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<PlanProvenance>,

    /// The operator nodes.
    pub nodes: Vec<Node>,

    /// The edges between node ports.
    pub edges: Vec<Edge>,

    /// The declared outputs that `emit` nodes deliver to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputDecl>,

    /// The replaceable regions of the plan, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scopes: Scopes,
}

impl Plan {
    /// Reads a plan from JSON, migrating it from an older accepted IR version if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not JSON, an object in it repeats a member name, its IR version is not
    /// accepted, or it does not match the plan's structure.
    pub fn from_json(json: &str) -> Result<Self, IrError> {
        Self::from_value(parse_document(json).context(Parse)?)
    }

    /// Reads a plan from a JSON value, migrating it from an older accepted IR version if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if its IR version is not accepted, or it does not match the plan's structure.
    pub fn from_value(mut value: Value) -> Result<Self, IrError> {
        migrate(&mut value)?;
        serde_json::from_value(value).context(Invalid { what: "plan" })
    }

    /// Returns the plan's hash: the RFC 8785 digest of its [normalized form](#normalized-form), without plan-level
    /// provenance.
    ///
    /// With [`ResolvedValues::Exclude`], the hash also leaves out the resolved values of configuration bindings in node
    /// parameters, so it changes only when the plan's structure, operators, literal parameters, or binding names
    /// change.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan contains an integer parameter outside the range that canonical JSON allows.
    pub fn hash(&self, resolved: ResolvedValues) -> Result<PlanHash, CanonicalError> {
        let mut value = serde_json::to_value(self).context(crate::canonical::ToJson)?;
        if let Some(members) = value.as_object_mut() {
            members.remove("provenance");
            if resolved == ResolvedValues::Exclude {
                strip_node_params(members);
            }
        }
        PlanHash::of_value(&value)
    }

    /// Returns the node with the given identifier, if there is one.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    /// Returns the output with the given identifier, if there is one.
    pub fn output(&self, id: &OutputId) -> Option<&OutputDecl> {
        self.outputs.iter().find(|output| &output.id == id)
    }
}

/// Removes the resolved values of configuration bindings from the parameters of every node of a serialized plan.
///
/// Bindings occur only in node parameters, so nothing else is touched: a map elsewhere in the plan may have members
/// named `config` and `resolved`, such as outputs or scope inputs with those names.
fn strip_node_params(document: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(nodes)) = document.get_mut("nodes") else {
        return;
    };
    for node in nodes {
        if let Some(Value::Object(params)) = node.get_mut("params") {
            params.values_mut().for_each(strip_resolved_values);
        }
    }
}

/// Where a plan as a whole came from.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanProvenance {
    /// The number of the configuration change the plan was lowered from, counted per process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_change: Option<u64>,

    /// The artifact revisions composed into the plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRevision>,
}

/// One revision of an approved artifact.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRevision {
    /// The artifact's revision number. Revisions strictly increase.
    pub revision: u64,

    /// The hash of the artifact.
    pub hash: PlanHash,
}

/// Where a node came from, which is also the authority for any loss it causes.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Provenance {
    /// Lowered from native configuration of the named component, serialized as `{"native": "<component>"}`.
    Native(NativeComponentId),

    /// Added or changed by an approved artifact, serialized as `{"artifact": {"revision": <n>, "hash": "<hash>"}}`.
    Artifact(ArtifactRevision),
}

/// One operator node.
///
/// A node's ports follow from its operator and parameters: for example, a `split` has the output ports `true` and
/// `false`, and a `tee` has as many numbered output ports as its parameters say.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// The node's identifier, stable across generations.
    pub id: NodeId,

    /// The registered operator the node runs.
    pub op: OperatorName,

    /// The operator's parameters.
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub params: Params,

    /// Where the node came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// An edge from one node's output port to another node's input port.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// The output port the edge leaves.
    pub from: PortRef,

    /// The input port the edge enters.
    pub to: PortRef,
}

/// A declared output: a destination that `emit` nodes deliver to, bound to one encoder stream and its recipients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDecl {
    /// The output's identifier.
    pub id: OutputId,

    /// The type of the records the output accepts.
    pub accepts: PortType,

    /// A digest of the output's resolved binding: its encoder, recipient set, and forwarder.
    ///
    /// An artifact records the fingerprint of every output it was written against, so that it is never silently
    /// applied to an output whose binding has changed.
    pub fingerprint: PlanHash,

    /// The physical recipients the output delivers to. Outputs that share a recipient feed the same delivery queue.
    ///
    /// An output without recipients, such as an internal tap, delivers to no endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<RecipientId>,

    /// What the output does when it cannot keep up or when it closes. Defaults to the native policy.
    #[serde(default, skip_serializing_if = "DeliveryPolicy::is_native")]
    pub delivery: DeliveryPolicy,
}

/// The replaceable regions of a plan, by name.
pub type Scopes = BTreeMap<ScopeName, ScopeDecl>;

/// A replaceable region of a plan.
///
/// A scope is the region between its inputs and its exits: every node reachable from the ports its inputs name,
/// without entering one of the ports its exits name. A policy artifact replaces that region. A scope without exits,
/// such as `post_enrich`, extends to the `emit` and `drop` nodes that end its paths.
///
/// Validation checks that a scope is closed, so that replacing it changes nothing outside it: nothing outside the
/// region feeds a node inside it except through the scope's inputs, each exit is fed from inside the region and leads
/// out of it, and no two scopes share a node.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeDecl {
    /// The scope's named inputs, each mapped to the output port that feeds it.
    pub inputs: BTreeMap<InputName, PortRef>,

    /// The scope's named exits, each mapped to the input port outside the scope that it feeds.
    ///
    /// Exits let a scope end before the end of its paths, such as a region of processing before aggregation. Artifact
    /// edges reach them as `exit.<name>`, so the artifact node ID `exit` is reserved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exits: BTreeMap<ExitName, PortRef>,
}

/// A policy artifact: a replacement for one scope of a plan, written against a recorded base.
///
/// An artifact is typically derived by editing the export of the default program's scope. It records the hash of that
/// base program and the fingerprints of the outputs it refers to, so that a structural change to the default program
/// or to an output is detected rather than silently merged. One artifact replaces exactly one scope.
///
/// Artifact nodes have local identifiers and no provenance: composing the artifact into a plan assigns both. Artifact
/// edges refer to the scope's inputs as `input.<name>`. Artifacts refer to configuration by binding only, never with
/// resolved values, and may write any parameter value as a reference to a blob in their blob table, which composition
/// replaces with the blob's content (see [`Blobs::inline`]).
///
/// Serializing an artifact writes its normalized form, as for a [plan](Plan#normalized-form). Approval binds to the
/// hash of the document as it was received, which [`ReceivedArtifact`] keeps.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyArtifact {
    /// The IR version the artifact is written in.
    pub ir_version: u32,

    /// The scope the artifact replaces.
    pub scope: ScopeName,

    /// What the artifact was written against.
    pub base: ArtifactBase,

    /// The artifact's nodes.
    pub nodes: Vec<ArtifactNode>,

    /// The edges between the scope's inputs, the artifact's node ports, and the scope's exits.
    pub edges: Vec<ArtifactEdge>,

    /// Content-addressed values that parameters refer to.
    #[serde(default, skip_serializing_if = "Blobs::is_empty")]
    pub blobs: Blobs,
}

impl PolicyArtifact {
    /// Reads an artifact from JSON, migrating it from an older accepted IR version if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not JSON or an object in it repeats a member name, or for any reason
    /// [`from_value`][Self::from_value] does.
    pub fn from_json(json: &str) -> Result<Self, IrError> {
        Self::from_value(parse_document(json).context(Parse)?)
    }

    /// Reads an artifact from a JSON value, migrating it from an older accepted IR version if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if its IR version is not accepted, it does not match the artifact's structure, an edge leaves
    /// one of the scope's exits, a node's parameters carry a resolved configuration value, or its blob table is
    /// inconsistent: a blob stored under a hash other than its content's, or a reference to a blob the table does not
    /// hold. Whether the scope declares the exits the edges enter is checked when the artifact is composed.
    pub fn from_value(mut value: Value) -> Result<Self, IrError> {
        migrate(&mut value)?;
        let artifact: Self = serde_json::from_value(value).context(Invalid {
            what: "policy artifact",
        })?;
        if let Some((index, edge)) = artifact
            .edges
            .iter()
            .enumerate()
            .find(|(_, edge)| matches!(edge.from, ArtifactPortRef::Exit(_)))
        {
            return EdgeFromExit {
                index,
                exit: edge.from.clone(),
            }
            .fail();
        }
        for node in &artifact.nodes {
            if node.params.iter().any(|(_, value)| contains_resolved_values(value)) {
                return ResolvedInArtifact { node: node.id.clone() }.fail();
            }
            for (_, value) in node.params.iter() {
                artifact.blobs.check_references(value).context(BlobTable)?;
            }
        }
        artifact.blobs.verify().context(BlobTable)?;
        Ok(artifact)
    }
}

/// A policy artifact as it was received, with the hash that approval binds to.
///
/// Approval binds to the RFC 8785 digest of the artifact document exactly as its author wrote it and its approver
/// reviewed it, before migration from an older IR version. That digest differs from the digest of the
/// [artifact][PolicyArtifact]'s serialized form when the document writes a defaulted member explicitly, or when it is
/// migrated, so it is computed from the received document and kept here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedArtifact {
    /// The artifact, migrated to the current IR version.
    pub artifact: PolicyArtifact,

    /// The digest of the canonical form of the document as it was received.
    pub hash: PlanHash,
}

impl ReceivedArtifact {
    /// Reads an artifact from JSON, and computes the hash of the document as received.
    ///
    /// # Errors
    ///
    /// Returns an error for any reason [`PolicyArtifact::from_json`] does, or if the document has no canonical form,
    /// because it contains a number other than an integer within ±(2^53 - 1).
    pub fn from_json(json: &str) -> Result<Self, IrError> {
        Self::from_value(parse_document(json).context(Parse)?)
    }

    /// Reads an artifact from a JSON value, and computes the hash of the value as received.
    ///
    /// # Errors
    ///
    /// Returns an error for any reason [`PolicyArtifact::from_value`] does, or if the value has no canonical form.
    pub fn from_value(value: Value) -> Result<Self, IrError> {
        let hash = PlanHash::of_value(&value).context(Unhashable)?;
        let artifact = PolicyArtifact::from_value(value)?;
        Ok(Self { artifact, hash })
    }
}

/// What a policy artifact was written against.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactBase {
    /// The hash of the exported program the artifact was derived from, without resolved values.
    ///
    /// When the current default program's export has a different hash, the artifact is reported as stale.
    pub program_hash: PlanHash,

    /// The fingerprint of every output the artifact refers to, as of its base.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<OutputId, PlanHash>,
}

/// One node of a policy artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactNode {
    /// The node's local identifier.
    pub id: ArtifactNodeId,

    /// The registered operator the node runs.
    pub op: OperatorName,

    /// The operator's parameters.
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub params: Params,
}

/// An edge within a policy artifact.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEdge {
    /// The port the edge leaves: a scope input, or an output port of an artifact node.
    pub from: ArtifactPortRef,

    /// The port the edge enters: an input port of an artifact node, or a scope exit.
    pub to: ArtifactPortRef,
}

/// An error in a plan's blob table.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum BlobError {
    /// A blob is stored under a hash that is not the hash of its content.
    #[snafu(display("The blob stored as {} has content hashing to {}.", claimed, actual))]
    Mismatch {
        /// The hash the blob is stored under.
        claimed: PlanHash,

        /// The hash of the blob's content.
        actual: PlanHash,
    },

    /// A blob's content has no canonical form.
    #[snafu(display("The blob stored as {} cannot be hashed: {}", claimed, source))]
    UnhashableBlob {
        /// The hash the blob is stored under.
        claimed: PlanHash,

        /// The underlying error.
        source: CanonicalError,
    },

    /// A parameter refers to a blob the table does not hold.
    #[snafu(display(
        "A parameter refers to the blob {}, which the blob table does not hold. Add the blob, or write the value \
         inline.",
        hash
    ))]
    MissingBlob {
        /// The hash referred to.
        hash: PlanHash,
    },

    /// A blob reference does not name a blob by its hash.
    #[snafu(display(
        "Invalid blob reference {}: a blob reference is {{\"blob\": \"sha256:<hex>\"}}, an object whose only member \
         is the blob's hash.",
        reference
    ))]
    InvalidReference {
        /// The reference as written.
        reference: Value,
    },
}

/// The member that marks a JSON object in an artifact's parameters as a blob reference: `{"blob": "<hash>"}`.
///
/// Parameters reserve this member, as they reserve [`config`](crate::canonical::BINDING_MEMBER): no map or record
/// parameter can have a member with this name.
pub const BLOB_MEMBER: &str = "blob";

/// A table of content-addressed values, keyed by the hash of their canonical JSON form.
///
/// Blobs are how an artifact carries large parameter values, such as long allowlists or mapper profiles, and values it
/// uses in several nodes, without repeating them: any parameter value of an artifact node, or any value nested in one,
/// may be written as a **blob reference**, `{"blob": "<hash>"}`. Blobs travel inside the artifact and count toward its
/// size. Composing the artifact into a plan replaces every reference with the blob's content
/// ([`inline`][Self::inline]), so plans never contain blob references, and operators never see them.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Blobs(BTreeMap<PlanHash, Value>);

impl Blobs {
    /// Returns `true` if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of blobs.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns the blob with the given hash, if there is one.
    pub fn get(&self, hash: &PlanHash) -> Option<&Value> {
        self.0.get(hash)
    }

    /// Adds a blob, and returns its hash.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` has no canonical form.
    pub fn insert(&mut self, value: Value) -> Result<PlanHash, CanonicalError> {
        let hash = PlanHash::of_value(&value)?;
        self.0.insert(hash, value);
        Ok(hash)
    }

    /// Checks that every blob is stored under the hash of its content.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first blob whose content does not match its hash.
    pub fn verify(&self) -> Result<(), BlobError> {
        for (claimed, value) in &self.0 {
            let actual = PlanHash::of_value(value).context(UnhashableBlob { claimed: *claimed })?;
            if actual != *claimed {
                return Err(BlobError::Mismatch {
                    claimed: *claimed,
                    actual,
                });
            }
        }
        Ok(())
    }

    /// Returns an iterator over the blobs, in hash order.
    pub fn iter(&self) -> impl Iterator<Item = (&PlanHash, &Value)> {
        self.0.iter()
    }

    /// Checks that every blob reference in `value` names a blob this table holds.
    ///
    /// # Errors
    ///
    /// Returns an error for the first reference that is malformed or names a blob the table does not hold.
    pub fn check_references(&self, value: &Value) -> Result<(), BlobError> {
        self.inline(&mut value.clone())
    }

    /// Replaces every blob reference in `value` with a copy of the blob's content.
    ///
    /// The content itself is not searched for references: a blob holds a value, never another reference.
    ///
    /// # Errors
    ///
    /// Returns an error for the first reference that is malformed or names a blob the table does not hold. `value` may
    /// then be partly inlined.
    pub fn inline(&self, value: &mut Value) -> Result<(), BlobError> {
        match value {
            Value::Object(members) if members.contains_key(BLOB_MEMBER) => {
                let hash = match (members.len(), members.get(BLOB_MEMBER)) {
                    (1, Some(Value::String(hash))) => hash.parse::<PlanHash>().ok(),
                    _ => None,
                };
                let Some(hash) = hash else {
                    return InvalidReference {
                        reference: value.clone(),
                    }
                    .fail();
                };
                let content = self.0.get(&hash).ok_or(BlobError::MissingBlob { hash })?;
                *value = content.clone();
                Ok(())
            }
            Value::Object(members) => members.values_mut().try_for_each(|member| self.inline(member)),
            Value::Array(values) => values.iter_mut().try_for_each(|element| self.inline(element)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ir::{id::LocalId, IR_VERSION},
        model::{KindSet, TimestampForm},
    };

    /// A change made to a JSON document, for table-driven rejection tests.
    type Mutation = fn(&mut Value);

    const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    /// The IR example from the implementation design, with a concrete fingerprint and the edges it implies.
    fn design_example() -> Value {
        json!({
            "ir_version": 1,
            "nodes": [
                {"id": "dsd_in", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "mixed"},
                 "provenance": {"native": "dsd_in"}},
                {"id": "dsd_prefix_filter_blocklist", "op": "split",
                 "params": {"predicate": {"intake_name": {"matches": {
                    "values": {"config": "dogstatsd.metric_filter.values"},
                    "prefix": {"config": "dogstatsd.metric_filter.match_prefix"}}}}}},
                {"id": "policy/drop_debug", "op": "drop",
                 "provenance": {"artifact": {"revision": 3, "hash": ZERO_HASH}}}
            ],
            "edges": [{"from": "dsd_in.metrics", "to": "dsd_enrich.in"}],
            "outputs": [{"id": "dd_metrics_encode", "accepts": {"kinds": "all", "timestamps": "mixed"},
                         "fingerprint": ZERO_HASH, "recipients": ["dd_out/primary"], "delivery": "native"}],
            "scopes": {"post_enrich": {"inputs": {"metrics": "metrics_enrich_host_tags.out"}}}
        })
    }

    fn artifact_example() -> Value {
        json!({
            "ir_version": 1,
            "scope": "post_enrich",
            "base": {"program_hash": ZERO_HASH, "outputs": {"dd_metrics_encode": ZERO_HASH}},
            "nodes": [
                {"id": "metrics_policy", "op": "tee", "params": {"ports": 2}},
                {"id": "drop_debug", "op": "split", "params": {"predicate": {"name": {"prefix": "demo.debug."}}}}
            ],
            "edges": [
                {"from": "input.metrics", "to": "metrics_policy.in"},
                {"from": "metrics_policy.1", "to": "drop_debug.in"}
            ]
        })
    }

    #[test]
    fn plans_round_trip_through_json() {
        let value = design_example();
        let plan = Plan::from_value(value.clone()).unwrap();

        assert_eq!(plan.ir_version, IR_VERSION);
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(
            plan.nodes[0].provenance,
            Some(Provenance::Native(NativeComponentId::new("dsd_in").unwrap()))
        );
        assert_eq!(plan.nodes[1].provenance, None);
        assert!(!plan.nodes[2].id.is_reserved());
        assert_eq!(plan.edges[0].from.to_string(), "dsd_in.metrics");
        let output = plan.output(&OutputId::new("dd_metrics_encode").unwrap()).unwrap();
        assert_eq!(output.accepts.kinds, KindSet::ALL);
        assert_eq!(output.accepts.timestamps, TimestampForm::Mixed);
        assert_eq!(output.delivery, DeliveryPolicy::Native);
        assert_eq!(plan.scopes.len(), 1);
        assert!(plan.node(&NodeId::reserved(LocalId::new("dsd_in").unwrap())).is_some());

        // Serialization writes the normalized form, which leaves out the default delivery policy.
        let mut normalized = value.clone();
        normalized["outputs"][0].as_object_mut().unwrap().remove("delivery");
        assert_eq!(serde_json::to_value(&plan).unwrap(), normalized);
        assert_eq!(Plan::from_value(normalized).unwrap(), plan);
        assert_eq!(Plan::from_json(&serde_json::to_string(&plan).unwrap()).unwrap(), plan);
    }

    /// Two small documents in normalized form, with their canonical forms and digests computed independently of this
    /// implementation, with Python: `json.dumps(document, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`,
    /// then `hashlib.sha256`. Both leave out members at their defaults: the plan's output has no `recipients` and no
    /// `delivery`, and the artifact's base has no `outputs`.
    #[test]
    fn hashes_are_rfc_8785_digests_of_the_normalized_form() {
        let plan_document = json!({
            "ir_version": 1,
            "nodes": [
                {"id": "src", "op": "source", "params": {"binding": "otlp"}, "provenance": {"native": "src"}},
                {"id": "e", "op": "emit", "params": {"output": "internal"}, "provenance": {"native": "e"}}
            ],
            "edges": [{"from": "src.metrics", "to": "e.in"}],
            "outputs": [
                {"id": "internal", "accepts": {"kinds": "all", "timestamps": "mixed"}, "fingerprint": ZERO_HASH}
            ]
        });
        let plan = Plan::from_value(plan_document.clone()).unwrap();
        assert_eq!(serde_json::to_value(&plan).unwrap(), plan_document);
        let expected = "sha256:2d05d24f670bfb222f93c82a9f8d6dcc7802c4d8030d100c369b63be2b3bb625";
        assert_eq!(plan.hash(ResolvedValues::Include).unwrap().to_string(), expected);
        assert_eq!(PlanHash::of_value(&plan_document).unwrap().to_string(), expected);

        // A document that writes the defaults explicitly reads as the same plan, so it has the same plan hash.
        let mut explicit = plan_document.clone();
        explicit["outputs"][0]["recipients"] = json!([]);
        explicit["outputs"][0]["delivery"] = json!("native");
        assert_eq!(
            Plan::from_value(explicit)
                .unwrap()
                .hash(ResolvedValues::Include)
                .unwrap()
                .to_string(),
            expected
        );

        let artifact_document = json!({
            "ir_version": 1,
            "scope": "post_enrich",
            "base": {"program_hash": ZERO_HASH},
            "nodes": [{"id": "metrics_policy", "op": "tee", "params": {"ports": 1}}],
            "edges": [{"from": "input.metrics", "to": "metrics_policy.in"}]
        });
        let received = ReceivedArtifact::from_value(artifact_document.clone()).unwrap();
        assert_eq!(
            received.hash.to_string(),
            "sha256:daaba618367b175c73b633082844b3b30304ff427a64d9cc64e3abeeb409846e"
        );
        assert_eq!(serde_json::to_value(&received.artifact).unwrap(), artifact_document);
        assert_eq!(
            PlanHash::of_serializable(&received.artifact).unwrap(),
            received.hash,
            "an artifact this crate writes hashes as its document"
        );

        // Approval binds to the document as received: writing a default explicitly makes a different document, with a
        // different hash, although it reads as the same artifact.
        let mut explicit = artifact_document;
        explicit["base"]["outputs"] = json!({});
        let explicit = ReceivedArtifact::from_json(&explicit.to_string()).unwrap();
        assert_eq!(
            explicit.hash.to_string(),
            "sha256:80a9db687504a7b20b9ec492c6b106d163d2fc74c83f1d061859db6a016193c1"
        );
        assert_eq!(explicit.artifact, received.artifact);
    }

    #[test]
    fn resolved_values_are_stripped_only_from_node_parameters() {
        // Scopes named `config` and `resolved` are not configuration bindings: a hash without resolved values still
        // covers every scope.
        let scoped = |port: &str| {
            let mut value = design_example();
            value["scopes"] = json!({
                "config": {"inputs": {"metrics": "dsd_in.metrics"}},
                "resolved": {"inputs": {"metrics": port}}
            });
            Plan::from_value(value).unwrap()
        };
        assert_ne!(
            scoped("dsd_in.metrics").hash(ResolvedValues::Exclude).unwrap(),
            scoped("dsd_prefix_filter_blocklist.false")
                .hash(ResolvedValues::Exclude)
                .unwrap()
        );
    }

    #[test]
    fn artifact_edges_enter_a_scopes_exits_and_never_leave_them() {
        let mut value = artifact_example();
        value["scope"] = json!("dogstatsd_pre_aggregation");
        value["edges"][1] = json!({"from": "metrics_policy.1", "to": "exit.aggregated"});
        let artifact = PolicyArtifact::from_value(value.clone()).unwrap();
        assert_eq!(
            artifact.edges[1].to,
            ArtifactPortRef::Exit(ExitName::new("aggregated").unwrap())
        );
        assert_eq!(
            serde_json::to_value(&artifact).unwrap()["edges"][1]["to"],
            json!("exit.aggregated")
        );

        value["edges"][1] = json!({"from": "exit.aggregated", "to": "drop_debug.in"});
        let error = PolicyArtifact::from_value(value.clone()).unwrap_err();
        assert!(matches!(error, IrError::EdgeFromExit { index: 1, .. }), "{}", error);
        assert!(
            error
                .to_string()
                .starts_with("Artifact edge 1 leaves `exit.aggregated`, a scope exit."),
            "{}",
            error
        );
        assert!(ReceivedArtifact::from_value(value).is_err());
    }

    #[test]
    fn artifacts_carry_no_resolved_values() {
        let mut value = artifact_example();
        value["nodes"][1]["params"]["predicate"] = json!({"name": {"matches": {
            "values": {"config": "dogstatsd.metric_filter.values", "resolved": ["smuggled"]},
            "prefix": false}}});
        let error = PolicyArtifact::from_value(value.clone()).unwrap_err();
        assert!(matches!(error, IrError::ResolvedInArtifact { .. }), "{}", error);
        assert!(error
            .to_string()
            .starts_with("Artifact node `drop_debug` carries a resolved configuration value"));
        assert!(ReceivedArtifact::from_value(value.clone()).is_err());

        // Without the resolved value, the binding is what artifacts may carry.
        value["nodes"][1]["params"]["predicate"]["name"]["matches"]["values"] =
            json!({"config": "dogstatsd.metric_filter.values"});
        assert!(PolicyArtifact::from_value(value).is_ok());

        // Outputs named `config` and `resolved` are outputs, not a binding.
        let mut value = artifact_example();
        value["base"]["outputs"] = json!({"config": ZERO_HASH, "resolved": ZERO_HASH});
        assert!(PolicyArtifact::from_value(value).is_ok());
    }

    #[test]
    fn repeated_member_names_are_rejected() {
        let plan = r#"{"ir_version": 1, "nodes": [], "nodes": [], "edges": []}"#;
        let error = Plan::from_json(plan).unwrap_err();
        assert!(matches!(error, IrError::Parse { .. }), "{}", error);
        assert!(
            error.to_string().contains("the member `nodes` appears more than once"),
            "{}",
            error
        );

        // At any depth, including inside parameters, which the IR's own types never see as objects.
        let artifact = r#"{"ir_version": 1, "scope": "post_enrich",
            "base": {"program_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"},
            "nodes": [{"id": "s", "op": "split",
                       "params": {"predicate": {"const": true}, "predicate": {"const": false}}}],
            "edges": []}"#;
        for error in [
            PolicyArtifact::from_json(artifact).unwrap_err(),
            ReceivedArtifact::from_json(artifact).unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("the member `predicate` appears more than once"),
                "{}",
                error
            );
        }
        assert!(PolicyArtifact::from_json(&artifact.replacen(r#", "predicate": {"const": false}"#, "", 1)).is_ok());
    }

    #[test]
    fn artifact_blobs_are_checked_when_read_and_inlined_by_composition() {
        let list = json!(["demo.debug.a", "demo.debug.b"]);
        let hash = PlanHash::of_value(&list).unwrap();
        let with_blobs = |blobs: Value, reference: Value| {
            let mut value = artifact_example();
            value["nodes"][1]["params"]["predicate"] = json!({"name": {"in": reference}});
            value["blobs"] = blobs;
            value
        };

        let artifact = PolicyArtifact::from_value(with_blobs(
            json!({hash.to_string(): list}),
            json!({"blob": hash.to_string()}),
        ))
        .unwrap();
        let mut params = serde_json::to_value(&artifact.nodes[1].params).unwrap();
        artifact.blobs.inline(&mut params).unwrap();
        assert_eq!(
            params,
            json!({"predicate": {"name": {"in": ["demo.debug.a", "demo.debug.b"]}}})
        );

        // (blob table, reference, expected error fragment)
        let tampered = json!({ZERO_HASH: list});
        let cases = [
            (
                json!({}),
                json!({"blob": hash.to_string()}),
                "which the blob table does not hold",
            ),
            (
                json!({hash.to_string(): list}),
                json!({"blob": hash.to_string(), "extra": 1}),
                "Invalid blob reference",
            ),
            (
                json!({hash.to_string(): list}),
                json!({"blob": "sha256:00"}),
                "Invalid blob reference",
            ),
            (tampered, json!(["demo.debug.a"]), "has content hashing to"),
        ];
        for (blobs, reference, fragment) in cases {
            let error = PolicyArtifact::from_value(with_blobs(blobs, reference)).unwrap_err();
            assert!(matches!(error, IrError::BlobTable { .. }), "{}", error);
            assert!(error.to_string().contains(fragment), "{}: {}", fragment, error);
        }
    }

    #[test]
    fn artifacts_round_trip_through_json() {
        let value = artifact_example();
        let artifact = PolicyArtifact::from_value(value.clone()).unwrap();
        assert_eq!(artifact.scope.as_str(), "post_enrich");
        assert_eq!(artifact.base.outputs.len(), 1);
        assert_eq!(
            artifact.edges[0].from,
            ArtifactPortRef::Input(InputName::new("metrics").unwrap())
        );
        assert_eq!(serde_json::to_value(&artifact).unwrap(), value);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        let cases: Vec<(&str, Mutation)> = vec![
            ("plan", |v| v["extra"] = json!(1)),
            ("node", |v| v["nodes"][0]["extra"] = json!(1)),
            ("edge", |v| v["edges"][0]["extra"] = json!(1)),
            ("output", |v| v["outputs"][0]["extra"] = json!(1)),
            ("port type", |v| v["outputs"][0]["accepts"]["extra"] = json!(1)),
            ("scope", |v| v["scopes"]["post_enrich"]["extra"] = json!(1)),
            ("artifact provenance", |v| {
                v["nodes"][2]["provenance"]["artifact"]["extra"] = json!(1)
            }),
            ("provenance", |v| v["nodes"][0]["provenance"]["copied"] = json!("x")),
        ];
        for (what, mutate) in cases {
            let mut value = design_example();
            mutate(&mut value);
            let error = Plan::from_value(value).unwrap_err().to_string();
            assert!(
                error.starts_with("The document is not a valid plan"),
                "{}: {}",
                what,
                error
            );
        }

        let artifact_cases: Vec<(&str, Mutation)> = vec![
            ("artifact", |v| v["extra"] = json!(1)),
            ("base", |v| v["base"]["extra"] = json!(1)),
            // Artifacts cannot claim provenance for their nodes: composition assigns it.
            ("node provenance", |v| {
                v["nodes"][0]["provenance"] = json!({"native": "dsd_in"})
            }),
            ("edge", |v| v["edges"][0]["extra"] = json!(1)),
        ];
        for (what, mutate) in artifact_cases {
            let mut value = artifact_example();
            mutate(&mut value);
            let error = PolicyArtifact::from_value(value).unwrap_err().to_string();
            assert!(
                error.starts_with("The document is not a valid policy artifact"),
                "{}: {}",
                what,
                error
            );
        }
    }

    #[test]
    fn malformed_identifiers_are_rejected() {
        let cases: Vec<(&str, Mutation, &str)> = vec![
            ("node ID", |v| v["nodes"][0]["id"] = json!("DSD"), "Invalid node ID"),
            (
                "operator",
                |v| v["nodes"][0]["op"] = json!("split!"),
                "Invalid operator name",
            ),
            (
                "edge",
                |v| v["edges"][0]["from"] = json!("dsd_in"),
                "Invalid port reference",
            ),
            (
                "hash",
                |v| v["outputs"][0]["fingerprint"] = json!("sha256:abc"),
                "Invalid hash",
            ),
            (
                "native ID",
                |v| v["nodes"][0]["provenance"]["native"] = json!("_x"),
                "Invalid native component ID",
            ),
            (
                "recipient",
                |v| v["outputs"][0]["recipients"][0] = json!("a b"),
                "Invalid recipient ID",
            ),
            (
                "delivery",
                |v| v["outputs"][0]["delivery"] = json!("drop_oldest"),
                "drop_oldest",
            ),
        ];
        for (what, mutate, fragment) in cases {
            let mut value = design_example();
            mutate(&mut value);
            let error = Plan::from_value(value).unwrap_err().to_string();
            assert!(error.contains(fragment), "{}: {}", what, error);
        }

        let mut value = artifact_example();
        value["nodes"][0]["id"] = json!("input");
        let error = PolicyArtifact::from_value(value).unwrap_err().to_string();
        assert!(error.contains("`input` is reserved"), "{}", error);
    }

    #[test]
    fn unsupported_versions_are_rejected_before_parsing() {
        let mut value = design_example();
        value["ir_version"] = json!(2);
        assert!(matches!(
            Plan::from_value(value).unwrap_err(),
            IrError::UnsupportedVersion { .. }
        ));
        assert!(matches!(Plan::from_json("{").unwrap_err(), IrError::Parse { .. }));
    }

    #[test]
    fn plan_hashes_ignore_provenance_and_optionally_resolved_values() {
        let plan = Plan::from_value(design_example()).unwrap();
        let hash = |plan: &Plan, resolved| plan.hash(resolved).unwrap();

        // Plan-level provenance does not change the hash.
        let mut with_provenance = plan.clone();
        with_provenance.provenance = Some(PlanProvenance {
            config_change: Some(12),
            artifacts: vec![],
        });
        assert_eq!(
            hash(&plan, ResolvedValues::Include),
            hash(&with_provenance, ResolvedValues::Include)
        );

        // A resolved value changes the inclusive hash only.
        let mut resolved = plan.clone();
        let predicate = serde_json::from_value(json!({"intake_name": {"matches": {
            "values": {"config": "dogstatsd.metric_filter.values", "resolved": ["blocked"]},
            "prefix": {"config": "dogstatsd.metric_filter.match_prefix", "resolved": false}}}}))
        .unwrap();
        resolved.nodes[1].params.insert("predicate".parse().unwrap(), predicate);
        assert_ne!(
            hash(&plan, ResolvedValues::Include),
            hash(&resolved, ResolvedValues::Include)
        );
        assert_eq!(
            hash(&plan, ResolvedValues::Exclude),
            hash(&resolved, ResolvedValues::Exclude)
        );

        // A structural change changes both.
        let mut changed = plan.clone();
        changed.edges.clear();
        assert_ne!(
            hash(&plan, ResolvedValues::Exclude),
            hash(&changed, ResolvedValues::Exclude)
        );
    }

    #[test]
    fn received_artifact_hashes_cover_content() {
        let received = ReceivedArtifact::from_value(artifact_example()).unwrap();
        let mut changed = artifact_example();
        changed["nodes"].as_array_mut().unwrap().pop();
        changed["edges"].as_array_mut().unwrap().pop();
        assert_ne!(received.hash, ReceivedArtifact::from_value(changed).unwrap().hash);
        assert_eq!(received.hash, PlanHash::of_value(&artifact_example()).unwrap());
    }

    #[test]
    fn blobs_are_content_addressed() {
        let mut blobs = Blobs::default();
        let hash = blobs.insert(json!(["a.b", "c.d"])).unwrap();
        assert_eq!(hash, PlanHash::of_value(&json!(["a.b", "c.d"])).unwrap());
        assert_eq!(blobs.get(&hash), Some(&json!(["a.b", "c.d"])));
        assert!(blobs.verify().is_ok());

        // A blob stored under the wrong hash fails verification.
        let tampered: Blobs = serde_json::from_str(&format!(r#"{{"{}": ["x"]}}"#, ZERO_HASH)).unwrap();
        let error = tampered.verify().unwrap_err();
        assert!(matches!(error, BlobError::Mismatch { .. }), "{}", error);

        // Blobs that cannot be canonicalized cannot be added.
        assert!(blobs.insert(json!(0.5)).is_err());
        assert_eq!(blobs.len(), 1);
    }
}
