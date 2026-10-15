use super::*;
use crate::compile::{default_program_source, RecipientLabels};

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
        present(&sample, PRESENT_MS),
        routing(&[("primary", &["demo.canary", "demo.debug.*"]), ("mrf", &[])])
    );
    // With 10-second windows, a family arrives once per window: one seen 12.5 s ago still arrives.
    assert_eq!(present_ms(1), PRESENT_MS);
    assert_eq!(present_ms(10), 12_500);
    let ten_second_windows = state(&[("primary", &[("demo.canary", 12_500), ("demo.debug.*", 12_501)])]);
    assert_eq!(
        present(&ten_second_windows, present_ms(10)),
        routing(&[("primary", &["demo.canary"])])
    );
}

#[test]
fn a_launch_is_described_by_its_window_expiry_and_lanes() {
    use crate::settings::{DemoPorts, LaneFlags};

    let mut settings = RunSettings {
        ports: DemoPorts::default(),
        run_dir: "/tmp/check".into(),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: false,
        contexts: 0,
        lanes: LaneFlags::default(),
    };
    assert_eq!(
        describe_launch(&settings),
        "1-second aggregation windows, counter expiry 0 s, with ADP's default lanes"
    );
    settings.aggregate_window_secs = 10;
    settings.counter_expiry_secs = 1;
    settings.lanes = LaneFlags {
        decoders: Some(2),
        state: Some(2),
        post: None,
    };
    assert_eq!(
        describe_launch(&settings),
        "10-second aggregation windows, counter expiry 1 s, on lanes: decoders 2, state lanes 2, post-aggregation \
         lanes automatic"
    );
}

#[test]
fn the_width_change_composes_into_the_default_program_as_the_check_expects() {
    use crate::{
        compile::{compile, default_program_source},
        preview::{tests::fixture, CompileContext},
    };

    let (plan, export) = fixture();
    let source = default_program_source(&export, &RecipientLabels::default());
    let edited = edits::width_change(&source, "30s").unwrap();
    // Both windows, and nothing else, change.
    let changed = source
        .lines()
        .zip(edited.lines())
        .filter(|(old, new)| old != new)
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        [
            (
                "    window: config(\"dogstatsd.aggregation.window_duration_seconds\"),",
                "    window: 30s,"
            ),
            (
                "    window: config(\"dogstatsd.aggregation.window_duration_seconds\"),",
                "    window: 30s,"
            ),
        ]
    );
    assert_eq!(source.lines().count(), edited.lines().count());

    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        others: &[],
        revision: 2,
        labels: &RecipientLabels::default(),
        in_effect: None,
    };
    let (outcome, compiled) = compile(&edited, Some(&context));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(
        outcome.preview.valid && !outcome.preview.unchanged,
        "{:#?}",
        outcome.preview
    );
    // The artifact gives the canary 30-second buckets, which the canary check aligns its ranges to.
    let document = compiled.unwrap().document;
    assert_eq!(crate::canary::artifact_widths(&document), [30, 30]);

    // The unmodified program composes to the default program itself.
    let (outcome, _) = compile(&source, Some(&context));
    assert!(
        outcome.diagnostics.is_empty() && outcome.preview.unchanged,
        "{:#?}",
        outcome
    );

    // A source without one of the nodes says which.
    let error = edits::width_change("policy metrics {\n}\n", "30s").unwrap_err();
    assert!(error.to_string().contains("dsd_no_agg_split"), "{}", error);
}

