#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use barkus_core::{generate::decode, ir::GrammarIr, profile::Profile};
use libfuzzer_sys::fuzz_target;
use saluki_io::deser::codec::dogstatsd::{DogStatsDCodec, DogStatsDCodecConfiguration};
use std::sync::LazyLock;

static GRAMMAR: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar = std::fs::read_to_string("fuzz/grammar.ebnf").unwrap();
    barkus_ebnf::compile(&grammar).unwrap()
});

#[derive(Debug)]
struct DogStatsDInput {
    messages: Vec<Vec<u8>>,
}

impl<'a> Arbitrary<'a> for DogStatsDInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let tape = u.bytes(u.len())?;
        let profile = Profile::default();
        let (ast, _) = decode(&*GRAMMAR, &profile, tape)
            .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let text = ast.serialize();
        let messages = text
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        Ok(DogStatsDInput { messages })
    }
}

static CODEC: LazyLock<DogStatsDCodec> =
    LazyLock::new(|| DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()));

fuzz_target!(|input: DogStatsDInput| {
    for msg in &input.messages {
        let u = CODEC.decode_packet(msg);
        let m = u.unwrap();
    }
});
