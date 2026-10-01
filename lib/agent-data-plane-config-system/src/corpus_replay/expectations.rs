//! Compares every replay result with the hand-edited expectation table in `expected.rs`.
//!
//! The default expectation is that ADP matches the recorded Agent. A check is one comparison,
//! identified by [`CheckKey`]: the case's stable origin (`Case::check_origin`), the tier, the
//! checkpoint (or, for a translation check, the semantic update key and its occurrence), the
//! getter, and the setting key. An [`Expectation`] names one check and the exact ADP result
//! that is expected today. Each entry is used exactly once: a check with no entry fails, an entry
//! with no check (or two entries for one check) fails, and so does an entry whose result changed.
//!
//! A check only exists where a comparison was actually performed. A recorded read whose getter no
//! leaf kind stands for, an explicit-only read, or a read of another getter than the one compared
//! produces no check at all: an expectation that names it is unused, never an improvement. A
//! recorded result whose shape does not match its getter is a corpus fault and always fails.
//!
//! The deserialize, translate and validate tiers check every operation of a replayed stream (an
//! update's setting, or each setting a snapshot carries) at each stage that actually ran, and
//! record successes too: a key that passes gets a check that passes, so an expectation pinning a
//! rejection becomes "this check now passes" the moment the rejection is fixed, while a removed key
//! or a check that never runs again stays "no check has this identity." A stage that did not run
//! because an earlier one failed yields no check and is never inferred to have succeeded. Validation
//! is compared with the contract in `expected_validation`, which is stated from the streamed
//! `api_key`, not read back from the validator. Two results that share one identity are an ambiguous
//! check and fail; none is merged away.
//!
//! A recorded panic or case fault abandons the checks its case had left, so an unused expectation for
//! a key that case holds is not reported (the panic or fault itself always fails).
//!
//! Two kinds of entry exist:
//!
//! - [`Status::KnownBug`]: ADP is wrong today; the desired result is the Agent's. The entry pins
//!   today's result, and fails with "remove this check's known-bug expectation" once ADP matches.
//! - [`Status::Intentional`]: ADP deliberately differs, or the difference cannot be removed by ADP
//!   (the Agent's wire format loses a number, the Agent writes a value only at startup, the raw
//!   `Get` of a JSON environment variable returns the raw string). The entry pins the exact
//!   result and the reason.
//!
//! A panic in a production call is not an expected result: it always fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::panic::Location;

use datadog_agent_config::{Leaf, LEAVES};
use datadog_agent_config_corpus::{Case, Corpus, Getter, GetterResult, GoFloat, GoValue, Outcome};
use saluki_config::dynamic::ConfigUpdate;
use serde_json::Value as JsonValue;

use super::bootstrap::corpus_bootstrap;
use super::compare::{Reason, Verdict};
use super::derived::corpus_derived_rows;
use super::driver::{case_updates, replay_case, Commit, Stage, StepRecord, Validation};
use super::leaf_replay::{corpus_rows, fold_case, lookup, Checkpoint, Fault, Isolator, RowResult};
use super::Panicked;

/// The directory of the handwritten case files, relative to the repository root.
const CASES_DIR: &str = "lib/datadog-agent/config-recorder/cases";

/// The generated corpus, relative to the repository root.
const CORPUS_FILE: &str = "lib/datadog-agent/config-recorder/corpus.jsonl";

/// Which replay produced a check.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Tier {
    /// A streamed setting deserialized in isolation, against a recorded getter.
    Leaf,
    /// A value ADP computes from several settings, against a recorded getter.
    Derived,
    /// What ADP's own `datadog.yaml` and environment reader reads, against a recorded getter.
    Bootstrap,
    /// Whether the whole configuration still deserializes after a streamed update, by operation:
    /// the update's setting, or each setting a snapshot carries.
    Deserialize,
    /// Whether a streamed update translates: each modeled setting it carries, against the Agent's
    /// acceptance of the same update.
    Translate,
    /// The typed result of validating what an update translated to, by operation. The contract is
    /// stated in [`expected_validation`], not read back from the validator.
    Validate,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::Leaf => "leaf",
            Tier::Derived => "derived",
            Tier::Bootstrap => "bootstrap",
            Tier::Deserialize => "deserialize",
            Tier::Translate => "translate",
            Tier::Validate => "validate",
        })
    }
}

impl Tier {
    /// Tiers that one replay produces together: a panic abandons the rest of all of them.
    fn family(self) -> u8 {
        match self {
            Tier::Leaf => 0,
            Tier::Derived => 1,
            Tier::Bootstrap => 2,
            Tier::Deserialize | Tier::Translate | Tier::Validate => 3,
        }
    }
}

/// The identity of one check.
///
/// `at` is `snapshot`, `final`, or `update <key> #<n>` (the nth update of that key in the case).
/// `getter` is the recorded getter, or `-` for a translation check.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct CheckKey {
    pub(crate) origin: String,
    pub(crate) tier: Tier,
    pub(crate) at: String,
    pub(crate) getter: String,
    pub(crate) key: String,
}

impl fmt::Display for CheckKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "origin {} / {} / {} / getter {} / key {}",
            self.origin, self.tier, self.at, self.getter, self.key
        )
    }
}

/// What ADP produced for a check whose result is not a match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Adp {
    /// ADP computed this value, rendered exactly.
    Value(String),
    /// ADP returned exactly this error text.
    Error(String),
    /// A production call panicked. Cannot be an expected result.
    Panic(String),
    /// The recorded result does not have its getter's shape, so no comparison is possible. A corpus
    /// fault that cannot be an expected result.
    Corpus(String),
}

impl Adp {
    /// ADP computed this value, rendered exactly.
    pub(crate) fn value(v: impl Into<String>) -> Adp {
        Adp::Value(v.into())
    }

    /// ADP returned exactly this error text.
    pub(crate) fn error(e: impl Into<String>) -> Adp {
        Adp::Error(e.into())
    }
}

impl fmt::Display for Adp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Adp::Value(v) => write!(f, "value {v}"),
            Adp::Error(e) => write!(f, "error {e:?}"),
            Adp::Panic(p) => write!(f, "PANIC {p}"),
            Adp::Corpus(c) => write!(f, "malformed recorded result: {c}"),
        }
    }
}

/// Why an expectation exists.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cause {
    pub(crate) name: &'static str,
    pub(crate) why: &'static str,
}

/// How an expectation treats a difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    /// ADP is wrong today; the desired result is the Agent's.
    KnownBug,
    /// ADP deliberately differs, or cannot do otherwise.
    Intentional,
}

/// The identity and expected result of one table entry, as `expected.rs` writes it.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The stable origin of the cases the check comes from.
    pub(crate) origin: &'static str,
    pub(crate) tier: Tier,
    /// `snapshot`, `final`, or `update <key> #<n>`.
    pub(crate) at: &'static str,
    /// The recorded getter, or `-` for a translation check.
    pub(crate) getter: &'static str,
    /// The setting key the check is about.
    pub(crate) key: &'static str,
    /// The exact ADP result expected today.
    pub(crate) adp: Adp,
    pub(crate) cause: Cause,
}

