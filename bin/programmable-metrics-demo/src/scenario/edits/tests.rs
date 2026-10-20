use super::*;

/// Team B's URL, as the default program names its recipient.
const TEAM_B: &str = "http://127.0.0.1:17102";

/// The default program of the demo's configuration, detached, as the editor prints it: what does
/// nothing as configured left out and noted at the top, the aggregation's window and the demo's short flush and expiry,
/// `let metrics` naming the stream enrichment would read (enrichment does nothing, so it is left out), the main `emit`
/// naming the primary endpoint and teams B and C, and the failover gateway reading its two signals.
const PRINTED: &str = r#"policy {
  # Not shown, because they do nothing as configured: mapper (no profiles), namespace prefix (none),
  #   metric filterlist (empty), tag filterlist (empty), post-aggregation filter (empty), enrichment
  #   merge (one input), host enrichment (changes no metric)

  # Sources, where each one's metrics enter the program: dogstatsd.

  # Timestamped points skip aggregation (no-aggregation pipeline)
  let dsd_no_agg_split = dogstatsd | timestamp_split(1s)

  # Aggregation; edits keep its state unless they change its window or lineage
  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)

  # Aggregated and timestamped points rejoin
  let dsd_post_agg_filter_in = (dsd_agg, dsd_no_agg_split.passthrough)

  # Copies every metric to the client telemetry tap and to enrichment
  let dsd_post_agg_filter = dsd_post_agg_filter_in

  dsd_post_agg_filter | emit output dsd_client_telemetry

  # Policy: one branch per output; route here
  let metrics = dsd_post_agg_filter

  # Recipients:
  #   primary                   Primary         http://127.0.0.1:17101
  #   mrf                       Failover (MRF)  http://127.0.0.1:17104
  #   "http://127.0.0.1:17102"  Team B
  #   "http://127.0.0.1:17103"  Team C
  metrics | emit [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]

  # Multi-Region Failover: mirrors listed, or all, metrics while failing over
  let mrf_metrics_gateway = metrics
    | where config.multi_region_failover.enabled and config.multi_region_failover.metric_mirroring.enabled

  mrf_metrics_gateway | emit mrf
}
"#;

/// The `let` that names the stream every destination reads in [`PRINTED`].
const POLICY_LET: &str = "  let metrics = dsd_post_agg_filter";

/// The statement of the main `emit` in [`PRINTED`].
const MAIN_EMIT: &str = "  metrics | emit [primary, \"http://127.0.0.1:17102\", \"http://127.0.0.1:17103\"]";

/// Returns [`PRINTED`] as the default program prints with host tags: `let metrics` ends in them.
fn printed_with_host_tags() -> String {
    PRINTED.replace(
        &format!("{}\n", POLICY_LET),
        "  let metrics = dsd_post_agg_filter\n    | host_tags(config.host.tags, deadline: config.host.tags_deadline)\n",
    )
}

/// Returns the lines `edited` has that `source` does not, and the lines it lost, in order.
fn changed_lines<'a>(source: &'a str, edited: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
    let old = source.lines().collect::<Vec<_>>();
    let new = edited.lines().collect::<Vec<_>>();
    let added = new.iter().filter(|line| !old.contains(line)).copied().collect();
    let removed = old.iter().filter(|line| !new.contains(line)).copied().collect();
    (added, removed)
}

fn parses(case: &str, edited: &str) -> saluki_metric_plan::ir::PolicyArtifact {
    saluki_metric_plan_syntax::parse(edited)
        .unwrap_or_else(|diagnostics| panic!("{}: {}\n{}", case, diagnostics, edited))
        .artifact
}

