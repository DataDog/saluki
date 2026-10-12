use super::*;
use crate::resources::TEAM_D_OUTPUT;

/// A sample of the demo's state, with families at each intake seen the given number of milliseconds ago.
fn state(intakes: &[(&str, &[(&str, u64)])]) -> Value {
    let intakes = intakes
        .iter()
        .map(|(name, families)| {
            let families = families
                .iter()
                .map(|(family, age)| json!({ "family": family, "age_ms": age }))
                .collect::<Vec<_>>();
            json!({ "name": name, "families": families })
        })
        .collect::<Vec<_>>();
    json!({ "intakes": intakes })
}

#[test]
fn an_intake_receives_the_demo_families_seen_recently() {
    let sample = state(&[
        (
            "primary",
            &[
                ("demo.canary", 900),
                ("demo.debug.*", PRESENT_MS),
                // Seen too long ago: it stopped arriving.
                ("demo.checkout.requests", PRESENT_MS + 1),
                // Not one of the demo's families.
                ("datadog.agent.running", 10),
            ],
        ),
        ("mrf", &[]),
    ]);
    assert_eq!(
        present(&sample),
        routing(&[("primary", &["demo.canary", "demo.debug.*"]), ("mrf", &[])])
    );
}

#[test]
fn a_routing_difference_names_what_each_intake_misses_and_has_besides() {
    let expected = changed(default_routing(), "primary", &["demo.debug.*"], true);
    // S1 not in effect yet at the primary endpoint, and team B receiving the platform family it should not.
    let found = changed(default_routing(), "team-b", &["demo.platform.queue_depth"], false);
    assert_eq!(
        routing_difference(&expected, &found),
        "primary is also receiving demo.debug.*; team-b is also receiving demo.platform.queue_depth"
    );
    let found = changed(expected.clone(), "team-c", &["demo.catalog.views"], true);
    assert_eq!(
        routing_difference(&expected, &found),
        "team-c is not receiving demo.catalog.views"
    );
    assert_eq!(routing_difference(&expected, &expected), "");
}

#[test]
fn the_default_routing_is_what_the_demo_configuration_routes_natively() {
    let routing = default_routing();
    assert_eq!(
        routing.keys().collect::<Vec<_>>(),
        ["mrf", "primary", "team-b", "team-c", "team-d"]
    );
    // Team D's endpoint is in no configuration file: nothing reaches it until a resources artifact declares it.
    assert!(routing["team-d"].is_empty());
    assert_eq!(routing["primary"].len(), FAMILIES.len());
    assert_eq!(
        routing["team-b"].iter().collect::<Vec<_>>(),
        ["demo.checkout.latency", "demo.checkout.requests"]
    );
    assert_eq!(routing["team-c"].iter().collect::<Vec<_>>(), ["demo.catalog.views"]);
    assert!(routing["mrf"].is_empty());
}

#[test]
fn the_scenario_edits_compose_as_the_check_expects() {
    use crate::{
        compile::{compile, default_program_source},
        preview::{tests::fixture, CompileContext},
    };

    let (plan, export) = fixture();
    let source = default_program_source(&export);
    let outputs = Outputs {
        primary: "dd_metrics_encode".to_string(),
        team_b: "metrics_routing_encode_0".to_string(),
        team_c: "metrics_routing_encode_1".to_string(),
        mrf: "mrf_metrics_encode".to_string(),
    };
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        others: &[],
        revision: 2,
    };
    let clean = [
        ("S1", edits::drop_debug(&source).unwrap()),
        ("S2", edits::widen_team_b(&source, &outputs).unwrap()),
        ("S3", edits::exclusive_team_b(&source, &outputs).unwrap()),
        ("S4", edits::mirror_to_failover(&source, &outputs).unwrap()),
    ];
    for (id, edited) in clean {
        let (outcome, _) = compile(&edited, Some(&context));
        assert!(outcome.diagnostics.is_empty(), "{}: {:#?}", id, outcome.diagnostics);
        assert!(outcome.preview.valid && !outcome.preview.unchanged, "{}", id);
    }
    let flagged = [
        (
            "S5a",
            edits::duplicate_branch(&source, &outputs).unwrap(),
            "duplicate_contribution",
        ),
        (
            "S5b",
            edits::undeclared_output(&source, &outputs).unwrap(),
            UNDECLARED_OUTPUT,
        ),
        (
            "S5c",
            edits::aggregation_without_parameters(&source).unwrap(),
            "valid_parameters",
        ),
    ];
    for (id, edited, fragment) in flagged {
        let (outcome, compiled) = compile(&edited, Some(&context));
        assert!(compiled.is_some(), "{}: publishable", id);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == crate::checks::Severity::Error
                    && (diagnostic.message.contains(fragment) || diagnostic.rule == Some(fragment))),
            "{}: {:#?}",
            id,
            outcome.diagnostics
        );
    }

    // S5d: an aggregation with every parameter in the policy scope is publishable. The editor does not place plans, so
    // it flags nothing, and ADP's placement refuses it.
    let edited = edits::aggregation_in_the_policy_scope(&source).unwrap();
    let (outcome, compiled) = compile(&edited, Some(&context));
    assert!(compiled.is_some(), "S5d: publishable");
    assert!(outcome.diagnostics.is_empty(), "S5d: {:#?}", outcome.diagnostics);
    assert!(
        outcome.preview.reason.contains("not placed"),
        "{}",
        outcome.preview.reason
    );
}