/// One hand-edited expectation: the exact ADP result for one check.
#[derive(Debug)]
pub(crate) struct Expectation {
    pub(crate) check: CheckKey,
    pub(crate) status: Status,
    pub(crate) adp: Adp,
    pub(crate) cause: Cause,
    /// Where the entry is written, so a failure points at the line to edit.
    pub(crate) at: &'static Location<'static>,
}

/// Builds a known-bug expectation: ADP is wrong today, and the desired result is the Agent's.
#[track_caller]
pub(crate) fn known_bug(entry: Entry) -> Expectation {
    expectation(Status::KnownBug, entry)
}

/// Builds an intentional expectation: ADP deliberately differs, or cannot do otherwise.
#[track_caller]
pub(crate) fn intentional(entry: Entry) -> Expectation {
    expectation(Status::Intentional, entry)
}

#[track_caller]
fn expectation(
    status: Status,
    Entry {
        origin,
        tier,
        at,
        getter,
        key,
        adp,
        cause,
    }: Entry,
) -> Expectation {
    Expectation {
        check: CheckKey {
            origin: origin.into(),
            tier,
            at: at.into(),
            getter: getter.into(),
            key: key.into(),
        },
        status,
        adp,
        cause,
        at: Location::caller(),
    }
}

/// The result of a check that did not match the Agent.
#[derive(Debug)]
pub(crate) struct Mismatch {
    pub(crate) adp: Adp,
}

/// One check: its identity, where it came from, and what both sides produced.
#[derive(Debug)]
pub(crate) struct CheckResult {
    pub(crate) key: CheckKey,
    pub(crate) case: String,
    /// The streamed value the check saw, rendered.
    pub(crate) streamed: String,
    /// What the Agent recorded for this check, rendered. The desired result.
    pub(crate) agent: String,
    /// `None` when ADP matched the Agent (or translated successfully, for the translate tier).
    pub(crate) mismatch: Option<Mismatch>,
}

/// Every check of the corpus, and every case-level fault that stopped a case from being replayed.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub(crate) results: Vec<CheckResult>,
    /// Faults that abandoned a case or tier state (a stream that cannot be built, a panic in a
    /// fold), as `case: text`. The remaining cases are still replayed.
    pub(crate) faults: Vec<String>,
}

/// Whether a recorded read was deliberately never compared: no leaf kind stands for its getter, it
/// is an explicit-only read, or another getter than the compared one. Such a row is no check at
/// all, so an expectation naming it is unused.
fn skipped(verdict: &Verdict) -> bool {
    matches!(
        verdict,
        Verdict::NotCompared {
            reason: Reason::NotEmulated { .. } | Reason::ExplicitOnly | Reason::OtherGetter { .. },
        }
    )
}

/// A performed comparison as a check outcome, with the Agent's rendered result as the desired one.
fn mismatch_of(verdict: &Verdict) -> Option<Mismatch> {
    match verdict {
        Verdict::Match => None,
        Verdict::Differs { adp, .. } => Some(Mismatch {
            adp: Adp::Value(adp.clone()),
        }),
        Verdict::AdpRejects { error } => Some(Mismatch {
            adp: Adp::Error(error.clone()),
        }),
        Verdict::Panicked { operation, message } => Some(Mismatch {
            adp: Adp::Panic(format!("{operation}: {message}")),
        }),
        // The corpus reader rules this shape out; seeing it means the corpus is broken.
        Verdict::NotCompared {
            reason: Reason::ResultShape,
        } => Some(Mismatch {
            adp: Adp::Corpus("the result does not have its getter's shape".into()),
        }),
        Verdict::NotCompared { .. } => None,
    }
}

fn at_name(checkpoint: Checkpoint) -> &'static str {
    match checkpoint {
        Checkpoint::Snapshot => "snapshot",
        Checkpoint::Final => "final",
    }
}

/// Renders what the Agent recorded for `getter` of `key` at `checkpoint`, exactly and readably:
/// the same text the comparison rules render the Agent's side with, so a failure shows two
/// comparable results instead of a `Debug` dump of the corpus type.
fn recorded_agent(case: &Case, checkpoint: Checkpoint, key: &str, getter: Getter) -> String {
    let Outcome::Started(started) = &case.outcome else {
        return "<startup failed>".into();
    };
    started
        .keys
        .iter()
        .filter(|line| line.key == key)
        .filter_map(|line| match checkpoint {
            Checkpoint::Snapshot => Some(&line.reads.snapshot),
            Checkpoint::Final => line.reads.final_.as_ref(),
        })
        .flat_map(|read| &read.getters)
        .find(|read| read.getter == getter)
        .map_or("<no such read>".into(), |read| render_result(&read.result))
}

/// Renders a recorded getter result in the comparison's own terms.
fn render_result(result: &GetterResult) -> String {
    match result {
        GetterResult::Bool(b) | GetterResult::IsConfigured(b) => b.to_string(),
        GetterResult::String(s) => format!("{s:?}"),
        GetterResult::Int(n) => n.value.to_string(),
        GetterResult::Int32(n) => n.value.to_string(),
        GetterResult::SizeInBytes(n) => n.value.to_string(),
        GetterResult::Duration(n) => format!("{}ns", n.value),
        GetterResult::Float64(f) => render_go_float(f),
        GetterResult::Float64Slice(None) => "null".to_string(),
        GetterResult::Float64Slice(Some(fs)) => {
            format!("[{}]", fs.iter().map(render_go_float).collect::<Vec<_>>().join(", "))
        }
        GetterResult::StringSlice(None) => "null".to_string(),
        GetterResult::StringSlice(Some(v)) => format!("{v:?}"),
        GetterResult::StringMap(None) => "null".to_string(),
        GetterResult::StringMap(Some(m)) => render_go_map(m),
        GetterResult::StringMapString(None) => "null".to_string(),
        GetterResult::StringMapString(Some(m)) => format!("{m:?}"),
        GetterResult::StringMapStringSlice(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k:?}: {}", render_option(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        GetterResult::Get(v) => render_go(v),
        GetterResult::Section(m) => render_go_map(m),
    }
}

/// Renders `None` as the Go `nil` reading and a slice as its `Debug`, as the comparison does.
fn render_option(v: &Option<Vec<String>>) -> String {
    match v {
        None => "null".to_string(),
        Some(v) => format!("{v:?}"),
    }
}

/// Renders a Go float by its exact corpus token.
fn render_go_float(f: &GoFloat) -> String {
    match f {
        GoFloat::Finite(n) => n.token.clone(),
        GoFloat::NaN => "NaN".to_string(),
        GoFloat::PosInf => "+Inf".to_string(),
        GoFloat::NegInf => "-Inf".to_string(),
    }
}

/// Renders a Go value in JSON form, numbers by their exact corpus token.
fn render_go(v: &GoValue) -> String {
    match v {
        GoValue::Null => "null".to_string(),
        GoValue::Bool(b) => b.to_string(),
        GoValue::Int(n) => n.token.clone(),
        GoValue::Float(f) => render_go_float(f),
        GoValue::String(s) => JsonValue::String(s.clone()).to_string(),
        GoValue::List(items) => format!("[{}]", items.iter().map(render_go).collect::<Vec<_>>().join(",")),
        GoValue::Map(m) => render_go_map(m),
    }
}

