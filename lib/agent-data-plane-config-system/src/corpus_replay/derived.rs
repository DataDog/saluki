//! Compares values ADP computes from settings with the Agent's recorded getter results.
//!
//! ADP may combine settings, as it does for the shutdown timeout, or parse a setting, as it does
//! for byte sizes. Comparing deserialized settings alone cannot catch differences in those computed
//! values: equal size strings, for example, can produce different byte counts. For each case that
//! records such a key, this comparison applies every update, translates the resulting sources at
//! each recorded checkpoint, and compares the computed value with each recorded getter read. A
//! difference means ADP runs with a value that the Agent's getter does not return.
//!
//! [`DERIVATIONS`] lists the derivations this tier replays; [`NOT_REPLAYED`] lists the ones it does
//! not, and why.

use std::fmt;
use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::LeafValue;
use datadog_agent_config_corpus::{Corpus, Getter, GetterRead, GetterResult, Outcome};
use serde_json::Value;

use super::compare::{compare_byte_count, compare_result, Reason, Verdict};
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
    /// A byte count, compared only with `GetSizeInBytes`.
    Bytes(u64),
}

/// A value ADP computes from translated settings, paired with the key whose Agent getter computes the counterpart.
pub(crate) struct Derivation {
    /// The Agent key whose getter returns the Agent's derived value.
    pub(crate) key: &'static str,
    /// The getter the corpus records for `key`, which the derived value is expressed for.
    pub(crate) getter: Getter,
    /// Derives the value from ADP's translated configuration.
    pub(crate) derive: fn(&SalukiConfiguration) -> Derived,
}

/// Every derivation this tier replays.
pub(crate) const DERIVATIONS: &[Derivation] = &[
    Derivation {
        key: "data_plane.stop_timeout",
        getter: Getter::GetInt,
        derive: stop_timeout,
    },
    Derivation {
        key: "log_file_max_size",
        getter: Getter::GetSizeInBytes,
        derive: log_file_max_size,
    },
    Derivation {
        key: "dogstatsd_log_file_max_size",
        getter: Getter::GetSizeInBytes,
        derive: dogstatsd_log_file_max_size,
    },
];

/// Every value ADP computes but this comparison does not replay, each with a name and reason.
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
    (
        "extra-sample-rate-default",
        "an unset `apm_config.extra_sample_rate` becomes 1.0; the Agent's trace config also starts at 1.0 \
         (pkg/trace/config/config.go) and reads the getter only when the key is configured \
         (comp/trace/config/impl/setup.go), so the getter returns the schema's 0",
    ),
    (
        "max-tps-alias",
        "`apm_config.max_traces_per_second` supplies the target rate unless `target_traces_per_second` is set; the \
         Agent chooses in comp/trace/config/impl/setup.go, and each getter returns only its own key",
    ),
    (
        "dogstatsd-log-file-default",
        "an unset or default `dogstatsd_log_file` leaves the path unset, and ADP picks the platform path at \
         startup, outside the translated configuration; the getter returns the recording host's resolved path, \
         which depends on the platform, not only on settings",
    ),
    (
        "forwarder-storage-path-default",
        "an empty or default `forwarder_storage_path` keeps ADP's default, which the retry queue resolves from \
         `run_path` at startup (saluki-components retry.rs), outside the translated configuration",
    ),
    (
        "run-path-unset",
        "an empty or placeholder `run_path` becomes unset; the getter returns the string, which the leaf tier \
         compares",
    ),
    (
        "metric-filter-precedence",
        "a non-empty `metric_filterlist` replaces `statsd_metric_blocklist`; the Agent chooses in \
         comp/filterlist/impl/filterlist.go, and each getter returns only its own key",
    ),
    (
        "forwarder-backoff-fallback",
        "a non-positive backoff base or max, or a factor below 2, falls back to the default; the Agent does the \
         same after the getters, in comp/forwarder/defaultforwarder/impl/blocked_endpoints.go",
    ),
    (
        "retry-queue-max-size",
        "the byte budget prefers `forwarder_retry_queue_payloads_max_size` and scales the deprecated count key; the \
         Agent combines them in comp/forwarder/defaultforwarder/impl/default_forwarder.go, and each getter returns \
         only its own key",
    ),
    (
        "zstd-level-precedence",
        "the effective zstd level prefers the ADP-only level, then an explicit Agent level, then ADP's default of 3; \
         the getter returns the Agent's level, default 1, so the defaults differ by design",
    ),
    (
        "otlp-grpc-receiver-defaults",
        "a zero gRPC max receive size and zero or sub-second keepalive values take grpc-go's defaults; the \
         Agent's OTLP receiver applies them in grpc-go, so the getters return the configured value",
    ),
    (
        "v3-series-mode-fallback",
        "an unrecognized `use_v3_api.series` mode disables V3; the Agent recovers in pkg/serializer/metrics.go \
         (`evalSeriesV3`), so the getter returns the raw string",
    ),
    (
        "negative-clamp",
        "a negative count or interval (such as `forwarder_timeout`) becomes 0; the getter returns the negative \
         number, which the leaf tier compares, and each Agent consumer handles it separately",
    ),
    (
        "blank-string-unset",
        "an empty string (or, for some keys, a whitespace-only one) becomes unset and others are trimmed; the \
         getter returns the raw string, which the leaf tier compares",
    ),
    (
        "requires-datadog-forwarder",
        "combines ADP-only pipeline switches; no Agent key or getter computes it",
    ),
];

