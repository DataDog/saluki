//! Produces one verdict per recorded getter for every key line of every started corpus case, at both
//! of the corpus's checkpoints (record.md §5.3).
//!
//! This is the leaf tier. It compares against the Agent's stream with every update folded in,
//! whether or not the typed configuration would have committed it: the Agent applied every update,
//! so comparing against the last-known-good state instead would turn one rejected value into
//! mismatches on every other key. The committed-state tier is the driver's.
//!
//! A whole-configuration deserialization failure names no key, so each leaf is deserialized in
//! isolation: `DatadogConfiguration` is built from an object that holds only that leaf's value, and
//! every other field takes its default. This is sound because the generated source model has no
//! flattened fields and does not deny unknown fields, so its fields deserialize independently; the
//! isolation invariant test checks that on the whole corpus.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use datadog_agent_config::{DatadogConfiguration, Leaf, LeafValue, LEAVES};
use datadog_agent_config_corpus::{Corpus, Getter, KeyLine, Outcome, Read};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::compare::{compare_leaf, LeafKind, Verdict};
use super::driver::case_updates;
use crate::source::SourceTree;
use crate::system::fold;

/// One of the two points at which the corpus records getter reads (record.md §5.3).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Checkpoint {
    /// After the first snapshot: `reads.snapshot`.
    Snapshot,
    /// After the last update: `reads.final`.
    Final,
}

/// A case's stream folded up to each checkpoint, every update included.
pub(crate) struct FoldedCase {
    /// The value of the Agent layer after the first snapshot.
    pub(crate) snapshot: Value,
    /// The value of the Agent layer after every event of the stream.
    pub(crate) last: Value,
    /// How many events follow the first snapshot.
    pub(crate) updates: usize,
    /// The merged sources after the first snapshot, provenance included, as translation reads them.
    pub(crate) snapshot_sources: SourceTree,
    /// The merged sources after every event of the stream, provenance included.
    pub(crate) last_sources: SourceTree,
}

/// Folds every event of the started case `case_name` onto an empty local base, rejected or not.
///
/// Each event is applied the way the update step applies it: a snapshot replaces the Agent layer and
/// a partial update sets one key. The merged value is the empty local base overlaid with that layer.
///
/// # Errors
///
/// Returns an error if the case's stream cannot be built (see [`case_updates`]).
pub(crate) fn fold_case(corpus: &Corpus, case_name: &str) -> Result<FoldedCase, String> {
    let updates = case_updates(corpus, case_name)?;
    let merged = |agent: &SourceTree| SourceTree::empty().overlay(agent);

    let mut agent = SourceTree::empty();
    let mut snapshot = None;
    for update in &updates {
        fold(&mut agent, update);
        snapshot.get_or_insert_with(|| merged(&agent));
    }

    let snapshot_sources = snapshot.expect("case_updates returns a non-empty stream");
    let last_sources = merged(&agent);
    Ok(FoldedCase {
        snapshot: snapshot_sources.to_value(),
        last: last_sources.to_value(),
        updates: updates.len() - 1,
        snapshot_sources,
        last_sources,
    })
}

/// Deserializes `DatadogConfiguration` alone from a merged value, without the Saluki-only model.
pub(crate) fn deserialize_datadog(value: &Value) -> Result<DatadogConfiguration, String> {
    DatadogConfiguration::deserialize(value).map_err(|e| e.to_string())
}

/// Returns the value at the dotted `path` of `value`, following one object member per segment.
pub(crate) fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |node, segment| node.as_object()?.get(segment))
}

/// Inserts `leaf_value` at the dotted `path` of the object `root`, creating objects on the way.
fn insert(root: &mut Map<String, Value>, path: &str, leaf_value: Value) {
    let mut node = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            node.insert(segment.to_string(), leaf_value);
            return;
        }
        node = node
            .entry(segment)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("only objects are created on the way to a leaf");
    }
}

/// Returns every path `leaf` can be read from: its dotted key, then each alias as a sibling of the
/// key's last segment (a serde alias renames only the field it is written on).
fn leaf_paths(leaf: &Leaf) -> impl Iterator<Item = String> + '_ {
    let parent = leaf.key.rsplit_once('.').map(|(parent, _)| parent);
    std::iter::once(leaf.key.to_string()).chain(leaf.aliases.iter().map(move |alias| match parent {
        Some(parent) => format!("{parent}.{alias}"),
        None => (*alias).to_string(),
    }))
}

