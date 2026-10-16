//! The edits of the scenarios, made to the default program's source as the editor shows it, the way a presenter
//! types them.
//!
//! The default program is the whole metrics path, its one scope fed by every source; the policy region starts at its
//! `fork`, [`POLICY_NODE`] (`| fork as metrics_policy { ... }`), which enrichment feeds, and whose first branch emits to
//! the main output's recipients: `emit [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]`, the primary
//! endpoint and every additional one. Each edit finds the text it changes by what it says, never by line number or
//! exact layout: a stage by its operator (`timestamp_split(`), a node by its `as` clause or the `let` that names it, the
//! policy `fork` by `fork as metrics_policy`, the main `emit` as the first branch of the policy `fork` that emits to
//! `primary`. Strings and comments are never matched. An edit fails with a message naming what it looked for when the
//! default program is printed otherwise, so that a change of the printed form shows up as a clear check failure.

use saluki_error::{generic_error, GenericError};

/// The family the failover scenario mirrors, which the failover endpoint then receives while failover is on.
pub const MIRRORED_METRIC: &str = "demo.checkout.requests";

/// The recipient the unknown-recipient guardrail emits to, which ADP's directory does not name.
pub const UNKNOWN_RECIPIENT: &str = "team_z";

/// The family the routing to an added destination sends there: the metrics tagged `team:platform`.
pub const PLATFORM_FAMILY: &str = "demo.platform.queue_depth";

/// The policy `fork`, where the policy region begins: enrichment feeds it, and it has one branch per destination.
pub const POLICY_NODE: &str = "metrics_policy";

/// DogStatsD aggregation, the program's one aggregation: the `let` that ends in it names it.
const AGGREGATION_NODE: &str = "dsd_agg";

/// The failover gateway: the policy `fork`'s branch that mirrors metrics to the failover endpoint while failover is
/// on, `where <signals> as mrf_metrics_gateway | emit mrf`.
const FAILOVER_GATEWAY: &str = "mrf_metrics_gateway";

/// The name the renaming scenario gives the aggregation.
pub const RENAMED_AGGREGATION: &str = "agg";

/// The node the recipe adds, which sends the checkout families to team B.
pub const TEAM_B_ROUTE: &str = "to_team_b";

/// The node that routes the platform family to an added destination.
pub const DESTINATION_ROUTE: &str = "to_destination";

/// The node S3b adds, which sends the metrics tagged `team:checkout` to team B alone, ahead of the policy.
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

