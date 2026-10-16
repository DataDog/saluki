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
use crate::{
    compile::{compile, default_program_source, CompileOutcome},
    stub::protocol::{ActiveArtifact, ScopeState},
};

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
    let bundle = export(
        &plan,
        &crate::stub::protocol::POLICY_SCOPE.parse().unwrap(),
        &catalog_for(&plan),
        &BTreeMap::new(),
    )
    .unwrap();
    (plan, serde_json::to_value(bundle).unwrap())
}

/// The plan crate's default program, and its export as the demo reads it.
pub(crate) fn fixture() -> (Plan, ExportBundle) {
    let (plan, document) = fixture_documents();
    (plan, ExportBundle::from_value(document).unwrap())
}

/// The plan crate's default program with the two scopes it declared through stage 9, and the export of its policy
/// scope, `post_enrich`, as the demo reads it: for the tests of artifacts of several scopes, which the preview composes
/// as ADP's installer does, though ADP declares one scope today.
pub(crate) fn two_scope_fixture() -> (Plan, ExportBundle) {
    let plan = Plan::from_value(saluki_metric_plan::testing::with_two_scopes(
        serde_json::from_str(DEFAULT_PROGRAM_JSON).unwrap(),
    ))
    .unwrap();
    let catalog = saluki_metric_plan::testing::two_scopes_of(catalog_for(&plan));
    let bundle = export(&plan, &"post_enrich".parse().unwrap(), &catalog, &BTreeMap::new()).unwrap();
    let bundle = ExportBundle::from_value(serde_json::to_value(bundle).unwrap()).unwrap();
    (plan, bundle)
}

