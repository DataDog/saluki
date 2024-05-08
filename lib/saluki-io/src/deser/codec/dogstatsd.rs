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
    IResult, InputTakeAtPosition as _,
};
use snafu::Snafu;

use saluki_core::{constants::internal::CONTAINER_ID_TAG_KEY, topology::interconnect::EventBuffer};
use saluki_env::time::get_unix_timestamp;
use saluki_event::{metric::*, Event};

use crate::deser::Decoder;

enum OneOrMany<T> {
    Single(T),
    Multiple(Vec<T>),
}

#[derive(Debug, Default)]
pub struct DogstatsdCodec;

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

        match parse_dogstatsd(data) {
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
                        String::from_utf8_lossy(e.input)
                    ),
                }),
            },
        }
    }
}

fn parse_dogstatsd(input: &[u8]) -> IResult<&[u8], OneOrMany<Event>> {
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

        while let Ok((tail, chunk)) = remaining.split_at_position_complete::<_, ()>(|b| b == b'|') {
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
                    let (_, tags) = all_consuming(preceded(tag("#"), metric_tags))(chunk)?;
                    maybe_tags = Some(tags);
                }
                // Container ID: client-provided container ID for the contaier that this metric originated from.
                b'c' if chunk.len() > 1 && chunk[1] == b':' => {
                    let (_, container_id) = all_consuming(preceded(tag("c:"), container_id))(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // Timestamp: client-provided timestamp for the metric, relative to the Unix epoch, in seconds.
                b'T' => {
                    let (_, timestamp) = all_consuming(preceded(tag("T"), unix_timestamp))(chunk)?;
                    maybe_timestamp = Some(timestamp);
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

    let timestamp = maybe_timestamp.unwrap_or_else(get_unix_timestamp);
    let mut tags = maybe_tags.unwrap_or_default();

    // We add the container ID as a special tag if it was provided.
    if let Some(container_id) = maybe_container_id {
        tags.insert_tag(format!("{}:{}", CONTAINER_ID_TAG_KEY, container_id));
    }

    let metric_context = MetricContext {
        name: metric_name.to_string(),
        tags,
    };
    let metric_metadata = MetricMetadata::from_timestamp(timestamp)
        .with_sample_rate(maybe_sample_rate)
        .with_origin(MetricOrigin::dogstatsd());

    match metric_values {
        OneOrMany::Single(metric_value) => Ok((
            remaining,
            OneOrMany::Single(Event::Metric(Metric {
                context: metric_context,
                value: metric_value,
                metadata: metric_metadata,
            })),
        )),
        OneOrMany::Multiple(metric_values) => {
            // TODO: This could be more efficient if we used a helper to determine if we were iterating over the last
            // element, such that we avoid the additional, unnecessary clone. `itertools` provides a helper for this.
            let metrics = metric_values
                .into_iter()
                .map(|value| {
                    Event::Metric(Metric {
                        context: metric_context.clone(),
                        value,
                        metadata: metric_metadata.clone(),
                    })
                })
                .collect();

            Ok((remaining, OneOrMany::Multiple(metrics)))
        }
    }
}

fn metric_name(input: &[u8]) -> IResult<&[u8], &str> {
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

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

fn metric_tags(input: &[u8]) -> IResult<&[u8], MetricTags> {
    // Take every that's not a control character or pipe character.
    let (remaining, raw_tag_bytes) = take_while1(|c: u8| !c.is_ascii_control() && c != b'|')(input)?;

    let mut tags = MetricTags::default();
    for raw_tag in raw_tag_bytes.split(|c| *c == b',') {
        let tag = std::str::from_utf8(raw_tag).map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Char)))?;

        tags.insert_tag(tag);
    }

    Ok((remaining, tags))
}

fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

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

#[cfg(test)]
mod tests {
    use proptest::{collection::vec as arb_vec, prelude::*};
    use saluki_core::constants::internal::CONTAINER_ID_TAG_KEY;
    use saluki_event::{metric::*, Event};

    use super::{parse_dogstatsd, OneOrMany};

    fn create_metric(name: &str, value: MetricValue) -> Metric {
        Metric {
            context: MetricContext {
                name: name.to_string(),
                tags: MetricTags::default(),
            },
            value,
            metadata: MetricMetadata::from_timestamp(0).with_origin(MetricOrigin::dogstatsd()),
        }
    }

    fn counter(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::Counter { value })
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

    fn check_basic_metric_eq(expected: Metric, actual: OneOrMany<Event>) {
        match actual {
            OneOrMany::Single(Event::Metric(actual)) => {
                assert_eq!(expected.context, actual.context);
                assert_eq!(expected.value, actual.value);
            }
            OneOrMany::Multiple(_) => unreachable!("should never be called for multi-value metric assertions"),
        }
    }

    fn check_basic_metric_multivalue_eq(expected: Vec<Metric>, actual: OneOrMany<Event>) {
        match actual {
            OneOrMany::Single(_) => unreachable!("should never be called for single value metric assertions"),
            OneOrMany::Multiple(events) => {
                for (expected_event, actual_event) in expected.iter().zip(events.iter()) {
                    match actual_event {
                        Event::Metric(actual) => {
                            assert_eq!(expected_event.context, actual.context);
                            assert_eq!(expected_event.value, actual.value);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_basic_metrics() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_raw = format!("{}:{}|c", counter_name, counter_value);
        let counter_expected = counter(counter_name, counter_value);
        let (remaining, counter_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());

        let gauge_name = "my.gauge";
        let gauge_value = 2.0;
        let gauge_raw = format!("{}:{}|g", gauge_name, gauge_value);
        let gauge_expected = gauge(gauge_name, gauge_value);
        let (remaining, gauge_actual) = parse_dogstatsd(gauge_raw.as_bytes()).unwrap();
        check_basic_metric_eq(gauge_expected, gauge_actual);
        assert!(remaining.is_empty());

        // Special case where we check this for all three variants -- timers, histograms, and distributions -- since we
        // treat them all the same when parsing.
        let distribution_name = "my.distribution";
        let distribution_value = 3.0;
        for kind in &["ms", "h", "d"] {
            let distribution_raw = format!("{}:{}|{}", distribution_name, distribution_value, kind);
            let distribution_expected = distribution(distribution_name, distribution_value);
            let (remaining, distribution_actual) = parse_dogstatsd(distribution_raw.as_bytes()).unwrap();
            check_basic_metric_eq(distribution_expected, distribution_actual);
            assert!(remaining.is_empty());
        }

        let set_name = "my.set";
        let set_value = "value";
        let set_raw = format!("{}:{}|s", set_name, set_value);
        let set_expected = set(set_name, set_value);
        let (remaining, set_actual) = parse_dogstatsd(set_raw.as_bytes()).unwrap();
        check_basic_metric_eq(set_expected, set_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_tags() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_tags = &["tag1", "tag2"];
        let counter_raw = format!("{}:{}|c|#{}", counter_name, counter_value, counter_tags.join(","));
        let mut counter_expected = counter(counter_name, counter_value);
        for tag in counter_tags {
            counter_expected.context.tags.insert_tag(tag.to_string());
        }

        let (remaining, counter_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_sample_rate() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let counter_sample_rate = 0.5;
        let counter_raw = format!("{}:{}|c|@{}", counter_name, counter_value, counter_sample_rate);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected.metadata.sample_rate = Some(counter_sample_rate);

        let (remaining, counter_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_container_id() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let container_id = "abcdef123456";
        let counter_raw = format!("{}:{}|c|c:{}", counter_name, counter_value, container_id);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected
            .context
            .tags
            .insert_tag(format!("{}:{}", CONTAINER_ID_TAG_KEY, container_id));

        let (remaining, counter_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_unix_timestamp() {
        let counter_name = "my.counter";
        let counter_value = 1.0;
        let timestamp = 1234567890;
        let counter_raw = format!("{}:{}|c|T{}", counter_name, counter_value, timestamp);
        let mut counter_expected = counter(counter_name, counter_value);
        counter_expected.metadata.timestamp = timestamp;

        let (remaining, counter_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_eq(counter_expected, counter_actual);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_multivalue_metrics() {
        let counter_name = "my.counter";
        let counter_values = [1.0, 2.0, 3.0];
        let counter_values_stringified = counter_values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let counter_raw = format!("{}:{}|c", counter_name, counter_values_stringified.join(":"));
        let counters_expected = counter_multivalue(counter_name, &counter_values);
        let (remaining, counters_actual) = parse_dogstatsd(counter_raw.as_bytes()).unwrap();
        check_basic_metric_multivalue_eq(counters_expected, counters_actual);
        assert!(remaining.is_empty());

        let gauge_name = "my.gauge";
        let gauge_values = [42.0, 5.0, -18.0];
        let gauge_values_stringified = gauge_values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let gauge_raw = format!("{}:{}|g", gauge_name, gauge_values_stringified.join(":"));
        let gauges_expected = gauge_multivalue(gauge_name, &gauge_values);
        let (remaining, gauges_actual) = parse_dogstatsd(gauge_raw.as_bytes()).unwrap();
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
            let (remaining, distributions_actual) = parse_dogstatsd(distribution_raw.as_bytes()).unwrap();
            check_basic_metric_multivalue_eq(distributions_expected, distributions_actual);
            assert!(remaining.is_empty());
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
            let _ = parse_dogstatsd(&input);
        }
    }
}