/// Returns the indentation of the line `at` is on.
fn indent_at(source: &str, at: usize) -> &str {
    let start = line_start(source, at);
    let line = &source[start..line_end(source, start)];
    &line[..line.len() - line.trim_start().len()]
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

/// Returns the offset of the `fork` keyword of the policy `fork`, `fork as metrics_policy`.
fn policy_fork(source: &str) -> Result<usize, GenericError> {
    let not_found = || {
        generic_error!(
            "The default program has no policy `fork` (looked for `fork as {}`).",
            POLICY_NODE
        )
    };
    let at = as_clause(source, POLICY_NODE).ok_or_else(not_found)?;
    let before = source[..at].trim_end();
    if !before.ends_with("fork") || before[..before.len() - 4].chars().next_back().is_some_and(is_ident) {
        return Err(not_found());
    }
    Ok(before.len() - 4)
}

/// Puts `stage` (a stage, such as `where <predicate>`) right before the policy `fork`, so that what enrichment feeds
/// the policy passes through it first: on a line of its own above the fork's line and the comments above it, or, when
/// the fork does not start its line, inline.
pub(crate) fn insert_before_policy(source: &str, stage: &str) -> Result<String, GenericError> {
    let fork = policy_fork(source)?;
    let start = line_start(source, fork);
    let before = source[start..fork].trim();
    let indent = indent_at(source, fork).to_string();
    if before == "|" {
        // `| fork as metrics_policy {`, a stage of its own line.
        let above = comments_above(source, start);
        Ok(format!(
            "{}{}| {}\n{}",
            &source[..above],
            indent,
            stage,
            &source[above..]
        ))
    } else if before.is_empty() {
        // A fork that starts a branch of another fork: the stage starts the branch instead.
        let above = comments_above(source, start);
        Ok(format!(
            "{}{}{}\n{}| {}",
            &source[..above],
            indent,
            stage,
            &source[above..fork],
            &source[fork..]
        ))
    } else {
        // `<head> | fork as metrics_policy {` on one line.
        Ok(format!("{}{} | {}", &source[..fork], stage, &source[fork..]))
    }
}

/// Returns the span of the main `emit` line in `source`: the first branch of the policy `fork` that emits to the
/// primary endpoint, from the line's first non-blank character to its end.
fn main_emit(source: &str) -> Result<std::ops::Range<usize>, GenericError> {
    let not_found = || {
        generic_error!(
            "The default program's policy `fork` has no branch that emits to the primary endpoint (looked for a line \
             `emit primary` or `emit [primary, ...]` after `fork as {}`).",
            POLICY_NODE
        )
    };
    let fork = policy_fork(source)?;
    let open = fork + source[fork..].find('{').ok_or_else(not_found)?;
    let close = matching_close(source, open).ok_or_else(not_found)?;
    let mut at = line_end(source, open) + 1;
    while at < close {
        let end = line_end(source, at);
        let line = source[at..end].trim_start();
        if line.starts_with("emit primary") || line.starts_with("emit [primary") {
            let start = end - source[at..end].trim_start().len();
            return Ok(start..end);
        }
        at = end + 1;
    }
    Err(not_found())
}

/// Adds a branch to the policy `fork`, right after the main `emit`.
pub(crate) fn add_policy_branch(source: &str, branch: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let indent = indent_at(source, line.start).to_string();
    Ok(format!(
        "{}\n{}{}{}",
        &source[..line.end],
        indent,
        branch,
        &source[line.end..]
    ))
}

/// Returns a branch that emits what `predicate` matches to `recipients` (as `emit` writes them: one bare, several in
/// brackets) and drops the rest, as the node `id`.
fn route(id: &str, predicate: &str, recipients: &str) -> String {
    format!("where {} as {} | emit {}", predicate, id, recipients)
}

/// Replaces the predicate of the `where` stage named `id` (`where <predicate> as <id>`) with what `edit` makes of it.
fn edit_where(
    source: &str, id: &str, what: &str, edit: impl FnOnce(&str) -> Result<String, GenericError>,
) -> Result<String, GenericError> {
    let at = as_clause(source, id).ok_or_else(|| {
        generic_error!(
            "The default program has no {} (looked for `where ... as {}`).",
            what,
            id
        )
    })?;
    let keyword = words(&source[..at], "where")
        .last()
        .copied()
        .ok_or_else(|| generic_error!("The {} `{}` is not a `where` stage.", what, id))?;
    let written = &source[keyword + "where".len()..at];
    // The `where` must be the stage the `as` names: a `|` in code between them means another stage, or another
    // statement, comes first.
    let mask = code_mask(source);
    let crosses_a_stage = written
        .char_indices()
        .any(|(offset, c)| c == '|' && mask[keyword + "where".len() + offset]);
    if crosses_a_stage {
        return Err(generic_error!("The {} `{}` is not a `where` stage.", what, id));
    }
    let predicate = written.trim();
    if predicate.is_empty() {
        return Err(generic_error!("The {} `{}` has no predicate.", what, id));
    }
    let trailing = &written[written.trim_end().len()..];
    Ok(format!(
        "{}where {}{}{}",
        &source[..keyword],
        edit(predicate)?,
        trailing,
        &source[at..]
    ))
}

/// Returns the offset of the operator name of the first stage that runs `op`: the name after a `|`, or at the start of
/// a line, as a fork's branch starts.
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

/// S1: drops every `demo.debug.*` metric ahead of the policy, after aggregation and enrichment.
pub fn drop_debug(source: &str) -> Result<String, GenericError> {
    insert_before_policy(source, "where not (name starts_with \"demo.debug.\") as drop_debug")
}

/// S2, the recipe: narrows team B to the checkout families. Team B's URL leaves the main `emit`, and a branch of the
/// policy `fork`, [`TEAM_B_ROUTE`], sends `demo.checkout.*` to team B.
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
    add_policy_branch(
        &narrowed,
        &route(TEAM_B_ROUTE, "name starts_with \"demo.checkout.\"", &name),
    )
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

/// Returns the first line, at or above the line `at` is on, that starts a statement: at the statement's indentation, two
/// spaces, and no comment.
fn statement_start(source: &str, at: usize) -> Result<usize, GenericError> {
    let mut statement = line_start(source, at);
    loop {
        let line = &source[statement..line_end(source, statement)];
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && !line.trim_start().starts_with('#') && !line.trim().is_empty() {
            return Ok(statement);
        }
        if statement == 0 {
            return Err(generic_error!("The statement around offset {} has no start.", at));
        }
        statement = line_start(source, statement - 1);
    }
}

/// Removes `width` spaces of indentation from each line of `text` that has them.
fn dedent(text: &str, width: usize) -> String {
    let prefix = " ".repeat(width);
    text.split_inclusive('\n')
        .map(|line| line.strip_prefix(prefix.as_str()).unwrap_or(line))
        .collect()
}

/// Writes the `fork` whose block holds the offset `inner` in its long form: the statement it ends becomes
/// `let <fork> = ... | tee(ports: n)`, and each branch `i` a statement of its own that starts from `<fork>.i`, with the
/// comments above it. The node IDs stay as they were: the tee is named by the `let`, and a branch's nodes derive
/// theirs from the port they start from either way.
fn unfork_around(source: &str, inner: usize) -> Result<String, GenericError> {
    let mask = code_mask(source);
    // The `{` that holds `inner`: the nearest one before it that is not closed before it.
    let mut depth = 0usize;
    let mut open = None;
    for (at, c) in source[..inner].char_indices().rev() {
        if !mask[at] {
            continue;
        }
        match c {
            '}' | ')' | ']' => depth += 1,
            '{' | '(' | '[' if depth > 0 => depth -= 1,
            '{' => {
                open = Some(at);
                break;
            }
            _ => {}
        }
    }
    let open = open.ok_or_else(|| generic_error!("The policy `fork` is in no other fork."))?;
    let close = matching_close(source, open).ok_or_else(|| generic_error!("The outer fork has no end."))?;
    let header = source[..open].trim_end();
    let (before, name) = header
        .rsplit_once(" as ")
        .filter(|(before, name)| before.trim_end().ends_with("fork") && name.chars().all(is_ident))
        .ok_or_else(|| generic_error!("The fork around the policy `fork` has no `as` clause: {}", header))?;
    let keyword = before.trim_end().len() - "fork".len();
    let statement = statement_start(source, keyword)?;
    if source[statement..].trim_start().starts_with("let ") {
        return Err(generic_error!(
            "The fork around the policy `fork` ends a `let`, which no pipeline may."
        ));
    }
    // The branches: each starts on a line at the branches' indentation, after the comments above it, and goes on in
    // the lines indented further.
    let body = &source[line_end(source, open) + 1..line_start(source, close)];
    let indent = body
        .lines()
        .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| line.len() - line.trim_start().len())
        .ok_or_else(|| generic_error!("The fork around the policy `fork` has no branches."))?;
    let mut branches: Vec<(String, String)> = Vec::new();
    let mut comments = String::new();
    for line in body.split_inclusive('\n') {
        let line_indent = line.len() - line.trim_start().len();
        let code = line.trim();
        if code.is_empty() {
            continue;
        }
        if line_indent <= indent && code.starts_with('#') {
            comments.push_str(&format!("  {}\n", code));
        } else if line_indent <= indent && !code.starts_with(['}', ')', ']']) {
            branches.push((std::mem::take(&mut comments), line.trim_start().to_string()));
        } else if let Some((_, branch)) = branches.last_mut() {
            branch.push_str(&dedent(line, indent - 2));
        }
    }
    let mut statements = format!(
        "{}let {} = {}tee(ports: {})\n",
        &source[statement..statement + 2],
        name,
        &source[statement + 2..keyword],
        branches.len()
    );
    for (index, (comments, branch)) in branches.iter().enumerate() {
        statements.push_str(&format!("\n{}  {}.{} | {}", comments, name, index, branch));
    }
    let after = line_end(source, close);
    Ok(format!(
        "{}{}{}",
        &source[..statement],
        statements,
        &source[after + 1..]
    ))
}

