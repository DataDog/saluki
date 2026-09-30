//! Checks every result of replaying the corpus against the checked-in `known-results.txt`.
//!
//! The file lists every result that is not a plain match, one fact per tab-separated line, sorted, so
//! that any change to what the replay computes shows up as a line added or removed. Its columns:
//!
//! - `count <group> <label> <n>`: rows by leaf verdict (`NotCompared` by a stable reason code, for
//!   example `not-emulated:Bool` or `explicit-only`), cases by outcome, and system steps by outcome.
//! - `case <name> <match> <differs> <adp-rejects> <not-modeled> <not-compared> `: one line per started
//!   case, the counts of that case's leaf rows at both checkpoints, by the same five verdicts.
//! - `startup-failed <name>`: one line per case whose Agent startup failed (its count is on the
//!   `count cases startup-failed` line).
//! - `leaf <case> <checkpoint> <key> <getter> <kind> <streamed> differs <adp value> <agent value>`, or
//!   `... adp-rejects <error>`: every leaf row that is neither a match nor not compared. `<kind>` is
//!   the key's `LeafValue` variant name (for example `Bool`); `<streamed>` is the value at the key's
//!   path in the folded tree at that checkpoint (JSON), or `-` when the tree does not hold it.
//! - `derived <case> <checkpoint> <key> <getter> <kind> <streamed> differs <adp value> <agent value>`,
//!   or `... adp-rejects <error>`: every derived row that is neither a match nor not compared, with the
//!   leaf tier's columns. `<key>` is the Agent key the derivation stands for and `<kind>` the derived
//!   value's `LeafValue` variant name, or `ByteCount` for a byte count. The derived tier's rows are
//!   counted by verdict on `count derived` lines (`NotCompared` by reason code, as for the leaf tier,
//!   for example `other-getter:GetSizeInBytes` for a `GetString` read of a byte-size key), always
//!   including `match`, `differs`, and `adp-rejects`.
//! - `bootstrap <case> <checkpoint> <key> <getter> <kind> <input> differs <adp value> <agent value>`, or
//!   `... adp-rejects <error>`: every row of the bootstrap tier that is neither a match nor not
//!   compared, with the leaf tier's columns. `<checkpoint>` is always `snapshot`, and `<input>` is the
//!   value the bootstrap base holds at the key's path (JSON), or `-` when it does not hold it. The
//!   tier's rows are counted by verdict on `count bootstrap` lines, always including `match`,
//!   `differs`, `adp-rejects` and `not-modeled`, and so are its cases: `cases-in-scope`,
//!   `cases-out-of-scope` (a fleet policy or a CLI override), and `cases-aborted`.
//! - `bootstrap-aborts <case> <error>`: every case in scope whose bootstrap base does not build, with
//!   the error ADP aborts its boot on; such a case has no `bootstrap` rows.
//! - `derived-n/a <name> <reason>`: every derivation ADP has that the derived tier does not replay.
//! - `translator <case> <step> <key> <error>`: every key whose translation failed at a step.
//! - `system <case> <step> <stage> <error>`: every rejected step except the blank API key, which the
//!   corpus baseline makes every case hit and is counted instead. A translation failure gives its
//!   number of keys here; its errors are on the `translator` lines. `system` and `translator` lines
//!   are identified by step position, so inserting an update into a recorded case renumbers every
//!   later step and drops that case's `system`/`translator` annotations.
//! - `not-modeled <key>`: every distinct key the corpus records that is not a supported leaf.
//! - `uncovered <key>`: every supported leaf no started case compares, by a match or a difference.
//!
//! Steps are numbered from `0000`, the first snapshot. A field longer than [`MAX_FIELD_CHARS`] keeps
//! its first [`KEPT_CHARS`] characters, followed by its full length and its 64-bit FNV-1a hash, so a
//! change anywhere in a long value still changes the line.
//!
//! # Divergence types
//!
//! Every divergence belongs to a declared **divergence type**: one cause, and one place a fix would go, never one
//! key. A type line, `type <name> <layer> <reason>`, declares one. Type lines are written by hand, sit in one block
//! right after the header in name order, and survive a bless verbatim. The name is kebab-case, and the layer is one
//! of:
//!
//! | Layer              | Where the divergence arises                                          | Fixable in ADP |
//! |--------------------|----------------------------------------------------------------------|----------------|
//! | `adp-deserializer` | ADP's serde reading of a value                                       | yes            |
//! | `adp-translator`   | ADP's translation into its typed configuration                       | yes            |
//! | `adp-bootstrap`    | ADP's own reading of `datadog.yaml` and `DD_*`                       | yes            |
//! | `adp-defaults`     | ADP's default values                                                 | yes            |
//! | `adp-derived`      | values ADP computes from other settings                              | yes            |
//! | `agent-load`       | the Agent writes the value while loading; only the stream carries it | no             |
//! | `agent-wire`       | lost on the Agent's protobuf stream                                  | no             |
//! | `comparison`       | how the replay compares, not ADP's behavior                          | no             |
//!
//! Any line may end with a tab and `# <text>`, an annotation the comparison with the computed results ignores, as it
//! ignores the type lines. Every `leaf`, `translator`, `derived`, `bootstrap` and `bootstrap-aborts` line **MUST**
//! carry one that reads `# <type>` or `# <type>: <note>`, where `<type>` is a declared type name. On any other line
//! (`count`, `case`, `startup-failed`, `system`, `derived-n/a`, `not-modeled`, `uncovered` and `type`) an annotation
//! is an optional free-text note. The check fails, listing each offending line, when a line that needs an annotation
//! has none, an annotation names an undeclared type, a declared type is used by no line, a type line is malformed or
//! names an unknown layer, or a type is declared twice.
//!
//! Blessing keeps an annotation only on a line whose content, apart from the annotation, is unchanged: a new or
//! changed line comes out unannotated, and the check then names it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use datadog_agent_config::LEAVES;
use datadog_agent_config_corpus::{Corpus, Getter, Outcome};

