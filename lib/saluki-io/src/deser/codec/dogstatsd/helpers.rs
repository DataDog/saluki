use nom::{
    bytes::complete::{tag, take_while1},
    character::complete::u64 as parse_u64,
    combinator::{all_consuming, map, rest},
    error::{Error, ErrorKind},
    sequence::preceded,
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};

use super::DogStatsDCodecConfiguration;

/// DogStatsD message type.
#[derive(Eq, PartialEq)]
pub enum MessageType {
    MetricSample,
    Event,
    ServiceCheck,
}

pub const EVENT_PREFIX: &[u8] = b"_e{";
pub const SERVICE_CHECK_PREFIX: &[u8] = b"_sc|";

pub const TIMESTAMP_PREFIX: &[u8] = b"d:";
pub const HOSTNAME_PREFIX: &[u8] = b"h:";
pub const AGGREGATION_KEY_PREFIX: &[u8] = b"k:";
pub const PRIORITY_PREFIX: &[u8] = b"p:";
pub const SOURCE_TYPE_PREFIX: &[u8] = b"s:";
pub const ALERT_TYPE_PREFIX: &[u8] = b"t:";
pub const TAGS_PREFIX: &[u8] = b"#";
pub const SERVICE_CHECK_MESSAGE_PREFIX: &[u8] = b"m:";
pub const LOCAL_DATA_PREFIX: &[u8] = b"c:";
pub const EXTERNAL_DATA_PREFIX: &[u8] = b"e:";
pub const CARDINALITY_PREFIX: &[u8] = b"card:";

/// Parses the given raw payload and returns the DogStatsD message type.
///
/// If the payload isn't an event or service check, it's assumed to be a metric.
#[inline]
pub fn parse_message_type(data: &[u8]) -> MessageType {
    if data.starts_with(EVENT_PREFIX) {
        return MessageType::Event;
    } else if data.starts_with(SERVICE_CHECK_PREFIX) {
        return MessageType::ServiceCheck;
    }
    MessageType::MetricSample
}

/// Splits the input buffer at the given delimiter.
///
/// If the delimiter isn't found, or the input buffer is empty, `None` is returned. Otherwise, the buffer is
/// split into two parts at the delimiter, and the delimiter is _not_ included.
#[inline]
pub fn split_at_delimiter(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    match memchr::memchr(delimiter, input) {
        Some(index) => Some((&input[0..index], &input[index + 1..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, &[]))
            }
        }
    }
}

/// Maps the input slice as a UTF-8 string.
///
/// # Errors
///
/// If the input slice isn't valid UTF-8, an error is returned.
#[inline]
pub fn utf8(input: &[u8]) -> IResult<&[u8], &str> {
    match simdutf8::basic::from_utf8(input) {
        Ok(s) => Ok((&[], s)),
        Err(_) => Err(nom::Err::Error(Error::new(input, ErrorKind::Verify))),
    }
}

