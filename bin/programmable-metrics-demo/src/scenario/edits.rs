//! The edits of the scenarios, made to the default program's source as the editor shows it, the way a presenter
//! types them.
//!
//! The default program is the whole metrics path, its one scope fed by every source; the policy region starts at its
//! `tee`, [`POLICY_NODE`], which enrichment feeds, and whose first branch emits to the main output's recipients: `->
//! emit to [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]`, the primary endpoint and every they say (the
//! edge into the policy `tee`, the main `emit`, the failover gateway's `emit`, a node's definition), never by line
//! number, and fails with a message naming what it looked for when the default program is printed otherwise, so that a
//! change of the printed form shows up as a clear check failure.

use saluki_error::{generic_error, GenericError};

/// The family the failover scenario mirrors, which the failover endpoint then receives while failover is on.
pub const MIRRORED_METRIC: &str = "demo.checkout.requests";

/// The recipient the unknown-recipient guardrail emits to, which ADP's directory does not name.
pub const UNKNOWN_RECIPIENT: &str = "team_z";

/// The family the routing to an added destination sends there: the metrics tagged `team:platform`.
pub const PLATFORM_FAMILY: &str = "demo.platform.queue_depth";

/// The policy `tee`, where the policy region begins: enrichment feeds it, and it has one branch per destination.
pub const POLICY_NODE: &str = "metrics_policy";

/// DogStatsD aggregation, the program's one aggregation.
const AGGREGATION_NODE: &str = "dsd_agg";

/// The name the renaming scenario gives the aggregation.
pub const RENAMED_AGGREGATION: &str = "agg";

/// The node the recipe adds, which sends the checkout families to team B.
pub const TEAM_B_ROUTE: &str = "to_team_b";

/// The node that routes the platform family to an added destination.
pub const DESTINATION_ROUTE: &str = "to_destination";

/// How the start of the main `emit`, the policy `tee`'s branch to the primary endpoint and every additional one, reads.
const MAIN_EMIT: &str = "\n    -> emit to [primary";

/// The failover gateway's `emit`, on its `true` branch.
const FAILOVER_EMIT: &str = "    true  -> emit to [mrf]\n";

/// Returns a recipient name as a program writes an endpoint URL: quoted.
pub fn quoted(url: &str) -> String {
    format!("\"{}\"", url)
}

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

/// Puts a node definition right above the policy section, before the `tee` that refers to it.
fn define_above_policy(source: &str, node: &str) -> Result<String, GenericError> {
    let at = definition_start(source, POLICY_NODE)?;
    Ok(format!("{}{}\n{}", &source[..at], node, &source[at..]))
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
    define_above_policy(&edited, &node.replace("{next}", POLICY_NODE))
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

/// Returns the span of the main `emit` line in `source`, from its first space to its newline.
fn main_emit(source: &str) -> Result<std::ops::Range<usize>, GenericError> {
    let start = source.find(MAIN_EMIT).map(|at| at + 1).ok_or_else(|| {
        generic_error!(
            "The default program's policy `tee` has no branch that emits to the primary endpoint (looked for {:?}).",
            MAIN_EMIT
        )
    })?;
    let end = source[start..]
        .find('\n')
        .map(|end| start + end)
        .ok_or_else(|| generic_error!("The main `emit` has no end."))?;
    Ok(start..end)
}

/// Adds a branch to the policy `tee`, right after the main `emit`.
fn add_policy_branch(source: &str, branch: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    Ok(format!(
        "{}\n    -> {}{}",
        &source[..line.end],
        branch,
        &source[line.end..]
    ))
}

/// Returns a `split` node that emits what `predicate` matches to `recipients` and drops the rest.
fn route(id: &str, predicate: &str, recipients: &str) -> String {
    format!(
        "  node {} = split({}) {{\n    true  -> emit to [{}]\n    false -> drop\n  }}\n",
        id, predicate, recipients
    )
}

/// S1: drops every `demo.debug.*` metric ahead of the policy, after aggregation and enrichment.
pub fn drop_debug(source: &str) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "drop_debug",
        "  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  -> drop\n    false -> {next}\n  }\n",
    )
}

