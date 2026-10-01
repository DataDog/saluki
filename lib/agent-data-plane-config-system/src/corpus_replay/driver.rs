//! Replays a recorded Agent event stream through the running process's configuration update path.
//!
//! [`config_event_to_update`] converts each wire event, and [`evaluate`] deserializes, translates,
//! and validates the result against the accumulated Agent settings.
//! Replay starts with no local configuration, so only the Agent's stream contributes.
//!
//! Replay runs in one of two [`Commit`] modes. [`Commit::Accepted`] is the running process's rule:
//! an update is adopted only when every stage, validation included, succeeds. The corpus baseline
//! streams `api_key` as `""`, so under that rule nearly every case's startup is rejected and its
//! later updates are never applied. [`Commit::Translated`] is for inspecting what the typed
//! configuration makes of every recorded shape: it adopts an update that translates whatever
//! validation says, so a rejected shape is attributed to the update that introduced it, not to every
//! later step. The leaf and derived tiers fold every event independently of either rule.

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::TranslateErrors;
use datadog_agent_config_corpus::Corpus;
use saluki_config::dynamic::ConfigUpdate;

use super::guard;
use super::leaf_replay::lookup;
use super::loader::{build_events, CaseEvents};
use crate::agent_stream::config_event_to_update;
use crate::source::SourceTree;
use crate::system::{evaluate, Error, Evaluation, Stages};

/// The `sequence_id` replay gives each case's first snapshot. The process ignores it (record.md §4.2).
const BASE_SEQUENCE_ID: i32 = 1;

/// When replay adopts an update's Agent layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Commit {
    /// Only when deserialization, translation, and validation all succeed, as the running process does.
    Accepted,
    /// Whenever the update deserializes and translates, whatever validation says.
    Translated,
}

/// A stage of turning merged sources into a runnable configuration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Stage {
    Deserialize,
    Translate,
    Validate,
    /// The production call panicked; the error holds the panic text.
    Panic,
}

/// The stage at which one step failed, and why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Failure {
    pub(crate) stage: Stage,
    /// The exact error text (or panic text).
    pub(crate) error: String,
    /// Whether validation failed with [`Error::MissingApiKey`], matched on the typed error.
    pub(crate) missing_api_key: bool,
    /// For a translation failure, each error's key and message, in the order translation recorded them.
    pub(crate) translate_errors: Vec<(String, String)>,
}

/// What [`crate::system::validate`] said about a translated configuration, by its typed result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Validation {
    /// The configuration is runnable.
    Valid,
    /// Rejected with the typed [`Error::MissingApiKey`].
    MissingApiKey,
    /// Rejected with any other error, by its text. No validation rule other than the API key exists
    /// today, so any such result is unexpected.
    Rejected(String),
}

/// The record of one event of the stream.
#[derive(Clone, Debug)]
pub(crate) struct StepRecord {
    /// The key a partial update set; `None` for a snapshot.
    pub(crate) key: Option<String>,
    /// Which update of `key` this is, counting from 1; 0 for a snapshot.
    pub(crate) occurrence: usize,
    /// The stage that failed, if any. The stages run in order, so at most one fails.
    pub(crate) failure: Option<Failure>,
    /// Whether replay adopted the step's Agent layer.
    pub(crate) committed: bool,
    /// The typed result of validation; `None` when an earlier stage failed, so validation never ran.
    pub(crate) validation: Option<Validation>,
    /// The merged `api_key` value validation read, independent of the translated configuration;
    /// `None` when the step never reached validation or the merged sources hold no `api_key`.
    pub(crate) api_key: Option<serde_json::Value>,
}

/// Applies updates one at a time onto an Agent layer, under a [`Commit`] rule.
pub(crate) struct Replay {
    base: SourceTree,
    agent: SourceTree,
    commit: Commit,
    occurrences: std::collections::BTreeMap<String, usize>,
}

impl Replay {
    /// Starts from an empty local base and an empty Agent layer.
    pub(crate) fn new(commit: Commit) -> Self {
        Self {
            base: SourceTree::empty(),
            agent: SourceTree::empty(),
            commit,
            occurrences: Default::default(),
        }
    }

