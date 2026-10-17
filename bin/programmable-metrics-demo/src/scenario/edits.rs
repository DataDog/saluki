//! The edits of the scenarios, made to the default program's source as the editor shows it, the way a presenter
//! types them.
//!
//! The default program is the whole metrics path, its one scope fed by every source. Enrichment ends in the stream
//! every destination reads, `let metrics = ...`, and each use of it, `metrics | ...`, is a branch of the policy: the
//! first emits to the main output's recipients, `metrics | emit [primary, "http://127.0.0.1:17102",
//! "http://127.0.0.1:17103"]`, the primary endpoint and every additional one. Where the export leaves enrichment out,
//! as in the demo, `let metrics` names the stream enrichment would have read, `let metrics = dsd_post_agg_filter`.
//!
//! Each edit finds the text it changes by what it says, never by line number or exact layout: a stage by its operator
//! (`timestamp_split(`), a node by the `let` that names it, the policy by `let metrics`, the main `emit` as the first
//! branch of the policy that emits to `primary`. Strings and comments are never matched. An
//! edit fails with a message naming what it looked for when the default program is printed otherwise, so that a change
//! of the printed form shows up as a clear check failure.

use saluki_error::{generic_error, GenericError};

/// The family the failover scenario mirrors, which the failover endpoint then receives while failover is on.
pub const MIRRORED_METRIC: &str = "demo.checkout.requests";

/// The recipient the unknown-recipient guardrail emits to, which ADP's directory does not name.
pub const UNKNOWN_RECIPIENT: &str = "team_z";

/// The family the routing to an added destination sends there: the metrics tagged `team:platform`.
pub const PLATFORM_FAMILY: &str = "demo.platform.queue_depth";

/// DogStatsD aggregation, the program's one aggregation: the `let` that ends in it names it.
const AGGREGATION_NODE: &str = "dsd_agg";

/// The failover gateway: the policy's branch that mirrors metrics to the failover endpoint while failover is on,
/// `let mrf_metrics_gateway = metrics | where <signals>`, then `mrf_metrics_gateway | emit mrf`.
const FAILOVER_GATEWAY: &str = "mrf_metrics_gateway";

/// The name the renaming scenario gives the aggregation.
pub const RENAMED_AGGREGATION: &str = "agg";

/// The node the recipe adds, which sends the checkout families to team B.
pub const TEAM_B_ROUTE: &str = "to_team_b";

/// The node that routes the platform family to an added destination.
pub const DESTINATION_ROUTE: &str = "to_destination";

/// The node S3b adds, the branch that sends the metrics tagged `team:checkout` to team B alone.
const EXCLUSIVE_ROUTE: &str = "checkout_only";

/// Returns a recipient name as a program writes an endpoint URL: quoted.
pub fn quoted(url: &str) -> String {
    format!("\"{}\"", url)
}

/// Returns whether `c` can be part of an identifier.
fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Returns, for each byte of `source`, whether it is code: outside strings and comments.
fn code_mask(source: &str) -> Vec<bool> {
    let mut mask = vec![true; source.len()];
    let mut in_string = false;
    let mut in_comment = false;
    let mut escaped = false;
    for (at, c) in source.char_indices() {
        let code = if in_comment {
            if c == '\n' {
                in_comment = false;
                true
            } else {
                false
            }
        } else if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            false
        } else if c == '"' {
            in_string = true;
            false
        } else if c == '#' {
            in_comment = true;
            false
        } else {
            true
        };
        for byte in &mut mask[at..at + c.len_utf8()] {
            *byte = code;
        }
    }
    mask
}

/// Returns the offsets at which `word` occurs in `source` as code, as a whole word: not part of a longer identifier,
/// and not a field after a `.`, such as `tags.<word>`.
fn words(source: &str, word: &str) -> Vec<usize> {
    let mask = code_mask(source);
    source
        .match_indices(word)
        .map(|(at, _)| at)
        .filter(|at| mask[*at])
        .filter(|at| {
            !source[..*at]
                .chars()
                .next_back()
                .is_some_and(|c| is_ident(c) || c == '.')
        })
        .filter(|at| !source[at + word.len()..].chars().next().is_some_and(is_ident))
        .collect()
}

