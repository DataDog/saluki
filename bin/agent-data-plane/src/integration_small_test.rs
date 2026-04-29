use std::{path::PathBuf, sync::LazyLock};

use agent_data_plane::fuzz::{
    DogStatsDInput, PROFILE, aggregate_metric_lines, metric_line_from_raw, inner, saluki_path
};
use barkus_core::ir::GrammarIr;
use bytes::Bytes;
use rand::{rngs::SmallRng, SeedableRng};
use saluki_error::GenericError;

static GRAMMAR_SINGLE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path: PathBuf = saluki_path().join("fuzz/dogstatsd_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path).unwrap();
    barkus_ebnf::compile(&grammar).unwrap()
});

fn generate_corpus_random() -> Result<DogStatsDInput, GenericError> {
    let seed = Some(1234u64);
    let count = 100;
    let mut rng: SmallRng = match seed {
        Some(s) => SmallRng::seed_from_u64(s),
        None => SmallRng::from_rng(&mut rand::rng()),
    };
    let generated_packets: Vec<(u64, Bytes)> = (0..count)
        .map(|_| {
            let (ast, _tape, _map) = barkus_core::generate::generate(&GRAMMAR_SINGLE, &PROFILE, &mut rng)
                .map_err(|e| GenericError::msg("Barkus - failed to generate").context(e))?;
            let ast_bytes = ast.serialize();
            metric_line_from_raw(&ast_bytes)
        })
        .collect::<Result<_, GenericError>>()?;
    Ok(DogStatsDInput { messages: aggregate_metric_lines(generated_packets) })
}


fn main() {
    println!("hello!");
    let corpus = generate_corpus_random().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(corpus));
    println!("goodbye!");
}
