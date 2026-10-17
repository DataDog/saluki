//! One scope over the whole metrics path: the default program's one scope, `metrics`, fed by every source and with no
//! exits, so that one artifact edits pre-aggregation, aggregation, and the policy region together, and its host's
//! restriction to one aggregation per program.
//!
//! The plan is the default-program fixture (`fixtures/default-program.json`); where a test compares with the two scopes
//! the default program declared through stage 9, it is the same fixture with only its `scopes` member changed
//! ([`with_two_scopes`]). Artifacts are edits of the scope's export, written by hand as changes to its JSON document,
//! composed with the real composer ([`compose_all`]), classified with the real plan diff, and installed through the
//! installer core on the frame computed from the boot plan. Expected results are written by hand from the fixture.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use saluki_core::{
    data_model::event::metric::{context::Context, Metric},
    execution::{AdmissionPointId, AdmissionTime, GenerationId, GenerationRef, RetireNotifier},
};
use serde_json::{json, Value};

use super::{
    correspondence::CorrespondenceKind,
    diff::NodeChange,
    tests::{describe, node_mut},
    *,
};
use crate::{
    canonical::ResolvedValues,
    channel::{ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot},
    compile::{compile, place, Frame, SlotId},
    compose::{
        compose_all,
        tests::{approval, catalog_for},
        Approval, ComposeError, Composed, Composer,
    },
    export::export,
    extension::TransitionClass,
    ir::{NodeId, OutputId, Plan, ReceivedArtifact, ScopeName},
    operators::OperatorRegistry,
    runtime::harness::FrameHarness,
    testing::{
        default_program_slot_name, with_two_scopes, DEFAULT_PROGRAM_JSON, DEFAULT_PROGRAM_SCOPE,
        DEFAULT_PROGRAM_SCOPE_INPUTS,
    },
    validate::{Location, MemoryCatalog, Rule, ValidationReport, Validator},
};

/// The one scope.
const SCOPE: &str = DEFAULT_PROGRAM_SCOPE;

/// The scope's inputs, each the `metrics` port of one source of the fixture.
const INPUTS: [(&str, &str, crate::operators::SourceBinding); 5] = DEFAULT_PROGRAM_SCOPE_INPUTS;

fn id(id: &str) -> NodeId {
    id.parse().unwrap()
}

fn scope() -> ScopeName {
    SCOPE.parse().unwrap()
}

fn registry() -> OperatorRegistry {
    OperatorRegistry::native()
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}

/// The default-program fixture with the two scopes it declared through stage 9.
fn two_scope_plan() -> Plan {
    Plan::from_value(with_two_scopes(serde_json::from_str(DEFAULT_PROGRAM_JSON).unwrap())).unwrap()
}

/// The default-program fixture, with its one scope, editable with `edit` first.
fn unified_with(edit: impl FnOnce(&mut Value)) -> Plan {
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    edit(&mut value);
    Plan::from_value(value).unwrap()
}

/// The boot plan.
fn unified_plan() -> Plan {
    unified_with(|_| {})
}

/// The host's catalog for the default program: the fixtures' catalog, which declares the one scope's inputs, each
/// carrying what its source produces, and one aggregation per program, with every binding's value, as composition
/// needs.
fn unified_catalog(plan: &Plan) -> MemoryCatalog {
    catalog_for(plan)
}

fn report(plan: &Plan) -> ValidationReport {
    Validator::new(&registry(), &unified_catalog(&unified_plan())).validate(plan)
}

fn frame(plan: &Plan) -> Frame {
    Frame::compute(plan, &registry(), &default_program_slot_name).unwrap()
}

/// The export of the one scope of the re-scoped boot plan, as an artifact document.
fn exported() -> Value {
    let plan = unified_plan();
    let bundle = export(&plan, &scope(), &unified_catalog(&plan), &BTreeMap::new()).unwrap();
    serde_json::to_value(&bundle.artifact).unwrap()
}

/// Receives an artifact document.
fn received(document: Value) -> ReceivedArtifact {
    ReceivedArtifact::from_value(document).unwrap()
}

/// Composes an artifact document into the re-scoped boot plan.
fn composed(document: Value) -> Result<Composed, ComposeError> {
    let plan = unified_plan();
    compose_all(
        &plan,
        &[(&received(document), &approval(1))],
        &unified_catalog(&plan),
        &registry(),
    )
}

/// Replaces the artifact edge `from -> to` with `edges`.
fn replace_edge(document: &mut Value, from: &str, to: &str, edges: &[(&str, &str)]) {
    let list = document["edges"].as_array_mut().unwrap();
    let at = list
        .iter()
        .position(|edge| edge["from"] == from && edge["to"] == to)
        .unwrap_or_else(|| panic!("the artifact has the edge `{} -> {}`", from, to));
    list.remove(at);
    for (offset, (from, to)) in edges.iter().enumerate() {
        list.insert(at + offset, json!({"from": from, "to": to}));
    }
}

fn add_nodes(document: &mut Value, nodes: &[Value]) {
    document["nodes"].as_array_mut().unwrap().extend(nodes.iter().cloned());
}

/// The routing edit: a split ahead of the policy tee that drops `demo.debug.` metrics.
fn route(document: &mut Value) {
    add_nodes(
        document,
        &[
            json!({"id": "drop_debug", "op": "split", "params": {"predicate": {"name": {"prefix": "demo.debug."}}}}),
            json!({"id": "drop_debug_true", "op": "drop"}),
        ],
    );
    replace_edge(
        document,
        "metrics.out",
        "metrics_fanout.in",
        &[
            ("metrics.out", "drop_debug.in"),
            ("drop_debug.true", "drop_debug_true.in"),
            ("drop_debug.false", "metrics_fanout.in"),
        ],
    );
}

/// The width edit: aggregation, and the timestamp split ahead of it, at a literal 30-second window.
fn widen(document: &mut Value) {
    node_mut(document, "dsd_no_agg_split")["params"]["window"] = json!("30s");
    node_mut(document, "dsd_agg")["params"]["window"] = json!("30s");
}

/// Returns the known `window` of a node of `plan`, as JSON.
fn window(plan: &Plan, node: &str) -> Option<Value> {
    let window = plan.node(&id(node))?.params.get("window")?;
    Some(window.get("resolved").unwrap_or(window).clone())
}

fn edited(edit: impl FnOnce(&mut Value)) -> Value {
    let mut document = exported();
    edit(&mut document);
    document
}

// 1. The re-scoped plan validates.

