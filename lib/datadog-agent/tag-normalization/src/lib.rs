//! Metric tag key normalization, in the form the Datadog metrics intake stores tags in.
//!
//! Tags are submitted verbatim and the intake normalizes them on ingest, so a decision that has to agree with what the
//! intake stores, such as matching `metric_tag_filterlist` keys, must compare normalized keys. The specification is
//! `SPEC.md`, next to this crate's `Cargo.toml`: this crate implements its tag key K(t) ([`tag_key`]) and rule key R(k)
//! ([`rule_key`]), and the tests check the vectors in `testdata/` and the specification's properties.
//!
//! [`tag_key`] borrows the input for an already-normalized ASCII key, and otherwise writes into a caller-provided
//! [`TagBuf`], which is small enough to live on the stack.

use std::iter;

use icu_properties::{
    props::{GeneralCategory, GeneralCategoryGroup},
    CodePointMapData,
};

/// Output length, in bytes, at or after which no further code point is appended.
const MAX_LENGTH: usize = 200;

/// Largest input byte offset at which a code point is still processed.
const MAX_INPUT_OFFSET: usize = 400;

/// Longest possible output. The length check runs before appending, so the last code point, at most 4 bytes, can take
/// the output past [`MAX_LENGTH`].
const MAX_OUTPUT_LENGTH: usize = MAX_LENGTH + 3;

/// Fixed-capacity scratch buffer for a normalized tag key.
pub struct TagBuf {
    buf: [u8; MAX_OUTPUT_LENGTH],
    len: usize,
}

impl TagBuf {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Self {
            buf: [0; MAX_OUTPUT_LENGTH],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        simdutf8::basic::from_utf8(&self.buf[..self.len]).expect("only whole code points are written")
    }

    fn push(&mut self, c: char) {
        self.len += c.encode_utf8(&mut self.buf[self.len..]).len();
    }
}

impl Default for TagBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// The key of a tag, as the intake stores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TagKey<'a> {
    /// The normalized key: the normalized tag up to its first `:`, or the whole normalized tag when it has none.
    pub key: &'a str,

    /// Whether the normalized tag contains a `:`, and so has a value.
    ///
    /// When true, the first `:` of the normalized tag comes from the first `:` of the raw tag, so the raw tag's value
    /// is everything after its first `:`. When false, the intake stores a bare tag, even if the raw tag has a `:` that
    /// was cut off by the length limit.
    pub has_value: bool,
}

/// Returns the key the intake stores for `tag`.
///
/// The key may be empty, for example for `:foo` or `123:foo`, both stored as `:foo`.
pub fn tag_key<'a>(tag: &'a str, buf: &'a mut TagBuf) -> TagKey<'a> {
    if let Some(key) = normalized_ascii_key(tag) {
        return key;
    }

    let has_value = run(tag.char_indices(), buf, true);
    TagKey {
        key: buf.as_str(),
        has_value,
    }
}

/// Normalizes a configured tag key into the key the intake stores for tags with that key.
///
/// Returns `None` when no stored tag can have that key, which is when it contains `:`. Such a key must not match
/// anything. The empty key is a valid result: the intake stores `:foo` and `123:foo` with an empty key, so the rule
/// keys ``, `123` and `_` all match them.
pub fn rule_key(key: &str) -> Option<String> {
    if key.contains(':') {
        return None;
    }

    // Appending `:` normalizes `key` as the key of a tag with a value, so a trailing `_` that is part of a stored key
    // survives: `foo_:x` has the key `foo_`, but the bare tag `foo_` has the key `foo`.
    let mut buf = TagBuf::new();
    run(key.char_indices().chain(iter::once((key.len(), ':'))), &mut buf, true);
    Some(buf.as_str().to_string())
}

/// Returns whether a stored tag with the normalized key `key` can have a value.
///
/// The intake only appends the `:` that starts a value while its output is shorter than the length limit (§4 of the
/// specification), so a key that already fills it is always stored as a bare tag.
pub fn can_have_value(key: &str) -> bool {
    key.len() < MAX_LENGTH
}