fn render_go_map(m: &BTreeMap<String, GoValue>) -> String {
    format!(
        "{{{}}}",
        m.iter()
            .map(|(k, v)| format!("{}:{}", JsonValue::String(k.clone()), render_go(v)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Every key a supported leaf can be streamed under: its dotted key and each alias, as a sibling
/// of the key's last segment.
fn leaf_keys(leaf: &Leaf) -> impl Iterator<Item = String> + '_ {
    let parent = leaf.key.rsplit_once('.').map(|(parent, _)| parent);
    std::iter::once(leaf.key.to_string()).chain(leaf.aliases.iter().map(move |alias| match parent {
        Some(parent) => format!("{parent}.{alias}"),
        None => (*alias).to_string(),
    }))
}

/// Runs every tier over the corpus and returns one [`CheckResult`] per performed comparison, plus
/// every successful translation of the translate tier.
pub(crate) fn collect(corpus: &Corpus) -> Collected {
    let mut out = Collected::default();
    let case_of = |name: &str| corpus.case(name).expect("replay rows name corpus cases");
    let streamed = |v: &Option<serde_json::Value>| v.as_ref().map_or("<absent>".to_string(), ToString::to_string);

    let (rows, faults) = corpus_rows(corpus);
    out.faults.extend(faults);
    for row in rows {
        let (RowResult::Leaf(verdict), Some(getter)) = (&row.result, row.getter) else {
            continue;
        };
        if skipped(verdict) {
            continue;
        }
        let case = case_of(&row.case);
        out.results.push(CheckResult {
            key: CheckKey {
                origin: case.check_origin().into(),
                tier: Tier::Leaf,
                at: at_name(row.checkpoint).into(),
                getter: getter.to_string(),
                key: row.key.clone(),
            },
            case: row.case.clone(),
            streamed: streamed(&row.streamed),
            agent: recorded_agent(case, row.checkpoint, &row.key, getter),
            mismatch: mismatch_of(verdict),
        });
    }

    let (rows, faults) = corpus_derived_rows(corpus);
    out.faults.extend(faults);
    for row in rows {
        if skipped(&row.verdict) {
            continue;
        }
        let case = case_of(&row.case);
        out.results.push(CheckResult {
            key: CheckKey {
                origin: case.check_origin().into(),
                tier: Tier::Derived,
                at: at_name(row.checkpoint).into(),
                getter: row.getter.to_string(),
                key: row.key.into(),
            },
            case: row.case.clone(),
            streamed: streamed(&row.streamed),
            agent: recorded_agent(case, row.checkpoint, row.key, row.getter),
            mismatch: mismatch_of(&row.verdict),
        });
    }

    let bootstrap = corpus_bootstrap(corpus);
    for row in bootstrap.rows {
        let (RowResult::Leaf(verdict), Some(getter)) = (&row.result, row.getter) else {
            continue;
        };
        if skipped(verdict) {
            continue;
        }
        let case = case_of(&row.case);
        out.results.push(CheckResult {
            key: CheckKey {
                origin: case.check_origin().into(),
                tier: Tier::Bootstrap,
                at: at_name(row.checkpoint).into(),
                getter: getter.to_string(),
                key: row.key.clone(),
            },
            case: row.case.clone(),
            streamed: streamed(&row.streamed),
            agent: recorded_agent(case, row.checkpoint, &row.key, getter),
            mismatch: mismatch_of(verdict),
        });
    }
    // A case whose bootstrap base cannot be built aborts ADP's boot; the Agent started.
    for (name, fault) in bootstrap.aborts {
        let case = case_of(&name);
        let adp = match fault {
            Fault::Error(error) => Adp::Error(error),
            Fault::Panic(Panicked { operation, message }) => Adp::Panic(format!("{operation}: {message}")),
        };
        out.results.push(CheckResult {
            key: CheckKey {
                origin: case.check_origin().into(),
                tier: Tier::Bootstrap,
                at: "snapshot".into(),
                getter: "-".into(),
                key: "<boot>".into(),
            },
            case: name,
            streamed: "<the case's YAML and environment>".into(),
            agent: "the Agent started".into(),
            mismatch: Some(Mismatch { adp }),
        });
    }

    let canon = leaf_key_canons();
    for case in &corpus.cases {
        if matches!(case.outcome, Outcome::Started(_)) {
            collect_translation(corpus, case, &canon, &mut out);
        }
    }
    out
}

/// Maps every key a supported leaf can be streamed under to the leaf's primary key, so a check
/// keeps one identity whether the case streamed the key itself or one of its aliases.
fn leaf_key_canons() -> BTreeMap<String, &'static str> {
    let mut canon = BTreeMap::new();
    // Primary keys first, so an alias spelled like another leaf's key never takes that key over.
    for leaf in LEAVES {
        canon.insert(leaf.key.to_string(), leaf.key);
    }
    for leaf in LEAVES {
        for path in leaf_keys(leaf).skip(1) {
            canon.entry(path).or_insert(leaf.key);
        }
    }
    canon
}

/// The validation a step must get, stated from the streamed `api_key` alone and not from the
/// validator: a missing, null or blank (whitespace-only) string key is the typed
/// [`Validation::MissingApiKey`]; any other key is accepted. Validation has no other rule, so no other
/// result is expected.
fn expected_validation(api_key: Option<&JsonValue>) -> Validation {
    let blank = match api_key {
        None | Some(JsonValue::Null) => true,
        Some(JsonValue::String(key)) => key.trim().is_empty(),
        Some(_) => false,
    };
    if blank {
        Validation::MissingApiKey
    } else {
        Validation::Valid
    }
}

fn describe_validation(validation: &Validation) -> String {
    match validation {
        Validation::Valid => "valid".into(),
        Validation::MissingApiKey => "MissingApiKey".into(),
        Validation::Rejected(error) => format!("rejected with {error:?}"),
    }
}

/// One check a replayed step yields, before it is tied to a case and an origin.
#[derive(Debug, PartialEq)]
struct StepCheck {
    tier: Tier,
    /// The operation's setting, or a `<marker>` for a step-level observation.
    key: String,
    /// What the desired result is, rendered.
    agent: String,
    /// `None` when the stage did what it should for this operation.
    adp: Option<Adp>,
}

const ACCEPTED: &str = "the Agent accepted the update";

/// Turns one replayed step into its checks, one per stage per operation.
///
/// An operation is a setting the step is about: an update's own setting, or each setting of a
/// snapshot the case itself exercises. Stages run in order and a later stage yields checks only if
/// it actually ran, so a stage that did not run is not inferred to have succeeded:
///
/// - deserialization: a failed update is one failing check on its setting carrying the exact
///   whole-configuration error. A failed snapshot has no single setting, so each operation reports
///   what `isolate` says about it, and the whole error must be one of those or it gets its own
///   `<whole tree>` check that nothing accepts silently;
/// - translation: each key the translator rejected fails with its errors, and every other
///   operation of a step whose translation ran passes, so fixing one of several errors is seen;
/// - validation: the typed result against [`expected_validation`].
fn step_checks(
    step: &StepRecord, is_snapshot: bool, operations: &[String], supported: &BTreeSet<&str>,
    canon: &BTreeMap<String, &'static str>, isolate: &mut dyn FnMut(&str) -> Option<Adp>,
) -> Vec<StepCheck> {
    let mut checks = Vec::new();
    let mut add = |tier: Tier, key: &str, agent: &str, adp: Option<Adp>| {
        checks.push(StepCheck {
            tier,
            key: key.into(),
            agent: agent.into(),
            adp,
        });
    };
    let failure = step.failure.as_ref();
    match failure.map(|f| f.stage) {
        Some(Stage::Panic) => {
            let error = &failure.expect("a panic is a failure").error;
            add(Tier::Translate, "<panic>", ACCEPTED, Some(Adp::Panic(error.clone())));
            return checks;
        }
        Some(Stage::Deserialize) => {
            let whole = &failure.expect("a deserialization failure is a failure").error;
            let mut explained = !is_snapshot;
            for operation in operations {
                let adp = if is_snapshot {
                    isolate(operation)
                } else {
                    Some(Adp::Error(whole.clone()))
                };
                explained |= adp == Some(Adp::Error(whole.clone()));
                add(Tier::Deserialize, operation, ACCEPTED, adp);
            }
            if !explained {
                add(
                    Tier::Deserialize,
                    "<whole tree>",
                    ACCEPTED,
                    Some(Adp::Error(whole.clone())),
                );
            }
            // Nothing after deserialization ran.
            return checks;
        }
        _ => {}
    }
    for operation in operations {
        add(Tier::Deserialize, operation, ACCEPTED, None);
    }

    let mut rejected: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    if let Some(failure) = failure.filter(|f| f.stage == Stage::Translate) {
        // One check per key: several errors on one key are joined, in recorded order.
        for (key, error) in &failure.translate_errors {
            let key = canon.get(key).copied().unwrap_or(key);
            rejected.entry(key).or_default().push(error);
        }
    }
    for (key, errors) in &rejected {
        add(Tier::Translate, key, ACCEPTED, Some(Adp::Error(errors.join("\n"))));
    }
    for operation in operations.iter().filter(|o| supported.contains(o.as_str())) {
        if !rejected.contains_key(operation.as_str()) {
            add(Tier::Translate, operation, ACCEPTED, None);
        }
    }

    if let Some(actual) = &step.validation {
        let expected = expected_validation(step.api_key.as_ref());
        let agent = format!(
            "validation returns {} (the streamed api_key decides it)",
            describe_validation(&expected)
        );
        for operation in operations {
            let adp = (*actual != expected)
                .then(|| Adp::Error(format!("validation returned {}", describe_validation(actual))));
            add(Tier::Validate, operation, &agent, adp);
        }
    }
    checks
}

/// Replays `case` and turns each step of its stream into checks, see [`step_checks`].
///
/// Cases of one check origin stream the same baseline snapshot, so a snapshot's operations are only
/// the keys the case itself records in its first snapshot: the baseline keys every case streams
/// belong to the baseline case's own checks. Batched cases therefore never share a check identity,
/// and when two results do, the judge fails them as ambiguous instead of merging them.
fn collect_translation(corpus: &Corpus, case: &Case, canon: &BTreeMap<String, &'static str>, out: &mut Collected) {
    let own: BTreeSet<&str> = match &case.outcome {
        Outcome::Started(started) => started
            .keys
            .iter()
            .filter(|line| line.snapshot.is_some())
            .map(|line| line.key.as_str())
            .collect(),
        Outcome::StartupError(_) => return,
    };
    let steps = match replay_case(corpus, &case.name, Commit::Translated) {
        Ok(steps) => steps,
        Err(error) => {
            out.faults.push(format!("case {:?}: {error}", case.name));
            return;
        }
    };
    let updates = match case_updates(corpus, &case.name) {
        Ok(updates) => updates,
        Err(error) => {
            out.faults.push(error);
            return;
        }
    };
    let snapshot_tree = fold_case(corpus, &case.name).ok().map(|f| f.snapshot);
    let supported: BTreeSet<&str> = canon.values().copied().collect();
    let mut isolator = Isolator::new();
    for (step, update) in steps.iter().zip(&updates) {
        let at = match &step.key {
            None => "snapshot".to_string(),
            Some(key) => format!("update {key} #{}", step.occurrence),
        };
        // What the step's stream held for a key: the update's value, or the snapshot's value for it.
        let streamed_of = |key: &str| match update {
            ConfigUpdate::Partial(setting) => setting.value.to_string(),
            ConfigUpdate::Snapshot(_) => snapshot_tree
                .as_ref()
                .and_then(|tree| lookup(tree, key))
                .map_or("<absent>".to_string(), ToString::to_string),
        };
        // Record each operation under its key's primary spelling, so the check keeps one identity
        // whether the case streamed the key itself or one of its aliases.
        let primary = |key: &str| canon.get(key).map_or(key.to_string(), |k| (*k).to_string());
        let operations: BTreeSet<String> = match update {
            ConfigUpdate::Snapshot(settings) => settings
                .iter()
                .map(|s| s.key.as_str())
                .filter(|k| own.contains(k))
                .map(primary)
                .collect(),
            ConfigUpdate::Partial(setting) => std::iter::once(primary(&setting.key)).collect(),
        };
        let operations: Vec<String> = operations.into_iter().collect();
        let mut isolate = |key: &str| {
            let tree = snapshot_tree.as_ref()?;
            let index = isolator.leaf_index(key)?;
            match &*isolator.isolate(index, tree) {
                Ok(_) => None,
                Err(Fault::Error(error)) => Some(Adp::Error(error.clone())),
                Err(Fault::Panic(Panicked { operation, message })) => {
                    Some(Adp::Panic(format!("{operation}: {message}")))
                }
            }
        };
        let is_snapshot = matches!(update, ConfigUpdate::Snapshot(_));
        for check in step_checks(step, is_snapshot, &operations, &supported, canon, &mut isolate) {
            out.results.push(CheckResult {
                streamed: streamed_of(&check.key),
                key: CheckKey {
                    origin: case.check_origin().into(),
                    tier: check.tier,
                    at: at.clone(),
                    getter: "-".into(),
                    key: check.key,
                },
                case: case.name.clone(),
                agent: check.agent,
                mismatch: check.adp.map(|adp| Mismatch { adp }),
            });
        }
    }
}

/// Where a case's inputs are written: the handwritten YAML line of `name:`, or the corpus input line.
fn input_location(case: &Case) -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let file = format!("{CASES_DIR}/{}.yaml", case.name);
    match std::fs::read_to_string(format!("{root}/{file}")) {
        Ok(text) => {
            let line = text.lines().position(|l| l.starts_with("name:")).map_or(1, |n| n + 1);
            format!("{file}:{line}")
        }
        Err(_) => format!("{CORPUS_FILE}:{}", case.input_line),
    }
}

/// Renders the original inputs of a case that can produce a check.
fn original_input(case: &Case) -> String {
    let i = &case.inputs;
    let mut out = String::new();
    if let Some(yaml) = &i.yaml {
        let _ = write!(out, "yaml: {yaml:?}; ");
    }
    if let Some(env) = &i.env {
        let _ = write!(out, "env: {env:?}; ");
    }
    if let Some(fleet) = &i.fleet_policy {
        let _ = write!(out, "fleet policy: {fleet:?}; ");
    }
    if !i.cli.is_empty() {
        let _ = write!(out, "cli: {:?}; ", i.cli);
    }
    if !i.updates.is_empty() {
        let _ = write!(out, "updates: {:?}; ", i.updates);
    }
    if out.is_empty() {
        out.push_str("<none: the baseline defaults>");
    }
    out
}

/// The case a fault text names, if its name can be read back out of the text.
fn fault_case<'a>(corpus: &'a Corpus, fault: &str) -> Option<&'a Case> {
    let name = fault.strip_prefix("case \"")?;
    corpus.case(name.split('"').next()?)
}

