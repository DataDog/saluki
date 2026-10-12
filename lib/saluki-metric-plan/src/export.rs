//! The export bundle: the default program's policy scope as an artifact, with the catalog it is checked against.
//!
//! Authors edit the default program rather than start from nothing: the host exports the scope an artifact may
//! replace as a [`PolicyArtifact`], which an author edits and the channel delivers back. The bundle carries what is
//! needed to check and preview an artifact away from the host:
//!
//! - `export_version`: [`EXPORT_VERSION`];
//! - `scope`: the scope exported;
//! - `artifact`: the scope as a policy artifact, with its configuration bindings unresolved (an export never carries
//!   resolved values) and its [base](ArtifactBase) filled in: the base program hash and the fingerprint of every output
//!   the scope emits to. Its nodes and edges are in the order the plan lists them, which lowering writes in the surface
//!   syntax's canonical order, so printing an export and parsing the text gives back the same document and hash;
//! - `ir_versions`: the IR version written, and the range of IR versions accepted;
//! - `catalog`: the host's [`MemoryCatalog`] document: scope inputs with their port types, outputs with what they
//!   accept, their fingerprints, their recipients, and, for a native output that had one, its version 1 fingerprint,
//!   and bindings with their types, liveness, and current values. Outputs a resources artifact declares are listed
//!   like native ones; delivery hosts and their settings are not;
//! - `labels`: display labels of outputs, by output ID, such as those a resources artifact gives its outputs, left
//!   out when there are none.
//!
//! Readers ignore members they do not know.
//!
//! # The scope as an artifact
//!
//! A scope's **region** is every node reachable from the output ports its inputs name, without entering a port one of
//! its exits names. The exported artifact has:
//!
//! - one node per region node, in plan order, whose local ID is the node's local ID (a reserved ID, for the default
//!   program), with the node's operator and its parameters without resolved values, and without provenance;
//! - every edge that enters a region node or an exit's port, in plan order, with an edge from a scope input's port
//!   written as leaving `input.<name>`, and an edge into an exit's port as entering `exit.<name>`;
//! - a base whose `outputs` hold the fingerprint the plan declares for every output a region `emit` delivers to, and
//!   whose `program_hash` is the hash of the artifact document itself without its `base.program_hash` member (see
//!   [`base`]).
//!
//! Composing an unmodified export back into the plan it came from gives that plan again (see
//! [`compose`](crate::compose::compose)).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{strip_resolved_values, CanonicalError, PlanHash},
    ir::{
        ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, Blobs, ExitName, InputName, NodeId,
        OutputId, Params, Plan, PolicyArtifact, PortRef, ScopeName, ACCEPTED_IR_VERSIONS, IR_VERSION,
    },
    operators::names,
    validate::MemoryCatalog,
};

/// The version of the export bundle's format.
pub const EXPORT_VERSION: u32 = 1;

/// The IR versions a host writes and accepts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IrVersions {
    /// The IR version the host writes.
    pub current: u32,

    /// The lowest and highest IR versions the host accepts, written as a two-element list.
    pub accepted: (u32, u32),
}

impl IrVersions {
    /// Returns the IR versions of this version of the plan crate.
    pub const fn supported() -> Self {
        Self {
            current: IR_VERSION,
            accepted: (*ACCEPTED_IR_VERSIONS.start(), *ACCEPTED_IR_VERSIONS.end()),
        }
    }
}

/// An export bundle: one scope of the default program as an artifact, with the catalog it is checked against.
///
/// Reading a bundle ignores the members it does not know, in the bundle and in its catalog's outputs and bindings, so
/// that the bundle can gain members without breaking its readers; the catalog document itself, and the artifact, are
/// read strictly elsewhere. An output's display label is read from `labels`, or from a `label` member of the
/// catalog's output when `labels` has none for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "BundleDocument")]
pub struct ExportBundle {
    /// The bundle's format version, [`EXPORT_VERSION`].
    pub export_version: u32,

    /// The scope exported.
    pub scope: ScopeName,

    /// The scope as a policy artifact, with its bindings unresolved and its base filled in.
    pub artifact: PolicyArtifact,

    /// The IR versions the host writes and accepts.
    pub ir_versions: IrVersions,

    /// The host's catalog document.
    pub catalog: MemoryCatalog,

    /// Display labels of outputs, by output ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<OutputId, String>,
}

/// An export bundle as read, before its catalog's unknown members are left out.
#[derive(Deserialize)]
struct BundleDocument {
    export_version: u32,
    scope: ScopeName,
    artifact: Value,
    ir_versions: IrVersions,
    catalog: Value,
    #[serde(default)]
    labels: BTreeMap<OutputId, String>,
}