#[test]
fn the_width_change_sets_the_inline_window_of_a_detached_export() {
    use crate::{
        compile::{compile, default_program_source},
        preview::{tests::detached_fixture, CompileContext},
    };

    // A detached export holds the window as a literal, and prints the split's only parameter inline.
    let (plan, export) = detached_fixture();
    let source = default_program_source(&export, &RecipientLabels::default());
    assert!(source.contains("timestamp_split(window: 10s)"), "{}", source);
    let edited = edits::width_change(&source, "30s").unwrap();
    let changed = source
        .lines()
        .zip(edited.lines())
        .filter(|(old, new)| old != new)
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        [
            (
                "  node dsd_no_agg_split = timestamp_split(window: 10s) {",
                "  node dsd_no_agg_split = timestamp_split(window: 30s) {"
            ),
            ("    window: 10s,", "    window: 30s,"),
        ]
    );
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        others: &[],
        revision: 2,
        labels: &RecipientLabels::default(),
        in_effect: None,
    };
    let (outcome, compiled) = compile(&edited, Some(&context));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(
        outcome.preview.valid && !outcome.preview.unchanged,
        "{:#?}",
        outcome.preview
    );
    assert_eq!(crate::canary::artifact_widths(&compiled.unwrap().document), [30, 30]);
}

#[test]
fn a_routing_difference_names_what_each_intake_misses_and_has_besides() {
    let expected = changed(default_routing(), "primary", &["demo.debug.*"], true);
    // S1 not in effect yet at the primary endpoint, and team B missing the platform family.
    let found = changed(default_routing(), "team-b", &["demo.platform.queue_depth"], true);
    assert_eq!(
        routing_difference(&expected, &found),
        "primary is also receiving demo.debug.*; team-b is not receiving demo.platform.queue_depth"
    );
    assert_eq!(routing_difference(&expected, &expected), "");
    assert!(routing_holds(&expected, &expected));

    // An intake the routing does not name must receive nothing, as a destination being removed while it drains; one
    // the routing names must be listed.
    let mut draining = expected.clone();
    draining.insert("dest-d".to_string(), BTreeSet::new());
    assert!(routing_holds(&expected, &draining));
    let receiving = changed(draining, "dest-d", &["demo.canary"], false);
    assert!(!routing_holds(&expected, &receiving));
    assert_eq!(
        routing_difference(&expected, &receiving),
        "dest-d is also receiving demo.canary"
    );
    let added = changed(expected.clone(), "dest-d", &["demo.canary"], false);
    assert!(!routing_holds(&added, &expected));
    assert_eq!(
        routing_difference(&added, &expected),
        "dest-d is not receiving demo.canary"
    );
}

#[test]
fn the_default_routing_is_what_the_demo_configuration_routes_natively() {
    let routing = default_routing();
    assert_eq!(
        routing.keys().collect::<Vec<_>>(),
        ["mrf", "primary", "team-b", "team-c"]
    );
    // Teams B and C are additional endpoints: natively, they receive everything the primary endpoint does.
    for intake in ["primary", "team-b", "team-c"] {
        assert_eq!(routing[intake].len(), FAMILIES.len(), "{}", intake);
    }
    assert!(routing["mrf"].is_empty());
    // The recipe narrows team B to the checkout families.
    let narrowed = narrowed_routing();
    assert_eq!(
        narrowed["team-b"].iter().collect::<Vec<_>>(),
        ["demo.checkout.latency", "demo.checkout.requests"]
    );
    assert_eq!(narrowed["team-c"], routing["team-c"]);
}