use super::bootstrap::corpus_bootstrap;
use super::compare::{Reason, Verdict};
use super::derived::{corpus_derived_rows, NOT_REPLAYED};
use super::driver::{replay_case, Stage};
use super::leaf_replay::{corpus_rows, Checkpoint, RowResult};
use crate::system::Error;

/// The known-results file, next to this module.
const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/corpus_replay/known-results.txt");

/// The environment variable that turns the check into a rewrite of the file.
const BLESS_VAR: &str = "ADP_CORPUS_REPLAY_BLESS";

/// The command that rewrites the file.
const BLESS_COMMAND: &str = "ADP_CORPUS_REPLAY_BLESS=1 cargo nextest run --lib -p agent-data-plane-config-system \
                             corpus_replay_results_equal_the_known_results_file";

/// The longest field written whole, in characters.
const MAX_FIELD_CHARS: usize = 200;

/// How many characters of a longer field are kept.
const KEPT_CHARS: usize = 160;

/// The most added or removed lines a failure lists.
const MAX_LISTED: usize = 50;

/// What separates an annotation from the line it annotates.
const ANNOTATION: &str = "\t# ";

/// The first column of a line that declares a divergence type.
const TYPE_TIER: &str = "type";

/// The layers a divergence type may name, documented in the module's table.
const LAYERS: [&str; 8] = [
    "adp-deserializer",
    "adp-translator",
    "adp-bootstrap",
    "adp-defaults",
    "adp-derived",
    "agent-load",
    "agent-wire",
    "comparison",
];

/// The tiers whose every line must name its divergence type in its annotation.
const TYPED_TIERS: [&str; 5] = ["leaf", "translator", "derived", "bootstrap", "bootstrap-aborts"];

/// The five leaf verdicts a `case` line counts, in column order.
const CASE_VERDICTS: [&str; 5] = ["match", "differs", "adp-rejects", "not-modeled", "not-compared"];

