use super::*;

/// Team B's URL, as the default program names its recipient.
const TEAM_B: &str = "http://127.0.0.1:17102";

/// The default program of the demo's configuration, detached, as the editor prints it, with its base: what does
/// nothing as configured left out and noted at the top, the aggregation's window and the demo's short flush and expiry,
/// the policy `fork` a branch of the `fork` that copies every metric to the client telemetry tap, the main `emit`
/// naming the primary endpoint and teams B and C, and the failover gateway reading its two signals.
const PRINTED: &str = r#"policy metrics {
  # Not shown, because they do nothing as configured: mapper (no profiles), namespace prefix (none),
  #   metric filterlist (empty), tag filterlist (empty), post-aggregation filter (empty), enrichment
  #   merge (one input), host enrichment (changes no metric)

  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"

  # Sources: each one's metrics enter at an input of the scope: dogstatsd.

  # Timestamped points skip aggregation (no-aggregation pipeline)
  let dsd_no_agg_split = dogstatsd | timestamp_split(1s)

  # Aggregation; edits keep its state unless they change its window or lineage
  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)

  # Aggregated and timestamped points rejoin
  # Copies every metric to the client telemetry tap and to enrichment
  (dsd_agg, dsd_no_agg_split.passthrough) as dsd_post_agg_filter_in | fork as dsd_post_agg_filter_fanout {
    emit output dsd_client_telemetry
    # Recipients:
    #   primary                   Primary         http://127.0.0.1:17101
    #   mrf                       Failover (MRF)  http://127.0.0.1:17104
    #   "http://127.0.0.1:17102"  Team B
    #   "http://127.0.0.1:17103"  Team C
    # Policy: one branch per output; route here
    fork as metrics_policy {
      emit [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]
      # Multi-Region Failover: mirrors listed, or all, metrics while failing over
      where config.multi_region_failover.enabled and config.multi_region_failover.metric_mirroring.enabled
          as mrf_metrics_gateway
        | emit mrf
    }
  }
}
"#;

/// The line of the main `emit`.
const MAIN_EMIT: &str = "      emit [primary, \"http://127.0.0.1:17102\", \"http://127.0.0.1:17103\"]";

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
            &[
                "    where not (name starts_with \"demo.debug.\") as drop_debug",
                "    | fork as metrics_policy {",
            ],
            &["    fork as metrics_policy {"],
        ),
        (
            "S2, the recipe",
            PRINTED,
            Box::new(|source| narrow_team_b(source, TEAM_B)),
            &[
                "      emit [primary, \"http://127.0.0.1:17103\"]",
                "      where name starts_with \"demo.checkout.\" as to_team_b | emit \"http://127.0.0.1:17102\"",
            ],
            &[MAIN_EMIT],
        ),
        (
            "S3a",
            &recipe,
            Box::new(widen_team_b),
            &["      where name starts_with \"demo.checkout.\" or tags.team == \"platform\" as to_team_b | emit \
               \"http://127.0.0.1:17102\""],
            &["      where name starts_with \"demo.checkout.\" as to_team_b | emit \"http://127.0.0.1:17102\""],
        ),
        (
            "S4a",
            PRINTED,
            Box::new(mirror_checkout_on_failover),
            &["      where config.multi_region_failover.enabled and \
               config.multi_region_failover.metric_mirroring.enabled and name == \"demo.checkout.requests\""],
            &["      where config.multi_region_failover.enabled and \
               config.multi_region_failover.metric_mirroring.enabled"],
        ),
        // The duplicated branch is a line the source already has.
        ("S5a", PRINTED, Box::new(duplicate_branch), &[], &[]),
        (
            "S5b",
            PRINTED,
            Box::new(two_hosts),
            &["        | emit [primary, mrf]"],
            &["        | emit mrf"],
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
            &["      emit team_z"],
            &[],
        ),
        (
            "S9c",
            &recipe,
            Box::new(|source| route_platform_to(source, "http://127.0.0.1:50001")),
            &["      where tags.team == \"platform\" as to_destination | emit \"http://127.0.0.1:50001\""],
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
                "  (agg, dsd_no_agg_split.passthrough) as dsd_post_agg_filter_in | fork as dsd_post_agg_filter_fanout {",
            ],
            &[
                "  let dsd_agg = dsd_no_agg_split.default | aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s)",
                "  (dsd_agg, dsd_no_agg_split.passthrough) as dsd_post_agg_filter_in | fork as dsd_post_agg_filter_fanout \
                 {",
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
        // A stage added for the policy goes above the policy section's comments.
        if let (Some(at), Some(added)) = (edited.find("# Recipients:"), edited.find("as drop_debug")) {
            assert!(
                added < at,
                "{}: the stage is not above the policy section:\n{}",
                case,
                edited
            );
        }
        edited_texts.push((case, edited));
    }

    let duplicated = duplicate_branch(PRINTED).unwrap();
    assert_eq!(duplicated.matches(&format!("{}\n", MAIN_EMIT)).count(), 2);

    for (case, edited) in edited_texts {
        parses(case, &edited);
    }
}

