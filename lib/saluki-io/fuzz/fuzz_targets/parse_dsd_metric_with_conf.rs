#![no_main]

use libfuzzer_sys::arbitrary::{self, Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_context::Context;
use saluki_core::data_model::event::metric::{Metric, MetricMetadata};
use saluki_io::deser::codec::dogstatsd::{DogstatsdCodec, DogstatsdCodecConfiguration, ParsedPacket};

// These are only used for checking that the limits are enforced.
// When "no limit" is used, we simply don't check that it's under that amount.
const MAX_TAG_COUNT: u16 = 100;
const MAX_TAG_LENGTH: u16 = 500;

#[derive(Debug)]
struct FuzzInput<'a> {
    data: &'a [u8],
    permissive: bool,
    timestamps: bool,
    max_tag_count: Option<u16>,
    max_tag_length: Option<u16>,
}

impl<'a> Arbitrary<'a> for FuzzInput<'a> {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let data = u.bytes(u.len())?;
        let permissive = u.arbitrary()?;
        let timestamps = u.arbitrary()?;

        // These value can be None, which means no limit is enforced.
        let max_tag_count = if u.arbitrary()? {
            Some(u.int_in_range(1..=MAX_TAG_COUNT)?)
        } else {
            None
        };

        let max_tag_length = if u.arbitrary()? {
            Some(u.int_in_range(1..=MAX_TAG_LENGTH)?)
        } else {
            None
        };

        Ok(FuzzInput {
            data,
            permissive,
            timestamps,
            max_tag_count,
            max_tag_length,
        })
    }
}

fuzz_target!(|input: FuzzInput| {
    // Build configuration from fuzz input
    let mut config = DogstatsdCodecConfiguration::default()
        .with_permissive_mode(input.permissive)
        .with_timestamps(input.timestamps);

    if let Some(count) = input.max_tag_count {
        config = config.with_maximum_tag_count(count as usize);
    }

    if let Some(length) = input.max_tag_length {
        config = config.with_maximum_tag_length(length as usize);
    }

    let codec = DogstatsdCodec::from_configuration(config);

    if let Ok(packet) = codec.decode_packet(input.data) {
        // if parsing succeeds, verify it doesn't panic when converting to a Metric
        match packet {
            ParsedPacket::Metric(metric_packet) => {
                let tags = metric_packet.tags.into_iter().map(Tag::from).collect::<SharedTagSet>();

                // Validate configuration limits are respected before creating the metric
                if let Some(max_count) = input.max_tag_count {
                    assert!(tags.len() <= max_count as usize);
                }

                if let Some(max_length) = input.max_tag_length {
                    for tag in tags.clone().into_iter() {
                        assert!(tag.len() <= max_length as usize);
                    }
                }

                let context = Context::from_parts(metric_packet.metric_name, tags);
                let _metric = Metric::from_parts(context, metric_packet.values, MetricMetadata::default());
            }
            ParsedPacket::Event(_) | ParsedPacket::ServiceCheck(_) => {
                // These are valid parsable packets, just not metrics
            }
        }
    }

    // Parsing failures are expected and fine - the fuzzer should not panic
});
