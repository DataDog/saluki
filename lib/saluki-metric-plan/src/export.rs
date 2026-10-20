//! The export bundle: the default program as an artifact, with the catalog it is checked against.
//!
//! Authors edit the default program rather than start from nothing: the host exports it as a [`PolicyArtifact`], a
//! whole program, which an author edits and the channel delivers back. The bundle carries what is needed to check and
//! preview an artifact away from the host:
//!
//! - `export_version`: [`EXPORT_VERSION`];
//! - `artifact`: the default program as a policy artifact, every node but its sources, with its configuration
//!   bindings unresolved (an export never carries resolved values). Its nodes and edges are in the order the plan
//!   lists them; an edge from a source leaves `input.<binding>`;
//! - `ir_versions`: the IR version written, and the range of IR versions accepted;
//! - `catalog`: the host's [`MemoryCatalog`] document: outputs with what they accept and their recipients, and
//!   bindings with their types, liveness, and current values. Delivery hosts and their settings are not listed;
//! - `labels`: display labels of outputs, by output ID, left out when there are none;
//! - `annotations`: notes for tools, left out when there are none ([`Annotations`]). They are not part of the artifact;
//! - `form`: how the artifact writes the default program ([`ExportForm`]): `bound`, every node as the default program
//!   has it, or `detached`, as a published program starts from it. A reader of a bundle without `form` reads `bound`;
//! - `omitted`: the nodes a detached export leaves out because they do nothing as configured, by local ID, each with
//!   why (`identity`, `constant_split`, or `unreachable`), left out when there are none.
//!
//! Readers ignore members they do not know. The hash of the exported artifact is its [program hash](program_hash),
//! which the host's status reports as `default_program_hash`.
//!
//! # The detached form
//!
//! A published program owns its policy: it writes what to do with metrics as literals, and binds only facts and
//! signals. The detached form is the export a published program starts from ([`export_as`] with
//! [`ExportForm::Detached`]). It differs from the bound form in two ways:
//!
//! - It is only what happens: a node that does nothing as configured is left out, with what only it reached, and
//!   listed in `omitted` with why ([`eliminate`]), and the artifact is written in [normal form](crate::normalize)
//!   again. A node bound to a fact or signal is never left out for its value.
//! - Every `emit` to an output whose recipients the catalog's recipient directory names exactly is written as an `emit`
//!   to those names, such as `emit to [primary]`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{strip_resolved_values, CanonicalError, PlanHash},
    compose::sources,
    ir::{
        is_identifier, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, ArtifactRevision, BindingName,
        Blobs, InputName, NodeId, OutputId, Params, Plan, PolicyArtifact, PortRef, ACCEPTED_IR_VERSIONS, IR_VERSION,
    },
    normalize::{normalize_artifact, normalize_params},
    operators::{names, OperatorRegistry, EMIT_TO},
    validate::{Catalog, MemoryCatalog},
};

mod eliminate;
pub use self::eliminate::{eliminate, Eliminated, Omission};

/// The version of the export bundle's format.
pub const EXPORT_VERSION: u32 = 2;

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

/// An export bundle: the default program as an artifact, with the catalog it is checked against.
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

    /// The default program as a policy artifact, with its bindings unresolved.
    pub artifact: PolicyArtifact,

    /// The IR versions the host writes and accepts.
    pub ir_versions: IrVersions,

    /// The host's catalog document.
    pub catalog: MemoryCatalog,

    /// Display labels of outputs, by output ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<OutputId, String>,

    /// Notes for tools about the exported program's nodes and the catalog's bindings.
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    pub annotations: Annotations,

    /// How the artifact writes the default program: every node as the default program has it, or as a published program
    /// starts from it.
    #[serde(default)]
    pub form: ExportForm,

    /// The nodes of the default program the artifact leaves out because they do nothing as configured, by local ID, with
    /// why ([`eliminate`]): only a detached export leaves any out. A host names them for a reader in
    /// [`Annotations::omitted`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted: BTreeMap<ArtifactNodeId, Omission>,
}

