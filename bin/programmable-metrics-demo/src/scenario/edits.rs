//! The edits of the scenarios, made to the default program's source as the editor shows it, the way a presenter
//! types them.
//!
//! The default program is the whole metrics path, its one scope fed by every source; the policy region starts at its
//! `tee`, [`POLICY_NODE`], which enrichment feeds. Each edit finds the lines it changes by what they say (the edge into
//! the policy `tee`, an inline `emit` of an output, a `tee` branch, a node's definition), never by line number, and
//! fails with a message naming what it looked for when the default program is printed otherwise, so that a change of
//! the printed form shows up as a clear check failure.

use saluki_error::{generic_error, GenericError};

/// The family the mirroring scenario allowlists, which the failover endpoint then receives.
pub const MIRRORED_METRIC: &str = "demo.checkout.requests";

/// The output the undeclared-output guardrail emits to, which ADP does not declare.
pub const UNDECLARED_OUTPUT: &str = "metrics_routing_encode_99";

/// The node the team D routing scenarios add, which emits the platform family to team D's declared output.
pub const TEAM_D_ROUTE: &str = "team_d_route";

/// The family the team D routing scenarios route to team D.
pub const TEAM_D_FAMILY: &str = "demo.platform.queue_depth";

/// The outputs of the default program that feed each intake, from the export's legend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outputs {
    /// The output that feeds the primary endpoint.
    pub primary: String,

    /// The output that feeds team B's endpoint (the `demo.checkout.` route).
    pub team_b: String,

    /// The output that feeds team C's endpoint (the `demo.catalog.` route).
    pub team_c: String,

    /// The output that feeds the multi-region failover endpoint.
    pub mrf: String,
}

/// The policy `tee`, where the policy region begins: enrichment feeds it, and it has one branch per output.
pub const POLICY_NODE: &str = "metrics_policy";

/// DogStatsD aggregation, the program's one aggregation.
const AGGREGATION_NODE: &str = "dsd_agg";

/// Returns where the definition of `node` starts in `source`: the start of its `node <id> = ` line, or of the comment
/// lines right above it, which mark the section it begins.
fn definition_start(source: &str, node: &str) -> Result<usize, GenericError> {
    let header = format!("\n  node {} = ", node);
    let mut start = source
        .find(&header)
        .map(|at| at + 1)
        .ok_or_else(|| generic_error!("The default program has no node `{}`.", node))?;
    while let Some(previous) = source[..start.saturating_sub(1)].rfind('\n').map(|at| at + 1) {
        if !source[previous..start].starts_with("  #") {
            break;
        }
        start = previous;
    }
    Ok(start)
}

/// Returns the span of the definition of `node` in `source`, from its `node <id> = ` line through the line that closes
/// its block.
fn definition(source: &str, node: &str) -> Result<std::ops::Range<usize>, GenericError> {
    let header = format!("\n  node {} = ", node);
    let start = source
        .find(&header)
        .map(|at| at + 1)
        .ok_or_else(|| generic_error!("The default program has no node `{}`.", node))?;
    let end = source[start..]
        .find("\n  }\n")
        .map(|end| start + end + "\n  }\n".len())
        .ok_or_else(|| generic_error!("The definition of node `{}` has no end.", node))?;
    Ok(start..end)
}

/// Puts `node` (a node definition, whose edges name `{next}` where the policy `tee` goes) between the node that feeds
/// the policy `tee` and the `tee`: the feeding node's `-> metrics_policy` becomes `-> <id>`, `<id>` passes on to the
/// `tee`, and the definition goes right above the policy section.
fn insert_before_policy(source: &str, id: &str, node: &str) -> Result<String, GenericError> {
    let edge = format!("\n    -> {}\n", POLICY_NODE);
    if !source.contains(&edge) {
        return Err(generic_error!(
            "The default program has no node that feeds the policy `tee` (looked for {:?}).",
            edge
        ));
    }
    let edited = source.replacen(&edge, &format!("\n    -> {}\n", id), 1);
    let at = definition_start(&edited, POLICY_NODE)?;
    let node = node.replace("{next}", POLICY_NODE);
    Ok(format!("{}{}\n{}", &edited[..at], node, &edited[at..]))
}

