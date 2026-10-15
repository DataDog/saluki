//! The export bundle: a scope of the default program as an artifact, with the catalog it is checked against.
//!
//! Authors edit the default program rather than start from nothing: the host exports the scope an artifact may
//! replace as a [`PolicyArtifact`], which an author edits and the channel delivers back. The bundle carries what is
//! needed to check and preview an artifact away from the host:
//!
//! - `export_version`: [`EXPORT_VERSION`];
//! - `scope`: the scope exported;
//! - `artifact`: the scope as a policy artifact, with its configuration bindings unresolved (an export never carries
//!   resolved values) and its [base](ArtifactBase) filled in: the base program hash and the fingerprint of every output
//!   the scope emits to. Its nodes and edges are in the order the plan lists them. Lowering writes nodes in the
//!   surface syntax's canonical order, but not every edge: the canonical order writes the edges from the scope's
//!   inputs first, in input-name order, and each node's edges with the node. So printing the export of a scope with
//!   several inputs and parsing the text gives back a document whose edges are in another order, with another hash,
//!   which composes to the same plan, with the same hash, since composition puts an artifact's edges in the default
//!   program's order;
//! - `ir_versions`: the IR version written, and the range of IR versions accepted;
//! - `catalog`: the host's [`MemoryCatalog`] document: scope inputs with their port types, outputs with what they
//!   accept, their fingerprints, and their recipients, and bindings with their types, liveness, and current values.
//!   Outputs a resources artifact declares are listed like native ones; delivery hosts and their settings are not;
//! - `labels`: display labels of outputs, by output ID, such as those a resources artifact gives its outputs, left
//!   out when there are none;
//! - `annotations`: notes for tools, left out when there are none: one line per node of the exported artifact and per
//!   binding, saying what it is and where it comes from, which a tool shows beside the program
//!   ([`Annotations`]). They are not part of the artifact;
//! - `form`: how the artifact writes the default program's policy settings ([`ExportForm`]): `bound` to the native
//!   settings, as the default program has them, or `detached`, as the literal values they have now. A reader of a
//!   bundle without `form` reads `bound`;
//! - `omitted`: the nodes of the scope a detached export leaves out because they do nothing as configured, by local
//!   ID, each with why (`identity`, `constant_split`, or `unreachable`), left out when there are none.
//!
//! Readers ignore members they do not know.
//!
//! # The scope as an artifact
//!
//! A scope's **region** is every node reachable from the output ports its inputs name, without entering a port one of
//! its exits names. The exported artifact has:
//!
//! - one node per region node, in plan order, whose local ID is the node's local ID (a reserved ID, for the default
//!   program), with the node's operator and its parameters without resolved values;
//! - every edge that enters a region node or an exit's port, in plan order, with an edge from a scope input's port
//!   written as leaving `input.<name>`, and an edge into an exit's port as entering `exit.<name>`;
//! - a base whose `outputs` hold the fingerprint the plan declares for every output a region `emit` delivers to, and
//!   whose `program_hash` is the hash of the artifact document itself without its `base.program_hash` member (see
//!   [`base`]).
//!
//! Composing an unmodified bound export back into the plan it came from gives that plan again (see
//! [`compose`](crate::compose::compose)), and an unmodified detached one a plan that does what it does.
//!
//! # The detached form
//!
//! A published program owns its policy: it writes what to do with metrics as literals, and binds only facts and
//! signals, which describe the host and its state rather than choose anything. The detached form is the export such a
//! program starts from ([`export_as`] with [`ExportForm::Detached`]): every parameter bound to a binding whose catalog
//! class is [`policy`](BindingClass::Policy) is written as the value it resolves to now, fact and signal bindings stay
//! bound, and every predicate is written with its literal `flag` and `is_empty` folded
//! ([`Predicate::fold_literals`]), so that a list the default program reads twice, such as a mirroring allowlist
//! tested for emptiness and then matched, is not written twice. A binding the catalog does not declare stays bound.
//! Every `emit` to an output whose recipients the catalog's recipient directory names exactly is written as an `emit`
//! to those names, such as `emit to [primary]`, as a published program addresses destinations: by the recipients it
//! sends to, never by the native output that encodes for them.
//!
//! The detached form is only what happens: a node that does nothing with the literal values it now has, such as a
//! metric filterlist with nothing in it, a mapper with no profiles, or a `merge` of one input, is left out, with what
//! only it reached, and listed in `omitted` with why ([`eliminate`], with the policy settings as the literals). A node
//! bound to a fact or signal is never left out for its value. The default program itself keeps every node.
//!
//! The detached artifact records the same base as the bound one: the base describes the scope's structure, which a
//! literal does not change, so composing the detached export is no stale base. Composition compares nodes by what
//! they do with the values they have, so the detached export composes to a plan that does what the default program
//! does, without the nodes it leaves out. Over the default program it is therefore a generation of its own, which
//! builds no kernel and keeps aggregation's state: plans are compared as written ([`operation_hash`]), so that the
//! plan running is always the plan its generation was built from. When it leaves out nothing, it adopts the running
//! generation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::Snafu;