/// The plan crate's default program and its detached export, with a recipient directory: `primary` names the main
/// output's recipient, `mrf` the failover output's, and each routing group's endpoint its URL, as ADP's directory names
/// them. The detached export then addresses those outputs by name, as ADP's does.
fn named_fixture() -> (saluki_metric_plan::ir::Plan, crate::export::ExportBundle) {
    let (plan, export) = crate::preview::tests::detached_fixture_with(|catalog| {
        let named = |recipients: &[&str], endpoint: &str| saluki_metric_plan::validate::CatalogRecipient {
            recipients: recipients.iter().map(|id| id.parse().unwrap()).collect(),
            host: if endpoint.contains("17104") {
                "mrf_dd_out"
            } else {
                "dd_out"
            }
            .parse()
            .unwrap(),
            endpoint: endpoint.to_string(),
        };
        catalog
            .with_recipient(
                "primary".parse().unwrap(),
                named(&["dd_out/primary/0"], "http://127.0.0.1:17101"),
            )
            .with_recipient(
                "http://127.0.0.1:17102".parse().unwrap(),
                named(&["dd_out/additional_0/0"], "http://127.0.0.1:17102"),
            )
            .with_recipient(
                "http://127.0.0.1:17103".parse().unwrap(),
                named(
                    &["dd_out/additional_1/0", "dd_out/additional_1/1"],
                    "http://127.0.0.1:17103",
                ),
            )
            .with_recipient(
                "mrf".parse().unwrap(),
                named(&["mrf_dd_out/primary/0"], "http://127.0.0.1:17104"),
            )
    });
    (plan, export)
}

/// Returns `source` without the fixture's endpoint routing groups: their branches of the policy `tee` and their nodes.
fn without_routing_groups(source: &str) -> String {
    let mut edited = source.to_string();
    for group in ["metrics_routing_filter_0", "metrics_routing_filter_1"] {
        edited = edited.replacen(&format!("    -> {}\n", group), "", 1);
        let header = format!("\n  node {} = ", group);
        let Some(start) = edited.find(&header) else { continue };
        let end = start + edited[start + 1..].find("\n  }\n").unwrap() + "\n  }\n".len() + 1;
        // The comment above the node goes with it.
        let start = edited[..start]
            .rfind("\n  #")
            .filter(|at| !edited[*at + 1..start].contains("\n  node"))
            .unwrap_or(start);
        edited.replace_range(start..end, "");
    }
    edited
}

