//! Sorted string-set matching.
//!
//! [`SortedSetMatcher`] is the one implementation behind every list of names or values a metric is matched against:
//! metric filterlists, allowlists, and set-membership predicates. It compares either raw bytes, for fields matched
//! verbatim such as the raw metric name or a tag value, or intake names, for decisions that must agree with what the
//! intake stores.

use stringtheory::MetaString;

use crate::intake_name::{is_normalized, normalize_into, NameBuf};

/// Sorts a list of literal prefixes and removes the redundant ones: duplicates, and every prefix that starts with
/// another prefix of the list.
///
/// What is left matches exactly the values the whole list matches by prefix, and no retained prefix starts with
/// another, so a value can only start with the greatest retained prefix not greater than it. Prefixes are compared
/// verbatim, byte by byte.
pub fn compact_prefixes<T>(prefixes: &mut Vec<T>)
where
    T: AsRef<str>,
{
    prefixes.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    if prefixes.is_empty() {
        return;
    }

    // After sorting, every prefix that starts with another follows it, possibly after others that also start with
    // it, so comparing each prefix with the last one retained finds them all.
    let mut retained = 0;
    for next in 1..prefixes.len() {
        if !prefixes[next].as_ref().starts_with(prefixes[retained].as_ref()) {
            retained += 1;
            prefixes.swap(retained, next);
        }
    }
    prefixes.truncate(retained + 1);
}

/// A compiled set of strings matched exactly or by prefix.
///
/// Entries are taken verbatim: they are never normalized or otherwise rewritten. In exact mode a value matches when it
/// equals an entry. In prefix mode a value matches when it starts with an entry, so the empty entry matches everything.
///
/// Entries are kept sorted and without duplicates, and in prefix mode an entry that another entry is a prefix of is
/// dropped as redundant (see [`compact_prefixes`]), so a lookup is a single binary search followed, in prefix mode, by
/// one comparison against its predecessor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SortedSetMatcher {
    data: Vec<MetaString>,
    match_prefix: bool,
}

impl SortedSetMatcher {
    /// Creates a matcher from a list of entries and the match mode.
    ///
    /// When `match_prefix` is `true`, values match by prefix; otherwise they must equal an entry.
    pub fn new<T, I>(values: I, match_prefix: bool) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let mut data = values
            .into_iter()
            .map(|value| MetaString::from(value.as_ref()))
            .collect::<Vec<_>>();

        if match_prefix {
            compact_prefixes(&mut data);
        } else {
            data.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
            data.dedup();
        }