/// The members of a catalog document that [`MemoryCatalog`] reads.
const CATALOG_MEMBERS: &[&str] = &["outputs", "bindings", "scopes", "state_budget"];

/// The members of a catalog output that [`MemoryCatalog`] reads.
const CATALOG_OUTPUT_MEMBERS: &[&str] = &["accepts", "fingerprint", "recipients", "legacy_fingerprint"];

/// The members of a catalog binding that [`MemoryCatalog`] reads.
const CATALOG_BINDING_MEMBERS: &[&str] = &["type", "liveness", "value"];

/// The member of a catalog output that carries its display label, in bundles that place labels there.
const LABEL_MEMBER: &str = "label";

impl TryFrom<BundleDocument> for ExportBundle {
    type Error = String;

    fn try_from(document: BundleDocument) -> Result<Self, Self::Error> {
        let BundleDocument {
            export_version,
            scope,
            artifact,
            ir_versions,
            catalog,
            mut labels,
        } = document;
        let artifact = PolicyArtifact::from_value(artifact).map_err(|error| error.to_string())?;
        let catalog = known_catalog_members(catalog, &mut labels);
        let catalog = serde_json::from_value(catalog).map_err(|error| format!("invalid catalog: {}", error))?;
        Ok(Self {
            export_version,
            scope,
            artifact,
            ir_versions,
            catalog,
            labels,
        })
    }
}

/// Returns a catalog document with only the members [`MemoryCatalog`] reads, adding the label of every output that
/// carries one to `labels`, unless `labels` already has one for it.
///
/// A member of a known kind that is not an object is kept as it is, for the catalog's reader to refuse.
fn known_catalog_members(catalog: Value, labels: &mut BTreeMap<OutputId, String>) -> Value {
    fn keep(value: Value, members: &[&str]) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .filter(|(member, _)| members.contains(&member.as_str()))
                    .collect(),
            ),
            other => other,
        }
    }

    let mut catalog = match keep(catalog, CATALOG_MEMBERS) {
        Value::Object(catalog) => catalog,
        other => return other,
    };
    if let Some(Value::Object(outputs)) = catalog.get_mut("outputs") {
        for (id, output) in outputs.iter_mut() {
            if let (Ok(id), Some(label)) = (id.parse::<OutputId>(), output.get(LABEL_MEMBER).and_then(Value::as_str)) {
                labels.entry(id).or_insert_with(|| label.to_string());
            }
            *output = keep(output.take(), CATALOG_OUTPUT_MEMBERS);
        }
    }
    if let Some(Value::Object(bindings)) = catalog.get_mut("bindings") {
        for binding in bindings.values_mut() {
            *binding = keep(binding.take(), CATALOG_BINDING_MEMBERS);
        }
    }
    Value::Object(catalog)
}

/// Exports one scope of a lowered plan as an artifact, with the host's catalog and output labels.
///
/// `plan` is the default program as lowered, with its bindings resolved; the export strips the resolved values and the
/// nodes' provenance, and writes reserved node IDs as the artifact's local IDs, so that composing the unmodified
/// export back reproduces the default program. The catalog and labels are carried as given.
///
/// # Errors
///
/// Returns an error if the plan does not declare the scope, the scope cannot be written as an artifact (see
/// [`ExportError::NotExportable`]), or the scope has no canonical form.
pub fn export(
    plan: &Plan, scope: &ScopeName, catalog: &MemoryCatalog, labels: &BTreeMap<OutputId, String>,
) -> Result<ExportBundle, ExportError> {
    let region = ScopeRegion::of(plan, scope)?;
    let base = region.base(plan, scope)?;
    let ScopeRegion { nodes, edges, .. } = region;
    Ok(ExportBundle {
        export_version: EXPORT_VERSION,
        scope: scope.clone(),
        artifact: PolicyArtifact {
            ir_version: plan.ir_version,
            scope: scope.clone(),
            base,
            nodes,
            edges,
            blobs: Blobs::default(),
        },
        ir_versions: IrVersions::supported(),
        catalog: catalog.clone(),
        labels: labels.clone(),
    })
}

