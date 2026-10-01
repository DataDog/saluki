//! Replays recorded Agent configuration cases through the running process's configuration system.
//!
//! `loader` reconstructs the exact wire events the Agent sent; `driver` runs them through the real
//! update path.
//! `compare` checks each modeled setting against recorded getter reads. `derived` compares values
//! computed from settings, while `bootstrap` compares what ADP reads directly from a case's YAML and
//! environment, without the Agent stream. `expectations` holds the machinery that compares every
//! result with the hand-edited table in `expected`, by check identity; the default expectation is
//! that ADP matches the recorded Agent.
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
mod expectations;
mod expected;
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

/// A production call that panicked, caught at that call so the replay can abandon only the state it was building.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Panicked {
    /// The production call that panicked, for example `deserialize`.
    pub(crate) operation: &'static str,
    /// The panic payload text.
    pub(crate) message: String,
}

/// Runs one production call, turning a panic into [`Panicked`].
///
/// Catch narrowly: only around a single call into production code, never around the loader, the
/// expectation machinery, or a whole case.
pub(crate) fn guard<T>(operation: &'static str, call: impl FnOnce() -> T) -> Result<T, Panicked> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        Panicked { operation, message }
    })
}