/// Returns the longest input slice that contains only ASCII alphanumeric characters and "separators" as a UTF-8 string.
///
/// Separators are defined as spaces, underscores, hyphens, and periods.
///
/// # Errors
///
/// If the input slice doesn't at least one byte of valid characters, an error is returned.
#[inline]
pub fn ascii_alphanum_and_seps(input: &[u8]) -> IResult<&[u8], &str> {
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b' ' || c == b'_' || c == b'-' || c == b'.';
    map(take_while1(valid_char), |b: &[u8]| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        saluki_antithesis::always!(
            b.is_ascii(),
            "DogStatsD name bytes are ASCII before unchecked UTF-8 conversion"
        );
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Extracts as many raw tags from the input slice as possible, up to the configured limit.
///
/// Tags can be limited by length as well as count. If any tags exceed the maximum length, they're dropped. If the number
/// of tags exceeds the maximum count, the excess tags are dropped. The remaining slice doesn't contain any dropped tags.
///
/// # Errors
///
/// If the input slice isn't at least one byte long, or if it's not valid UTF-8, an error is returned.
#[inline]
pub fn tags(config: &DogStatsDCodecConfiguration) -> impl Fn(&[u8]) -> IResult<&[u8], RawTags<'_>> {
    let max_tag_count = config.maximum_tag_count;
    let max_tag_len = config.maximum_tag_length;

    move |input| match simdutf8::basic::from_utf8(input) {
        Ok(tags) => Ok((&[], RawTags::new(tags, max_tag_count, max_tag_len))),
        Err(_) => Err(nom::Err::Error(Error::new(input, ErrorKind::Verify))),
    }
}

/// Parses a Unix timestamp from the input slice.
///
/// # Errors
///
/// If the input slice isn't a valid unsigned 64-bit integer, an error is returned.
#[inline]
pub fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

/// Parses Local Data from the input slice.
///
/// # Errors
///
/// If the input slice doesn't contain at least one byte of valid characters, an error is returned.
#[inline]
pub fn local_data(input: &[u8]) -> IResult<&[u8], &str> {
    // Local Data is only meant to be able to represent container IDs (which arelong hexadecimal strings), or in special
    // cases, the inode number of the cgroup controller that contains the container sending the metrics, where the value
    // will look like `in-<integer value>`.
    //
    // In some cases, it might contain _multiple_ of these values, separated by a comma.
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b',';
    map(take_while1(valid_char), |b: &[u8]| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        saluki_antithesis::always!(
            b.is_ascii(),
            "DogStatsD local-data bytes are ASCII before unchecked UTF-8 conversion"
        );
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Parses External Data from the input slice.
///
/// # Errors
///
/// If the input slice doesn't contain at least one byte of valid characters, an error is returned.
#[inline]
pub fn external_data(input: &[u8]) -> IResult<&[u8], &str> {
    // External Data is only meant to be able to represent origin information, which includes container names, pod UIDs,
    // and the like... which are constrained by the RFC 1123 definition of a DNS label: lowercase ASCII letters,
    // numbers, and hyphens.
    //
    // We don't go the full nine yards with enforcing the "starts with a letter and number" bit.. but we _do_ allow
    // commas since individual items in the External Data string are comma-separated.
    let valid_char = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b',';
    map(take_while1(valid_char), |b: &[u8]| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        saluki_antithesis::always!(
            b.is_ascii(),
            "DogStatsD external-data bytes are ASCII before unchecked UTF-8 conversion"
        );
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Parses `OriginTagCardinality` from the input slice.
///
/// Unknown cardinality values are accepted and returned as `None` rather than failing the parse.
/// This matches the behavior of the core Datadog Agent, which silently ignores unrecognized values.
#[inline]
pub fn cardinality(input: &[u8]) -> IResult<&[u8], Option<OriginTagCardinality>> {
    let (remaining, raw_bytes) = all_consuming(preceded(tag(CARDINALITY_PREFIX), rest)).parse(input)?;

    // Use simdutf8 (consistent with other UTF-8 checks in this codec) for checked conversion.
    // Non-UTF-8 bytes are treated as an unrecognized value — return None so the frame continues
    // processing rather than hard-failing.
    let cardinality = simdutf8::basic::from_utf8(raw_bytes)
        .ok()
        .and_then(|s| OriginTagCardinality::try_from(s).ok());

    Ok((remaining, cardinality))
}

#[cfg(test)]
mod tests {
    use saluki_context::origin::OriginTagCardinality;

    use super::{cardinality, CARDINALITY_PREFIX};

    fn card(s: &str) -> Vec<u8> {
        format!("{}{}", simdutf8::basic::from_utf8(CARDINALITY_PREFIX).unwrap(), s).into_bytes()
    }

    #[test]
    fn cardinality_known_values() {
        let cases = [
            ("none", Some(OriginTagCardinality::None)),
            ("low", Some(OriginTagCardinality::Low)),
            ("orchestrator", Some(OriginTagCardinality::Orchestrator)),
            ("high", Some(OriginTagCardinality::High)),
        ];
        for (value, expected) in cases {
            let (_, result) = cardinality(&card(value)).expect("parse should succeed");
            assert_eq!(result, expected, "failed for '{}'", value);
        }
    }

    #[test]
    fn cardinality_unknown_value_returns_none() {
        // An unrecognized value should parse successfully and return None rather than
        // failing the parse and dropping the whole metric frame.
        let (_, result) = cardinality(&card("not-a-valid-cardinality")).expect("parse should succeed");
        assert_eq!(result, None);
    }

    #[test]
    fn cardinality_case_insensitive() {
        // Matching is case-insensitive to align with the core Datadog Agent (StringToTagCardinality
        // uses strings.ToLower). Wrong-case values should resolve to the correct cardinality.
        let cases = [
            ("LOW", Some(OriginTagCardinality::Low)),
            ("HIGH", Some(OriginTagCardinality::High)),
            ("Orchestrator", Some(OriginTagCardinality::Orchestrator)),
            ("NONE", Some(OriginTagCardinality::None)),
        ];
        for (value, expected) in cases {
            let (_, result) = cardinality(&card(value)).expect("parse should succeed");
            assert_eq!(result, expected, "failed for '{}'", value);
        }
    }

    #[test]
    fn cardinality_non_utf8_bytes_returns_none() {
        // Non-UTF-8 bytes after the prefix must not invoke undefined behavior; they should
        // be treated as an unrecognized value and return None. This is the bug that was fixed:
        // the previous implementation used from_utf8_unchecked which would cause UB here.
        let mut input = CARDINALITY_PREFIX.to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        let (_, result) = cardinality(&input).expect("parse should succeed");
        assert_eq!(result, None);
    }
}
