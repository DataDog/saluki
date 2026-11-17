//! The format of a v0.x trace payload, read from its `Content-Type` as the trace-agent reads it.
//!
//! The trace-agent takes the media type from `getMediaType` (`pkg/trace/api/api.go`), which is Go's
//! `mime.ParseMediaType` (`mime/mediatype.go`), except that a value it can't parse, an absent one included, reads as
//! `application/json`. Parsing lowercases the type, trims whitespace around it, and checks the parameters after it, so
//! `Application/MsgPack ; charset=utf-8` is MessagePack while `application/msgpack; charset` (a parameter without a
//! value) is JSON. Its quirks are kept on purpose:
//!
//! - Lowercasing is Unicode's, so `K` (U+212A, the Kelvin sign) reads as `k` and `İ` (U+0130) as `i`.
//! - Whitespace is Unicode's, so a no-break space around the type is trimmed.
//! - A trailing `;` is allowed, and a parameter given twice is an error only if its values differ.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use http::header::CONTENT_TYPE;
use http::HeaderMap;

/// How a v0.x trace payload is decoded, as `decodeConvertedTracerPayload` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PayloadFormat {
    /// `application/msgpack`.
    MessagePack,
    /// `application/json` or `text/json`, or a `Content-Type` that doesn't parse.
    Json,
    /// Any other media type: JSON if it decodes as JSON, and MessagePack otherwise.
    Unknown,
}

/// Returns the format of a request's payload, from its first `Content-Type`.
pub(super) fn payload_format(headers: &HeaderMap) -> PayloadFormat {
    let content_type = headers
        .get(CONTENT_TYPE)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    match parse_media_type(content_type).as_deref() {
        Some("application/msgpack") => PayloadFormat::MessagePack,
        Some("application/json" | "text/json") | None => PayloadFormat::Json,
        Some(_) => PayloadFormat::Unknown,
    }
}

/// Returns the media type of a `Content-Type` value, or `None` if Go's `mime.ParseMediaType` would fail on it.
///
/// Header values are bytes, and Go reads them as such: invalid UTF-8 fails in the type, and is taken as it is in a
/// quoted parameter value.
fn parse_media_type(value: &[u8]) -> Option<String> {
    let base_len = value.iter().position(|&c| c == b';').unwrap_or(value.len());
    let (base, mut params) = value.split_at(base_len);

    // `strings.TrimSpace(strings.ToLower(base))`. Invalid UTF-8 would lowercase to U+FFFD, which isn't a token.
    let base = simdutf8::basic::from_utf8(base).ok()?;
    let media_type: String = base.chars().map(simple_lowercase).collect();
    let media_type = media_type.trim_matches(char::is_whitespace);
    check_media_type(media_type.as_bytes())?;

    // Go keeps RFC 2231 continuations (`name*0`) in maps of their own, which only matters for duplicates of the exact
    // same name, as here. The map is std's, whose hashes are randomized, as Go's are: the names come from the client.
    let mut seen: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    loop {
        params = trim_left_space(params);
        if params.is_empty() {
            break;
        }
        let Some((name, value, rest)) = consume_media_param(params) else {
            // A trailing `;` isn't an error.
            let rest = params.strip_prefix(b";")?;
            if trim_left_space(rest).is_empty() {
                break;
            }
            return None;
        };
        match seen.entry(name) {
            Entry::Occupied(existing) if *existing.get() != value => return None,
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(value);
            }
        }
        params = rest;
    }

    Some(media_type.to_owned())
}

/// Go's `unicode.ToLower`, a simple case mapping: the only character whose full mapping differs is `İ`, which maps to
/// `i` rather than `i` and a combining dot.
fn simple_lowercase(c: char) -> char {
    match c {
        '\u{0130}' => 'i',
        c => {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(lower), None) => lower,
                _ => c,
            }
        }
    }
}

/// Go's `checkMediaTypeDisposition`: a token, optionally followed by `/` and another token.
fn check_media_type(value: &[u8]) -> Option<()> {
    let (kind, rest) = consume_token(value);
    if kind.is_empty() {
        return None;
    }
    if rest.is_empty() {
        return Some(());
    }
    let (subtype, rest) = consume_token(rest.strip_prefix(b"/")?);
    (!subtype.is_empty() && rest.is_empty()).then_some(())
}

/// Go's `consumeMediaParam`: `; name=value`, returning the lowercased name, the value and what follows.
fn consume_media_param(value: &[u8]) -> Option<(Vec<u8>, Vec<u8>, &[u8])> {
    let rest = trim_left_space(value).strip_prefix(b";")?;
    let (name, rest) = consume_token(trim_left_space(rest));
    if name.is_empty() {
        return None;
    }
    let rest = trim_left_space(trim_left_space(rest).strip_prefix(b"=")?);
    let (value, after) = consume_value(rest)?;
    Some((name.to_ascii_lowercase(), value, after))
}