#[test]
fn the_default_program_rescoped_to_one_scope_validates_with_no_findings() {
    let plan = unified_plan();
    let validated = report(&plan);
    assert!(validated.findings().is_empty(), "{}", validated);

    // The scope's region is the whole metrics path: every node but the sources, which feed it.
    let bundle = export(&plan, &scope(), &unified_catalog(&plan), &BTreeMap::new()).unwrap();
    let sources = ["dsd_in", "otlp_in", "checks_ipc_in", "liveness_in", "trace_sampler"];
    assert_eq!(bundle.artifact.nodes.len(), plan.nodes.len() - sources.len());
    let region = bundle
        .artifact
        .nodes
        .iter()
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    let expected = plan
        .nodes
        .iter()
        .map(|node| node.id.local().to_string())
        .filter(|node| !sources.contains(&node.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(region, expected);
    // Every edge is the region's: the sources' edges leave the scope's inputs.
    assert_eq!(bundle.artifact.edges.len(), plan.edges.len());
    let inputs = bundle
        .artifact
        .edges
        .iter()
        .map(|edge| edge.from.to_string())
        .filter(|from| from.starts_with("input."))
        .collect::<Vec<_>>();
    assert_eq!(
        inputs,
        [
            "input.dogstatsd",
            "input.otlp",
            "input.checks",
            "input.liveness",
            "input.trace_sampler"
        ]
    );

    // Closedness is checked: an input left out leaves its source's records entering the scope from outside it.
    let without_otlp = unified_with(|value| {
        value["scopes"][SCOPE]["inputs"].as_object_mut().unwrap().remove("otlp");
    });
    let rescoped = report(&without_otlp);
    let closed = rescoped
        .findings()
        .iter()
        .filter(|finding| finding.rule == crate::validate::Rule::ScopeClosed)
        .map(|finding| finding.message.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        closed,
        [
            "`metrics_in.1` is inside the scope, but is fed from `otlp_in.metrics`, outside it; feed the scope \
          only through its inputs, or declare that port as one of them"
        ]
    );
}

// 2. The unmodified export composes back to the same plan.

#[test]
fn the_unmodified_export_of_the_one_scope_composes_back_to_the_same_plan_and_diffs_as_no_change() {
    let plan = unified_plan();
    let composed = composed(exported()).unwrap();
    assert_eq!(
        composed.plan.hash(ResolvedValues::Include).unwrap(),
        plan.hash(ResolvedValues::Include).unwrap()
    );
    assert!(composed.report.findings().is_empty(), "{}", composed.report);
    assert!(!composed.stale_base && !composed.stale_review);

    // Every node of the region, aggregation included, is the default program's own.
    let region = plan.nodes.len() - INPUTS.len();
    assert_eq!(composed.correspondence.iter().count(), region);
    assert!(composed.correspondence.removed().is_empty());
    for (artifact_node, correspondence) in composed.correspondence.iter() {
        assert_eq!(
            correspondence.kind,
            CorrespondenceKind::Unmodified,
            "`{}`",
            artifact_node
        );
        assert_eq!(correspondence.node.local().as_str(), artifact_node.as_str());
    }
    assert_eq!(composed.plan.node(&id("dsd_agg")), plan.node(&id("dsd_agg")));

    // The diff is empty: no node changed, aggregation keeps its lineage, and nothing is a cut.
    let changes = diff_with_correspondence(&plan, &composed.plan, &composed.correspondence, &registry());
    assert!(changes.diff.is_empty(), "{:?}", changes.diff);
    assert_eq!(changes.diff.nodes.len(), plan.nodes.len());
    assert!(changes
        .diff
        .nodes
        .iter()
        .all(|node| node.change == NodeChange::Unchanged));
    assert_eq!(changes.artifact_nodes.len(), region);
    let owners = &changes.diff.state_owners;
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].node, id("dsd_agg"));
    assert_eq!(
        (owners[0].class, owners[0].change),
        (TransitionClass::Retain, StateOwnerChange::Kept)
    );
    assert_eq!(owners[0].lineage.0, owners[0].lineage.1);
    assert_eq!(changes.diff.cut(), None);
}

#[test]
fn an_artifact_whose_edges_are_in_another_order_composes_as_it_does_in_the_exports_order() {
    let plan = unified_plan();
    let hash = |composed: &Composed| composed.plan.hash(ResolvedValues::Include).unwrap();
    // The order of edges means nothing to how a plan runs, and the plan's hash covers it: composition puts an
    // artifact's edges in the default program's order, so the unmodified export composes to the default program
    // whatever order its edges are in, as a printed and parsed export writes them.
    type Shuffle = fn(&mut Vec<Value>);
    let shuffles: [(&str, Shuffle); 3] = [
        ("reversed", |edges| edges.reverse()),
        ("rotated", |edges| edges.rotate_left(7)),
        ("the inputs' edges first", |edges| {
            edges.sort_by_key(|edge| !edge["from"].as_str().unwrap().starts_with("input."))
        }),
    ];
    for (what, shuffle) in shuffles {
        let mut document = exported();
        shuffle(document["edges"].as_array_mut().unwrap());
        assert_ne!(document["edges"], exported()["edges"], "{}", what);
        let composed = composed(document).unwrap();
        assert_eq!(composed.plan.edges, plan.edges, "{}", what);
        assert_eq!(hash(&composed), plan.hash(ResolvedValues::Include).unwrap(), "{}", what);

        // An edit composes to the same plan when the default program's edges are shuffled: they take their places, and
        // each new edge follows the edge listed before it, the new ones in the order they are listed. (Its nodes name
        // the artifact's hash, which covers the document's order, so the edges are compared.)
        let in_order = composed_edges(edited(route));
        let listed = edited(route)["edges"].as_array().unwrap().clone();
        let is_new = |edge: &Value| edge.to_string().contains("drop_debug");
        let new = listed.iter().filter(|edge| is_new(edge)).cloned().collect::<Vec<_>>();
        let first_new = listed.iter().position(is_new).unwrap();
        let before = listed[first_new - 1].clone();
        let mut document = edited(route);
        let edges = document["edges"].as_array_mut().unwrap();
        edges.retain(|edge| !is_new(edge));
        shuffle(edges);
        let at = edges.iter().position(|edge| *edge == before).unwrap();
        for (offset, edge) in new.into_iter().enumerate() {
            edges.insert(at + 1 + offset, edge);
        }
        assert_eq!(composed_edges(document), in_order, "{}", what);
    }
}

fn composed_edges(document: Value) -> Vec<crate::ir::Edge> {
    composed(document).unwrap().plan.edges
}

// 3. The composed plan places as the boot plan does.

#[test]
fn the_composed_plan_places_onto_the_boot_frame_exactly_as_the_boot_plan_does() {
    let plan = unified_plan();
    let frame = frame(&plan);
    // The scopes do not shape the frame: it is the two-scope fixture's frame.
    assert_eq!(frame.slots(), frame_of_two_scopes().slots());
    assert_eq!(frame.slots().iter().filter(|slot| slot.is_isolation()).count(), 1);

    let composed = composed(exported()).unwrap();
    let boot_report = report(&plan);
    let boot = place(&plan, &boot_report, &registry(), &frame).unwrap();
    let candidate = place(&composed.plan, &composed.report, &registry(), &frame).unwrap();
    assert_eq!(candidate, boot);
    for node in &plan.nodes {
        assert_eq!(candidate.site(&node.id), boot.site(&node.id), "`{}`", node.id);
    }
    // Compiled, every slot keeps every kernel: the same programs, the same exits used.
    let boot = compile(&plan, &boot_report, &registry(), &frame).unwrap();
    let candidate = compile(&composed.plan, &composed.report, &registry(), &frame).unwrap();
    assert_eq!(candidate.placement, boot.placement);
    let reuse = kernel_reuse(Some(&boot.programs), &candidate.programs);
    assert_eq!(reuse.len(), frame.slots().len());
    assert!(reuse
        .iter()
        .all(|slot| slot.unchanged && slot.build.is_empty() && slot.release.is_empty()));
}

fn frame_of_two_scopes() -> Frame {
    frame(&two_scope_plan())
}

// 4. A routing-only edit.

#[test]
fn a_routing_only_edit_composes_validates_places_and_retains_aggregation_with_no_cut() {
    let plan = unified_plan();
    let composed = composed(edited(route)).unwrap();
    assert!(composed.report.findings().is_empty(), "{}", composed.report);
    let correspondence = |node: &str| composed.correspondence.get(&node.parse().unwrap()).unwrap().clone();
    assert_eq!(correspondence("drop_debug").node, id("policy/drop_debug"));
    assert_eq!(correspondence("drop_debug").kind, CorrespondenceKind::Added);
    assert_eq!(correspondence("dsd_agg").kind, CorrespondenceKind::Unmodified);
    // Everything ahead of the new split is the default program's own.
    for node in [
        "dsd_enrich",
        "dsd_no_agg_split",
        "dsd_agg",
        "metrics_host_enrichment",
        "metrics",
    ] {
        assert_eq!(correspondence(node).kind, CorrespondenceKind::Unmodified, "`{}`", node);
    }

    let frame = frame(&plan);
    compile(&composed.plan, &composed.report, &registry(), &frame).unwrap();

    let changes = diff_with_correspondence(&plan, &composed.plan, &composed.correspondence, &registry());
    let aggregation = changes
        .diff
        .state_owners
        .iter()
        .find(|owner| owner.node == id("dsd_agg"))
        .unwrap();
    assert_eq!(
        (aggregation.class, aggregation.change),
        (TransitionClass::Retain, StateOwnerChange::Kept)
    );
    assert_eq!(aggregation.lineage.0, aggregation.lineage.1);
    assert_eq!(changes.diff.cut(), None);
    let changed = changes
        .diff
        .changes()
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    assert!(changed.contains(&"policy/drop_debug".to_string()), "{:?}", changed);
    assert!(!changed.iter().any(|node| node.starts_with("dsd_")), "{:?}", changed);
}