/// Whether `case` records `key` (under any spelling) or `key` is a `<marker>` that any of its steps can
/// yield.
fn case_holds_key(case: &Case, key: &str, canon: &BTreeMap<String, &'static str>) -> bool {
    let Outcome::Started(started) = &case.outcome else {
        return false;
    };
    key.starts_with('<')
        || started
            .keys
            .iter()
            .any(|line| line.key == key || canon.get(&line.key).is_some_and(|primary| *primary == key))
}

/// The problems found, in a stable order, with the number of failed checks and distinct cases.
#[derive(Debug, Default)]
pub(crate) struct Judgement {
    pub(crate) problems: Vec<String>,
    pub(crate) failed_checks: usize,
    pub(crate) failed_cases: BTreeSet<String>,
}

impl Judgement {
    /// The failure message, or `None` if every check met its expectation.
    pub(crate) fn message(&self) -> Option<String> {
        if self.problems.is_empty() {
            return None;
        }
        Some(format!(
            "{} problem(s): {} failed check(s) in {} case(s)\n\n{}",
            self.problems.len(),
            self.failed_checks,
            self.failed_cases.len(),
            self.problems.join("\n\n")
        ))
    }
}

/// One problem in the making: a case fault sorts first, everything else by its check identity.
struct Pending {
    case_fault: bool,
    key: CheckKey,
    text: String,
}

