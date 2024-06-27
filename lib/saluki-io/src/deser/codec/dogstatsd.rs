use std::collections::HashSet;

use bytes::Buf;
use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::u64 as parse_u64,
    combinator::{all_consuming, map},
    error::{Error, ErrorKind},
    multi::separated_list1,
    number::complete::double,
    sequence::{preceded, separated_pair, terminated},
    IResult,
};
use saluki_context::{ContextRef, ContextResolver};
use saluki_metrics::static_metrics;
use snafu::Snafu;

use saluki_core::topology::interconnect::EventBuffer;
use saluki_event::{metric::*, Event};
use tracing::trace;

use crate::deser::Decoder;

static_metrics! {
    name => CodecMetrics,
    prefix => dogstatsd,
    metrics => [
        counter(failed_context_resolve_total),
    ]
}

enum OneOrMany<T> {
    Single(T),
    Multiple(Vec<T>),
}

/// DogStatsD codec configuration.
#[derive(Clone, Debug)]
pub struct DogstatsdCodecConfiguration {
    maximum_tag_length: usize,
    maximum_tag_count: usize,
    timestamps: bool,
}

impl DogstatsdCodecConfiguration {
    /// Sets the maximum tag length.
    ///
    /// This controls the number of bytes that are allowed for a single tag. If a tag exceeds this limit, it is
    /// truncated to the closest previous UTF-8 character boundary, in order to preserve UTF-8 validity.
    ///
    /// Defaults to no limit.
    pub fn with_maximum_tag_length(mut self, maximum_tag_length: usize) -> Self {
        self.maximum_tag_length = maximum_tag_length;
        self
    }

    /// Sets the maximum tag count.
    ///
    /// This is the maximum number of tags allowed for a single metric. If the number of tags exceeds this limit,
    /// remaining tags are simply ignored.
    ///
    /// Defaults to no limit.
    pub fn with_maximum_tag_count(mut self, maximum_tag_count: usize) -> Self {
        self.maximum_tag_count = maximum_tag_count;
        self
    }

    /// Sets whether or not timestamps are read from metrics.
    ///
    /// This is generally used in conjunction with aggregating metrics pipelines to control whether or not metrics are
    /// able to specify their own timestamp in order to be forwarded immediately without aggregation.
    ///
    /// Defaults to `true`.
    pub fn with_timestamps(mut self, timestamps: bool) -> Self {
        self.timestamps = timestamps;
        self
    }
}

impl Default for DogstatsdCodecConfiguration {
    fn default() -> Self {
        Self {
            maximum_tag_length: usize::MAX,
            maximum_tag_count: usize::MAX,
            timestamps: true,
        }
    }
}

/// A [DogStatsD][dsd] codec.
///
/// This codec is used to parse the DogStatsD protocol, which is a superset of the StatsD protocol. DogStatsD adds a
/// number of additional features, such as the ability to specify tags, send histograms directly, send service checks
/// and events (DataDog-specific), and more.
///
/// ## Missing
///
/// - Service checks and events are not currently supported.
///
/// [dsd]: https://docs.datadoghq.com/developers/dogstatsd/
#[derive(Clone, Debug)]
pub struct DogstatsdCodec {
    config: DogstatsdCodecConfiguration,
    context_resolver: ContextResolver,
    codec_metrics: CodecMetrics,
}

impl DogstatsdCodec {
    /// Creates a new `DogstatsdCodec` with the given context resolver, using a default configuration.
    pub fn from_context_resolver(context_resolver: ContextResolver) -> Self {
        Self {
            config: DogstatsdCodecConfiguration::default(),
            context_resolver,
            codec_metrics: CodecMetrics::new(),
        }
    }