/// Returns the artifact nodes a composition does not keep as the default program's own, with how they relate to it.
fn not_unmodified(composed: &Composed) -> Vec<(String, CorrespondenceKind)> {
    composed
        .correspondence
        .iter()
        .filter(|(_, correspondence)| correspondence.kind != CorrespondenceKind::Unmodified)
        .map(|(node, correspondence)| (node.to_string(), correspondence.kind))
        .collect()
}

#[test]
fn a_width_only_edit_modifies_only_the_nodes_whose_parameters_it_changes() {
    use CorrespondenceKind::Modified;

    // A node is what it does, whatever feeds it: the nodes downstream of the widened aggregation do what they did,
    // so only the aggregation and the timestamp split, whose windows change, are the artifact's as written.
    let unified = composed(edited(widen)).unwrap();
    assert!(unified.report.findings().is_empty(), "{}", unified.report);
    assert_eq!(
        not_unmodified(&unified),
        [
            ("dsd_agg".to_string(), Modified),
            ("dsd_no_agg_split".to_string(), Modified),
        ]
    );

    // The same edit of the two-scope program's pre-aggregation scope.
    let plan = two_scope_plan();
    let pre = "dogstatsd_pre_aggregation".parse().unwrap();
    let catalog = catalog_for(&plan);
    let mut document = serde_json::to_value(export(&plan, &pre, &catalog, &BTreeMap::new()).unwrap().artifact).unwrap();
    widen(&mut document);
    let two_scopes = compose_all(&plan, &[(&received(document), &approval(1))], &catalog, &registry()).unwrap();
    assert_eq!(
        not_unmodified(&two_scopes),
        [
            ("dsd_agg".to_string(), Modified),
            ("dsd_no_agg_split".to_string(), Modified),
        ]
    );
}

/// Renames node `from` of an artifact document `to`, and the edges at its ports.
fn rename_node(document: &mut Value, from: &str, to: &str) {
    node_mut(document, from)["id"] = json!(to);
    for edge in document["edges"].as_array_mut().unwrap() {
        for end in ["from", "to"] {
            let port = edge[end].as_str().unwrap().to_string();
            if let Some(rest) = port.strip_prefix(&format!("{}.", from)) {
                edge[end] = json!(format!("{}.{}", to, rest));
            }
        }
    }
}

#[test]
fn the_one_aggregation_keeps_the_default_programs_id_and_lineage_whatever_the_artifact_calls_it() {
    let plan = unified_plan();
    let correspondence = |composed: &Composed, node: &str| {
        let found = composed.correspondence.get(&node.parse().unwrap()).unwrap();
        (found.node.to_string(), found.kind)
    };
    let aggregation = |composed: &Composed| {
        let changes = diff_with_correspondence(&plan, &composed.plan, &composed.correspondence, &registry());
        let owner = changes
            .diff
            .state_owners
            .iter()
            .find(|owner| owner.node == id("dsd_agg"))
            .cloned();
        (
            owner.map(|owner| (owner.class, owner.change)),
            changes.diff.cut().is_some(),
        )
    };

    // Renamed and otherwise unmodified, the aggregation is the default program's own: the composed plan is the default
    // program, and the diff keeps aggregation's lineage with no cut. The correspondence speaks of `agg`.
    let renamed = composed(edited(|document| rename_node(document, "dsd_agg", "agg"))).unwrap();
    assert!(renamed.report.findings().is_empty(), "{}", renamed.report);
    assert_eq!(
        correspondence(&renamed, "agg"),
        ("dsd_agg".to_string(), CorrespondenceKind::Unmodified)
    );
    assert_eq!(
        renamed.plan.hash(ResolvedValues::Include).unwrap(),
        plan.hash(ResolvedValues::Include).unwrap()
    );
    assert_eq!(
        aggregation(&renamed),
        (Some((TransitionClass::Retain, StateOwnerChange::Kept)), false)
    );

    // Renamed and widened: still `dsd_agg`, now the artifact's, and a new width is a cut, as without the rename.
    let widened = composed(edited(|document| {
        widen(document);
        rename_node(document, "dsd_agg", "agg");
    }))
    .unwrap();
    assert!(widened.report.findings().is_empty(), "{}", widened.report);
    assert_eq!(
        correspondence(&widened, "agg"),
        ("dsd_agg".to_string(), CorrespondenceKind::Modified)
    );
    let plain = composed(edited(widen)).unwrap();
    assert_eq!(aggregation(&widened), aggregation(&plain));
    assert!(aggregation(&widened).1, "a new width is a cut");

    // Renamed and fed otherwise, through a new split ahead of it: still `dsd_agg`, and still the default program's
    // node, since it does what that node does, whatever feeds it; its lineage parameters are unchanged, so it keeps
    // its state, with no cut.
    let refed = composed(edited(|document| {
        rename_node(document, "dsd_agg", "agg");
        add_nodes(
            document,
            &[
                json!({"id": "drop_debug", "op": "split",
                       "params": {"predicate": {"name": {"prefix": "debug."}}}}),
                json!({"id": "drop_debug_true", "op": "drop"}),
            ],
        );
        replace_edge(
            document,
            "dsd_tag_filterlist.out",
            "agg.in",
            &[
                ("dsd_tag_filterlist.out", "drop_debug.in"),
                ("drop_debug.true", "drop_debug_true.in"),
                ("drop_debug.false", "agg.in"),
            ],
        );
    }))
    .unwrap();
    assert!(refed.report.findings().is_empty(), "{}", refed.report);
    assert_eq!(
        correspondence(&refed, "agg"),
        ("dsd_agg".to_string(), CorrespondenceKind::Unmodified)
    );
    assert_eq!(
        aggregation(&refed),
        (Some((TransitionClass::Retain, StateOwnerChange::Kept)), false)
    );

    // A host that does not restrict programs to one aggregation places the renamed node in the artifact's namespace,
    // as a new state owner.
    let mut document = serde_json::to_value(unified_catalog(&plan)).unwrap();
    document.as_object_mut().unwrap().remove("single_aggregation");
    let catalog = serde_json::from_value::<MemoryCatalog>(document).unwrap();
    let artifact = received(edited(|document| rename_node(document, "dsd_agg", "agg")));
    let unrestricted = compose_all(&plan, &[(&artifact, &approval(1))], &catalog, &registry()).unwrap();
    assert_eq!(
        correspondence(&unrestricted, "agg"),
        ("policy/agg".to_string(), CorrespondenceKind::Added)
    );

    // With two aggregations, neither takes the reserved ID in place of its own, and validation refuses the second.
    let two = composed(edited(|document| {
        rename_node(document, "dsd_agg", "agg");
        let mut second = node_mut(document, "agg").clone();
        second["id"] = json!("late_agg");
        document["nodes"].as_array_mut().unwrap().push(second);
        replace_edge(
            document,
            "metrics.out",
            "metrics_fanout.in",
            &[("metrics.out", "late_agg.in"), ("late_agg.out", "metrics_fanout.in")],
        );
    }))
    .unwrap();
    assert_eq!(correspondence(&two, "agg").0, "policy/agg");
    assert_eq!(correspondence(&two, "late_agg").0, "policy/late_agg");
    assert!(
        two.report.errors().any(|error| error.rule == Rule::SingleAggregation),
        "{}",
        two.report
    );
}