use crate::{
    canonical::{strip_resolved_values, CanonicalError, PlanHash, BINDING_MEMBER, RESOLVED_MEMBER},
    ir::{
        is_identifier, ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, BindingName, Blobs,
        ComposedArtifact, ExitName, InputName, NodeId, OperatorName, OutputId, ParamType, Params, Plan, PolicyArtifact,
        PortRef, ScopeName, ACCEPTED_IR_VERSIONS, IR_VERSION,
    },
    operators::{names, OperatorRegistry, EMIT_TO},
    predicate::Predicate,
    validate::{Catalog, MemoryCatalog},
};

mod eliminate;
pub use self::eliminate::{eliminate, policy_literals, Eliminated, Omission};

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

    /// Notes for tools about the exported program's nodes and the catalog's bindings.
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    pub annotations: Annotations,

    /// How the artifact writes the default program's policy settings: bound to them, or as their current values.
    #[serde(default)]
    pub form: ExportForm,

    /// The nodes of the scope's region the artifact leaves out because they do nothing as configured, by local ID, with
    /// why ([`eliminate`]): only a detached export leaves any out. A host names them for a reader in
    /// [`Annotations::omitted`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted: BTreeMap<ArtifactNodeId, Omission>,
}

/// How an export writes the default program's policy settings (see [the detached form](self#the-detached-form)).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportForm {
    /// Bound to the native settings, as the default program has them: the form for tools that study lowering.
    #[default]
    Bound,

    /// Written as the literal values the settings have now, with facts and signals still bound: the form a published
    /// program starts from.
    Detached,

    /// A form a later host writes that this reader does not know. A host never exports in it.
    #[serde(other)]
    Unknown,
}

/// Notes for tools about an exported program: what each of its nodes and each binding is, and where it comes from, in
/// one line each, such as "Metric filterlist, from `metric_filterlist`".
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
    /// ([`ExportBundle::omitted`]), in words a reader knows, such as "Not shown, because they do nothing as configured:
    /// metric filterlist (empty), mapper (no profiles)", which an editor prints near the top of the program.
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
    scope: ScopeName,
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
    "scopes",
    "state_budget",
    "single_aggregation",
    "recipients",
    "detached_programs",
];

/// The members of a catalog output that [`MemoryCatalog`] reads.
const CATALOG_OUTPUT_MEMBERS: &[&str] = &["accepts", "fingerprint", "recipients"];

/// The members of an entry of a catalog's recipient directory that [`MemoryCatalog`] reads.
const CATALOG_RECIPIENT_MEMBERS: &[&str] = &["recipients", "host", "endpoint", "source"];

/// The sources of directory entries this version knows: an entry from another source is left out.
const RECIPIENT_SOURCES: &[&str] = &["native", "config", "resources"];

/// The members of a catalog binding that [`MemoryCatalog`] reads.
const CATALOG_BINDING_MEMBERS: &[&str] = &["type", "liveness", "class", "value"];

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
            annotations,
            form,
            omitted,
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
        recipients.retain(|_, entry| {
            entry
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| RECIPIENT_SOURCES.contains(&source))
        });
        for entry in recipients.values_mut() {
            *entry = keep(entry.take(), CATALOG_RECIPIENT_MEMBERS);
        }
    }
    Value::Object(catalog)
}

