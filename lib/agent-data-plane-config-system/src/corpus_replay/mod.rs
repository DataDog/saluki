//! Replays config-recorder corpus cases through this crate's config system. `loader` turns a case
//! into the exact `ConfigEvent` stream the Agent sent, and `driver` pushes that stream through the
//! same update step the running process uses. `compare` decides whether a typed leaf agrees with a
//! recorded getter result, and `derived` does the same for values ADP derives from several settings.
//! `bootstrap` replays a case's YAML and environment through ADP's own bootstrap reader instead.
//! `known_results` checks what they compute against the checked-in `known-results.txt`, where every divergence names
//! a declared divergence type: its cause, and the layer a fix would go in.
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