    /// Evaluates `update` against the committed Agent layer, commits it per the rule, and returns the
    /// step's record and the translated configuration, if the update got that far.
    pub(crate) fn apply(&mut self, update: &ConfigUpdate) -> (StepRecord, Option<SalukiConfiguration>) {
        let mut validated = None;
        let (config, failure, tentative) = match guard("evaluate", || evaluate(&self.base, &self.agent, update)) {
            Err(panicked) => (
                None,
                Some(Failure {
                    stage: Stage::Panic,
                    error: format!("{} panicked: {}", panicked.operation, panicked.message),
                    missing_api_key: false,
                    translate_errors: Vec::new(),
                }),
                None,
            ),
            Ok(Evaluation {
                tentative,
                merged,
                stages,
            }) => {
                let (config, failure) = match stages {
                    Stages::Undeserializable(error) => (
                        None,
                        Some(Failure {
                            stage: Stage::Deserialize,
                            error: error.to_string(),
                            missing_api_key: false,
                            translate_errors: Vec::new(),
                        }),
                    ),
                    Stages::Untranslatable { errors, .. } => (
                        None,
                        Some(Failure {
                            stage: Stage::Translate,
                            error: errors.to_string(),
                            missing_api_key: false,
                            translate_errors: keyed_errors(&errors),
                        }),
                    ),
                    Stages::Translated { config, validation, .. } => {
                        validated = Some((
                            match &validation {
                                Ok(()) => Validation::Valid,
                                Err(Error::MissingApiKey) => Validation::MissingApiKey,
                                Err(error) => Validation::Rejected(error.to_string()),
                            },
                            lookup(&merged.to_value(), "api_key").cloned(),
                        ));
                        (
                            Some(*config),
                            validation.err().map(|error| Failure {
                                stage: Stage::Validate,
                                missing_api_key: matches!(error, Error::MissingApiKey),
                                error: error.to_string(),
                                translate_errors: Vec::new(),
                            }),
                        )
                    }
                };
                (config, failure, Some(tentative))
            }
        };

        let committed = match self.commit {
            Commit::Accepted => config.is_some() && failure.is_none(),
            Commit::Translated => config.is_some(),
        };
        if let (true, Some(tentative)) = (committed, tentative) {
            self.agent = tentative;
        }

        let key = match update {
            ConfigUpdate::Snapshot(_) => None,
            ConfigUpdate::Partial(setting) => Some(setting.key.clone()),
        };
        let occurrence = key.as_ref().map_or(0, |key| {
            let n = self.occurrences.entry(key.clone()).or_default();
            *n += 1;
            *n
        });
        let (validation, api_key) = match validated {
            Some((validation, api_key)) => (Some(validation), api_key),
            None => (None, None),
        };
        (
            StepRecord {
                key,
                occurrence,
                failure,
                committed,
                validation,
                api_key,
            },
            config,
        )
    }
}

/// Returns each error of `errors` as its key and its message.
fn keyed_errors(errors: &TranslateErrors) -> Vec<(String, String)> {
    errors
        .into_iter()
        .map(|error| (error.key().to_string(), error.to_string()))
        .collect()
}