#[test]
fn the_scenario_edits_compose_as_the_check_expects() {
    use crate::{compile::compile, preview::CompileContext};

    let (plan, export) = named_fixture();
    let source = default_program_source(&export, &RecipientLabels::default());
    assert!(source.contains("    -> emit to [primary]\n"), "{}", source);
    assert!(source.contains("    true  -> emit to [mrf]\n"), "{}", source);
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        others: &[],
        revision: 2,
        labels: &RecipientLabels::default(),
        in_effect: None,
    };
    let team_b = "http://127.0.0.1:17102";
    let clean = [
        ("S1", edits::drop_debug(&source).unwrap()),
        // Team B's endpoint has a routing group's output: the exclusive route reuses it.
        ("S3b", edits::exclusive_team_b(&source, team_b).unwrap()),
        ("S4a", edits::mirror_checkout_on_failover(&source).unwrap()),
    ];
    for (id, edited) in clean {
        let (outcome, _) = compile(&edited, Some(&context));
        assert!(outcome.diagnostics.is_empty(), "{}: {:#?}", id, outcome.diagnostics);
        assert!(outcome.preview.valid && !outcome.preview.unchanged, "{}", id);
    }
    let flagged = [
        (
            "S5a",
            edits::duplicate_branch(&source).unwrap(),
            "duplicate_contribution",
        ),
        ("S5b", edits::two_hosts(&source).unwrap(), TWO_HOSTS[0]),
        ("S5c", edits::second_aggregation(&source).unwrap(), "single_aggregation"),
        ("S5e", edits::policy_binding(&source).unwrap(), POLICY_BINDING[0]),
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

    // S5c's finding is at the second aggregation's line.
    let edited = edits::second_aggregation(&source).unwrap();
    let (outcome, _) = compile(&edited, Some(&context));
    let line = edited[..edited.find("  node rollup = aggregate(").unwrap()]
        .matches('\n')
        .count()
        + 1;
    let at = outcome
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.rule == Some("single_aggregation"))
        .expect("the second aggregation is flagged");
    assert_eq!(at.range.line, line, "{:#?}", at);

    // S5d: the one aggregation moved after enrichment is publishable. The editor does not place plans, so it flags
    // nothing, and ADP's placement refuses it.
    let edited = edits::aggregation_after_enrichment(&source).unwrap();
    let (outcome, compiled) = compile(&edited, Some(&context));
    assert!(compiled.is_some(), "S5d: publishable");
    assert!(outcome.diagnostics.is_empty(), "S5d: {:#?}", outcome.diagnostics);
    assert!(
        outcome.preview.reason.contains("not placed"),
        "{}",
        outcome.preview.reason
    );

    // S5f waits for a recipient the directory does not name: not an error, and no diff.
    let (outcome, compiled) = compile(&edits::unknown_recipient(&source).unwrap(), Some(&context));
    assert!(
        compiled.is_some() && outcome.errors == 0,
        "S5f: {:#?}",
        outcome.diagnostics
    );
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains(UNKNOWN_RECIPIENT)),
        "S5f: {:#?}",
        outcome.diagnostics
    );
    assert!(!outcome.preview.available);

    // An emit to recipients no output delivers to exactly: the editor stands in for the output ADP adds, says so for
    // every such set, and validates with it, so that a provisional diff and ADP's findings show before publishing.
    // Without the fixture's routing groups, whose outputs would share recipients with the new set.
    let without_groups = without_routing_groups(&source);
    let routed = edits::exclusive_team_b(&without_groups, team_b).unwrap().replace(
        &format!("emit to [{}]", edits::quoted(team_b)),
        &format!("emit to [primary, {}]", edits::quoted(team_b)),
    );
    let (outcome, _) = compile(&routed, Some(&context));
    assert_eq!(outcome.errors, 0, "{:#?}", outcome.diagnostics);
    let adds = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with(IMPLIED_OUTPUT))
        .collect::<Vec<_>>();
    assert_eq!(adds.len(), 1, "{:#?}", outcome.diagnostics);
    assert!(!adds[0].message.contains("checkout_only"), "{}", adds[0].message);
    assert!(outcome.preview.available && outcome.preview.valid && outcome.preview.provisional);
    assert!(
        outcome.preview.outputs.iter().any(|output| output.change == "added"
            && output
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with("ADP will add it"))),
        "{:#?}",
        outcome.preview.outputs
    );
    // The same program with a duplicate branch: the editor finds what ADP will reject, before publishing.
    let doubled = edits::duplicate_branch(&routed).unwrap();
    let (outcome, _) = compile(&doubled, Some(&context));
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule == Some("duplicate_contribution")),
        "{:#?}",
        outcome.diagnostics
    );

    // S14a: the aggregation renamed does what the default program does.
    let (outcome, _) = compile(&edits::rename_aggregation(&source).unwrap(), Some(&context));
    assert!(
        outcome.diagnostics.is_empty() && outcome.preview.unchanged,
        "S14a: {:#?}",
        outcome
    );
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
                    { "change": "changed", "aspects": ["params"] },
                    { "change": "removed", "aspects": [] },
                ],
                "edges_added": ["a.out -> b.in", "b.out -> c.in"],
                "edges_removed": ["a.out -> c.in"],
            }),
            "1 added, 2 changed, 1 removed, 2 edges added, 1 removed",
        ),
    ];
    for (preview, expected) in cases {
        assert_eq!(diff_summary(&preview), expected, "{}", preview);
    }
}