#[test]
fn the_exclusive_route_gives_a_policy_fork_in_a_branch_a_statement_of_its_own() {
    let edited = exclusive_team_b(PRINTED, TEAM_B).unwrap();
    let start = edited.find("  let dsd_post_agg_filter_fanout = ").unwrap();
    let end = edited.find("    | fork as metrics_policy {\n").unwrap();
    assert_eq!(
        &edited[start..end],
        "  let dsd_post_agg_filter_fanout = (dsd_agg, dsd_no_agg_split.passthrough) as dsd_post_agg_filter_in | \
         tee(ports: 2)\n\n  dsd_post_agg_filter_fanout.0 | emit output dsd_client_telemetry\n\n  # Recipients:\n  #   \
         primary                   Primary         http://127.0.0.1:17101\n  #   mrf                       Failover \
         (MRF)  http://127.0.0.1:17104\n  #   \"http://127.0.0.1:17102\"  Team B\n  #   \"http://127.0.0.1:17103\"  \
         Team C\n  # Policy: one branch per output; route here\n  let checkout_only = dsd_post_agg_filter_fanout.1 | \
         split(tags.team == \"checkout\")\n\n  checkout_only.true | emit \"http://127.0.0.1:17102\"\n\n  \
         checkout_only.false\n"
    );
    // The same nodes, with the same IDs, and the two the route adds.
    let ids = |artifact: &saluki_metric_plan::ir::PolicyArtifact| {
        let mut ids = artifact
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    let mut expected = ids(&parses("default", PRINTED));
    expected.extend(["checkout_only".to_string(), "checkout_only_true".to_string()]);
    expected.sort();
    assert_eq!(ids(&parses("S3b", &edited)), expected);

    // A policy `fork` that ends a statement of its own takes the split on the same statement.
    let own =
        "policy metrics {\n  base \"sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\"\n\n  \
               dogstatsd\n    | fork as metrics_policy {\n      emit primary\n    }\n}\n";
    let edited = exclusive_team_b(own, TEAM_B).unwrap();
    assert!(
        edited.contains(
            "  let checkout_only = dogstatsd\n    | split(tags.team == \"checkout\")\n\n  checkout_only.true | emit \
             \"http://127.0.0.1:17102\"\n\n  checkout_only.false\n    | fork as metrics_policy {\n"
        ),
        "{}",
        edited
    );
    parses("S3b, own statement", &edited);
}

#[test]
fn a_stage_goes_before_the_policy_fork_wherever_the_fork_starts() {
    let stage = "where tags.env != \"dev\"";
    // On a line of its own, above the comments that belong to the fork.
    let own_line = "  x\n    # Policy\n    | fork as metrics_policy {\n        emit primary\n      }\n";
    assert_eq!(
        insert_before_policy(own_line, stage).unwrap(),
        "  x\n    | where tags.env != \"dev\"\n    # Policy\n    | fork as metrics_policy {\n        emit primary\n      \
         }\n"
    );
    // A branch of another fork: the stage starts the branch, and the fork follows it.
    let branch =
        "  x\n    | fork as outer {\n        emit output tap\n        fork as metrics_policy {\n          emit \
                  primary\n        }\n      }\n";
    assert_eq!(
        insert_before_policy(branch, stage).unwrap(),
        "  x\n    | fork as outer {\n        emit output tap\n        where tags.env != \"dev\"\n        | fork as \
         metrics_policy {\n          emit primary\n        }\n      }\n"
    );
    // After the head on one line.
    let inline = "  x | fork as metrics_policy {\n      emit primary\n    }\n";
    assert_eq!(
        insert_before_policy(inline, stage).unwrap(),
        "  x | where tags.env != \"dev\" | fork as metrics_policy {\n      emit primary\n    }\n"
    );
    // A name in a string or a comment is not the fork.
    let error = insert_before_policy(
        "  # fork as metrics_policy\n  x | emit \"fork as metrics_policy\"\n",
        stage,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("fork as metrics_policy"), "{}", error);
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
        "metrics_policy_0".to_string(),
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
    let one = PRINTED.replace(MAIN_EMIT, "        emit [primary, \"http://127.0.0.1:17102\"]");
    assert!(narrow_team_b(&one, TEAM_B)
        .unwrap()
        .contains("\n        emit primary\n"));
}

#[test]
fn the_aggregation_moves_after_enrichment_and_its_feeder_feeds_what_it_fed() {
    let edited = aggregation_after_enrichment(PRINTED).unwrap();
    // The `let` that held only the aggregation goes, its comment with it, and its use reads what it read.
    assert!(!edited.contains("let dsd_agg"), "{}", edited);
    assert!(!edited.contains("# Aggregation"), "{}", edited);
    assert!(
        edited.contains("  (dsd_no_agg_split.default, dsd_no_agg_split.passthrough) as dsd_post_agg_filter_in | fork"),
        "{}",
        edited
    );
    let moved = "    aggregate(window: 1s, flush_interval: 1s, counter_expiry: 0s) as dsd_agg\n";
    assert!(edited.contains(moved), "{}", edited);
    assert!(edited.find(moved).unwrap() < edited.find("    # Recipients:").unwrap());

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

    let parsed = parses("S5d", &edited);
    let edges = parsed
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    for edge in [
        "dsd_no_agg_split.default -> dsd_post_agg_filter_in.0",
        "dsd_post_agg_filter_fanout.1 -> dsd_agg.in",
        "dsd_agg.out -> metrics_policy.in",
    ] {
        assert!(edges.iter().any(|found| found == edge), "{} in {:?}", edge, edges);
    }
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
    let error = drop_debug("policy metrics {\n}\n").unwrap_err().to_string();
    assert!(error.contains("fork as metrics_policy"), "{}", error);
    let error = two_hosts("policy metrics {\n}\n").unwrap_err().to_string();
    assert!(error.contains("failover gateway"), "{}", error);
    let error = widen_team_b(PRINTED).unwrap_err().to_string();
    assert!(error.contains("route to team B"), "{}", error);
    let error = width_change("policy metrics {\n}\n", "30s").unwrap_err().to_string();
    assert!(error.contains("timestamp_split"), "{}", error);
}

#[test]
fn a_where_edit_refuses_an_id_that_names_another_stage() {
    // The last `where` before `as tags` belongs to another stage, so the edit refuses rather than rewrite both.
    let source = "  dogstatsd | where name == \"a|b\" | tag_projection(rules: []) as tags | drop\n";
    let error = edit_where(source, "tags", "tag stage", |predicate| Ok(predicate.to_string())).unwrap_err();
    assert!(error.to_string().contains("is not a `where` stage"), "{}", error);

    // A `|` inside a string is not a stage boundary.
    let source = "  dogstatsd | where name == \"a|b\" as named | drop\n";
    let edited = edit_where(source, "named", "filter", |predicate| {
        Ok(format!("not ({})", predicate))
    })
    .unwrap();
    assert_eq!(edited, "  dogstatsd | where not (name == \"a|b\") as named | drop\n");
}