/// Returns the key of `tag` without normalizing when the key is provably already normalized ASCII.
fn normalized_ascii_key(tag: &str) -> Option<TagKey<'_>> {
    let bytes = tag.as_bytes();
    match bytes.first() {
        None => {
            return Some(TagKey {
                key: "",
                has_value: false,
            })
        }
        Some(b':') => {
            return Some(TagKey {
                key: "",
                has_value: true,
            })
        }
        Some(b) if b.is_ascii_lowercase() => {}
        Some(_) => return None,
    }

    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match b {
            // A `:` this late is never emitted, and a key this long is truncated: only the state machine handles
            // either, so stop scanning.
            _ if i >= MAX_LENGTH => return None,
            b':' => {
                return Some(TagKey {
                    key: &tag[..i],
                    has_value: true,
                })
            }
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'/' | b'-' => {}
            b'_' if bytes[i - 1] != b'_' => {}
            _ => return None,
        }
    }

    (bytes.len() <= MAX_LENGTH && bytes[bytes.len() - 1] != b'_').then_some(TagKey {
        key: tag,
        has_value: false,
    })
}

/// Runs the normalization state machine over `(byte offset, code point)` pairs, writing the result into `buf`.
///
/// With `stop_at_colon`, this stops at the first `:` that would be appended and returns `true`, leaving the key in
/// `buf` without stripping a trailing `_`. Otherwise, and when no `:` is reached, it writes the whole normalized tag
/// and returns `false`. Only the tests use the whole-tag mode.
fn run(input: impl Iterator<Item = (usize, char)>, buf: &mut TagBuf, stop_at_colon: bool) -> bool {
    buf.len = 0;
    let mut last_was_underscore = false;

    for (offset, c) in input {
        if buf.len >= MAX_LENGTH || offset > MAX_INPUT_OFFSET {
            break;
        }

        // The arm order matters: a tag only starts with a letter or `:`, and the class tests apply to `c` before it
        // is lowercased.
        let out = match c {
            'a'..='z' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            ':' if stop_at_colon => return true,
            ':' => c,
            c if is_letter(c) => lower(c),
            _ if buf.len == 0 => continue,
            '.' | '/' | '-' => c,
            c if is_digit(c) => c,
            _ if !last_was_underscore => {
                buf.push('_');
                last_was_underscore = true;
                continue;
            }
            _ => continue,
        };
        buf.push(out);
        last_was_underscore = false;
    }

    if last_was_underscore {
        buf.len -= 1;
    }
    false
}

fn general_category(c: char) -> GeneralCategory {
    CodePointMapData::<GeneralCategory>::new().get(c)
}

/// Returns whether `c` is in general category L (Lu, Ll, Lt, Lm, Lo).
fn is_letter(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    GeneralCategoryGroup::Letter.contains(general_category(c))
}

/// Returns whether `c` is in general category Nd.
fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    general_category(c) == GeneralCategory::DecimalNumber
}

/// U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE: its simple lowercase mapping is `i`, but its full mapping, which
/// `char::to_lowercase` implements, is `i` followed by U+0307. It is the only code point where the two differ.
const CAPITAL_I_WITH_DOT_ABOVE: char = '\u{130}';