/// Returns an object holding only what `tree` holds for `leaf`: the value at its key, or at an
/// alias. When the tree holds both, both are kept, so the isolated object fails exactly as the whole
/// tree does. A leaf the tree does not hold is isolated as an empty object and reads its default.
fn isolated_object(leaf: &Leaf, tree: &Value) -> Value {
    let mut object = Map::new();
    for path in leaf_paths(leaf) {
        if let Some(value) = lookup(tree, &path) {
            insert(&mut object, &path, value.clone());
        }
    }
    Value::Object(object)
}

/// The result of deserializing one isolated leaf.
type Isolated = Rc<Result<DatadogConfiguration, String>>;

/// Deserializes leaves in isolation, remembering each distinct isolated object per leaf: across the
/// corpus most leaves hold the same streamed default, so most isolated objects repeat.
pub(crate) struct Isolator {
    /// Every path a leaf can be read from (its key and each alias), to the leaf's index in `LEAVES`.
    by_key: HashMap<String, usize>,
    kinds: Vec<&'static str>,
    /// The configuration every field of which holds its default.
    default: DatadogConfiguration,
    cache: HashMap<(usize, String), Isolated>,
}

impl Isolator {
    pub(crate) fn new() -> Self {
        let default = deserialize_datadog(&Value::Object(Map::new())).expect("the defaults deserialize");
        // Keys first, so an alias spelled like another leaf's key never takes that key over.
        let mut by_key: HashMap<String, usize> = LEAVES
            .iter()
            .enumerate()
            .map(|(i, leaf)| (leaf.key.to_string(), i))
            .collect();
        for (i, leaf) in LEAVES.iter().enumerate() {
            for path in leaf_paths(leaf).skip(1) {
                by_key.entry(path).or_insert(i);
            }
        }
        Self {
            by_key,
            kinds: LEAVES.iter().map(|leaf| kind_name(&(leaf.get)(&default))).collect(),
            default,
            cache: HashMap::new(),
        }
    }

    /// Deserializes `DatadogConfiguration` from only what `tree` holds for `LEAVES[index]`.
    pub(crate) fn isolate(&mut self, index: usize, tree: &Value) -> Isolated {
        let object = isolated_object(&LEAVES[index], tree);
        let key = (index, object.to_string());
        Rc::clone(
            self.cache
                .entry(key)
                .or_insert_with(|| Rc::new(deserialize_datadog(&object))),
        )
    }
}

/// The name of a leaf's `LeafValue` variant, which groups the rows a report prints.
pub(crate) fn kind_name(leaf: &LeafValue<'_>) -> &'static str {
    match leaf {
        LeafValue::Bool(_) => "Bool",
        LeafValue::Duration(_) => "Duration",
        LeafValue::F64(_) => "F64",
        LeafValue::I64(_) => "I64",
        LeafValue::JsonList(_) => "JsonList",
        LeafValue::OptionI64(_) => "OptionI64",
        LeafValue::OptionStr(_) => "OptionStr",
        LeafValue::Str(_) => "Str",
        LeafValue::StringList(_) => "StringList",
        LeafValue::StringListMap(_) => "StringListMap",
        LeafValue::StringMap(_) => "StringMap",
        LeafValue::StringMapList(_) => "StringMapList",
    }
}

/// What one row says about a key at a checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowResult {
    /// The key is a supported leaf; this is its verdict against one recorded getter.
    Leaf(Verdict),
    /// The key is not a supported leaf (unsupported by the overlay, or a section key), so nothing
    /// is compared for it.
    NotModeled,
}

/// One verdict: a (case, checkpoint, key, getter) and what the comparison found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub(crate) case: String,
    pub(crate) checkpoint: Checkpoint,
    pub(crate) key: String,
    /// The supported leaf the key line resolves to, by its key or an alias; `None` for a key that is
    /// not modeled.
    pub(crate) leaf: Option<&'static str>,
    /// The recorded getter; `None` for a key that is not modeled, which gets one row in all.
    pub(crate) getter: Option<Getter>,
    /// The leaf's `LeafValue` variant name; `None` for a key that is not modeled.
    pub(crate) kind: Option<&'static str>,
    /// The value at `key`'s path in the folded tree at this checkpoint, or `None` if the tree does
    /// not hold it.
    pub(crate) streamed: Option<Value>,
    pub(crate) result: RowResult,
}

impl Row {
    fn sort_key(&self) -> (&str, Checkpoint, &str, Option<&'static str>) {
        (&self.case, self.checkpoint, &self.key, self.getter.map(Getter::as_str))
    }
}

/// Escapes line breaks so a rendered value never splits a row across lines.
pub(crate) fn one_line(s: &str) -> String {
    s.replace('\r', "\\r").replace('\n', "\\n")
}