// 5 and 6. Installation through the installer core.

/// The host's composer: [`compose_all`] over the re-scoped boot plan and its catalog.
struct UnifiedComposer {
    plan: Plan,
    catalog: MemoryCatalog,
}

impl Composer for UnifiedComposer {
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        compose_all(&self.plan, artifacts, &self.catalog, &registry())
    }
}

/// An installer core on the frame of the re-scoped boot plan, booted with it, with the real composer and the real
/// cut classification, and the host around it: it performs commits and cut finalizations on its schedule.
struct Host {
    core: InstallerCore,
    composer: UnifiedComposer,
    retired: Arc<Mutex<Vec<GenerationId>>>,
    arming: Option<ArmCut>,
    schedule: GenerationRef,
}

impl Host {
    fn new() -> Self {
        Self::with(unified_plan(), InstallerConfig::default())
    }

    /// Boots the default program `plan`, with the installer configured as `config` says.
    fn with(plan: Plan, config: InstallerConfig) -> Self {
        let retired = Arc::new(Mutex::new(Vec::new()));
        let notified = Arc::clone(&retired);
        let notifier = move |_: GenerationId| {
            let notified = Arc::clone(&notified);
            RetireNotifier::new(move |id| notified.lock().unwrap().push(id))
        };
        let boot = Candidate {
            plan: Arc::new(plan.clone()),
            report: report(&plan),
            origin: Origin::DefaultProgram { config_change: 0 },
        };
        let (core, boot) = InstallerCore::new(Arc::new(frame(&plan)), registry(), config, notifier, boot).unwrap();
        Self {
            core,
            composer: UnifiedComposer {
                catalog: unified_catalog(&plan),
                plan,
            },
            retired,
            arming: None,
            schedule: boot.generation,
        }
    }

    /// The channel delivers the one scope's artifact `document` in config `demo` at version and revision `revision`.
    fn deliver(&mut self, revision: u64, document: Value) -> Vec<String> {
        let payload = json!({
            "revision": revision,
            "artifact_hash": received(document.clone()).hash,
            "artifact": document,
        })
        .to_string();
        let config = ArtifactConfig {
            id: ArtifactConfigId::new("demo"),
            version: revision,
            sha256: ArtifactConfig::transport_hash(&payload),
            payload,
        };
        let snapshot = ArtifactSnapshot::new(ArtifactProduct::Policy, vec![config]);
        let actions = self.core.on_snapshot(&snapshot, &self.composer);
        self.perform(actions)
    }

    /// The channel revokes the policy: a snapshot without configs.
    fn revoke(&mut self) -> Vec<String> {
        let snapshot = ArtifactSnapshot::new(ArtifactProduct::Policy, Vec::new());
        let actions = self.core.on_snapshot(&snapshot, &self.composer);
        self.perform(actions)
    }

    fn acknowledge(&mut self) -> Vec<String> {
        let generation = self.core.status().preparing.expect("a generation is being prepared").id;
        let mut actions = Vec::new();
        for slot in 0..self.core.frame().slots().len() as u16 {
            actions.extend(self.core.prepare_acknowledged(generation, SlotId::new(slot), Ok(())));
        }
        self.perform(actions)
    }

    fn cut_generation(&self) -> GenerationId {
        self.core.cut_in_flight().expect("a cut is in flight").generation().id()
    }

    fn perform(&mut self, mut actions: Vec<Action>) -> Vec<String> {
        let mut performed = Vec::new();
        loop {
            let mut next = Vec::new();
            for action in actions {
                performed.push(describe(&action));
                match action {
                    Action::Commit { generation } | Action::FinalizeCut { generation } => self.schedule = generation,
                    Action::ArmCut(arming) => self.arming = Some(arming),
                    Action::InstallDefaultProgram => {
                        let plan = self.composer.plan.clone();
                        let candidate = Candidate {
                            report: report(&plan),
                            plan: Arc::new(plan),
                            origin: Origin::DefaultProgram { config_change: 1 },
                        };
                        next.extend(self.core.on_default_program(candidate, &self.composer).1);
                    }
                    _ => {}
                }
            }
            let retired = std::mem::take(&mut *self.retired.lock().unwrap());
            next.extend(retired.into_iter().flat_map(|id| self.core.retired(id)));
            if next.is_empty() {
                return performed;
            }
            actions = next;
        }
    }

    /// Arms the cut asked for at `secs`, then reports the first stamp of its generation and every affected point
    /// past it.
    fn run_cut(&mut self, secs: u64) -> Vec<String> {
        let arming = self.arming.take().expect("an arming was asked for");
        let (generation, affected) = (arming.generation.id(), arming.affected.len());
        // The host keeps no reference to the generations but the schedule's, as a host's installer worker does.
        drop(arming);
        let actions = self.core.cut_armed(generation, AdmissionTime::from_unix_secs(secs));
        let mut performed = self.perform(actions);
        assert_eq!(self.cut_generation(), generation);
        let progress = CutProgress {
            stamped_new: true,
            passed: affected,
            affected,
        };
        let actions = self.core.cut_progress(generation, progress);
        performed.extend(self.perform(actions));
        performed
    }

    fn active_plan(&self) -> Arc<Plan> {
        Arc::clone(self.core.active_plan().unwrap())
    }

    fn demo(&self) -> (crate::channel::ApplyState, Option<u64>, Option<String>) {
        let ack = self
            .core
            .config_states()
            .into_iter()
            .find(|ack| ack.id.as_str() == "demo")
            .unwrap();
        (ack.state, ack.generation, ack.error)
    }
}

