//! Replays recorded Agent configuration cases through the running process's configuration system.
//!
//! `loader` reconstructs the exact wire events the Agent sent; `driver` runs them through the real
//! update path.
//! `compare` checks each modeled setting against recorded getter reads. `derived` compares values
//! computed from settings, while `bootstrap` compares what ADP reads directly from a case's YAML and
//! environment, without the Agent stream. `known_results` checks the results against
//! `known-results.txt`, which groups annotated differences by cause and by where a fix belongs.
//!
//! The corpus and its contract (`lib/datadog-agent/config-recorder/docs/record.md`) are owned
//! elsewhere; this module only reads them through `datadog_agent_config_corpus::read`.

use std::sync::OnceLock;

use datadog_agent_config_corpus::{read, Corpus};

mod bootstrap;
mod classification;
mod compare;
mod derived;
mod driver;
mod known_results;
mod leaf_replay;
mod loader;

/// The checked-in corpus, parsed once and shared by every test that reads it.
static CORPUS_ONE: OnceLock<Corpus> = OnceLock::new();

/// Returns the checked-in corpus, reading and parsing it on first use.
///
/// # Panics
///
/// Panics if the corpus file cannot be read or is not well-formed.
fn corpus() -> &'static Corpus {
    CORPUS_ONE.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../datadog-agent/config-recorder/corpus.jsonl"
        );
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        read(&bytes).unwrap_or_else(|v| panic!("corpus.jsonl should be well-formed: {v:#?}"))
    })
}