impl fmt::Display for RowResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowResult::Leaf(Verdict::Match) => f.write_str("match"),
            RowResult::Leaf(Verdict::Differs { adp, agent }) => {
                write!(f, "differs: adp {} agent {}", one_line(adp), one_line(agent))
            }
            RowResult::Leaf(Verdict::AdpRejects { error }) => write!(f, "adp rejects: {}", one_line(error)),
            RowResult::Leaf(Verdict::NotCompared { reason }) => write!(f, "not compared: {reason}"),
            RowResult::NotModeled => f.write_str("not modeled"),
        }
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let getter = self.getter.map_or("-", Getter::as_str);
        write!(
            f,
            "{} {:?} {} {getter}: {}",
            self.case, self.checkpoint, self.key, self.result
        )
    }
}

/// The key lines of a case paired with their reads at `checkpoint`.
///
/// # Errors
///
/// Returns an error if a key line's final read is not present exactly when the case has updates.
pub(crate) fn reads_at<'a>(
    case: &str, keys: &'a [KeyLine], checkpoint: Checkpoint, has_updates: bool,
) -> Result<Vec<(&'a str, &'a Read)>, String> {
    keys.iter()
        .filter_map(|line| match (checkpoint, &line.reads.final_) {
            (Checkpoint::Snapshot, _) => Some(Ok((line.key.as_str(), &line.reads.snapshot))),
            (Checkpoint::Final, Some(_)) if !has_updates => Some(Err(format!(
                "case {case:?}: key {:?} has a final read but the case has no updates",
                line.key
            ))),
            (Checkpoint::Final, None) if has_updates => Some(Err(format!(
                "case {case:?}: key {:?} has no final read but the case has updates",
                line.key
            ))),
            (Checkpoint::Final, Some(read)) => Some(Ok((line.key.as_str(), read))),
            (Checkpoint::Final, None) => None,
        })
        .collect()
}

/// Produces the rows for `reads` at one checkpoint, reading each key's leaf from `tree` in isolation.
pub(crate) fn checkpoint_rows(
    isolator: &mut Isolator, case: &str, checkpoint: Checkpoint, tree: &Value, reads: &[(&str, &Read)],
) -> Vec<Row> {
    let mut rows = Vec::new();
    for &(key, read) in reads {
        let streamed = lookup(tree, key).cloned();
        let row = |leaf, getter, kind, result| Row {
            case: case.to_string(),
            checkpoint,
            key: key.to_string(),
            leaf,
            getter,
            kind,
            streamed: streamed.clone(),
            result,
        };
        let Some(&index) = isolator.by_key.get(key) else {
            rows.push(row(None, None, None, RowResult::NotModeled));
            continue;
        };
        let leaf = &LEAVES[index];
        let kind = Some(isolator.kinds[index]);
        let verdicts = match &*isolator.isolate(index, tree) {
            Ok(config) => compare_leaf((leaf.get)(config), &read.getters),
            // Only the getter the leaf's kind stands for would have been compared with the leaf. Every
            // other getter keeps its verdict, which depends only on the kind, so the default leaf gives it.
            Err(error) => {
                let default = (leaf.get)(&isolator.default);
                let emulated = LeafKind::of(&default).emulated();
                compare_leaf(default, &read.getters)
                    .into_iter()
                    .map(|(getter, verdict)| match emulated.contains(&getter) {
                        true => (getter, Verdict::AdpRejects { error: error.clone() }),
                        false => (getter, verdict),
                    })
                    .collect()
            }
        };
        rows.extend(
            verdicts
                .into_iter()
                .map(|(getter, verdict)| row(Some(leaf.key), Some(getter), kind, RowResult::Leaf(verdict))),
        );
    }
    rows
}

