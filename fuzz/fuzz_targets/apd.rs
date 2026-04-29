#![no_main]

use agent_data_plane::fuzz::{inner, DogStatsDInput};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: DogStatsDInput| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(input));
});
