use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::u64 as parse_u64,
    combinator::{all_consuming, map},
    error::{Error, ErrorKind},
    sequence::preceded,
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};

use super::DogstatsdCodecConfiguration;

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
/// If the payload is not an event or service check, it is assumed to be a metric.
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
/// If the delimiter is not found, or the input buffer is empty, `None` is returned. Otherwise, the buffer is
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
/// If the input slice is not valid UTF-8, an error is returned.
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
/// If the input slice does not at least one byte of valid characters, an error is returned.
#[inline]
pub fn ascii_alphanum_and_seps(input: &[u8]) -> IResult<&[u8], &str> {
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b' ' || c == b'_' || c == b'-' || c == b'.';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Extracts as many raw tags from the input slice as possible, up to the configured limit.
///
/// Tags can be limited by length as well as count. If any tags exceed the maximum length, they are dropped. If the number
/// of tags exceeds the maximum count, the excess tags are dropped. The remaining slice does not contain any dropped tags.
///
/// # Errors
///
/// If the input slice is not at least one byte long, or if it is not valid UTF-8, an error is returned.
#[inline]
pub fn tags(config: &DogstatsdCodecConfiguration) -> impl Fn(&[u8]) -> IResult<&[u8], RawTags<'_>> {
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
/// If the input slice is not a valid unsigned 64-bit integer, an error is returned.
#[inline]
pub fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

/// Parses Local Data from the input slice.
///
/// # Errors
///
/// If the input slice does not contain at least one byte of valid characters, an error is returned.
#[inline]
pub fn local_data(input: &[u8]) -> IResult<&[u8], &str> {
    // Local Data is only meant to be able to represent container IDs (which arelong hexadecimal strings), or in special
    // cases, the inode number of the cgroup controller that contains the container sending the metrics, where the value
    // will look like `in-<integer value>`.
    //
    // In some cases, it might contain _multiple_ of these values, separated by a comma.
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b',';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Parses External Data from the input slice.
///
/// # Errors
///
/// If the input slice does not contain at least one byte of valid characters, an error is returned.
#[inline]
pub fn external_data(input: &[u8]) -> IResult<&[u8], &str> {
    // External Data is only meant to be able to represent origin information, which includes container names, pod UIDs,
    // and the like... which are constrained by the RFC 1123 definition of a DNS label: lowercase ASCII letters,
    // numbers, and hyphens.
    //
    // We don't go the full nine yards with enforcing the "starts with a letter and number" bit.. but we _do_ allow
    // commas since individual items in the External Data string are comma-separated.
    let valid_char = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b',';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

/// Parses `OriginTagCardinality` from the input slice.
///
/// # Errors
///
///
#[inline]
pub fn cardinality(input: &[u8]) -> IResult<&[u8], Option<OriginTagCardinality>> {
    // Cardinality is a string that can be one of the following values:
    // - "none"
    // - "low"
    // - "orchestrator"
    // - "high"
    let (remaining, raw_cardinality) = map(
        all_consuming(preceded(
            tag(CARDINALITY_PREFIX),
            alt((tag("none"), tag("low"), tag("orchestrator"), tag("high"))),
        )),
        |b| {
            // SAFETY: We know the bytes in `b` can only be comprised of UTF-8 characters, because our tags are all based on valid
            // UTF-8 strings, which ensures that it's valid to interpret the bytes directly as UTF-8.
            unsafe { std::str::from_utf8_unchecked(b) }
        },
    )
    .parse(input)?;

    OriginTagCardinality::try_from(raw_cardinality)
        .map(|cardinality| (remaining, Some(cardinality)))
        .map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Verify)))
}