/// Accumulates the problems of one judging run.
struct Acc {
    pending: Vec<Pending>,
    failed_checks: usize,
    failed_cases: BTreeSet<String>,
}

impl Acc {
    /// Records one problem. `cases` are the cases it happened in; a case fault is not a failed
    /// check, everything else is.
    fn problem(&mut self, case_fault: bool, key: CheckKey, text: String, cases: impl IntoIterator<Item = String>) {
        if !case_fault {
            self.failed_checks += 1;
        }
        self.failed_cases.extend(cases);
        self.pending.push(Pending { case_fault, key, text });
    }

    fn finish(mut self) -> Judgement {
        // Case faults first, each by their case; every check problem by its full check identity.
        self.pending
            .sort_by(|a, b| b.case_fault.cmp(&a.case_fault).then_with(|| a.key.cmp(&b.key)));
        Judgement {
            problems: self.pending.into_iter().map(|p| p.text).collect(),
            failed_checks: self.failed_checks,
            failed_cases: self.failed_cases,
        }
    }
}

/// The context every check failure shows first: the case, its input, the check's identity, and the
/// streamed value it compared.
fn check_context(result: &CheckResult, case: &Case) -> String {
    format!(
        "Case: {}\nInput file: {}\nCheck: {}\nInput: {}\nStreamed value: {}",
        result.case,
        input_location(case),
        result.key,
        original_input(case),
        result.streamed
    )
}

/// The cases of an origin with their input locations, or why there are none.
fn origin_summary(cases_of_origin: &BTreeMap<&str, Vec<&Case>>, origin: &str) -> String {
    cases_of_origin.get(origin).map_or_else(
        || "no corpus case has this origin".to_string(),
        |cases| {
            cases
                .iter()
                .map(|c| format!("{} ({})", c.name, input_location(c)))
                .collect::<Vec<_>>()
                .join(", ")
        },
    )
}

