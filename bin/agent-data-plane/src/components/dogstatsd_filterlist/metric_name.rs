//! Metric-name normalization, in the name space the Datadog metrics intake stores names in.
//!
//! Metric names are submitted verbatim, but the intake rewrites them on ingest. Any decision made here that has to
//! agree with what the intake stores -- matching a metric filterlist, for example -- must therefore compare normalized
//! names rather than the raw names seen on the wire.
//!
//! This is a port of `pkg/util/metricname` in the Datadog Agent, which is itself a faithful port of
//! `NormMetricNameParse`/`ValidateMetricName` in dd-go (`model/metric.go`). Keep the three in sync: a divergence here
//! silently changes which metrics get filtered.
//!
//! Normalization never allocates. [`is_normalized`] is a single pass that lets callers skip the rewrite entirely for
//! the overwhelmingly common case of an already-normalized name, and [`normalize_into`] rewrites into a caller-provided
//! [`NameBuf`], which is small enough to live on the stack.

/// Maximum allowed length of a metric name, in bytes.
///
/// Names longer than this are rejected outright by the intake: they are not truncated. Mirrors `model.MaxMetricLen` in
/// dd-go.
///
/// Note that the public documentation states a 200 character limit. 350 is what the intake actually enforces, so it is
/// what we mirror here.
pub(super) const MAX_LENGTH: usize = 350;

/// Fixed-capacity scratch buffer for a normalized metric name.
///
/// A normalized name is never longer than the name it came from, and a name longer than [`MAX_LENGTH`] is rejected
/// outright, so this capacity is always enough. That is what lets [`normalize_into`] rewrite a name without allocating.
pub(super) struct NameBuf {
    buf: [u8; MAX_LENGTH],
    len: usize,
}

impl NameBuf {
    /// Creates an empty buffer.
    pub(super) fn new() -> Self {
        Self {
            buf: [0; MAX_LENGTH],
            len: 0,
        }
    }

    /// Returns the buffer contents.
    ///
    /// This is a byte slice rather than a `&str` so that no UTF-8 validation pass is needed. The contents are always
    /// valid UTF-8, since only ASCII bytes are ever written, but callers only ever compare them.
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Returns the last byte in the buffer, or `None` when the buffer is empty.
    fn last(&self) -> Option<u8> {
        (self.len > 0).then(|| self.buf[self.len - 1])
    }

    /// Appends `b` to the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is full. Callers in this module cannot trigger that: `normalize_into` pushes at most one
    /// byte per input byte, and it rejects inputs longer than [`MAX_LENGTH`] before pushing anything.
    fn push(&mut self, b: u8) {
        debug_assert!(b.is_ascii(), "normalized metric names only contain ASCII bytes");

        self.buf[self.len] = b;
        self.len += 1;
    }

    /// Replaces the last byte in the buffer with `b`.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    fn overwrite_last(&mut self, b: u8) {
        debug_assert!(b.is_ascii(), "normalized metric names only contain ASCII bytes");

        self.buf[self.len - 1] = b;
    }

    /// Removes the last byte from the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    fn truncate_last(&mut self) {
        assert!(self.len > 0, "cannot truncate an empty buffer");

        self.len -= 1;
    }

    /// Empties the buffer.
    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Returns whether `b` is an ASCII letter.