    /// Sets the given configuration for the codec.
    ///
    /// Different aspects of the codec's behavior (such as tag length, tag count, and timestamp parsing) can be
    /// controlled through its configuration. See [`DogstatsdCodecConfiguration`] for more information.
    pub fn with_configuration(self, config: DogstatsdCodecConfiguration) -> Self {
        Self {
            config,
            context_resolver: self.context_resolver,
            codec_metrics: self.codec_metrics,
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ParseError {
    #[snafu(display("structural error: {}", reason))]
    Structural { reason: String },

    #[snafu(display("incomplete input (needed {} bytes)", needed))]
    IncompleteInput { needed: usize },
}

impl Decoder for DogstatsdCodec {
    type Error = ParseError;

    fn decode<B: Buf>(&mut self, buf: &mut B, events: &mut EventBuffer) -> Result<usize, Self::Error> {
        let data = buf.chunk();

        match parse_dogstatsd(data, &self.config, &self.context_resolver, &self.codec_metrics) {
            Ok((remaining, parsed_events)) => {
                buf.advance(data.len() - remaining.len());

                match parsed_events {
                    OneOrMany::Single(parsed_event) => {
                        events.push(parsed_event);
                        Ok(1)
                    }
                    OneOrMany::Multiple(parsed_events) => {
                        let events_len = parsed_events.len();
                        events.extend(parsed_events);
                        Ok(events_len)
                    }
                }
            }
            Err(e) => match e {
                // If we need more data, it's not an error, so we just break out.
                nom::Err::Incomplete(_) => unreachable!("incomplete error should not be emitted"),
                nom::Err::Error(e) | nom::Err::Failure(e) => Err(ParseError::Structural {
                    reason: format!(
                        "encountered error '{:?}' while processing message '{}'",
                        e.code,
                        String::from_utf8_lossy(data)
                    ),
                }),
            },
        }
    }
}

fn parse_dogstatsd<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration, context_resolver: &ContextResolver,
    codec_metrics: &CodecMetrics,
) -> IResult<&'a [u8], OneOrMany<Event>> {
    // We always parse the metric name and value first, where value is both the kind (counter, gauge, etc) and the
    // actual value itself.
    let (remaining, (metric_name, metric_values)) = separated_pair(metric_name, tag(":"), metric_value)(input)?;

    // At this point, we may have some of this additional data, and if so, we also then would have a pipe separator at
    // the very front, which we'd want to consume before going further.
    //
    // After that, we simply split the remaining bytes by the pipe separator, and then try and parse each chunk to see
    // if it's any of the protocol extensions we know of.
    let mut maybe_sample_rate = None;
    let mut maybe_tags = None;
    let mut maybe_container_id = None;
    let mut maybe_timestamp = None;

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;

        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            if chunk.is_empty() {
                break;
            }

            match chunk[0] {
                // Sample rate: indicates client-side sampling of this metric which will need to be "reinflated" at some
                // point downstream to calculate the true metric value.
                b'@' => {
                    let (_, sample_rate) = all_consuming(preceded(tag("@"), double))(chunk)?;
                    maybe_sample_rate = Some(sample_rate);
                }
                // Tags: additional tags to be added to the metric.
                b'#' => {
                    let (_, tags) = all_consuming(preceded(tag("#"), metric_tags(config)))(chunk)?;
                    maybe_tags = Some(tags);
                }
                // Container ID: client-provided container ID for the contaier that this metric originated from.
                b'c' if chunk.len() > 1 && chunk[1] == b':' => {
                    let (_, container_id) = all_consuming(preceded(tag("c:"), container_id))(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // Timestamp: client-provided timestamp for the metric, relative to the Unix epoch, in seconds.
                b'T' => {
                    if config.timestamps {
                        let (_, timestamp) = all_consuming(preceded(tag("T"), unix_timestamp))(chunk)?;
                        maybe_timestamp = Some(timestamp);
                    }
                }
                _ => {
                    // We don't know what this is, so we just skip it.
                    //
                    // TODO: Should we throw an error, warn, or be silently permissive?
                }
            }

            remaining = tail;
        }

        // TODO: Similarly to the above comment, should having any remaining data here cause us to throw an error, warn,
        // or be silently permissive?

        remaining
    } else {
        remaining
    };

    let tags = maybe_tags.unwrap_or_default();

    // Resolve the context now that we have the name and any tags.
    let context_ref = ContextRef::from_name_and_tags(metric_name, &tags);
    let context = match context_resolver.resolve(context_ref) {
        Some(context) => context,
        None => {
            codec_metrics.failed_context_resolve_total().increment(1);

            // We couldn't resolve the context, so we just skip this metric.
            return Ok((remaining, OneOrMany::Multiple(Vec::new())));
        }
    };

    let metric_metadata = MetricMetadata::default()
        .with_timestamp(maybe_timestamp)
        .with_sample_rate(maybe_sample_rate)
        .with_origin_entity(maybe_container_id.map(OriginEntity::container_id))
        .with_origin(MetricOrigin::dogstatsd());

    match metric_values {
        OneOrMany::Single(metric_value) => {
            let metric = Metric::from_parts(context, metric_value, metric_metadata);
            trace!(%metric, "Parsed metric.");

            Ok((remaining, OneOrMany::Single(Event::Metric(metric))))
        }
        OneOrMany::Multiple(metric_values) => {
            // TODO: This could be more efficient if we used a helper to determine if we were iterating over the last
            // element, such that we avoid the additional, unnecessary clone. `itertools` provides a helper for this.
            let metrics = metric_values
                .into_iter()
                .map(|value| {
                    let metric = Metric::from_parts(context.clone(), value, metric_metadata.clone());
                    trace!(%metric, "Parsed metric from multi-value payload.");

                    Event::Metric(metric)
                })
                .collect();

            Ok((remaining, OneOrMany::Multiple(metrics)))
        }
    }
}

#[inline]
fn split_at_delimiter(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
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
fn metric_name(input: &[u8]) -> IResult<&[u8], &str> {
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

#[inline]
fn metric_value(input: &[u8]) -> IResult<&[u8], OneOrMany<MetricValue>> {
    let (remaining, raw_value) = terminated(take_while1(|b| b != b'|'), tag("|"))(input)?;
    let (remaining, raw_kind) = alt((tag(b"g"), tag(b"c"), tag(b"ms"), tag(b"h"), tag(b"s"), tag(b"d")))(remaining)?;

    let metric_value = match raw_kind {
        b"s" => {
            let value = String::from_utf8_lossy(raw_value).to_string();
            OneOrMany::Single(MetricValue::Set {
                values: HashSet::from([value]),
            })
        }
        other => {
            // All other metric types interpret the raw value as an integer/float, so do that first so we can return
            // early due to any error from parsing.
            //
            // We try to split the value by colons, if possible, which would indicate we have a multi-value payload.
            let (_, values) = all_consuming(separated_list1(tag(b":"), double))(raw_value)?;
            if values.len() == 1 {
                OneOrMany::Single(metric_type_to_metric_value(other, values[0])?)
            } else {
                let mut metric_values = Vec::with_capacity(values.len());
                for value in values {
                    metric_values.push(metric_type_to_metric_value(other, value)?);
                }
                OneOrMany::Multiple(metric_values)
            }
        }
    };

    Ok((remaining, metric_value))
}

#[inline]
fn metric_type_to_metric_value(metric_type: &[u8], value: f64) -> Result<MetricValue, nom::Err<Error<&[u8]>>> {
    match metric_type {
        b"g" => Ok(MetricValue::Gauge { value }),
        b"c" => Ok(MetricValue::Counter { value }),
        // TODO: We're handling distributions 100% correctly, but we're taking a shortcut here by also handling
        // timers/histograms directly as distributions.
        //
        // We need to figure out if this is OK or if we need to keep them separate and only convert up at the source
        // level based on configuration or something.
        b"ms" | b"h" | b"d" => Ok(MetricValue::distribution_from_value(value)),
        _ => Err(nom::Err::Error(Error::new(metric_type, ErrorKind::Char))),
    }
}

#[inline]
fn metric_tags(config: &DogstatsdCodecConfiguration) -> impl Fn(&[u8]) -> IResult<&[u8], Vec<&str>> + '_ {
    move |input: &[u8]| {
        // Take everything that's not a control character or pipe character.
        let (remaining, mut raw_tag_bytes) = take_while1(|c: u8| !c.is_ascii_control() && c != b'|')(input)?;

        let mut tags = Vec::with_capacity(16);
        while let Some((raw_tag, tail)) = split_at_delimiter(raw_tag_bytes, b',') {
            if tags.len() >= config.maximum_tag_count {
                // We've reached the maximum number of tags, so we just skip the rest.
                break;
            }

            let tag = simdutf8::basic::from_utf8(raw_tag)
                .map(|s| limit_str_to_len(s, config.maximum_tag_length))
                .map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Char)))?;

            tags.push(tag);

            raw_tag_bytes = tail;
        }