/// The topology shutdown timeout, which `GetInt` returns in whole seconds.
fn stop_timeout(config: &SalukiConfiguration) -> Derived {
    whole_seconds(config.stop_timeout())
}

/// The size in bytes at which ADP rotates its own log file, translated from `log_file_max_size`.
fn log_file_max_size(config: &SalukiConfiguration) -> Derived {
    Derived::Bytes(config.control.logging.file_max_size.value)
}

/// The size in bytes at which ADP rotates the DogStatsD debug log, translated from
/// `dogstatsd_log_file_max_size`.
fn dogstatsd_log_file_max_size(config: &SalukiConfiguration) -> Derived {
    Derived::Bytes(config.domains.dogstatsd.debug_log.log_file_max_size)
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
    /// The name of the value's `LeafValue` variant, or `ByteCount` for a byte count.
    fn kind(&self) -> &'static str {
        match self {
            Derived::Exact(leaf) => kind_name(leaf),
            Derived::NotWhole { .. } => kind_name(&LeafValue::I64(0)),
            Derived::Bytes(_) => "ByteCount",
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
/// rendering, so a match of the nearest integer is a difference. A [`Derived::Bytes`] value is
/// compared through [`compare_byte_count`] instead.
fn compare_derived_result(value: &Derived, getter: Getter, result: &GetterResult) -> Verdict {
    match value {
        Derived::Bytes(adp) => compare_byte_count(*adp, getter, result),
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
                            // A failed translation has no value, so only the derivation's own getter is
                            // rejected; any other recorded getter is not compared, whatever the value.
                            let rejected = read
                                .getters
                                .iter()
                                .map(|r| {
                                    let verdict = if r.getter == derivation.getter {
                                        Verdict::AdpRejects { error: error.clone() }
                                    } else {
                                        Verdict::NotCompared {
                                            reason: Reason::OtherGetter {
                                                compared: derivation.getter,
                                            },
                                        }
                                    };
                                    (r.getter, verdict)
                                })
                                .collect();
                            // The kind does not depend on the value, so the default configuration names it.
                            ((derivation.derive)(&SalukiConfiguration::default()).kind(), rejected)
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
            match (derivation.derive)(&config) {
                Derived::Exact(leaf) => assert!(
                    LeafKind::of(&leaf).emulated().contains(&derivation.getter),
                    "{}: a {} value is not compared with {}",
                    derivation.key,
                    kind_name(&leaf),
                    derivation.getter
                ),
                Derived::Bytes(_) => assert_eq!(derivation.getter, Getter::GetSizeInBytes, "{}", derivation.key),
                Derived::NotWhole { .. } => panic!("{}: the default derives an exact value", derivation.key),
            }
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

    /// The derivation of [`DERIVATIONS`] for `key`.
    fn derivation(key: &str) -> &'static Derivation {
        DERIVATIONS
            .iter()
            .find(|d| d.key == key)
            .unwrap_or_else(|| panic!("the table has {key}"))
    }

    /// A recorded `GetSizeInBytes` result of `value`.
    fn get_size_in_bytes(value: u64) -> GetterResult {
        GetterResult::SizeInBytes(Number {
            value,
            token: value.to_string(),
        })
    }

    #[test]
    fn byte_size_rows_read_their_own_translated_field() {
        let mut config = SalukiConfiguration::default();
        config.control.logging.file_max_size.value = 1_000;
        config.domains.dogstatsd.debug_log.log_file_max_size = 2_000;
        for (key, expected) in [("log_file_max_size", 1_000), ("dogstatsd_log_file_max_size", 2_000)] {
            let row = derivation(key);
            assert_eq!(row.getter, Getter::GetSizeInBytes, "{key}");
            let value = (row.derive)(&config);
            assert!(matches!(value, Derived::Bytes(n) if n == expected), "{key}: {value:?}");
            assert_eq!(value.kind(), "ByteCount");
            assert_eq!(
                compare_derived_result(&value, row.getter, &get_size_in_bytes(expected)),
                Verdict::Match,
                "{key}"
            );
            assert_eq!(
                compare_derived_result(&value, row.getter, &get_size_in_bytes(expected + 1)),
                Verdict::Differs {
                    adp: expected.to_string(),
                    agent: (expected + 1).to_string()
                },
                "{key}"
            );
            // The size string is the leaf tier's; this tier compares only the byte count.
            assert_eq!(
                compare_derived_result(&value, Getter::GetString, &GetterResult::String("1KB".to_string())),
                Verdict::NotCompared {
                    reason: Reason::OtherGetter {
                        compared: Getter::GetSizeInBytes
                    }
                },
                "{key}"
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