///
/// The intake works on bytes, not characters, so non-ASCII letters are deliberately not accepted here.
fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Returns whether `b` is an ASCII letter or digit.
fn is_alphanumeric(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Returns the index of the first ASCII letter in `name`.
///
/// Returns `None` when `name` is empty, longer than [`MAX_LENGTH`] bytes, or contains no ASCII letter. The intake drops
/// such names outright.
fn first_alpha(name: &str) -> Option<usize> {
    if name.is_empty() || name.len() > MAX_LENGTH {
        return None;
    }

    name.as_bytes().iter().position(|&b| is_alpha(b))
}

/// Returns whether `name` is a name the intake would store unchanged, meaning normalizing it would be the identity.
///
/// This is a single pass and never allocates, which is what lets filterlist matching skip the rewrite entirely for the
/// overwhelmingly common case of an already-normalized name. See [`super::Blocklist::contains`].
///
/// The predicate is exact: `is_normalized(s)` is true if and only if normalizing `s` yields `s` unchanged. The tests in
/// this module pin that equivalence.
pub(super) fn is_normalized(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_LENGTH {
        return false;
    }

    let bytes = name.as_bytes();

    // A normalized name always starts with an ASCII letter, because everything before the first one is stripped.
    if !is_alpha(bytes[0]) {
        return false;
    }

    for i in 1..bytes.len() {
        match bytes[i] {
            // Kept verbatim. Note that runs of periods, and a trailing period, are both legal in a normalized name.
            b if is_alphanumeric(b) => {}
            b'.' => {}

            // An underscore is only ever emitted between two alphanumerics: it is not emitted after a period or another
            // underscore, a following period overwrites it, and a trailing one is stripped.
            b'_' => {
                if !is_alphanumeric(bytes[i - 1]) {
                    return false;
                }

                if i == bytes.len() - 1 || !is_alphanumeric(bytes[i + 1]) {
                    return false;
                }
            }

            _ => return false,
        }
    }

    true
}

/// Writes `name` into `buf` as the Datadog intake stores it, and returns the normalized name as ASCII bytes.
///
/// Returns `None` when the intake would reject the name outright rather than rewrite it, which happens when the name is
/// empty, longer than [`MAX_LENGTH`] bytes, or contains no ASCII letter. Callers should treat such a name as one the
/// intake never stores.
///
/// The rules, applied byte-wise:
///
/// 1. Everything before the first ASCII letter is discarded.
/// 2. ASCII alphanumerics are kept verbatim; case is preserved.
/// 3. A period is kept, but a period following an underscore replaces it.
/// 4. Every other byte becomes an underscore, and an underscore is not emitted directly after a period or another
///    underscore. Note that this applies to literal underscores in the input too, so `a._b` becomes `a.b`.
/// 5. A trailing underscore is stripped.
///
/// Because step 4 works on bytes, each byte of a multi-byte UTF-8 sequence is treated separately and the sequence
/// collapses to a single underscore.
///
/// Normalizing is idempotent: the output always satisfies [`is_normalized`].
///
/// # Design
///
/// This writes into a caller-provided buffer rather than returning an owned string so that no caller is forced to pay
/// for the rewrite allocation. `buf` is reset first, so a single buffer can be reused across calls.
pub(super) fn normalize_into<'buf>(buf: &'buf mut NameBuf, name: &str) -> Option<&'buf [u8]> {
    let start = first_alpha(name)?;
    buf.clear();

    // The first byte written is `name[start]`, an ASCII letter, so the buffer is never empty in later iterations.
    for &b in &name.as_bytes()[start..] {
        if is_alphanumeric(b) {
            buf.push(b);
        } else if b == b'.' {
            match buf.last() {
                // Overwrite an underscore that comes directly before a period.
                Some(b'_') => buf.overwrite_last(b'.'),
                _ => buf.push(b'.'),
            }
        } else {
            match buf.last() {
                // No double underscores, and no underscore directly after a period.
                Some(b'.') | Some(b'_') => {}
                _ => buf.push(b'_'),
            }
        }
    }

    // Strip a trailing underscore. The buffer holds at least the leading letter, so it cannot become empty.
    if buf.last() == Some(b'_') {
        buf.truncate_last();
    }

    Some(buf.as_bytes())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Normalizes `name`, returning an owned string.
    ///
    /// Production code normalizes into a [`NameBuf`] instead, so this exists only to keep the assertions below
    /// readable.
    fn normalize(name: &str) -> Option<String> {
        let mut buf = NameBuf::new();
        normalize_into(&mut buf, name)
            .map(|normalized| String::from_utf8(normalized.to_vec()).expect("normalized names are ASCII"))
    }

    /// Mirrors the `testMetricNames` table in dd-go (`model/metric_test.go`), so that a divergence between the two
    /// implementations shows up as a test failure here.
    const NORMALIZED_NAMES: &[(&str, &str)] = &[
        // Bad metric names, which need remapping.
        (
            "test*&(*._-_Metrictastic*(*)(  wut_who_doesthis??",
            "test.Metrictastic_wut_who_doesthis",
        ),
        ("?does.this.work?", "does.this.work"),
        ("5-2 arsenal over spurs", "arsenal_over_spurs"),
        (
            "dd.crawler.amazon web services.run_time",
            "dd.crawler.amazon_web_services.run_time",
        ),
        // Multiple metric names that normalize to the same thing.
        ("multiple-norm-1", "multiple_norm_1"),
        ("multiple_norm-1", "multiple_norm_1"),
        // Invalid characters are dropped rather than doubled up.
        ("a$.b", "a.b"),
        ("a_.b", "a.b"),
        ("__init__.metric", "init.metric"),
        ("a___..b", "a..b"),
        ("a_.", "a."),
        // An underscore is only ever kept between two alphanumerics, so literal underscores next to a period or to each
        // other are dropped.
        ("a._b", "a.b"),
        ("a__b", "a_b"),
        ("a_b", "a_b"),
        ("a_", "a"),
        // Already normalized, so returned untouched.
        ("foo", "foo"),
        ("n_o_i_n_d_e_x.pct_aggr.1234", "n_o_i_n_d_e_x.pct_aggr.1234"),
        // Case is preserved, unlike tags.
        ("MyMetric.Count", "MyMetric.Count"),
        // Leading non-letters are stripped, not rejected.
        ("1app.requests", "app.requests"),
        ("...foo", "foo"),
        // Runs of periods, and a trailing period, survive.
        ("foo...bar", "foo...bar"),
        ("foo.bar.", "foo.bar."),
        // Spaces and punctuation collapse to a single underscore.
        ("my metric  name", "my_metric_name"),
        ("app-request-count", "app_request_count"),
        ("host.cpu%util", "host.cpu_util"),
        // Non-ASCII is handled byte-wise and collapses away.
        ("café.requests", "caf.requests"),
        ("🍣.metric", "metric"),
    ];

    /// Names the intake rejects outright rather than rewriting.
    const UNSTORABLE_NAMES: &[&str] = &["", "_", "...", "123", "🍣"];

    #[test]
    fn normalizes_names() {
        for (input, expected) in NORMALIZED_NAMES {
            assert_eq!(normalize(input).as_deref(), Some(*expected), "input: {:?}", input);
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        for (input, _) in NORMALIZED_NAMES {
            let once = normalize(input).expect("should be storable");
            let twice = normalize(&once).expect("should be storable");
            assert_eq!(once, twice, "input: {:?}", input);
        }
    }

    #[test]
    fn unstorable_names_are_rejected() {
        for input in UNSTORABLE_NAMES {
            assert_eq!(normalize(input), None, "input: {:?}", input);
        }
    }

    #[test]
    fn names_over_the_length_limit_are_rejected() {
        // A name of exactly `MAX_LENGTH` bytes is accepted.
        let at_limit = "a".repeat(MAX_LENGTH);
        assert_eq!(normalize(&at_limit).as_deref(), Some(at_limit.as_str()));

        // One byte over is rejected, and is not truncated to fit.
        let over_limit = "a".repeat(MAX_LENGTH + 1);
        assert_eq!(normalize(&over_limit), None);

        // A name that would fit only after normalization is still rejected, because the intake checks the length of the
        // raw name.
        let shrinks = "a-".repeat(MAX_LENGTH);
        assert_eq!(normalize(&shrinks), None);
    }

    #[test]
    fn is_normalized_matches_the_name_tables() {
        for (input, expected) in NORMALIZED_NAMES {
            assert_eq!(is_normalized(input), input == expected, "input: {:?}", input);
        }

        for input in UNSTORABLE_NAMES {
            assert!(!is_normalized(input), "input: {:?}", input);
        }
    }

    #[test]
    fn is_normalized_agrees_with_normalize() {
        // These cover the boundary cases the fast path in `Blocklist::contains` relies on: is_normalized(s) is true
        // exactly when normalizing `s` returns `s` unchanged.
        let inputs = [
            "",
            "_",
            ".",
            "a",
            "A",
            "a_",
            "a.",
            "a..",
            "a._b",
            "a_.b",
            "a__b",
            "a_b",
            "1",
            "1a",
            "a1",
            "a-b",
            "a b",
            "a.b",
            "a..b",
            ".a",
            "_a",
            "a_._b",
            "foo.bar.baz",
            "foo.bar.",
            "foo_bar",
            "foo__bar",
            "FOO.Bar_1",
            "café",
            "🍣",
            "a\0b",
            "a\tb",
        ];

        for input in inputs {
            assert_eq!(
                is_normalized(input),
                normalize(input).as_deref() == Some(input),
                "input: {:?}",
                input
            );
        }
    }

    /// An independent transcription of `NormMetricNameParse` in dd-go (`model/metric.go`), deliberately written as one
    /// straightforward allocating pass with no fast path.
    ///
    /// This is the oracle for the exhaustive and property tests below, so that they pin this module against dd-go's
    /// behavior rather than against itself. Keep it a transcription: if it is ever "simplified" to call the production
    /// code, it stops being an oracle.
    fn reference_normalize(name: &str) -> Option<String> {
        if name.is_empty() || name.len() > MAX_LENGTH {
            return None;
        }

        let bytes = name.as_bytes();
        let start = bytes.iter().position(|b| b.is_ascii_alphabetic())?;

        let mut res: Vec<u8> = Vec::with_capacity(name.len());
        for &c in &bytes[start..] {
            if c.is_ascii_alphanumeric() {
                res.push(c);
            } else if c == b'.' {
                if res[res.len() - 1] == b'_' {
                    let last = res.len() - 1;
                    res[last] = b'.';
                } else {
                    res.push(b'.');
                }
            } else {
                let last = res[res.len() - 1];
                if last != b'.' && last != b'_' {
                    res.push(b'_');
                }
            }
        }

        if res[res.len() - 1] == b'_' {
            res.pop();
        }

        Some(String::from_utf8(res).expect("normalized names are ASCII"))
    }

    /// Asserts every invariant the fast path depends on for one input.
    fn assert_matches_reference(name: &str) {
        let expected = reference_normalize(name);
        let actual = normalize(name);
        assert_eq!(
            actual, expected,
            "normalization disagrees with the oracle for {:?}",
            name
        );

        let Some(actual) = actual else {
            // Unstorable names must never be reported as normalized, otherwise the fast path in `Blocklist::contains`
            // would search for a name the intake would have dropped.
            assert!(
                !is_normalized(name),
                "unstorable name {:?} is reported normalized",
                name
            );
            return;
        };

        // The fast path skips the rewrite entirely for names `is_normalized` accepts, so that must imply the rewrite is
        // a no-op.
        if is_normalized(name) {
            assert_eq!(
                actual, name,
                "{:?} is reported normalized but normalizes to {:?}",
                name, actual
            );
        }

        // And the output must itself be a fixed point.
        assert!(
            is_normalized(&actual),
            "normalizing {:?} produced {:?}, which is not normalized",
            name,
            actual
        );
        assert_eq!(
            normalize(&actual).as_deref(),
            Some(actual.as_str()),
            "normalization is not idempotent for {:?}",
            name
        );
    }

    /// Checks this module against the dd-go transcription over every string up to six characters drawn from an alphabet
    /// that reaches every branch: a letter, an upper-case letter, a digit, a period, an underscore, a byte that becomes
    /// an underscore, and a multi-byte character.
    ///
    /// That is 137,257 cases, which run in a few milliseconds. Exhaustive enumeration is stronger here than sampling,
    /// because every interesting transition happens between adjacent characters.
    #[test]
    fn normalization_matches_reference_exhaustively() {
        const ALPHABET: &[char] = &['a', 'B', '1', '.', '_', '-', 'é'];
        const MAX_DEPTH: usize = 6;

        fn recurse(name: &mut String, depth: usize, checked: &mut usize) {
            assert_matches_reference(name);
            *checked += 1;

            if depth == 0 {
                return;
            }

            for c in ALPHABET {
                name.push(*c);
                recurse(name, depth - 1, checked);
                name.pop();
            }
        }

        let mut name = String::new();
        let mut checked = 0;
        recurse(&mut name, MAX_DEPTH, &mut checked);

        assert_eq!(
            checked,
            (0..=MAX_DEPTH).map(|d| ALPHABET.len().pow(d as u32)).sum::<usize>()
        );
    }

    proptest! {
        /// Checks the same invariants as the exhaustive test over longer, more varied names, including names that
        /// straddle the length limit.
        #[test]
        fn property_test_normalization_matches_reference(name in "[a-zA-Z0-9._\\-é \t\u{1F363}]{0,32}") {
            assert_matches_reference(&name);
        }

        /// Checks that names around and beyond `MAX_LENGTH` are handled consistently, since the length check guards the
        /// capacity of the no-allocation scratch buffer.
        #[test]
        fn property_test_normalization_matches_reference_at_length_limit(
            name in "[a-z.\\-]{345,355}",
        ) {
            assert_matches_reference(&name);
        }
    }
}
