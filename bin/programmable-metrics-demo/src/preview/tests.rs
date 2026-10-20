//! Composition, validation, and the plan diff, through `compile`, against the plan crate's default program fixture
//! and its export: the real thing ADP serves, with real hashes.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    canonical::{BINDING_MEMBER, RESOLVED_MEMBER},
    export::export,
    ir::BindingName,
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    validate::{Catalog as _, CatalogBinding, MemoryCatalog},
};
use serde_json::Value;

use super::*;
use crate::compile::{compile, default_program_source, CompileOutcome};

/// Returns the host's catalog of a default program: the fixtures' catalog, with every binding the program resolves
/// carrying the value it resolves to, as ADP's catalog carries the values lowering writes.
fn catalog_for(plan: &Plan) -> MemoryCatalog {
    fn collect(value: &Value, found: &mut BTreeMap<BindingName, Value>) {
        match value {
            Value::Object(members) => {
                if let (Some(Value::String(name)), Some(resolved)) =
                    (members.get(BINDING_MEMBER), members.get(RESOLVED_MEMBER))
                {
                    found.insert(name.parse().unwrap(), resolved.clone());
                }
                members.values().for_each(|member| collect(member, found));
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, found)),
            _ => {}
        }
    }

    let mut found = BTreeMap::new();
    for node in &plan.nodes {
        node.params.iter().for_each(|(_, value)| collect(value, &mut found));
    }
    let mut catalog = default_program_catalog();
    for (name, value) in found {
        let declared = catalog
            .binding(&name)
            .expect("the catalog declares every binding")
            .clone();
        catalog = catalog.with_catalog_binding(
            name,
            CatalogBinding {
                value: Some(value),
                ..declared
            },
        );
    }
    catalog
}

/// The plan crate's default program, and its export bundle document as ADP serves it.
pub(crate) fn fixture_documents() -> (Plan, Value) {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let bundle = export(&plan, &catalog_for(&plan), &BTreeMap::new()).unwrap();
    (plan, serde_json::to_value(bundle).unwrap())
}

/// The plan crate's default program, and its export as the demo reads it.
pub(crate) fn fixture() -> (Plan, ExportBundle) {
    let (plan, document) = fixture_documents();
    (plan, ExportBundle::from_value(document).unwrap())
}

/// Returns the core plan of the default program `plan`, as ADP elaborates it with the export's catalog: the plan it
/// installs.
pub(crate) fn core(plan: &Plan, export: &ExportBundle) -> Plan {
    saluki_metric_plan::elaborate::elaborate_default(plan, &export.catalog, &OperatorRegistry::native()).plan
}

/// Compiles `source` against the fixture, with the default program active (generation 3) and revision 7 next.
fn compile_against(source: &str, plan: &Plan, export: &ExportBundle) -> CompileOutcome {
    let active = core(plan, export);
    let context = CompileContext {
        export,
        default_program: Some(plan),
        active: Some(ActivePlan {
            plan: &active,
            hash: active.hash().unwrap(),
            generation: Some(3),
            artifact_revision: None,
        }),
        revision: 7,
        labels: &crate::compile::RecipientLabels::default(),
        in_effect: None,
    };
    compile(source, Some(&context)).0
}

/// Returns the line `text` is on in `source`, counting from 1.
fn line_of(source: &str, text: &str) -> usize {
    let at = source
        .find(text)
        .unwrap_or_else(|| panic!("{:?} is not in the source", text));
    source[..at].matches('\n').count() + 1
}

/// Puts `stage` at the end of the stream every destination reads, as the S1 scenario does.
fn with_stage_before_policy(source: &str, stage: &str) -> String {
    crate::scenario::edits::append_to_metrics(source, stage).unwrap()
}