/// Produces every row of the corpus's started cases, sorted by case, checkpoint, key and getter.
///
/// # Errors
///
/// Returns every harness error: a case whose stream cannot be built, a case whose stream and
/// recorded updates disagree, or a key line whose final read is not present exactly when the case
/// has updates.
pub(crate) fn corpus_rows(corpus: &Corpus) -> Result<Vec<Row>, Vec<String>> {
    let mut isolator = Isolator::new();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for case in &corpus.cases {
        let Outcome::Started(started) = &case.outcome else {
            continue;
        };
        let has_updates = !started.updates.is_empty();
        let folded = match fold_case(corpus, &case.name) {
            Ok(folded) => folded,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if folded.updates > 0 && !has_updates {
            errors.push(format!(
                "case {:?}: the stream carries updates the case does not record",
                case.name
            ));
            continue;
        }
        let checkpoints = [
            (Checkpoint::Snapshot, &folded.snapshot),
            (Checkpoint::Final, &folded.last),
        ];
        for (checkpoint, tree) in checkpoints {
            match reads_at(&case.name, &started.keys, checkpoint, has_updates) {
                Ok(reads) => rows.extend(checkpoint_rows(&mut isolator, &case.name, checkpoint, tree, &reads)),
                Err(error) => errors.push(error),
            }
        }
    }
    rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Instant;

    use datadog_agent_config_corpus::BASELINE_CASE;
    use serde_json::json;

    use super::*;
    use crate::corpus_replay::corpus;

    /// The most grouped lines the corpus test prints.
    const PRINTED_LINES: usize = 400;

    /// Runs the whole corpus through the leaf tier and prints what it found. It asserts only that no
    /// harness error occurred: the verdicts themselves are checked against known results elsewhere.
    #[test]
    fn every_started_corpus_key_line_gets_leaf_verdicts() {
        let start = Instant::now();
        let rows = corpus_rows(corpus()).unwrap_or_else(|errors| panic!("harness errors: {errors:#?}"));

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut compared = BTreeSet::new();
        let mut not_modeled = BTreeSet::new();
        // Differs and AdpRejects rows by leaf kind, then by error text or rendered pair.
        let mut grouped: BTreeMap<(&str, String), Vec<&Row>> = BTreeMap::new();
        for row in &rows {
            let (label, group) = match &row.result {
                RowResult::Leaf(Verdict::Match) => ("Match".to_string(), None),
                RowResult::Leaf(Verdict::Differs { .. }) => ("Differs".to_string(), Some(row.result.to_string())),
                RowResult::Leaf(Verdict::AdpRejects { .. }) => ("AdpRejects".to_string(), Some(row.result.to_string())),
                RowResult::Leaf(Verdict::NotCompared { reason }) => (format!("NotCompared ({reason})"), None),
                RowResult::NotModeled => ("NotModeled".to_string(), None),
            };
            *counts.entry(label).or_default() += 1;
            match &row.result {
                RowResult::NotModeled => {
                    not_modeled.insert(row.key.as_str());
                }
                RowResult::Leaf(Verdict::Match | Verdict::Differs { .. }) => {
                    compared.insert(row.key.as_str());
                }
                RowResult::Leaf(_) => {}
            }
            if let Some(group) = group {
                grouped.entry((row.kind.unwrap_or("-"), group)).or_default().push(row);
            }
        }

        println!("{} rows in {:?}", rows.len(), start.elapsed());
        for (label, count) in &counts {
            println!("{label}: {count}");
        }
        println!("distinct keys compared: {}", compared.len());
        println!("distinct keys not modeled: {}", not_modeled.len());
        for key in &not_modeled {
            println!("  not modeled: {key}");
        }
        let mut lines = Vec::new();
        for ((kind, group), rows) in &grouped {
            lines.push(format!("[{kind}] {group} ({} rows)", rows.len()));
            for row in rows {
                let getter = row.getter.map_or("-", Getter::as_str);
                lines.push(format!("    {} {:?} {} {getter}", row.case, row.checkpoint, row.key));
            }
        }
        let cut = lines.len().saturating_sub(PRINTED_LINES);
        for line in lines.iter().take(PRINTED_LINES) {
            println!("{line}");
        }
        println!("grouped lines: {} ({cut} not printed)", lines.len());

        assert!(!rows.is_empty(), "the corpus has key lines");
    }

    /// Renders a leaf exactly and deterministically: `Debug` is exact for floats and lists, but a
    /// `HashMap` prints in iteration order, so maps are rendered with their entries sorted.
    fn exact(leaf: LeafValue<'_>) -> String {
        fn sorted<V: fmt::Debug>(map: &HashMap<String, V>) -> String {
            format!("{:?}", map.iter().collect::<BTreeMap<_, _>>())
        }
        match leaf {
            LeafValue::StringListMap(map) => format!("StringListMap({})", sorted(map)),
            LeafValue::StringMap(map) => format!("StringMap({})", sorted(map)),
            LeafValue::StringMapList(maps) => {
                let maps: Vec<_> = maps.iter().map(sorted).collect();
                format!("StringMapList({maps:?})")
            }
            leaf => format!("{leaf:?}"),
        }
    }

    /// Checks that isolating a leaf changes nothing about it: for every started case and checkpoint,
    /// the whole tree deserializes exactly when every isolated leaf does, and then each leaf reads the
    /// same from both. A failure means fields are coupled and per-key verdicts cannot be trusted.
    #[test]
    fn isolated_leaves_agree_with_the_whole_tree() {
        let corpus = corpus();
        let mut isolator = Isolator::new();
        let mut failures = Vec::new();
        let mut checked = 0;
        for case in &corpus.cases {
            let Outcome::Started(started) = &case.outcome else {
                continue;
            };
            let folded = fold_case(corpus, &case.name).expect("the case replays");
            let mut trees = vec![(Checkpoint::Snapshot, &folded.snapshot)];
            if !started.updates.is_empty() {
                trees.push((Checkpoint::Final, &folded.last));
            }
            for (checkpoint, tree) in trees {
                checked += 1;
                let whole = deserialize_datadog(tree);
                let isolated: Vec<_> = (0..LEAVES.len()).map(|i| isolator.isolate(i, tree)).collect();
                let rejected: Vec<_> = LEAVES
                    .iter()
                    .zip(&isolated)
                    .filter(|(_, iso)| iso.is_err())
                    .map(|(leaf, _)| leaf.key)
                    .collect();
                match &whole {
                    Ok(_) if !rejected.is_empty() => failures.push(format!(
                        "{} {checkpoint:?}: the whole tree deserializes but isolated leaves fail: {rejected:?}",
                        case.name
                    )),
                    Err(error) if rejected.is_empty() => failures.push(format!(
                        "{} {checkpoint:?}: the whole tree fails ({error}) but every isolated leaf deserializes",
                        case.name
                    )),
                    Ok(config) => {
                        for (leaf, iso) in LEAVES.iter().zip(&isolated) {
                            let iso = iso.as_ref().as_ref().expect("no isolated leaf failed");
                            let (from_whole, alone) = (exact((leaf.get)(config)), exact((leaf.get)(iso)));
                            if from_whole != alone {
                                failures.push(format!(
                                    "{} {checkpoint:?} {}: whole tree reads {from_whole}, isolated reads {alone}",
                                    case.name, leaf.key
                                ));
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        assert!(checked > 0, "the corpus has started cases");
        assert!(failures.is_empty(), "isolation invariant violated: {failures:#?}");
    }

    #[test]
    fn a_rejected_value_rejects_only_its_own_key() {
        let corpus = corpus();
        let case = corpus.case(BASELINE_CASE).expect("the corpus has the baseline case");
        let Outcome::Started(started) = &case.outcome else {
            panic!("the baseline case starts");
        };
        let reads = reads_at(&case.name, &started.keys, Checkpoint::Snapshot, false).expect("snapshot reads");
        let tree = fold_case(corpus, &case.name).expect("the baseline replays").snapshot;
        let mut isolator = Isolator::new();
        let clean = checkpoint_rows(&mut isolator, &case.name, Checkpoint::Snapshot, &tree, &reads);

        // A bool key the Agent's snapshot agrees with, which a list cannot deserialize into.
        let target = clean
            .iter()
            .find(|row| row.kind == Some("Bool") && row.result == RowResult::Leaf(Verdict::Match))
            .map(|row| row.key.clone())
            .expect("the baseline has a matching bool key");
        let mut poisoned = tree.clone();
        let object = poisoned.as_object_mut().expect("the tree is an object");
        insert(object, &target, json!([true]));
        assert!(
            deserialize_datadog(&poisoned).is_err(),
            "the whole tree rejects the list"
        );

        let rows = checkpoint_rows(&mut isolator, &case.name, Checkpoint::Snapshot, &poisoned, &reads);
        assert_eq!(rows.len(), clean.len());
        let (mut matches, mut rejected) = (0, 0);
        for (row, before) in rows.iter().zip(&clean) {
            if row.key == target && row.getter == Some(Getter::GetBool) {
                assert!(
                    matches!(&row.result, RowResult::Leaf(Verdict::AdpRejects { error }) if !error.is_empty()),
                    "{row}"
                );
                rejected += 1;
            } else if row.key == target {
                // A getter a bool leaf does not stand for keeps the verdict it had.
                assert!(
                    matches!(&row.result, RowResult::Leaf(Verdict::NotCompared { .. })),
                    "{row}"
                );
                assert_eq!(row, before);
            } else {
                assert_eq!(row, before, "only {target} changes");
                matches += usize::from(row.result == RowResult::Leaf(Verdict::Match));
            }
        }
        assert!(matches > 0, "other keys still match");
        assert_eq!(rejected, 1, "the emulated getter of {target} is rejected");
    }
}
