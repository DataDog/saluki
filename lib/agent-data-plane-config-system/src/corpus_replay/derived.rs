//! Produces one verdict per recorded getter for every value ADP derives from several settings that
//! mirrors a value the Agent derives and streams.
//!
//! This is the derived tier. The leaf tier compares each setting as ADP deserializes it, so it cannot
//! see a value ADP computes itself. When ADP and the Agent compute such a value differently, ADP runs
//! with a value the Agent's getter does not return. For each case that records a derived key, this tier
//! translates the folded sources at each checkpoint (every update applied, as the leaf tier folds
//! them), derives the value from the translated configuration, and compares it with the recorded
//! getter reads.
//!
//! [`DERIVATIONS`] lists the derivations this tier replays; [`NOT_REPLAYED`] lists the ones it does
//! not, and why.

use std::fmt;
use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::LeafValue;
use datadog_agent_config_corpus::{Corpus, Getter, GetterRead, GetterResult, Outcome};
use serde_json::Value;

use super::compare::{compare_result, Verdict};
use super::leaf_replay::{fold_case, kind_name, lookup, one_line, reads_at, Checkpoint};
use crate::system::translate_strict;

/// A value ADP derives, expressed in the type of the getter it is compared with.
#[derive(Clone, Debug)]
pub(crate) enum Derived {
    /// The value, exactly representable as the getter's result.
    Exact(LeafValue<'static>),
    /// A value the integer getter cannot represent exactly: never a match, whatever the Agent returns.
    NotWhole {
        /// The nearest integer, which the Agent's result is rendered against.
        nearest: i64,
        /// The exact value, rendered.
        exact: String,
    },
}

/// One value ADP derives that stands for a value the Agent derives and streams under `key`.
pub(crate) struct Derivation {
    /// The Agent key whose getter returns the Agent's derived value.
    pub(crate) key: &'static str,
    /// The getter the corpus records for `key`, which the derived value is expressed for.
    pub(crate) getter: Getter,
    /// Derives the value from ADP's translated configuration.
    pub(crate) derive: fn(&SalukiConfiguration) -> Derived,
}

/// Every derivation this tier replays.
pub(crate) const DERIVATIONS: &[Derivation] = &[Derivation {
    key: "data_plane.stop_timeout",
    getter: Getter::GetInt,
    derive: stop_timeout,
}];

/// Every derivation ADP has that this tier does not replay: its name and the reason.
pub(crate) const NOT_REPLAYED: &[(&str, &str)] = &[
    (
        "api-key-trim",
        "ADP trims `api_key` on every update; comparing it needs the protocol tier (what is sent), not a getter",
    ),
    (
        "no-proxy-cloud-metadata",
        "effective no-proxy list appends cloud metadata IPs; the Agent's streamed list already has them; needs a \
         proxy-list derivation in the table (follow-up)",
    ),
    (
        "container-roots",
        "depends on the process environment and filesystem, not only settings",
    ),
    (
        "cri-socket",
        "depends on the process environment and filesystem, not only settings",
    ),
    (
        "main-endpoint",
        "the Agent derives it in a consumer; no recorded getter returns the derived URL",
    ),
    (
        "mrf-endpoint",
        "the Agent derives it in a consumer; no recorded getter returns the derived URL",
    ),
    (
        "otlp-receiver-endpoint-override",
        "ADP-only keys overwrite the Agent's endpoint by design; never equal",
    ),
];

/// The topology shutdown timeout, which `GetInt` returns in whole seconds.
fn stop_timeout(config: &SalukiConfiguration) -> Derived {
    whole_seconds(config.stop_timeout())
}

/// Expresses `duration` as whole seconds, or as [`Derived::NotWhole`] if it has a fractional second or
/// more seconds than an `i64` holds.
fn whole_seconds(duration: Duration) -> Derived {
    let secs = match i64::try_from(duration.as_secs()) {
        Ok(secs) if duration.subsec_nanos() == 0 => return Derived::Exact(LeafValue::I64(secs)),
        Ok(secs) => secs,
        Err(_) => i64::MAX,
    };
    let round_up = duration.subsec_nanos() >= 500_000_000;
    Derived::NotWhole {
        nearest: if round_up { secs.saturating_add(1) } else { secs },
        exact: format!("{duration:?}"),
    }
}

impl Derived {
    /// The name of the value's `LeafValue` variant.
    fn kind(&self) -> &'static str {
        match self {
            Derived::Exact(leaf) => kind_name(leaf),
            Derived::NotWhole { .. } => kind_name(&LeafValue::I64(0)),
        }
    }
}