#[test]
fn an_unmodified_export_composes_to_the_active_default_program_with_an_empty_diff() {
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    let outcome = compile_against(&source, &plan, &export);
    assert!(outcome.ok);
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(outcome.checked_against.composed);
    let preview = &outcome.preview;
    assert!(
        preview.available && preview.valid && preview.unchanged,
        "{:#?}",
        preview
    );
    assert!(preview.nodes.is_empty() && preview.edges_added.is_empty() && preview.edges_removed.is_empty());
    // The composed plan is the default program's core plan, with its hash, so ADP adopts the running generation.
    let hash = core(&plan, &export).hash().unwrap().to_string();
    assert_eq!(preview.plan_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(
        preview.against,
        Some(PreviewBase {
            kind: BaseKind::Active,
            plan_hash: hash.clone(),
            generation: Some(3),
            artifact_revision: None,
        })
    );

    // Without the active plan, the diff compares with the default program, and says so.
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        revision: 1,
        labels: &crate::compile::RecipientLabels::default(),
        in_effect: None,
    };
    let (outcome, _) = compile(&source, Some(&context));
    assert!(outcome.preview.unchanged);
    assert_eq!(outcome.preview.against.unwrap().kind, BaseKind::DefaultProgram);
    assert!(outcome.preview.reason.contains("was not available"));
}

#[test]
fn a_change_diffs_node_by_node_in_the_artifacts_terms() {
    let (plan, export) = fixture();
    // S1: drop the debug metrics at the end of `let metrics`.
    let source = crate::scenario::edits::drop_debug(&default_program_source(
        &export,
        &crate::compile::RecipientLabels::default(),
    ))
    .unwrap();
    let outcome = compile_against(&source, &plan, &export);
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    let preview = &outcome.preview;
    assert!(preview.available && preview.valid && !preview.unchanged);
    assert_ne!(
        preview.plan_hash,
        preview.against.as_ref().map(|against| against.plan_hash.clone())
    );

    let found = preview
        .nodes
        .iter()
        .map(|node| {
            (
                node.node.as_str(),
                node.artifact_node.as_deref(),
                node.op.as_str(),
                node.change,
                node.aspects.clone(),
            )
        })
        .collect::<Vec<_>>();
    // The `where` ends `let metrics` now, so it is the node `metrics`, which changes operator, and the host tags node
    // before it is the artifact's `metrics_host_tags` (a `policy/` ID); the inline drop is the artifact's too: a `where`
    // drops what its `false` port leads to, which normalization keeps whichever way the predicate is written. The merge
    // feeds host tags, and the `tee` is fed by the split: both are the default program's nodes, with other edges.
    // Nothing else changes: a node is what it does, whatever feeds it, so the branches behind the `tee` are unchanged.
    let expected = vec![
        (
            "metrics_in",
            Some("metrics_in"),
            "merge",
            ChangeKind::Changed,
            vec!["edges"],
        ),
        (
            "policy/metrics_host_tags",
            Some("metrics_host_tags"),
            "host_tags",
            ChangeKind::Added,
            vec![],
        ),
        (
            "metrics",
            Some("metrics"),
            "split",
            ChangeKind::Changed,
            vec!["operator", "params", "edges"],
        ),
        (
            "metrics_fanout",
            Some("metrics_fanout"),
            "tee",
            ChangeKind::Changed,
            vec!["edges"],
        ),
        (
            "policy/metrics_false",
            Some("metrics_false"),
            "drop",
            ChangeKind::Added,
            vec![],
        ),
    ];
    assert_eq!(found, expected);
    assert_eq!(
        preview.edges_added,
        [
            "metrics_in.out -> policy/metrics_host_tags.in",
            "policy/metrics_host_tags.out -> metrics.in",
            "metrics.true -> metrics_fanout.in",
            "metrics.false -> policy/metrics_false.in",
        ]
    );
    assert_eq!(
        preview.edges_removed,
        ["metrics_in.out -> metrics.in", "metrics.out -> metrics_fanout.in"]
    );
    assert!(preview.outputs.is_empty());

    // Artifact nodes are placed in the source: the split and the drop it implies at its `where`, host tags at its
    // stage.
    let stage = line_of(&source, crate::scenario::edits::DROP_DEBUG);
    assert_eq!(preview.nodes[2].range.unwrap().line, stage);
    assert_eq!(preview.nodes[4].range.unwrap().line, stage);
    assert_eq!(preview.nodes[1].range.unwrap().line, line_of(&source, "| host_tags("));
    // So is a node of the default program that the artifact keeps: the merge, at its `let`.
    assert_eq!(
        preview.nodes[0].range.unwrap().line,
        line_of(&source, "  let metrics = ")
    );
}