/// How an export writes the default program (see [the detached form](self#the-detached-form)).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportForm {
    /// Every node as the default program has it, emits to outputs: the form for tools that study lowering, whose
    /// unmodified artifact composes to the default program itself.
    #[default]
    Bound,

    /// Without the nodes that do nothing as configured, and with emits addressed to recipients: the form a published
    /// program starts from.
    Detached,

    /// A form a later host writes that this reader does not know. A host never exports in it.
    #[serde(other)]
    Unknown,
}

/// Notes for tools about an exported program: what each of its nodes and each binding is, and where it comes from, in
/// one line each, such as `Metric filterlist, from metric_filterlist`.
///
/// The host writes them, since it knows what its default program stands for; an editor prints a node's note as a
/// comment above the node. They are not part of the artifact, so they change neither its hash nor its base. Readers
/// ignore members they do not know.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Annotations {
    /// A note for each of the exported artifact's nodes that has one, by its local ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nodes: BTreeMap<ArtifactNodeId, String>,

    /// A note for each of the catalog's bindings that has one, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<BindingName, String>,

    /// One line saying what the exported artifact leaves out because it does nothing as configured
    /// ([`ExportBundle::omitted`]), in words a reader knows, such as `Not shown, because they do nothing as configured:
    /// metric filterlist (empty), mapper (no profiles)`, which an editor prints near the top of the program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<String>,
}

impl Annotations {
    /// Returns `true` if there are no notes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.bindings.is_empty() && self.omitted.is_none()
    }
}

/// An export bundle as read, before its catalog's unknown members are left out.
#[derive(Deserialize)]
struct BundleDocument {
    export_version: u32,
    artifact: Value,
    ir_versions: IrVersions,
    catalog: Value,
    #[serde(default)]
    labels: BTreeMap<OutputId, String>,
    #[serde(default)]
    annotations: Annotations,
    #[serde(default)]
    form: ExportForm,
    #[serde(default)]
    omitted: BTreeMap<ArtifactNodeId, Omission>,
}

/// The members of a catalog document that [`MemoryCatalog`] reads.
const CATALOG_MEMBERS: &[&str] = &[
    "outputs",
    "bindings",
    "state_budget",
    "single_aggregation",
    "recipients",
];

/// The members of a catalog output that [`MemoryCatalog`] reads.
const CATALOG_OUTPUT_MEMBERS: &[&str] = &["accepts", "recipients"];

/// The members of an entry of a catalog's recipient directory that [`MemoryCatalog`] reads.
const CATALOG_RECIPIENT_MEMBERS: &[&str] = &["recipients", "host", "endpoint"];

/// The members of a catalog binding that [`MemoryCatalog`] reads.
const CATALOG_BINDING_MEMBERS: &[&str] = &["type", "liveness", "class", "value"];

/// The member of a catalog output that carries its display label, in bundles that place labels there.
const LABEL_MEMBER: &str = "label";

impl TryFrom<BundleDocument> for ExportBundle {
    type Error = String;

    fn try_from(document: BundleDocument) -> Result<Self, Self::Error> {
        let BundleDocument {
            export_version,
            artifact,
            ir_versions,
            catalog,
            mut labels,
            annotations,
            form,
            omitted,
        } = document;
        let artifact = PolicyArtifact::from_value(artifact).map_err(|error| error.to_string())?;
        let catalog = known_catalog_members(catalog, &mut labels);
        let catalog = serde_json::from_value(catalog).map_err(|error| format!("invalid catalog: {}", error))?;
        Ok(Self {
            export_version,
            artifact,
            ir_versions,
            catalog,
            labels,
            annotations,
            form,
            omitted,
        })
    }
}

/// Returns a catalog document with only the members [`MemoryCatalog`] reads, adding the label of every output that
/// carries one to `labels`, unless `labels` already has one for it. A directory entry from a source this version does
/// not know is left out.
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
    if let Some(Value::Object(recipients)) = catalog.get_mut("recipients") {
        for entry in recipients.values_mut() {
            *entry = keep(entry.take(), CATALOG_RECIPIENT_MEMBERS);
        }
    }
    Value::Object(catalog)
}