/// Describes the preparation of `generation` with what each slot prepares, as [`describe`] writes it. The frame's
/// slots are the three pre-aggregation regions, aggregation's isolation slot (3), the post-aggregation filter's region,
/// and enrichment with the whole policy region (5).
fn prepare(generation: u64, slots: &str) -> String {
    format!(
        "prepare {} [{}]",
        generation,
        slots.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// The admission point of the DogStatsD source, the only one upstream of aggregation's isolation slot.
fn dogstatsd_point() -> AdmissionPointId {
    let frame = frame(&unified_plan());
    let point = frame
        .admission_points()
        .iter()
        .find(|point| point.node == id("dsd_in"))
        .unwrap();
    point.id
}

#[test]
fn an_unmodified_export_of_the_one_scope_adopts_the_running_generation() {
    let mut host = Host::new();
    let boot_hash = host.active_plan().hash(ResolvedValues::Include).unwrap();
    // The composition is the boot plan: it adopts the boot generation. Nothing is prepared, so no kernel is built and
    // nothing is cut or drained; the generation keeps its ID and only its origin changes, which the regions are told,
    // for its panic budget. The config is acknowledged active on the boot generation.
    let document = exported();
    let hash = received(document.clone()).hash;
    let performed = host.deliver(1, document);
    let origin = Origin::Artifact { revision: 1, hash };
    assert_eq!(
        performed,
        strings(&["candidate 2: unchanged", &format!("adopt 1 as {}", origin)]),
        "{:?}",
        performed
    );
    assert_eq!(host.demo(), (crate::channel::ApplyState::Active, Some(1), None));
    let status = host.core.status();
    let active = status.active.as_ref().unwrap();
    assert_eq!((active.id, active.origin), (GenerationId::new(1), origin));
    assert_eq!(host.schedule.id(), GenerationId::new(1));
    // The plan the generation holds is the composition's: the same hash, and a provenance that names the program.
    let plan = host.active_plan();
    assert_eq!(plan.hash(ResolvedValues::Include).unwrap(), boot_hash);
    assert_eq!(plan.provenance.as_ref().unwrap().artifacts[0].revision().revision, 1);

    // Another document that composes to the same plan, its edges in another order, adopts it again, as its revision.
    let mut document = exported();
    document["edges"].as_array_mut().unwrap().reverse();
    let performed = host.deliver(2, document.clone());
    let origin = Origin::Artifact {
        revision: 2,
        hash: received(document).hash,
    };
    assert_eq!(
        performed,
        strings(&["candidate 3: unchanged", &format!("adopt 1 as {}", origin)]),
        "{:?}",
        performed
    );

    // Revoked, the default program takes the generation back the same way: no new generation, native behavior again.
    let performed = host.revoke();
    assert!(
        performed.contains(&format!("adopt 1 as {}", Origin::DefaultProgram { config_change: 1 })),
        "{:?}",
        performed
    );
    assert!(
        !performed.iter().any(|action| action.starts_with("prepare")),
        "{:?}",
        performed
    );
    let status = host.core.status();
    let active = status.active.as_ref().unwrap();
    assert_eq!(active.id, GenerationId::new(1));
    assert!(matches!(active.origin, Origin::DefaultProgram { .. }));
}

#[test]
fn a_generation_the_default_program_adopted_after_its_artifacts_budget_ran_out_is_replaced() {
    let mut host = Host::new();
    host.deliver(1, exported());
    // The artifact's generation exhausts its panic budget, but the revocation is processed before the region's report:
    // the default program adopts the generation, with a zero budget.
    let performed = host.revoke();
    assert!(
        performed.contains(&format!("adopt 1 as {}", Origin::DefaultProgram { config_change: 1 })),
        "{:?}",
        performed
    );
    // The failure was decided under the artifact's budget. The generation now runs the default program, which cannot
    // roll back, so the default program is asked for again, and a candidate equal to a failed generation gets a new
    // one rather than being unchanged: metrics are not dropped until the next configuration change.
    let actions = host
        .core
        .generation_failed(GenerationId::new(1), "it exceeded its panic budget, with 4 panics");
    let performed = host.perform(actions);
    assert!(
        performed.contains(&"install the default program".to_string()),
        "{:?}",
        performed
    );
    assert!(
        performed.iter().any(|action| action.starts_with("prepare 2 ")),
        "{:?}",
        performed
    );
    let performed = host.acknowledge();
    assert!(performed.contains(&"commit 2".to_string()), "{:?}", performed);
    assert_eq!(host.schedule.id(), GenerationId::new(2));
    let status = host.core.status();
    let active = status.active.as_ref().unwrap();
    assert_eq!(active.id, GenerationId::new(2));
    assert!(matches!(active.origin, Origin::DefaultProgram { .. }));
}

#[test]
fn a_routing_only_edit_installs_through_the_installer_core_as_a_commit() {
    let mut host = Host::new();
    let performed = host.deliver(1, edited(route));
    // Only the policy region changes, and aggregation's isolation slot keeps its instance.
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                2,
                "0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:unchanged reuse 2 build 0, 3:unchanged \
                 reuse 1 build 0, 4:changed reuse 0 build 0, 5:changed reuse 2 build 0"
            ),
            "candidate 2 generation 2: pending",
        ])
    );
    let performed = host.acknowledge();
    assert_eq!(
        performed[..2],
        strings(&["commit 2", "candidate 2 generation 2: active"])
    );
    assert!(host.arming.is_none() && host.core.cut_in_flight().is_none());
    assert_eq!(host.schedule.id(), GenerationId::new(2));
}

#[test]
fn a_width_only_edit_installs_through_the_installer_core_as_a_cut() {
    let mut host = Host::new();
    let performed = host.deliver(1, edited(widen));
    // Only the region running the timestamp split, and aggregation's isolation slot, change: aggregation's new
    // lineage is built.
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                2,
                "0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:changed reuse 1 build 1, 3:changed \
                 reuse 0 build 1, 4:unchanged reuse 0 build 0, 5:unchanged reuse 2 build 0"
            ),
            "candidate 2 generation 2: pending",
        ])
    );
    assert_eq!(
        host.acknowledge(),
        strings(&[
            "candidate 2 generation 2: prepared",
            &format!("arm the cut of 2 over [{:?}]", dogstatsd_point()),
        ])
    );
    let arming = host.arming.as_ref().unwrap();
    assert_eq!(arming.widths, vec![Duration::from_secs(10), Duration::from_secs(30)]);
    assert_eq!(arming.old.id(), GenerationId::new(1));

    let performed = host.run_cut(1_700_000_130);
    assert_eq!(
        performed,
        strings(&[
            "watch the cut of 2 at 1700000130.000000000",
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: draining",
            "finalize the cut of 2",
            "candidate 1 generation 1: retired",
            "release 1 credits",
        ])
    );
    assert_eq!(host.schedule.id(), GenerationId::new(2));
    let plan = host.active_plan();
    assert_eq!(window(&plan, "dsd_agg"), Some(json!("30s")));
    assert_eq!(window(&plan, "dsd_no_agg_split"), Some(json!("30s")));
    assert_eq!(host.demo().1, Some(2));
}

#[test]
fn a_width_and_routing_edit_installs_as_one_generation_through_one_cut_with_the_routing_in_effect() {
    let mut host = Host::new();
    let performed = host.deliver(
        1,
        edited(|document| {
            widen(document);
            route(document);
        }),
    );
    // One preparation: the width change's two regions, and the policy region, whose decision the split joins. The
    // post-aggregation filter's region builds and reuses nothing new: only its program's fingerprint changes, as for
    // the routing edit alone.
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                2,
                "0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:changed reuse 1 build 1, 3:changed \
                 reuse 0 build 1, 4:changed reuse 0 build 0, 5:changed reuse 2 build 0"
            ),
            "candidate 2 generation 2: pending",
        ])
    );
    assert_eq!(
        host.acknowledge(),
        strings(&[
            "candidate 2 generation 2: prepared",
            &format!("arm the cut of 2 over [{:?}]", dogstatsd_point()),
        ])
    );
    let performed = host.run_cut(1_700_000_130);
    assert!(
        performed.contains(&"finalize the cut of 2".to_string()),
        "{:?}",
        performed
    );
    assert_eq!(host.core.status().live, vec![GenerationId::new(2)]);

    // The one generation has both changes, the filter in normal form, keeping its `true` port.
    let plan = host.active_plan();
    assert_eq!(window(&plan, "dsd_agg"), Some(json!("30s")));
    assert!(plan.node(&id("policy/drop_debug")).is_some());
    assert!(plan
        .edges
        .iter()
        .any(|edge| edge.from.to_string() == "policy/drop_debug.true" && edge.to.to_string() == "metrics_fanout.in"));
    assert_eq!(host.demo().1, Some(2));
}

/// The base time of the runtime test: a multiple of 30 seconds.
const B: u64 = 1_700_000_100;