#[test]
fn validation_findings_are_placed_at_the_artifact_nodes_they_are_about() {
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    // A second branch of the policy to the primary output: every record reaches it twice.
    let duplicate = source.replacen(
        "  metrics | emit output dd_metrics_encode\n",
        "  metrics | emit output dd_metrics_encode\n\n  metrics | emit output dd_metrics_encode\n",
        1,
    );
    let outcome = compile_against(&duplicate, &plan, &export);
    // Publishable all the same: ADP rejects it, which is what the demo shows.
    assert!(outcome.ok);
    let errors = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{:#?}", outcome.diagnostics);
    assert_eq!(errors[0].origin, Origin::Validation);
    assert_eq!(errors[0].rule, Some("duplicate_contribution"));
    assert_eq!(errors[0].range.line, line_of(&duplicate, "  let metrics = "));
    // The plan diff is still shown, of a plan that does not validate.
    assert!(outcome.preview.available && !outcome.preview.valid);
    assert_eq!(outcome.errors, 1);

    // A parameter the operator does not have is placed at the parameter.
    let unknown_parameter = with_stage_before_policy(
        &source,
        "namespace_prefix(\"demo\", exclusions: [], colour: \"red\") as extra",
    );
    let outcome = compile_against(&unknown_parameter, &plan, &export);
    let parameter = outcome
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.rule == Some("valid_parameters"))
        .unwrap_or_else(|| panic!("{:#?}", outcome.diagnostics));
    assert!(
        unknown_parameter[parameter.range.start..parameter.range.end].contains("red"),
        "{:?}",
        &unknown_parameter[parameter.range.start..parameter.range.end]
    );
}

#[test]
fn an_aggregation_composes_and_validation_places_its_findings_at_its_node() {
    // A state owner is no composition error: ADP composes it, and validation, then placement, judge it. The default
    // program already has its one aggregation, so a second is refused, at its stage.
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    let source = with_stage_before_policy(&source, "aggregate(window: 10s) as rollup");
    let outcome = compile_against(&source, &plan, &export);
    assert!(outcome.ok);
    assert!(!outcome.diagnostics.is_empty());
    for diagnostic in &outcome.diagnostics {
        assert_eq!(
            (diagnostic.severity, diagnostic.origin),
            (Severity::Error, Origin::Validation),
            "{:#?}",
            diagnostic
        );
        assert_eq!(
            diagnostic.range.line,
            line_of(&source, "as rollup"),
            "{:#?}",
            diagnostic
        );
    }
    assert!(outcome
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.rule == Some("single_aggregation")));
    assert!(outcome.preview.available && !outcome.preview.valid);
}

