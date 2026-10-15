use super::*;

/// Team B's URL, as the default program names its recipient.
const TEAM_B: &str = "http://127.0.0.1:17102";

/// The default program of the demo's configuration, detached and trimmed, as the editor prints it: what does nothing as
/// configured left out and noted at the top, the main `emit` names the primary endpoint and teams B and C, and the
/// failover gateway reads its two signals.
const PRINTED: &str = r#"policy metrics {
  # Not shown, because they do nothing as configured: mapper (no
  #   profiles), namespace prefix (none), metric filterlist (empty)

  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"

  # Sources: where each one's metrics enter
  input.dogstatsd -> dsd_no_agg_split

  # Timestamped points skip aggregation (no-aggregation pipeline)
  node dsd_no_agg_split = timestamp_split(window: 1s) {
    default     -> dsd_agg
    passthrough -> dsd_post_agg_filter_in.1
  }

  # Aggregation; edits keep its state unless they change its window or lineage
  node dsd_agg = aggregate(
    window: 1s,
    flush_interval: 1s,
    context_limit: 1000000,
    counter_expiry: 0s,
    flush_open_windows: false,
    histogram_aggregates: ["max", "median", "avg", "count"],
    histogram_percentiles: [95],
    copy_to_distribution: false,
    copy_to_distribution_prefix: "",
  ) {
    -> dsd_post_agg_filter_in.0
  }

  # Aggregated and timestamped points rejoin
  node dsd_post_agg_filter_in = merge(inputs: 2) {
    -> dsd_post_agg_filter_fanout
  }

  # Copies every metric to the client telemetry tap and to enrichment
  node dsd_post_agg_filter_fanout = tee {
    -> emit dsd_client_telemetry
    -> metrics_policy
  }

  # Recipients:
  #   http://127.0.0.1:17102  (Team B)
  #   http://127.0.0.1:17103  (Team C)
  #   mrf  http://127.0.0.1:17104
  #   primary  http://127.0.0.1:17101
  # Policy: one branch per output; route here
  node metrics_policy = tee {
    -> emit to [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]
    -> mrf_metrics_gateway
  }

  # Multi-Region Failover: mirrors listed, or all, metrics while failing over
  node mrf_metrics_gateway = split(
    flag(config("multi_region_failover.enabled"))
    and flag(config("multi_region_failover.metric_mirroring.enabled"))
  ) {
    true  -> emit to [mrf]
    false -> drop
  }
}
"#;

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
                "    -> drop_debug",
                "  node drop_debug = split(name starts_with \"demo.debug.\") {",
                "    true  -> drop",
                "    false -> metrics_policy",
            ],
            &["    -> metrics_policy"],
        ),
        (
            "S2, the recipe",
            PRINTED,
            Box::new(|source| narrow_team_b(source, TEAM_B)),
            &[
                "  node to_team_b = split(name starts_with \"demo.checkout.\") {",
                "    true  -> emit to [\"http://127.0.0.1:17102\"]",
                "    -> emit to [primary, \"http://127.0.0.1:17103\"]",
                "    -> to_team_b",
            ],
            &["    -> emit to [primary, \"http://127.0.0.1:17102\", \"http://127.0.0.1:17103\"]"],
        ),
        (
            "S3a",
            &recipe,
            Box::new(widen_team_b),
            &[
                "  node to_team_b = split(",
                "    name starts_with \"demo.checkout.\"",
                "    or tag(\"team\") == \"platform\"",
            ],
            &["  node to_team_b = split(name starts_with \"demo.checkout.\") {"],
        ),
        (
            "S3b",
            PRINTED,
            Box::new(|source| exclusive_team_b(source, TEAM_B)),
            &[
                "    -> checkout_only",
                "  node checkout_only = split(tag(\"team\") == \"checkout\") {",
                "    true  -> emit to [\"http://127.0.0.1:17102\"]",
                "    false -> metrics_policy",
            ],
            &["    -> metrics_policy"],
        ),
        (
            "S4a",
            PRINTED,
            Box::new(mirror_checkout_on_failover),
            &["    and name in [\"demo.checkout.requests\"]"],
            &[],
        ),
        // The duplicated branch is a line the source already has.
        ("S5a", PRINTED, Box::new(duplicate_branch), &[], &[]),
        (
            "S5b",
            PRINTED,
            Box::new(two_hosts),
            &["    true  -> emit to [primary, mrf]"],
            &["    true  -> emit to [mrf]"],
        ),
        (
            "S5e",
            PRINTED,
            Box::new(policy_binding),
            &["    histogram_percentiles: config(\"metrics_encoding.histogram.percentiles\"),"],
            &["    histogram_percentiles: [95],"],
        ),
        (
            "S5f",
            PRINTED,
            Box::new(unknown_recipient),
            &["    -> emit to [team_z]"],
            &[],
        ),
        (
            "S9c",
            &recipe,
            Box::new(|source| route_platform_to(source, "http://127.0.0.1:50001")),
            &[
                "  node to_destination = split(tag(\"team\") == \"platform\") {",
                "    true  -> emit to [\"http://127.0.0.1:50001\"]",
                "    -> to_destination",
            ],
            &[],
        ),
        (
            "S14a",
            PRINTED,
            Box::new(rename_aggregation),
            &["    default     -> agg", "  node agg = aggregate("],
            &["    default     -> dsd_agg", "  node dsd_agg = aggregate("],
        ),
    ];
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
        parses(case, &edited);
        // A node added for the policy goes above the policy section's comments.
        if let Some(at) = edited.find("  # Recipients:") {
            for node in ["drop_debug", "checkout_only", "rollup", TEAM_B_ROUTE, DESTINATION_ROUTE] {
                if let Some(defined) = edited.find(&format!("\n  node {} = ", node)) {
                    assert!(
                        defined < at,
                        "{}: `{}` is not above the policy section:\n{}",
                        case,
                        node,
                        edited
                    );
                }
            }
        }
    }

    let duplicated = duplicate_branch(PRINTED).unwrap();
    assert_eq!(
        duplicated
            .matches("    -> emit to [primary, \"http://127.0.0.1:17102\", \"http://127.0.0.1:17103\"]\n")
            .count(),
        2
    );
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
    // An endpoint the main `emit` does not name cannot be narrowed.
    let error = narrow_team_b(PRINTED, "http://127.0.0.1:50001")
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not name team B"), "{}", error);
}