fn at(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

#[test]
fn a_width_and_routing_edit_runs_through_region_drivers_with_the_routing_in_effect_from_the_cut() {
    let boot = unified_plan();
    let combined = composed(edited(|document| {
        widen(document);
        route(document);
    }))
    .unwrap();
    assert!(combined.report.findings().is_empty(), "{}", combined.report);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut harness = FrameHarness::starting_at(
        &boot,
        &report(&boot),
        registry(),
        Default::default(),
        AdmissionTime::from_unix_secs(B),
    );
    let debug = || vec![Metric::gauge(Context::from_static_name("demo.debug.queue"), 1.0)];
    let kept = || vec![Metric::gauge(Context::from_static_name("demo.kept.queue"), 2.0)];

    let hits = || vec![Metric::counter(Context::from_static_name("app.hits"), 1.0)];

    // Before the cut, OTLP's debug metric reaches the main output, and DogStatsD's counter is aggregated in 10-second
    // buckets.
    let tick = |harness: &mut FrameHarness, relative: u64| {
        harness.heartbeat("dsd_in", at(relative));
        harness.advance(at(relative));
    };
    harness.admit("otlp_in", at(1), debug());
    harness.admit("otlp_in", at(1), kept());
    harness.admit("dsd_in", at(1), hits());
    tick(&mut harness, 15);

    assert!(
        harness.prepare(&combined.plan, &combined.report),
        "the installation is a cut"
    );
    let cut = harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(30)]);
    let after = cut.as_unix_secs() - B;
    assert_eq!(after, 30);
    for relative in (30..after).step_by(15) {
        tick(&mut harness, relative);
    }
    // At and after the cut, the new generation drops the debug metric, keeps the rest, and aggregates in 30-second
    // buckets.
    harness.admit("otlp_in", at(after), debug());
    harness.admit("otlp_in", at(after), kept());
    harness.admit("dsd_in", at(after + 1), hits());
    for relative in (after..=after + 60).step_by(15) {
        tick(&mut harness, relative);
    }
    harness.finalize_cut();

    let main = OutputId::new("dd_metrics_encode").unwrap();
    let delivered = harness
        .delivered()
        .iter()
        .filter(|record| record.output == main)
        .map(described)
        .collect::<Vec<_>>();
    assert_eq!(
        delivered,
        [
            "g1 @1 demo.debug.queue gauge [-:1]",
            "g1 @1 demo.kept.queue gauge [-:2]",
            "g1 @15 tenant.app.hits rate/10s [0:1]",
            // The cut is at the first 30-second boundary at least the cut margin away: the debug metric admitted then
            // is dropped by the new generation, which also runs the 30-second aggregation.
            "g2 @30 demo.kept.queue gauge [-:2]",
            "g1 @30 tenant.app.hits rate/10s [10:0,20:0]",
            "g2 @60 tenant.app.hits rate/30s [30:1]",
            "g2 @90 tenant.app.hits rate/30s [60:0]",
        ]
    );
}