#[test]
fn composition_errors_are_one_finding_at_the_node_or_output_and_no_diff() {
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    let cases = [(
        "an emit to an output ADP does not declare",
        source.replacen(
            "emit output metrics_routing_encode_1\n",
            "emit output metrics_routing_encode_99\n",
            1,
        ),
        "metrics_routing_encode_99",
        "emits to output `metrics_routing_encode_99`, which the host does not declare. Emit to one of the outputs \
             it declares: `",
    )];
    for (case, source, place, fragment) in cases {
        let outcome = compile_against(&source, &plan, &export);
        assert!(outcome.ok, "{}", case);
        assert_eq!(outcome.diagnostics.len(), 1, "{}: {:#?}", case, outcome.diagnostics);
        let diagnostic = &outcome.diagnostics[0];
        assert_eq!(
            (diagnostic.severity, diagnostic.origin, diagnostic.rule),
            (Severity::Error, Origin::Composition, None),
            "{}",
            case
        );
        assert!(
            diagnostic.message.contains(fragment),
            "{}: {}",
            case,
            diagnostic.message
        );
        assert_eq!(diagnostic.range.line, line_of(&source, place), "{}", case);
        assert!(!outcome.preview.available, "{}", case);
        assert!(outcome.preview.reason.contains("cannot be composed"), "{}", case);
    }
}

/// The plan crate's default program, and its detached export, as ADP serves it by default.
pub(crate) fn detached_fixture() -> (Plan, ExportBundle) {
    detached_fixture_with(|catalog| catalog)
}

/// The plan crate's default program, and its detached export as the demo reads it, from the host's catalog as `host`
/// changes it, such as with a recipient directory.
pub(crate) fn detached_fixture_with(host: impl FnOnce(MemoryCatalog) -> MemoryCatalog) -> (Plan, ExportBundle) {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let bundle = saluki_metric_plan::export::export_as(
        &plan,
        &host(catalog_for(&plan)),
        &BTreeMap::new(),
        saluki_metric_plan::export::ExportForm::Detached,
        &saluki_metric_plan::operators::OperatorRegistry::native(),
    )
    .unwrap();
    (
        plan,
        ExportBundle::from_value(serde_json::to_value(bundle).unwrap()).unwrap(),
    )
}

#[test]
fn the_detached_export_shows_no_change_in_what_it_does_and_a_note_that_adp_drops_what_does_nothing() {
    // The fixture's detached export leaves out host enrichment, which changes no metric.
    let (plan, export) = detached_fixture();
    let source = crate::compile::default_program_source(&export, &crate::compile::RecipientLabels::default());
    let outcome = compile_against(&source, &plan, &export);
    assert!(
        outcome.preview.available && outcome.preview.valid,
        "{:?}",
        outcome.preview
    );
    assert!(outcome.preview.unchanged, "{:?}", outcome.preview.nodes);
    assert!(
        outcome.preview.notes.iter().any(|note| note
            == "ADP will drop 1 node that does nothing as configured, which the program leaves out; that builds no \
                kernel."),
        "{:?}",
        outcome.preview.notes
    );
}

/// Returns `plan` with the node `from` called `to`, and its edges with it.
fn with_node_renamed(plan: &Plan, from: &str, to: NodeId) -> Plan {
    let from = from.parse::<NodeId>().unwrap();
    assert!(plan.node(&from).is_some(), "the plan has no node `{}`", from);
    let mut plan = plan.clone();
    for node in plan.nodes.iter_mut().filter(|node| node.id == from) {
        node.id = to.clone();
    }
    for edge in &mut plan.edges {
        for end in [&mut edge.from, &mut edge.to] {
            if end.node == from {
                end.node = to.clone();
            }
        }
    }
    plan
}

/// Returns the plan ID of an artifact node that no node of the default program has.
fn policy_id(local: &str) -> NodeId {
    NodeId::policy(saluki_metric_plan::ir::LocalId::new(local).unwrap())
}

#[test]
fn renaming_a_state_owner_is_a_note_that_says_how_to_keep_its_id() {
    let (plan, _) = fixture();
    let registry = OperatorRegistry::native();
    let renamed = with_node_renamed(&plan, "dsd_agg", policy_id("agg"));
    assert_eq!(
        renamed_state_owners(&plan, &renamed, &registry, |_| None),
        [
            "This edit renames the aggregation from `dsd_agg` to `agg`, so its state will not carry over; keep its ID \
          by naming it with `let dsd_agg = ...`."
        ]
    );
    // The new node is named as the artifact writes it, when the correspondence knows it.
    let named = renamed_state_owners(&plan, &renamed, &registry, |node| {
        (node == &policy_id("agg")).then(|| "rollup".to_string())
    });
    assert_eq!(
        named,
        [
            "This edit renames the aggregation from `dsd_agg` to `rollup`, so its state will not carry over; keep its \
          ID by naming it with `let dsd_agg = ...`."
        ]
    );
}

