//! Shared DogStatsD metric filterlist matcher.

use stringtheory::MetaString;

mod metric_name;

use self::metric_name::{is_normalized, normalize_into, NameBuf};

/// Compiled blocklist for metric names that should be filtered.
///
/// Entries are taken verbatim. They are expected to already be normalized, meaning they are metric names as the intake
/// stores and displays them, which is what users copy into a filterlist. [`Blocklist::contains`] normalizes the name it
/// is given, so the comparison happens in that same name space.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Blocklist {
    data: Vec<MetaString>,
    match_prefix: bool,
}

impl Blocklist {
    /// Creates a matcher from filter values and the match mode.
    pub(super) fn new<T, I>(values: I, match_prefix: bool) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let mut data = values
            .into_iter()
            .map(|value| MetaString::from(value.as_ref()))
            .collect::<Vec<_>>();
        data.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

        if match_prefix && !data.is_empty() {
            let mut i = 0;
            for j in 1..data.len() {
                if !data[j].as_ref().starts_with(data[i].as_ref()) {
                    i += 1;
                    data[i] = data[j].clone();
                }
            }
            data.truncate(i + 1);
        }

        Self { data, match_prefix }
    }

    /// Returns whether `name` matches a configured metric name.
    ///
    /// The name is normalized before being compared. Metric names arrive exactly as they were submitted, but the intake
    /// rewrites them on ingest, so a raw name such as `my metric-name` is stored -- and shown to users, and therefore
    /// configured in filterlists -- as `my_metric_name`. Matching the raw name would let those metrics through the
    /// filterlist and still have them show up in Datadog. Names the intake would reject never match.
    ///
    /// This never allocates. Names that are already normalized are compared as given, and the rest are normalized into a
    /// stack buffer. Deployments with no filterlist configured pay nothing at all, since an empty list returns before any
    /// of that.
    pub(super) fn contains(&self, name: &str) -> bool {
        if self.data.is_empty() {
            return false;
        }

        // Fast path: already normalized, so compare the name as given.
        if is_normalized(name) {
            return self.search(name.as_bytes());
        }

        let mut buf = NameBuf::new();
        match normalize_into(&mut buf, name) {
            Some(normalized) => self.search(normalized),
            None => false,
        }
    }

    /// Looks `name` up in the compiled list.
    ///
    /// `name` must already be normalized. It is taken as bytes rather than as a `&str` so that a name normalized into a
    /// [`NameBuf`] needs no UTF-8 validation pass: byte-wise ordering matches string ordering, so the search is
    /// unaffected.
    fn search(&self, name: &[u8]) -> bool {
        let i = self
            .data
            .binary_search_by(|candidate| candidate.as_ref().as_bytes().cmp(name));

        if self.match_prefix {
            let index = i.unwrap_or_else(|idx| idx);
            if index > 0 && name.starts_with(self.data[index - 1].as_ref().as_bytes()) {
                return true;
            }
        }

        i.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_entries_verbatim() {
        let compiled = |values: &[&str]| {
            Blocklist::new(values.iter().copied(), true)
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

    /// Normalizing a prefix entry as if it were a complete metric name would strip its trailing separator, which widens
    /// it: `redis.checkpoint_` must not start behaving like `redis.checkpoint`.
    #[test]
    fn prefix_entries_are_not_rewritten() {
        let blocklist = Blocklist::new(["redis.checkpoint_"], true);

        // In the family the user asked for.
        assert!(blocklist.contains("redis.checkpoint_bytes"));
        assert!(
            blocklist.contains("redis.checkpoint-bytes"),
            "raw name normalizes into the family"
        );

        // Adjacent names that merely share the shorter prefix must be left alone.
        assert!(!blocklist.contains("redis.checkpointing.count"));
        assert!(!blocklist.contains("redis.checkpointed"));
    }

    /// Filterlist matching happens in the same name space the intake stores, so a metric submitted with a raw name is
    /// filtered by its normalized name.
    #[test]
    fn matching_normalizes_the_metric_name() {
        // (expected, name, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            // The submitted name needs normalizing, and the configured entry is the normalized name the user sees in
            // Datadog.
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
            let blocklist = Blocklist::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                blocklist.contains(name),
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

        assert!(!Blocklist::new(["foo"], true).contains(&name));
        assert!(!Blocklist::new([name.as_str()], false).contains(&name));
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
            let blocklist = Blocklist::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                blocklist.contains(name),
                *expected,
                "name: {:?}, entries: {:?}, match_prefix: {}",
                name,
                entries,
                match_prefix
            );
        }
    }
}