        Self { data, match_prefix }
    }

    /// Returns `true` if this matcher matches by prefix rather than exactly.
    pub fn match_prefix(&self) -> bool {
        self.match_prefix
    }

    /// Returns `true` if this matcher has no entries, and therefore matches nothing.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the number of entries the matcher keeps: the distinct entries, without the redundant ones in prefix
    /// mode.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns the memory the matcher's entries take, as memory bounds declare it: each entry's string and its bytes.
    pub fn entry_bytes(&self) -> usize {
        self.data
            .iter()
            .map(|entry| entry.len() + std::mem::size_of::<MetaString>())
            .sum()
    }

    /// Returns whether `value` matches an entry, comparing it verbatim.
    ///
    /// `value` is taken as bytes rather than as a `&str` so that a name normalized into a [`NameBuf`] needs no UTF-8
    /// validation pass: byte-wise ordering matches string ordering, so the search is unaffected.
    pub fn matches(&self, value: &[u8]) -> bool {
        if self.data.is_empty() {
            return false;
        }

        let i = self
            .data
            .binary_search_by(|candidate| candidate.as_ref().as_bytes().cmp(value));

        if self.match_prefix {
            let index = i.unwrap_or_else(|idx| idx);
            if index > 0 && value.starts_with(self.data[index - 1].as_ref().as_bytes()) {
                return true;
            }
        }

        i.is_ok()
    }

    /// Returns whether the intake name of `name` matches an entry.
    ///
    /// The name is normalized before being compared. Metric names arrive exactly as they were submitted, but the
    /// intake rewrites them on ingest: a raw name such as `my metric-name` is stored as `my_metric_name`, which is also
    /// what users see after ingestion and therefore how they write a filterlist entry. Matching the raw name would let
    /// those metrics through the filterlist and still have them stored. Names the intake would reject never match.
    ///
    /// Entries are expected to already be normalized names. An entry that is not matches nothing, so a misconfigured
    /// entry under-matches rather than silently matching more than was asked for.
    ///
    /// This never allocates. Names that are already normalized are compared as given, and the rest are normalized into
    /// a stack buffer. An empty matcher returns before any of that, so deployments with no list configured pay nothing.
    pub fn matches_intake_name(&self, name: &str) -> bool {
        if self.data.is_empty() {
            return false;
        }

        // Fast path: already normalized, so compare the name as given.
        if is_normalized(name) {
            return self.matches(name.as_bytes());
        }

        let mut buf = NameBuf::new();
        match normalize_into(&mut buf, name) {
            Some(normalized) => self.matches(normalized),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_entries_verbatim() {
        let compiled = |values: &[&str]| {
            SortedSetMatcher::new(values.iter().copied(), true)
                .data
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(compiled(&[]), Vec::<String>::new());
        assert_eq!(compiled(&["a"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa", "b", "bb"]), vec!["a", "b"]);
        assert_eq!(compiled(&["a", "b", "bb"]), vec!["a", "b"]);

        // Entries are taken verbatim, never rewritten. A non-normalized entry is kept as-is and simply matches nothing,
        // rather than being rewritten into something that matches more than the user asked for.
        assert_eq!(compiled(&["a-b", "a_b"]), vec!["a-b", "a_b"]);
    }

    #[test]
    fn entry_bytes_count_each_kept_entry_and_its_string() {
        let entry = std::mem::size_of::<MetaString>();
        assert_eq!(SortedSetMatcher::new(Vec::<&str>::new(), false).entry_bytes(), 0);
        // Duplicates are kept once, and in prefix mode a covered prefix not at all.
        assert_eq!(
            SortedSetMatcher::new(["abc", "de", "abc"], false).entry_bytes(),
            3 + 2 + 2 * entry
        );
        assert_eq!(SortedSetMatcher::new(["a.", "a.b."], true).entry_bytes(), 2 + entry);
    }

    /// Normalizing a prefix entry as if it were a complete metric name would strip its trailing separator, which widens
    /// it: `redis.checkpoint_` must not start behaving like `redis.checkpoint`.
    #[test]
    fn prefix_entries_are_not_rewritten() {
        let matcher = SortedSetMatcher::new(["redis.checkpoint_"], true);

        // In the family the user asked for.
        assert!(matcher.matches_intake_name("redis.checkpoint_bytes"));
        assert!(
            matcher.matches_intake_name("redis.checkpoint-bytes"),
            "raw name normalizes into the family"
        );

        // Adjacent names that merely share the shorter prefix must be left alone.
        assert!(!matcher.matches_intake_name("redis.checkpointing.count"));
        assert!(!matcher.matches_intake_name("redis.checkpointed"));
    }

    /// Filterlist matching happens in the same name space the intake stores, so a metric submitted with a raw name is
    /// filtered by its normalized name.
    #[test]
    fn matching_normalizes_the_metric_name() {
        // (expected, name, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            // The submitted name needs normalizing, and the configured entry is the normalized name the user sees after
            // ingestion.
            (true, "my metric-name", &["my_metric_name"], false),
            (true, "custom.metric one", &["custom.metric_one"], false),
            (true, "host.cpu%util", &["host.cpu_util"], false),
            (true, "1app.requests", &["app.requests"], false),
            (true, "café.requests", &["caf.requests"], false),
            // Distinct raw names that normalize to the same thing are both filtered.
            (true, "multiple-norm-1", &["multiple_norm_1"], false),
            (true, "multiple_norm-1", &["multiple_norm_1"], false),
            // Entries are expected to already be normalized. A non-normalized entry matches nothing rather than being
            // rewritten, so a misconfigured entry under-filters instead of silently over-filtering.
            (false, "my_metric_name", &["my metric-name"], false),
            (false, "my metric-name", &["my-metric-name"], false),
            // Normalization must not make unrelated names collide.
            (false, "my.metric", &["my_metric"], false),
            (false, "other metric", &["my_metric"], false),
            // Prefix matching also works on the normalized name.
            (true, "custom.metric name.count", &["custom.metric_name"], true),
            (false, "custom.metric name.count", &["custom.other"], true),
            // Names the intake rejects outright never match.
            (false, "", &["foo"], false),
            (false, "123", &["foo"], false),
        ];

        for (expected, name, entries, match_prefix) in cases {
            let matcher = SortedSetMatcher::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                matcher.matches_intake_name(name),
                *expected,
                "name: {:?}, entries: {:?}, match_prefix: {}",
                name,
                entries,
                match_prefix
            );
        }
    }

    #[test]
    fn overlong_names_never_match() {
        let name = "foo".repeat(200);

        assert!(!SortedSetMatcher::new(["foo"], true).matches_intake_name(&name));
        assert!(!SortedSetMatcher::new([name.as_str()], false).matches_intake_name(&name));
    }

    #[test]
    fn matches_exact_and_prefix_entries() {
        // (expected, name, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            (false, "some", &[], false),
            (false, "some", &[], true),
            (false, "foo", &["bar", "baz"], false),
            (false, "foo", &["bar", "baz"], true),
            (false, "bar", &["foo", "baz"], false),
            (false, "bar", &["foo", "baz"], true),
            (true, "baz", &["foo", "baz"], false),
            (true, "baz", &["foo", "baz"], true),
            (false, "foobar", &["foo", "baz"], false),
            (true, "foobar", &["foo", "baz"], true),
        ];

        for (expected, name, entries, match_prefix) in cases {
            let matcher = SortedSetMatcher::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                matcher.matches_intake_name(name),
                *expected,
                "name: {:?}, entries: {:?}, match_prefix: {}",
                name,
                entries,
                match_prefix
            );
        }
    }

    /// Raw matching compares values verbatim, without normalizing them, which is what matching the raw `name` field or
    /// a tag value requires.
    #[test]
    fn raw_matching_compares_values_verbatim() {
        // (expected, value, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            // A raw value that the intake would rewrite matches only an entry spelled the same way.
            (true, "my metric-name", &["my metric-name"], false),
            (false, "my metric-name", &["my_metric_name"], false),
            // Values the intake would reject are still ordinary values here.
            (true, "", &[""], false),
            (true, "123", &["123"], false),
            (true, "🍣", &["🍣"], false),
            // Case is significant.
            (false, "Prod", &["prod"], false),
            // Prefix mode, including multi-byte prefixes and the empty prefix that matches everything.
            (true, "us-east-1", &["us-"], true),
            (false, "eu-west-1", &["us-"], true),
            (true, "🍣.roll", &["🍣"], true),
            (true, "anything", &[""], true),
            (true, "", &[""], true),
            // In exact mode, a prefix of an entry is not a match, and neither is an extension of one.
            (false, "us", &["us-east-1"], false),
            (false, "us-east-1a", &["us-east-1"], false),
            // An empty matcher matches nothing, not even the empty value.
            (false, "", &[], false),
            (false, "", &[], true),
        ];

        for (expected, value, entries, match_prefix) in cases {
            let matcher = SortedSetMatcher::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                matcher.matches(value.as_bytes()),
                *expected,
                "value: {:?}, entries: {:?}, match_prefix: {}",
                value,
                entries,
                match_prefix
            );
        }
    }

    #[test]
    fn keeps_distinct_entries_in_both_modes() {
        // (entries, match_prefix, kept entries)
        let cases: &[(&[&str], bool, &[&str])] = &[
            (&[], false, &[]),
            (&["b", "a", "b", "ab"], false, &["a", "ab", "b"]),
            (&["b", "a", "b", "ab"], true, &["a", "b"]),
            (&["", "a"], true, &[""]),
            (&["", "a"], false, &["", "a"]),
        ];
        for (entries, match_prefix, kept) in cases {
            let matcher = SortedSetMatcher::new(entries.iter().copied(), *match_prefix);
            let data = matcher.data.iter().map(|entry| entry.as_ref()).collect::<Vec<_>>();
            assert_eq!(&data, kept, "{:?}, {}", entries, match_prefix);
            assert_eq!(matcher.len(), kept.len());
        }
    }

    #[test]
    fn compacting_prefixes_removes_duplicates_and_covered_prefixes() {
        // (prefixes, compacted)
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &[]),
            (&["b."], &["b."]),
            (&["b.", "a.", "b."], &["a.", "b."]),
            (&["billing.latency.", "billing.", "billing.requests."], &["billing."]),
            // A prefix covered by an earlier one is removed even when an uncovered prefix sorts between them.
            (&["a", "a.b", "ab", "a.c"], &["a"]),
            (&["a.", "a.b", "ab", "abc", "b"], &["a.", "ab", "b"]),
            (&["", "anything"], &[""]),
            (&["🍣.roll", "🍣"], &["🍣"]),
        ];
        for (prefixes, compacted) in cases {
            let mut values = prefixes.iter().map(|prefix| prefix.to_string()).collect::<Vec<_>>();
            compact_prefixes(&mut values);
            assert_eq!(&values, compacted, "{:?}", prefixes);
        }
    }

    #[test]
    fn reports_its_mode_and_emptiness() {
        let empty = SortedSetMatcher::new(Vec::<String>::new(), true);
        assert!(empty.is_empty());
        assert!(empty.match_prefix());

        let exact = SortedSetMatcher::new(["a"], false);
        assert!(!exact.is_empty());
        assert!(!exact.match_prefix());
    }
}