/// Exports a lowered plan, the default program, as an artifact, with the host's catalog and output labels.
///
/// `plan` is the default program as lowered, with its bindings resolved; the export strips the resolved values, and
/// writes reserved node IDs as the artifact's local IDs, so that composing the unmodified
/// export back reproduces the default program. The catalog and labels are carried as given; the bundle has no
/// annotations, which the host adds if it has any ([`ExportBundle::annotations`]).
///
/// # Errors
///
/// Returns an error if the plan cannot be written as an artifact (see [`ExportError::NotExportable`]).
pub fn export(
    plan: &Plan, catalog: &MemoryCatalog, labels: &BTreeMap<OutputId, String>,
) -> Result<ExportBundle, ExportError> {
    // The bound form leaves nothing out, so no operator's identity is asked for.
    export_as(plan, catalog, labels, ExportForm::Bound, &OperatorRegistry::native())
}

/// Exports a lowered plan as an artifact in the form `form`, with the host's catalog and output labels.
///
/// This is [`export`] for [`ExportForm::Bound`]. For [`ExportForm::Detached`], the nodes that do nothing as configured
/// are left out and listed in [`ExportBundle::omitted`], emits are addressed to recipients where the catalog names them
/// exactly, and the artifact is written in [normal form](crate::normalize) (see [the detached
/// form](self#the-detached-form)). The bound form is the plan as the plan writes it, which is in normal form for a
/// lowered default program.
/// `registry` is the host's operator registry, which says which nodes do nothing.
///
/// # Errors
///
/// As for [`export`].
pub fn export_as(
    plan: &Plan, catalog: &MemoryCatalog, labels: &BTreeMap<OutputId, String>, form: ExportForm,
    registry: &OperatorRegistry,
) -> Result<ExportBundle, ExportError> {
    if form == ExportForm::Unknown {
        return Err(ExportError::UnknownForm);
    }
    let region = ScopeRegion::of(plan)?;
    // The detached form is only what happens: what does nothing as configured is left out.
    let (eliminated, omitted) = match form {
        ExportForm::Detached => {
            let eliminated = eliminate(plan, registry);
            let omitted = eliminated
                .omitted
                .iter()
                .filter(|(node, _)| region.members.contains(node))
                .filter_map(|(node, why)| Some((ArtifactNodeId::new(node.local().as_str()).ok()?, *why)))
                .collect();
            (Some(eliminated.plan), omitted)
        }
        ExportForm::Bound | ExportForm::Unknown => (None, BTreeMap::new()),
    };
    let ScopeRegion { mut nodes, edges, .. } = match &eliminated {
        Some(eliminated) => ScopeRegion::of(eliminated)?,
        None => region,
    };
    if let Some(plan) = &eliminated {
        for node in &mut nodes {
            let id = NodeId::reserved(node.id.to_local_id());
            let Some(lowered) = plan.node(&id) else { continue };
            node.params = normalize_params(&node.op, &unresolved(&lowered.params));
            if let Some(to) = recipients_of_output(plan, catalog, &node.params) {
                node.params = to;
            }
        }
    }
    let mut artifact = PolicyArtifact {
        ir_version: plan.ir_version,
        nodes,
        edges,
        blobs: Blobs::default(),
    };
    // Leaving nodes out can leave a filter that keeps what leaves its `false` port, which normal form turns. The bound
    // form is written as the plan writes it.
    if eliminated.is_some() {
        artifact = normalize_artifact(&artifact);
    }
    Ok(ExportBundle {
        export_version: EXPORT_VERSION,
        artifact,
        ir_versions: IrVersions::supported(),
        catalog: catalog.clone(),
        labels: labels.clone(),
        annotations: Annotations::default(),
        form,
        omitted,
    })
}

