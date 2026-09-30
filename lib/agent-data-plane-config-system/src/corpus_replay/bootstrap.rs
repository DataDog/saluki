//! Replays each corpus case's recorded inputs through ADP's own bootstrap reader and compares the
//! result with what the Agent's getters returned after the first snapshot.
//!
//! This is the bootstrap tier. Before the Agent's configuration stream arrives, and in standalone
//! mode instead of it, ADP reads `datadog.yaml` and the environment itself. This tier gives that
//! reader the exact YAML text and environment a case gave the Agent, with the environment after the
//! file, and reads each supported leaf from the resulting base in isolation, as the leaf tier reads
//! it from the streamed tree. A difference means either that ADP reads a YAML shape or environment
//! value differently from the Agent, or that the Agent changed the value while loading, which ADP's
//! bootstrap cannot see.
//!
//! Only started cases whose inputs have no fleet policy and no CLI override are in scope: the Agent's
//! first snapshot then comes from the YAML, the environment, and defaults alone. Updates come after
//! the first snapshot, so only that checkpoint is compared.

use std::path::Path;

use datadog_agent_config_corpus::{Corpus, Inputs, Outcome};

use super::leaf_replay::{checkpoint_rows, reads_at, Checkpoint, Isolator, Row};
use crate::loaded::{build_base_from, EnvPrecedence};

/// The path a YAML parse error names: the file the Agent would have read.
const YAML_ORIGIN: &str = "datadog.yaml";

/// What the bootstrap tier found across the corpus.
pub(crate) struct BootstrapResults {
    /// One row per recorded getter of every key line of every case in scope whose base built, at the
    /// snapshot checkpoint. A row's `streamed` value is the one the base holds at the key's path.
    pub(crate) rows: Vec<Row>,
    /// Every case in scope whose base did not build, with the error ADP aborts its boot on.
    pub(crate) aborts: Vec<(String, String)>,
    /// How many started cases are in scope.
    pub(crate) in_scope: usize,
    /// How many started cases are out of scope because of a fleet policy or a CLI override.
    pub(crate) out_of_scope: usize,
}

/// Returns whether a started case's first snapshot depends only on its YAML, its environment, and
/// defaults.
fn in_scope(inputs: &Inputs) -> bool {
    inputs.fleet_policy.is_none() && inputs.cli.is_empty()
}

/// Builds the bootstrap base from a case's inputs: its YAML (an empty object when absent) and its
/// environment (empty when absent), the environment read after the file.
fn case_base(inputs: &Inputs) -> Result<serde_json::Value, String> {
    let yaml = inputs.yaml.as_deref().unwrap_or("{}");
    let vars: Vec<(String, String)> = inputs
        .env
        .iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    build_base_from(yaml, Path::new(YAML_ORIGIN), &vars, EnvPrecedence::AfterFile).map_err(|e| e.to_string())
}

/// Replays every started case in scope through the bootstrap reader.
///
/// Rows are sorted by case, key and getter; aborts by case.
pub(crate) fn corpus_bootstrap(corpus: &Corpus) -> BootstrapResults {
    let mut isolator = Isolator::new();
    let mut results = BootstrapResults {
        rows: Vec::new(),
        aborts: Vec::new(),
        in_scope: 0,
        out_of_scope: 0,
    };
    for case in &corpus.cases {
        let Outcome::Started(started) = &case.outcome else {
            continue;
        };
        if !in_scope(&case.inputs) {
            results.out_of_scope += 1;
            continue;
        }
        results.in_scope += 1;
        let base = match case_base(&case.inputs) {
            Ok(base) => base,
            Err(error) => {
                results.aborts.push((case.name.clone(), error));
                continue;
            }
        };
        // The snapshot checkpoint never needs the case's updates, so `has_updates` does not matter.
        let reads = reads_at(&case.name, &started.keys, Checkpoint::Snapshot, false)
            .expect("the snapshot checkpoint pairs every key line with its snapshot read");
        results.rows.extend(checkpoint_rows(
            &mut isolator,
            &case.name,
            Checkpoint::Snapshot,
            &base,
            &reads,
        ));
    }
    results.rows.sort_by(|a, b| {
        (&a.case, &a.key, a.getter.map(|g| g.as_str())).cmp(&(&b.case, &b.key, b.getter.map(|g| g.as_str())))
    });
    results.aborts.sort();
    results
}
