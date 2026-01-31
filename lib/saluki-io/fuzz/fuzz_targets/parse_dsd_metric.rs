#![no_main]

use libfuzzer_sys::fuzz_target;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_context::Context;
use saluki_core::data_model::event::metric::{Metric, MetricMetadata};
use saluki_io::deser::codec::dogstatsd::{DogstatsdCodec, DogstatsdCodecConfiguration, ParsedPacket};

fuzz_target!(|data: &[u8]| {
    // Fuzz with default configuration
    let config = DogstatsdCodecConfiguration::default();
    let codec = DogstatsdCodec::from_configuration(config);
    
    // Try to parse the input
    if let Ok(packet) = codec.decode_packet(data) {
        // If parsing succeeds, verify it doesn't panic when converting to a Metric
        match packet {
            ParsedPacket::Metric(metric_packet) => {
                let tags = metric_packet.tags.into_iter().map(Tag::from).collect::<SharedTagSet>();
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

