use nom::{
    bytes::complete::tag,
    combinator::{all_consuming, map_res},
    number::complete::double,
    sequence::{preceded, separated_pair},
    IResult, Parser as _,
};
use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
use saluki_core::data_model::event::metric::{MetricValues, SampleRate};

use super::{helpers::*, message::CARDINALITY_PREFIX, DogstatsdCodecConfiguration, WellKnownTags};

/// A DogStatsD metric packet.
pub struct MetricPacket<'a> {
    pub metric_name: &'a str,
    pub tags: RawTags<'a>,
    pub values: MetricValues,
    pub num_points: u64,
    pub timestamp: Option<u64>,
    pub container_id: Option<&'a str>,
    pub external_data: Option<&'a str>,
    pub well_known_tags: WellKnownTags<'a>,
    pub cardinality: Option<OriginTagCardinality>,
}

pub fn parse_dogstatsd_metric<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration,
) -> IResult<&'a [u8], MetricPacket<'a>> {
    // We always parse the metric name and value(s) first, where value is both the kind (counter, gauge, etc) and the
    // actual value itself.
    let metric_name_parser = if config.permissive {
        permissive_metric_name
    } else {
        ascii_alphanum_and_seps
    };
    let (remaining, (metric_name, (metric_type, raw_metric_values))) =
        separated_pair(metric_name_parser, tag(":"), raw_metric_values).parse(input)?;

    // At this point, we may have some of this additional data, and if so, we also then would have a pipe separator at
    // the very front, which we'd want to consume before going further.
    //
    // After that, we simply split the remaining bytes by the pipe separator, and then try and parse each chunk to see
    // if it's any of the protocol extensions we know of.
    let mut maybe_sample_rate = None;
    let mut maybe_tags = None;
    let mut maybe_container_id = None;
    let mut maybe_timestamp = None;
    let mut maybe_external_data = None;
    let mut maybe_cardinality = None;

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
                    let (_, sample_rate) =
                        all_consuming(preceded(tag("@"), map_res(double, SampleRate::try_from))).parse(chunk)?;
                    maybe_sample_rate = Some(sample_rate);
                }
                // Tags: additional tags to be added to the metric.
                b'#' => {
                    let (_, tags) = all_consuming(preceded(tag("#"), metric_tags(config))).parse(chunk)?;
                    maybe_tags = Some(tags);
                }
                // Container ID: client-provided container ID for the container that this metric originated from.
                b'c' if chunk.len() > 1 && chunk[1] == b':' => {
                    let (_, container_id) = all_consuming(preceded(tag("c:"), container_id)).parse(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // Timestamp: client-provided timestamp for the metric, relative to the Unix epoch, in seconds.
                b'T' => {
                    if config.timestamps {
                        let (_, timestamp) = all_consuming(preceded(tag("T"), unix_timestamp)).parse(chunk)?;
                        maybe_timestamp = Some(timestamp);
                    }
                }
                // External Data: client-provided data used for resolving the entity ID that this metric originated from.
                b'e' if chunk.len() > 1 && chunk[1] == b':' => {
                    let (_, external_data) = all_consuming(preceded(tag("e:"), external_data)).parse(chunk)?;
                    maybe_external_data = Some(external_data);
                }
                // Cardinality: client-provided cardinality for the metric.
                b'c' if chunk.starts_with(CARDINALITY_PREFIX) => {
                    let (_, cardinality) = cardinality(chunk)?;
                    maybe_cardinality = Some(cardinality);
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

    let (num_points, mut metric_values) = metric_values_from_raw(raw_metric_values, metric_type, maybe_sample_rate)?;

    // If we got a timestamp, apply it to all metric values.
    if let Some(timestamp) = maybe_timestamp {
        metric_values.set_timestamp(timestamp);
    }

    let tags = maybe_tags.unwrap_or_else(RawTags::empty);
    let well_known_tags = WellKnownTags::from_raw_tags(tags.clone());
    let cardinality = maybe_cardinality.or(well_known_tags.cardinality);

    Ok((
        remaining,
        MetricPacket {
            metric_name,
            tags,
            values: metric_values,
            num_points,
            timestamp: maybe_timestamp,
            container_id: maybe_container_id,
            external_data: maybe_external_data,
            well_known_tags,
            cardinality,
        },
    ))
}

#[cfg(test)]
mod tests {
    use proptest::{collection::vec as arb_vec, prelude::*};
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{SharedTagSet, Tag},
        Context,
    };
    use saluki_core::data_model::event::metric::*;

    use super::{parse_dogstatsd_metric, DogstatsdCodecConfiguration};

    type OptionalNomResult<'input, T> = Result<Option<T>, nom::Err<nom::error::Error<&'input [u8]>>>;

    fn parse_dsd_metric(input: &[u8]) -> OptionalNomResult<'_, Metric> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_metric_with_conf(input, &default_config)
    }

    fn parse_dsd_metric_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> OptionalNomResult<'input, Metric> {
        let (remaining, packet) = parse_dogstatsd_metric(input, config)?;
        assert!(remaining.is_empty());

        let tags = packet.tags.into_iter().map(Tag::from).collect::<SharedTagSet>();
        let context = Context::from_parts(packet.metric_name, tags);

        Ok(Some(Metric::from_parts(
            context,
            packet.values,
            MetricMetadata::default(),
        )))
    }

    #[track_caller]
    fn check_basic_metric_eq(expected: Metric, actual: Option<Metric>) -> Metric {
        let actual = actual.expect("event should not have been None");
        assert_eq!(expected.context(), actual.context());
        assert_eq!(expected.values(), actual.values());
        assert_eq!(expected.metadata(), actual.metadata());
        actual
    }

    #[test]
    fn basic_metric() {
        let name = "my.counter";
        let value = 1.0;
        let raw = format!("{}:{}|c", name, value);
        let expected = Metric::counter(name, value);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let name = "my.gauge";
        let value = 2.0;
        let raw = format!("{}:{}|g", name, value);
        let expected = Metric::gauge(name, value);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        // Special case where we check this for both timers and histograms since we treat them both the same when
        // parsing.
        let name = "my.timer_or_histogram";
        let value = 3.0;
        for kind in &["ms", "h"] {
            let raw = format!("{}:{}|{}", name, value, kind);
            let expected = Metric::histogram(name, value);
            let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
            check_basic_metric_eq(expected, actual);
        }

        let distribution_name = "my.distribution";
        let distribution_value = 3.0;
        let distribution_raw = format!("{}:{}|d", distribution_name, distribution_value);
        let distribution_expected = Metric::distribution(distribution_name, distribution_value);
        let distribution_actual = parse_dsd_metric(distribution_raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(distribution_expected, distribution_actual);

        let set_name = "my.set";
        let set_value = "value";
        let set_raw = format!("{}:{}|s", set_name, set_value);
        let set_expected = Metric::set(set_name, set_value);
        let set_actual = parse_dsd_metric(set_raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(set_expected, set_actual);
    }

    #[test]
    fn metric_tags() {
        let name = "my.counter";
        let value = 1.0;
        let tags = ["tag1", "tag2"];
        let raw = format!("{}:{}|c|#{}", name, value, tags.join(","));
        let expected = Metric::counter((name, &tags[..]), value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn metric_sample_rate() {
        let name = "my.counter";
        let value = 1.0;
        let sample_rate = 0.5;
        let raw = format!("{}:{}|c|@{}", name, value, sample_rate);

        let value_sample_rate_adjusted = value * (1.0 / sample_rate);
        let expected = Metric::counter(name, value_sample_rate_adjusted);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        let actual = check_basic_metric_eq(expected, actual);
        let values = match actual.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, v)| (ts.map(|v| v.get()).unwrap_or(0), v))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (0, value_sample_rate_adjusted));
    }

    #[test]
    fn metric_container_id() {
        let name = "my.counter";
        let value = 1.0;
        let container_id = "abcdef123456";
        let raw = format!("{}:{}|c|c:{}", name, value, container_id);
        let expected = Metric::counter(name, value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.container_id, Some(container_id));
    }

    #[test]
    fn metric_unix_timestamp() {
        let name = "my.counter";
        let value = 1.0;
        let timestamp = 1234567890;
        let raw = format!("{}:{}|c|T{}", name, value, timestamp);
        let mut expected = Metric::counter(name, value);
        expected.values_mut().set_timestamp(timestamp);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn metric_external_data() {
        let name = "my.counter";
        let value = 1.0;
        let external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let raw = format!("{}:{}|c|e:{}", name, value, external_data);
        let expected = Metric::counter(name, value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.external_data, Some(external_data));
    }

    #[test]
    fn metric_cardinality() {
        let name = "my.counter";
        let value = 1.0;
        let cardinality = "high";
        let raw = format!("{}:{}|c|card:{}", name, value, cardinality);
        let expected = Metric::counter(name, value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::High));
    }

    #[test]
    fn metric_cardinality_precedence() {
        // Tests that the cardinality specificed in the card field takes precedence over `dd.internal.card`
        let name = "my.counter";
        let value = 1.0;
        let tags = ["dd.internal.card:low"];
        let cardinality = "high";
        let raw = format!("{}:{}|c|#{}|card:{}", name, value, tags.join(","), cardinality);
        let expected = Metric::counter((name, &tags[..]), value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::High));
    }

    #[test]
    fn metric_multiple_extensions() {
        let name = "my.counter";
        let value = 1.0;
        let sample_rate = 0.5;
        let tags = ["tag1", "tag2"];
        let container_id = "abcdef123456";
        let external_data = "it-false,cn-redis,pu-810fe89d-da47-410b-8979-9154a40f8183";
        let cardinality = "orchestrator";
        let timestamp = 1234567890;
        let raw = format!(
            "{}:{}|c|#{}|@{}|c:{}|e:{}|card:{}|T{}",
            name,
            value,
            tags.join(","),
            sample_rate,
            container_id,
            external_data,
            cardinality,
            timestamp
        );

        let value_sample_rate_adjusted = value * (1.0 / sample_rate);
        let mut expected = Metric::counter((name, &tags[..]), value_sample_rate_adjusted);
        expected.values_mut().set_timestamp(timestamp);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        let actual = check_basic_metric_eq(expected, actual);
        let values = match actual.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, v)| (ts.map(|v| v.get()).unwrap_or(0), v))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (timestamp, value_sample_rate_adjusted));

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.container_id, Some(container_id));
        assert_eq!(packet.external_data, Some(external_data));
        assert_eq!(packet.cardinality, Some(OriginTagCardinality::Orchestrator));
    }

    #[test]
    fn metric_well_known_tags() {
        // Just ensures that we're populating well-known tags.
        let raw = "my_counter:1|c|#dd.internal.card:low";

        let config = DogstatsdCodecConfiguration::default();
        let (_, packet) = parse_dogstatsd_metric(raw.as_bytes(), &config).expect("should not fail to parse");
        assert_eq!(packet.well_known_tags.hostname, None);
        assert_eq!(packet.well_known_tags.cardinality, Some(OriginTagCardinality::Low));
    }

    #[test]
    fn multivalue_metrics() {
        let name = "my.counter";
        let values = [1.0, 2.0, 3.0];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let raw = format!("{}:{}|c", name, values_stringified.join(":"));
        let expected = Metric::counter(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let name = "my.gauge";
        let values = [42.0, 5.0, -18.0];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let raw = format!("{}:{}|g", name, values_stringified.join(":"));
        let expected = Metric::gauge(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        // Special case where we check this for both timers and histograms since we treat them both the same when
        // parsing.
        //
        // Additionally, we have an optimization to return a single distribution metric from multi-value payloads, so we
        // also check here that only one metric is generated for multi-value timers/histograms/distributions.
        let name = "my.timer_or_histogram";
        let values = [27.5, 4.20, 80.085];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        for kind in &["ms", "h"] {
            let raw = format!("{}:{}|{}", name, values_stringified.join(":"), kind);
            let expected = Metric::histogram(name, values);
            let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
            check_basic_metric_eq(expected, actual);
        }

        let name = "my.distribution";
        let raw = format!("{}:{}|d", name, values_stringified.join(":"));
        let expected = Metric::distribution(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn respects_maximum_tag_count() {
        let input = b"foo:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let cases = [3, 2, 1];
        for max_tag_count in cases {
            let config = DogstatsdCodecConfiguration::default().with_maximum_tag_count(max_tag_count);

            let metric = parse_dsd_metric_with_conf(input, &config)
                .expect("should not fail to parse")
                .expect("should not fail to intern");
            assert_eq!(metric.context().tags().len(), max_tag_count);
        }
    }

    #[test]
    fn respects_maximum_tag_length() {
        let input = b"foo:1|c|#tag1:short,tag2:medium,tag3:longlong";

        let cases = [6, 5, 4];
        for max_tag_length in cases {
            let config = DogstatsdCodecConfiguration::default().with_maximum_tag_length(max_tag_length);

            let metric = parse_dsd_metric_with_conf(input, &config)
                .expect("should not fail to parse")
                .expect("should not fail to intern");
            for tag in metric.context().tags().into_iter() {
                assert!(tag.len() <= max_tag_length);
            }
        }
    }

    #[test]
    fn respects_read_timestamps() {
        let input = b"foo:1|c|T1234567890";

        let config = DogstatsdCodecConfiguration::default().with_timestamps(false);

        let metric = parse_dsd_metric_with_conf(input, &config)
            .expect("should not fail to parse")
            .expect("should not fail to intern");

        let value_timestamps = match metric.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, _)| ts.map(|v| v.get()).unwrap_or(0))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(value_timestamps.len(), 1);
        assert_eq!(value_timestamps[0], 0);
    }

    #[test]
    fn permissive_mode() {
        let payload = b"codeheap 'non-nmethods'.usage:0.3054|g|#env:dev,service:foobar,datacenter:localhost.dev";

        let config = DogstatsdCodecConfiguration::default().with_permissive_mode(true);
        match parse_dsd_metric_with_conf(payload, &config) {
            Ok(result) => assert!(result.is_some(), "should not fail to materialize metric after decoding"),
            Err(e) => panic!("should not have errored: {:?}", e),
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
            let _ = parse_dsd_metric(&input);
        }
    }
}