/// The fingerprint of team D's declared output, `resources_team_d`, delivering to the team D template's recipient
/// (hand-derived in `crate::resources`' tests).
const TEAM_D_FINGERPRINT: &str = "sha256:361f843bc9a6b25663cc6da865dd3af0daf04bb8890e3717ae98d7d30bb1c838";

/// The identity of the team D template's recipient.
const TEAM_D_RECIPIENT: &str = "dd_out/declared/team_d/http://127.0.0.1:17105";

/// The plan crate's default program and its export, with team D's output declared, as ADP lowers them once it accepts
/// the team D resources: the default program declares the output and the recipient, and the catalog lists the output
/// with its label. Nothing in the default program emits to it.
fn fixture_with_team_d() -> (saluki_metric_plan::ir::Plan, crate::export::ExportBundle) {
    let (plan, mut bundle) = crate::preview::tests::fixture_documents();
    let mut plan = serde_json::to_value(&plan).unwrap();
    let provenance = json!({"declared": {"revision": 1, "hash": format!("sha256:{}", "c".repeat(64))}});
    plan["outputs"].as_array_mut().unwrap().push(json!({
        "id": "resources_team_d", "accepts": {"kinds": "all", "timestamps": "mixed"},
        "fingerprint": TEAM_D_FINGERPRINT, "recipients": [TEAM_D_RECIPIENT],
        "binding": {"host": "dd_out", "selector": {"declared": [TEAM_D_RECIPIENT]},
                    "telemetry": "resources_team_d", "provenance": provenance}
    }));
    plan["resources"] = json!({"recipients": [{
        "id": TEAM_D_RECIPIENT, "host": "dd_out", "endpoint": "http://127.0.0.1:17105",
        "credential": "credentials/team_d", "classes": "metrics_only", "provenance": provenance
    }]});
    bundle["catalog"]["outputs"]["resources_team_d"] = json!({
        "accepts": {"kinds": "all", "timestamps": "mixed"}, "fingerprint": TEAM_D_FINGERPRINT,
        "recipients": [TEAM_D_RECIPIENT]
    });
    bundle["labels"] = json!({"resources_team_d": "Team D"});
    (
        saluki_metric_plan::ir::Plan::from_value(plan).unwrap(),
        crate::export::ExportBundle::from_value(bundle).unwrap(),
    )
}

#[test]
fn the_team_d_route_waits_for_its_output_with_the_drafts_fingerprint_and_composes_once_adp_declares_it() {
    use crate::{
        compile::{compile_with_resources, default_program_source},
        preview::{tests::fixture, CompileContext},
        resources::DraftFingerprints,
    };

    let draft = DraftFingerprints {
        fingerprints: [(TEAM_D_OUTPUT.parse().unwrap(), TEAM_D_FINGERPRINT.parse().unwrap())].into(),
        error: None,
    };
    let outputs = Outputs {
        primary: "dd_metrics_encode".to_string(),
        team_b: "metrics_routing_encode_0".to_string(),
        team_c: "metrics_routing_encode_1".to_string(),
        mrf: "mrf_metrics_encode".to_string(),
    };
    let context = |plan, export| CompileContext {
        export,
        default_program: Some(plan),
        active: None,
        others: &[],
        revision: 2,
    };

    // S10b: ADP does not declare the output yet. The draft gives its fingerprint, and composition says it is missing:
    // ADP holds the artifact until the resources arrive.
    let (plan, export) = fixture();
    let route = edits::route_to_team_d(&default_program_source(&export), &outputs, TEAM_D_OUTPUT).unwrap();
    let (outcome, compiled) = compile_with_resources(&route, Some(&context(&plan, &export)), &draft);
    let compiled = compiled.expect("publishable");
    assert_eq!(outcome.draft_fingerprints, [TEAM_D_OUTPUT]);
    assert_eq!(outcome.draft_note, None);
    assert!(!outcome.restored_fingerprints.contains(&TEAM_D_OUTPUT.to_string()));
    assert_eq!(
        compiled.document["base"]["outputs"][TEAM_D_OUTPUT], TEAM_D_FINGERPRINT,
        "{}",
        compiled.document
    );
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == crate::checks::Severity::Error
                && diagnostic.message.contains(TEAM_D_OUTPUT)),
        "{:#?}",
        outcome.diagnostics
    );
    // The editor prints the source back without the fingerprints it supplied.
    assert!(!outcome.canonical_source.unwrap().contains(TEAM_D_FINGERPRINT));

    // Without a draft that declares it, the editor says why it has no fingerprint for the output.
    let unreadable = DraftFingerprints {
        error: Some("The resources draft is not JSON".to_string()),
        ..DraftFingerprints::default()
    };
    let (outcome, _) = compile_with_resources(&route, Some(&context(&plan, &export)), &unreadable);
    assert!(outcome.draft_fingerprints.is_empty());
    assert!(
        outcome.draft_note.as_deref().unwrap().contains("not JSON"),
        "{:?}",
        outcome.draft_note
    );

    // S8b: ADP declares the output: the export gives its fingerprint, and the route composes and validates.
    let (plan, export) = fixture_with_team_d();
    let route = edits::route_to_team_d(&default_program_source(&export), &outputs, TEAM_D_OUTPUT).unwrap();
    let (outcome, compiled) = compile_with_resources(&route, Some(&context(&plan, &export)), &draft);
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(outcome.preview.valid && !outcome.preview.unchanged);
    assert!(outcome.restored_fingerprints.contains(&TEAM_D_OUTPUT.to_string()));
    assert!(outcome.draft_fingerprints.is_empty());
    assert_eq!(
        compiled.unwrap().document["base"]["outputs"][TEAM_D_OUTPUT],
        TEAM_D_FINGERPRINT
    );
    // The legend lists the declared output beside the native ones, feeding team D's intake.
    let legend = export.legend(&crate::settings::DemoPorts::default().recipients());
    let team_d = legend.outputs.iter().find(|output| output.id == TEAM_D_OUTPUT).unwrap();
    assert!(team_d.declared);
    assert_eq!(team_d.intake, Some("team-d"));
}