/// Exports one scope of a lowered plan as an artifact, with the host's catalog and output labels.
///
/// `plan` is the default program as lowered, with its bindings resolved; the export strips the resolved values, and
/// writes reserved node IDs as the artifact's local IDs, so that composing the unmodified
/// export back reproduces the default program. The catalog and labels are carried as given; the bundle has no
/// annotations, which the host adds if it has any ([`ExportBundle::annotations`]).
///
/// # Errors
///
/// Returns an error if the plan does not declare the scope, the scope cannot be written as an artifact (see
/// [`ExportError::NotExportable`]), or the scope has no canonical form.
pub fn export(
    plan: &Plan, scope: &ScopeName, catalog: &MemoryCatalog, labels: &BTreeMap<OutputId, String>,
) -> Result<ExportBundle, ExportError> {
    // The bound form leaves nothing out, so no operator's identity is asked for.
    export_as(
        plan,
        scope,
        catalog,
        labels,
        ExportForm::Bound,
        &OperatorRegistry::native(),
    )
}

/// Exports one scope of a lowered plan as an artifact in the form `form`, with the host's catalog and output labels.
///
/// This is [`export`] for [`ExportForm::Bound`]. For [`ExportForm::Detached`], every parameter bound to a policy
/// setting of `catalog` is written as the value it resolves to in `plan`, every predicate's literal `flag` and
/// `is_empty` are folded, and the nodes that do nothing with those values are left out and listed in
/// [`ExportBundle::omitted`] (see [the detached form](self#the-detached-form)); the base is the bound export's.
/// `registry` is the host's operator registry, which says which nodes do nothing.
///
/// # Errors
///
/// As for [`export`].
pub fn export_as(
    plan: &Plan, scope: &ScopeName, catalog: &MemoryCatalog, labels: &BTreeMap<OutputId, String>, form: ExportForm,
    registry: &OperatorRegistry,
) -> Result<ExportBundle, ExportError> {
    if form == ExportForm::Unknown {
        return Err(ExportError::UnknownForm);
    }
    let region = ScopeRegion::of(plan, scope)?;
    let base = region.base(plan, scope)?;
    let policy = policy_literals(catalog);
    // The detached form is only what happens: what does nothing with the policy settings' values is left out.
    let (eliminated, omitted) = match form {
        ExportForm::Detached => {
            let eliminated = eliminate(plan, registry, &policy);
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
        Some(eliminated) => ScopeRegion::of(eliminated, scope)?,
        None => region,
    };
    if let Some(plan) = &eliminated {
        for node in &mut nodes {
            let id = NodeId::reserved(node.id.to_local_id());
            let Some(lowered) = plan.node(&id) else { continue };
            let params = lowered
                .params
                .iter()
                .map(|(name, value)| {
                    let mut value = value.clone();
                    write_bindings_as_values(&mut value, &policy);
                    strip_resolved_values(&mut value);
                    (name.clone(), value)
                })
                .collect();
            node.params = fold_predicates(&node.op, params);
            if let Some(to) = recipients_of_output(plan, catalog, &node.params) {
                node.params = to;
            }
        }
    }
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

/// Returns the policy settings of `catalog`, by binding, whose current value in the default program `default` the plan
/// `running` does not run, in name order: for every node of `default` bound to a policy setting, the node is absent
/// from `running`, or the value at the binding's place in the node's parameters, written as a literal or as a binding
/// with its resolved value, differs from the value the setting has now.
///
/// So for a published program, which owns its policy, it lists the native settings changed since the program was
/// published, or that the program changes. A node of `running` that does what the default program's does, as its
/// [operation form](operation_params) says, lists nothing, whether it binds the settings or writes their values. A node
/// that does something else and wrote its parameters in another structure, such as a predicate folded otherwise, lists
/// each setting whose place it no longer has. A node of `default` that does nothing as configured ([`eliminate`]), such
/// as a filterlist with nothing in it, lists nothing when `running` leaves it out, as the detached export does, or has
/// it doing nothing too. Set natively later, it does something that `running`, without the node, does not: it lists
/// the settings that make it do something, those that, turned off alone (an empty list or string, the other flag,
/// zero), would make it do nothing again, such as a filterlist's entries but not its match mode, or every policy setting
/// it binds when none would.
///
/// `registry` is the host's operator registry, which says which nodes do nothing.
pub fn policy_differences(
    default: &Plan, running: &Plan, catalog: &MemoryCatalog, registry: &OperatorRegistry,
) -> Vec<BindingName> {
    fn walk(
        value: &Value, path: &mut Vec<PathStep>, policy: &dyn Fn(&BindingName) -> bool,
        found: &mut Vec<(Vec<PathStep>, BindingName, Value)>,
    ) {
        match value {
            Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
                let binding = members
                    .get(BINDING_MEMBER)
                    .and_then(Value::as_str)
                    .and_then(|name| name.parse::<BindingName>().ok());
                if let (Some(binding), Some(resolved)) = (binding, members.get(RESOLVED_MEMBER)) {
                    if policy(&binding) {
                        found.push((path.clone(), binding, resolved.clone()));
                    }
                }
            }
            Value::Object(members) => {
                for (name, member) in members {
                    path.push(PathStep::Member(name.clone()));
                    walk(member, path, policy, found);
                    path.pop();
                }
            }
            Value::Array(values) => {
                for (index, element) in values.iter().enumerate() {
                    path.push(PathStep::Index(index));
                    walk(element, path, policy, found);
                    path.pop();
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    fn at<'v>(value: &'v Value, path: &[PathStep]) -> Option<&'v Value> {
        path.iter().try_fold(value, |value, step| match step {
            PathStep::Member(name) => value.get(name),
            PathStep::Index(index) => value.get(index),
        })
    }
    fn at_mut<'v>(value: &'v mut Value, path: &[PathStep]) -> Option<&'v mut Value> {
        path.iter().try_fold(value, |value, step| match step {
            PathStep::Member(name) => value.get_mut(name),
            PathStep::Index(index) => value.get_mut(index),
        })
    }
    /// The values that commonly turn a setting off: an empty list or string, the other flag, zero.
    fn neutral(value: &Value) -> Option<Value> {
        match value {
            Value::Array(values) if !values.is_empty() => Some(Value::Array(Vec::new())),
            Value::String(value) if !value.is_empty() => Some(Value::String(String::new())),
            Value::Bool(value) => Some(Value::Bool(!value)),
            Value::Number(number) if number.as_f64() != Some(0.0) => Some(Value::from(0)),
            _ => None,
        }
    }

    let policy = policy_literals(catalog);
    // What does nothing as configured, as the detached export leaves it out.
    let does = |plan: &Plan| {
        eliminate(plan, registry, &policy)
            .omitted
            .into_iter()
            .map(|(node, _)| node)
            .collect::<BTreeSet<_>>()
    };
    let (idle_default, idle_running) = (does(default), does(running));
    let mut differs = BTreeSet::new();
    for node in &default.nodes {
        // A node that does nothing, which the running plan leaves out or has doing nothing too, follows nothing.
        if idle_default.contains(&node.id) && (idle_running.contains(&node.id) || running.node(&node.id).is_none()) {
            continue;
        }
        if running.node(&node.id).is_none() {
            // The running plan leaves out a node the default program has doing something: the settings that make it do
            // something differ, those that, turned off alone, would make it do nothing, such as a filterlist's entries
            // but not its match mode; or, when none would, every policy setting it binds.
            let mut found = Vec::new();
            for (name, value) in node.params.iter() {
                let mut at_param = Vec::new();
                walk(value, &mut Vec::new(), &policy, &mut at_param);
                found.extend(
                    at_param
                        .into_iter()
                        .map(|(path, binding, current)| (name, path, binding, current)),
                );
            }
            let explains = found
                .iter()
                .filter(|(name, path, _, current)| {
                    let Some(off) = neutral(current) else { return false };
                    let Some(mut param) = node.params.get(name.as_str()).cloned() else {
                        return false;
                    };
                    let Some(resolved) = at_mut(&mut param, path).and_then(|binding| binding.get_mut(RESOLVED_MEMBER))
                    else {
                        return false;
                    };
                    *resolved = off;
                    let mut turned_off = default.clone();
                    if let Some(other) = turned_off.nodes.iter_mut().find(|other| other.id == node.id) {
                        other.params.insert((*name).clone(), param);
                    }
                    does(&turned_off).contains(&node.id)
                })
                .map(|(_, _, binding, _)| binding.clone())
                .collect::<Vec<_>>();
            if explains.is_empty() {
                differs.extend(found.into_iter().map(|(_, _, binding, _)| binding));
            } else {
                differs.extend(explains);
            }
            continue;
        }
        let counterpart = running.node(&node.id).filter(|other| other.op == node.op);
        // A node that does what the default program's does follows every setting it binds, however it writes them.
        if counterpart
            .is_some_and(|other| operation_params(&other.op, &other.params) == operation_params(&node.op, &node.params))
        {
            continue;
        }
        for (name, value) in node.params.iter() {
            let mut found = Vec::new();
            walk(value, &mut Vec::new(), &policy, &mut found);
            for (path, binding, current) in found {
                let running_value = counterpart
                    .and_then(|other| other.params.get(name.as_str()))
                    .and_then(|value| at(value, &path))
                    .map(|value| match value {
                        Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
                            members.get(RESOLVED_MEMBER).cloned()
                        }
                        literal => Some(literal.clone()),
                    });
                if running_value.flatten().as_ref() != Some(&current) {
                    differs.insert(binding);
                }
            }
        }
    }
    differs.into_iter().collect()
}