/// Compiles `source` against the fixture, with the default program active (generation 3) and revision 7 next.
fn compile_against(source: &str, plan: &Plan, export: &ExportBundle) -> CompileOutcome {
    let context = CompileContext {
        export,
        default_program: Some(plan),
        active: Some(ActivePlan {
            plan,
            hash: plan.hash(ResolvedValues::Include).unwrap(),
            generation: Some(3),
            artifact_revision: None,
        }),
        others: &[],
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

/// Puts `stage` between enrichment and the policy `fork`, as the S1 scenario does.
fn with_stage_before_policy(source: &str, stage: &str) -> String {
    crate::scenario::edits::insert_before_policy(source, stage).unwrap()
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
    assert!(!preview.stale_base);
    // The composed plan is the default program itself: the hash ADP reports active after installing it is the hash
    // it already reports.
    let hash = plan.hash(ResolvedValues::Include).unwrap().to_string();
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
        others: &[],
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
    // S1: drop the debug metrics in front of the policy `fork`.
    let source = with_stage_before_policy(
        &default_program_source(&export, &crate::compile::RecipientLabels::default()),
        "where not (name starts_with \"demo.debug.\") as drop_debug",
    );
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
    // The new split and its inline drop are the artifact's (`policy/` IDs): a `where` drops what its `false` port
    // leads to, which normalization keeps whichever way the predicate is written. The host tags node, which fed the
    // `tee`,
    // now feeds the split, and the `tee` is fed by it: both are the default program's nodes, with other edges. Nothing
    // else changes: a node is what it does, whatever feeds it, so the routing drops behind the `tee` are unchanged.
    let expected = vec![
        (
            "metrics_enrich_host_tags",
            Some("metrics_enrich_host_tags"),
            "host_tags",
            ChangeKind::Changed,
            vec!["edges"],
        ),
        (
            "policy/drop_debug",
            Some("drop_debug"),
            "split",
            ChangeKind::Added,
            vec![],
        ),
        (
            "metrics_policy",
            Some("metrics_policy"),
            "tee",
            ChangeKind::Changed,
            vec!["edges"],
        ),
        (
            "policy/drop_debug_false",
            Some("drop_debug_false"),
            "drop",
            ChangeKind::Added,
            vec![],
        ),
    ];
    assert_eq!(found, expected);
    assert_eq!(
        preview.edges_added,
        [
            "metrics_enrich_host_tags.out -> policy/drop_debug.in",
            "policy/drop_debug.true -> metrics_policy.in",
            "policy/drop_debug.false -> policy/drop_debug_false.in",
        ]
    );
    assert_eq!(
        preview.edges_removed,
        ["metrics_enrich_host_tags.out -> metrics_policy.in"]
    );
    assert!(preview.outputs.is_empty());

    // Artifact nodes are placed in the source: the new split and the drop it implies at its `where`.
    let split = &preview.nodes[1];
    assert_eq!(split.range.unwrap().line, line_of(&source, "as drop_debug"));
    let drop = &preview.nodes[3];
    assert_eq!(drop.range.unwrap().line, line_of(&source, "as drop_debug"));
    // So is a node of the default program that the artifact keeps: the host tags node, at its stage.
    assert_eq!(preview.nodes[0].range.unwrap().line, line_of(&source, "| host_tags("));
}

#[test]
fn validation_findings_are_placed_at_the_artifact_nodes_they_are_about() {
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    // A second branch of the policy `fork` to the primary output: every record reaches it twice.
    let duplicate = source.replacen(
        "      emit output dd_metrics_encode\n",
        "      emit output dd_metrics_encode\n      emit output dd_metrics_encode\n",
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
    assert_eq!(errors[0].range.line, line_of(&duplicate, "| fork as metrics_policy"));
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

/// A pre-aggregation artifact that drops every DogStatsD metric before aggregation, under the default program's base.
fn pre_aggregation_document(plan: &Plan) -> Value {
    let base = saluki_metric_plan::export::base(plan, &PRE_AGGREGATION.parse().unwrap()).unwrap();
    serde_json::json!({
        "ir_version": 1,
        "scope": PRE_AGGREGATION,
        "base": base,
        "nodes": [{"id": "none", "op": "drop"}],
        "edges": [{"from": "input.metrics", "to": "none.in"}]
    })
}

const PRE_AGGREGATION: &str = "dogstatsd_pre_aggregation";

#[test]
fn an_artifact_composes_with_the_artifacts_adp_runs_for_the_other_scopes() {
    let (plan, export) = two_scope_fixture();
    let registry = OperatorRegistry::native();
    // ADP runs a pre-aggregation artifact, revision 5: its active plan composes it.
    let other = ScopeArtifact {
        artifact: receive_artifact(pre_aggregation_document(&plan)).unwrap(),
        revision: 5,
    };
    let approval = Approval {
        revision: 5,
        approved_against: None,
        active_plan_hash: None,
        in_effect: true,
    };
    let active = compose_all(&plan, &[(&other.artifact, &approval)], &export.catalog, &registry)
        .unwrap()
        .plan;
    let hash = active.hash(ResolvedValues::Include).unwrap();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: Some(ActivePlan {
            plan: &active,
            hash,
            generation: Some(4),
            artifact_revision: Some(5),
        }),
        others: std::slice::from_ref(&other),
        revision: 6,
        labels: &crate::compile::RecipientLabels::default(),
        in_effect: None,
    };

    // The unmodified policy scope export changes nothing: the pre-aggregation artifact stays, and the plan hash is the
    // one ADP reports once it installs the edited artifact.
    let (outcome, _) = compile(&source, Some(&context));
    let preview = &outcome.preview;
    assert!(preview.available && preview.unchanged, "{:#?}", preview);
    assert_eq!(preview.plan_hash, Some(hash.to_string()));
    assert!(
        preview
            .reason
            .contains("with the artifacts ADP runs for `dogstatsd_pre_aggregation` (revision 5)"),
        "{}",
        preview.reason
    );
    assert!(preview.reason.contains("not placed"), "{}", preview.reason);

    // Composed alone, the edited artifact would revert the pre-aggregation scope to the default program's region.
    let alone = CompileContext { others: &[], ..context };
    let (outcome, _) = compile(&source, Some(&alone));
    assert!(!outcome.preview.unchanged);
    assert_eq!(
        outcome.preview.plan_hash,
        Some(plan.hash(ResolvedValues::Include).unwrap().to_string())
    );
}

#[test]
fn the_artifacts_adp_runs_for_the_other_scopes_are_read_from_the_configs_the_stub_serves() {
    // The policy scope's artifact, and one of another scope, as of the two scopes of stage 9.
    let (plan, _) = two_scope_fixture();
    let (_, export) = fixture();
    let pre = pre_aggregation_document(&plan);
    let post = serde_json::to_value(&export.artifact.artifact).unwrap();
    let policy = crate::stub::protocol::POLICY_SCOPE;
    let hash = |document: &Value| ReceivedArtifact::from_value(document.clone()).unwrap().hash.to_string();
    let envelope = |revision, document: &Value| Envelope {
        revision,
        artifact_hash: hash(document),
        approved_against: None,
        artifact: document.clone(),
    };
    let active = |id: &str, revision, document: &Value| ActiveArtifact {
        id: id.to_string(),
        revision: Some(revision),
        artifact_hash: Some(hash(document)),
    };
    let served = vec![
        (PRE_AGGREGATION.to_string(), envelope(5, &pre)),
        (policy.to_string(), envelope(6, &post)),
    ];
    let found = |status: &ChannelStatus, scope: &str| {
        other_scope_artifacts(status, &served, scope)
            .iter()
            .map(|other| (other.artifact.artifact.scope.to_string(), other.revision))
            .collect::<Vec<_>>()
    };

    // ADP names every scope's artifact in `scopes`.
    let mut status = ChannelStatus {
        active_artifact: Some(active("demo", 6, &post)),
        ..ChannelStatus::default()
    };
    status.scopes.insert(
        PRE_AGGREGATION.to_string(),
        ScopeState {
            active_artifact: Some(active("demo-dogstatsd_pre_aggregation", 5, &pre)),
            ..ScopeState::default()
        },
    );
    assert_eq!(found(&status, policy), [(PRE_AGGREGATION.to_string(), 5)]);
    // The policy scope's artifact comes from the top-level member when `scopes` does not describe it.
    assert_eq!(found(&status, PRE_AGGREGATION), [(policy.to_string(), 6)]);

    // An artifact ADP runs that the stub no longer serves is left out, as ADP no longer composes it.
    status.active_artifact = Some(ActiveArtifact {
        artifact_hash: Some(format!("sha256:{}", "e".repeat(64))),
        ..active("demo", 4, &post)
    });
    assert!(found(&status, PRE_AGGREGATION).is_empty());
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

#[test]
fn a_stale_base_is_a_warning_and_the_artifact_still_composes() {
    let (plan, export) = fixture();
    let source = default_program_source(&export, &crate::compile::RecipientLabels::default());
    // The editor leaves the base out; a source that states one records it.
    let stale = format!("policy metrics {{\n  base \"sha256:{}\"\n\n", "e".repeat(64));
    let stated = source.replacen("policy metrics {\n", &stale, 1);
    let outcome = compile_against(&stated, &plan, &export);
    assert_eq!(outcome.diagnostics.len(), 1, "{:#?}", outcome.diagnostics);
    let diagnostic = &outcome.diagnostics[0];
    assert_eq!(
        (diagnostic.severity, diagnostic.origin),
        (Severity::Warning, Origin::Composition)
    );
    assert!(diagnostic.message.contains("is not the current default program"));
    assert_eq!(diagnostic.range.line, 2);
    // Unmodified otherwise: the same plan, reported stale.
    assert!(outcome.preview.stale_base && outcome.preview.valid && outcome.preview.unchanged);
    // Formatting keeps the base the source states, without an `outputs` block, whose fingerprints the export gave.
    let canonical = outcome.canonical_source.unwrap();
    assert!(canonical.starts_with(&stale), "{}", canonical);
    assert!(!canonical.contains("outputs {"), "{}", canonical);
    assert_eq!(compile_against(&canonical, &plan, &export).diagnostics.len(), 1);
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
        &crate::stub::protocol::POLICY_SCOPE.parse().unwrap(),
        &host(catalog_for(&plan).with_detached_programs()),
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
fn a_policy_binding_is_underlined_in_the_node_that_binds_it() {
    // Two nodes bind the same setting; composition names the second, so the underline is its `config.<binding>`, not
    // the first one in the source.
    let source = r#"policy metrics {
  base "sha256:0000000000000000000000000000000000000000000000000000000000000000"

  dogstatsd
    | where not (name starts_with config.dogstatsd.metric_filter.values) as first
    | where not (name in config.dogstatsd.metric_filter.values) as second
    | emit output dd_metrics_encode
}
"#;
    let parsed = saluki_metric_plan_syntax::parse(source).unwrap();
    let error = ComposeError::PolicyBinding {
        node: "second".parse().unwrap(),
        binding: "dogstatsd.metric_filter.values".parse().unwrap(),
        value: None,
        allowed: Vec::new(),
    };
    let span = compose_error_span(&error, &parsed, source).unwrap();
    let written = "config.dogstatsd.metric_filter.values";
    assert_eq!(&source[span.start..span.end], written);
    assert_eq!(span.start, source.rfind(written).unwrap());
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
          with `as dsd_agg`."
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
          ID with `as dsd_agg`."
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
    let renamed = with_node_renamed(&plan, "metrics_policy", policy_id("policy_copies"));
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
    let received = receive_artifact(serde_json::from_str(&renamed).unwrap()).unwrap();
    let parsed = Parsed {
        artifact: received.artifact.clone(),
        spans: Default::default(),
        base_given: true,
    };
    let labels = crate::compile::RecipientLabels::default();
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: Some(ActivePlan {
            plan: &plan,
            hash: plan.hash(ResolvedValues::Include).unwrap(),
            generation: Some(3),
            artifact_revision: None,
        }),
        others: &[],
        revision: 7,
        labels: &labels,
        in_effect: None,
    };
    let (findings, preview) = compose_preview("", &parsed, &received, &context, &plan, &LineIndex::new(""));
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
    let source = r#"policy metrics {
  base "sha256:1111111111111111111111111111111111111111111111111111111111111111"

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
