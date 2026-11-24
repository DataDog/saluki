//! Metric-name normalization in the intake storage namespace.
//!
//! Metric names arrive verbatim, while the metrics intake rewrites them on ingest. Decisions that must agree with
//! stored names, such as metric filterlist matching, compare normalized names instead of raw wire names.
//!
//! Normalization never allocates. [`is_normalized`] is a single pass that skips rewriting when a name is already
//! normalized, and [`normalize_into`] rewrites into a caller-provided stack [`NameBuf`].

/// Maximum allowed length of a metric name, in bytes.
///
/// The intake rejects longer names instead of truncating them.
pub const MAX_LENGTH: usize = 350;

/// Fixed-capacity scratch buffer for a normalized metric name.
///
/// A normalized name is never longer than its input, and inputs longer than [`MAX_LENGTH`] are rejected before
/// writing, so this buffer always has enough capacity.
pub struct NameBuf {
    buf: [u8; MAX_LENGTH],
    len: usize,
}

impl Default for NameBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl NameBuf {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Self {
            buf: [0; MAX_LENGTH],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn last(&self) -> Option<u8> {
        (self.len > 0).then(|| self.buf[self.len - 1])
    }

    fn push(&mut self, b: u8) {
        debug_assert!(b.is_ascii(), "normalized metric names only contain ASCII bytes");

        self.buf[self.len] = b;
        self.len += 1;
    }

    fn overwrite_last(&mut self, b: u8) {
        debug_assert!(b.is_ascii(), "normalized metric names only contain ASCII bytes");

        self.buf[self.len - 1] = b;
    }