/// Compares a derived value against every getter recorded for its key, one verdict per recorded getter.
pub(crate) fn compare_derived(value: &Derived, reads: &[GetterRead]) -> Vec<(Getter, Verdict)> {
    reads
        .iter()
        .map(|r| (r.getter, compare_derived_result(value, r.getter, &r.result)))
        .collect()
}

/// Compares a derived value against one recorded getter result, through [`compare_result`].
///
/// A [`Derived::NotWhole`] value is compared as its nearest integer and then reported with its exact
/// rendering, so a match of the nearest integer is a difference.
fn compare_derived_result(value: &Derived, getter: Getter, result: &GetterResult) -> Verdict {
    match value {
        Derived::Exact(leaf) => compare_result(*leaf, getter, result),
        Derived::NotWhole { nearest, exact } => match compare_result(LeafValue::I64(*nearest), getter, result) {
            Verdict::Match => Verdict::Differs {
                adp: exact.clone(),
                agent: nearest.to_string(),
            },
            Verdict::Differs { agent, .. } => Verdict::Differs {
                adp: exact.clone(),
                agent,
            },
            verdict => verdict,
        },
    }
}

/// One verdict of the derived tier: a (case, checkpoint, key, getter) and what the comparison found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DerivedRow {
    pub(crate) case: String,
    pub(crate) checkpoint: Checkpoint,
    /// The Agent key the derivation stands for.
    pub(crate) key: &'static str,
    pub(crate) getter: Getter,
    /// The derived value's `LeafValue` variant name.
    pub(crate) kind: &'static str,
    /// The value at `key`'s path in the folded tree at this checkpoint, or `None` if the tree does not
    /// hold it.
    pub(crate) streamed: Option<Value>,
    pub(crate) verdict: Verdict,
}

impl fmt::Display for DerivedRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?} {} {}: ", self.case, self.checkpoint, self.key, self.getter)?;
        match &self.verdict {
            Verdict::Match => f.write_str("match"),
            Verdict::Differs { adp, agent } => write!(f, "differs: adp {} agent {}", one_line(adp), one_line(agent)),
            Verdict::AdpRejects { error } => write!(f, "adp rejects: {}", one_line(error)),
            Verdict::NotCompared { reason } => write!(f, "not compared: {reason}"),
        }
    }
}