/// Returns the offset of the `as` of the clause `as <id>` in `source`.
fn as_clause(source: &str, id: &str) -> Option<usize> {
    words(source, "as").into_iter().find(|at| {
        let rest = &source[at + 2..];
        let name = rest.trim_start();
        rest.len() > name.len() && name.starts_with(id) && !name[id.len()..].chars().next().is_some_and(is_ident)
    })
}

/// Returns the offset of the bracket that closes the one at `open`, which is `(`, `[`, or `{`.
fn matching_close(source: &str, open: usize) -> Option<usize> {
    let mask = code_mask(source);
    let mut depth = 0usize;
    for (at, c) in source[open..].char_indices().map(|(offset, c)| (open + offset, c)) {
        if !mask[at] {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the start of the line `at` is on.
fn line_start(source: &str, at: usize) -> usize {
    source[..at].rfind('\n').map_or(0, |newline| newline + 1)
}

/// Returns the end of the line `at` is on, just before its newline.
fn line_end(source: &str, at: usize) -> usize {
    source[at..].find('\n').map_or(source.len(), |newline| at + newline)
}

/// Returns the start of the comment lines right above the line that starts at `start`, or `start` if there are none:
/// the comments that belong to that line.
fn comments_above(source: &str, start: usize) -> usize {
    let mut start = start;
    while start > 0 {
        let previous = line_start(source, start - 1);
        if !source[previous..start].trim_start().starts_with('#') {
            break;
        }
        start = previous;
    }
    start
}

/// Returns the indentation of the line that starts at `start`, in spaces.
fn indent_of(source: &str, start: usize) -> usize {
    let line = &source[start..line_end(source, start)];
    line.len() - line.trim_start().len()
}

/// Returns each statement of `source`, as the start of its first line and the end of its last: a statement starts on a
/// line indented two spaces that is no comment and no closing brace, and goes on in the lines after it that are
/// indented further. Headers, such as `base`, are statements to this too; no edit looks for one.
fn statements(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < source.len() {
        let code = source[at..line_end(source, at)].trim_start();
        if indent_of(source, at) == 2 && !code.is_empty() && !code.starts_with(['#', '}']) {
            let start = at;
            let mut end = line_end(source, at);
            while end < source.len() {
                let next = end + 1;
                if source[next..line_end(source, next)].trim().is_empty() || indent_of(source, next) <= 2 {
                    break;
                }
                end = line_end(source, next);
            }
            found.push(start..end);
            at = end + 1;
        } else {
            at = line_end(source, at) + 1;
        }
    }
    found
}

/// Returns the name a statement's `let` defines, if it is a `let`.
fn let_name(statement: &str) -> Option<&str> {
    let rest = statement.trim_start().strip_prefix("let ")?.trim_start();
    let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
    rest[end..].trim_start().starts_with('=').then(|| &rest[..end])
}

/// Returns the offset in `statement` of its pipeline's head: just past the `=` of a `let`, its whitespace skipped, or
/// its first non-blank character.
fn head_offset(statement: &str) -> usize {
    let skip = |at: usize| at + (statement[at..].len() - statement[at..].trim_start().len());
    match let_name(statement) {
        Some(_) => skip(statement.find('=').expect("a `let` has an `=`") + 1),
        None => skip(0),
    }
}

/// The stream that every destination's branch reads, which the default program names `metrics`: what enrichment ends
/// in, or, where the export leaves enrichment out, the stream it would have read (`let metrics =
/// dsd_post_agg_filter`). Each of its uses, `metrics | ...`, is a branch of the policy.
pub const METRICS: &str = "metrics";

/// Returns the `let metrics = ...` statement, the stream every destination's branch reads.
fn policy(source: &str) -> Result<std::ops::Range<usize>, GenericError> {
    statements(source)
        .into_iter()
        .find(|statement| let_name(&source[statement.clone()]) == Some(METRICS))
        .ok_or_else(|| {
            generic_error!(
                "The default program has no stream that every destination reads (looked for `let {} = ...`).",
                METRICS
            )
        })
}

/// Returns the statements after `let metrics` that read it, `metrics | ...` or `let <x> = metrics | ...`: the branches
/// of the policy, in order.
fn uses(source: &str) -> Result<Vec<std::ops::Range<usize>>, GenericError> {
    let policy = policy(source)?;
    Ok(statements(source)
        .into_iter()
        .filter(|statement| statement.start > policy.start)
        .filter(|statement| {
            let text = &source[statement.clone()];
            text[head_offset(text)..]
                .strip_prefix(METRICS)
                .is_some_and(|rest| !rest.starts_with(|c: char| is_ident(c) || c == '.'))
        })
        .collect())
}

/// Appends `stage` (a stage, such as `where <predicate>`) to the end of `let metrics`'s pipeline, on a line of its own,
/// whichever operator ends it, so that every destination's branch reads what it passes. The stage then ends the `let`,
/// so it is the node `metrics`, and the policy's tee keeps its ID, `metrics_fanout`.
pub(crate) fn append_to_metrics(source: &str, stage: &str) -> Result<String, GenericError> {
    let end = policy(source)?.end;
    Ok(format!("{}\n    | {}{}", &source[..end], stage, &source[end..]))
}

/// Returns the span of the main `emit` in `source`, from `emit` to the end of its statement: the first branch of the
/// policy that emits to the primary endpoint, `metrics | emit primary` or `metrics | emit [primary, ...]`.
fn main_emit(source: &str) -> Result<std::ops::Range<usize>, GenericError> {
    for statement in uses(source)? {
        let text = &source[statement.clone()];
        let Some(emit) = words(text, "emit").first().copied() else {
            continue;
        };
        let recipients = text[emit + "emit".len()..].trim_start();
        let primary = recipients
            .strip_prefix('[')
            .unwrap_or(recipients)
            .trim_start()
            .strip_prefix("primary")
            .is_some_and(|rest| !rest.starts_with(is_ident));
        if primary && text[..emit].trim_end().ends_with('|') {
            return Ok(statement.start + emit..statement.start + text.trim_end().len());
        }
    }
    Err(generic_error!(
        "The default program has no branch that emits to the primary endpoint (looked for a statement `{} | emit \
         primary` or `{} | emit [primary, ...]`).",
        METRICS,
        METRICS
    ))
}

/// Adds a branch to the policy, a statement of its own right after the main `emit`'s: `branch` makes it from the
/// stream it reads, `metrics`.
pub(crate) fn add_policy_branch(source: &str, branch: impl FnOnce(&str) -> String) -> Result<String, GenericError> {
    let main = main_emit(source)?;
    let end = uses(source)?
        .into_iter()
        .find(|statement| statement.contains(&main.start))
        .map_or(main.end, |statement| statement.end);
    Ok(format!("{}\n\n  {}{}", &source[..end], branch(METRICS), &source[end..]))
}

/// Returns a branch, from `stream`, that emits what `predicate` matches to `recipients` (as `emit` writes them: one
/// bare, several in brackets) and drops the rest, its filter named `id` with a `let`: `let <id> = <stream> | where
/// <predicate>`, then `<id> | emit <recipients>`, as the default program writes its routing filters.
fn route(stream: &str, id: &str, predicate: &str, recipients: &str) -> String {
    format!(
        "let {id} = {stream} | where {predicate}\n\n  {id} | emit {recipients}",
        id = id,
        stream = stream,
        predicate = predicate,
        recipients = recipients
    )
}

/// Returns the statement `<id> | ...`, which reads the stream the `let` `id` names.
fn statement_reading(source: &str, id: &str) -> Option<std::ops::Range<usize>> {
    statements(source).into_iter().find(|statement| {
        let text = &source[statement.clone()];
        let head = &text[head_offset(text)..];
        head.strip_prefix(id)
            .is_some_and(|rest| !rest.starts_with(|c: char| is_ident(c) || c == '.'))
    })
}

/// Replaces the predicate of the `where` that ends the `let` named `id` (`let <id> = ... | where <predicate>`) with
/// what `edit` makes of it.
fn edit_where(
    source: &str, id: &str, what: &str, edit: impl FnOnce(&str) -> Result<String, GenericError>,
) -> Result<String, GenericError> {
    let statement = statements(source)
        .into_iter()
        .find(|statement| let_name(&source[statement.clone()]) == Some(id))
        .ok_or_else(|| {
            generic_error!(
                "The default program has no {} (looked for `let {} = ... | where ...`).",
                what,
                id
            )
        })?;
    let text = &source[statement.clone()];
    let keyword = words(text, "where")
        .last()
        .copied()
        .ok_or_else(|| generic_error!("The {} `{}` does not end in a `where` stage.", what, id))?;
    let written = &text[keyword + "where".len()..];
    // The `where` must be the `let`'s last stage: a `|` in code after it means another stage comes after it.
    let mask = code_mask(text);
    let crosses_a_stage = written
        .char_indices()
        .any(|(offset, c)| c == '|' && mask[keyword + "where".len() + offset]);
    if crosses_a_stage {
        return Err(generic_error!("The {} `{}` does not end in a `where` stage.", what, id));
    }
    let predicate = written.trim();
    if predicate.is_empty() {
        return Err(generic_error!("The {} `{}` has no predicate.", what, id));
    }
    let trailing = &written[written.trim_end().len()..];
    let keyword = statement.start + keyword;
    Ok(format!(
        "{}where {}{}{}",
        &source[..keyword],
        edit(predicate)?,
        trailing,
        &source[statement.end..]
    ))
}

/// Returns the offset of the operator name of the first stage that runs `op`: the name after a `|`, or at the start of
/// a line.
fn stage(source: &str, op: &str) -> Option<usize> {
    words(source, op).into_iter().find(|at| {
        let before = source[..*at].trim_end_matches([' ', '\t']);
        before.ends_with('|') || before.ends_with('\n') || before.is_empty()
    })
}

/// Sets the parameter `param` of the first stage that runs `op` to `value`, the text of a value: where it is written
/// with its name, where it is written first without one (when `positional`, the operator's positional parameter), or,
/// where it is left out, added after the others.
fn set_param(source: &str, op: &str, param: &str, positional: bool, value: &str) -> Result<String, GenericError> {
    let at = stage(source, op).ok_or_else(|| generic_error!("The default program has no `{}` stage.", op))?;
    let after_name = at + op.len();
    if !source[after_name..].starts_with('(') {
        // Written without parentheses: every parameter is left out.
        return Ok(format!(
            "{}({}: {}){}",
            &source[..after_name],
            param,
            value,
            &source[after_name..]
        ));
    }
    let close = matching_close(source, after_name)
        .ok_or_else(|| generic_error!("The `{}` stage's parameters have no end.", op))?;
    let mask = code_mask(source);
    // The arguments at the stage's own depth, each as its span.
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = after_name + 1;
    for (offset, c) in source[after_name + 1..close].char_indices() {
        let index = after_name + 1 + offset;
        if !mask[index] {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                arguments.push(start..index);
                start = index + 1;
            }
            _ => {}
        }
    }
    arguments.push(start..close);
    let named = |argument: &std::ops::Range<usize>| {
        let text = source[argument.clone()].trim_start();
        text.split_once(':')
            .filter(|(name, _)| !name.is_empty() && name.trim_end().chars().all(is_ident))
            .map(|(name, _)| name.trim_end().to_string())
    };
    let replace = |argument: &std::ops::Range<usize>, value_start: usize| {
        let written = &source[value_start..argument.end];
        let trailing = &written[written.trim_end().len()..];
        format!(
            "{}{}{}{}",
            &source[..value_start],
            value,
            trailing,
            &source[argument.end..]
        )
    };
    for argument in &arguments {
        if named(argument).as_deref() == Some(param) {
            let colon = argument.start
                + source[argument.clone()]
                    .find(':')
                    .expect("a named argument has a colon");
            let value_start = colon
                + 1
                + (source[colon + 1..argument.end].len() - source[colon + 1..argument.end].trim_start().len());
            return Ok(replace(argument, value_start));
        }
    }
    if positional {
        if let Some(first) = arguments
            .first()
            .filter(|first| named(first).is_none() && !source[(*first).clone()].trim().is_empty())
        {
            let value_start = first.end - source[first.clone()].trim_start().len();
            return Ok(replace(first, value_start));
        }
    }
    // Left out: added after the last argument.
    let written = source[after_name + 1..close].trim_end();
    let end = after_name + 1 + written.len();
    let added = if written.trim().is_empty() {
        format!("{}: {}", param, value)
    } else if written.ends_with(',') {
        format!(" {}: {},", param, value)
    } else {
        format!(", {}: {}", param, value)
    };
    Ok(format!("{}{}{}", &source[..end], added, &source[end..]))
}

/// Replaces every use of the name `from` in `source`'s code with `to`: its `let`, its `as` clause, and every reference
/// to it.
fn rename(source: &str, from: &str, to: &str) -> Result<String, GenericError> {
    let found = words(source, from);
    if found.is_empty() {
        return Err(generic_error!("The default program does not name `{}`.", from));
    }
    let mut renamed = source.to_string();
    for at in found.into_iter().rev() {
        renamed.replace_range(at..at + from.len(), to);
    }
    Ok(renamed)
}

/// The stage S1 adds, which is also the first recipe.
pub const DROP_DEBUG: &str = "where not (name starts_with \"demo.debug.\")";

/// S1: drops every `demo.debug.*` metric ahead of the policy, after aggregation and enrichment: the stage
/// [`DROP_DEBUG`] at the end of the stream every destination reads. It is not named: its ID is the one the syntax
/// derives where it goes, which [`is_drop_debug`] recognizes.
pub fn drop_debug(source: &str) -> Result<String, GenericError> {
    append_to_metrics(source, DROP_DEBUG)
}

/// Returns whether `node`, a node of a plan or artifact document, is the `split` that [`drop_debug`] adds.
pub fn is_drop_debug(node: &serde_json::Value) -> bool {
    node["op"] == "split" && node["params"].to_string().contains("\"demo.debug.\"")
}

/// S2, the recipe: narrows team B to the checkout families. Team B's URL leaves the main `emit`, and a branch of the
/// policy, [`TEAM_B_ROUTE`], sends `demo.checkout.*` to team B.
pub fn narrow_team_b(source: &str, team_b: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let name = quoted(team_b);
    let emit = &source[line.clone()];
    let recipients = emit
        .trim_start_matches("emit")
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(str::trim)
        .filter(|recipient| !recipient.is_empty())
        .collect::<Vec<_>>();
    if !recipients.contains(&name.as_str()) {
        return Err(generic_error!(
            "The main `emit` does not name team B ({:?}): {}",
            team_b,
            emit
        ));
    }
    let rest = recipients
        .into_iter()
        .filter(|recipient| *recipient != name)
        .collect::<Vec<_>>();
    let narrowed = match rest.as_slice() {
        [one] => format!("emit {}", one),
        rest => format!("emit [{}]", rest.join(", ")),
    };
    let narrowed = format!("{}{}{}", &source[..line.start], narrowed, &source[line.end..]);
    add_policy_branch(&narrowed, |stream| {
        route(stream, TEAM_B_ROUTE, "name starts_with \"demo.checkout.\"", &name)
    })
}

/// S3a: widens the recipe's route to team B to the metrics tagged `team:platform` as well.
pub fn widen_team_b(narrowed: &str) -> Result<String, GenericError> {
    edit_where(narrowed, TEAM_B_ROUTE, "route to team B", |predicate| {
        if predicate != "name starts_with \"demo.checkout.\"" {
            return Err(generic_error!(
                "The route to team B does not route by the checkout prefix: {}",
                predicate
            ));
        }
        Ok(format!("{} or tags.team == \"platform\"", predicate))
    })
}

/// The metrics S3b sends to team B alone.
const CHECKOUT: &str = "tags.team == \"checkout\"";

/// S3b: routes the metrics tagged `team:checkout` to team B only: they no longer reach the primary endpoint, team C,
/// or any other destination.
///
/// Every branch of the policy reads every metric, so the edit says so in each: each branch the default program has
/// excludes them first, `metrics | where tags.team != "checkout" | ...`, and a branch of its own,
/// [`EXCLUSIVE_ROUTE`], sends them to team B, `let checkout_only = metrics | where tags.team == "checkout"`. Each
/// exclusion and the route split on the same predicate and send the metrics opposite ways, which is how validation
/// knows that no metric reaches team B twice, and the policy's tee and its branches keep their IDs.
pub fn exclusive_team_b(source: &str, team_b: &str) -> Result<String, GenericError> {
    let mut edited = source.to_string();
    // From the last branch, so that the offsets of the others hold.
    for statement in uses(source)?.into_iter().rev() {
        let head = statement.start + head_offset(&source[statement.clone()]);
        let end = source[head..]
            .find(|c: char| !(is_ident(c) || c == '.'))
            .map_or(source.len(), |length| head + length);
        edited.insert_str(end, " | where tags.team != \"checkout\"");
    }
    add_policy_branch(&edited, |stream| {
        route(stream, EXCLUSIVE_ROUTE, CHECKOUT, &quoted(team_b))
    })
}

/// S4a: mirrors only one checkout family on failover: the failover gateway's predicate gains
/// `and name == "<family>"`, so that, while failover is on (a signal), the failover endpoint receives
/// [`MIRRORED_METRIC`] alone.
pub fn mirror_checkout_on_failover(source: &str) -> Result<String, GenericError> {
    edit_where(source, FAILOVER_GATEWAY, "failover gateway", |predicate| {
        // `and` binds tighter than `or`: a predicate with an `or` of its own keeps its meaning in parentheses.
        let predicate = if words(predicate, "or").is_empty() {
            predicate.to_string()
        } else {
            format!("({})", predicate)
        };
        Ok(format!("{} and name == \"{}\"", predicate, MIRRORED_METRIC))
    })
}

/// S5, duplicate branch: a second branch of the policy to the main `emit`'s recipients, so that every record reaches
/// them twice.
pub fn duplicate_branch(source: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let emit = source[line].to_string();
    add_policy_branch(source, |stream| format!("{} | {}", stream, emit))
}

/// S5, two hosts: the failover gateway emits to the primary endpoint and the failover endpoint together, which are
/// recipients of two delivery hosts.
pub fn two_hosts(source: &str) -> Result<String, GenericError> {
    let not_found = || {
        generic_error!(
            "The default program has no failover gateway (looked for `{} | emit mrf`).",
            FAILOVER_GATEWAY
        )
    };
    let statement = statement_reading(source, FAILOVER_GATEWAY).ok_or_else(not_found)?;
    let emit = statement.start
        + words(&source[statement.clone()], "emit")
            .first()
            .copied()
            .ok_or_else(not_found)?;
    let recipients = source[emit + "emit".len()..statement.end].trim();
    if recipients != "mrf" && recipients != "[mrf]" {
        return Err(not_found());
    }
    Ok(format!(
        "{}emit [primary, mrf]{}",
        &source[..emit],
        &source[statement.end..]
    ))
}

/// S5, a policy binding: the aggregation's percentiles bind the native setting again,
/// `config.metrics_encoding.histogram.percentiles`, where the detached export writes their value or leaves out the
/// default. A published program owns its policy: it may bind facts and signals only.
///
/// The aggregation is in every default program's export, configured or not; the metric filterlist, which this edit
/// bound before, is left out of the export while it is empty, as it is in the demo.
pub fn policy_binding(source: &str) -> Result<String, GenericError> {
    set_param(
        source,
        "aggregate",
        "histogram_percentiles",
        false,
        "config.metrics_encoding.histogram.percentiles",
    )
}

/// S5, an unknown recipient: a branch of the policy emits everything to [`UNKNOWN_RECIPIENT`], which ADP's directory
/// does not name: ADP holds the program pending until a recipient of that name is declared.
pub fn unknown_recipient(source: &str) -> Result<String, GenericError> {
    add_policy_branch(source, |stream| format!("{} | emit {}", stream, UNKNOWN_RECIPIENT))
}

/// S9c: a branch of the policy, [`DESTINATION_ROUTE`], that sends the metrics tagged `team:platform` to the
/// destination at `url` as well.
pub fn route_platform_to(source: &str, url: &str) -> Result<String, GenericError> {
    add_policy_branch(source, |stream| {
        route(stream, DESTINATION_ROUTE, "tags.team == \"platform\"", &quoted(url))
    })
}

/// S14: renames the aggregation, [`AGGREGATION_NODE`], to [`RENAMED_AGGREGATION`]: the `let` that names it and every
/// reference to it. A program's one aggregation is the aggregation whatever its name, so its state carries over.
pub fn rename_aggregation(source: &str) -> Result<String, GenericError> {
    rename(source, AGGREGATION_NODE, RENAMED_AGGREGATION)
}

/// The operators whose windows a width change sets: DogStatsD aggregation, and the timestamp split ahead of it, whose
/// window must equal its aggregation's. The timestamp split's window is its positional parameter.
pub const WINDOWED_OPERATORS: [(&str, bool); 2] = [("timestamp_split", true), ("aggregate", false)];

/// S13: changes DogStatsD aggregation's width in the default program's source to `width` (a duration literal, such as
/// `30s`): the `window` of each of [`WINDOWED_OPERATORS`]'s stages, whatever it was set to, becomes the literal.
pub fn width_change(source: &str, width: &str) -> Result<String, GenericError> {
    let mut edited = source.to_string();
    for (op, positional) in WINDOWED_OPERATORS {
        edited = set_param(&edited, op, "window", positional, width)?;
    }
    Ok(edited)
}

/// S5, a second aggregation: an aggregation with every parameter, in front of the policy. ADP runs at most one
/// aggregation per program, so validation rejects it at its node, as the editor shows.
pub fn second_aggregation(source: &str) -> Result<String, GenericError> {
    append_to_metrics(
        source,
        "aggregate(window: 10s, flush_interval: 10s, context_limit: 1000, counter_expiry: 5m, flush_open_windows: \
         false, histogram_aggregates: [\"max\", \"count\"], histogram_percentiles: [95], copy_to_distribution: false, \
         copy_to_distribution_prefix: \"\")",
    )
}

/// S5, the aggregation moved after enrichment: DogStatsD aggregation's stage moves to the end of `let metrics`, and
/// what fed it feeds what it fed. It ends `let metrics` there, so it is the node `metrics` and the policy's tee keeps
/// its ID; a program's one aggregation takes the default program's ID whatever the program calls it. The aggregation ends the `let` [`AGGREGATION_NODE`]; once
/// it is gone, the stage before it ends the `let`, which takes that stage's ID as its name so that the stage keeps it,
/// or, when the `let` has no other stage, the `let` goes and its uses read what it read. The program still has one
/// aggregation, but the frame runs it only in its isolation slot, which enrichment has no exit to, so ADP refuses it
/// when it places the plan.
pub fn aggregation_after_enrichment(source: &str) -> Result<String, GenericError> {
    const OP: &str = "aggregate";
    let op = stage(source, OP).ok_or_else(|| generic_error!("The default program has no `{}` stage.", OP))?;
    let mask = code_mask(source);
    let bar = source[..op]
        .char_indices()
        .rev()
        .find(|(at, c)| mask[*at] && !c.is_whitespace())
        .filter(|(_, c)| *c == '|')
        .map(|(at, _)| at)
        .ok_or_else(|| generic_error!("The aggregation does not follow a `|`."))?;
    // The stage: its operator and parameters, to the end of its line.
    let end = if source[op + OP.len()..].starts_with('(') {
        matching_close(source, op + OP.len())
            .map(|close| close + 1)
            .ok_or_else(|| generic_error!("The aggregation's parameters have no end."))?
    } else {
        op + OP.len()
    };
    let stage_end = line_end(source, end);
    if !source[end..stage_end].trim().is_empty() {
        return Err(generic_error!(
            "The aggregation has an `as` clause or a stage after it on its line: {}",
            &source[end..stage_end]
        ));
    }
    let moved = source[op..end].to_string();
    // It ends a `let`, which names it: the next line of code continues no pipeline.
    let next = source[stage_end..]
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("");
    if next.starts_with('|') {
        return Err(generic_error!("The aggregation does not end its `let`."));
    }
    let statement = source[..op]
        .rfind("\n  let ")
        .map(|at| at + 1)
        .ok_or_else(|| generic_error!("The aggregation is in no `let`."))?;
    let name_start = statement + "  let ".len();
    let name = source[name_start..]
        .split(|c: char| !is_ident(c))
        .next()
        .unwrap_or_default()
        .to_string();
    if name != AGGREGATION_NODE {
        return Err(generic_error!(
            "The aggregation's `let` is `{}`, not `{}`.",
            name,
            AGGREGATION_NODE
        ));
    }
    let equals = name_start
        + source[name_start..bar]
            .find('=')
            .ok_or_else(|| generic_error!("The aggregation's `let` has no `=`."))?;
    // What the `let` says before the aggregation, without its comments: its head, and any stages after it.
    let before = source[equals + 1..bar]
        .lines()
        .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join(" ");
    let before = before.trim();
    let own_line = source[line_start(source, op)..op].trim() == "|";
    let edited = match before.rsplit_once('|') {
        // `let dsd_agg = <head> | aggregate(...)`: the `let` goes, and its uses read the head.
        None => {
            let from = comments_above(source, statement);
            let to = (stage_end + 1).min(source.len());
            let to = if source[to..].starts_with('\n') { to + 1 } else { to };
            let removed = format!("{}{}", &source[..from], &source[to..]);
            rename(&removed, AGGREGATION_NODE, before)?
        }
        // `let dsd_agg = <head> | ... | <stage> as <id> | aggregate(...)`: the stage before the aggregation ends the
        // `let`, which takes its ID.
        Some((_, feeder)) => {
            let feeder = feeder
                .trim()
                .rsplit_once(" as ")
                .map(|(_, id)| id.trim())
                .filter(|id| !id.is_empty() && id.chars().all(is_ident))
                .ok_or_else(|| {
                    generic_error!(
                        "The stage that feeds the aggregation has no `as` clause, so its ID would change with the \
                         `let`'s."
                    )
                })?
                .to_string();
            let removed = if own_line {
                let from = comments_above(source, line_start(source, op));
                format!("{}{}", &source[..from], &source[(stage_end + 1).min(source.len())..])
            } else {
                let from = source[..bar].trim_end().len();
                format!("{}{}", &source[..from], &source[end..])
            };
            let clause = as_clause(&removed, &feeder).expect("the feeder's `as` clause is still there");
            let clause_start = removed[..clause].trim_end().len();
            let mut removed = removed;
            removed.replace_range(clause_start..clause + "as ".len() + feeder.len(), "");
            rename(&removed, AGGREGATION_NODE, &feeder)?
        }
    };
    append_to_metrics(&edited, &moved)
}

#[cfg(test)]
mod tests;