    fn truncate_last(&mut self) {
        assert!(self.len > 0, "cannot truncate an empty buffer");

        self.len -= 1;
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

fn is_alphanumeric(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn first_alpha(name: &str) -> Option<usize> {
    if name.is_empty() || name.len() > MAX_LENGTH {
        return None;
    }

    name.as_bytes().iter().position(|&b| is_alpha(b))
}

/// Returns whether `name` is already in normalized form.
///
/// This is a single non-allocating pass. It returns `true` if and only if [`normalize_into`] produces `name`
/// unchanged.
pub fn is_normalized(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_LENGTH {
        return false;
    }

    let bytes = name.as_bytes();

    // A normalized name always starts with an ASCII letter because leading non-letters are stripped.
    if !is_alpha(bytes[0]) {
        return false;
    }

    for i in 1..bytes.len() {
        match bytes[i] {
            b if is_alphanumeric(b) => {}
            b'.' => {}
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

/// Writes `name` into `buf` in normalized form and returns the resulting ASCII bytes.
///
/// Returns `None` when `name` is empty, longer than [`MAX_LENGTH`] bytes, or contains no ASCII letter.
///
/// The rules apply byte-wise:
///
/// 1. Discard everything before the first ASCII letter.
/// 2. Keep ASCII alphanumerics verbatim, preserving case.
/// 3. Keep periods; a period following an underscore replaces that underscore.
/// 4. Convert every other byte into an underscore, skipping underscores directly after a period or another underscore.
/// 5. Strip a trailing underscore.
pub fn normalize_into<'buf>(buf: &'buf mut NameBuf, name: &str) -> Option<&'buf [u8]> {
    let start = first_alpha(name)?;
    buf.clear();

    for &b in &name.as_bytes()[start..] {
        if is_alphanumeric(b) {
            buf.push(b);
        } else if b == b'.' {
            match buf.last() {
                Some(b'_') => buf.overwrite_last(b'.'),
                _ => buf.push(b'.'),
            }
        } else {
            match buf.last() {
                Some(b'.') | Some(b'_') => {}
                _ => buf.push(b'_'),
            }
        }
    }

    if buf.last() == Some(b'_') {
        buf.truncate_last();
    }

    Some(buf.as_bytes())
}

/// Writes `prefix` into `buf` in normalized prefix form, preserving a trailing delimiter boundary.
///
/// For example, `service_` stays `service_` instead of stripping its trailing underscore and widening to `service`.
/// Returns `None` when no stored metric name can start with `prefix`, except that the empty prefix is valid and
/// matches every stored metric name.
pub fn normalize_prefix_into<'buf>(buf: &'buf mut NameBuf, prefix: &str) -> Option<&'buf [u8]> {
    if prefix.is_empty() {
        buf.clear();
        return Some(buf.as_bytes());
    }

    normalize_into(buf, prefix)?;

    let last = prefix.as_bytes()[prefix.len() - 1];
    if !is_alphanumeric(last) && last != b'.' && buf.last() != Some(b'.') {
        buf.push(b'_');
    }

    Some(buf.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(name: &str) -> Option<String> {
        let mut buf = NameBuf::new();
        normalize_into(&mut buf, name)
            .map(|normalized| String::from_utf8(normalized.to_vec()).expect("normalized names are ASCII"))
    }

    fn normalize_prefix(prefix: &str) -> Option<String> {
        let mut buf = NameBuf::new();
        normalize_prefix_into(&mut buf, prefix)
            .map(|normalized| String::from_utf8(normalized.to_vec()).expect("normalized names are ASCII"))
    }

    const NORMALIZED_NAMES: &[(&str, &str)] = &[
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
        ("multiple-norm-1", "multiple_norm_1"),
        ("multiple_norm-1", "multiple_norm_1"),
        ("a$.b", "a.b"),
        ("a_.b", "a.b"),
        ("__init__.metric", "init.metric"),
        ("a___..b", "a..b"),
        ("a_.", "a."),
        ("a._b", "a.b"),
        ("a__b", "a_b"),
        ("a_b", "a_b"),
        ("a_", "a"),
        ("foo", "foo"),
        ("n_o_i_n_d_e_x.pct_aggr.1234", "n_o_i_n_d_e_x.pct_aggr.1234"),
        ("MyMetric.Count", "MyMetric.Count"),
        ("1app.requests", "app.requests"),
        ("...foo", "foo"),
        ("foo...bar", "foo...bar"),
        ("foo.bar.", "foo.bar."),
        ("my metric  name", "my_metric_name"),
        ("app-request-count", "app_request_count"),
        ("host.cpu%util", "host.cpu_util"),
        ("café.requests", "caf.requests"),
        ("🍣.metric", "metric"),
    ];

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
    fn prefix_normalization_preserves_boundaries() {
        let cases: &[(&str, &str)] = &[
            ("service_", "service_"),
            ("service-", "service_"),
            ("service ", "service_"),
            ("service__", "service_"),
            ("service.a-", "service.a_"),
            ("service.", "service."),
            ("service._", "service."),
            ("service-.", "service."),
            ("service", "service"),
            ("my metric.", "my_metric."),
            ("my metric", "my_metric"),
            ("MyService.Sub", "MyService.Sub"),
            ("café", "caf_"),
            ("", ""),
        ];

        for (input, expected) in cases {
            let actual = normalize_prefix(input).expect("expected usable prefix");
            assert_eq!(actual, *expected, "input: {:?}", input);
            assert_eq!(normalize_prefix(&actual), Some(actual.clone()), "input: {:?}", input);
        }
    }

    #[test]
    fn unusable_prefixes_are_rejected() {
        let over_limit = "a".repeat(MAX_LENGTH + 1);
        let cases = ["123.", "_", ".", "-", "123", over_limit.as_str()];

        for input in cases {
            assert_eq!(normalize_prefix(input), None, "input: {:?}", input);
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
        let at_limit = "a".repeat(MAX_LENGTH);
        assert_eq!(normalize(&at_limit).as_deref(), Some(at_limit.as_str()));

        let over_limit = "a".repeat(MAX_LENGTH + 1);
        assert_eq!(normalize(&over_limit), None);

        let shrinks = "a-".repeat(MAX_LENGTH);
        assert_eq!(normalize(&shrinks), None);
    }

    #[test]
    fn is_normalized_agrees_with_normalize() {
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
}
