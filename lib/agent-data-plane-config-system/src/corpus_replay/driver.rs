//! Replays one corpus case's event stream through the update step the running process uses, and
//! records what the typed configuration makes of it.
//!
//! Each event passes through [`config_event_to_update`], the conversion the process applies to the
//! Agent's real stream, and then through [`evaluate`], the step the update loop runs. Replay starts
//! from an empty local base: only the Agent's stream contributes.

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::DatadogConfiguration;
use datadog_agent_config_corpus::Corpus;
use saluki_config::dynamic::ConfigUpdate;

use super::loader::{build_events, CaseEvents};
use crate::agent_stream::config_event_to_update;
use crate::source::SourceTree;
use crate::system::{evaluate, Evaluation, Stages};

/// The `sequence_id` replay gives each case's first snapshot. The process ignores it (record.md §4.2).
const BASE_SEQUENCE_ID: i32 = 1;

/// A stage of turning merged sources into a runnable configuration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Stage {
    Deserialize,
    Translate,
    Validate,
}

/// The stage at which one step failed, and why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Failure {
    pub(crate) stage: Stage,
    pub(crate) error: String,
}

/// What the typed configuration made of the merged sources at one point in the stream.
#[derive(Clone, Debug)]
pub(crate) struct State {
    /// The deserialized Datadog source, or the deserialization error.
    pub(crate) datadog: Result<DatadogConfiguration, String>,
    /// The translated configuration; `None` if deserialization or translation failed.
    pub(crate) saluki: Option<SalukiConfiguration>,
}

/// The record of one event of the stream.
#[derive(Clone, Debug)]
pub(crate) struct StepRecord {
    /// The key a partial update set; `None` for a snapshot.
    pub(crate) key: Option<String>,
    /// The stage that failed, if any. The stages run in order, so at most one fails.
    pub(crate) failure: Option<Failure>,
    /// Whether replay adopted the step's Agent layer.
    pub(crate) committed: bool,
}

/// The result of replaying one case.
///
/// The corpus records the Agent's getter reads at the same two points: `reads.snapshot` and
/// `reads.final` (record.md §5.3).
#[derive(Debug)]
pub(crate) struct CaseReplay {
    /// The state the first snapshot produced, whether or not it was committed.
    pub(crate) snapshot: State,
    /// The state after the last update, or `None` if the stream carries no update.
    ///
    /// This is the state of the last committed step, so an update that failed translation leaves the
    /// previous state in place, as the running process keeps its last-known-good configuration. If no
    /// step was ever committed, it is the first snapshot's state.
    pub(crate) last: Option<State>,
    /// One record per event, in stream order; the first is the snapshot.
    pub(crate) steps: Vec<StepRecord>,
}

/// Applies updates one at a time onto an Agent layer, under replay's commit rule.
pub(crate) struct Replay {
    base: SourceTree,
    agent: SourceTree,
}

impl Replay {
    /// Starts from an empty local base and an empty Agent layer.
    pub(crate) fn new() -> Self {
        Self {
            base: SourceTree::empty(),
            agent: SourceTree::empty(),
        }
    }

    /// Evaluates `update` against the committed Agent layer, commits it if it translated, and returns
    /// the step's record and the state it produced.
    pub(crate) fn apply(&mut self, update: &ConfigUpdate) -> (StepRecord, State) {
        let Evaluation { tentative, stages, .. } = evaluate(&self.base, &self.agent, update);
        let (state, failure) = match stages {
            Stages::Undeserializable(error) => (
                State {
                    datadog: Err(error.to_string()),
                    saluki: None,
                },
                Some(Failure {
                    stage: Stage::Deserialize,
                    error: error.to_string(),
                }),
            ),
            Stages::Untranslatable { sources, errors } => (
                State {
                    datadog: Ok(sources.datadog),
                    saluki: None,
                },
                Some(Failure {
                    stage: Stage::Translate,
                    error: errors.to_string(),
                }),
            ),
            Stages::Translated {
                sources,
                config,
                validation,
            } => (
                State {
                    datadog: Ok(sources.datadog),
                    saluki: Some(*config),
                },
                validation.err().map(|error| Failure {
                    stage: Stage::Validate,
                    error: error.to_string(),
                }),
            ),
        };

        // The running process commits only an update that both translates and validates. Replay
        // commits every update that translates, whatever validation says: the corpus baseline streams
        // `api_key` as `""`, so validation rejects every case's first snapshot, and replay must still
        // follow the typed configuration through the rest of the stream. An update that fails
        // translation is not committed, exactly as the process keeps its last-known-good layer.
        let committed = state.saluki.is_some();
        if committed {
            self.agent = tentative;
        }

        let key = match update {
            ConfigUpdate::Snapshot(_) => None,
            ConfigUpdate::Partial(setting) => Some(setting.key.clone()),
        };
        let record = StepRecord {
            key,
            failure,
            committed,
        };
        (record, state)
    }
}