/// Replaces the first occurrence of `from` in `source`, which must have it.
fn replace_once(source: &str, from: &str, to: &str, what: &str) -> Result<String, GenericError> {
    if !source.contains(from) {
        return Err(generic_error!(
            "The default program has no {} (looked for {:?}).",
            what,
            from
        ));
    }
    Ok(source.replacen(from, to, 1))
}

/// S1: drops every `demo.debug.*` metric ahead of the policy, after aggregation and enrichment.
pub fn drop_debug(source: &str) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "drop_debug",
        "  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  -> drop\n    false -> {next}\n  }\n",
    )
}

/// S2: widens team B's route, `demo.checkout.` by prefix, to the metrics tagged `team:platform` as well.
pub fn widen_team_b(source: &str, outputs: &Outputs) -> Result<String, GenericError> {
    // The route's split ends its predicate on a line of its own, followed by the branch that emits to team B.
    let end = format!("\n  ) {{\n    true  -> emit {}\n", outputs.team_b);
    let widened = format!("\n    or tag(\"team\") == \"platform\"{}", end);
    replace_once(
        source,
        &end,
        &widened,
        &format!("routing split whose `true` branch emits to `{}`", outputs.team_b),
    )
}

/// S3: routes the metrics tagged `team:checkout` to team B only: they no longer reach the primary endpoint.
pub fn exclusive_team_b(source: &str, outputs: &Outputs) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "checkout_only",
        &format!(
            "  node checkout_only = split(tag(\"team\") == \"checkout\") {{\n    true  -> emit {}\n    false -> \
             {{next}}\n  }}\n",
            outputs.team_b
        ),
    )
}

/// S4: mirrors one family to the failover endpoint: its gateway's predicate becomes a constant `true` (mirroring on,
/// whatever the configuration says) and a literal allowlist.
pub fn mirror_to_failover(source: &str, outputs: &Outputs) -> Result<String, GenericError> {
    let branch = format!("\n    true  -> emit {}\n", outputs.mrf);
    let at = source.find(&branch).ok_or_else(|| {
        generic_error!(
            "The default program has no split whose `true` branch emits to `{}`.",
            outputs.mrf
        )
    })?;
    let node = source[..at]
        .rfind("\n  node ")
        .ok_or_else(|| generic_error!("The failover gateway's branch is not inside a node."))?;
    const SPLIT: &str = "= split(";
    let predicate = source[node..at]
        .find(SPLIT)
        .map(|offset| node + offset + SPLIT.len())
        .ok_or_else(|| generic_error!("The node that emits to `{}` is not a split.", outputs.mrf))?;
    let end = source[..at]
        .rfind(") {")
        .filter(|end| *end >= predicate)
        .ok_or_else(|| generic_error!("The failover gateway's predicate has no end."))?;
    Ok(format!(
        "{}true and name in [\"{}\"]{}",
        &source[..predicate],
        MIRRORED_METRIC,
        &source[end..]
    ))
}

/// S5, duplicate branch: a second branch of the policy `tee` to the primary output, so that every record reaches it
/// twice.
pub fn duplicate_branch(source: &str, outputs: &Outputs) -> Result<String, GenericError> {
    let branch = format!("\n    -> emit {}\n", outputs.primary);
    replace_once(
        source,
        &branch,
        &format!("{}    -> emit {}\n", branch, outputs.primary),
        &format!("`tee` branch that emits to `{}`", outputs.primary),
    )
}