#[test]
fn acknowledgments_are_found_by_product_and_revision_and_status_says_which_program_runs() {
    let status = Status::read(json!({
        "program": {"runs": "published", "native_policy": "not_applied", "differs": ["dogstatsd.metric_filter.values"]},
        "config_states": [
            {"id": "demo", "revision": 2, "state": "pending", "error": "waiting for recipient `team_z`: ..."},
            {"product": "policy", "id": "demo", "revision": 3, "state": "active"}
        ]
    }));
    // A config state without a product is the policy product's, as the first protocol wrote it.
    let policy = status.acknowledgment(POLICY_PRODUCT, 2).unwrap();
    assert!(waiting_for(policy, UNKNOWN_RECIPIENT) && !settled(policy));
    assert!(!waiting_for(policy, "team_y"));
    assert!(settled(status.acknowledgment(POLICY_PRODUCT, 3).unwrap()));
    assert_eq!(
        (status.runs(), status.native_policy(), status.differs()),
        (
            Some("published"),
            Some("not_applied"),
            vec!["dogstatsd.metric_filter.values"]
        )
    );
    // An ADP that does not say which program runs says nothing of it.
    let silent = Status::read(json!({}));
    assert_eq!((silent.runs(), silent.differs()), (None, Vec::<&str>::new()));
}