/// S2, the recipe: narrows team B to the checkout families. Team B's URL leaves the main `emit`, and a branch of the
/// policy `tee`, [`TEAM_B_ROUTE`], sends `demo.checkout.*` to team B.
pub fn narrow_team_b(source: &str, team_b: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let name = format!(", {}", quoted(team_b));
    if !source[line.clone()].contains(&name) {
        return Err(generic_error!(
            "The main `emit` does not name team B ({:?}): {}",
            team_b,
            &source[line]
        ));
    }
    let narrowed = format!(
        "{}{}{}",
        &source[..line.start],
        source[line.clone()].replacen(&name, "", 1),
        &source[line.end..]
    );
    let branched = add_policy_branch(&narrowed, TEAM_B_ROUTE)?;
    define_above_policy(
        &branched,
        &route(TEAM_B_ROUTE, "name starts_with \"demo.checkout.\"", &quoted(team_b)),
    )
}

/// S3a: widens the recipe's route to team B to the metrics tagged `team:platform` as well.
pub fn widen_team_b(narrowed: &str) -> Result<String, GenericError> {
    replace_once(
        narrowed,
        "split(name starts_with \"demo.checkout.\") {",
        "split(\n    name starts_with \"demo.checkout.\"\n    or tag(\"team\") == \"platform\"\n  ) {",
        "route to team B by the checkout prefix",
    )
}

/// S3b: routes the metrics tagged `team:checkout` to team B only, ahead of the policy `tee`: they no longer reach the
/// primary endpoint or team C.
pub fn exclusive_team_b(source: &str, team_b: &str) -> Result<String, GenericError> {
    insert_before_policy(
        source,
        "checkout_only",
        &format!(
            "  node checkout_only = split(tag(\"team\") == \"checkout\") {{\n    true  -> emit to [{}]\n    false -> \
             {{next}}\n  }}\n",
            quoted(team_b)
        ),
    )
}

/// S4a: mirrors only one checkout family on failover: the failover gateway's predicate gains `and name in [...]`, so
/// that, while failover is on (a signal), the failover endpoint receives [`MIRRORED_METRIC`] alone.
pub fn mirror_checkout_on_failover(source: &str) -> Result<String, GenericError> {
    let at = source
        .find(FAILOVER_EMIT)
        .ok_or_else(|| generic_error!("The default program has no failover gateway ({:?}).", FAILOVER_EMIT))?;
    let end = source[..at]
        .rfind("\n  ) {\n")
        .ok_or_else(|| generic_error!("The failover gateway's predicate does not end on a line of its own."))?;
    Ok(format!(
        "{}\n    and name in [\"{}\"]{}",
        &source[..end],
        MIRRORED_METRIC,
        &source[end..]
    ))
}

/// S5, duplicate branch: a second branch of the policy `tee` to the main `emit`'s recipients, so that every record
/// reaches them twice.
pub fn duplicate_branch(source: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let emit = source[line].trim_start().trim_start_matches("-> ").to_string();
    add_policy_branch(source, &emit)
}

/// S5, two hosts: the failover gateway emits to the primary endpoint and the failover endpoint together, which are
/// recipients of two delivery hosts.
pub fn two_hosts(source: &str) -> Result<String, GenericError> {
    replace_once(
        source,
        FAILOVER_EMIT,
        "    true  -> emit to [primary, mrf]\n",
        "failover gateway",
    )
}

/// S5, a policy binding: the aggregation's percentiles bind the native setting again, `config(...)`, where the
/// detached export writes their value. A published program owns its policy: it may bind facts and signals only.
///
/// The aggregation is in every default program's export, configured or not; the metric filterlist, which this edit
/// bound before, is left out of the export while it is empty, as it is in the demo.
pub fn policy_binding(source: &str) -> Result<String, GenericError> {
    const PERCENTILES: &str = "histogram_percentiles: ";
    let span = definition(source, AGGREGATION_NODE)?;
    let start = source[span.clone()]
        .find(PERCENTILES)
        .map(|at| span.start + at + PERCENTILES.len())
        .ok_or_else(|| generic_error!("The aggregation `{}` writes no `{}...`.", AGGREGATION_NODE, PERCENTILES))?;
    let end = source[start..span.end]
        .find(",\n")
        .map(|at| start + at)
        .ok_or_else(|| generic_error!("The aggregation's percentiles have no end."))?;
    Ok(format!(
        "{}config(\"metrics_encoding.histogram.percentiles\"){}",
        &source[..start],
        &source[end..]
    ))
}