/// Describes a delivered record as `g<generation> @<admission time> <name> <kind> [<timestamp>:<value>, ...]`, with
/// times relative to `B`.
fn described(record: &crate::runtime::harness::DeliveredRecord) -> String {
    use saluki_core::data_model::event::metric::MetricValues;
    let points = |points: &saluki_core::data_model::event::metric::ScalarPoints| {
        points
            .into_iter()
            .map(|(timestamp, value)| match timestamp {
                Some(timestamp) => format!("{}:{}", timestamp.get() - B, value),
                None => format!("-:{}", value),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let values = match record.metric.values() {
        MetricValues::Rate(values, interval) => format!("rate/{}s [{}]", interval.as_secs(), points(values)),
        MetricValues::Gauge(values) => format!("gauge [{}]", points(values)),
        other => format!("{:?}", other),
    };
    format!(
        "g{} @{} {} {}",
        record.generation.map_or(0, |generation| generation.get()),
        record.admitted_at.map_or(0, |time| time.as_unix_secs() - B),
        record.metric.context().name(),
        values
    )
}

// 7. One aggregation at most.

/// An artifact with a copy of `dsd_agg` named `id`, spliced in place of the artifact edge `from -> to`.
fn second_aggregation(id: &str, from: &str, to: &str) -> Value {
    edited(|document| {
        let mut aggregation = node_mut(document, "dsd_agg").clone();
        aggregation["id"] = json!(id);
        add_nodes(document, &[aggregation]);
        let (input, output) = (format!("{}.in", id), format!("{}.out", id));
        replace_edge(document, from, to, &[(from, input.as_str()), (output.as_str(), to)]);
    })
}

/// What becomes of a composed candidate in the boot frame: refused, invalid, unplaceable (with the capabilities it
/// misses), or placed.
fn outcome(composed: Result<Composed, ComposeError>) -> String {
    let frame = frame(&unified_plan());
    match composed {
        Err(error) => format!("refused: {}", error),
        Ok(composed) if !composed.report.is_valid() => format!("invalid: {}", composed.report),
        Ok(composed) => match compile(&composed.plan, &composed.report, &registry(), &frame) {
            Ok(_) => "places".to_string(),
            Err(crate::compile::CompileError::Placement { source }) => {
                format!("unplaceable {:?}: {}", source.capabilities(), source)
            }
            Err(error) => format!("does not compile: {}", error),
        },
    }
}

/// The message of a second state owner whose input comes from a region with no exit to an isolation slot.
fn no_isolation_slot(node: &str, region: &str) -> String {
    format!(
        "unplaceable [FrameChanges]: The candidate plan cannot run in the physical structure built at boot: node `{}`: \
         it is a state owner, which runs only in an isolation slot that the region its input comes from has an exit \
         to, and the topology built at boot has no such isolation slot for `{}`: a new exit from a region to an \
         isolation slot requires stage 10 frame changes: the physical structure is built from the default program at \
         boot, so a restart does not add it.",
        node, region
    )
}

/// The message of the `single_aggregation` finding at the second aggregation `node`.
fn second_one(node: &str) -> String {
    format!(
        "a program has at most one aggregation; `{}` is a second one (`dsd_agg` is the first)",
        node
    )
}

/// Where a second aggregation goes, as (what, the artifact edge it is spliced into).
const SECOND_AGGREGATIONS: [(&str, (&str, &str)); 5] = [
    (
        "after enrichment, ahead of the policy tee",
        ("metrics.out", "metrics_fanout.in"),
    ),
    (
        "after the first, fed by its isolation slot",
        ("dsd_agg.out", "dsd_post_agg_filter_in.0"),
    ),
    (
        "ahead of the first, in the pre-aggregation pipeline",
        ("dsd_no_agg_split.default", "dsd_tag_filterlist.in"),
    ),
    (
        "on the no-aggregation pipeline's path, the timestamp split's other branch",
        ("dsd_no_agg_split.passthrough", "dsd_post_agg_filter_in.1"),
    ),
    ("on OTLP's path", ("input.otlp", "metrics_in.1")),
];

#[test]
fn a_second_aggregation_is_rejected_by_validation_wherever_it_is_at_its_node() {
    for (what, (from, to)) in SECOND_AGGREGATIONS {
        let composed = composed(second_aggregation("late_agg", from, to)).unwrap();
        // The one error is the second aggregation's, at its node, which an editor marks at the node's line.
        let errors = composed.report.errors().collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{}: {}", what, composed.report);
        assert_eq!(errors[0].rule, Rule::SingleAggregation, "{}", what);
        assert_eq!(errors[0].rule.as_str(), "single_aggregation");
        assert_eq!(
            errors[0].location,
            Location::Node {
                id: id("policy/late_agg"),
                port: None
            },
            "{}",
            what
        );
        assert_eq!(errors[0].message, second_one("policy/late_agg"), "{}", what);
        assert_eq!(
            outcome(Ok(composed)),
            format!(
                "invalid: error[single_aggregation] node `policy/late_agg`: {}\n",
                second_one("policy/late_agg")
            ),
            "{}",
            what
        );
    }

    // The default program's aggregation is the first, wherever the artifact lists its own: the one it adds is the one
    // reported, even ahead of it in plan order.
    let ahead = composed(edited(|document| {
        let mut aggregation = node_mut(document, "dsd_agg").clone();
        aggregation["id"] = json!("early_agg");
        document["nodes"].as_array_mut().unwrap().insert(0, aggregation);
        replace_edge(
            document,
            "dsd_no_agg_split.passthrough",
            "dsd_post_agg_filter_in.1",
            &[
                ("dsd_no_agg_split.passthrough", "early_agg.in"),
                ("early_agg.out", "dsd_post_agg_filter_in.1"),
            ],
        );
    }))
    .unwrap();
    let errors = ahead.report.errors().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{}", ahead.report);
    assert_eq!(
        errors,
        [format!(
            "error[single_aggregation] node `policy/early_agg`: {}",
            second_one("policy/early_agg")
        )]
    );
}

#[test]
fn a_second_aggregation_beside_a_widened_one_is_the_one_reported_wherever_it_is_listed() {
    // A width change modifies the default program's aggregation; an aggregation the artifact adds, listed ahead of it,
    // is still the second one: the first is the one with a reserved ID.
    let composed = composed(edited(|document| {
        widen(document);
        let mut aggregation = node_mut(document, "dsd_agg").clone();
        aggregation["id"] = json!("early_agg");
        document["nodes"].as_array_mut().unwrap().insert(0, aggregation);
        replace_edge(
            document,
            "dsd_no_agg_split.passthrough",
            "dsd_post_agg_filter_in.1",
            &[
                ("dsd_no_agg_split.passthrough", "early_agg.in"),
                ("early_agg.out", "dsd_post_agg_filter_in.1"),
            ],
        );
    }))
    .unwrap();
    let kind = |node: &str| {
        composed
            .correspondence
            .artifact_node(&id(node))
            .and_then(|local| composed.correspondence.get(local))
            .map(|correspondence| correspondence.kind)
    };
    assert_eq!(kind("dsd_agg"), Some(CorrespondenceKind::Modified));
    assert_eq!(kind("policy/early_agg"), Some(CorrespondenceKind::Added));
    let position = |node: &str| {
        composed
            .plan
            .nodes
            .iter()
            .position(|found| found.id == id(node))
            .unwrap()
    };
    assert!(position("policy/early_agg") < position("dsd_agg"));
    let errors = composed.report.errors().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        errors,
        [format!(
            "error[single_aggregation] node `policy/early_agg`: {}",
            second_one("policy/early_agg")
        )]
    );
}

/// The fixtures' catalog for the default program, without the restriction to one aggregation.
fn catalog_of_any_aggregations() -> MemoryCatalog {
    crate::testing::without_single_aggregation(unified_catalog(&unified_plan()))
}

#[test]
fn placement_alone_would_run_a_second_aggregation_on_the_other_branch_of_the_timestamp_split() {
    // Why the rule is needed: without it, placement rejects a second aggregation where the frame has no isolation slot
    // for it, but runs one on the timestamp split's other branch, whose region has an exit to the one isolation slot,
    // which runs a state owner of each branch by design.
    let plan = unified_plan();
    let catalog = catalog_of_any_aggregations();
    assert!(!crate::validate::Catalog::single_aggregation(&catalog));
    let expected = [
        no_isolation_slot("policy/late_agg", "metrics_enrich"),
        no_isolation_slot("policy/late_agg", "dsd_agg"),
        // The second takes the first's place in the pre-aggregation region's exit, so the first is the one without an
        // isolation slot it can reach.
        no_isolation_slot("dsd_agg", "dsd_post_agg_filter_region"),
        "places".to_string(),
        no_isolation_slot("policy/late_agg", "metrics_enrich"),
    ];
    for ((what, (from, to)), expected) in SECOND_AGGREGATIONS.into_iter().zip(expected) {
        let document = second_aggregation("late_agg", from, to);
        let composed = compose_all(&plan, &[(&received(document), &approval(1))], &catalog, &registry());
        assert!(
            composed.as_ref().is_ok_and(|composed| composed.report.is_valid()),
            "{}",
            what
        );
        assert_eq!(outcome(composed), expected, "{}", what);
    }
}

#[test]
fn the_installer_core_rejects_a_second_aggregation_even_where_placement_would_run_it() {
    let rejection = format!(
        "the plan is not valid: error[single_aggregation] node `policy/late_agg`: {}",
        second_one("policy/late_agg")
    );
    for (from, to) in [
        ("dsd_no_agg_split.passthrough", "dsd_post_agg_filter_in.1"),
        ("metrics.out", "metrics_fanout.in"),
    ] {
        let mut host = Host::new();
        let performed = host.deliver(1, second_aggregation("late_agg", from, to));
        assert_eq!(performed, [format!("candidate 2: rejected ({})", rejection)]);
        assert_eq!(
            host.demo(),
            (crate::channel::ApplyState::Rejected, None, Some(rejection.clone()))
        );
        assert_eq!(host.core.status().live, vec![GenerationId::new(1)]);
        assert!(host.arming.is_none());
    }
}

#[test]
fn the_one_aggregation_fed_by_the_otlp_input_has_no_isolation_slot_in_the_boot_frame() {
    // OTLP's records reach the enrichment region, whose region has no exit to an isolation slot: the one aggregation
    // cannot be fed by them, whether it is fed by them as well or moved to their path. A second aggregation there is
    // the validation rule's (above).

    // The one aggregation fed by OTLP too, through a merge ahead of it.
    let merged = composed(edited(|document| {
        add_nodes(
            document,
            &[json!({"id": "agg_in", "op": "merge", "params": {"inputs": 2}})],
        );
        replace_edge(
            document,
            "dsd_tag_filterlist.out",
            "dsd_agg.in",
            &[("dsd_tag_filterlist.out", "agg_in.0"), ("agg_in.out", "dsd_agg.in")],
        );
        replace_edge(document, "input.otlp", "metrics_in.1", &[("input.otlp", "agg_in.1")]);
        // The merge ahead of enrichment loses its OTLP port.
        node_mut(document, "metrics_in")["params"]["inputs"] = json!(4);
        for edge in document["edges"].as_array_mut().unwrap() {
            for (old, new) in [
                ("metrics_in.2", "metrics_in.1"),
                ("metrics_in.3", "metrics_in.2"),
                ("metrics_in.4", "metrics_in.3"),
            ] {
                if edge["to"] == old {
                    edge["to"] = json!(new);
                }
            }
        }
    }));
    assert!(merged.as_ref().is_ok_and(|composed| composed.report.is_valid()));
    assert_eq!(
        outcome(merged),
        "unplaceable [FrameChanges]: The candidate plan cannot run in the physical structure built at boot: node \
         `policy/agg_in`: it joins records from the regions `dsd_no_agg_split_region`, `metrics_enrich`, and no one of \
         them receives from all the others: connecting regions in a new way requires stage 10 frame changes: the \
         physical structure is built from the default program at boot, so a restart does not add it."
    );

    // The one aggregation moved to OTLP's path alone.
    let moved = composed(edited(|document| {
        replace_edge(
            document,
            "dsd_tag_filterlist.out",
            "dsd_agg.in",
            &[("dsd_tag_filterlist.out", "dsd_post_agg_filter_in.0")],
        );
        replace_edge(document, "dsd_agg.out", "dsd_post_agg_filter_in.0", &[]);
        replace_edge(
            document,
            "input.otlp",
            "metrics_in.1",
            &[("input.otlp", "dsd_agg.in"), ("dsd_agg.out", "metrics_in.1")],
        );
    }));
    assert!(moved.as_ref().is_ok_and(|composed| composed.report.is_valid()));
    assert_eq!(outcome(moved), no_isolation_slot("dsd_agg", "metrics_enrich"));
}

// What does nothing as configured: the detached export leaves it out; published over the default program, it is a
// generation that builds no kernel.

/// The default-program fixture with nothing configured for DogStatsD processing: no mapper profiles, no namespace, an
/// empty metric filterlist, no tag rules, and no histogram statistic filterlisted.
fn unconfigured_plan() -> Plan {
    unified_with(|value| {
        let mut resolve = |node: &str, path: &[&str], resolved: Value| {
            let mut at = &mut node_mut(value, node)["params"];
            for step in path {
                at = match step.parse::<usize>() {
                    Ok(index) => &mut at[index],
                    Err(_) => &mut at[*step],
                };
            }
            at["resolved"] = resolved;
        };
        resolve("dsd_enrich", &["profiles"], json!([]));
        resolve(
            "dsd_prefix_filter_blocklist_namespace_prefix",
            &["namespace"],
            json!(""),
        );
        resolve(
            "dsd_prefix_filter_blocklist",
            &["predicate", "not", "intake_name", "matches", "values"],
            json!([]),
        );
        resolve("dsd_tag_filterlist", &["rules"], json!([]));
        resolve("dsd_tag_filterlist", &["value_rules"], json!([]));
        resolve(
            "dsd_post_agg_filter",
            &["predicate", "not", "all", "1", "intake_name", "matches", "values"],
            json!([]),
        );
    })
}

/// The detached export of the one scope of `plan`, as an artifact document, and what it leaves out.
fn detached(plan: &Plan) -> (Value, Vec<String>) {
    let bundle = crate::export::export_as(
        plan,
        &scope(),
        &unified_catalog(plan),
        &BTreeMap::new(),
        crate::export::ExportForm::Detached,
        &registry(),
    )
    .unwrap();
    let omitted = bundle.omitted.keys().map(ToString::to_string).collect();
    (serde_json::to_value(&bundle.artifact).unwrap(), omitted)
}

/// Puts the metric filterlist back into a detached export that left it out, between the scope's DogStatsD input and the
/// timestamp split, matching `values` exactly.
fn with_filterlist(document: &mut Value, values: &[&str]) {
    add_nodes(
        document,
        &[
            json!({"id": "dsd_prefix_filter_blocklist", "op": "split",
                   "params": {"predicate": {"not": {"intake_name": {"matches": {
                       "values": values, "prefix": false}}}}}}),
            json!({"id": "dsd_prefix_filter_blocklist_false", "op": "drop"}),
        ],
    );
    replace_edge(
        document,
        "input.dogstatsd",
        "dsd_no_agg_split.in",
        &[
            ("input.dogstatsd", "dsd_prefix_filter_blocklist.in"),
            (
                "dsd_prefix_filter_blocklist.false",
                "dsd_prefix_filter_blocklist_false.in",
            ),
            ("dsd_prefix_filter_blocklist.true", "dsd_no_agg_split.in"),
        ],
    );
}

#[test]
fn the_shorter_detached_export_over_the_default_program_is_a_commit_that_builds_no_kernel() {
    let plan = unconfigured_plan();
    let mut host = Host::with(plan.clone(), InstallerConfig::default());
    let (document, omitted) = detached(&plan);
    assert_eq!(
        omitted,
        [
            "dsd_enrich",
            "dsd_post_agg_filter",
            "dsd_post_agg_filter_false",
            "dsd_prefix_filter_blocklist",
            "dsd_prefix_filter_blocklist_false",
            "dsd_prefix_filter_blocklist_namespace_prefix",
            "dsd_tag_filterlist",
            "metrics_host_enrichment",
        ]
    );

    // The program does what the default program does, but without the nodes that do nothing: the plans differ as
    // written, so it is a generation. Aggregation keeps its lineage (`retain`), so it is a commit, not a cut, and no
    // kernel is built: every slot that ran a left-out node gets a program without it, whose kernels it already has.
    let composed = composed(document.clone()).unwrap();
    let changes = super::diff::diff(&plan, &composed.plan, &registry());
    assert_eq!(
        changes
            .state_owners
            .iter()
            .map(|owner| (owner.node.to_string(), owner.class))
            .collect::<Vec<_>>(),
        [("dsd_agg".to_string(), TransitionClass::Retain)]
    );
    assert!(changes.cut().is_none());
    let performed = host.deliver(1, document.clone());
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                2,
                "0:changed reuse 1 build 0, 1:changed reuse 1 build 0, 2:changed reuse 0 build 0, 3:changed reuse 1 \
                 build 0"
            ),
            "candidate 2 generation 2: pending"
        ])
    );
    let performed = host.acknowledge();
    assert_eq!(
        performed[..2],
        strings(&["commit 2", "candidate 2 generation 2: active"])
    );
    assert!(host.arming.is_none() && host.core.cut_in_flight().is_none());
    // The plan running is the program as published: the nodes it leaves out are gone.
    assert!(host.active_plan().node(&id("dsd_prefix_filter_blocklist")).is_none());

    // An edit of it changes only the program of the slot it edits: putting the filterlist back with an entry.
    let mut filtered = document.clone();
    with_filterlist(&mut filtered, &["tenant.raw.blocked"]);
    let performed = host.deliver(2, filtered);
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                3,
                "0:changed reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:unchanged reuse 0 build 0, 3:unchanged \
                 reuse 1 build 0"
            ),
            "candidate 3 generation 3: pending"
        ])
    );
    host.acknowledge();

    // The program published again, without the filterlist, is a generation again; then the same program once more
    // adopts the generation that runs it.
    let performed = host.deliver(3, document.clone());
    assert_eq!(
        performed,
        strings(&[
            &prepare(
                4,
                "0:changed reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:unchanged reuse 0 build 0, 3:unchanged \
                 reuse 1 build 0"
            ),
            "candidate 4 generation 4: pending"
        ])
    );
    host.acknowledge();
    // The same program again, its edges in another order (an identical document is the same delivery): it composes to
    // the plan generation 4 runs, which it adopts, with nothing prepared.
    let mut reordered = document;
    reordered["edges"].as_array_mut().unwrap().reverse();
    let performed = host.deliver(4, reordered.clone());
    let origin = Origin::Artifact {
        revision: 4,
        hash: received(reordered).hash,
    };
    assert_eq!(
        performed,
        strings(&["candidate 5: unchanged", &format!("adopt 4 as {}", origin)])
    );
}