#[test]
fn plan_diffs_are_summarized_by_kind_of_change() {
    let cases = [
        (json!({ "available": false }), "no plan diff"),
        (json!({ "available": true, "unchanged": true }), "no change"),
        (
            json!({
                "available": true,
                "unchanged": false,
                "nodes": [
                    { "change": "added", "aspects": [] },
                    { "change": "changed", "aspects": ["edges"] },
                    { "change": "changed", "aspects": ["provenance"] },
                    { "change": "removed", "aspects": [] },
                ],
                "edges_added": ["a.out -> b.in", "b.out -> c.in"],
                "edges_removed": ["a.out -> c.in"],
            }),
            "1 added, 2 changed (1 in provenance only), 1 removed, 2 edges added, 1 removed",
        ),
    ];
    for (preview, expected) in cases {
        assert_eq!(diff_summary(&preview), expected, "{}", preview);
    }
}

#[test]
fn acknowledgments_are_found_by_product_and_revision_since_each_product_counts_its_own() {
    let status = Status::read(json!({
        "resources": {"active_artifact": {"id": "demo-resources", "revision": 2}},
        "config_states": [
            {"product": "resources", "id": "demo-resources", "revision": 2, "state": "active"},
            {"id": "demo", "revision": 2, "state": "pending", "error": "waiting for output `resources_team_d`"},
            {"product": "policy", "id": "demo", "revision": 3, "state": "active"}
        ]
    }));
    assert_eq!(status.resources_revision, Some(2));
    assert_eq!(
        status.acknowledgment(RESOURCES_PRODUCT_NAME, 2).unwrap()["state"],
        "active"
    );
    // A config state without a product is the policy product's, as the first protocol wrote it.
    let policy = status.acknowledgment(POLICY_PRODUCT, 2).unwrap();
    assert!(waiting_for(policy, TEAM_D_OUTPUT) && !settled(policy));
    assert!(!waiting_for(policy, "resources_team_e"));
    assert!(settled(status.acknowledgment(POLICY_PRODUCT, 3).unwrap()));
    assert!(status.acknowledgment(RESOURCES_PRODUCT_NAME, 3).is_none());
}

/// The resources product's name, as config states write it.
const RESOURCES_PRODUCT_NAME: &str = crate::stub::protocol::RESOURCES_PRODUCT;

#[test]
fn a_report_fails_on_an_error_a_violation_or_an_unbalanced_whole_run() {
    let balanced = CanaryReport {
        from_s: 100,
        to_s: 160,
        sent: 1_200,
        received: 1_200.0,
        state: "balanced".to_string(),
    };
    let passed = Report {
        canary: Some(balanced.clone()),
        ..Report::default()
    };
    assert_eq!(passed.failure(), None);

    let failed = Report {
        error: Some("S1 (drop demo.debug.*) failed".to_string()),
        ..passed.clone()
    };
    assert_eq!(failed.failure().as_deref(), Some("S1 (drop demo.debug.*) failed"));

    let violated = Report {
        violations: vec!["canaries missing for seconds 120 to 130".to_string()],
        ..passed.clone()
    };
    assert!(violated.failure().unwrap().contains("1 problems were seen"));

    let lost = Report {
        canary: Some(CanaryReport {
            received: 1_180.0,
            state: "missing".to_string(),
            ..balanced
        }),
        ..passed.clone()
    };
    assert!(lost.failure().unwrap().contains("is missing: 1200 sent, 1180 received"));

    let unchecked = Report { canary: None, ..passed };
    assert!(unchecked.failure().unwrap().contains("did not run"));
}