/// Returns the parameters of an `emit` to the output `params` names written as an `emit` to the names of its
/// recipients, if the catalog's recipient directory names them exactly: the recipient names whose recipients are all
/// the output's, together all of them, for which the catalog's output with exactly those recipients is this output, so
/// that composing the `emit` gives it back ([`Catalog::output_for`]). Returns `None` for an `emit` to an output without
/// recipients, such as an internal tap, or whose recipients no set of names stands for exactly.
///
/// The names that are identifiers, such as `primary`, come first, then the others, such as endpoint URLs, each in name
/// order.
fn recipients_of_output(plan: &Plan, catalog: &MemoryCatalog, params: &Params) -> Option<Params> {
    let output = emitted_output(params)?;
    let declared = plan.output(&output)?;
    if declared.recipients.is_empty() || params.iter().count() != 1 {
        return None;
    }
    let recipients = declared.recipients.iter().collect::<BTreeSet<_>>();
    let mut names = Vec::new();
    let mut covered = BTreeSet::new();
    for (name, entry) in catalog.recipients() {
        if !entry.recipients.is_empty() && entry.recipients.iter().all(|id| recipients.contains(id)) {
            names.push(name.clone());
            covered.extend(entry.recipients.iter());
        }
    }
    if covered != recipients || catalog.output_for(&declared.recipients) != Some(output) {
        return None;
    }
    names.sort_by_key(|name| (!is_identifier(name.as_str()), name.clone()));
    let names = names
        .into_iter()
        .map(|name| Value::String(name.to_string()))
        .collect::<Vec<_>>();
    Some(std::iter::once((EMIT_TO.parse().expect("`to` is a parameter name"), Value::Array(names))).collect())
}

/// Returns the nodes of the default program `default` among `nodes`, in plan order, that the plan `running` does not
/// run as `default` writes them: absent from `running`, or with another operator or other parameters. Both plans are
/// core plans ([`elaborate`](crate::elaborate)), so a parameter left out and its default, or a binding and its value,
/// are the same.
///
/// So for a published program, which owns its policy, and the nodes whose parameters native lowering writes from
/// policy settings, it lists the nodes whose native settings changed since the program was published, or that the
/// program changes. A node of `default` that does nothing as configured ([`eliminate`]), such as a filterlist with
/// nothing in it, is not listed when `running` leaves it out, as the detached export does, or has it doing nothing too.
///
/// `registry` is the host's operator registry, which says which nodes do nothing.
pub fn node_differences(
    default: &Plan, running: &Plan, nodes: &BTreeSet<NodeId>, registry: &OperatorRegistry,
) -> Vec<NodeId> {
    let idle = |plan: &Plan| {
        eliminate(plan, registry)
            .omitted
            .into_iter()
            .map(|(node, _)| node)
            .collect::<BTreeSet<_>>()
    };
    let (idle_default, idle_running) = (idle(default), idle(running));
    default
        .nodes
        .iter()
        .filter(|node| nodes.contains(&node.id))
        .filter(|node| match running.node(&node.id) {
            // A node that does nothing, which the running plan leaves out or has doing nothing too, differs in nothing.
            other if idle_default.contains(&node.id) && (other.is_none() || idle_running.contains(&node.id)) => false,
            None => true,
            Some(other) => other.op != node.op || other.params != node.params,
        })
        .map(|node| node.id.clone())
        .collect()
}

/// Returns the program hash of an artifact: the RFC 8785 digest of its normalized form, which is what an unmodified
/// export hashes to when it is received ([`ReceivedArtifact`](crate::ir::ReceivedArtifact)).
///
/// The host's status reports the program hash of the default program's export as `default_program_hash`, so a client
/// that keeps an export knows to fetch it again when the hash it last saw changes.
///
/// # Errors
///
/// Returns an error if the artifact has no canonical form.
pub fn program_hash(artifact: &PolicyArtifact) -> Result<PlanHash, CanonicalError> {
    PlanHash::of_serializable(artifact)
}

