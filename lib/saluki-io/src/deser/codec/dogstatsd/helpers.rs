use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::u64 as parse_u64,
    combinator::{all_consuming, map},
    error::{Error, ErrorKind},
    sequence::{preceded, terminated},
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
use saluki_core::data_model::event::metric::{MetricValues, SampleRate};

use super::{message::CARDINALITY_PREFIX, DogstatsdCodecConfiguration, NomParserError};

/// DogStatsD metric types.
pub enum MetricType {
    Count,
    Gauge,
    Set,
    Timer,
    Histogram,
    Distribution,
}

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

#[inline]
pub fn split_at_delimiter_inclusive(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    match memchr::memchr(delimiter, input) {
        Some(index) => Some((&input[0..index], &input[index..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, &[]))
            }
        }
    }
}

#[inline]
pub fn utf8(input: &[u8]) -> IResult<&[u8], &str> {
    match simdutf8::basic::from_utf8(input) {
        Ok(s) => Ok((&[], s)),
        Err(_) => Err(nom::Err::Error(Error::new(input, ErrorKind::Verify))),
    }
}

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

#[inline]
pub fn permissive_metric_name(input: &[u8]) -> IResult<&[u8], &str> {
    // Essentially, any ASCII character that is printable and isn't `:` is allowed here.
    let valid_char = |c: u8| c > 31 && c < 128 && c != b':';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

#[inline]
pub fn raw_metric_values(input: &[u8]) -> IResult<&[u8], (MetricType, &[u8])> {
    let (remaining, raw_values) = terminated(take_while1(|b| b != b'|'), tag("|")).parse(input)?;
    let (remaining, raw_kind) = alt((tag("g"), tag("c"), tag("ms"), tag("h"), tag("s"), tag("d"))).parse(remaining)?;

    // Make sure the raw value(s) are valid UTF-8 before we use them later on.
    if raw_values.is_empty() || simdutf8::basic::from_utf8(raw_values).is_err() {
        return Err(nom::Err::Error(Error::new(raw_values, ErrorKind::Verify)));
    }

    let metric_type = match raw_kind {
        b"c" => MetricType::Count,
        b"g" => MetricType::Gauge,
        b"s" => MetricType::Set,
        b"ms" => MetricType::Timer,
        b"h" => MetricType::Histogram,
        b"d" => MetricType::Distribution,
        _ => unreachable!("should be constrained by alt parser"),
    };

    Ok((remaining, (metric_type, raw_values)))
}

#[inline]
pub fn metric_values_from_raw(
    input: &[u8], metric_type: MetricType, sample_rate: Option<SampleRate>,
) -> Result<(u64, MetricValues), NomParserError<'_>> {
    let mut num_points = 0;
    let floats = FloatIter::new(input).inspect(|_| num_points += 1);

    let values = match metric_type {
        MetricType::Count => MetricValues::counter_sampled_fallible(floats, sample_rate)?,
        MetricType::Gauge => MetricValues::gauge_fallible(floats)?,
        MetricType::Set => {
            num_points = 1;

            // SAFETY: We've already checked above that `input` is valid UTF-8.
            let value = unsafe { std::str::from_utf8_unchecked(input) };
            MetricValues::set(value.to_string())
        }
        MetricType::Timer | MetricType::Histogram => MetricValues::histogram_sampled_fallible(floats, sample_rate)?,
        MetricType::Distribution => MetricValues::distribution_sampled_fallible(floats, sample_rate)?,
    };

    Ok((num_points, values))
}

#[inline]
pub fn metric_tags(config: &DogstatsdCodecConfiguration) -> impl Fn(&[u8]) -> IResult<&[u8], RawTags<'_>> {
    let max_tag_count = config.maximum_tag_count;
    let max_tag_len = config.maximum_tag_length;

    move |input: &[u8]| match split_at_delimiter_inclusive(input, b'|') {
        Some((tags, remaining)) => match simdutf8::basic::from_utf8(tags) {
            Ok(tags) => Ok((remaining, RawTags::new(tags, max_tag_count, max_tag_len))),
            Err(_) => Err(nom::Err::Error(Error::new(input, ErrorKind::Verify))),
        },
        None => Err(nom::Err::Error(Error::new(input, ErrorKind::TakeWhile1))),
    }
}

#[inline]
pub fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

#[inline]
pub fn container_id(input: &[u8]) -> IResult<&[u8], &str> {
    // We generally only expect container IDs to be either long hexadecimal strings (like 64 characters), or in special
    // cases, the inode number of the cgroup controller that contains the container sending the metrics, where the value
    // will look like `in-<integer value>`.
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })
    .parse(input)
}

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

#[inline]
pub fn cardinality(input: &[u8]) -> IResult<&[u8], OriginTagCardinality> {
    // Cardinality is a string that can be one of the following values:
    // - "none"
    // - "low"
    // - "orchestrator"
    // - "high"
    let (remaining, cardinality) = all_consuming(preceded(
        tag(CARDINALITY_PREFIX),
        alt((tag("none"), tag("low"), tag("orchestrator"), tag("high"))),
    ))
    .parse(input)?;
    let (_, cardinality) = utf8(cardinality)?;
    let cardinality = OriginTagCardinality::try_from(cardinality)
        .map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Verify)))?;
    Ok((remaining, cardinality))
}

pub struct FloatIter<'a> {
    raw_values: &'a [u8],
}

impl<'a> FloatIter<'a> {
    pub fn new(raw_values: &'a [u8]) -> Self {
        Self { raw_values }
    }
}

impl<'a> Iterator for FloatIter<'a> {
    type Item = Result<f64, nom::Err<nom::error::Error<&'a [u8]>>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.raw_values.is_empty() {
            return None;
        }

        let (raw_value, tail) = split_at_delimiter(self.raw_values, b':')?;
        self.raw_values = tail;

        // SAFETY: The caller that creates `ValueIter` is responsible for ensuring that the entire byte slice is valid
        // UTF-8.
        let value_s = unsafe { std::str::from_utf8_unchecked(raw_value) };
        match value_s.parse::<f64>() {
            Ok(value) => Some(Ok(value)),
            Err(_) => Some(Err(nom::Err::Error(Error::new(raw_value, ErrorKind::Float)))),
        }
    }
}
