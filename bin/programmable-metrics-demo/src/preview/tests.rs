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
        &"post_enrich".parse().unwrap(),
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

/// The plan crate's default program, and the export of one of its scopes as the demo reads it.
pub(crate) fn fixture_of_scope(scope: &str) -> (Plan, ExportBundle) {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let bundle = export(&plan, &scope.parse().unwrap(), &catalog_for(&plan), &BTreeMap::new()).unwrap();
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

/// Puts a node between the scope input and the node it fed, as the S1 scenario does.
fn with_node_after_input(source: &str, node: &str) -> String {
    let edge = "  input.metrics -> metrics_policy\n";
    assert!(source.contains(edge), "{}", source);
    source.replacen(edge, &format!("  input.metrics -> {}", node), 1)
}

#[test]
fn an_unmodified_export_composes_to_the_active_default_program_with_an_empty_diff() {
    let (plan, export) = fixture();
    let source = default_program_source(&export);
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
    };
    let (outcome, _) = compile(&source, Some(&context));
    assert!(outcome.preview.unchanged);
    assert_eq!(outcome.preview.against.unwrap().kind, BaseKind::DefaultProgram);
    assert!(outcome.preview.reason.contains("was not available"));
}

#[test]
fn a_change_diffs_node_by_node_in_the_artifacts_terms() {
    let (plan, export) = fixture();
    // S1: drop the debug metrics in front of the policy `tee`.
    let source = with_node_after_input(
        &default_program_source(&export),
        "drop_debug\n\n  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  -> drop\n    false -> \
         metrics_policy\n  }\n",
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
                node.provenance.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    // The new split and its inline drop are the artifact's (`policy/` IDs, revision 7). The node outside the scope
    // that fed the `tee` now feeds the split, and the `tee` is fed by it: both keep their native provenance, with other
    // edges. The four routing drops are fed other records than the default program feeds them (what the split lets
    // through), so native configuration no longer authorizes their loss: the artifact does.
    let changed_drop = |id| {
        (
            id,
            Some(id),
            "drop",
            ChangeKind::Changed,
            vec!["provenance"],
            Some("revision 7"),
        )
    };
    let expected = vec![
        (
            "metrics_enrich_host_tags",
            None,
            "host_tags",
            ChangeKind::Changed,
            vec!["edges"],
            Some("native"),
        ),
        (
            "policy/drop_debug",
            Some("drop_debug"),
            "split",
            ChangeKind::Added,
            vec![],
            Some("revision 7"),
        ),
        (
            "policy/drop_debug_true",
            Some("drop_debug_true"),
            "drop",
            ChangeKind::Added,
            vec![],
            Some("revision 7"),
        ),
        (
            "metrics_policy",
            Some("metrics_policy"),
            "tee",
            ChangeKind::Changed,
            vec!["edges"],
            Some("native"),
        ),
        changed_drop("metrics_routing_filter_0_false"),
        changed_drop("metrics_routing_filter_1_false"),
        changed_drop("mrf_metrics_gateway_false"),
        changed_drop("af_metrics_gateway_false"),
    ];
    assert_eq!(found, expected);
    assert_eq!(
        preview.edges_added,
        [
            "metrics_enrich_host_tags.out -> policy/drop_debug.in",
            "policy/drop_debug.true -> policy/drop_debug_true.in",
            "policy/drop_debug.false -> metrics_policy.in",
        ]
    );
    assert_eq!(
        preview.edges_removed,
        ["metrics_enrich_host_tags.out -> metrics_policy.in"]
    );
    assert!(preview.outputs.is_empty());

    // Artifact nodes are placed in the source: the new split at its definition, the inline drop at its `drop`.
    let split = &preview.nodes[1];
    assert_eq!(split.range.unwrap().line, line_of(&source, "  node drop_debug"));
    let drop = &preview.nodes[2];
    assert_eq!(drop.range.unwrap().line, line_of(&source, "    true  -> drop"));
    // A node outside the scope has no place in the source.
    assert_eq!(preview.nodes[0].range, None);
}

#[test]
fn validation_findings_are_placed_at_the_artifact_nodes_they_are_about() {
    let (plan, export) = fixture();
    let source = default_program_source(&export);
    // A second branch of the policy `tee` to the primary output: every record reaches it twice.
    let duplicate = source.replacen(
        "    -> emit dd_metrics_encode\n",
        "    -> emit dd_metrics_encode\n    -> emit dd_metrics_encode\n",
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
    assert_eq!(errors[0].range.line, line_of(&duplicate, "  node metrics_policy = tee"));
    // The plan diff is still shown, of a plan that does not validate.
    assert!(outcome.preview.available && !outcome.preview.valid);
    assert_eq!(outcome.errors, 1);

    // A parameter the operator does not have is placed at the parameter.
    let unknown_parameter = with_node_after_input(
        &source,
        "drop_debug\n\n  node drop_debug = split(predicate: name starts_with \"demo.debug.\", colour: \"red\") {\n    \
         true  -> drop\n    false -> metrics_policy\n  }\n",
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
    // A state owner is no composition error: ADP composes it, and validation, then placement, judge it.
    let (plan, export) = fixture();
    let source = default_program_source(&export);
    let source = with_node_after_input(
        &source,
        "rollup\n\n  node rollup = aggregate(window: 10s) {\n    -> metrics_policy\n  }\n",
    );
    let outcome = compile_against(&source, &plan, &export);
    assert!(outcome.ok);
    assert!(!outcome.diagnostics.is_empty());
    for diagnostic in &outcome.diagnostics {
        assert_eq!(
            (diagnostic.severity, diagnostic.origin, diagnostic.rule),
            (Severity::Error, Origin::Validation, Some("valid_parameters")),
            "{:#?}",
            diagnostic
        );
        assert!(
            diagnostic.message.contains("missing required parameter"),
            "{}",
            diagnostic.message
        );
        assert_eq!(diagnostic.range.line, line_of(&source, "  node rollup"));
    }
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
    let (plan, export) = fixture();
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
        upgraded: Default::default(),
    };
    let active = compose_all(&plan, &[(&other.artifact, &approval)], &export.catalog, &registry)
        .unwrap()
        .plan;
    let hash = active.hash(ResolvedValues::Include).unwrap();
    let source = default_program_source(&export);
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
    let (plan, export) = fixture();
    let pre = pre_aggregation_document(&plan);
    let post = serde_json::to_value(&export.artifact.artifact).unwrap();
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
        ("post_enrich".to_string(), envelope(6, &post)),
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
    assert_eq!(found(&status, "post_enrich"), [(PRE_AGGREGATION.to_string(), 5)]);
    // The policy scope's artifact comes from the top-level member when `scopes` does not describe it.
    assert_eq!(found(&status, PRE_AGGREGATION), [("post_enrich".to_string(), 6)]);

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
    let source = default_program_source(&export);
    let cases = [(
        "an emit to an output ADP does not declare",
        source.replacen("emit metrics_routing_encode_1\n", "emit metrics_routing_encode_99\n", 1),
        "emit metrics_routing_encode_99",
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
    let source = default_program_source(&export);
    let current = export.program_hash().to_string();
    let stale = format!("sha256:{}", "e".repeat(64));
    let outcome = compile_against(&source.replacen(&current, &stale, 1), &plan, &export);
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
}