/// A default program that cannot be exported.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ExportError {
    /// The default program cannot be written as an artifact.
    #[snafu(display("The default program cannot be exported as an artifact: {}", message))]
    NotExportable {
        /// Why.
        message: String,
    },

    /// The default program has no canonical form, so it has no hash.
    #[snafu(display("The default program has no canonical form: {}", source))]
    Unhashable {
        /// The underlying error.
        source: CanonicalError,
    },

    /// The form is one this version does not know ([`ExportForm::Unknown`]).
    #[snafu(display("The export form is not one this host writes: it writes `bound` and `detached`."))]
    UnknownForm,
}

/// Returns the program a plan composed from an artifact runs: the artifact's revision, and every node of the plan but
/// its sources. A default program has none.
///
/// What a plan runs is a property of the plan, not of its nodes: every node of a composed plan but its sources is the
/// artifact's, whether the author changed it or kept it as the default program has it. This is what the artifact limits
/// and placement's attributions apply to.
pub fn program_nodes(plan: &Plan) -> Option<(ArtifactRevision, BTreeSet<NodeId>)> {
    let artifact = plan.provenance.as_ref()?.artifact.clone()?;
    Some((artifact, non_source_nodes(plan)))
}

/// Returns every node of `plan` but its `source` nodes.
pub(crate) fn non_source_nodes(plan: &Plan) -> BTreeSet<NodeId> {
    plan.nodes
        .iter()
        .filter(|node| node.op.as_str() != names::SOURCE)
        .map(|node| node.id.clone())
        .collect()
}

/// A default program, as the parts of the artifact that exports it.
///
/// This is what [`export`] writes: every node but the sources, and the edges that enter them.
pub(crate) struct ScopeRegion {
    /// The program's nodes as artifact nodes, in plan order: local IDs, no resolved values.
    pub(crate) nodes: Vec<ArtifactNode>,

    /// Every edge that enters a program node, in plan order, with edges from a source leaving `input.<binding>`.
    pub(crate) edges: Vec<ArtifactEdge>,

    /// The IDs of the program's nodes.
    pub(crate) members: BTreeSet<NodeId>,
}

impl ScopeRegion {
    /// Writes the default program `plan` as an artifact's nodes and edges.
    ///
    /// # Errors
    ///
    /// Returns an error if the program cannot be written as an artifact: two of its nodes have the same local ID or
    /// one's is `input`, or a program node is fed from a node that is neither a source nor the program's.
    pub(crate) fn of(plan: &Plan) -> Result<Self, ExportError> {
        let not_exportable = |message: String| ExportError::NotExportable { message };
        let inputs: BTreeMap<PortRef, InputName> = sources(plan).into_iter().map(|(name, port)| (port, name)).collect();
        let members = non_source_nodes(plan);

        let mut nodes = Vec::new();
        let mut locals = BTreeSet::new();
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
            nodes.push(ArtifactNode {
                id: local,
                op: node.op.clone(),
                params: unresolved(&node.params),
            });
        }

        let local = |id: &NodeId| ArtifactNodeId::new(id.local().as_str()).ok();
        let mut edges = Vec::new();
        for edge in plan.edges.iter().filter(|edge| members.contains(&edge.to.node)) {
            let from = if let Some(input) = inputs.get(&edge.from) {
                ArtifactPortRef::Input(input.clone())
            } else if members.contains(&edge.from.node) {
                ArtifactPortRef::Node {
                    node: local(&edge.from.node).expect("program nodes have artifact node IDs"),
                    port: edge.from.port.clone(),
                }
            } else {
                return Err(not_exportable(format!(
                    "`{}` is fed from `{}`, which is neither a source nor a node of the program",
                    edge.to, edge.from
                )));
            };
            let to = ArtifactPortRef::Node {
                node: local(&edge.to.node).expect("program nodes have artifact node IDs"),
                port: edge.to.port.clone(),
            };
            edges.push(ArtifactEdge { from, to });
        }

        Ok(Self { nodes, edges, members })
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