/// Judges `collected` against `table`.
#[track_caller]
pub(crate) fn judge(corpus: &Corpus, collected: &Collected, table: &[Expectation]) -> Judgement {
    let assertion = Location::caller();
    let mut acc = Acc {
        pending: Vec::new(),
        failed_checks: 0,
        failed_cases: BTreeSet::new(),
    };
    let mut cases_of_origin: BTreeMap<&str, Vec<&Case>> = BTreeMap::new();
    for case in &corpus.cases {
        cases_of_origin.entry(case.check_origin()).or_default().push(case);
    }
    let cases_of = |origin: &str| -> Vec<String> {
        cases_of_origin
            .get(origin)
            .map(|cases| cases.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    };

    for fault in &collected.faults {
        match fault_case(corpus, fault) {
            Some(case) => {
                let key = CheckKey {
                    origin: case.name.clone(),
                    tier: Tier::Leaf,
                    at: String::new(),
                    getter: String::new(),
                    key: String::new(),
                };
                acc.problem(
                    true,
                    key,
                    format!(
                        "CASE ABANDONED (the remaining cases still ran): {fault}\n\nCase: {}\nInput file: {}\nInput: {}",
                        case.name,
                        input_location(case),
                        original_input(case)
                    ),
                    [case.name.clone()],
                );
            }
            None => {
                let key = CheckKey {
                    origin: fault.clone(),
                    tier: Tier::Leaf,
                    at: String::new(),
                    getter: String::new(),
                    key: String::new(),
                };
                acc.problem(true, key, format!("CASE ABANDONED: {fault}"), Vec::new())
            }
        }
    }

    // Index the table; a check with two entries, or an entry for an unknown origin, is a problem.
    let mut by_check: BTreeMap<&CheckKey, Vec<&Expectation>> = BTreeMap::new();
    for e in table {
        by_check.entry(&e.check).or_default().push(e);
    }
    for (check, entries) in &by_check {
        if entries.len() > 1 {
            let places: Vec<String> = entries.iter().map(|e| e.at.to_string()).collect();
            acc.problem(
                false,
                (*check).clone(),
                format!(
                    "DUPLICATE EXPECTATION: this check is written {} times.\n\nCheck: {}\nCases: {}\nWritten at: {}\n\nKeep exactly one entry; a check is pinned once.",
                    entries.len(),
                    check,
                    origin_summary(&cases_of_origin, &check.origin),
                    places.join(" and ")
                ),
                cases_of(&check.origin),
            );
        }
        if !cases_of_origin.contains_key(check.origin.as_str()) {
            acc.problem(
                false,
                (*check).clone(),
                format!(
                    "UNKNOWN ORIGIN: no corpus case has this check origin.\n\nCheck: {}\nExpected ADP: {}\nExpectation: {}\n\nThe expectation names cases that no longer exist; remove it or fix its origin.",
                    check,
                    entries[0].adp,
                    entries[0].at
                ),
                Vec::new(),
            );
        }
    }

    let mut seen: BTreeMap<&CheckKey, &CheckResult> = BTreeMap::new();
    for result in &collected.results {
        if let Some(first) = seen.insert(&result.key, result) {
            acc.problem(
                false,
                result.key.clone(),
                format!(
                    "AMBIGUOUS CHECK: two replay results share one identity.\n\nCases: {} and {}\nCheck: {}\n\nA check must have one identity; the judge cannot tell which result an expectation applies to.",
                    first.case, result.case, result.key
                ),
                [first.case.clone(), result.case.clone()],
            );
            continue;
        }
        let case = corpus.case(&result.case).expect("results name corpus cases");
        let expected = by_check.get(&result.key).and_then(|entries| entries.first().copied());
        let context = check_context(result, case);
        let recorded = if result.key.tier == Tier::Validate {
            "the Agent started; this validation requirement belongs to ADP"
        } else {
            &result.agent
        };
        match (&result.mismatch, expected) {
            // The default: ADP matches the Agent and nobody pinned anything.
            (None, None) => {}
            // The pinned difference is gone: the check now does what the Agent does.
            (None, Some(e)) => {
                let (now, actual) = if matches!(result.key.tier, Tier::Deserialize | Tier::Translate | Tier::Validate) {
                    (
                        "This check now passes: the stage does what the Agent (or the contract) expects.",
                        "the stage succeeded".to_string(),
                    )
                } else {
                    ("This check now matches the Agent.", result.agent.clone())
                };
                let advice = match e.status {
                    Status::KnownBug => "This is an improvement for this check only, not evidence that the bug is fixed everywhere. Remove this check's known-bug expectation so it asserts the recorded Agent result directly.",
                    Status::Intentional => "This expectation pins deliberate ADP behavior, so its disappearance is unexpected, not an improvement. Restore the deliberate behavior; only if the behavior is being deliberately revised, change or remove this expectation and its explanation in the same change.",
                };
                acc.problem(
                    false,
                    result.key.clone(),
                    format!(
                        "{context}\n\n{now}\n\nExpected current ADP ({:?}): {}\nActual ADP: {}\nRecorded Agent: {}\n\n{advice}\nExpectation: {}",
                        e.status, e.adp, actual, result.agent, e.at
                    ),
                    [result.case.clone()],
                );
            }
            (Some(m), None) => {
                acc.problem(
                    false,
                    result.key.clone(),
                    format!(
                        "{context}\n\nUNEXPECTED RESULT: ADP did not meet this check's expectation.\n\nExpected ADP: {}\nActual ADP: {}\nRecorded Agent: {}\n\nDecide whether ADP is wrong (add a known-bug entry) or deliberately different (add an intentional entry).\nExpectation: add one in corpus_replay/expected.rs; the failing assertion is at {assertion}.",
                        result.agent, m.adp, recorded
                    ),
                    [result.case.clone()],
                );
            }
            (Some(m), Some(e)) if e.adp == m.adp && !matches!(m.adp, Adp::Panic(_) | Adp::Corpus(_)) => {}
            (Some(m), Some(e)) => {
                let never = if matches!(m.adp, Adp::Panic(_) | Adp::Corpus(_)) {
                    "\nA panic, or a result whose recorded shape is malformed, is not an expected result: fix the code or the corpus, not the table."
                } else {
                    ""
                };
                acc.problem(
                    false,
                    result.key.clone(),
                    format!(
                        "{context}\n\nCHANGED RESULT ({:?}): the expectation no longer describes ADP.\n\nExpected current ADP: {}\nDesired (recorded Agent): {}\nActual ADP: {}\n\nCause: {} ({}){never}\nExpectation: {}",
                        e.status, e.adp, result.agent, m.adp, e.cause.name, e.cause.why, e.at
                    ),
                    [result.case.clone()],
                );
            }
        }
    }

    // A recorded panic or case fault abandoned the checks the case had left, so an entry for one of
    // them is not evidence that it stopped running. Only entries for keys the abandoned case holds
    // are covered, not those of other cases sharing its origin; the panic or fault itself already fails.
    let canon = leaf_key_canons();
    let mut abandoned: Vec<(&Case, Option<u8>)> = Vec::new();
    for fault in &collected.faults {
        abandoned.extend(fault_case(corpus, fault).map(|case| (case, None)));
    }
    for result in &collected.results {
        if matches!(result.mismatch, Some(Mismatch { adp: Adp::Panic(_) })) {
            abandoned.extend(
                corpus
                    .case(&result.case)
                    .map(|case| (case, Some(result.key.tier.family()))),
            );
        }
    }
    let is_abandoned = |check: &CheckKey| {
        abandoned.iter().any(|(case, family)| {
            case.check_origin() == check.origin
                && family.is_none_or(|family| family == check.tier.family())
                && case_holds_key(case, &check.key, &canon)
        })
    };

    // An entry whose check never ran. A comparison that was never performed is not an improvement.
    for (check, entries) in &by_check {
        if seen.contains_key(check) || !cases_of_origin.contains_key(check.origin.as_str()) || is_abandoned(check) {
            continue;
        }
        let e = entries[0];
        acc.problem(
            false,
            (*check).clone(),
            format!(
                "UNUSED EXPECTATION: no replay produced a check with this identity.\n\nCases: {}\nCheck: {}\nExpected ADP: {}\nCause: {} ({})\n\nThe check may have stopped running: a removed key, a different checkpoint, getter or key, or a deserialization that blocks its translation. Remove this expectation, or fix the check it names.\nExpectation: {}",
                origin_summary(&cases_of_origin, &check.origin),
                check,
                e.adp,
                e.cause.name,
                e.cause.why,
                e.at
            ),
            cases_of(&check.origin),
        );
    }
    acc.finish()
}

#[cfg(test)]
mod tests {
    use datadog_agent_config::LEAVES;
    use datadog_agent_config_corpus::Outcome;
    use saluki_config::dynamic::{ConfigSetting, ConfigUpdate};
    use serde_json::json;

    use super::super::compare::{LeafKind, Reason};
    use super::super::corpus;
    use super::super::driver::{Commit, Replay, Validation};
    use super::super::expected::expectations;
    use super::*;

    /// Supported leaves that no started case compares with a getter today. Growing this list is a
    /// coverage regression; a case that starts comparing one makes
    /// [`every_supported_leaf_is_compared_by_a_real_comparison`] fail until it is removed.
    const UNCOMPARED_LEAVES: &[&str] = &[
        "otlp_config.receiver.protocols.grpc.keepalive.server_parameters.max_connection_age",
        "otlp_config.receiver.protocols.grpc.keepalive.server_parameters.max_connection_age_grace",
        "otlp_config.receiver.protocols.grpc.keepalive.server_parameters.time",
        "otlp_config.receiver.protocols.grpc.keepalive.server_parameters.timeout",
        "otlp_config.receiver.protocols.grpc.tls.ca_file",
        "otlp_config.receiver.protocols.grpc.tls.cert_file",
        "otlp_config.receiver.protocols.grpc.tls.key_file",
        "otlp_config.receiver.protocols.http.cors.exposed_headers",
        "otlp_config.receiver.protocols.http.cors.max_age",
        "otlp_config.receiver.protocols.http.tls.ca_file",
        "otlp_config.receiver.protocols.http.tls.cert_file",
        "otlp_config.receiver.protocols.http.tls.key_file",
    ];

    /// Replays the whole corpus and requires every check to match the Agent or its expectation.
    #[test]
    fn corpus_replay_matches_the_agent_or_its_expectation() {
        let corpus = corpus();
        let collected = collect(corpus);
        assert!(collected.results.len() > 1000, "the corpus produces checks");
        assert!(
            collected
                .results
                .iter()
                .any(|r| r.key.tier == Tier::Translate && r.mismatch.is_none()),
            "the translate tier records successful translations, not only failures"
        );
        if let Some(message) = judge(corpus, &collected, &expectations()).message() {
            panic!("{message}");
        }
    }

    /// Startup follows the process's rule: a snapshot is adopted exactly when validation accepts it,
    /// and validation says exactly what [`expected_validation`] states for the streamed `api_key`.
    /// The corpus baseline streams `api_key: ""`, so nearly every case is rejected here and its later
    /// events are never applied (see `Commit::Accepted`). The per-operation validate checks of
    /// [`collect`] cover every other translated step.
    #[test]
    fn startup_adopts_a_snapshot_exactly_when_validation_accepts_it() {
        let corpus = corpus();
        let mut problems = Vec::new();
        let (mut rejected, mut accepted) = (0, 0);
        for case in corpus.cases.iter().filter(|c| matches!(c.outcome, Outcome::Started(_))) {
            let steps = replay_case(corpus, &case.name, Commit::Accepted).expect("the case replays");
            let first = &steps[0];
            let Some(validation) = &first.validation else {
                continue;
            };
            let valid = *validation == Validation::Valid;
            if *validation != expected_validation(first.api_key.as_ref())
                || first.committed != valid
                || (!valid && steps.len() != 1)
            {
                problems.push(format!(
                    "case {} ({}): api_key {:?}, validation {validation:?}, committed {}, {} step(s) replayed",
                    case.name,
                    input_location(case),
                    first.api_key,
                    first.committed,
                    steps.len()
                ));
            }
            if valid {
                accepted += 1;
            } else {
                rejected += 1;
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        assert!(
            rejected > 100 && accepted >= 1,
            "{rejected} rejected, {accepted} accepted"
        );
    }

    /// Every supported leaf is compared (Match or Differs) by at least one case, except the listed ones.
    #[test]
    fn every_supported_leaf_is_compared_by_a_real_comparison() {
        let (rows, faults) = corpus_rows(corpus());
        assert!(faults.is_empty(), "{faults:#?}");
        let compared: BTreeSet<&str> = rows
            .iter()
            .filter(|row| matches!(row.result, RowResult::Leaf(Verdict::Match | Verdict::Differs { .. })))
            .filter_map(|row| row.leaf)
            .collect();
        let uncompared: BTreeSet<&str> = LEAVES.iter().map(|l| l.key).filter(|k| !compared.contains(k)).collect();
        let listed: BTreeSet<&str> = UNCOMPARED_LEAVES.iter().copied().collect();
        assert_eq!(
            uncompared, listed,
            "leaves no case compares (left) versus UNCOMPARED_LEAVES (right)"
        );
    }

    /// The [`LeafKind`] a kind name stands for; `LeafKind` itself is not name-addressable.
    fn kind_of_name(name: &str) -> LeafKind {
        match name {
            "Bool" => LeafKind::Bool,
            "Duration" => LeafKind::Duration,
            "F64" => LeafKind::F64,
            "I64" => LeafKind::I64,
            "JsonList" => LeafKind::JsonList,
            "OptionI64" => LeafKind::OptionI64,
            "OptionStr" => LeafKind::OptionStr,
            "Str" => LeafKind::Str,
            "StringList" => LeafKind::StringList,
            "StringListMap" => LeafKind::StringListMap,
            "StringMap" => LeafKind::StringMap,
            "StringMapList" => LeafKind::StringMapList,
            other => panic!("{other} is not a leaf kind name"),
        }
    }

    /// The emulation list decides every recorded (leaf kind, getter) pair of the corpus: a getter
    /// the kind stands for is compared, a getter it does not stand for is reported as not
    /// emulated, and no recorded result has a shape its getter cannot have.
    #[test]
    fn the_emulation_list_decides_every_recorded_getter_pair() {
        let corpus = corpus();
        let (rows, _) = corpus_rows(corpus);
        let (derived, _) = corpus_derived_rows(corpus);
        let mut disagreements = Vec::new();
        for row in rows.iter().filter(|r| r.kind.is_some() && r.getter.is_some()) {
            let RowResult::Leaf(verdict) = &row.result else {
                continue;
            };
            let (kind, getter) = (row.kind.expect("filtered"), row.getter.expect("filtered"));
            let emulated = kind_of_name(kind).emulated().contains(&getter);
            let compared = !matches!(verdict, Verdict::NotCompared { .. });
            if emulated != compared {
                disagreements.push(format!(
                    "{row}: the emulation list says {getter} {} by a {kind} leaf",
                    if emulated { "is emulated" } else { "is not emulated" }
                ));
            }
        }
        for row in derived.iter().filter(|r| {
            matches!(
                r.verdict,
                Verdict::NotCompared {
                    reason: Reason::ResultShape
                }
            )
        }) {
            disagreements.push(format!("{row}: the recorded result does not have its getter's shape"));
        }
        assert!(
            disagreements.is_empty(),
            "the emulation list and the recorded pairs disagree:\n{}",
            disagreements.join("\n")
        );
    }

    /// The translate tier records the keys that translate, so a fixed rejection becomes an
    /// improvement on its exact check and a removed key becomes an unused expectation.
    #[test]
    fn translation_checks_record_successes_too() {
        let collected = collect(corpus());
        let mut by_at: BTreeMap<String, Vec<&CheckResult>> = BTreeMap::new();
        for result in collected.results.iter().filter(|r| r.key.tier == Tier::Translate) {
            if result.key.origin == "valid-stream-updates" {
                by_at.entry(result.key.at.clone()).or_default().push(result);
            }
        }
        let of = |at: &str| by_at.get(at).map(|rs| rs.as_slice()).unwrap_or(&[]);
        assert!(
            of("update dogstatsd_tag_cardinality #1").len() == 1
                && of("update dogstatsd_tag_cardinality #1")[0].mismatch.is_none(),
            "the accepted `high` update has a passing check: {:?}",
            of("update dogstatsd_tag_cardinality #1")
        );
        assert!(
            of("update dogstatsd_tag_cardinality #2").len() == 1
                && of("update dogstatsd_tag_cardinality #2")[0]
                    .mismatch
                    .as_ref()
                    .is_some_and(|m| matches!(m.adp, Adp::Error(_))),
            "the rejected `bogus` update has the pinned error check: {:?}",
            of("update dogstatsd_tag_cardinality #2")
        );
        assert!(
            of("update dogstatsd_tag_cardinality #3").len() == 1
                && of("update dogstatsd_tag_cardinality #3")[0].mismatch.is_none(),
            "the recovering `orchestrator` update has a passing check: {:?}",
            of("update dogstatsd_tag_cardinality #3")
        );
    }

    fn translate_one(settings: &[(&str, serde_json::Value)]) -> agent_data_plane_config::SalukiConfiguration {
        let mut replay = Replay::new(Commit::Accepted);
        let update = ConfigUpdate::snapshot(settings.iter().map(|(k, v)| ConfigSetting::explicit(*k, v.clone())));
        replay.apply(&update).1.expect("the snapshot translates")
    }

    /// The translated `api_key` keeps its whitespace, while a whitespace-only key is still the typed
    /// `MissingApiKey`: validation trims only to decide emptiness. Trimming what is sent is not
    /// observable through a getter (see the `api_key` limitation in `expected.rs`).
    #[test]
    fn api_key_whitespace_is_kept_but_a_whitespace_only_key_is_missing() {
        let config = translate_one(&[("api_key", json!(" cr-key "))]);
        assert_eq!(config.shared.endpoints.api_key, " cr-key ");

        let mut replay = Replay::new(Commit::Accepted);
        let blank = ConfigUpdate::snapshot([ConfigSetting::explicit("api_key", json!("  "))]);
        let failure = replay
            .apply(&blank)
            .0
            .failure
            .expect("a whitespace-only key is rejected");
        assert!(failure.missing_api_key);
    }

    /// ADP keeps `proxy.no_proxy` as streamed; the Agent's cloud-metadata entries arrive in the stream.
    #[test]
    fn no_proxy_is_translated_as_streamed() {
        let config = translate_one(&[
            ("api_key", json!("k")),
            ("proxy.no_proxy", json!(["a", "169.254.169.254"])),
        ]);
        assert_eq!(config.shared.endpoints.proxy.no_proxy, ["a", "169.254.169.254"]);
    }

    fn no_isolation(_: &str) -> Option<Adp> {
        None
    }

    fn checks_of(
        step: &StepRecord, is_snapshot: bool, operations: &[&str], isolate: &mut dyn FnMut(&str) -> Option<Adp>,
    ) -> Vec<(Tier, String, Option<Adp>)> {
        let canon = leaf_key_canons();
        let supported: BTreeSet<&str> = canon.values().copied().collect();
        let operations: Vec<String> = operations.iter().map(ToString::to_string).collect();
        step_checks(step, is_snapshot, &operations, &supported, &canon, isolate)
            .into_iter()
            .map(|c| (c.tier, c.key, c.adp))
            .collect()
    }

    fn set(key: &str, value: serde_json::Value) -> ConfigUpdate {
        ConfigUpdate::Partial(ConfigSetting::explicit(key, value))
    }

    fn snapshot_of(settings: &[(&str, serde_json::Value)]) -> ConfigUpdate {
        ConfigUpdate::snapshot(settings.iter().map(|(k, v)| ConfigSetting::explicit(*k, v.clone())))
    }

    fn only(checks: &[(Tier, String, Option<Adp>)], tier: Tier) -> Vec<(&str, Option<&Adp>)> {
        checks
            .iter()
            .filter(|c| c.0 == tier)
            .map(|c| (c.1.as_str(), c.2.as_ref()))
            .collect()
    }

    /// With several translator errors in one step, every key the translator did handle still gets
    /// its check, and fixing one error turns that key's check into a passing one while the other
    /// stays a failing check.
    #[test]
    fn a_step_with_several_translate_errors_checks_every_key() {
        let keys = ["dogstatsd_tag_cardinality", "log_level", "dogstatsd_log_file_max_size"];
        let step_of = |cardinality: &str, size: &str| {
            Replay::new(Commit::Translated)
                .apply(&snapshot_of(&[
                    ("api_key", json!("k")),
                    (keys[0], json!(cardinality)),
                    (keys[1], json!("error")),
                    (keys[2], json!(size)),
                ]))
                .0
        };
        let checks = checks_of(&step_of("bogus", "cr-a"), true, &keys, &mut no_isolation);
        let translate = only(&checks, Tier::Translate);
        assert_eq!(translate.len(), 3, "{translate:?}");
        for (key, adp) in &translate {
            match *key {
                "log_level" => assert_eq!(*adp, None),
                _ => assert!(matches!(adp, Some(Adp::Error(e)) if e.contains(key)), "{key}: {adp:?}"),
            }
        }
        assert!(only(&checks, Tier::Validate).is_empty(), "validation never ran");

        let fixed = checks_of(&step_of("high", "cr-a"), true, &keys, &mut no_isolation);
        let translate = only(&fixed, Tier::Translate);
        assert_eq!(translate.len(), 3);
        assert_eq!(translate.iter().filter(|(_, adp)| adp.is_none()).count(), 2);
        assert!(translate
            .iter()
            .any(|(key, adp)| *key == keys[2] && matches!(adp, Some(Adp::Error(_)))));
    }

    /// A rejected whole-configuration update is one exact failing deserialize check on the update's
    /// setting, and nothing after deserialization is inferred to have run.
    #[test]
    fn a_rejected_update_fails_its_own_deserialize_check_and_nothing_after() {
        let mut replay = Replay::new(Commit::Translated);
        replay.apply(&snapshot_of(&[("api_key", json!("k"))]));
        let (step, _) = replay.apply(&set("allow_arbitrary_tags", json!("on")));
        let checks = checks_of(&step, false, &["allow_arbitrary_tags"], &mut no_isolation);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].0, Tier::Deserialize);
        assert_eq!(checks[0].1, "allow_arbitrary_tags");
        assert_eq!(
            checks[0].2,
            Some(Adp::error(
                "invalid value: string \"on\", expected a boolean, a boolean string, or a number"
            ))
        );

        // An accepted update passes deserialization and reaches the later stages.
        let (step, _) = replay.apply(&set("allow_arbitrary_tags", json!(true)));
        let checks = checks_of(&step, false, &["allow_arbitrary_tags"], &mut no_isolation);
        assert_eq!(
            checks.iter().map(|c| c.0).collect::<Vec<_>>(),
            [Tier::Deserialize, Tier::Translate, Tier::Validate]
        );
        assert!(checks.iter().all(|c| c.2.is_none()), "{checks:?}");
    }

    /// A snapshot's whole-configuration error is attributed to the settings that reject in isolation,
    /// and an error no setting accounts for gets a `<whole tree>` check instead of passing silently.
    #[test]
    fn a_snapshot_deserialize_error_no_setting_explains_gets_its_own_check() {
        let step = Replay::new(Commit::Translated)
            .apply(&snapshot_of(&[("allow_arbitrary_tags", json!("on"))]))
            .0;
        let whole = "invalid value: string \"on\", expected a boolean, a boolean string, or a number";
        let explained = checks_of(&step, true, &["allow_arbitrary_tags"], &mut |_| Some(Adp::error(whole)));
        assert_eq!(
            explained,
            [(
                Tier::Deserialize,
                "allow_arbitrary_tags".into(),
                Some(Adp::error(whole))
            )]
        );

        let other = checks_of(&step, true, &["allow_arbitrary_tags"], &mut |_| {
            Some(Adp::error("another error"))
        });
        assert_eq!(other.len(), 2, "{other:?}");
        assert!(other.contains(&(Tier::Deserialize, "<whole tree>".into(), Some(Adp::error(whole)))));
    }

    /// Validation is checked against the stated contract, not against itself: a blank key must be the
    /// typed `MissingApiKey`, any other key must be accepted, and any other result fails.
    #[test]
    fn validation_is_checked_per_operation_against_the_api_key_contract() {
        for (api_key, expected) in [
            (json!(""), Validation::MissingApiKey),
            (json!(" \t"), Validation::MissingApiKey),
            (json!("k"), Validation::Valid),
        ] {
            let step = Replay::new(Commit::Translated)
                .apply(&snapshot_of(&[("api_key", api_key.clone())]))
                .0;
            assert_eq!(step.validation, Some(expected), "{api_key}");
            let checks = checks_of(&step, true, &["api_key"], &mut no_isolation);
            let validation = only(&checks, Tier::Validate);
            assert_eq!(validation, [("api_key", None)], "{api_key}");
        }
    }
}