/// Returns the 64-bit FNV-1a hash of `bytes`: fixed by its definition, unlike the standard hasher.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Renders `s` as one field: tabs and line breaks escaped, and long values shortened.
fn field(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    let length = escaped.chars().count();
    if length <= MAX_FIELD_CHARS {
        return escaped;
    }
    let kept: String = escaped.chars().take(KEPT_CHARS).collect();
    format!(
        "{kept}...[{length} chars, fnv1a64 {:016x}]",
        fnv1a64(escaped.as_bytes())
    )
}

fn checkpoint_name(checkpoint: Checkpoint) -> &'static str {
    match checkpoint {
        Checkpoint::Snapshot => "snapshot",
        Checkpoint::Final => "final",
    }
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Deserialize => "deserialize",
        Stage::Translate => "translate",
        Stage::Validate => "validate",
    }
}

fn step_name(position: usize) -> String {
    format!("{position:04}")
}

/// Returns a stable code for a `NotCompared` reason, unlike its `Display` text: rewording the
/// message must not rewrite lines.
fn reason_code(reason: &Reason) -> String {
    match reason {
        Reason::NotEmulated { kind, .. } => format!("not-emulated:{kind:?}"),
        Reason::ExplicitOnly => "explicit-only".to_string(),
        Reason::ResultShape => "result-shape".to_string(),
        Reason::OtherGetter { compared } => format!("other-getter:{compared}"),
    }
}

/// Splits a line into its content and its annotation, if any.
fn split_annotation(line: &str) -> (&str, Option<&str>) {
    match line.rsplit_once(ANNOTATION) {
        Some((content, annotation)) => (content, Some(annotation)),
        None => (line, None),
    }
}

/// Returns whether a line's content is a type line.
fn is_type_line(content: &str) -> bool {
    content.split('\t').next() == Some(TYPE_TIER)
}

