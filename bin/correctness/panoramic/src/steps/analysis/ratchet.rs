//! Known differences between the baseline and the comparison, tagged with the gap that explains them.
//!
//! A known difference lets a case pass while a gap is still open, and fails the case once the difference goes away
//! (a stale entry), so that every fix has to remove its entries.

use std::{collections::BTreeMap, fmt};

use serde::Deserialize;
use serde_json::Value;

/// The kind of a difference between a baseline record and its comparison counterpart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Both sides have the field, with different values.
    Value,

    /// Only the baseline has the field.
    Missing,

    /// Only the comparison has the field.
    Extra,

    /// The field is in a different location on each side. The path uses the baseline's location.
    Location,
}

/// A single difference between a baseline record and its comparison counterpart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldDiff {
    /// Path of the field, such as `envelope.status` for a log or `stats.hits` for a trace statistics group.
    pub path: String,

    /// The kind of difference.
    pub kind: DiffKind,

    /// The baseline value, if present.
    pub baseline: Option<Value>,

    /// The comparison value, if present.
    pub comparison: Option<Value>,
}

impl fmt::Display for FieldDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>| value.as_ref().map_or_else(|| "<absent>".to_string(), Value::to_string);
        write!(
            f,
            "{}: {:?} (baseline: {}, comparison: {})",
            self.path,
            self.kind,
            show(&self.baseline),
            show(&self.comparison)
        )
    }
}

/// A known difference, set in the `known_differences` of a `compare` step.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownDifference {
    /// The gap ID that explains the difference, such as `gap/content-in-message`.
    pub gap: String,

    /// The number of the GitHub issue that tracks the gap.
    #[serde(default)]
    pub issue: Option<u64>,

    /// Location-qualified field paths the difference applies to.
    ///
    /// Paths are matched segment by segment, splitting on `.`. A segment of `*` matches any one segment, and a segment
    /// of `**` matches any number of segments, including none.
    pub paths: Vec<String>,

    /// Kinds of difference the entry applies to. When empty, it applies to every kind.
    #[serde(default)]
    pub kinds: Vec<DiffKind>,

    /// Restricts the entry to records whose view has, for every listed field, one of the listed values. For example,
    /// `{fixture_id: [a, b]}` scopes a logs entry to two fixtures. When empty, the entry applies to every record. Each
    /// analysis says what the view of its records holds. Each listed value must match a difference at each path, or
    /// the entry is stale.
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

/// A part of a known difference's `when` that the ratchet tracks the use of on its own.
enum Scope<'a> {
    /// The entry has no `when`, so it applies to every record.
    All,

    /// One value of one field of the entry's `when`.
    Value(&'a str, &'a Value),
}

impl Scope<'_> {
    /// Returns `true` if a record whose view is `view` falls in the scope.
    fn holds(&self, view: &BTreeMap<String, Value>) -> bool {
        match self {
            Scope::All => true,
            Scope::Value(field, value) => view.get(*field) == Some(*value),
        }
    }
}

/// A known difference, and whether a diff used each of its paths, for each of its scopes.
struct Tracked<'a> {
    entry: &'a KnownDifference,
    scopes: Vec<Scope<'a>>,

    /// For each path of the entry, whether a diff used it, for each scope.
    used: Vec<Vec<bool>>,
}

/// Matches diffs against known differences, and tracks which entry paths were used, for each value of the entry's
/// `when`.
pub struct Ratchet<'a> {
    entries: Vec<Tracked<'a>>,
}

impl<'a> Ratchet<'a> {
    /// Creates a new `Ratchet` over the given entries.
    pub fn new(entries: &'a [KnownDifference]) -> Self {
        let entries = entries
            .iter()
            .map(|entry| {
                let mut scopes = entry
                    .when
                    .iter()
                    .flat_map(|(field, values)| values.iter().map(|value| Scope::Value(field.as_str(), value)))
                    .collect::<Vec<_>>();
                if scopes.is_empty() {
                    scopes.push(Scope::All);
                }
                Tracked {
                    entry,
                    used: vec![vec![false; scopes.len()]; entry.paths.len()],
                    scopes,
                }
            })
            .collect();
        Self { entries }
    }