/// S5, undeclared output: team C's route emits to an output ADP does not declare.
pub fn undeclared_output(source: &str, outputs: &Outputs) -> Result<String, GenericError> {
    replace_once(
        source,
        &format!("emit {}\n", outputs.team_c),
        &format!("emit {}\n", UNDECLARED_OUTPUT),
        &format!("`emit {}`", outputs.team_c),
    )
}

/// S8 and S10: a branch of the policy `tee` that routes the metrics tagged `team:platform` to team D's declared output
/// as well, `split(tag("team") == "platform") { true -> emit <output>; false -> drop }`, next to the branch that emits
/// to the primary output, which keeps receiving everything.
pub fn route_to_team_d(source: &str, outputs: &Outputs, team_d_output: &str) -> Result<String, GenericError> {
    let branch = format!("\n    -> emit {}\n", outputs.primary);
    let with_branch = replace_once(
        source,
        &branch,
        &format!("{}    -> {}\n", branch, TEAM_D_ROUTE),
        &format!("`tee` branch that emits to `{}`", outputs.primary),
    )?;
    // The node goes right above the policy section, before the `tee` that refers to it.
    let at = definition_start(&with_branch, POLICY_NODE)?;
    let node = format!(
        "  node {} = split(tag(\"team\") == \"platform\") {{\n    true  -> emit {}\n    false -> drop\n  }}\n\n",
        TEAM_D_ROUTE, team_d_output
    );
    Ok(format!("{}{}{}", &with_branch[..at], node, &with_branch[at..]))
}

/// The nodes whose windows a width change sets: DogStatsD aggregation, and the timestamp split ahead of it, whose
/// window must equal its aggregation's.
pub const WINDOWED_NODES: [&str; 2] = ["dsd_no_agg_split", "dsd_agg"];

/// S13: changes DogStatsD aggregation's width in the default program's source to `width` (a duration literal, such as
/// `30s`): the `window` of each of [`WINDOWED_NODES`], whatever it was set to, becomes the literal.
pub fn width_change(source: &str, width: &str) -> Result<String, GenericError> {
    let mut edited = source.to_string();
    for node in WINDOWED_NODES {
        let header = format!("\n  node {} = ", node);
        let start = edited
            .find(&header)
            .ok_or_else(|| generic_error!("The default program has no node `{}`.", node))?;
        // The node's definition ends at the first line that closes a block at the node's indentation.
        let end = edited[start + 1..]
            .find("\n  }\n")
            .map_or(edited.len(), |end| start + 1 + end);
        const WINDOW: &str = "\n    window: ";
        let window = edited[start..end]
            .find(WINDOW)
            .map(|offset| start + offset + WINDOW.len())
            .ok_or_else(|| generic_error!("Node `{}` of the default program sets no `window`.", node))?;
        let value_end = edited[window..]
            .find(",\n")
            .map(|offset| window + offset)
            .filter(|value_end| *value_end <= end)
            .ok_or_else(|| generic_error!("The `window` of node `{}` does not end its line with a comma.", node))?;
        edited.replace_range(window..value_end, width);
    }
    Ok(edited)
}

/// S5, aggregation without parameters: an aggregation in front of the policy that names only its window, while
/// validation requires every parameter of an aggregation.
pub fn aggregation_without_parameters(source: &str) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "rollup",
        "  node rollup = aggregate(window: 10s) {\n    -> {next}\n  }\n",
    )
}

/// S5, a second aggregation: an aggregation with every parameter, in front of the policy. ADP runs at most one
/// aggregation per program, so validation rejects it at its node, as the editor shows.
pub fn second_aggregation(source: &str) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "rollup",
        "  node rollup = aggregate(\n    window: 10s,\n    flush_interval: 10s,\n    context_limit: 1000,\n    \
         counter_expiry: 5m,\n    flush_open_windows: false,\n    histogram_aggregates: [\"max\", \"count\"],\n    \
         histogram_percentiles: [95],\n    copy_to_distribution: false,\n    copy_to_distribution_prefix: \"\",\n  ) \
         {\n    -> {next}\n  }\n",
    )
}

