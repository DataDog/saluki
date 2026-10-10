use super::*;

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
        ["mrf", "primary", "team-b", "team-c"]
    );
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
        ("S5c", edits::stateful_node(&source).unwrap(), "keeps state"),
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