        Ok((remaining, tags))
    }
}

#[inline]
fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

#[inline]
fn container_id(input: &[u8]) -> IResult<&[u8], &str> {
    // We generally only expect container IDs to be either long hexadecimal strings (like 64 characters), or in special
    // cases, the inode number of the cgroup controller that contains the container sending the metrics, where the value
    // will look like `in-<integer value>`.
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

#[inline]
fn limit_str_to_len(s: &str, limit: usize) -> &str {
    if limit >= s.len() {
        s
    } else {
        let sb = s.as_bytes();

        // Search through the last four bytes of the string, ending at the index `limit`, and look for the byte that
        // defines the boundary of a full UTF-8 character.
        let start = limit.saturating_sub(3);
        let new_index = sb[start..=limit]
            .iter()
            // Bit twiddling magic for checking if `b` is < 128 or >= 192.
            .rposition(|b| (*b as i8) >= -0x40);

        // SAFETY: UTF-8 characters are a maximum of four bytes, so we know we will have found a valid character
        // boundary by searching over four bytes, regardless of where the slice started.
        //
        // Similarly we know that taking everything from index 0 to the detected character boundary index will be a
        // valid UTF-8 string.
        unsafe {
            let safe_end = start + new_index.unwrap_unchecked();
            std::str::from_utf8_unchecked(&sb[..safe_end])
        }
    }
}

#[cfg(test)]
mod tests {
    use nom::IResult;
    use proptest::{collection::vec as arb_vec, prelude::*};
    use saluki_context::{ContextRef, ContextResolver};
    use saluki_event::{metric::*, Event};

    use super::{parse_dogstatsd, CodecMetrics, DogstatsdCodecConfiguration, OneOrMany};

    fn create_metric(name: &str, value: MetricValue) -> Metric {
        create_metric_with_tags(name, &[], value)
    }

    fn create_metric_with_tags(name: &str, tags: &[&str], value: MetricValue) -> Metric {
        let context_resolver: ContextResolver = ContextResolver::with_noop_interner();
        let context_ref = ContextRef::<'_, &str>::from_name_and_tags(name, tags);
        let context = context_resolver.resolve(context_ref).unwrap();

        Metric::from_parts(
            context,
            value,
            MetricMetadata::default().with_origin(MetricOrigin::dogstatsd()),
        )
    }

    fn counter(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::Counter { value })
    }

    fn counter_with_tags(name: &str, tags: &[&str], value: f64) -> Metric {
        create_metric_with_tags(name, tags, MetricValue::Counter { value })
    }

    fn gauge(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::Gauge { value })
    }

    fn distribution(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::distribution_from_value(value))
    }

    fn set(name: &str, value: &str) -> Metric {
        create_metric(
            name,
            MetricValue::Set {
                values: vec![value.to_string()].into_iter().collect(),
            },
        )
    }

    fn counter_multivalue(name: &str, values: &[f64]) -> Vec<Metric> {
        values.iter().map(|value| counter(name, *value)).collect()
    }

    fn gauge_multivalue(name: &str, values: &[f64]) -> Vec<Metric> {
        values.iter().map(|value| gauge(name, *value)).collect()
    }

    fn distribution_multivalue(name: &str, values: &[f64]) -> Vec<Metric> {
        values.iter().map(|value| distribution(name, *value)).collect()
    }

    fn parse_dogstatsd_with_default_config(input: &[u8]) -> IResult<&[u8], OneOrMany<Event>> {
        let default_config = DogstatsdCodecConfiguration::default();

        parse_dogstatsd_with_config(input, &default_config)
    }

    fn parse_dogstatsd_with_config<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> IResult<&'input [u8], OneOrMany<Event>> {
        let context_resolver = ContextResolver::with_noop_interner();
        let codec_metrics = CodecMetrics::new();

        parse_dogstatsd(input, config, &context_resolver, &codec_metrics)
    }

    #[track_caller]
    fn check_basic_metric_eq(expected: Metric, actual: OneOrMany<Event>) -> Metric {
        match actual {
            OneOrMany::Single(Event::Metric(actual)) => {
                assert_eq!(expected.context(), actual.context());
                assert_eq!(expected.value(), actual.value());
                actual
            }
            OneOrMany::Multiple(_) => unreachable!("should never be called for multi-value metric assertions"),
        }
    }

    #[track_caller]
    fn check_basic_metric_multivalue_eq(expected: Vec<Metric>, actual: OneOrMany<Event>) {
        match actual {
            OneOrMany::Single(_) => unreachable!("should never be called for single value metric assertions"),
            OneOrMany::Multiple(events) => {
                for (expected_event, actual_event) in expected.iter().zip(events.iter()) {
                    match actual_event {
                        Event::Metric(actual) => {
                            assert_eq!(expected_event.context(), actual.context());
                            assert_eq!(expected_event.value(), actual.value());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn basic_metrics() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_raw = format!("{}:{}|c", counter_name, counter_value);
        let counter_expected = counter(counter_name, counter_value);
        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());

        let gauge_name = "my.gauge";
        let gauge_value = 2.0;
        let gauge_raw = format!("{}:{}|g", gauge_name, gauge_value);
        let gauge_expected = gauge(gauge_name, gauge_value);
        let (remaining, gauge_actual) = parse_dogstatsd_with_default_config(gauge_raw.as_bytes()).unwrap();
        check_basic_metric_eq(gauge_expected, gauge_actual);
        assert!(remaining.is_empty());

        // Special case where we check this for all three variants -- timers, histograms, and distributions -- since we
        // treat them all the same when parsing.
        let distribution_name = "my.distribution";
        let distribution_value = 3.0;
        for kind in &["ms", "h", "d"] {
            let distribution_raw = format!("{}:{}|{}", distribution_name, distribution_value, kind);
            let distribution_expected = distribution(distribution_name, distribution_value);
            let (remaining, distribution_actual) =
                parse_dogstatsd_with_default_config(distribution_raw.as_bytes()).unwrap();
            check_basic_metric_eq(distribution_expected, distribution_actual);
            assert!(remaining.is_empty());
        }

        let set_name = "my.set";
        let set_value = "value";
        let set_raw = format!("{}:{}|s", set_name, set_value);
        let set_expected = set(set_name, set_value);
        let (remaining, set_actual) = parse_dogstatsd_with_default_config(set_raw.as_bytes()).unwrap();
        check_basic_metric_eq(set_expected, set_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn tags() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_tags = &["tag1", "tag2"];
        let counter_raw = format!("{}:{}|c|#{}", counter_name, counter_value, counter_tags.join(","));
        let counter_expected = counter_with_tags(counter_name, counter_tags, counter_value);

        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn sample_rate() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_sample_rate = 0.5;
        let counter_raw = format!("{}:{}|c|@{}", counter_name, counter_value, counter_sample_rate);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected.metadata_mut().set_sample_rate(counter_sample_rate);

        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn container_id() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let container_id = "abcdef123456";
        let counter_raw = format!("{}:{}|c|c:{}", counter_name, counter_value, container_id);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected
            .metadata_mut()
            .set_origin_entity(OriginEntity::container_id(container_id));

        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn unix_timestamp() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let timestamp = 1234567890;
        let counter_raw = format!("{}:{}|c|T{}", counter_name, counter_value, timestamp);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected.metadata_mut().set_timestamp(timestamp);

        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn multiple_extensions() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_sample_rate = 0.5;
        let counter_tags = &["tag1", "tag2"];
        let container_id = "abcdef123456";
        let timestamp = 1234567890;
        let counter_raw = format!(
            "{}:{}|c|#{}|@{}|c:{}|T{}",
            counter_name,
            counter_value,
            counter_tags.join(","),
            counter_sample_rate,
            container_id,
            timestamp
        );
        let mut counter_expected = counter_with_tags(counter_name, counter_tags, counter_value);
        counter_expected.metadata_mut().set_sample_rate(counter_sample_rate);
        counter_expected
            .metadata_mut()
            .set_origin_entity(OriginEntity::container_id(container_id));
        counter_expected.metadata_mut().set_timestamp(timestamp);

        let (remaining, counter_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        let counter_actual = check_basic_metric_eq(counter_expected, counter_actual);
        assert_eq!(
            counter_actual.metadata().origin_entity(),
            Some(&OriginEntity::container_id(container_id))
        );
        assert_eq!(counter_actual.metadata().timestamp(), Some(timestamp));
        assert!(remaining.is_empty());
    }

    #[test]
    fn multivalue_metrics() {
        let counter_name = "my.counter";
        let counter_values = [1.0, 2.0, 3.0];
        let counter_values_stringified = counter_values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let counter_raw = format!("{}:{}|c", counter_name, counter_values_stringified.join(":"));
        let counters_expected = counter_multivalue(counter_name, &counter_values);
        let (remaining, counters_actual) = parse_dogstatsd_with_default_config(counter_raw.as_bytes()).unwrap();
        check_basic_metric_multivalue_eq(counters_expected, counters_actual);
        assert!(remaining.is_empty());

        let gauge_name = "my.gauge";
        let gauge_values = [42.0, 5.0, -18.0];
        let gauge_values_stringified = gauge_values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let gauge_raw = format!("{}:{}|g", gauge_name, gauge_values_stringified.join(":"));
        let gauges_expected = gauge_multivalue(gauge_name, &gauge_values);
        let (remaining, gauges_actual) = parse_dogstatsd_with_default_config(gauge_raw.as_bytes()).unwrap();
        check_basic_metric_multivalue_eq(gauges_expected, gauges_actual);
        assert!(remaining.is_empty());

        // Special case where we check this for all three variants -- timers, histograms, and distributions -- since we
        // treat them all the same when parsing.
        let distribution_name = "my.distribution";
        let distribution_values = [27.5, 4.20, 80.085];
        let distribution_values_stringified = distribution_values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        for kind in &["ms", "h", "d"] {
            let distribution_raw = format!(
                "{}:{}|{}",
                distribution_name,
                distribution_values_stringified.join(":"),
                kind
            );
            let distributions_expected = distribution_multivalue(distribution_name, &distribution_values);
            let (remaining, distributions_actual) =
                parse_dogstatsd_with_default_config(distribution_raw.as_bytes()).unwrap();
            check_basic_metric_multivalue_eq(distributions_expected, distributions_actual);
            assert!(remaining.is_empty());
        }
    }

    #[test]
    fn respects_maximum_tag_count() {
        let input = "foo:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let cases = [3, 2, 1];
        for max_tag_count in cases {
            let config = DogstatsdCodecConfiguration {
                maximum_tag_count: max_tag_count,
                ..Default::default()
            };

            let (remaining, result) =
                parse_dogstatsd_with_config(input.as_bytes(), &config).expect("should not fail to parse");

            assert!(remaining.is_empty());
            match result {
                OneOrMany::Single(Event::Metric(metric)) => {
                    assert_eq!(metric.context().tags().len(), max_tag_count);
                }
                _ => unreachable!("should only have a single metric"),
            }
        }
    }

    #[test]
    fn respects_maximum_tag_length() {
        let input = "foo:1|c|#tag1:short,tag2:medium,tag3:longlong";

        let cases = [6, 5, 4];
        for max_tag_length in cases {
            let config = DogstatsdCodecConfiguration {
                maximum_tag_length: max_tag_length,
                ..Default::default()
            };

            let (remaining, result) =
                parse_dogstatsd_with_config(input.as_bytes(), &config).expect("should not fail to parse");

            assert!(remaining.is_empty());
            match result {
                OneOrMany::Single(Event::Metric(metric)) => {
                    for tag in metric.context().tags().into_iter() {
                        assert!(tag.len() <= max_tag_length);
                    }
                }
                _ => unreachable!("should only have a single metric"),
            }
        }
    }

    #[test]
    fn respects_read_timestamps() {
        let input = "foo:1|c|T1234567890";

        let no_read_timestamps_config = DogstatsdCodecConfiguration::default().with_timestamps(false);

        let (remaining, result) = parse_dogstatsd_with_config(input.as_bytes(), &no_read_timestamps_config)
            .expect("should not fail to parse");

        assert!(remaining.is_empty());
        match result {
            OneOrMany::Single(Event::Metric(metric)) => {
                assert_eq!(metric.metadata().timestamp(), None);
            }
            _ => unreachable!("should only have a single metric"),
        }
    }

    #[test]
    fn no_metrics_when_interner_full_allocations_disallowed() {
        // We're specifically testing here that when we don't allow outside allocations, we should not be able to
        // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
        // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
        // _inlined_ either, since that will get around the interner being full.
        //
        // We set our metric name to be longer than 31 bytes (the inlining limit) to ensure this.

        let default_config = DogstatsdCodecConfiguration::default();
        let context_resolver = ContextResolver::with_noop_interner().with_heap_allocations(false);
        let codec_metrics = CodecMetrics::new();

        let input = "big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let (remaining, result) = parse_dogstatsd(input.as_bytes(), &default_config, &context_resolver, &codec_metrics)
            .expect("should not fail to parse");

        assert!(remaining.is_empty());
        match result {
            OneOrMany::Multiple(metrics) => {
                assert!(metrics.is_empty());
            }
            _ => unreachable!("should be Multiple variant"),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]
        #[test]
        fn property_test_malicious_input_non_exhaustive(input in arb_vec(0..255u8, 0..1000)) {
            // We're testing that the parser is resilient to malicious input, which means that it should not panic or
            // crash when given input that's not well-formed.
            //
            // As this is a property test, it is _not_ exhaustive but generally should catch simple issues that manage
            // to escape the unit tests. This is left here for the sole reason of incrementally running this every time
            // all tests are run, in the hopes of potentially catching an issue that might have been missed.
            //
            // TODO: True exhaustive-style testing a la afl/honggfuzz.
            let _ = parse_dogstatsd_with_default_config(&input);
        }
    }
}