/// Returns the base an artifact derived from one scope of a plan records: the base program hash, and the fingerprint
/// of every output the scope emits to.
///
/// The **program hash** is the hash of the scope's export: the RFC 8785 digest of the exported artifact document
/// without its `base.program_hash` member, that is, of its `ir_version`, `scope`, `nodes`, `edges`, and the output
/// fingerprints of its base. It covers the scope's structure, operators, literal parameters, binding names, and the
/// outputs it delivers to, but neither resolved values (an export has none) nor node provenance (an export has none),
/// nor anything outside the scope. So a change of native configuration values never changes it, while a structural
/// change inside the scope, or another binding of an output the scope emits to, always does, and a change outside the
/// scope never does: the base program hash changes exactly when the export's artifact does, so a client that keeps an
/// export knows to fetch it again when the hash it last saw changes. An author's tool can tell whether an artifact is
/// an unmodified export by comparing the hash of its document without `base.program_hash` with its
/// `base.program_hash`.
///
/// [`export`] fills the exported artifact's base with this, and composition compares an artifact's base with this
/// for the current default program: a different program hash is a stale base, and a different or missing output
/// fingerprint of an output the artifact emits to is a removed output. The host's status reports the program hash as
/// `default_program_hash`.
///
/// # Errors
///
/// Returns an error if the plan does not declare the scope, the scope cannot be written as an artifact, or the scope
/// has no canonical form.
pub fn base(plan: &Plan, scope: &ScopeName) -> Result<ArtifactBase, ExportError> {
    ScopeRegion::of(plan, scope)?.base(plan, scope)
}

/// A scope that cannot be exported.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ExportError {
    /// The plan does not declare the scope.
    #[snafu(display("The default program declares no scope `{}`.", scope))]
    UnknownScope {
        /// The scope.
        scope: ScopeName,
    },

    /// The scope's region cannot be written as an artifact.
    #[snafu(display("Scope `{}` cannot be exported as an artifact: {}", scope, message))]
    NotExportable {
        /// The scope.
        scope: ScopeName,

        /// Why.
        message: String,
    },

    /// The scope has no canonical form, so it has no hash.
    #[snafu(display("The default program has no canonical form: {}", source))]
    Unhashable {
        /// The underlying error.
        source: CanonicalError,
    },
}

/// One scope of a plan, as the parts of the artifact that replaces it.
///
/// This is what [`export`] writes, what [`base`] hashes, and what composition replaces: the region's nodes and the
/// edges that enter them.
pub(crate) struct ScopeRegion {
    /// The region's nodes as artifact nodes, in plan order: local IDs, no provenance, no resolved values.
    pub(crate) nodes: Vec<ArtifactNode>,

    /// Every edge that enters a region node or an exit's port, in plan order, with edges from a scope input leaving
    /// `input.<name>`, and edges into an exit's port entering `exit.<name>`.
    pub(crate) edges: Vec<ArtifactEdge>,

    /// The fingerprint of every output a region `emit` delivers to, as the plan declares it.
    pub(crate) outputs: BTreeMap<OutputId, PlanHash>,

    /// The IDs of the region's nodes.
    pub(crate) members: BTreeSet<NodeId>,
}

