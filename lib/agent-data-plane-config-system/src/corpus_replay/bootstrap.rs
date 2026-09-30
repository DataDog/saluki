//! Replays each corpus case's recorded inputs through ADP's own bootstrap reader and compares the
//! result with what the Agent's getters returned after the first snapshot.
//!
//! Before the Agent's configuration stream arrives, ADP reads `datadog.yaml` and the environment
//! itself; standalone mode uses those inputs instead of the stream. This bootstrap comparison gives
//! ADP the exact YAML and environment recorded for each case, applying the environment after the
//! file. It compares each supported setting in isolation against the Agent's getter result, as the
//! streamed-value comparison does. A difference may come from how ADP reads YAML or the environment,
//! or from a value the Agent changed during loading that ADP's bootstrap reader cannot see.
//!
//! Only cases that started without a fleet policy or CLI override qualify: their first Agent
//! snapshot comes from YAML, environment variables, and defaults alone. Later updates are not compared.

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

/// Builds the bootstrap base from a case's YAML and environment, applying the environment after the file.
///
/// Missing YAML becomes an empty object; missing environment variables contribute nothing.
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