#[test]
fn no_note_without_a_renamed_state_owner() {
    let (plan, _) = fixture();
    let registry = OperatorRegistry::native();
    // Unchanged.
    assert!(renamed_state_owners(&plan, &plan, &registry, |_| None).is_empty());
    // A node without state renamed: there is nothing to carry over.
    let renamed = with_node_renamed(&plan, "metrics_fanout", policy_id("policy_copies"));
    assert!(renamed_state_owners(&plan, &renamed, &registry, |_| None).is_empty());
    // The aggregation removed, with none in its place.
    let mut removed = plan.clone();
    removed.nodes.retain(|node| node.id.to_string() != "dsd_agg");
    assert!(renamed_state_owners(&plan, &removed, &registry, |_| None).is_empty());
    // A second aggregation beside the first.
    let mut added = plan.clone();
    let mut second = plan.node(&"dsd_agg".parse().unwrap()).unwrap().clone();
    second.id = policy_id("rollup");
    added.nodes.push(second);
    assert!(renamed_state_owners(&plan, &added, &registry, |_| None).is_empty());
}

#[test]
fn renaming_the_one_aggregation_of_a_program_keeps_its_id_and_gets_no_note() {
    // Composition gives a program's one aggregation the default program's ID whatever the artifact calls it, so its
    // state carries over. The artifact is the export's, renamed directly, without the syntax.
    let (plan, export) = detached_fixture();
    let document = serde_json::to_string(&export.artifact.artifact).unwrap();
    let renamed = document
        .replace("\"dsd_agg\"", "\"agg\"")
        .replace("\"dsd_agg.", "\"agg.");
    assert_ne!(renamed, document);
    let received = saluki_metric_plan::compose::receive_artifact(serde_json::from_str(&renamed).unwrap()).unwrap();
    let parsed = Parsed {
        artifact: received.artifact.clone(),
        spans: Default::default(),
    };
    let labels = crate::compile::RecipientLabels::default();
    let active = core(&plan, &export);
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: Some(ActivePlan {
            plan: &active,
            hash: active.hash().unwrap(),
            generation: Some(3),
            artifact_revision: None,
        }),
        revision: 7,
        labels: &labels,
        in_effect: None,
    };
    let (findings, preview) = compose_preview(&parsed, &received, &context, &plan, &LineIndex::new(""));
    assert!(preview.available, "{:?}\n{:?}", preview, findings);
    assert!(
        !preview.notes.iter().any(|note| note.contains("renames")),
        "{:?}",
        preview.notes
    );
}

#[test]
fn nodes_normalization_renames_are_placed_where_they_are_written() {
    // A `split` that drops on `true` is normalized to one that drops on `false`, which swaps its terminals' derived
    // IDs: what composition calls `s_true` is the `emit` written on `s.false`, and `s_false` is the `drop`.
    let source = r#"policy {
  let s = dogstatsd | split(name == "a")

  s.true | drop

  s.false | emit primary
}
"#;
    let parsed = saluki_metric_plan_syntax::parse(source).unwrap();
    let written = super::WrittenIds::of(&parsed);
    let line = |id: &str| {
        let span = written.node(&parsed, id).unwrap();
        source[..span.start].matches('\n').count() + 1
    };
    assert_eq!(line("s_true"), line_of(source, "s.false | emit primary"));
    assert_eq!(line("s_false"), line_of(source, "s.true | drop"));
    assert_eq!(line("s"), line_of(source, "let s"));
}