impl ScopeRegion {
    /// Finds the region of `scope` in `plan`, and writes it as an artifact's nodes and edges.
    ///
    /// The region is every node reachable from the ports the scope's inputs name, without entering a port one of its
    /// exits names: an edge into an exit's port is the region's edge to `exit.<name>`.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan does not declare the scope, or the region cannot be written as an artifact: two
    /// inputs, or two exits, name one port, an edge enters a node the plan does not have, a region node is fed from
    /// outside the region other than through an input, two region nodes have the same local ID or one's is `input` or
    /// `exit`, or an `emit` delivers to an output the plan does not declare.
    pub(crate) fn of(plan: &Plan, scope: &ScopeName) -> Result<Self, ExportError> {
        let declaration = plan
            .scopes
            .get(scope)
            .ok_or_else(|| ExportError::UnknownScope { scope: scope.clone() })?;
        let not_exportable = |message: String| ExportError::NotExportable {
            scope: scope.clone(),
            message,
        };

        let mut inputs: BTreeMap<&PortRef, &InputName> = BTreeMap::new();
        for (name, port) in &declaration.inputs {
            if let Some(other) = inputs.insert(port, name) {
                return Err(not_exportable(format!(
                    "its inputs `{}` and `{}` both name `{}`; an artifact edge leaves one input",
                    other, name, port
                )));
            }
        }
        let mut exits: BTreeMap<&PortRef, &ExitName> = BTreeMap::new();
        for (name, port) in &declaration.exits {
            if let Some(other) = exits.insert(port, name) {
                return Err(not_exportable(format!(
                    "its exits `{}` and `{}` both name `{}`; an artifact edge enters one exit",
                    other, name, port
                )));
            }
        }

        // The region: every node reachable from the inputs' ports, without entering an exit's port.
        let mut members = BTreeSet::new();
        let mut frontier = plan
            .edges
            .iter()
            .filter(|edge| inputs.contains_key(&edge.from) && !exits.contains_key(&edge.to))
            .map(|edge| &edge.to.node)
            .collect::<Vec<_>>();
        while let Some(node) = frontier.pop() {
            if members.insert(node.clone()) {
                frontier.extend(
                    plan.edges
                        .iter()
                        .filter(|edge| &edge.from.node == node && !exits.contains_key(&edge.to))
                        .map(|edge| &edge.to.node),
                );
            }
        }

        let mut nodes = Vec::new();
        let mut locals = BTreeSet::new();
        let mut found = BTreeSet::new();
        for node in plan.nodes.iter().filter(|node| members.contains(&node.id)) {
            let local = ArtifactNodeId::new(node.id.local().as_str()).map_err(|error| {
                not_exportable(format!(
                    "node `{}` has a local ID that artifact nodes cannot have: {}",
                    node.id, error
                ))
            })?;
            if !locals.insert(local.clone()) {
                return Err(not_exportable(format!(
                    "more than one of its nodes has the local ID `{}`, which an artifact names one node by",
                    local
                )));
            }
            found.insert(&node.id);
            nodes.push(ArtifactNode {
                id: local,
                op: node.op.clone(),
                params: unresolved(&node.params),
            });
        }
        if let Some(missing) = members.iter().find(|member| !found.contains(member)) {
            return Err(not_exportable(format!(
                "an edge enters `{}`, which is not a node of the plan",
                missing
            )));
        }

        let local = |id: &NodeId| ArtifactNodeId::new(id.local().as_str()).ok();
        let mut edges = Vec::new();
        for edge in &plan.edges {
            let source = if let Some(input) = inputs.get(&edge.from) {
                Some(ArtifactPortRef::Input((*input).clone()))
            } else if members.contains(&edge.from.node) {
                Some(ArtifactPortRef::Node {
                    node: local(&edge.from.node).expect("region nodes have artifact node IDs"),
                    port: edge.from.port.clone(),
                })
            } else {
                None
            };
            let to = if let Some(exit) = exits.get(&edge.to) {
                // An edge into an exit's port from outside the region is not the region's.
                if source.is_none() {
                    continue;
                }
                ArtifactPortRef::Exit((*exit).clone())
            } else if members.contains(&edge.to.node) {
                ArtifactPortRef::Node {
                    node: local(&edge.to.node).expect("region nodes have artifact node IDs"),
                    port: edge.to.port.clone(),
                }
            } else {
                // The region holds every node a region node's edge enters, but through an exit: this edge is not the
                // region's.
                continue;
            };
            let Some(from) = source else {
                return Err(not_exportable(format!(
                    "`{}` is inside the scope, but is fed from `{}`, outside it; a scope is fed only through its \
                     inputs",
                    edge.to, edge.from
                )));
            };
            edges.push(ArtifactEdge { from, to });
        }

        let mut outputs = BTreeMap::new();
        for node in nodes.iter().filter(|node| node.op.as_str() == names::EMIT) {
            let output = emitted_output(&node.params)
                .ok_or_else(|| not_exportable(format!("`emit` node `{}` names no valid output", node.id)))?;
            let declared = plan.output(&output).ok_or_else(|| {
                not_exportable(format!(
                    "`emit` node `{}` delivers to `{}`, which the plan does not declare",
                    node.id, output
                ))
            })?;
            outputs.insert(output, declared.fingerprint);
        }

        Ok(Self {
            nodes,
            edges,
            outputs,
            members,
        })
    }

    /// Returns the base of an artifact of this region: the hash of the artifact document without its base program
    /// hash, and the output fingerprints.
    ///
    /// # Errors
    ///
    /// Returns an error if the region has no canonical form.
    pub(crate) fn base(&self, plan: &Plan, scope: &ScopeName) -> Result<ArtifactBase, ExportError> {
        let artifact = PolicyArtifact {
            ir_version: plan.ir_version,
            scope: scope.clone(),
            base: ArtifactBase {
                program_hash: PlanHash::from_digest([0; 32]),
                outputs: self.outputs.clone(),
            },
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            blobs: Blobs::default(),
        };
        let mut document = serde_json::to_value(&artifact).map_err(|source| ExportError::Unhashable {
            source: CanonicalError::ToJson { source },
        })?;
        if let Some(Value::Object(base)) = document.get_mut("base") {
            base.remove("program_hash");
        }
        let program_hash = PlanHash::of_value(&document).map_err(|source| ExportError::Unhashable { source })?;
        Ok(ArtifactBase {
            program_hash,
            outputs: self.outputs.clone(),
        })
    }
}

/// Returns parameters with the resolved values of their configuration bindings removed.
pub(crate) fn unresolved(params: &Params) -> Params {
    params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            strip_resolved_values(&mut value);
            (name.clone(), value)
        })
        .collect()
}

/// Returns the output an `emit` node's parameters name, if they name a valid one.
pub(crate) fn emitted_output(params: &Params) -> Option<OutputId> {
    params.get("output")?.as_str()?.parse().ok()
}

#[cfg(test)]
mod tests;
