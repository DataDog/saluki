//! Known differences between the baseline and the comparison, tagged with the gap that explains them.
//!
//! A known difference lets a case pass while a gap is still open, and fails the case once the difference goes away
//! (a stale entry), so that every fix has to remove its entries.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use stele::{path_matches, DiffKind, FieldDiff};

/// A known difference, set in the `logs.known_differences` section of a correctness case.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownDifference {
    /// The gap ID that explains the difference, such as `gap/content-in-message`.
    pub gap: String,

    /// Location-qualified field paths the difference applies to.
    ///
    /// Paths are matched segment by segment, splitting on `.`. A segment of `*` matches any one segment, and a segment
    /// of `**` matches any number of segments, including none.
    pub paths: Vec<String>,

    /// Kinds of difference the entry applies to. When empty, it applies to every kind.
    #[serde(default)]
    pub kinds: Vec<DiffKind>,

    /// Restricts the entry to logs whose baseline semantic view (see `stele::Log::semantic_view`) has, for every
    /// listed field, one of the listed values. For example, `{fixture_id: [a, b]}` scopes the entry to two fixtures.
    /// When empty, the entry applies to every log. A non-empty `when` never covers a request header difference, since
    /// those belong to no log.
    #[serde(default)]
    pub when: BTreeMap<String, Vec<Value>>,

    /// Why the difference exists.
    pub reason: String,
}

impl KnownDifference {
    fn applies(&self, diff: &FieldDiff, view: &BTreeMap<String, Value>) -> bool {
        (self.kinds.is_empty() || self.kinds.contains(&diff.kind))
            && self
                .when
                .iter()
                .all(|(field, values)| view.get(field).is_some_and(|value| values.contains(value)))
    }
}

/// Matches diffs against known differences, and tracks which entry paths were used.
pub struct Ratchet<'a> {
    entries: &'a [KnownDifference],
    used: Vec<Vec<bool>>,
}

impl<'a> Ratchet<'a> {
    /// Creates a new `Ratchet` over the given entries.
    pub fn new(entries: &'a [KnownDifference]) -> Self {
        Self {
            entries,
            used: entries.iter().map(|entry| vec![false; entry.paths.len()]).collect(),
        }
    }

    /// Returns `true` if any entry covers `diff`, found on a log whose baseline semantic view is `view`.
    ///
    /// Every entry path matching `diff` is marked as used, so overlapping entries aren't reported as stale.
    pub fn allows(&mut self, diff: &FieldDiff, view: &BTreeMap<String, Value>) -> bool {
        let mut allowed = false;
        for (entry, used) in self.entries.iter().zip(&mut self.used) {
            if !entry.applies(diff, view) {
                continue;
            }
            for (pattern, used) in entry.paths.iter().zip(used.iter_mut()) {
                if path_matches(pattern, &diff.path) {
                    *used = true;
                    allowed = true;
                }
            }
        }
        allowed
    }

    /// Returns a description of every entry path that matched no diff.
    pub fn stale_entries(&self) -> Vec<String> {
        let mut stale = Vec::new();
        for (entry, used) in self.entries.iter().zip(&self.used) {
            for (path, used) in entry.paths.iter().zip(used) {
                if !used {
                    stale.push(format!(
                        "Known difference for {} no longer occurs: path `{}` ({}). Remove it from the case.",
                        entry.gap, path, entry.reason
                    ));
                }
            }
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn diff(path: &str, kind: DiffKind) -> FieldDiff {
        FieldDiff {
            path: path.to_string(),
            kind,
            baseline: Some(json!(1)),
            comparison: Some(json!(2)),
        }
    }

    fn entry(gap: &str, paths: &[&str], kinds: &[DiffKind]) -> KnownDifference {
        KnownDifference {
            gap: gap.to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            kinds: kinds.to_vec(),
            when: BTreeMap::new(),
            reason: "test".to_string(),
        }
    }

    fn no_view() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    #[test]
    fn entries_are_scoped_to_their_paths_and_kinds() {
        let entries = [
            entry("gap/content-in-message", &["content.*"], &[DiffKind::Location]),
            entry("gap/status-strings", &["envelope.status"], &[]),
        ];
        let mut ratchet = Ratchet::new(&entries);

        assert!(ratchet.allows(&diff("content.attr", DiffKind::Location), &no_view()));
        // The gap/content-in-message entry only covers location diffs.
        assert!(!ratchet.allows(&diff("content.attr", DiffKind::Value), &no_view()));
        assert!(!ratchet.allows(&diff("content.attr.nested", DiffKind::Location), &no_view()));
        assert!(ratchet.allows(&diff("envelope.status", DiffKind::Value), &no_view()));
        assert!(ratchet.allows(&diff("envelope.status", DiffKind::Missing), &no_view()));
        assert!(!ratchet.allows(&diff("envelope.hostname", DiffKind::Value), &no_view()));
        assert!(ratchet.stale_entries().is_empty());
    }

    #[test]
    fn entries_can_be_scoped_to_logs() {
        let mut scoped = entry("gap/zero-severity-status", &["envelope.status"], &[DiffKind::Value]);
        scoped.when = BTreeMap::from([("fixture_id".to_string(), vec![json!("severity-0"), json!("severity-1")])]);
        let entries = [scoped];
        let mut ratchet = Ratchet::new(&entries);
        let view = |id: &str| BTreeMap::from([("fixture_id".to_string(), json!(id))]);
        let status = diff("envelope.status", DiffKind::Value);

        assert!(ratchet.allows(&status, &view("severity-1")));
        assert!(!ratchet.allows(&status, &view("severity-2")));
        assert!(!ratchet.allows(&status, &no_view()));
        assert!(ratchet.stale_entries().is_empty());
    }

    #[test]
    fn overlapping_entries_are_all_used() {
        let entries = [
            entry("gap/content-in-message", &["content.**", "content.attr.k"], &[]),
            entry("gap/as-string-coercion", &["content.attr.*"], &[]),
        ];
        let mut ratchet = Ratchet::new(&entries);

        assert!(ratchet.allows(&diff("content.attr.k", DiffKind::Value), &no_view()));
        assert!(ratchet.stale_entries().is_empty(), "{:?}", ratchet.stale_entries());
    }

    #[test]
    fn unused_entry_paths_are_stale() {
        let entries = [entry(
            "gap/observed-timestamp",
            &["content.otel.timestamp", "content.@timestamp"],
            &[],
        )];
        let mut ratchet = Ratchet::new(&entries);

        assert!(ratchet.allows(&diff("content.otel.timestamp", DiffKind::Value), &no_view()));

        let stale = ratchet.stale_entries();
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("gap/observed-timestamp"), "{}", stale[0]);
        assert!(stale[0].contains("`content.@timestamp`"), "{}", stale[0]);
    }

    #[test]
    fn entries_parse_from_case_yaml() {
        let yaml = r#"
- gap: gap/content-in-message
  paths: ["content.**"]
  kinds: [location, value]
  reason: properties are at the top level
- gap: gap/envelope-timestamp
  paths: ["envelope.timestamp"]
  reason: no encode-time timestamp
"#;
        let entries: Vec<KnownDifference> = serde_saphyr::from_str(yaml).expect("entries should parse");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kinds, vec![DiffKind::Location, DiffKind::Value]);
        assert!(entries[1].kinds.is_empty());

        let unknown_field = "- {gap: gap/content-in-message, paths: [a], reason: r, ignore: true}";
        assert!(serde_saphyr::from_str::<Vec<KnownDifference>>(unknown_field).is_err());
    }
}
