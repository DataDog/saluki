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
//!   `... adp-rejects <error>`, or `... not-compared <reason code>`: every derived row that is not a
//!   match, with the leaf tier's columns. `<key>` is the Agent key the derivation stands for and
//!   `<kind>` the derived value's `LeafValue` variant name. The derived tier's rows are counted by
//!   verdict on `count derived` lines, always including `match`, `differs`, and `adp-rejects`.
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
//! Any line may end with a tab and `# <text>`, an annotation the check ignores. Blessing keeps an
//! annotation on a line whose identity columns (every column before the verdict detail) are unchanged.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use datadog_agent_config::LEAVES;
use datadog_agent_config_corpus::{Corpus, Getter, Outcome};

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

/// How many leading columns identify a line of the tier named by its first column.
fn identity_columns(tier: &str) -> usize {
    match tier {
        "count" => 3,
        "leaf" | "derived" => 5,
        "translator" => 4,
        "system" => 4,
        "case" | "startup-failed" | "not-modeled" | "uncovered" | "derived-n/a" => 2,
        _ => usize::MAX,
    }
}

/// Returns a stable code for a `NotCompared` reason, unlike its `Display` text: rewording the
/// message must not rewrite lines.
fn reason_code(reason: &Reason) -> String {
    match reason {
        Reason::NotEmulated { kind, .. } => format!("not-emulated:{kind:?}"),
        Reason::ExplicitOnly => "explicit-only".to_string(),
        Reason::ResultShape => "result-shape".to_string(),
    }
}

/// Splits a line into its content and its annotation, if any.
fn split_annotation(line: &str) -> (&str, Option<&str>) {
    match line.split_once(ANNOTATION) {
        Some((content, annotation)) => (content, Some(annotation)),
        None => (line, None),
    }
}

/// Returns the identity columns of a line's content.
fn identity(content: &str) -> String {
    let columns: Vec<&str> = content.split('\t').collect();
    let n = identity_columns(columns[0]).min(columns.len());
    columns[..n].join("\t")
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
            Verdict::NotCompared { reason } => ("not-compared".to_string(), Some(vec![reason_code(reason)])),
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
        let label = match &row.verdict {
            Verdict::NotCompared { reason } => reason_code(reason),
            _ => label,
        };
        *counts.entry(("derived", label)).or_default() += 1;
    }
    for (name, reason) in NOT_REPLAYED {
        lines.insert(format!("derived-n/a\t{}\t{}", field(name), field(reason)));
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
         # `corpus_replay_results_equal_the_known_results_file`; do not edit, except to annotate a line by\n\
         # ending it with a tab and `# <text>`. To regenerate it, run:\n\
         #   {BLESS_COMMAND}\n\
         # Columns are tab-separated; see `known_results.rs` for each tier's columns. `system` and\n\
         # `translator` lines are identified by step position, so inserting an update into a recorded\n\
         # case renumbers its later steps and drops their annotations.\n"
    );
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Returns `computed` with each annotation of `existing` kept on the line whose identity it had.
fn bless(computed: &str, existing: &str) -> String {
    let annotations: HashMap<String, &str> = existing
        .lines()
        .filter_map(|line| match split_annotation(line) {
            (content, Some(annotation)) => Some((identity(content), annotation)),
            (_, None) => None,
        })
        .collect();
    let mut out = String::new();
    for line in computed.lines() {
        out.push_str(line);
        if let Some(annotation) = annotations.get(&identity(line)).filter(|_| !line.starts_with('#')) {
            out.push_str(ANNOTATION);
            out.push_str(annotation);
        }
        out.push('\n');
    }
    out
}

/// Returns a failure message listing the lines of `computed` that `expected` lacks, and the reverse,
/// or `None` if the two hold the same lines in the same order. Annotations in `expected` are ignored.
fn mismatch(computed: &str, expected: &str) -> Option<String> {
    let computed: Vec<&str> = computed.lines().collect();
    let expected: Vec<&str> = expected.lines().map(|line| split_annotation(line).0).collect();
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
    /// or code change: it rewrites the file, keeping annotations, and passes. The file's diff is then
    /// the review of what changed.
    #[test]
    fn corpus_replay_results_equal_the_known_results_file() {
        let computed = known_results(corpus());
        let existing = std::fs::read_to_string(FILE).unwrap_or_default();
        if std::env::var(BLESS_VAR).is_ok_and(|v| v == "1") {
            std::fs::write(FILE, bless(&computed, &existing)).unwrap_or_else(|e| panic!("writing {FILE}: {e}"));
            return;
        }
        if let Some(message) = mismatch(&computed, &existing) {
            panic!("{message}");
        }
    }
}