/// Go's `consumeValue`: a token, or a quoted string. Returns `None` where Go returns an empty value with nothing
/// consumed.
fn consume_value(value: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if value.first() != Some(&b'"') {
        let (token, rest) = consume_token(value);
        return (!token.is_empty()).then(|| (token.to_vec(), rest));
    }
    let mut unquoted = Vec::new();
    let mut i = 1;
    while i < value.len() {
        match value[i] {
            b'"' => return Some((unquoted, &value[i + 1..])),
            // An escape is only one before a special character; otherwise the backslash is kept, as MSIE sends paths.
            b'\\' if value.get(i + 1).copied().is_some_and(is_tspecial) => {
                unquoted.push(value[i + 1]);
                i += 1;
            }
            b'\r' | b'\n' => return None,
            c => unquoted.push(c),
        }
        i += 1;
    }
    None
}

fn consume_token(value: &[u8]) -> (&[u8], &[u8]) {
    let len = value.iter().position(|&c| !is_token_char(c)).unwrap_or(value.len());
    value.split_at(len)
}

/// Go's `strings.TrimLeftFunc(v, unicode.IsSpace)` on bytes: invalid UTF-8 isn't whitespace.
fn trim_left_space(mut value: &[u8]) -> &[u8] {
    while let Some(&first) = value.first() {
        let width = match first {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        let space = value
            .get(..width)
            .and_then(|c| simdutf8::basic::from_utf8(c).ok())
            .is_some_and(|c| c.chars().all(char::is_whitespace));
        if !space {
            break;
        }
        value = &value[width..];
    }
    value
}

/// A token character of RFC 1521: ASCII, but not a control, a space or a special character.
fn is_token_char(c: u8) -> bool {
    c.is_ascii_graphic() && !is_tspecial(c)
}

fn is_tspecial(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'@' | b',' | b';' | b':' | b'\\' | b'"' | b'/' | b'[' | b']' | b'?' | b'='
    )
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn format_of(content_type: Option<&[u8]>) -> PayloadFormat {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(CONTENT_TYPE, HeaderValue::from_bytes(content_type).unwrap());
        }
        payload_format(&headers)
    }

    #[test]
    fn content_types_select_the_format_as_get_media_type_does() {
        use PayloadFormat::*;

        // Each value was checked against the trace-agent's `getMediaType` (Go 1.26.11).
        let cases: &[(&[u8], PayloadFormat)] = &[
            (b"", Json),
            (b"application/json", Json),
            (b"text/json", Json),
            (b"TEXT/JSON", Json),
            (b"application/msgpack", MessagePack),
            (b"Application/MsgPack ", MessagePack),
            (b"application/msgpack; charset=utf-8", MessagePack),
            (b"application/msgpack;", MessagePack),
            (b"application/msgpack; a=1; a=1", MessagePack),
            (br#"application/msgpack; a="x\"y""#, MessagePack),
            ("application/msgpac\u{212a}".as_bytes(), MessagePack),
            ("appl\u{0130}cation/msgpack".as_bytes(), MessagePack),
            ("\u{00a0}application/msgpack".as_bytes(), MessagePack),
            (b"application/msgpack; a=\"\xff\"", MessagePack),
            // Values that don't parse read as JSON.
            (b"application/msgpack; ;", Json),
            (b"application/msgpack; a", Json),
            (b"application/msgpack; a=1; a=2", Json),
            (b"application/msgpack; a*0=1; a*0=2", Json),
            (br#"application/msgpack; a="unterminated"#, Json),
            (b"application /msgpack", Json),
            (b"application/msgpack/x", Json),
            (b"application/msgpack, text/json", Json),
            (b"applic\xffation/msgpack", Json),
            // Other media types are decoded as JSON, or else as MessagePack.
            (b"application/x-msgpack", Unknown),
            (b"application/vnd.msgpack", Unknown),
            (b"text/plain", Unknown),
            (b"msgpack", Unknown),
            (b"*/*", Unknown),
        ];
        for (content_type, expected) in cases {
            assert_eq!(
                format_of(Some(content_type)),
                *expected,
                "{:?}",
                String::from_utf8_lossy(content_type)
            );
        }
        assert_eq!(format_of(None), Json);
    }

    #[test]
    fn a_long_parameter_list_parses_in_linear_time() {
        // A list this long took seconds of CPU when duplicates were found by a linear search, before any body is read.
        let mut value = b"application/msgpack".to_vec();
        for i in 0..100_000 {
            value.extend_from_slice(format!("; p{i}=v").as_bytes());
        }
        let start = std::time::Instant::now();
        assert_eq!(parse_media_type(&value).as_deref(), Some("application/msgpack"));
        let elapsed = start.elapsed();
        // Loose enough for a loaded CI runner: the quadratic search took far longer than either.
        let bound = if cfg!(debug_assertions) {
            std::time::Duration::from_secs(10)
        } else {
            std::time::Duration::from_secs(1)
        };
        assert!(
            elapsed < bound,
            "{elapsed:?} for {} bytes (bound {bound:?})",
            value.len()
        );
    }

    #[test]
    fn duplicate_parameters_are_checked_by_name_and_value() {
        assert_eq!(parse_media_type(b"text/plain; A=1; a=1").as_deref(), Some("text/plain"));
        assert_eq!(parse_media_type(b"text/plain; a=1; A=2"), None);
        assert_eq!(
            parse_media_type(br#"text/plain; a=""; b=x"#).as_deref(),
            Some("text/plain")
        );
        assert_eq!(parse_media_type(b"text/plain; a=\"x\ny\""), None);
        assert_eq!(
            parse_media_type(br#"text/plain; a="C:\dev""#).as_deref(),
            Some("text/plain")
        );
    }
}
