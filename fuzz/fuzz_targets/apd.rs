#![no_main]

use std::sync::LazyLock;
use bytes::Bytes;

use agent_data_plane::fuzz::{DogStatsDInput, aggregate_metric_lines, inner, metric_line_from_raw, saluki_path};
use libfuzzer_sys::fuzz_target;

use arbitrary::{Arbitrary, Unstructured};
use barkus_core::{generate::decode, ir::GrammarIr, profile::Profile};

pub static PROFILE: LazyLock<Profile> = LazyLock::new(|| Profile::default());

static GRAMMAR_MULTILINE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path = saluki_path().join("fuzz/dogstatsd_multi_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path)
        .expect("failed to read dogstatsd EBNF grammar file at fuzz/dogstatsd_multi_offset.ebnf");
    barkus_ebnf::compile(&grammar).expect("failed to compile dogstatsd EBNF grammar")
});

#[derive(Debug)]
struct FuzzDogStatsDInput
{
    metrics: DogStatsDInput,
}

impl<'a> Arbitrary<'a> for FuzzDogStatsDInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let tape = u.bytes(u.len())?;
        let profile = Profile::default();
        let (ast, _) = decode(&*&GRAMMAR_MULTILINE, &profile, tape).map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let text = ast.serialize();
        let message_lines: Vec<(u64, Bytes)> = text
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(metric_line_from_raw)
            .collect::<Result<_, _>>()
            .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let aggregated_messages = aggregate_metric_lines(message_lines);

        Ok(FuzzDogStatsDInput{ metrics: DogStatsDInput {
            messages: aggregated_messages,
        }})
    }
}

fuzz_target!(|input: FuzzDogStatsDInput| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(input.metrics));
});