/// Replays the started case `case_name` of `corpus` under `commit`.
///
/// Under [`Commit::Accepted`] a rejected first snapshot abandons the case, as a rejected startup
/// ends the process: the later events are not applied and get no record. A step that fails is part
/// of the result, not an error.
///
/// # Errors
///
/// Returns an error if the loader cannot build the case's stream, the case never started, the stream
/// does not open with a snapshot, or an event converts to no update.
pub(crate) fn replay_case(corpus: &Corpus, case_name: &str, commit: Commit) -> Result<Vec<StepRecord>, String> {
    let updates = case_updates(corpus, case_name)?;
    let mut replay = Replay::new(commit);
    let mut steps = Vec::with_capacity(updates.len());
    for update in &updates {
        let (record, _) = replay.apply(update);
        let abandoned = record.failure.as_ref().is_some_and(|f| f.stage == Stage::Panic)
            || (steps.is_empty() && commit == Commit::Accepted && !record.committed);
        steps.push(record);
        if abandoned {
            break;
        }
    }
    Ok(steps)
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
        let update = guard("convert stream event", || config_event_to_update(event))
            .map_err(|p| {
                format!(
                    "case {case_name:?}: event {position}: {} panicked: {}",
                    p.operation, p.message
                )
            })?
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
    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use datadog_agent_config_corpus::Outcome;
    use saluki_config::dynamic::{ConfigSetting, ConfigUpdate};
    use serde_json::json;

    use super::*;
    use crate::corpus_replay::corpus;

    const KEY: &str = "00000000000000000000000000000000";

    fn snapshot(settings: &[(&str, serde_json::Value)]) -> ConfigUpdate {
        ConfigUpdate::snapshot(settings.iter().map(|(k, v)| ConfigSetting::explicit(*k, v.clone())))
    }

    fn set(key: &str, value: serde_json::Value) -> ConfigUpdate {
        ConfigUpdate::Partial(ConfigSetting::explicit(key, value))
    }

    fn cardinality(config: &SalukiConfiguration) -> OriginTagCardinality {
        config.domains.dogstatsd.origin.tag_cardinality
    }

    #[test]
    fn a_rejected_update_is_not_committed_and_does_not_poison_the_next_one() {
        let mut replay = Replay::new(Commit::Accepted);
        let (record, config) = replay.apply(&snapshot(&[
            ("api_key", json!(KEY)),
            ("dogstatsd_tag_cardinality", json!("high")),
        ]));
        assert!(record.committed && record.failure.is_none());
        assert_eq!(cardinality(&config.expect("translated")), OriginTagCardinality::High);

        let (record, _) = replay.apply(&set("dogstatsd_tag_cardinality", json!("bogus")));
        assert!(!record.committed);
        assert_eq!(record.failure.as_ref().map(|f| f.stage), Some(Stage::Translate));

        // Had the rejected value lingered, this update would fold onto it and fail too.
        let (record, config) = replay.apply(&set("log_level", json!("error")));
        assert!(record.committed, "{record:?}");
        let config = config.expect("translated");
        assert_eq!(config.control.logging.level, "error");
        assert_eq!(
            cardinality(&config),
            OriginTagCardinality::High,
            "last good value is kept"
        );
    }

    /// The valid-stream-updates case through the same `evaluate` path the process uses: `high` is
    /// accepted (the Agent streams no event for the unchanged `low`), `bogus` is rejected with the exact translation error and leaves `high`
    /// in force, and `orchestrator` recovers.
    #[test]
    fn valid_stream_updates_case_rejects_bogus_and_recovers() {
        let corpus = corpus();
        let steps = replay_case(corpus, "valid-stream-updates", Commit::Accepted).expect("the case replays");
        let outcome: Vec<_> = steps
            .iter()
            .map(|s| {
                (
                    s.key.as_deref(),
                    s.occurrence,
                    s.committed,
                    s.failure.as_ref().map(|f| f.stage),
                )
            })
            .collect();
        let key = Some("dogstatsd_tag_cardinality");
        assert_eq!(
            outcome,
            vec![
                (None, 0, true, None),
                (key, 1, true, None),
                (key, 2, false, Some(Stage::Translate)),
                (key, 3, true, None),
            ]
        );
        let bogus = steps[2].failure.as_ref().expect("bogus fails");
        assert_eq!(bogus.translate_errors.len(), 1);
        assert!(
            bogus.error.contains("unknown tag cardinality `bogus`"),
            "{}",
            bogus.error
        );
        assert_eq!(bogus.translate_errors[0].0, "dogstatsd_tag_cardinality");

        // Replay the same stream by hand to read the last-good state between the steps.
        let updates = case_updates(corpus, "valid-stream-updates").expect("stream");
        let mut replay = Replay::new(Commit::Accepted);
        let mut seen = Vec::new();
        let mut last_good = None;
        for update in &updates {
            let (record, config) = replay.apply(update);
            if record.committed {
                last_good = config.as_ref().map(cardinality);
            }
            seen.push(last_good);
        }
        assert_eq!(
            seen,
            vec![
                Some(OriginTagCardinality::Low),
                Some(OriginTagCardinality::High),
                Some(OriginTagCardinality::High),
                Some(OriginTagCardinality::Orchestrator),
            ]
        );
    }

    // Exercise the connected startup and update loop, not only the stage evaluator. A logging
    // update after each event provides an acknowledgement even when the event was rejected.
    #[tokio::test]
    async fn connected_configuration_keeps_the_last_valid_recorded_update() {
        use std::time::Duration;

        use tokio::sync::mpsc;

        use crate::system::ConfigurationSystem;

        let updates = case_updates(corpus(), "valid-stream-updates").expect("recorded stream");
        let (tx, rx) = mpsc::channel(8);
        tx.send(updates[0].clone()).await.unwrap();
        let (system, worker) = ConfigurationSystem::connected(rx, SourceTree::empty())
            .await
            .expect("valid snapshot");
        assert_eq!(cardinality(&system.config()), OriginTagCardinality::Low);
        let worker = tokio::spawn(worker.run());
        let checkpoints = [
            (OriginTagCardinality::High, "warn"),
            (OriginTagCardinality::High, "error"),
            (OriginTagCardinality::Orchestrator, "info"),
        ];
        assert_eq!(updates.len(), checkpoints.len() + 1);
        for (update, (expected, level)) in updates[1..].iter().zip(checkpoints) {
            tx.send(update.clone()).await.unwrap();
            tx.send(set("log_level", json!(level))).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while system.config().control.logging.level != level {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the following valid update is applied");
            assert_eq!(cardinality(&system.config()), expected, "after {update:?}");
        }
        drop(tx);
        assert!(matches!(worker.await.unwrap(), Err(Error::UpdateStreamClosed)));
    }

    #[test]
    fn a_blank_api_key_snapshot_is_rejected_with_the_typed_error_and_abandons_startup() {
        let blank = snapshot(&[("api_key", json!("")), ("dogstatsd_port", json!(9125))]);
        let mut replay = Replay::new(Commit::Accepted);
        let (record, config) = replay.apply(&blank);
        assert!(!record.committed, "the process does not adopt a snapshot it rejects");
        let failure = record.failure.expect("validation fails");
        assert_eq!((failure.stage, failure.missing_api_key), (Stage::Validate, true));
        assert_eq!(failure.error, Error::MissingApiKey.to_string());
        assert_eq!(config.expect("translated").domains.dogstatsd.listeners.port, 9125);

        // The shape mode still adopts it, so later steps are attributed to their own update.
        let mut replay = Replay::new(Commit::Translated);
        assert!(replay.apply(&blank).0.committed);
        let (_, config) = replay.apply(&set("log_level", json!("error")));
        assert_eq!(config.expect("translated").domains.dogstatsd.listeners.port, 9125);
    }

    #[test]
    fn a_snapshot_replaces_the_whole_agent_layer() {
        let mut replay = Replay::new(Commit::Accepted);
        replay.apply(&snapshot(&[("api_key", json!(KEY)), ("dogstatsd_port", json!(9125))]));
        let (record, config) = replay.apply(&snapshot(&[("api_key", json!(KEY)), ("log_level", json!("error"))]));
        assert!(record.committed);
        let config = config.expect("translated");
        assert_eq!(config.control.logging.level, "error");
        assert_eq!(config.domains.dogstatsd.listeners.port, 8125);
    }

    #[test]
    fn every_started_case_builds_a_stream_the_driver_can_replay() {
        for case in &corpus().cases {
            if matches!(case.outcome, Outcome::Started(_)) {
                replay_case(corpus(), &case.name, Commit::Translated)
                    .unwrap_or_else(|e| panic!("{} ({}): {e}", case.name, case.input_line));
            }
        }
    }
}