/// Returns an artifact's node IDs, sorted.
fn ids(artifact: &saluki_metric_plan::ir::PolicyArtifact) -> Vec<String> {
    let mut ids = artifact
        .nodes
        .iter()
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Each edit, with the lines it adds and removes, then each edited text parsed: the lines are checked first, so that a
/// change of the edits shows up apart from a change of the syntax.
#[test]
fn each_edit_changes_the_lines_its_scenario_describes() {
    let recipe = narrow_team_b(PRINTED, TEAM_B).unwrap();
    type Edit = Box<dyn Fn(&str) -> Result<String, GenericError>>;
    /// A case: its name, the source it edits, the edit, and the lines it adds and removes.
    type Case<'a> = (&'a str, &'a str, Edit, &'a [&'a str], &'a [&'a str]);
    let cases: Vec<Case> = vec![
        (
            "S1",
            PRINTED,
            Box::new(drop_debug),
            &["    | where not (name starts_with \"demo.debug.\")"],
            &[],
        ),
        (
            "S2, the recipe",
            PRINTED,
            Box::new(|source| narrow_team_b(source, TEAM_B)),
            &[
                "  metrics | emit [primary, \"http://127.0.0.1:17103\"]",
                "  let to_team_b = metrics | where name starts_with \"demo.checkout.\"",
                "  to_team_b | emit \"http://127.0.0.1:17102\"",
            ],
            &[MAIN_EMIT],
        ),
        (
            "S3a",
            &recipe,
            Box::new(widen_team_b),
            &["  let to_team_b = metrics | where name starts_with \"demo.checkout.\" or tags.team == \"platform\""],
            &["  let to_team_b = metrics | where name starts_with \"demo.checkout.\""],
        ),
        (
            "S4a",
            PRINTED,
            Box::new(mirror_checkout_on_failover),
            &["    | where config.multi_region_failover.enabled and \
               config.multi_region_failover.metric_mirroring.enabled and name == \"demo.checkout.requests\""],
            &["    | where config.multi_region_failover.enabled and \
               config.multi_region_failover.metric_mirroring.enabled"],
        ),
        // The duplicated branch is a statement the source already has.
        ("S5a", PRINTED, Box::new(duplicate_branch), &[], &[]),
        (
            "S5b",
            PRINTED,
            Box::new(two_hosts),
            &["  mrf_metrics_gateway | emit [primary, mrf]"],
            &["  mrf_metrics_gateway | emit mrf"],
        ),
        (
            "S5e",
            PRINTED,
            Box::new(policy_binding),
            &["  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s, \
               histogram_percentiles: config.metrics_encoding.histogram.percentiles)"],
            &["  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)"],
        ),
        (
            "S5f",
            PRINTED,
            Box::new(unknown_recipient),
            &["  metrics | emit team_z"],
            &[],
        ),
        (
            "S9c",
            &recipe,
            Box::new(|source| route_platform_to(source, "http://127.0.0.1:50001")),
            &[
                "  let to_destination = metrics | where tags.team == \"platform\"",
                "  to_destination | emit \"http://127.0.0.1:50001\"",
            ],
            &[],
        ),
        (
            "S13a",
            PRINTED,
            Box::new(|source| width_change(source, "30s")),
            &[
                "  let dsd_no_agg_split = dogstatsd | timestamp_split(30s)",
                "  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 30s, flush_interval: 1s, counter_expiry: \
                 0s)",
            ],
            &[
                "  let dsd_no_agg_split = dogstatsd | timestamp_split(1s)",
                "  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)",
            ],
        ),
        (
            "S14a",
            PRINTED,
            Box::new(rename_aggregation),
            &[
                "  let agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)",
                "  let dsd_post_agg_filter_in = (agg, dsd_no_agg_split.passthrough)",
            ],
            &[
                "  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)",
                "  let dsd_post_agg_filter_in = (dsd_agg, dsd_no_agg_split.passthrough)",
            ],
        ),
    ];
    let mut edited_texts = Vec::new();
    for (case, source, edit, added, removed) in cases {
        let edited = edit(source).unwrap();
        let (found_added, found_removed) = changed_lines(source, &edited);
        assert_eq!(
            (found_added.as_slice(), found_removed.as_slice()),
            (added, removed),
            "{}:\n{}",
            case,
            edited
        );
        // A stage added for the policy goes ahead of every branch, above the recipient directory.
        if let (Some(at), Some(added)) = (edited.find("# Recipients:"), edited.find(DROP_DEBUG)) {
            assert!(
                added < at,
                "{}: the stage is not ahead of the policy:\n{}",
                case,
                edited
            );
        }
        edited_texts.push((case, edited));
    }

    // The duplicated branch is the main `emit` again.
    let duplicated = duplicate_branch(PRINTED).unwrap();
    assert_eq!(duplicated.matches(&format!("{}\n", MAIN_EMIT)).count(), 2);

    for (case, edited) in edited_texts {
        parses(case, &edited);
    }
}

#[test]
fn a_stage_ends_let_metrics_and_the_policy_tee_keeps_its_id() {
    for (case, source) in [
        ("without host tags", PRINTED.to_string()),
        ("with host tags", printed_with_host_tags()),
    ] {
        let default = parses(case, &source);
        let edited = drop_debug(&source).unwrap();
        let artifact = parses(case, &edited);
        let node = |id: &str| artifact.nodes.iter().find(|node| node.id.as_str() == id);
        // The `where` ends `let metrics`, so it is the node `metrics`, and the tee its uses read keeps its ID.
        let split = node("metrics").unwrap_or_else(|| panic!("{}: no `metrics`\n{}", case, edited));
        assert_eq!(split.op.as_str(), "split", "{}", case);
        assert!(is_drop_debug(&serde_json::to_value(split).unwrap()), "{}", case);
        assert_eq!(node("metrics_fanout").unwrap().op.as_str(), "tee", "{}", case);
        assert!(default.nodes.iter().any(|node| node.id.as_str() == "metrics_fanout"));
    }

    // With host tags, they are the stage before the `where` now: `metrics_host_tags`.
    let edited = drop_debug(&printed_with_host_tags()).unwrap();
    assert!(
        edited.contains(
            "    | host_tags(config.host.tags, deadline: config.host.tags_deadline)\n    | where not (name starts_with \
             \"demo.debug.\")\n\n"
        ),
        "{}",
        edited
    );
    let artifact = parses("S1, with host tags", &edited);
    assert!(artifact
        .nodes
        .iter()
        .any(|node| node.id.as_str() == "metrics_host_tags" && node.op.as_str() == "host_tags"));

    // A branch is a statement of its own that reads `metrics`, and adds only its own nodes.
    let default = parses("default", PRINTED);
    let artifact = parses("S2", &narrow_team_b(PRINTED, TEAM_B).unwrap());
    let mut expected = ids(&default);
    expected.extend([
        "to_team_b".to_string(),
        "to_team_b_true".to_string(),
        "to_team_b_false".to_string(),
    ]);
    expected.sort();
    assert_eq!(ids(&artifact), expected);
}

#[test]
fn the_exclusive_route_excludes_checkout_from_every_other_branch() {
    // Each branch excludes the checkout metrics first, and a branch of their own sends them to team B.
    let edited = exclusive_team_b(PRINTED, TEAM_B).unwrap();
    let (added, removed) = changed_lines(PRINTED, &edited);
    assert_eq!(
        added,
        [
            "  metrics | where tags.team != \"checkout\" | emit [primary, \"http://127.0.0.1:17102\", \
             \"http://127.0.0.1:17103\"]",
            "  let checkout_only = metrics | where tags.team == \"checkout\"",
            "  checkout_only | emit \"http://127.0.0.1:17102\"",
            "  let mrf_metrics_gateway = metrics | where tags.team != \"checkout\"",
        ],
        "{}",
        edited
    );
    assert_eq!(removed, [MAIN_EMIT, "  let mrf_metrics_gateway = metrics"]);
    // The policy's tee and the gateway keep their IDs.
    let after = ids(&parses("S3b", &edited));
    for id in [
        "metrics_fanout",
        "mrf_metrics_gateway",
        "checkout_only",
        "checkout_only_true",
    ] {
        assert!(after.contains(&id.to_string()), "{} in {:?}", id, after);
    }
    parses(
        "S3b, with host tags",
        &exclusive_team_b(&printed_with_host_tags(), TEAM_B).unwrap(),
    );
}

#[test]
fn a_stage_goes_at_the_end_of_let_metrics() {
    let stage = "where tags.env != \"dev\"";
    // At the end of `let metrics`, whichever operator ends it, or none, on a line of its own.
    let named = "  let metrics = x\n\n  metrics | emit primary\n\n  metrics | drop\n";
    assert_eq!(
        append_to_metrics(named, stage).unwrap(),
        "  let metrics = x\n    | where tags.env != \"dev\"\n\n  metrics | emit primary\n\n  metrics | drop\n"
    );
    let reuse = "  let metrics = (a, b) | host_enrichment\n\n  metrics | emit primary\n\n  metrics | drop\n";
    assert_eq!(
        append_to_metrics(reuse, stage).unwrap(),
        "  let metrics = (a, b) | host_enrichment\n    | where tags.env != \"dev\"\n\n  metrics | emit primary\n\n  \
         metrics | drop\n"
    );
    let broken = "  let metrics = (a, b)\n    | host_tags([])\n\n  metrics | emit primary\n";
    assert_eq!(
        append_to_metrics(broken, stage).unwrap(),
        "  let metrics = (a, b)\n    | host_tags([])\n    | where tags.env != \"dev\"\n\n  metrics | emit primary\n"
    );
    // A name in a string or a comment is not the stream.
    let error = append_to_metrics("  # let metrics = x\n  x | emit \"let metrics = x\"\n", stage)
        .unwrap_err()
        .to_string();
    assert!(error.contains("let metrics = ..."), "{}", error);
}

#[test]
fn the_recipe_routes_checkout_to_team_b_from_a_branch_of_its_own() {
    let artifact = parses("S2", &narrow_team_b(PRINTED, TEAM_B).unwrap());
    let emits = artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == "emit")
        .map(|node| (node.id.to_string(), node.params.get("to").cloned()))
        .collect::<Vec<_>>();
    assert!(emits.contains(&("to_team_b_true".to_string(), Some(serde_json::json!([TEAM_B])))));
    assert!(emits.contains(&(
        "metrics_fanout_0".to_string(),
        Some(serde_json::json!(["primary", "http://127.0.0.1:17103"]))
    )));
}

#[test]
fn an_endpoint_the_main_emit_does_not_name_cannot_be_narrowed() {
    let error = narrow_team_b(PRINTED, "http://127.0.0.1:50001")
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not name team B"), "{}", error);
    // Narrowed to one recipient, the main `emit` names it bare.
    let one = PRINTED.replace(MAIN_EMIT, "  metrics | emit [primary, \"http://127.0.0.1:17102\"]");
    assert!(narrow_team_b(&one, TEAM_B)
        .unwrap()
        .contains("\n  metrics | emit primary\n"));
}

#[test]
fn the_aggregation_moves_after_enrichment_and_its_feeder_feeds_what_it_fed() {
    let edited = aggregation_after_enrichment(PRINTED).unwrap();
    // The `let` that held only the aggregation goes, its comment with it, and its use reads what it read.
    assert!(!edited.contains("let dsd_agg"), "{}", edited);
    assert!(!edited.contains("# Aggregation"), "{}", edited);
    assert!(
        edited.contains("  let dsd_post_agg_filter_in = (dsd_no_agg_split.default, dsd_no_agg_split.passthrough)"),
        "{}",
        edited
    );
    let moved =
        "  let metrics = dsd_post_agg_filter\n    | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s) as \
                 dsd_agg\n";
    assert!(edited.contains(moved), "{}", edited);

    // A `let` with a stage before the aggregation keeps that stage, which names the `let` instead.
    let filtered = PRINTED.replace(
        "  let dsd_agg = dsd_no_agg_split.default | aggregate(",
        "  let dsd_agg = dsd_no_agg_split.default\n    | tag_projection(rules: []) as dsd_tag_filterlist\n    | aggregate(",
    );
    let edited_filtered = aggregation_after_enrichment(&filtered).unwrap();
    assert!(
        edited_filtered
            .contains("  let dsd_tag_filterlist = dsd_no_agg_split.default\n    | tag_projection(rules: [])\n\n"),
        "{}",
        edited_filtered
    );
    assert!(
        edited_filtered.contains("(dsd_tag_filterlist, dsd_no_agg_split.passthrough)"),
        "{}",
        edited_filtered
    );
    parses("S5d, with a feeder", &edited_filtered);

    // The aggregation ends `let metrics` and keeps its ID, and the policy's tee reads it.
    let parsed = parses("S5d", &edited);
    let edges = parsed
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    for edge in [
        "dsd_no_agg_split.default -> dsd_post_agg_filter_in.0",
        "dsd_agg.out -> dsd_agg_fanout.in",
    ] {
        assert!(edges.iter().any(|found| found == edge), "{} in {:?}", edge, edges);
    }
    assert!(parsed
        .nodes
        .iter()
        .any(|node| node.id.as_str() == "dsd_agg" && node.op.as_str() == "aggregate"));
    assert_eq!(parsed.nodes.len(), parses("default", PRINTED).nodes.len());
}

#[test]
fn the_width_change_sets_both_windows_to_the_literal() {
    let edited = width_change(PRINTED, "30s").unwrap();
    assert!(edited.contains("timestamp_split(30s)"), "{}", edited);
    assert!(edited.contains("aggregate(window: 30s,"), "{}", edited);
    // A window written by name, or broken across lines, is found as well.
    let named = PRINTED.replace("timestamp_split(1s)", "timestamp_split(\n      window: 1s,\n    )");
    let edited = width_change(&named, "30s").unwrap();
    assert!(
        edited.contains("timestamp_split(\n      window: 30s,\n    )"),
        "{}",
        edited
    );
    parses("S13a", &edited);
}

#[test]
fn an_edit_that_cannot_find_its_lines_says_what_it_looked_for() {
    let error = drop_debug("policy {\n}\n").unwrap_err().to_string();
    assert!(error.contains("let metrics = ..."), "{}", error);
    let error = two_hosts("policy {\n}\n").unwrap_err().to_string();
    assert!(error.contains("failover gateway"), "{}", error);
    let error = widen_team_b(PRINTED).unwrap_err().to_string();
    assert!(error.contains("route to team B"), "{}", error);
    let error = width_change("policy {\n}\n", "30s").unwrap_err().to_string();
    assert!(error.contains("timestamp_split"), "{}", error);
}

#[test]
fn a_where_edit_refuses_a_let_that_does_not_end_in_one() {
    // The `where` is not the `let`'s last stage, so the edit refuses rather than rewrite the wrong stage.
    let source = "  let tags = dogstatsd | where name == \"a|b\" | tag_projection(rules: [])\n\n  tags | drop\n";
    let error = edit_where(source, "tags", "tag stage", |predicate| Ok(predicate.to_string())).unwrap_err();
    assert!(
        error.to_string().contains("does not end in a `where` stage"),
        "{}",
        error
    );

    // A `|` inside a string is not a stage boundary.
    let source = "  let named = dogstatsd | where name == \"a|b\"\n\n  named | drop\n";
    let edited = edit_where(source, "named", "filter", |predicate| {
        Ok(format!("not ({})", predicate))
    })
    .unwrap();
    assert_eq!(
        edited,
        "  let named = dogstatsd | where not (name == \"a|b\")\n\n  named | drop\n"
    );
}