/// S5, the aggregation moved after enrichment: DogStatsD aggregation's definition moves in front of the policy, and
/// the node that fed it feeds what it fed. The program still has one aggregation, but the frame runs it only in its
/// isolation slot, which enrichment has no exit to, so ADP refuses it when it places the plan.
pub fn aggregation_after_enrichment(source: &str) -> Result<String, GenericError> {
    let span = definition(source, AGGREGATION_NODE)?;
    let block = &source[span.clone()];
    // Aggregation has one output port, which the printed form leaves out: `-> <target>`.
    const EDGE: &str = "\n    -> ";
    let target = block
        .rfind(EDGE)
        .map(|at| at + EDGE.len())
        .and_then(|start| block[start..].find('\n').map(|end| &block[start..start + end]))
        .ok_or_else(|| generic_error!("Aggregation's definition has no `-> <target>` line."))?
        .to_string();
    // Aggregation's definition, the comment above it, and the blank line after it leave; its feeder feeds its target.
    let start = definition_start(source, AGGREGATION_NODE)?;
    let after = if source[span.end..].starts_with('\n') {
        span.end + 1
    } else {
        span.end
    };
    let mut edited = format!("{}{}", &source[..start], &source[after..]);
    let into = format!("\n    -> {}\n", AGGREGATION_NODE);
    edited = replace_once(
        &edited,
        &into,
        &format!("\n    -> {}\n", target),
        &format!("node that feeds `{}`", AGGREGATION_NODE),
    )?;
    let moved = block.replacen(&format!("{}{}\n", EDGE, target), &format!("{}{{next}}\n", EDGE), 1);
    insert_before_policy(&edited, AGGREGATION_NODE, &moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::with_sections;

    /// The default program of the demo's configuration, trimmed, as the editor prints it before its section comments.
    const PRINTED: &str = r#"policy metrics {
  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"

  input.dogstatsd -> dsd_enrich

  node dsd_enrich = dogstatsd_mapper(profiles: []) {
    -> dsd_no_agg_split
  }

  node dsd_no_agg_split = timestamp_split(
    window: config("dogstatsd.aggregation.window_duration_seconds"),
  ) {
    default     -> dsd_tag_filterlist
    passthrough -> dsd_post_agg_filter_in.1
  }

  node dsd_tag_filterlist = tag_projection(rules: []) {
    -> dsd_agg
  }

  node dsd_agg = aggregate(
    window: config("dogstatsd.aggregation.window_duration_seconds"),
    context_limit: config("dogstatsd.aggregation.context_limit"),
  ) {
    -> dsd_post_agg_filter_in.0
  }

  node dsd_post_agg_filter_in = merge(inputs: 2) {
    -> metrics_enrich_in.0
  }

  node metrics_enrich_in = merge(inputs: 1) {
    -> metrics_enrich
  }

  node metrics_enrich = host_enrichment {
    -> metrics_policy
  }

  node metrics_policy = tee {
    -> emit dd_metrics_encode
    -> metrics_routing_filter_0
    -> mrf_metrics_gateway
  }

  node metrics_routing_filter_0 = split(
    intake_name matches(
      values: config("endpoint_routing.group.b0b0b0b0.prefixes"),
      prefix: true,
    )
  ) {
    true  -> emit metrics_routing_encode_0
    false -> drop
  }

  node mrf_metrics_gateway = split(
    flag(config("multi_region_failover.enabled"))
    and is_empty(config("multi_region_failover.metric_mirroring.allowlist"))
  ) {
    true  -> emit mrf_metrics_encode
    false -> drop
  }
}
"#;

    /// The trimmed default program as the editor shows it, with its section comments.
    fn source() -> String {
        with_sections(PRINTED)
    }

    fn outputs() -> Outputs {
        Outputs {
            primary: "dd_metrics_encode".to_string(),
            team_b: "metrics_routing_encode_0".to_string(),
            team_c: "metrics_routing_encode_0".to_string(),
            mrf: "mrf_metrics_encode".to_string(),
        }
    }

    /// Returns the lines `edited` has that `source` does not, and the lines it lost, in order.
    fn changed_lines<'a>(source: &'a str, edited: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
        let old = source.lines().collect::<Vec<_>>();
        let new = edited.lines().collect::<Vec<_>>();
        let added = new.iter().filter(|line| !old.contains(line)).copied().collect();
        let removed = old.iter().filter(|line| !new.contains(line)).copied().collect();
        (added, removed)
    }

    #[test]
    fn the_editor_marks_where_each_section_of_the_program_begins() {
        let source = source();
        let comments = source
            .lines()
            .filter(|line| line.starts_with("  #"))
            .collect::<Vec<_>>();
        assert_eq!(
            comments,
            [
                "  # Sources: where each one's metrics enter",
                "  # DogStatsD processing, before aggregation",
                "  # Aggregation: a program has at most one",
                "  # After aggregation: filter, client telemetry tap",
                "  # Enrichment: every source, then host enrichment",
                "  # Policy: one branch per output; route here",
            ]
        );
        assert!(source.contains("\n  # Aggregation: a program has at most one\n  node dsd_agg = aggregate(\n"));
        // The comments change nothing in the artifact.
        let parsed = |text: &str| saluki_metric_plan_syntax::parse(text).unwrap().artifact;
        assert_eq!(parsed(&source), parsed(PRINTED));
    }

    #[test]
    fn each_edit_changes_the_lines_its_scenario_describes() {
        let source = source();
        let outputs = outputs();
        type Edit = fn(&str, &Outputs) -> Result<String, GenericError>;
        let cases: [(&str, Edit, &[&str], &[&str]); 9] = [
            (
                "S1",
                |source, _| drop_debug(source),
                &[
                    "    -> drop_debug",
                    "  node drop_debug = split(name starts_with \"demo.debug.\") {",
                    "    true  -> drop",
                    "    false -> metrics_policy",
                ],
                &["    -> metrics_policy"],
            ),
            ("S2", widen_team_b, &["    or tag(\"team\") == \"platform\""], &[]),
            (
                "S3",
                exclusive_team_b,
                // Its `true` branch emits to team B's output, a line the source already has.
                &[
                    "    -> checkout_only",
                    "  node checkout_only = split(tag(\"team\") == \"checkout\") {",
                    "    false -> metrics_policy",
                ],
                &["    -> metrics_policy"],
            ),
            (
                "S4",
                mirror_to_failover,
                &["  node mrf_metrics_gateway = split(true and name in [\"demo.checkout.requests\"]) {"],
                &[
                    "  node mrf_metrics_gateway = split(",
                    "    flag(config(\"multi_region_failover.enabled\"))",
                    "    and is_empty(config(\"multi_region_failover.metric_mirroring.allowlist\"))",
                ],
            ),
            // Line sets: the duplicated branch is a line the source already has.
            ("S5a", duplicate_branch, &[], &[]),
            (
                "S5b",
                undeclared_output,
                &["    true  -> emit metrics_routing_encode_99"],
                &["    true  -> emit metrics_routing_encode_0"],
            ),
            (
                "S5c",
                |source, _| aggregation_without_parameters(source),
                // Its edge into the policy `tee` is a line the source already has.
                &["    -> rollup", "  node rollup = aggregate(window: 10s) {"],
                &[],
            ),
            (
                "S5d",
                |source, _| second_aggregation(source),
                &[
                    "    -> rollup",
                    "  node rollup = aggregate(",
                    "    window: 10s,",
                    "    flush_interval: 10s,",
                    "    context_limit: 1000,",
                    "    counter_expiry: 5m,",
                    "    flush_open_windows: false,",
                    "    histogram_aggregates: [\"max\", \"count\"],",
                    "    histogram_percentiles: [95],",
                    "    copy_to_distribution: false,",
                    "    copy_to_distribution_prefix: \"\",",
                ],
                &[],
            ),
            (
                "S8 route to team D",
                |source, outputs| route_to_team_d(source, outputs, "resources_team_d"),
                // Its `false` branch drops, a line the source already has.
                &[
                    "  node team_d_route = split(tag(\"team\") == \"platform\") {",
                    "    true  -> emit resources_team_d",
                    "    -> team_d_route",
                ],
                &[],
            ),
        ];
        for (case, edit, added, removed) in cases {
            let edited = edit(&source, &outputs).unwrap();
            let (found_added, found_removed) = changed_lines(&source, &edited);
            assert_eq!(
                (found_added.as_slice(), found_removed.as_slice()),
                (added, removed),
                "{}:\n{}",
                case,
                edited
            );
            saluki_metric_plan_syntax::parse(&edited)
                .unwrap_or_else(|diagnostics| panic!("{}: {}\n{}", case, diagnostics, edited));
            // A node inserted ahead of the policy goes above the policy section's comment.
            if let Some(at) = edited.find("  # Policy:") {
                for node in ["drop_debug", "checkout_only", "rollup", "team_d_route"] {
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

        let duplicated = duplicate_branch(&source, &outputs).unwrap();
        assert_eq!(duplicated.matches("    -> emit dd_metrics_encode\n").count(), 2);
        assert_eq!(duplicated.lines().count(), source.lines().count() + 1);
    }

    #[test]
    fn the_aggregation_moves_after_enrichment_and_its_feeder_feeds_what_it_fed() {
        let source = source();
        let edited = aggregation_after_enrichment(&source).unwrap();
        let parsed = saluki_metric_plan_syntax::parse(&edited)
            .unwrap_or_else(|diagnostics| panic!("{}\n{}", diagnostics, edited))
            .artifact;
        let edges = parsed
            .edges
            .iter()
            .map(|edge| format!("{} -> {}", edge.from, edge.to))
            .collect::<Vec<_>>();
        for edge in [
            "dsd_tag_filterlist.out -> dsd_post_agg_filter_in.0",
            "metrics_enrich.out -> dsd_agg.in",
            "dsd_agg.out -> metrics_policy.in",
        ] {
            assert!(edges.iter().any(|found| found == edge), "{} in {:?}", edge, edges);
        }
        assert_eq!(
            parsed.nodes.len(),
            saluki_metric_plan_syntax::parse(&source).unwrap().artifact.nodes.len()
        );
        // The aggregation's section comment goes with it; it is defined right above the policy section.
        assert!(!edited.contains("# Aggregation"), "{}", edited);
        assert!(edited.find("\n  node dsd_agg = ").unwrap() > edited.find("\n  node metrics_enrich = ").unwrap());
        assert!(edited.find("\n  node dsd_agg = ").unwrap() < edited.find("  # Policy:").unwrap());
    }

    #[test]
    fn the_width_change_sets_both_windows_to_the_literal() {
        let edited = width_change(&source(), "30s").unwrap();
        assert_eq!(edited.matches("\n    window: 30s,\n").count(), 2, "{}", edited);
        assert!(!edited.contains("window_duration_seconds"), "{}", edited);
        saluki_metric_plan_syntax::parse(&edited).unwrap();
    }

    #[test]
    fn an_edit_that_cannot_find_its_lines_says_what_it_looked_for() {
        let outputs = Outputs {
            team_b: "metrics_routing_encode_7".to_string(),
            ..outputs()
        };
        let error = widen_team_b(&source(), &outputs).unwrap_err().to_string();
        assert!(error.contains("emits to `metrics_routing_encode_7`"), "{}", error);
        let error = drop_debug("policy metrics {\n}\n").unwrap_err().to_string();
        assert!(error.contains("-> metrics_policy"), "{}", error);
    }
}