/// One step into a JSON value: a member of an object, or an element of an array.
#[derive(Clone)]
enum PathStep {
    Member(String),
    Index(usize),
}

/// Writes every configuration binding in a parameter value for which `literal` holds as the value it resolved to.
///
/// A binding without a resolved value is left as it is.
pub(crate) fn write_bindings_as_values(value: &mut Value, literal: &dyn Fn(&BindingName) -> bool) {
    match value {
        Value::Object(members) if members.contains_key(BINDING_MEMBER) => {
            let chosen = members
                .get(BINDING_MEMBER)
                .and_then(Value::as_str)
                .and_then(|name| name.parse::<BindingName>().ok())
                .is_some_and(|name| literal(&name));
            if chosen {
                if let Some(resolved) = members.remove(RESOLVED_MEMBER) {
                    *value = resolved;
                }
            }
        }
        Value::Object(members) => members
            .values_mut()
            .for_each(|member| write_bindings_as_values(member, literal)),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|element| write_bindings_as_values(element, literal)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Returns what a node of operator `op` with parameters `params` does: its parameters with every configuration binding
/// written as the value it has ([`write_bindings_as_values`]) and every predicate's literal `flag` and `is_empty`
/// folded ([`fold_predicates`]), as the detached export writes them.
///
/// A parameter bound to a value and the same literal do the same thing: kernels are keyed by this form, the plan diff
/// compares it, and the installer decides by it whether a candidate changes anything.
pub fn operation_params(op: &OperatorName, params: &Params) -> Params {
    let literal = params
        .iter()
        .map(|(name, value)| {
            let mut value = value.clone();
            write_bindings_as_values(&mut value, &|_| true);
            (name.clone(), value)
        })
        .collect();
    fold_predicates(op, literal)
}

/// Returns the hash of what `plan` does: the plan, without provenance, its nodes and edges in order, with each node's
/// parameters in their [operation form](operation_params), as their operator decodes and encodes them when it can, so
/// that a set written in another order is the same set.
///
/// Two plans with one operation hash run the same kernels on the same edges: a parameter bound to a value and the same
/// literal hash alike, and so do plans that list their nodes or edges in another order, which means nothing to how a
/// plan runs. (Composition puts the edges a program shares with the default program in the default program's order,
/// but an edge it does not share, such as the one that takes the place of a node the detached export leaves out,
/// follows the program's own order.) A candidate whose operation hash is the active generation's changes nothing, and
/// adopts it if it is another program's. A node that does nothing as configured counts: a program written from the
/// detached export, which leaves such nodes out ([`eliminate`]), is a new generation over the default program, which
/// builds no kernel, so that the plan running is always the plan the generation was built from.
pub fn operation_hash(plan: &Plan, registry: &OperatorRegistry) -> Result<PlanHash, CanonicalError> {
    let mut plan = plan.clone();
    plan.provenance = None;
    plan.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    plan.edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    for node in &mut plan.nodes {
        let params = operation_params(&node.op, &node.params);
        node.params = match registry.configure(node.op.as_str(), &params) {
            Ok(configured) => configured.params().encode(),
            Err(_) => params,
        };
    }
    plan.hash(crate::canonical::ResolvedValues::Include)
}

/// Returns `params`, parameters of a node of operator `op`, with every predicate parameter's literal `flag` and
/// `is_empty` folded ([`Predicate::fold_literals`]). A predicate that does not read, or that folding leaves as it is,
/// is kept as written.
pub(crate) fn fold_predicates(op: &OperatorName, params: Params) -> Params {
    let Some(def) = OperatorRegistry::native().get(op.as_str()) else {
        return params;
    };
    let predicates = def
        .params()
        .iter()
        .filter(|spec| spec.ty == ParamType::Predicate)
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    params
        .iter()
        .map(|(name, value)| {
            let folded = predicates
                .contains(&name.as_str())
                .then(|| serde_json::from_value::<Predicate>(value.clone()).ok())
                .flatten()
                .and_then(|predicate| {
                    let folded = predicate.fold_literals();
                    (folded != predicate)
                        .then(|| serde_json::to_value(folded).ok())
                        .flatten()
                });
            (name.clone(), folded.unwrap_or_else(|| value.clone()))
        })
        .collect()
}

/// Returns the base an artifact derived from one scope of a plan records: the base program hash, and the fingerprint
/// of every output the scope emits to.
///
/// The **program hash** is the hash of the scope's export: the RFC 8785 digest of the exported artifact document
/// without its `base.program_hash` member, that is, of its `ir_version`, `scope`, `nodes`, `edges`, and the output
/// fingerprints of its base. It covers the scope's structure, operators, literal parameters, binding names, and the
/// outputs it delivers to, but neither resolved values (an export has none) nor anything outside the scope. So a change
/// of native configuration values never changes it, while a structural change inside the scope, or another binding of
/// an output the scope emits to, always does, and a change outside the scope never does: the base program hash changes
/// exactly when the export's artifact does, so a client that keeps an export knows to fetch it again when the hash it
/// last saw changes. An author's tool can tell whether an artifact is an unmodified export by comparing the hash of its
/// document without `base.program_hash` with its `base.program_hash`.
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

    /// The form is one this version does not know ([`ExportForm::Unknown`]).
    #[snafu(display("The export form is not one this host writes: it writes `bound` and `detached`."))]
    UnknownForm,
}

/// Returns the nodes of the region of `scope` in `plan`: every node reachable from the ports its inputs name, without
/// entering a port one of its exits names. A scope the plan does not declare has none.
///
/// In a plan composed from artifacts, the region of a scope an artifact replaced is the artifact's nodes: what the
/// program running there consists of ([`program_nodes`]).
pub fn scope_members(plan: &Plan, scope: &ScopeName) -> BTreeSet<NodeId> {
    let Some(declaration) = plan.scopes.get(scope) else {
        return BTreeSet::new();
    };
    let inputs = declaration.inputs.values().collect::<BTreeSet<_>>();
    let exits = declaration.exits.values().collect::<BTreeSet<_>>();
    let mut members = BTreeSet::new();
    let mut frontier = plan
        .edges
        .iter()
        .filter(|edge| inputs.contains(&edge.from) && !exits.contains(&edge.to))
        .map(|edge| &edge.to.node)
        .collect::<Vec<_>>();
    while let Some(node) = frontier.pop() {
        if members.insert(node.clone()) {
            frontier.extend(
                plan.edges
                    .iter()
                    .filter(|edge| &edge.from.node == node && !exits.contains(&edge.to))
                    .map(|edge| &edge.to.node),
            );
        }
    }
    members
}

/// Returns the nodes of the program each artifact composed into `plan` runs, by artifact, in the order the plan's
/// provenance lists them: the region of the scope it replaced ([`scope_members`]). A default program has none.
///
/// What a plan runs is a property of the plan, not of its nodes: every node of a scope an artifact replaced is the
/// artifact's, whether the author changed it or kept it as the default program has it. This is what the artifact limits
/// and placement's attributions apply to.
pub fn program_nodes(plan: &Plan) -> Vec<(ComposedArtifact, BTreeSet<NodeId>)> {
    plan.provenance
        .iter()
        .flat_map(|provenance| &provenance.artifacts)
        .map(|artifact| (artifact.clone(), scope_members(plan, &artifact.scope)))
        .collect()
}

/// One scope of a plan, as the parts of the artifact that replaces it.
///
/// This is what [`export`] writes, what [`base`] hashes, and what composition replaces: the region's nodes and the
/// edges that enter them.
pub(crate) struct ScopeRegion {
    /// The region's nodes as artifact nodes, in plan order: local IDs, no resolved values.
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

        let members = scope_members(plan, scope);

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