/// Produces every derived row of the corpus's started cases, sorted by case, checkpoint, key and getter.
///
/// # Errors
///
/// Returns every harness error: a case whose stream cannot be built, or a key line whose final read is
/// not present exactly when the case has updates.
pub(crate) fn corpus_derived_rows(corpus: &Corpus) -> Result<Vec<DerivedRow>, Vec<String>> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for case in &corpus.cases {
        let Outcome::Started(started) = &case.outcome else {
            continue;
        };
        if !started
            .keys
            .iter()
            .any(|line| DERIVATIONS.iter().any(|d| d.key == line.key))
        {
            continue;
        }
        let folded = match fold_case(corpus, &case.name) {
            Ok(folded) => folded,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let has_updates = !started.updates.is_empty();
        let checkpoints = [
            (Checkpoint::Snapshot, &folded.snapshot, &folded.snapshot_sources),
            (Checkpoint::Final, &folded.last, &folded.last_sources),
        ];
        for (checkpoint, tree, sources) in checkpoints {
            let reads = match reads_at(&case.name, &started.keys, checkpoint, has_updates) {
                Ok(reads) => reads,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let mut translated = None;
            for derivation in DERIVATIONS {
                for &(_, read) in reads.iter().filter(|(key, _)| *key == derivation.key) {
                    let config = translated.get_or_insert_with(|| translate_strict(sources).map_err(|e| e.to_string()));
                    let (kind, verdicts) = match config {
                        Ok(config) => {
                            let value = (derivation.derive)(config);
                            (value.kind(), compare_derived(&value, &read.getters))
                        }
                        Err(error) => {
                            let rejected = read
                                .getters
                                .iter()
                                .map(|r| (r.getter, Verdict::AdpRejects { error: error.clone() }))
                                .collect();
                            (kind_name(&LeafValue::I64(0)), rejected)
                        }
                    };
                    rows.extend(verdicts.into_iter().map(|(getter, verdict)| DerivedRow {
                        case: case.name.clone(),
                        checkpoint,
                        key: derivation.key,
                        getter,
                        kind,
                        streamed: lookup(tree, derivation.key).cloned(),
                        verdict,
                    }));
                }
            }
        }
    }
    rows.sort_by(|a, b| {
        (&a.case, a.checkpoint, a.key, a.getter.as_str()).cmp(&(&b.case, b.checkpoint, b.key, b.getter.as_str()))
    });
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_config_corpus::Number;

    use super::super::compare::LeafKind;
    use super::*;

    /// The stop-timeout derivation of [`DERIVATIONS`].
    fn stop_timeout_row() -> &'static Derivation {
        DERIVATIONS
            .iter()
            .find(|d| d.key == "data_plane.stop_timeout")
            .expect("the table has the stop timeout")
    }

    /// A recorded `GetInt` result of `value`.
    fn get_int(value: i64) -> GetterResult {
        GetterResult::Int(Number {
            value,
            token: value.to_string(),
        })
    }

    /// A configuration whose component stop timeouts are 3 and 7 seconds, with the given `stop_timeout`.
    fn config_with(stop_timeout: Option<Duration>) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.control.stop_timeout = stop_timeout;
        config.control.aggregator_stop_timeout = Duration::from_secs(3);
        config.shared.endpoints.forwarder.stop_timeout = Duration::from_secs(7);
        config
    }

    fn derive_and_compare(config: &SalukiConfiguration, agent: i64) -> Verdict {
        let row = stop_timeout_row();
        compare_derived_result(&(row.derive)(config), row.getter, &get_int(agent))
    }

    #[test]
    fn every_derivation_is_expressed_for_its_recorded_getter() {
        let config = SalukiConfiguration::default();
        for derivation in DERIVATIONS {
            let Derived::Exact(leaf) = (derivation.derive)(&config) else {
                panic!("{}: the default derives an exact value", derivation.key);
            };
            assert!(
                LeafKind::of(&leaf).emulated().contains(&derivation.getter),
                "{}: a {} value is not compared with {}",
                derivation.key,
                kind_name(&leaf),
                derivation.getter
            );
        }
    }

    #[test]
    fn stop_timeout_row_uses_the_configured_value() {
        let config = config_with(Some(Duration::from_secs(11)));
        assert!(matches!(
            (stop_timeout_row().derive)(&config),
            Derived::Exact(LeafValue::I64(11))
        ));
        assert_eq!(derive_and_compare(&config, 11), Verdict::Match);
    }

    #[test]
    fn stop_timeout_row_sums_component_timeouts_otherwise() {
        let config = config_with(None);
        assert!(matches!(
            (stop_timeout_row().derive)(&config),
            Derived::Exact(LeafValue::I64(10))
        ));
        assert_eq!(derive_and_compare(&config, 10), Verdict::Match);
        assert_eq!(
            derive_and_compare(&config, 4),
            Verdict::Differs {
                adp: "10".to_string(),
                agent: "4".to_string()
            }
        );
    }

    #[test]
    fn stop_timeout_row_renders_a_non_whole_second_as_a_difference_from_the_nearest_integer() {
        for (millis, nearest) in [(4_400, 4), (4_600, 5)] {
            let config = config_with(Some(Duration::from_millis(millis)));
            let exact = format!("{:?}", Duration::from_millis(millis));
            assert!(matches!(
                (stop_timeout_row().derive)(&config),
                Derived::NotWhole { nearest: n, exact: e } if n == nearest && e == exact
            ));
            assert_eq!(
                derive_and_compare(&config, nearest),
                Verdict::Differs {
                    adp: exact.clone(),
                    agent: nearest.to_string()
                }
            );
            assert_eq!(
                derive_and_compare(&config, 9),
                Verdict::Differs {
                    adp: exact,
                    agent: "9".to_string()
                }
            );
        }
    }

    #[test]
    fn whole_seconds_saturates_beyond_the_integer_range() {
        let exact = format!("{:?}", Duration::from_secs(u64::MAX));
        assert!(matches!(
            whole_seconds(Duration::from_secs(u64::MAX)),
            Derived::NotWhole { nearest: i64::MAX, exact: e } if e == exact
        ));
    }
}