    /// Returns `true` if any entry covers `diff`, found on a record whose view is `view`.
    ///
    /// Every entry path matching `diff` is marked as used for each `when` value that the view holds, so overlapping
    /// entries aren't reported as stale.
    pub fn allows(&mut self, diff: &FieldDiff, view: &BTreeMap<String, Value>) -> bool {
        let mut allowed = false;
        for tracked in &mut self.entries {
            if !tracked.entry.applies(diff, view) {
                continue;
            }
            for (pattern, used) in tracked.entry.paths.iter().zip(&mut tracked.used) {
                if !path_matches(pattern, &diff.path) {
                    continue;
                }
                allowed = true;
                for (scope, used) in tracked.scopes.iter().zip(used.iter_mut()) {
                    if scope.holds(view) {
                        *used = true;
                    }
                }
            }
        }
        allowed
    }

    /// Returns a description of every entry path that matched no diff, for each `when` value that it matched no diff
    /// for.
    pub fn stale_entries(&self) -> Vec<String> {
        let mut stale = Vec::new();
        for Tracked { entry, scopes, used } in &self.entries {
            let issue = entry.issue.map(|issue| format!(" (#{})", issue)).unwrap_or_default();
            for (path, used) in entry.paths.iter().zip(used) {
                for (scope, used) in scopes.iter().zip(used) {
                    if *used {
                        continue;
                    }
                    let scope = match scope {
                        Scope::All => String::new(),
                        Scope::Value(field, value) => format!(" when `{}` is {}", field, value),
                    };
                    stale.push(format!(
                        "Known difference for {}{} no longer occurs: path `{}`{} ({}). Remove it from the case.",
                        entry.gap, issue, path, scope, entry.reason
                    ));
                }
            }
        }
        stale
    }
}

/// Returns `true` if `path` matches the glob `pattern`.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.split('.').collect::<Vec<_>>();
    let path = path.split('.').collect::<Vec<_>>();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, _) => path.is_empty(),
        (Some((&"**", rest)), _) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        (Some((segment, rest)), Some((path_segment, path_rest))) => {
            (*segment == "*" || segment == path_segment) && segments_match(rest, path_rest)
        }
        (Some(_), None) => false,
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
            issue: None,
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
    fn globs_match_segment_by_segment() {
        let cases = [
            ("envelope.status", "envelope.status", true),
            ("envelope.status", "envelope.status.extra", false),
            ("content.*", "content.attr", true),
            ("content.*", "content.attr.nested", false),
            ("content.otel.*", "content.otel.trace_id", true),
            ("content.otel.*", "content.otelx.trace_id", false),
            ("content.**", "content.attr.nested.deep", true),
            ("content.**", "content", true),
            ("content.**", "envelope_properties.attr", false),
            ("**.trace_id", "envelope_properties.otel.trace_id", true),
        ];

        for (pattern, path, expected) in cases {
            assert_eq!(path_matches(pattern, path), expected, "{} vs {}", pattern, path);
        }
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

        assert!(ratchet.allows(&status, &view("severity-0")));
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
    fn a_scoped_entry_is_stale_for_each_value_that_no_longer_differs() {
        let mut scoped = entry("gap/zero-severity-status", &["envelope.status"], &[]);
        scoped.when = BTreeMap::from([("fixture_id".to_string(), vec![json!("severity-0"), json!("severity-1")])]);
        let entries = [scoped];
        let mut ratchet = Ratchet::new(&entries);
        let view = BTreeMap::from([("fixture_id".to_string(), json!("severity-1"))]);

        assert!(ratchet.allows(&diff("envelope.status", DiffKind::Value), &view));

        let stale = ratchet.stale_entries();
        assert_eq!(
            stale,
            vec![
                "Known difference for gap/zero-severity-status no longer occurs: path `envelope.status` when \
                 `fixture_id` is \"severity-0\" (test). Remove it from the case."
            ]
        );
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