#[test]
fn a_report_fails_on_an_error_a_violation_or_an_unbalanced_whole_run() {
    let balanced = CanaryReport {
        from_s: 100,
        to_s: 160,
        sent: 1_200,
        received: 1_200.0,
        host_dropped: 0.0,
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

    let unchecked = Report {
        canary: None,
        ..passed.clone()
    };
    assert!(unchecked.failure().unwrap().contains("did not run"));

    // The second launch's failure is the check's, named as the second launch's; the first launch's comes first.
    let second = Report {
        launch: "10-second aggregation windows, on lanes".to_string(),
        error: Some("S12b (a counter whose state lane is empty at a tick) failed".to_string()),
        ..passed.clone()
    };
    let both = Report {
        second_launch: Some(Box::new(second.clone())),
        ..passed.clone()
    };
    assert_eq!(
        both.failure().as_deref(),
        Some(
            "in the second launch (10-second aggregation windows, on lanes): S12b (a counter whose state lane is \
             empty at a tick) failed"
        )
    );
    let first_failed = Report {
        error: Some("S1 failed".to_string()),
        ..both.clone()
    };
    assert_eq!(first_failed.failure().as_deref(), Some("S1 failed"));
    let both_passed = Report {
        second_launch: Some(Box::new(passed.clone())),
        ..passed.clone()
    };
    assert_eq!(both_passed.failure(), None);
    // The summary lists both launches.
    let lines = both.summary_lines();
    assert_eq!(lines.iter().filter(|line| line.starts_with("launch: ")).count(), 2);
    assert!(lines.contains(&"launch: 10-second aggregation windows, on lanes".to_string()));
}

/// What the demo's canary check answers for a range of seconds in which `rate` canaries were sent each second and all
/// received.
fn balanced_range((from_s, to_s): (u64, u64), rate: u64) -> Value {
    let sent = (to_s - from_s) * rate;
    json!({"state": "balanced", "from_s": from_s, "to_s": to_s, "sent": sent, "received": sent as f64,
           "allowance": 2.0 + 0.2 * rate as f64})
}

#[test]
fn the_whole_run_is_read_in_ranges_within_the_history_however_long_it_is() {
    // A launch of fifteen minutes, three times the history the demo keeps, at 20 canaries a second: 10-second buckets,
    // then 30-second ones from the tenth minute, and a stretch of 100 s in which the runner reads ADP's status alone.
    let started_s = 1_000_000;
    let width_at = |now_s: u64| if now_s < started_s + 600 { 10 } else { 30 };
    let first = crate::canary::window((started_s + 20) * 1000, 10);
    let mut totals = CanaryTotals::starting_at(first.0);
    let mut ranges = Vec::new();
    for now_s in started_s + 20..started_s + 900 {
        if (started_s + 400..started_s + 500).contains(&now_s) {
            continue;
        }
        let now_ms = now_s * 1000 + 300;
        let (_, rolling_to_s) = crate::canary::window(now_ms, width_at(now_s));
        if let Some(range) = totals.due(rolling_to_s) {
            // Read a moment after the sample: within the history, and delivered.
            assert_eq!(
                crate::canary::readable(range, now_ms + 200),
                Ok(()),
                "{:?} at {}",
                range,
                now_s
            );
            totals.add(range, &balanced_range(range, 20)).unwrap();
            ranges.push(range);
        }
    }
    // The check ends half a minute later, with the rest of the run read at once.
    let end_ms = (started_s + 930) * 1000;
    let (_, last_s) = crate::canary::window(end_ms, 30);
    assert!(last_s > totals.to_s);
    let rest = (totals.to_s, last_s);
    assert_eq!(crate::canary::readable(rest, end_ms), Ok(()));
    totals.add(rest, &balanced_range(rest, 20)).unwrap();
    ranges.push(rest);

    // The ranges follow one another from the first balanced range to the last, and their sums are the whole run's.
    assert!(ranges.len() > 10, "{:?}", ranges);
    assert!(ranges.windows(2).all(|pair| pair[0].1 == pair[1].0));
    assert_eq!((ranges[0].0, ranges.last().unwrap().1), (first.0, last_s));
    let report = totals.report();
    assert_eq!((report.from_s, report.to_s), (first.0, last_s));
    assert_eq!(report.sent, (last_s - first.0) * 20);
    assert_eq!(
        (report.received, report.state.as_str()),
        (report.sent as f64, "balanced")
    );
    // Read at once, as the check used to, the whole run starts long before the history the demo keeps.
    assert!(crate::canary::readable((first.0, last_s), end_ms).is_err());
}

#[test]
fn the_whole_runs_totals_continue_range_by_range_and_say_what_their_sum_holds() {
    let mut totals = CanaryTotals::starting_at(1_000);
    assert_eq!(totals.due(1_059), None);
    assert_eq!(totals.due(1_060), Some((1_000, 1_060)));
    totals.add((1_000, 1_060), &balanced_range((1_000, 1_060), 20)).unwrap();
    // A range that leaves a gap, overlaps, or is empty does not continue the run's; nor does an answer without counts.
    for range in [(1_070, 1_100), (1_050, 1_100), (1_060, 1_060)] {
        assert!(totals.add(range, &balanced_range((0, 1), 20)).is_err(), "{:?}", range);
    }
    assert!(totals.add((1_060, 1_120), &json!({"state": "balanced"})).is_err());
    assert_eq!(totals.to_s, 1_060);

    // Ranges that each balance within the allowance can sum to a loss: the sum decides.
    for range in [(1_060, 1_120), (1_120, 1_180)] {
        let mut short = balanced_range(range, 20);
        short["received"] = json!(1_195.0);
        totals.add(range, &short).unwrap();
    }
    let report = totals.report();
    assert_eq!((report.sent, report.received), (3_600, 3_590.0));
    assert_eq!(report.state, "missing");

    // A range whose shortfall the host dropped before ADP read it counts as the host's, and the whole run passes
    // unless more is missing than the host dropped.
    let mut dropped = CanaryTotals::starting_at(1_000);
    dropped
        .add((1_000, 1_060), &balanced_range((1_000, 1_060), 20))
        .unwrap();
    let mut flood = balanced_range((1_060, 1_120), 20);
    flood["state"] = json!("host_dropped");
    flood["received"] = json!(1_156.0);
    dropped.add((1_060, 1_120), &flood).unwrap();
    let report = dropped.report();
    assert_eq!(
        (report.sent, report.received, report.host_dropped),
        (2_400, 2_356.0, 44.0)
    );
    assert_eq!(report.state, "host_dropped");
    let passed = Report {
        canary: Some(report),
        ..Report::default()
    };
    assert_eq!(passed.failure(), None);
    assert!(
        passed
            .summary_lines()
            .iter()
            .any(|line| line.contains("the host dropped 44 before ADP read them")),
        "{:?}",
        passed.summary_lines()
    );
    // A later range a few short within its own allowance still sums to a loss the host's drops do not cover.
    let mut short = balanced_range((1_120, 1_180), 20);
    short["received"] = json!(1_195.0);
    dropped.add((1_120, 1_180), &short).unwrap();
    let mut short = balanced_range((1_180, 1_240), 20);
    short["received"] = json!(1_195.0);
    dropped.add((1_180, 1_240), &short).unwrap();
    assert_eq!(dropped.report().state, "missing");

    // A range ADP was not up for leaves the whole run waiting, whatever the sums.
    let mut waiting = CanaryTotals::starting_at(1_000);
    let mut range = balanced_range((1_000, 1_060), 20);
    range["state"] = json!("waiting");
    waiting.add((1_000, 1_060), &range).unwrap();
    assert_eq!(waiting.report().state, "waiting");
}

#[tokio::test]
async fn a_misrouted_lane_event_is_a_violation_reported_once_per_lane() {
    use crate::settings::{DemoPorts, LaneFlags};

    let _ = saluki_tls::initialize_default_crypto_provider();
    let settings = RunSettings {
        ports: DemoPorts::default(),
        run_dir: "/tmp/check".into(),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 10,
        counter_expiry_secs: 1,
        traffic_rate: 20.0,
        traffic_paused: false,
        contexts: 0,
        lanes: LaneFlags::default(),
    };
    let mut runner = Runner::new(&settings).unwrap();
    let misrouted = |lane: &str, value: f64| {
        json!({"name": "plan_lane_misrouted_total", "labels": {"component_id": "dsd_agg", "lane": lane},
               "counter": true, "value": value})
    };
    let sample = |series: Vec<Value>| json!({"telemetry": {"plan": series}});
    let events = json!({"name": "plan_lane_events_total", "labels": {"component_id": "dsd_agg", "lane": "0"},
                        "value": 120.0});

    // Zero, as it must stay, and other lane series: nothing to report.
    runner.observe(&sample(vec![misrouted("0", 0.0), misrouted("1", 0.0), events.clone()]));
    assert!(runner.report.violations.is_empty());

    runner.observe(&sample(vec![misrouted("0", 0.0), misrouted("1", 3.0), events.clone()]));
    runner.observe(&sample(vec![misrouted("0", 0.0), misrouted("1", 5.0), events]));
    assert_eq!(
        runner.report.violations,
        ["lane 1 of dsd_agg admitted 3 events whose key another lane owns (plan_lane_misrouted_total)"]
    );
    assert!(runner.report.failure().unwrap().contains("plan_lane_misrouted_total"));
}

#[tokio::test]
async fn canaries_the_host_dropped_are_counted_apart_and_canaries_missing_are_violations_that_say_where() {
    use crate::settings::{DemoPorts, LaneFlags};

    let _ = saluki_tls::initialize_default_crypto_provider();
    let settings = RunSettings {
        ports: DemoPorts::default(),
        run_dir: "/tmp/check".into(),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: false,
        contexts: 0,
        lanes: LaneFlags::default(),
    };
    let mut runner = Runner::new(&settings).unwrap();
    let sample = |state: &str, from_s: u64, received: f64, reason: Option<&str>| {
        json!({"canary": {"state": state, "from_s": from_s, "to_s": from_s + 10, "sent": 200, "received": received,
                          "reason": reason, "elsewhere": []}})
    };

    // A stretch of ranges the host dropped canaries in: counted apart, and no violation.
    runner.observe(&sample("balanced", 100, 200.0, None));
    for from_s in 101..105 {
        runner.observe(&sample(
            "host_dropped",
            from_s,
            170.0,
            Some("The host dropped the 30 ..."),
        ));
    }
    runner.observe(&sample("balanced", 105, 199.0, None));
    assert!(runner.report.violations.is_empty(), "{:?}", runner.report.violations);
    assert_eq!(
        (runner.report.canary_samples, runner.report.host_dropped_samples),
        (2, 4)
    );
    assert_eq!(runner.whole_run.as_ref().map(|totals| totals.from_s), Some(100));

    // Canaries missing are a violation, which says where they went.
    let lost = "ADP's DogStatsD source received all but about 0 of the datagrams sent: the other 30 canaries were \
                lost in ADP.";
    runner.observe(&sample("missing", 106, 170.0, Some(lost)));
    assert_eq!(
        runner.report.violations,
        [format!(
            "canaries missing for seconds 106 to 116: 200 sent, 170.0 received at the primary endpoint ({})",
            lost
        )]
    );
}

#[tokio::test]
async fn canaries_at_other_endpoints_are_a_violation_only_beyond_what_was_sent() {
    use crate::settings::{DemoPorts, LaneFlags};

    let _ = saluki_tls::initialize_default_crypto_provider();
    let settings = RunSettings {
        ports: DemoPorts::default(),
        run_dir: "/tmp/check".into(),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: false,
        contexts: 0,
        lanes: LaneFlags::default(),
    };
    let mut runner = Runner::new(&settings).unwrap();
    let sample = |elsewhere: Value| {
        json!({"canary": {"state": "balanced", "from_s": 100, "to_s": 110, "sent": 200, "received": 200.0,
                          "allowance": 6.0, "elsewhere": elsewhere}})
    };
    // Teams B and C dual-ship everything, and a destination added within the range received part of it.
    runner.observe(&sample(json!([
        {"intake": "team-b", "received": 200.0}, {"intake": "team-c", "received": 205.0},
        {"intake": "dest-d", "received": 60.0}
    ])));
    assert!(runner.report.violations.is_empty(), "{:?}", runner.report.violations);
    // More than were sent is a canary delivered twice.
    runner.observe(&sample(json!([{"intake": "team-b", "received": 400.0}])));
    assert_eq!(
        runner.report.violations,
        ["\"team-b\" received 400 canaries for seconds 100 to 110, more than the 200 sent"]
    );
}

#[tokio::test]
async fn a_whole_run_range_the_check_cannot_say_yet_is_added_once_it_can() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    use axum::{routing::get, Json, Router};

    use crate::settings::{DemoPorts, LaneFlags};

    // A demo whose canary check waits for a scrape of ADP's telemetry the first time it is asked, and then finds that
    // the host dropped the range's shortfall.
    let asked = Arc::new(AtomicU64::new(0));
    let answers = Arc::clone(&asked);
    let demo = Router::new().route(
        "/api/canary",
        get(move || {
            let first = answers.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                let (state, pending) = if first {
                    ("waiting", true)
                } else {
                    ("host_dropped", false)
                };
                Json(
                    json!({"state": state, "pending_telemetry": pending, "sent": 1_200, "received": 1_100.0,
                            "allowance": 6.0}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move { axum::serve(listener, demo).await });

    let _ = saluki_tls::initialize_default_crypto_provider();
    let settings = RunSettings {
        ports: DemoPorts {
            ui: port,
            ..DemoPorts::default()
        },
        run_dir: "/tmp/check".into(),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: false,
        contexts: 0,
        lanes: LaneFlags::default(),
    };
    let mut runner = Runner::new(&settings).unwrap();
    runner.whole_run = Some(CanaryTotals::starting_at(1_000));

    // Not added while the check cannot say yet, so that a later sample reads it again.
    assert!(!runner.add_canary_range((1_000, 1_060)).await.unwrap());
    assert_eq!(runner.whole_run.as_ref().unwrap().to_s, 1_000);
    // Read again, it is added, with what the host dropped.
    assert!(runner.add_canary_range((1_000, 1_060)).await.unwrap());
    let report = runner.whole_run.as_ref().unwrap().report();
    assert_eq!(
        (report.to_s, report.host_dropped, report.state.as_str()),
        (1_060, 100.0, "host_dropped")
    );
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    server.abort();
}