/// Returns whether `name` is kebab-case: lowercase ASCII letters and digits, in words joined by single hyphens.
fn is_kebab_case(name: &str) -> bool {
    name.split('-')
        .all(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// Returns the divergence type an annotation names: its text before `: `, or all of it.
fn annotation_type(annotation: &str) -> &str {
    annotation.split_once(": ").map_or(annotation, |(name, _)| name)
}

/// Computes the file's content from the corpus, without annotations.
///
/// # Panics
///
/// Panics on a harness error: a case the leaf tier or the driver cannot replay.
fn known_results(corpus: &Corpus) -> String {
    let rows = corpus_rows(corpus).unwrap_or_else(|errors| panic!("harness errors: {errors:#?}"));
    let mut lines = BTreeSet::new();
    let mut counts: BTreeMap<(&str, String), usize> = BTreeMap::new();
    let mut not_modeled = BTreeSet::new();
    let mut compared = BTreeSet::new();
    // Per case, the counts of its leaf rows, in `CASE_VERDICTS` order.
    let mut case_counts: BTreeMap<&str, [usize; CASE_VERDICTS.len()]> = BTreeMap::new();

    for row in &rows {
        let getter = row.getter.map_or("-", Getter::as_str);
        let streamed = row.streamed.as_ref().map_or("-".to_string(), |v| field(&v.to_string()));
        let leaf_line = |verdict: &str, detail: &[String]| {
            let mut columns = vec![
                "leaf".to_string(),
                field(&row.case),
                checkpoint_name(row.checkpoint).to_string(),
                field(&row.key),
                getter.to_string(),
                row.kind.unwrap_or("-").to_string(),
                streamed.clone(),
                verdict.to_string(),
            ];
            columns.extend_from_slice(detail);
            columns.join("\t")
        };
        let (label, verdict_index) = match &row.result {
            RowResult::Leaf(Verdict::Match) => ("match".to_string(), 0),
            RowResult::Leaf(Verdict::Differs { adp, agent }) => {
                lines.insert(leaf_line("differs", &[field(adp), field(agent)]));
                ("differs".to_string(), 1)
            }
            RowResult::Leaf(Verdict::AdpRejects { error }) => {
                lines.insert(leaf_line("adp-rejects", &[field(error)]));
                ("adp-rejects".to_string(), 2)
            }
            RowResult::Leaf(Verdict::NotCompared { reason }) => (reason_code(reason), 4),
            RowResult::NotModeled => {
                not_modeled.insert(row.key.as_str());
                ("not-modeled".to_string(), 3)
            }
        };
        if let (RowResult::Leaf(Verdict::Match | Verdict::Differs { .. }), Some(leaf)) = (&row.result, row.leaf) {
            compared.insert(leaf);
        }
        *counts.entry(("leaf", label)).or_default() += 1;
        case_counts.entry(row.case.as_str()).or_insert([0; CASE_VERDICTS.len()])[verdict_index] += 1;
    }

    let derived = corpus_derived_rows(corpus).unwrap_or_else(|errors| panic!("harness errors: {errors:#?}"));
    for label in ["match", "differs", "adp-rejects"] {
        counts.insert(("derived", label.to_string()), 0);
    }
    for row in &derived {
        let streamed = row.streamed.as_ref().map_or("-".to_string(), |v| field(&v.to_string()));
        let (label, detail) = match &row.verdict {
            Verdict::Match => ("match".to_string(), None),
            Verdict::Differs { adp, agent } => ("differs".to_string(), Some(vec![field(adp), field(agent)])),
            Verdict::AdpRejects { error } => ("adp-rejects".to_string(), Some(vec![field(error)])),
            Verdict::NotCompared { reason } => (reason_code(reason), None),
        };
        if let Some(detail) = detail {
            let mut columns = vec![
                "derived".to_string(),
                field(&row.case),
                checkpoint_name(row.checkpoint).to_string(),
                field(row.key),
                row.getter.as_str().to_string(),
                row.kind.to_string(),
                streamed,
                label.clone(),
            ];
            columns.extend(detail);
            lines.insert(columns.join("\t"));
        }
        *counts.entry(("derived", label)).or_default() += 1;
    }
    for (name, reason) in NOT_REPLAYED {
        lines.insert(format!("derived-n/a\t{}\t{}", field(name), field(reason)));
    }

    let bootstrap = corpus_bootstrap(corpus);
    for label in ["match", "differs", "adp-rejects", "not-modeled"] {
        counts.insert(("bootstrap", label.to_string()), 0);
    }
    counts.insert(("bootstrap", "cases-in-scope".to_string()), bootstrap.in_scope);
    counts.insert(("bootstrap", "cases-out-of-scope".to_string()), bootstrap.out_of_scope);
    counts.insert(("bootstrap", "cases-aborted".to_string()), bootstrap.aborts.len());
    for row in &bootstrap.rows {
        let detail = match &row.result {
            RowResult::Leaf(Verdict::Differs { adp, agent }) => Some(("differs", vec![field(adp), field(agent)])),
            RowResult::Leaf(Verdict::AdpRejects { error }) => Some(("adp-rejects", vec![field(error)])),
            _ => None,
        };
        if let Some((verdict, detail)) = detail {
            let mut columns = vec![
                "bootstrap".to_string(),
                field(&row.case),
                checkpoint_name(row.checkpoint).to_string(),
                field(&row.key),
                row.getter.map_or("-", Getter::as_str).to_string(),
                row.kind.unwrap_or("-").to_string(),
                row.streamed.as_ref().map_or("-".to_string(), |v| field(&v.to_string())),
                verdict.to_string(),
            ];
            columns.extend(detail);
            lines.insert(columns.join("\t"));
        }
        let label = match &row.result {
            RowResult::Leaf(Verdict::Match) => "match".to_string(),
            RowResult::Leaf(Verdict::Differs { .. }) => "differs".to_string(),
            RowResult::Leaf(Verdict::AdpRejects { .. }) => "adp-rejects".to_string(),
            RowResult::Leaf(Verdict::NotCompared { reason }) => reason_code(reason),
            RowResult::NotModeled => "not-modeled".to_string(),
        };
        *counts.entry(("bootstrap", label)).or_default() += 1;
    }
    for (case, error) in &bootstrap.aborts {
        lines.insert(format!("bootstrap-aborts\t{}\t{}", field(case), field(error)));
    }

    let blank_api_key = Error::MissingApiKey.to_string();
    for case in &corpus.cases {
        if !matches!(case.outcome, Outcome::Started(_)) {
            *counts.entry(("cases", "startup-failed".to_string())).or_default() += 1;
            lines.insert(format!("startup-failed\t{}", field(&case.name)));
            continue;
        }
        *counts.entry(("cases", "started".to_string())).or_default() += 1;
        let verdicts = case_counts
            .get(case.name.as_str())
            .copied()
            .unwrap_or([0; CASE_VERDICTS.len()]);
        let counted: Vec<String> = verdicts.iter().map(ToString::to_string).collect();
        lines.insert(format!("case\t{}\t{}", field(&case.name), counted.join("\t")));
        let replayed = replay_case(corpus, &case.name).unwrap_or_else(|error| panic!("harness error: {error}"));
        let case_name = field(&case.name);
        for (position, step) in replayed.steps.iter().enumerate() {
            let outcome = match &step.failure {
                None => "accepted".to_string(),
                Some(failure) if failure.stage == Stage::Validate && failure.error == blank_api_key => {
                    "validate: blank api key".to_string()
                }
                Some(failure) => {
                    // A translation failure's detail is on its per-key `translator` lines; repeating the
                    // whole error list here would only duplicate them.
                    let error = if failure.translate_errors.is_empty() {
                        field(&failure.error)
                    } else {
                        format!("{} key(s), see the translator lines", failure.translate_errors.len())
                    };
                    let columns = [
                        "system",
                        case_name.as_str(),
                        &step_name(position),
                        stage_name(failure.stage),
                        &error,
                    ];
                    lines.insert(columns.join("\t"));
                    for (key, error) in &failure.translate_errors {
                        let columns = [
                            "translator",
                            case_name.as_str(),
                            &step_name(position),
                            &field(key),
                            &field(error),
                        ];
                        lines.insert(columns.join("\t"));
                    }
                    stage_name(failure.stage).to_string()
                }
            };
            *counts.entry(("system", outcome)).or_default() += 1;
        }
    }

    for ((group, label), n) in &counts {
        lines.insert(format!("count\t{group}\t{label}\t{n}"));
    }
    for key in not_modeled {
        lines.insert(format!("not-modeled\t{}", field(key)));
    }
    for leaf in LEAVES.iter().filter(|leaf| !compared.contains(leaf.key)) {
        lines.insert(format!("uncovered\t{}", field(leaf.key)));
    }

    let mut out = format!(
        "# Known results of replaying the config-recorder corpus through the typed configuration: every\n\
         # result that is not a plain match. Generated by the test\n\
         # `corpus_replay_results_equal_the_known_results_file`; do not edit, except to write the `type` lines\n\
         # and to annotate a line by ending it with a tab and `# <text>`. To regenerate it, run:\n\
         #   {BLESS_COMMAND}\n\
         # Columns are tab-separated; see `known_results.rs` for each tier's columns. `system` and\n\
         # `translator` lines are identified by step position, so inserting an update into a recorded\n\
         # case renumbers its later steps and drops their annotations.\n\
         # A bless leaves each new divergence unannotated: fit it into an existing type or declare a new one.\n"
    );
    for line in lines {
        // An annotation is the text after the last `\t# `, so a computed column that starts with `# ` would read as
        // one.
        assert!(
            !line.contains(ANNOTATION),
            "a known-results line would read as annotated: {line:?}"
        );
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Returns `computed` with the type lines of `existing` after its header, and each annotation of `existing` kept on
/// the line whose content, apart from the annotation, is unchanged.
fn bless(computed: &str, existing: &str) -> String {
    let mut types = Vec::new();
    let mut annotations: HashMap<&str, &str> = HashMap::new();
    for line in existing.lines().filter(|line| !line.starts_with('#')) {
        match split_annotation(line) {
            (content, _) if is_type_line(content) => types.push(line),
            (content, Some(annotation)) => {
                annotations.insert(content, annotation);
            }
            (_, None) => {}
        }
    }
    let mut out = String::new();
    let mut lines = computed.lines().peekable();
    while let Some(line) = lines.next_if(|line| line.starts_with('#')) {
        out.push_str(line);
        out.push('\n');
    }
    for line in types {
        out.push_str(line);
        out.push('\n');
    }
    for line in lines {
        out.push_str(line);
        if let Some(annotation) = annotations.get(line) {
            out.push_str(ANNOTATION);
            out.push_str(annotation);
        }
        out.push('\n');
    }
    out
}

/// Returns a failure message listing each line of `file` that breaks the divergence-type rules of the module doc, or
/// `None` if none does.
fn type_violations(file: &str) -> Option<String> {
    // Each declared type's line number and layer, and each annotated line's number, type, and text.
    let mut declared: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    let mut annotated = Vec::new();
    let mut problems: Vec<(usize, String)> = Vec::new();
    let mut unannotated = 0;
    for (number, line) in (1..).zip(file.lines()) {
        if line.starts_with('#') {
            continue;
        }
        let (content, annotation) = split_annotation(line);
        let columns: Vec<&str> = content.split('\t').collect();
        if columns[0] == TYPE_TIER {
            let [_, name, layer, reason] = columns[..] else {
                problems.push((
                    number,
                    format!("a type line has {} columns, not 4: {line}", columns.len()),
                ));
                continue;
            };
            if !is_kebab_case(name) || reason.is_empty() {
                problems.push((
                    number,
                    format!("a type line needs a kebab-case name and a reason: {line}"),
                ));
            } else if !LAYERS.contains(&layer) {
                let layers = LAYERS.join(", ");
                problems.push((number, format!("unknown layer `{layer}`, not one of {layers}: {line}")));
            } else if let Some((first, _)) = declared.get(name) {
                problems.push((
                    number,
                    format!("type `{name}` is already declared on line {first}: {line}"),
                ));
            } else {
                declared.insert(name, (number, layer));
            }
        } else if TYPED_TIERS.contains(&columns[0]) {
            match annotation {
                Some(annotation) => annotated.push((number, annotation_type(annotation), line)),
                None => {
                    unannotated += 1;
                    problems.push((number, format!("no divergence type: {line}")));
                }
            }
        }
    }
    let used: BTreeSet<&str> = annotated.iter().map(|(_, name, _)| *name).collect();
    for (number, name, line) in &annotated {
        if !declared.contains_key(name) {
            problems.push((*number, format!("undeclared type `{name}`: {line}")));
        }
    }
    for (name, (number, _)) in &declared {
        if !used.contains(name) {
            problems.push((*number, format!("type `{name}` is used by no line")));
        }
    }
    if problems.is_empty() {
        return None;
    }
    problems.sort();
    let mut message = format!("{} line(s) of {FILE} break its divergence types\n", problems.len());
    for (number, problem) in problems.iter().take(MAX_LISTED) {
        message.push_str(&format!("line {number}: {problem}\n"));
    }
    if problems.len() > MAX_LISTED {
        message.push_str(&format!("... and {} more\n", problems.len() - MAX_LISTED));
    }
    if unannotated > 0 {
        message.push_str(
            "end each line with no divergence type with a tab and `# <type>` or `# <type>: <note>`: fit each new \
             divergence into an existing type or declare a new one with a `type` line\n",
        );
    }
    message.push_str("the declared types:\n");
    for (name, (_, layer)) in &declared {
        message.push_str(&format!("  {name} ({layer})\n"));
    }
    Some(message)
}

/// Returns a failure message listing the lines of `computed` that `expected` lacks, and the reverse,
/// or `None` if the two hold the same lines in the same order. Type lines and annotations in `expected` are ignored.
fn mismatch(computed: &str, expected: &str) -> Option<String> {
    let computed: Vec<&str> = computed.lines().collect();
    let expected: Vec<&str> = expected
        .lines()
        .map(|line| split_annotation(line).0)
        .filter(|content| !is_type_line(content))
        .collect();
    if computed == expected {
        return None;
    }
    let (have, want): (BTreeSet<&str>, BTreeSet<&str>) =
        (computed.iter().copied().collect(), expected.iter().copied().collect());
    let added: Vec<&str> = have.difference(&want).copied().collect();
    let removed: Vec<&str> = want.difference(&have).copied().collect();
    let mut message = format!(
        "the corpus replay's results differ from {FILE}: {} line(s) added, {} removed\n",
        added.len(),
        removed.len()
    );
    for (sign, lines) in [("+", &added), ("-", &removed)] {
        for line in lines.iter().take(MAX_LISTED) {
            message.push_str(&format!("{sign} {line}\n"));
        }
        if lines.len() > MAX_LISTED {
            message.push_str(&format!("{sign} ... and {} more\n", lines.len() - MAX_LISTED));
        }
    }
    if added.is_empty() && removed.is_empty() {
        message.push_str("the lines are the same but their order or repetition differs\n");
    }
    message.push_str(&format!(
        "if the change is intended, update the file with:\n  {BLESS_COMMAND}\n"
    ));
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus_replay::corpus;

    /// Fails unless the corpus replay computes exactly the checked-in known results, so both a new
    /// divergence and a fixed one (a listed line that no longer occurs) fail.
    ///
    /// Bless (`ADP_CORPUS_REPLAY_BLESS=1`) is for a person updating the expected results after a corpus
    /// or code change: it rewrites the file, keeping the type lines and the annotations of unchanged lines. The
    /// file's diff is then the review of what changed. Either way, the test then fails unless every divergence names
    /// a declared type, so a bless that adds or changes a divergence fails until its line is annotated.
    #[test]
    fn corpus_replay_results_equal_the_known_results_file() {
        let computed = known_results(corpus());
        let existing = std::fs::read_to_string(FILE).unwrap_or_default();
        let file = if std::env::var(BLESS_VAR).is_ok_and(|v| v == "1") {
            let blessed = bless(&computed, &existing);
            std::fs::write(FILE, &blessed).unwrap_or_else(|e| panic!("writing {FILE}: {e}"));
            blessed
        } else {
            if let Some(message) = mismatch(&computed, &existing) {
                panic!("{message}");
            }
            existing
        };
        if let Some(message) = type_violations(&file) {
            panic!("{message}");
        }
    }

    /// A small known-results file: a header, one type, and one line of each kind the type rules treat differently.
    const SMALL: &str = "# header\n\
                         type\tnull-collection\tadp-deserializer\ta null collection is rejected\n\
                         count\tleaf\tmatch\t3\t# a free note on an exempt line\n\
                         leaf\tc\tsnapshot\tk\tGet\tStr\tnull\tadp-rejects\tinvalid type: null\t# null-collection\n\
                         bootstrap-aborts\tc\tbad env\t# null-collection: with a note\n\
                         uncovered\tk2\n";

    #[test]
    fn type_check_passes_a_typed_file_whose_exempt_lines_carry_free_notes() {
        assert_eq!(type_violations(SMALL), None);
    }

    #[test]
    fn type_check_fails_each_broken_rule_naming_the_line() {
        let cases = [
            (
                "missing annotation",
                SMALL.replace("\t# null-collection: with a note", ""),
                "line 5: no divergence type: bootstrap-aborts\tc\tbad env\n",
            ),
            (
                "undeclared type",
                SMALL.replace("# null-collection: with", "# no-such-type: with"),
                "line 5: undeclared type `no-such-type`: bootstrap-aborts\tc\tbad env\t# no-such-type: with a note\n",
            ),
            (
                "unused type",
                format!("{SMALL}type\tunused-type\tcomparison\tnothing uses it\n"),
                "line 7: type `unused-type` is used by no line\n",
            ),
            (
                "unknown layer",
                SMALL.replace("\tadp-deserializer\t", "\tadp-elsewhere\t"),
                "line 2: unknown layer `adp-elsewhere`, not one of adp-deserializer, adp-translator, adp-bootstrap, \
                 adp-defaults, adp-derived, agent-load, agent-wire, comparison: type\tnull-collection\tadp-elsewhere\t",
            ),
            (
                "duplicate type",
                format!("{SMALL}type\tnull-collection\tcomparison\tagain\n"),
                "line 7: type `null-collection` is already declared on line 2: type\tnull-collection\tcomparison\tagain\n",
            ),
            (
                "malformed type line",
                format!("{SMALL}type\tno-reason\tcomparison\n"),
                "line 7: a type line has 3 columns, not 4: type\tno-reason\tcomparison\n",
            ),
            (
                "type name not kebab-case",
                format!("{SMALL}type\tNot_Kebab\tcomparison\treason\n"),
                "line 7: a type line needs a kebab-case name and a reason: type\tNot_Kebab\tcomparison\treason\n",
            ),
        ];
        for (name, file, expected) in cases {
            let message = type_violations(&file).unwrap_or_else(|| panic!("{name}: the check passed"));
            assert!(message.contains(expected), "{name}: {message}");
        }
    }

    #[test]
    fn type_check_failure_on_a_missing_annotation_asks_for_a_type_and_lists_the_declared_types() {
        let message = type_violations(&SMALL.replace("\t# null-collection: with a note", "")).expect("the check fails");
        assert!(
            message.contains("fit each new divergence into an existing type or declare a new one"),
            "{message}"
        );
        assert!(
            message.ends_with("the declared types:\n  null-collection (adp-deserializer)\n"),
            "{message}"
        );
    }

    #[test]
    fn bless_keeps_an_annotation_on_an_unchanged_line() {
        let existing = "# old header\nleaf\tc\tsnapshot\tk\tGet\tStr\t1\tdiffers\t1\t2\t# wire-number-loss: a note\n";
        let computed = "# new header\nleaf\tc\tsnapshot\tk\tGet\tStr\t1\tdiffers\t1\t2\n";
        assert_eq!(
            bless(computed, existing),
            "# new header\nleaf\tc\tsnapshot\tk\tGet\tStr\t1\tdiffers\t1\t2\t# wire-number-loss: a note\n"
        );
    }

    #[test]
    fn bless_drops_the_annotation_of_a_line_whose_verdict_or_value_changed() {
        let existing = "# header\n\
                        leaf\tc\tsnapshot\tk\tGet\tStr\t1\tadp-rejects\tbad\t# null-collection\n\
                        leaf\tc\tsnapshot\tk2\tGet\tStr\t1\tdiffers\t1\t2\t# wire-number-loss\n";
        let computed = "# header\n\
                        leaf\tc\tsnapshot\tk\tGet\tStr\t1\tdiffers\t1\t2\n\
                        leaf\tc\tsnapshot\tk2\tGet\tStr\t1\tdiffers\t1\t3\n";
        assert_eq!(bless(computed, existing), computed);
    }

    #[test]
    fn bless_keeps_the_type_lines_verbatim_after_the_header() {
        let existing = "# old header\n\
                        type\tb-type\tcomparison\tsecond\t# a note\n\
                        type\ta-type\tagent-wire\tfirst\n\
                        count\tleaf\tmatch\t3\n";
        let computed = "# new header\n# second header line\ncount\tleaf\tmatch\t4\n";
        let blessed = bless(computed, existing);
        assert_eq!(
            blessed,
            "# new header\n# second header line\n\
             type\tb-type\tcomparison\tsecond\t# a note\n\
             type\ta-type\tagent-wire\tfirst\n\
             count\tleaf\tmatch\t4\n"
        );
        assert_eq!(mismatch(computed, &blessed), None);
    }
}