/// Returns the simple lowercase mapping of `c`.
fn lower(c: char) -> char {
    if c == CAPITAL_I_WITH_DOT_ABOVE {
        return 'i';
    }
    let mut lowered = c.to_lowercase();
    match (lowered.next(), lowered.next()) {
        (Some(single), None) => single,
        // Unreachable: `lowercase_is_single_char_except_capital_i_with_dot_above` checks that U+0130 is the only
        // exception, so a Rust or Unicode upgrade that adds another fails that test instead of reaching this arm.
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const VECTORS: &str = include_str!("../testdata/vectors.txt");
    const RULE_VECTORS: &str = include_str!("../testdata/rule_vectors.txt");

    /// Decodes `bytes` as a Go `for range` loop does: each byte of an invalid sequence becomes its own U+FFFD.
    fn decode(bytes: &[u8]) -> Vec<(usize, char)> {
        let mut decoded = Vec::new();
        let mut offset = 0;
        for chunk in bytes.utf8_chunks() {
            decoded.extend(chunk.valid().char_indices().map(|(i, c)| (offset + i, c)));
            offset += chunk.valid().len();
            for _ in chunk.invalid() {
                decoded.push((offset, char::REPLACEMENT_CHARACTER));
                offset += 1;
            }
        }
        decoded
    }

    /// N(t): the whole normalized tag. Production code only ever needs the key.
    fn normalize(tag: &[u8]) -> String {
        let mut buf = TagBuf::new();
        run(decode(tag).into_iter(), &mut buf, false);
        buf.as_str().to_string()
    }

    /// K(t), through the state machine only, for any bytes.
    fn key(tag: &[u8]) -> (String, bool) {
        let mut buf = TagBuf::new();
        let has_value = run(decode(tag).into_iter(), &mut buf, true);
        (buf.as_str().to_string(), has_value)
    }

    /// Parses one Go interpreted string literal from the start of `line`, advancing past it.
    fn parse_go_string(line: &mut &str) -> Vec<u8> {
        let mut chars = line
            .strip_prefix('"')
            .expect("string must start with a quote")
            .char_indices();
        let mut out = Vec::new();
        let hex = |chars: &mut std::str::CharIndices<'_>, digits: usize| {
            let s: String = chars.by_ref().take(digits).map(|(_, c)| c).collect();
            u32::from_str_radix(&s, 16).expect("valid hex escape")
        };

        loop {
            let (i, c) = chars.next().expect("unterminated string");
            match c {
                '"' => {
                    *line = &line[i + 2..];
                    return out;
                }
                // The vectors are written with Go's `strconv.QuoteToASCII`, and only use these escapes.
                '\\' => match chars.next().expect("unterminated escape").1 {
                    escape @ ('\\' | '"') => out.push(escape as u8),
                    'x' => out.push(hex(&mut chars, 2) as u8),
                    'u' => push_char(&mut out, hex(&mut chars, 4)),
                    'U' => push_char(&mut out, hex(&mut chars, 8)),
                    other => panic!("unsupported escape \\{other}: extend this parser"),
                },
                c => push_char(&mut out, c as u32),
            }
        }
    }

    fn push_char(out: &mut Vec<u8>, code_point: u32) {
        let c = char::from_u32(code_point).expect("escape is a Unicode scalar value");
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }

    enum Field {
        String(Vec<u8>),
        Reject,
    }

    /// Parses a vector file into lines of fields, dropping comments and blank lines.
    fn parse_vectors(contents: &str) -> Vec<(usize, Vec<Field>)> {
        let mut vectors = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            let mut rest = line;
            let mut fields = Vec::new();
            loop {
                rest = rest.trim_start();
                if rest.is_empty() || rest.starts_with('#') {
                    break;
                } else if rest.starts_with('"') {
                    fields.push(Field::String(parse_go_string(&mut rest)));
                } else if let Some(after) = rest.strip_prefix("reject") {
                    fields.push(Field::Reject);
                    rest = after;
                } else {
                    panic!("line {}: unexpected input {rest:?}", index + 1);
                }
            }
            if !fields.is_empty() {
                vectors.push((index + 1, fields));
            }
        }
        vectors
    }

    fn utf8(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("expected values are valid UTF-8")
    }

    /// Checks the specification's properties P1 to P7 for one input.
    fn check_properties(tag: &[u8]) -> Result<(), TestCaseError> {
        let normalized = normalize(tag);
        let (key, has_value) = key(tag);

        // P1: idempotence.
        prop_assert_eq!(normalize(normalized.as_bytes()), normalized.clone());

        // P2: the key is the prefix of the normalized tag before its first ':'.
        let expected_key = normalized.split(':').next().unwrap_or_default();
        prop_assert_eq!(key.as_str(), expected_key);
        prop_assert_eq!(has_value, normalized.contains(':'));

        // P3: normalizing first does not change the key.
        prop_assert_eq!(self::key(normalized.as_bytes()), (key.clone(), has_value));

        // P4: every stored key, including the empty key, is accepted and unchanged.
        prop_assert_eq!(rule_key(&key), Some(key.clone()));

        // P5: the fast path agrees with the state machine wherever it applies.
        if let Ok(tag) = simdutf8::basic::from_utf8(tag) {
            if let Some(fast) = normalized_ascii_key(tag) {
                prop_assert_eq!((fast.key, fast.has_value), (key.as_str(), has_value));
            }
            let mut buf = TagBuf::new();
            let actual = tag_key(tag, &mut buf);
            prop_assert_eq!((actual.key, actual.has_value), (key.as_str(), has_value));

            // P7: a rule key containing ':' is rejected.
            if tag.contains(':') {
                prop_assert_eq!(rule_key(tag), None);
            }
        }

        // P6: bounds and shape.
        prop_assert!(normalized.len() <= MAX_OUTPUT_LENGTH);
        prop_assert!(!normalized.ends_with('_'));
        prop_assert!(!key.contains(':'));
        if let Some(first) = normalized.chars().next() {
            prop_assert!(first == ':' || is_letter(first), "starts with {:?}", first);
        }

        Ok(())
    }

    #[test]
    fn tag_vectors() {
        let vectors = parse_vectors(VECTORS);
        assert!(!vectors.is_empty());

        for (line, fields) in vectors {
            let [Field::String(input), Field::String(expected_tag), Field::String(expected_key)] = &fields[..] else {
                panic!("line {line}: expected three strings");
            };
            let (expected_tag, expected_key) = (utf8(expected_tag.clone()), utf8(expected_key.clone()));

            assert_eq!(normalize(input), expected_tag, "line {line}: normalized tag");
            assert_eq!(key(input).0, expected_key, "line {line}: key");
            if let Ok(input) = simdutf8::basic::from_utf8(input) {
                assert_eq!(
                    tag_key(input, &mut TagBuf::new()).key,
                    expected_key,
                    "line {line}: tag_key"
                );
            }
            check_properties(input).unwrap_or_else(|error| panic!("line {line}: {error}"));
        }
    }

    #[test]
    fn rule_vectors() {
        let vectors = parse_vectors(RULE_VECTORS);
        assert!(!vectors.is_empty());

        for (line, fields) in vectors {
            let (input, expected) = match &fields[..] {
                [Field::String(input), Field::String(expected)] => (input, Some(utf8(expected.clone()))),
                [Field::String(input), Field::Reject] => (input, None),
                _ => panic!("line {line}: expected a string and a rule key or `reject`"),
            };
            let input = simdutf8::basic::from_utf8(input).expect("rule keys are configured strings");

            assert_eq!(rule_key(input), expected, "line {line}");
        }
    }

    #[test]
    fn lowercase_is_single_char_except_capital_i_with_dot_above() {
        // `lower` relies on U+0130 being the only code point whose full lowercase mapping, the one `char::to_lowercase`
        // implements, is not its simple one. This rechecks that against the toolchain's Unicode version.
        let multi: Vec<char> = (0..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|c| c.to_lowercase().count() != 1)
            .collect();
        assert_eq!(multi, [CAPITAL_I_WITH_DOT_ABOVE]);
    }

    #[test]
    fn fast_path_covers_common_keys() {
        // These must not need the state machine, or the filter's hot path pays for it on every tag.
        for (tag, key, has_value) in [
            ("env:prod", "env", true),
            ("service:web-1", "service", true),
            ("kube_namespace:Default", "kube_namespace", true),
            ("availability-zone:US-EAST-1A", "availability-zone", true),
            ("bare_tag", "bare_tag", false),
            (":value", "", true),
            ("", "", false),
        ] {
            assert_eq!(
                normalized_ascii_key(tag),
                Some(TagKey { key, has_value }),
                "tag: {tag:?}"
            );
        }

        for tag in ["Env:prod", "1env:prod", "my tag:x", "a__b:x", "a_", "caf\u{e9}:x"] {
            assert_eq!(normalized_ascii_key(tag), None, "tag: {tag:?}");
        }
    }

    fn interesting_char() -> impl Strategy<Value = char> {
        prop_oneof![
            4 => prop::sample::select(vec!['a', 'Z', ':', '_', '.', '/', '-', '1', ' ', '!']),
            1 => prop::sample::select(vec![
                '\u{e9}', '\u{c9}', '\u{301}', '\u{130}', '\u{212a}', '\u{23a}', '\u{3a3}', '\u{663}', '\u{b2}',
                '\u{2163}', '\u{fffd}', '\u{10400}', '\u{1f363}',
            ]),
            1 => any::<char>(),
        ]
    }

    proptest! {
        #[test]
        fn property_test_spec_holds_for_any_bytes(tag in prop::collection::vec(any::<u8>(), 0..512)) {
            check_properties(&tag)?;
        }

        #[test]
        fn property_test_spec_holds_for_tag_like_strings(
            chars in prop::collection::vec(interesting_char(), 0..260),
        ) {
            let tag: String = chars.into_iter().collect();
            check_properties(tag.as_bytes())?;
        }
    }
}