/// S3b: routes the metrics tagged `team:checkout` to team B only, ahead of the policy `fork`: they no longer reach the
/// primary endpoint or team C.
///
/// A `split` whose ports are both used is named by a `let`, so the statement that ends in the policy `fork` becomes
/// `let checkout_only = <what it was> | split(...)`, its `true` port emits to team B, and its `false` port carries on
/// to the policy `fork`. A policy `fork` that is a branch of another fork is first given a statement of its own, the
/// other fork written in its long form.
pub fn exclusive_team_b(source: &str, team_b: &str) -> Result<String, GenericError> {
    let fork = policy_fork(source)?;
    if source[line_start(source, fork)..fork].trim().is_empty() {
        let unforked = unfork_around(source, fork)?;
        return exclusive_team_b(&unforked, team_b);
    }
    let fork_line = line_start(source, fork);
    // The statement's first line: the last line above the fork's that starts at the statement's indentation, two
    // spaces, and is no comment.
    let mut statement = fork_line;
    loop {
        let line = &source[statement..line_end(source, statement)];
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && !line.trim_start().starts_with('#') && !line.trim().is_empty() {
            break;
        }
        if statement == 0 {
            return Err(generic_error!(
                "The statement that ends in the policy `fork` has no start."
            ));
        }
        statement = line_start(source, statement - 1);
    }
    if source[statement..].trim_start().starts_with("let ") {
        return Err(generic_error!("The policy `fork` ends a `let`, which no pipeline may."));
    }
    let head = statement + 2;
    let routes = format!(
        "\n  {id}.true | emit {team_b}\n\n  {id}.false\n",
        id = EXCLUSIVE_ROUTE,
        team_b = quoted(team_b),
    );
    let edited = if source[fork_line..fork].trim() == "|" {
        // `| fork as metrics_policy {` on a line of its own: the split goes above it and the comments above it, and
        // the `false` port's statement continues with them.
        let above = comments_above(source, fork_line);
        let indent = indent_at(source, fork);
        format!(
            "{}{}| split(tags.team == \"checkout\")\n{}{}",
            &source[..above],
            indent,
            routes,
            &source[above..]
        )
    } else {
        // `<head> | fork as metrics_policy {` on one line: the split takes the fork's place on it.
        let bar = source[..fork].rfind('|').expect("an inline fork follows a `|`");
        format!(
            "{}| split(tags.team == \"checkout\")\n{}    | {}",
            &source[..bar],
            routes,
            &source[fork..]
        )
    };
    Ok(format!(
        "{}let {} = {}",
        &edited[..head],
        EXCLUSIVE_ROUTE,
        &edited[head..]
    ))
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

/// S5, duplicate branch: a second branch of the policy `fork` to the main `emit`'s recipients, so that every record
/// reaches them twice.
pub fn duplicate_branch(source: &str) -> Result<String, GenericError> {
    let line = main_emit(source)?;
    let emit = source[line].to_string();
    add_policy_branch(source, &emit)
}

/// S5, two hosts: the failover gateway emits to the primary endpoint and the failover endpoint together, which are
/// recipients of two delivery hosts.
pub fn two_hosts(source: &str) -> Result<String, GenericError> {
    let not_found = || {
        generic_error!(
            "The default program has no failover gateway (looked for `where ... as {} | emit mrf`).",
            FAILOVER_GATEWAY
        )
    };
    let at = as_clause(source, FAILOVER_GATEWAY).ok_or_else(not_found)?;
    let emit = at + words(&source[at..], "emit").first().copied().ok_or_else(not_found)?;
    let end = line_end(source, emit);
    let recipients = source[emit + "emit".len()..end].trim();
    if recipients != "mrf" && recipients != "[mrf]" {
        return Err(not_found());
    }
    Ok(format!("{}emit [primary, mrf]{}", &source[..emit], &source[end..]))
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

/// S5, an unknown recipient: a branch of the policy `fork` emits everything to [`UNKNOWN_RECIPIENT`], which ADP's
/// directory does not name: ADP holds the program pending until a recipient of that name is declared.
pub fn unknown_recipient(source: &str) -> Result<String, GenericError> {
    add_policy_branch(source, &format!("emit {}", UNKNOWN_RECIPIENT))
}

/// S9c: a branch of the policy `fork`, [`DESTINATION_ROUTE`], that sends the metrics tagged `team:platform` to the
/// destination at `url` as well.
pub fn route_platform_to(source: &str, url: &str) -> Result<String, GenericError> {
    add_policy_branch(
        source,
        &route(DESTINATION_ROUTE, "tags.team == \"platform\"", &quoted(url)),
    )
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
    insert_before_policy(
        source,
        "aggregate(window: 10s, flush_interval: 10s, context_limit: 1000, counter_expiry: 5m, flush_open_windows: \
         false, histogram_aggregates: [\"max\", \"count\"], histogram_percentiles: [95], copy_to_distribution: false, \
         copy_to_distribution_prefix: \"\") as rollup",
    )
}

/// S5, the aggregation moved after enrichment: DogStatsD aggregation's stage moves in front of the policy, keeping its
/// ID with `as dsd_agg`, and what fed it feeds what it fed. The aggregation ends the `let` [`AGGREGATION_NODE`]; once
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
    insert_before_policy(&edited, &format!("{} as {}", moved, AGGREGATION_NODE))
}

#[cfg(test)]
mod tests;