/// S5, an unknown recipient: a branch of the policy `tee` emits everything to [`UNKNOWN_RECIPIENT`], which ADP's
/// directory does not name: ADP holds the program pending until a recipient of that name is declared.
pub fn unknown_recipient(source: &str) -> Result<String, GenericError> {
    add_policy_branch(source, &format!("emit to [{}]", UNKNOWN_RECIPIENT))
}

/// S9c: a branch of the policy `tee`, [`DESTINATION_ROUTE`], that sends the metrics tagged `team:platform` to the
/// destination at `url` as well.
pub fn route_platform_to(source: &str, url: &str) -> Result<String, GenericError> {
    let branched = add_policy_branch(source, DESTINATION_ROUTE)?;
    define_above_policy(
        &branched,
        &route(DESTINATION_ROUTE, "tag(\"team\") == \"platform\"", &quoted(url)),
    )
}

/// S14: renames the aggregation, [`AGGREGATION_NODE`], to [`RENAMED_AGGREGATION`]: its definition and the edge into
/// it. A program's one aggregation is the aggregation whatever its name, so its state carries over.
pub fn rename_aggregation(source: &str) -> Result<String, GenericError> {
    let defined = replace_once(
        source,
        &format!("\n  node {} = ", AGGREGATION_NODE),
        &format!("\n  node {} = ", RENAMED_AGGREGATION),
        "aggregation",
    )?;
    replace_once(
        &defined,
        &into(AGGREGATION_NODE),
        &into(RENAMED_AGGREGATION),
        "edge into the aggregation",
    )
}

/// Returns how the end of an edge into `node` reads, whatever port it leaves: `-> <node>` and the line's end. The
/// timestamp split feeds the aggregation from its `default` port, whose line names it (`default     -> dsd_agg`), and a
/// node of one output port feeds it from a line without a port (`-> dsd_agg`).
fn into(node: &str) -> String {
    format!("-> {}\n", node)
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
        // The parameter is on a line of its own, or inline when it is the operator's only one, as in a detached
        // export, where it holds a literal: `timestamp_split(window: 10s)`.
        const WINDOW: &str = "window: ";
        let window = edited[start..end]
            .match_indices(WINDOW)
            .map(|(offset, _)| start + offset)
            .find(|at| matches!(edited.as_bytes()[at - 1], b' ' | b'('))
            .map(|at| at + WINDOW.len())
            .ok_or_else(|| generic_error!("Node `{}` of the default program sets no `window`.", node))?;
        // The value ends at the first comma or closing parenthesis outside its own parentheses, as in
        // `config("...")`.
        let mut depth = 0usize;
        let value_end = edited[window..]
            .char_indices()
            .find(|(_, c)| match c {
                '(' => {
                    depth += 1;
                    false
                }
                ')' if depth > 0 => {
                    depth -= 1;
                    false
                }
                ',' | ')' | '\n' => depth == 0,
                _ => false,
            })
            .map(|(offset, _)| window + offset)
            .filter(|value_end| *value_end <= end && edited.as_bytes()[*value_end] != b'\n')
            .ok_or_else(|| {
                generic_error!(
                    "The `window` of node `{}` ends with neither a comma nor a parenthesis.",
                    node
                )
            })?;
        edited.replace_range(window..value_end, width);
    }
    Ok(edited)
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
    edited = replace_once(
        &edited,
        &into(AGGREGATION_NODE),
        &into(&target),
        &format!("node that feeds `{}`", AGGREGATION_NODE),
    )?;
    let moved = block.replacen(&format!("{}{}\n", EDGE, target), &format!("{}{{next}}\n", EDGE), 1);
    insert_before_policy(&edited, AGGREGATION_NODE, &moved)
}

#[cfg(test)]
mod tests;