#[test]
fn the_default_program_takes_back_only_a_generation_that_runs_every_node_it_has() {
    let plan = unconfigured_plan();
    let mut host = Host::with(plan.clone(), InstallerConfig::default());
    let (document, _) = detached(&plan);
    host.deliver(1, document);
    host.acknowledge();
    assert_eq!(host.schedule.id(), GenerationId::new(2));
    // Revoked, the default program, which runs every node it has, gets a generation of its own: the one running does
    // what it does with every record, but leaves its nodes out.
    let performed = host.revoke();
    assert!(
        performed.iter().any(|action| action.starts_with("prepare 3 ")),
        "{:?}",
        performed
    );
    assert!(
        !performed.iter().any(|action| action.starts_with("adopt")),
        "{:?}",
        performed
    );
}

#[test]
fn a_published_program_without_a_node_that_does_nothing_follows_no_setting_until_the_node_does_something() {
    let registry = registry();
    let plan = unconfigured_plan();
    let catalog = unified_catalog(&plan);
    let (document, _) = detached(&plan);
    let composed = compose_all(
        &plan,
        &[(&received(document.clone()), &approval(1))],
        &catalog,
        &registry,
    )
    .unwrap();
    assert!(crate::export::policy_differences(&plan, &composed.plan, &catalog, &registry).is_empty());

    // The filterlist set natively: the program, which has none, no longer does what the default program does, and
    // its entries differ, not its match mode, which alone would not make it do nothing again.
    let changed = unified_with(|value| {
        *value = serde_json::to_value(&plan).unwrap();
        node_mut(value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["not"]["intake_name"]["matches"]
            ["values"]["resolved"] = json!(["tenant.raw.blocked"]);
    });
    let changed_catalog = unified_catalog(&changed);
    let recomposed = compose_all(
        &changed,
        &[(&received(document), &approval(1))],
        &changed_catalog,
        &registry,
    )
    .unwrap();
    assert_eq!(
        crate::export::policy_differences(&changed, &recomposed.plan, &changed_catalog, &registry),
        ["dogstatsd.metric_filter.values"
            .parse::<crate::ir::BindingName>()
            .unwrap()]
    );
}
