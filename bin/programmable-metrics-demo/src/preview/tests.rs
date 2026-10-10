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
fn composition_errors_are_one_finding_at_the_node_or_output_and_no_diff() {
    let (plan, export) = fixture();
    let source = default_program_source(&export);
    let cases = [
        (
            "a stateful node",
            with_node_after_input(
                &source,
                "rollup\n\n  node rollup = aggregate(window: 10s) {\n    -> metrics_policy\n  }\n",
            ),
            "  node rollup",
            "keeps state",
        ),
        (
            "an emit to an output ADP does not declare",
            source.replacen("emit metrics_routing_encode_1\n", "emit metrics_routing_encode_99\n", 1),
            "emit metrics_routing_encode_99",
            "emits to output `metrics_routing_encode_99`, which the host does not declare. Emit to one of the outputs \
             it declares: `",
        ),
    ];
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
