#![no_main]

use libfuzzer_sys::fuzz_target;
use saluki_io::deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration};

fuzz_target!(|data: &[u8]| {
    let config = DogStatsDCodecConfiguration::default();
    let codec = DogStatsDCodec::from_configuration(config);

    // Try to parse the input.
    //
    // Parsing failures are expected and fine: the fuzzer should not panic, so we don't care about success or failure.
    let _ = codec.decode_packet(data);
});
