//! Metric-name normalization, in the name space the metrics intake stores names in.
//!
//! Metric names are submitted verbatim, but the intake rewrites them on ingest. Any decision that has to agree with
//! what the intake stores -- matching a metric filterlist, for example -- must therefore compare normalized names
//! rather than the raw names seen on the wire. The normalized name is the **intake name**; a name the intake rejects
//! outright has none.
//!
//! Normalization never allocates. [`is_normalized`] is a single pass that lets callers skip the rewrite entirely for
//! the overwhelmingly common case of an already-normalized name, and [`normalize_into`] rewrites into a caller-provided
//! [`NameBuf`], which is small enough to live on the stack. [`intake_name`] combines the two.

/// Maximum allowed length of a metric name, in bytes. Names longer than this are rejected outright by the intake: they
/// are not truncated.
pub const MAX_LENGTH: usize = 350;

/// Fixed-capacity scratch buffer for a normalized metric name.
///
/// A normalized name is never longer than the name it came from, and a name longer than [`MAX_LENGTH`] is rejected
/// outright, so this capacity is always enough. That is what lets [`normalize_into`] rewrite a name without allocating.
pub struct NameBuf {
    buf: [u8; MAX_LENGTH],
    len: usize,
}

impl NameBuf {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Self {
            buf: [0; MAX_LENGTH],
            len: 0,
        }
    }

    /// Returns the buffer contents.
    ///
    /// This is a byte slice rather than a `&str` so that no UTF-8 validation pass is needed. The contents are always
    /// valid UTF-8, since only ASCII bytes are ever written, but callers only ever compare them.
    pub fn as_bytes(&self) -> &[u8] {
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
/// overwhelmingly common case of an already-normalized name.
///
/// The predicate is exact: `is_normalized(s)` is true if and only if normalizing `s` yields `s` unchanged. The tests in
/// this module pin that equivalence.
pub fn is_normalized(name: &str) -> bool {
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

/// Writes `name` into `buf` as the metrics intake stores it, and returns the normalized name as ASCII bytes.
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
pub fn normalize_into<'buf>(buf: &'buf mut NameBuf, name: &str) -> Option<&'buf [u8]> {
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

/// Returns the intake name of `name`: the name as the metrics intake stores it, or `None` if the intake rejects it.
///
/// A name that is already normalized, which is the overwhelmingly common case, is returned as given without being
/// copied. Any other name is normalized into `scratch`. This never allocates. Creating a [`NameBuf`] costs a 350-byte
/// initialization, so callers on a hot path keep one and reuse it, or check [`is_normalized`] first and only create one
/// when it is needed.
///
/// The result is ASCII bytes rather than a `&str` for the same reason as [`normalize_into`]: callers only compare it.
pub fn intake_name<'a>(name: &'a str, scratch: &'a mut NameBuf) -> Option<&'a [u8]> {
    if is_normalized(name) {
        return Some(name.as_bytes());
    }

    normalize_into(scratch, name)
}

#[cfg(test)]
mod tests {
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
        // These cover the boundary cases the fast path in `intake_name` relies on: is_normalized(s) is true
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

    #[test]
    fn intake_name_matches_the_name_tables() {
        let mut scratch = NameBuf::new();

        for (input, expected) in NORMALIZED_NAMES {
            assert_eq!(
                intake_name(input, &mut scratch),
                Some(expected.as_bytes()),
                "input: {:?}",
                input
            );
        }

        for input in UNSTORABLE_NAMES {
            assert_eq!(intake_name(input, &mut scratch), None, "input: {:?}", input);
        }

        let over_limit = "a".repeat(MAX_LENGTH + 1);
        assert_eq!(intake_name(&over_limit, &mut scratch), None);
    }

    #[test]
    fn intake_name_borrows_already_normalized_names() {
        // The fast path returns the input itself rather than a copy in the scratch buffer, which is what keeps the
        // common case free of any rewrite work.
        let mut scratch = NameBuf::new();
        let name = "already.normalized_name";
        let result = intake_name(name, &mut scratch).expect("name is storable");
        assert_eq!(result.as_ptr(), name.as_ptr());
        assert_eq!(result.len(), name.len());

        // A name that needs rewriting comes back from the scratch buffer instead.
        let name = "needs rewriting";
        let result = intake_name(name, &mut scratch).expect("name is storable");
        assert_eq!(result, b"needs_rewriting");
        assert_ne!(result.as_ptr(), name.as_ptr());
    }
}
