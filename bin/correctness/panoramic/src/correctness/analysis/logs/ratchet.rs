//! Known differences between the baseline and the comparison, tagged with the gap that explains them.
//!
//! A known difference lets a case pass while a gap is still open, and fails the case once the difference goes away
//! (a stale entry), so that every fix has to remove its entries.

use serde::Deserialize;

use super::{DiffKind, FieldDiff};

/// The kinds of difference a known difference entry can match.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum KnownDiffKind {
    /// Both sides have the field, with different values.
    Value,

    /// Only the baseline has the field.
    Missing,

    /// Only the comparison has the field.
    Extra,

    /// The field is in a different location on each side.
    Location,
}

impl KnownDiffKind {
    fn matches(self, kind: &DiffKind) -> bool {
        matches!(
            (self, kind),
            (Self::Value, DiffKind::Value)
                | (Self::Missing, DiffKind::Missing)
                | (Self::Extra, DiffKind::Extra)
                | (Self::Location, DiffKind::Location { .. })
        )
    }
}

/// A known difference, set in the `logs.known_differences` section of a correctness case.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownDifference {
    /// The gap ID that explains the difference, such as `gap/content-in-message`.
    pub gap: String,

    /// Location-qualified field paths the difference applies to.
    ///
    /// Paths are matched segment by segment, splitting on `.`. Within a segment, `*` matches any run of characters,
    /// and a segment of `**` matches any number of segments, including none.
    pub paths: Vec<String>,

    /// Kinds of difference the entry applies to. When empty, it applies to every kind.
    #[serde(default)]
    pub kinds: Vec<KnownDiffKind>,

    /// Why the difference exists.
    pub reason: String,
}

/// Matches diffs against known differences, and tracks which entries were used.
pub struct Ratchet<'a> {
    entries: &'a [KnownDifference],
    used: Vec<Vec<usize>>,
}

impl<'a> Ratchet<'a> {
    /// Creates a new `Ratchet` over the given entries.
    pub fn new(entries: &'a [KnownDifference]) -> Self {
        Self {
            entries,
            used: entries.iter().map(|entry| vec![0; entry.paths.len()]).collect(),
        }
    }

    /// Returns the gap of the first entry matching `diff`, if any, and records the match.
    pub fn classify(&mut self, diff: &FieldDiff) -> Option<&'a str> {
        for (entry_idx, entry) in self.entries.iter().enumerate() {
            if !entry.kinds.is_empty() && !entry.kinds.iter().any(|kind| kind.matches(&diff.kind)) {
                continue;
            }
            if let Some(path_idx) = entry.paths.iter().position(|pattern| path_matches(pattern, &diff.path)) {
                self.used[entry_idx][path_idx] += 1;
                return Some(&entry.gap);
            }
        }
        None
    }

    /// Returns a description of every entry path that matched no diff.
    pub fn stale_entries(&self) -> Vec<String> {
        let mut stale = Vec::new();
        for (entry, used) in self.entries.iter().zip(&self.used) {
            for (path, count) in entry.paths.iter().zip(used) {
                if *count == 0 {
                    stale.push(format!(
                        "Known difference for {} no longer occurs: path `{}`{} ({}). Remove it from the case.",
                        entry.gap,
                        path,
                        describe_kinds(&entry.kinds),
                        entry.reason
                    ));
                }
            }
        }
        stale
    }
}

fn describe_kinds(kinds: &[KnownDiffKind]) -> String {
    if kinds.is_empty() {
        String::new()
    } else {
        format!(", kinds {:?}", kinds)
    }
}

/// Returns `true` if `path` matches the glob `pattern`.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.split('.').collect::<Vec<_>>();
    let path = path.split('.').collect::<Vec<_>>();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((segment, rest)) => match path.split_first() {
            Some((path_segment, path_rest)) => {
                segment_matches(segment.as_bytes(), path_segment.as_bytes()) && segments_match(rest, path_rest)
            }
            None => false,
        },
    }
}

fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
        Some((c, rest)) => text.first() == Some(c) && segment_matches(rest, &text[1..]),
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

    fn entry(gap: &str, paths: &[&str], kinds: &[KnownDiffKind]) -> KnownDifference {
        KnownDifference {
            gap: gap.to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            kinds: kinds.to_vec(),
            reason: "test".to_string(),
        }
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
            ("**.trace_id", "envelope_properties.otel.trace_id", true),
            ("content.list[*]", "content.list[3]", true),
            ("content.list[*]", "content.list", false),
            ("content.attr*", "content.attribute", true),
            ("content.attr*", "envelope_properties.attribute", false),
        ];

        for (pattern, path, expected) in cases {
            assert_eq!(path_matches(pattern, path), expected, "{} vs {}", pattern, path);
        }
    }

    #[test]
    fn entries_are_scoped_to_their_paths_and_kinds() {
        let entries = [
            entry("gap/content-in-message", &["content.*"], &[KnownDiffKind::Location]),
            entry("gap/status-strings", &["envelope.status"], &[]),
        ];
        let mut ratchet = Ratchet::new(&entries);
        let moved = DiffKind::Location {
            baseline: super::super::FieldLocation::Content,
            comparison: super::super::FieldLocation::EnvelopeProperties,
        };

        assert_eq!(
            ratchet.classify(&diff("content.attr", moved)),
            Some("gap/content-in-message")
        );
        // The gap/content-in-message entry only covers location diffs.
        assert_eq!(ratchet.classify(&diff("content.attr", DiffKind::Value)), None);
        assert_eq!(ratchet.classify(&diff("content.attr.nested", DiffKind::Value)), None);
        assert_eq!(
            ratchet.classify(&diff("envelope.status", DiffKind::Value)),
            Some("gap/status-strings")
        );
        assert_eq!(
            ratchet.classify(&diff("envelope.status", DiffKind::Missing)),
            Some("gap/status-strings")
        );
        assert_eq!(ratchet.classify(&diff("envelope.hostname", DiffKind::Value)), None);
        assert!(ratchet.stale_entries().is_empty());
    }

    #[test]
    fn unused_entry_paths_are_stale() {
        let entries = [entry(
            "gap/observed-timestamp",
            &["content.otel.timestamp", "content.@timestamp"],
            &[],
        )];
        let mut ratchet = Ratchet::new(&entries);

        assert_eq!(
            ratchet.classify(&diff("content.otel.timestamp", DiffKind::Value)),
            Some("gap/observed-timestamp")
        );

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
        assert_eq!(entries[0].kinds, vec![KnownDiffKind::Location, KnownDiffKind::Value]);
        assert!(entries[1].kinds.is_empty());

        let unknown_field = "- {gap: gap/content-in-message, paths: [a], reason: r, ignore: true}";
        assert!(serde_saphyr::from_str::<Vec<KnownDifference>>(unknown_field).is_err());
    }
}