#[test]
fn the_aggregation_moves_after_enrichment_and_its_feeder_feeds_what_it_fed() {
    let edited = aggregation_after_enrichment(PRINTED).unwrap();
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
    // The aggregation's section comment goes with it; it is defined right above the policy section.
    assert!(!edited.contains("# Aggregation"), "{}", edited);
    assert!(
        edited.find("\n  node dsd_agg = ").unwrap() > edited.find("\n  node dsd_post_agg_filter_fanout = ").unwrap()
    );
    assert!(edited.find("\n  node dsd_agg = ").unwrap() < edited.find("  # Recipients:").unwrap());
}

#[test]
fn the_width_change_sets_both_windows_to_the_literal() {
    let edited = width_change(PRINTED, "30s").unwrap();
    assert!(edited.contains("timestamp_split(window: 30s)"), "{}", edited);
    assert!(edited.contains("\n    window: 30s,\n"), "{}", edited);
    parses("S13a", &edited);
}

#[test]
fn an_edit_that_cannot_find_its_lines_says_what_it_looked_for() {
    let error = drop_debug("policy metrics {\n}\n").unwrap_err().to_string();
    assert!(error.contains("-> metrics_policy"), "{}", error);
    let error = two_hosts("policy metrics {\n}\n").unwrap_err().to_string();
    assert!(error.contains("failover gateway"), "{}", error);
    let error = widen_team_b(PRINTED).unwrap_err().to_string();
    assert!(error.contains("route to team B"), "{}", error);
}