/// Replays the started case `case_name` of `corpus`.
///
/// A step that fails deserialization, translation, or validation is part of the result, not an error.
///
/// # Errors
///
/// Returns an error if the loader cannot build the case's stream, the case never started, the stream
/// does not open with a snapshot, or an event converts to no update.
pub(crate) fn replay_case(corpus: &Corpus, case_name: &str) -> Result<CaseReplay, String> {
    let updates = case_updates(corpus, case_name)?;
    let (first, rest) = updates.split_first().expect("case_updates returns a non-empty stream");

    let mut replay = Replay::new();
    let (record, snapshot) = replay.apply(first);
    let mut steps = vec![record];
    let mut current = snapshot.clone();
    for update in rest {
        let (record, state) = replay.apply(update);
        if record.committed {
            current = state;
        }
        steps.push(record);
    }

    Ok(CaseReplay {
        snapshot,
        last: (!rest.is_empty()).then_some(current),
        steps,
    })
}

/// Converts the started case `case_name` of `corpus` into the updates the process would apply, in
/// stream order. The first is always a snapshot.
///
/// # Errors
///
/// Returns an error if the loader cannot build the case's stream, the case never started, the stream
/// is empty or does not open with a snapshot, or an event converts to no update.
pub(crate) fn case_updates(corpus: &Corpus, case_name: &str) -> Result<Vec<ConfigUpdate>, String> {
    let events = match build_events(corpus, case_name, BASE_SEQUENCE_ID)? {
        CaseEvents::Started(events) => events,
        CaseEvents::StartupFailed => return Err(format!("case {case_name:?}: did not start")),
    };

    let mut updates = Vec::with_capacity(events.len());
    for (position, event) in events.into_iter().enumerate() {
        let update = config_event_to_update(event)
            .ok_or_else(|| format!("case {case_name:?}: event {position} converts to no update"))?;
        updates.push(update);
    }
    match updates.first() {
        None => Err(format!("case {case_name:?}: the stream is empty")),
        Some(ConfigUpdate::Snapshot(_)) => Ok(updates),
        Some(ConfigUpdate::Partial(_)) => Err(format!("case {case_name:?}: the stream does not open with a snapshot")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use datadog_agent_config_corpus::Outcome;
    use saluki_config::dynamic::{ConfigSetting, ConfigUpdate};
    use serde_json::json;

    use super::*;
    use crate::corpus_replay::corpus;
    use crate::system::Error;

    fn translated(state: &State) -> &SalukiConfiguration {
        state.saluki.as_ref().expect("the step translated")
    }

    /// Replays every started corpus case, requires the driver itself never to fail, and prints which
    /// steps the typed configuration could not deserialize, translate, or validate.
    #[test]
    fn every_started_corpus_case_replays() {
        let corpus = corpus();
        let mut driver_errors = Vec::new();
        // Failing steps as (case, position, key), grouped by error text and stage.
        type FailingSteps = Vec<(String, usize, Option<String>)>;
        let mut failures: BTreeMap<(String, Stage), FailingSteps> = BTreeMap::new();
        let (mut cases, mut snapshots_translated, mut steps, mut committed) = (0, 0, 0, 0);

        for case in &corpus.cases {
            let Outcome::Started(started) = &case.outcome else {
                continue;
            };
            let replayed = match catch_unwind(AssertUnwindSafe(|| replay_case(corpus, &case.name))) {
                Ok(Ok(replayed)) => replayed,
                Ok(Err(error)) => {
                    driver_errors.push(error);
                    continue;
                }
                Err(_) => {
                    driver_errors.push(format!("case {:?}: replay panicked", case.name));
                    continue;
                }
            };

            let recorded_events: usize = started.keys.iter().map(|k| k.events.len()).sum();
            assert_eq!(replayed.steps.len(), recorded_events + 1, "case {:?}", case.name);
            assert_eq!(replayed.steps[0].key, None, "case {:?}", case.name);
            assert_eq!(replayed.last.is_some(), recorded_events > 0, "case {:?}", case.name);

            cases += 1;
            snapshots_translated += usize::from(replayed.snapshot.saluki.is_some());
            steps += replayed.steps.len();
            for (position, step) in replayed.steps.into_iter().enumerate() {
                committed += usize::from(step.committed);
                if let Some(failure) = step.failure {
                    failures.entry((failure.error, failure.stage)).or_default().push((
                        case.name.clone(),
                        position,
                        step.key,
                    ));
                }
            }
        }

        let mut by_stage: BTreeMap<Stage, usize> = BTreeMap::new();
        for ((_, stage), steps) in &failures {
            *by_stage.entry(*stage).or_default() += steps.len();
        }
        println!(
            "replayed {cases} cases ({snapshots_translated} first snapshots translated), {steps} steps, {committed} \
             committed; failed steps by stage: {by_stage:?}"
        );
        for ((error, stage), steps) in &failures {
            println!("{stage:?} ({} steps): {error}", steps.len());
            for (case, position, key) in steps {
                println!("  {case} step {position} {}", key.as_deref().unwrap_or("<snapshot>"));
            }
        }

        assert!(driver_errors.is_empty(), "driver errors: {driver_errors:#?}");
        assert!(cases > 0, "the corpus has started cases");
    }

    #[test]
    fn a_partial_update_that_fails_translation_is_not_committed() {
        let mut replay = Replay::new();
        replay.apply(&ConfigUpdate::snapshot([
            ConfigSetting::explicit("api_key", json!("k")),
            ConfigSetting::explicit("dogstatsd_tag_cardinality", json!("high")),
        ]));

        let (record, _) = replay.apply(&ConfigUpdate::Partial(ConfigSetting::explicit(
            "dogstatsd_tag_cardinality",
            json!("bogus"),
        )));
        assert!(!record.committed);
        assert_eq!(record.failure.as_ref().map(|f| f.stage), Some(Stage::Translate));

        // Had the rejected value been committed, this update would fold onto it and fail too.
        let (record, state) = replay.apply(&ConfigUpdate::Partial(ConfigSetting::explicit(
            "log_level",
            json!("error"),
        )));
        assert!(record.committed);
        assert_eq!(record.failure, None);
        let config = translated(&state);
        assert_eq!(config.control.logging.level, "error");
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
    }

    #[test]
    fn a_snapshot_with_a_blank_api_key_fails_validation_but_is_committed() {
        let mut replay = Replay::new();
        let snapshot = ConfigUpdate::snapshot([
            ConfigSetting::explicit("api_key", json!("")),
            ConfigSetting::explicit("dogstatsd_port", json!(9125)),
        ]);

        // The running process rejects this snapshot outright.
        let production = evaluate(&SourceTree::empty(), &SourceTree::empty(), &snapshot).stages;
        assert!(matches!(production.into_authoritative(), Err(Error::MissingApiKey)));

        let (record, state) = replay.apply(&snapshot);
        assert!(record.committed);
        let failure = record.failure.expect("validation fails");
        assert_eq!(failure.stage, Stage::Validate);
        assert_eq!(failure.error, Error::MissingApiKey.to_string());
        assert_eq!(translated(&state).domains.dogstatsd.listeners.port, 9125);
        let datadog = state.datadog.expect("the snapshot deserializes");
        assert_eq!(datadog.api_key, "");

        // The next update folds onto the committed snapshot.
        let (_, state) = replay.apply(&ConfigUpdate::Partial(ConfigSetting::explicit(
            "log_level",
            json!("error"),
        )));
        assert_eq!(translated(&state).domains.dogstatsd.listeners.port, 9125);
    }

    #[test]
    fn a_snapshot_replaces_the_whole_agent_layer() {
        let mut replay = Replay::new();
        replay.apply(&ConfigUpdate::snapshot([
            ConfigSetting::explicit("api_key", json!("k")),
            ConfigSetting::explicit("dogstatsd_port", json!(9125)),
        ]));

        let (record, state) = replay.apply(&ConfigUpdate::snapshot([ConfigSetting::explicit(
            "log_level",
            json!("error"),
        )]));

        assert!(record.committed);
        let config = translated(&state);
        assert_eq!(config.control.logging.level, "error");
        assert_eq!(config.domains.dogstatsd.listeners.port, 8125);
        assert_eq!(config.shared.endpoints.api_key, "");
    }
}
