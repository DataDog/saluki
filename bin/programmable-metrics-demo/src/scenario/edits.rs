//! The edits of the scenarios, made to the default program's source as the editor shows it, the way a presenter
//! types them.
//!
//! Each edit finds the lines it changes by what they say (the scope input's edge, an inline `emit` of an output, a
//! `tee` branch), never by line number, and fails with a message naming what it looked for when the default program is
//! printed otherwise, so that a change of the printed form shows up as a clear check failure.

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

/// Returns the node the scope input feeds, from the source's `input.metrics -> <node>` line.
fn input_target(source: &str) -> Result<&str, GenericError> {
    const EDGE: &str = "\n  input.metrics -> ";
    let start = source
        .find(EDGE)
        .ok_or_else(|| generic_error!("The default program has no `input.metrics -> <node>` line."))?
        + EDGE.len();
    let end = source[start..].find('\n').map_or(source.len(), |end| start + end);
    Ok(source[start..end].trim())
}

/// Puts `node` (a node definition, whose edges name `{next}` where the scope input's node goes) between the scope
/// input and the node it fed: `input.metrics -> <id>`, and `<id>` passes on to the node.
fn insert_after_input(source: &str, id: &str, node: &str) -> Result<String, GenericError> {
    let target = input_target(source)?.to_string();
    let edge = format!("\n  input.metrics -> {}\n", target);
    let node = node.replace("{next}", &target);
    Ok(source.replacen(&edge, &format!("\n  input.metrics -> {}\n\n{}", id, node), 1))
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

/// S1: drops every `demo.debug.*` metric before anything else sees it.
pub fn drop_debug(source: &str) -> Result<String, GenericError> {
    insert_after_input(
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
    insert_after_input(
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
    // The node goes after the scope input's edge, before the `tee` that refers to it.
    let target = input_target(&with_branch)?.to_string();
    let edge = format!("\n  input.metrics -> {}\n", target);
    let node = format!(
        "\n  node {} = split(tag(\"team\") == \"platform\") {{\n    true  -> emit {}\n    false -> drop\n  }}\n",
        TEAM_D_ROUTE, team_d_output
    );
    Ok(with_branch.replacen(&edge, &format!("{}{}", edge, node), 1))
}

/// The nodes of the pre-aggregation scope whose windows a width change sets: DogStatsD aggregation, and the timestamp
/// split ahead of it, whose window must equal its aggregation's.
pub const WINDOWED_NODES: [&str; 2] = ["dsd_no_agg_split", "dsd_agg"];

/// S13: changes DogStatsD aggregation's width in the pre-aggregation scope's source to `width` (a duration literal,
/// such as `30s`): the `window` of each of [`WINDOWED_NODES`], whatever it was set to, becomes the literal.
pub fn width_change(source: &str, width: &str) -> Result<String, GenericError> {
    let mut edited = source.to_string();
    for node in WINDOWED_NODES {
        let header = format!("\n  node {} = ", node);
        let start = edited
            .find(&header)
            .ok_or_else(|| generic_error!("The pre-aggregation scope's source has no node `{}`.", node))?;
        // The node's definition ends at the first line that closes a block at the node's indentation.
        let end = edited[start + 1..]
            .find("\n  }\n")
            .map_or(edited.len(), |end| start + 1 + end);
        const WINDOW: &str = "\n    window: ";
        let window = edited[start..end]
            .find(WINDOW)
            .map(|offset| start + offset + WINDOW.len())
            .ok_or_else(|| generic_error!("Node `{}` of the pre-aggregation scope sets no `window`.", node))?;
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
    insert_after_input(
        source,
        "rollup",
        "  node rollup = aggregate(window: 10s) {\n    -> {next}\n  }\n",
    )
}

/// S5, an aggregation in the policy scope: an aggregation with every parameter, in front of the policy. The frame runs
/// state owners only in their isolation slot, ahead of the policy scope, so ADP refuses it when it places the plan.
pub fn aggregation_in_the_policy_scope(source: &str) -> Result<String, GenericError> {
    insert_after_input(
        source,
        "rollup",
        "  node rollup = aggregate(\n    window: 10s,\n    flush_interval: 10s,\n    context_limit: 1000,\n    \
         counter_expiry: 5m,\n    flush_open_windows: false,\n    histogram_aggregates: [\"max\", \"count\"],\n    \
         histogram_percentiles: [95],\n    copy_to_distribution: false,\n    copy_to_distribution_prefix: \"\",\n  ) \
         {\n    -> {next}\n  }\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default program of the demo's configuration, as the editor prints it (the demo's export fixture).
    const SOURCE: &str = r#"policy post_enrich {
  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"

  input.metrics -> metrics_policy

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

    fn outputs() -> Outputs {
        Outputs {
            primary: "dd_metrics_encode".to_string(),
            team_b: "metrics_routing_encode_0".to_string(),
            team_c: "metrics_routing_encode_0".to_string(),
            mrf: "mrf_metrics_encode".to_string(),
        }
    }

    /// Returns the lines `edited` has that `SOURCE` does not, and the lines it lost, in order.
    fn changed_lines(edited: &str) -> (Vec<&str>, Vec<&str>) {
        let old = SOURCE.lines().collect::<Vec<_>>();
        let new = edited.lines().collect::<Vec<_>>();
        let added = new.iter().filter(|line| !old.contains(line)).copied().collect();
        let removed = old.iter().filter(|line| !new.contains(line)).copied().collect();
        (added, removed)
    }

    #[test]
    fn each_edit_changes_the_lines_its_scenario_describes() {
        let outputs = outputs();
        type Edit = fn(&str, &Outputs) -> Result<String, GenericError>;
        let cases: [(&str, Edit, &[&str], &[&str]); 9] = [
            (
                "S1",
                |source, _| drop_debug(source),
                &[
                    "  input.metrics -> drop_debug",
                    "  node drop_debug = split(name starts_with \"demo.debug.\") {",
                    "    true  -> drop",
                    "    false -> metrics_policy",
                ],
                &["  input.metrics -> metrics_policy"],
            ),
            ("S2", widen_team_b, &["    or tag(\"team\") == \"platform\""], &[]),
            (
                "S3",
                exclusive_team_b,
                // Its `true` branch emits to team B's output, a line the source already has.
                &[
                    "  input.metrics -> checkout_only",
                    "  node checkout_only = split(tag(\"team\") == \"checkout\") {",
                    "    false -> metrics_policy",
                ],
                &["  input.metrics -> metrics_policy"],
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
            ("S5 duplicate branch", duplicate_branch, &[], &[]),
            (
                "S5 undeclared output",
                undeclared_output,
                &["    true  -> emit metrics_routing_encode_99"],
                &["    true  -> emit metrics_routing_encode_0"],
            ),
            (
                "S5 aggregation without parameters",
                |source, _| aggregation_without_parameters(source),
                &[
                    "  input.metrics -> rollup",
                    "  node rollup = aggregate(window: 10s) {",
                    "    -> metrics_policy",
                ],
                &["  input.metrics -> metrics_policy"],
            ),
            (
                "S5 aggregation in the policy scope",
                |source, _| aggregation_in_the_policy_scope(source),
                &[
                    "  input.metrics -> rollup",
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
                    "    -> metrics_policy",
                ],
                &["  input.metrics -> metrics_policy"],
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
            let edited = edit(SOURCE, &outputs).unwrap();
            let (found_added, found_removed) = changed_lines(&edited);
            assert_eq!(
                (found_added.as_slice(), found_removed.as_slice()),
                (added, removed),
                "{}:\n{}",
                case,
                edited
            );
            saluki_metric_plan_syntax::parse(&edited)
                .unwrap_or_else(|diagnostics| panic!("{}: {}\n{}", case, diagnostics, edited));
        }

        let duplicated = duplicate_branch(SOURCE, &outputs).unwrap();
        assert_eq!(duplicated.matches("    -> emit dd_metrics_encode\n").count(), 2);
        assert_eq!(duplicated.lines().count(), SOURCE.lines().count() + 1);
    }

    #[test]
    fn an_edit_that_cannot_find_its_lines_says_what_it_looked_for() {
        let outputs = Outputs {
            team_b: "metrics_routing_encode_7".to_string(),
            ..outputs()
        };
        let error = widen_team_b(SOURCE, &outputs).unwrap_err().to_string();
        assert!(error.contains("emits to `metrics_routing_encode_7`"), "{}", error);
        let error = drop_debug("policy post_enrich {\n}\n").unwrap_err().to_string();
        assert!(error.contains("input.metrics"), "{}", error);
    }
}
