//! Builds the exact `ConfigEvent` stream the Agent would have sent for one corpus case
//! (record.md §3.2, §4.2), from the typed corpus (`datadog_agent_config_corpus::Corpus`).

use datadog_agent_config_corpus::{Corpus, Outcome, Setting};
use datadog_protos::agent::{config_event, ConfigEvent, ConfigSetting, ConfigSnapshot, ConfigUpdate};
use prost_types::value::Kind;
use prost_types::{ListValue, Struct as ProstStruct, Value as ProstValue};
use serde_json::{Number, Value as JsonValue};

/// What building a case's event stream produced.
#[derive(Debug, PartialEq)]
pub(crate) enum CaseEvents {
    /// The case's construction failed at startup (record.md §4.1); nothing was ever streamed.
    ///
    /// Kept distinct from `Started(vec![])` so a caller can never mistake "nothing recorded" for "an
    /// empty stream."
    StartupFailed,
    /// The case started; this is the exact stream the Agent would have sent.
    Started(Vec<ConfigEvent>),
}

/// Builds the `ConfigEvent` stream for the case named `case_name` in `corpus`.
///
/// `base` is the caller's chosen base `sequence_id` for the case's first snapshot: the corpus does
/// not record the Agent's own base id (ADP ignores `sequence_id` on the first snapshot), so every
/// later sequence id in the stream is `base` plus the case's own recorded deltas (record.md §4.2).
///
/// Returns an error, naming the case and (where relevant) the key, when the case's recorded events
/// cannot be turned into a single well-ordered, unambiguous stream: a duplicate `sequence_id`, a
/// `sequence_id` that overflows `i32`, an event attributed to an update that does not exist, or a
/// value that cannot be represented in the wire format.
pub(crate) fn build_events(corpus: &Corpus, case_name: &str, base: i32) -> Result<CaseEvents, String> {
    let case = corpus
        .case(case_name)
        .ok_or_else(|| format!("case {case_name:?}: not found in the corpus"))?;

    let started = match &case.outcome {
        Outcome::StartupError(_) => return Ok(CaseEvents::StartupFailed),
        Outcome::Started(started) => started,
    };

    // record.md §3.2: the three-layer rebuild (baseline, then side effects, then the case's own
    // key lines) is exactly `Corpus::first_snapshot`.
    let snapshot_map = corpus
        .first_snapshot(case_name)
        .ok_or_else(|| format!("case {case_name:?}: has no first snapshot to rebuild"))?;

    let mut settings = Vec::with_capacity(snapshot_map.len());
    for (key, setting) in snapshot_map {
        settings.push(config_setting(case_name, key, setting)?);
    }

    let mut events = vec![ConfigEvent {
        event: Some(config_event::Event::Snapshot(ConfigSnapshot {
            origin: started.origin.clone(),
            sequence_id: base,
            settings,
        })),
    }];

    // record.md §4.2: update i's `before` is `base` plus the sum of every earlier update's
    // `seq_delta`; update 0's `before` equals `base` itself. An event's absolute sequence id is
    // that offset plus its own (update-relative) `seq`.
    let mut update_offsets = Vec::with_capacity(started.updates.len());
    let mut offset = i64::from(base);
    for update in &started.updates {
        update_offsets.push(offset);
        offset += update.seq_delta as i64;
    }

    let mut positioned: Vec<(i64, &str, &Setting)> = Vec::new();
    for key_line in &started.keys {
        for event in &key_line.events {
            let update_offset = update_offsets.get(event.update).ok_or_else(|| {
                format!(
                    "case {case_name:?} key {:?}: event attributes to update {}, but the case only has {} updates",
                    key_line.key,
                    event.update,
                    started.updates.len()
                )
            })?;
            let sequence_id = update_offset + event.seq.get() as i64;
            positioned.push((sequence_id, key_line.key.as_str(), &event.setting));
        }
    }
    positioned.sort_by_key(|(sequence_id, ..)| *sequence_id);

    for window in positioned.windows(2) {
        let [(seq_a, key_a, _), (seq_b, key_b, _)] = window else {
            unreachable!("windows(2) always yields two elements")
        };
        if seq_a == seq_b {
            return Err(format!(
                "case {case_name:?}: keys {key_a:?} and {key_b:?} both land on sequence_id {seq_a}"
            ));
        }
    }

    for (sequence_id, key, setting) in positioned {
        let sequence_id = i32::try_from(sequence_id)
            .map_err(|_| format!("case {case_name:?} key {key:?}: sequence_id {sequence_id} overflows i32"))?;
        events.push(ConfigEvent {
            event: Some(config_event::Event::Update(ConfigUpdate {
                origin: started.origin.clone(),
                sequence_id,
                setting: Some(config_setting(case_name, key, setting)?),
            })),
        });
    }

    Ok(CaseEvents::Started(events))
}

/// Converts one corpus [`Setting`] for `key` into the wire `ConfigSetting`.
fn config_setting(case_name: &str, key: &str, setting: &Setting) -> Result<ConfigSetting, String> {
    // record.md §5.1: the real Agent proto has an `unset_source` field (proto field 4) that
    // saluki's vendored proto does not define; prost silently skips unknown fields when decoding,
    // so the loader drops `unset_source` here too, to match what ADP actually sees on the wire.
    // This is the only place `Setting::unset_source` is read and discarded.
    let value = match &setting.value {
        Some(value) => {
            Some(json_to_prost_value(value).map_err(|e| format!("case {case_name:?} key {key:?}: value: {e}"))?)
        }
        None => None,
    };

    Ok(ConfigSetting {
        source: setting.source.as_str().to_string(),
        key: key.to_string(),
        value,
    })
}

/// Converts a `serde_json::Value` into a `google.protobuf.Value`, strictly.
///
/// JSON numbers become `NumberValue(f64)`; a number with no exact `f64` representation (for
/// example an integer above 2^53 that does not round-trip through `f64`) is an error rather than a
/// silent rounding.
fn json_to_prost_value(value: &JsonValue) -> Result<ProstValue, String> {
    let kind = match value {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(b) => Kind::BoolValue(*b),
        JsonValue::Number(n) => Kind::NumberValue(exact_f64(n)?),
        JsonValue::String(s) => Kind::StringValue(s.clone()),
        JsonValue::Array(items) => Kind::ListValue(ListValue {
            values: items.iter().map(json_to_prost_value).collect::<Result<Vec<_>, _>>()?,
        }),
        JsonValue::Object(fields) => Kind::StructValue(ProstStruct {
            fields: fields
                .iter()
                .map(|(k, v)| Ok((k.clone(), json_to_prost_value(v)?)))
                .collect::<Result<_, String>>()?,
        }),
    };
    Ok(ProstValue { kind: Some(kind) })
}

/// Converts a JSON number to `f64`, returning an error rather than rounding when the conversion is not exact.
///
/// A number `serde_json` stored as an integer must round-trip through `f64` unchanged; a number it
/// stored as a float is already an `f64` by construction, so it is exact by definition.
fn exact_f64(n: &Number) -> Result<f64, String> {
    if let Some(i) = n.as_i64() {
        let as_float = i as f64;
        return if as_float as i64 == i {
            Ok(as_float)
        } else {
            Err(format!("integer {i} has no exact f64 representation"))
        };
    }
    if let Some(u) = n.as_u64() {
        let as_float = u as f64;
        return if as_float as u64 == u {
            Ok(as_float)
        } else {
            Err(format!("integer {u} has no exact f64 representation"))
        };
    }
    n.as_f64()
        .ok_or_else(|| format!("number {n} is not representable as f64"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use datadog_agent_config_corpus::read;

    use super::*;

    const HEADER: &str = r#"{"agent_commit":"281d921619d52ce7b99aef40607285992c9c2e89","container_image":"i","containerized":false,"features":[],"format":1,"go_version":"go1.26.7","goarch":"arm64","goos":"linux","inputs_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","type":"header"}"#;

    const BASELINE: &str = r#"{"case":"baseline-default","group":"baseline","inputs":{},"origin":"datadog.yaml","type":"case","why":[]}
{"case":"baseline-default","key":"a","reads":{"snapshot":{"getters":[{"getter":"GetInt","result":1}],"go_type":"int"}},"snapshot":{"source":"default","value":1},"type":"key"}
{"case":"baseline-default","key":"b","reads":{"snapshot":{"getters":[{"getter":"GetString","result":"x"}],"go_type":"string"}},"snapshot":{"source":"default","value":"x"},"type":"key"}
{"case":"baseline-default","key":"c","reads":{"snapshot":{"getters":[{"getter":"GetBool","result":true}],"go_type":"bool"}},"snapshot":{"source":"default","value":true},"type":"key"}"#;

    /// Builds a tiny in-memory corpus from `HEADER` and `BASELINE` plus the caller's own lines,
    /// through the strict reader (never by hand-building `#[non_exhaustive]` model structs).
    fn corpus_with(lines: &str) -> Corpus {
        let bytes = format!("{HEADER}\n{BASELINE}\n{lines}\n").into_bytes();
        read(&bytes).unwrap_or_else(|v| panic!("fixture corpus should be well-formed: {v:#?}"))
    }

    fn config_event_snapshot(event: &ConfigEvent) -> &ConfigSnapshot {
        match &event.event {
            Some(config_event::Event::Snapshot(s)) => s,
            other => panic!("expected a ConfigSnapshot event, got {other:?}"),
        }
    }

    fn config_event_update(event: &ConfigEvent) -> &ConfigUpdate {
        match &event.event {
            Some(config_event::Event::Update(u)) => u,
            other => panic!("expected a ConfigUpdate event, got {other:?}"),
        }
    }

    /// The test-only inverse of [`json_to_prost_value`], kept separate from the crate's
    /// `proto_value_to_serde_value` (which collapses an absent value into `Value::Null` for
    /// `ConfigSetting`'s purposes): here `None` and `Some(Value::Null)` must stay distinguishable,
    /// since the corpus itself distinguishes "the proto `value` field is unset" from "it is null."
    fn prost_value_to_json(value: &ProstValue) -> JsonValue {
        match value.kind.as_ref().expect("value always has a kind in these tests") {
            Kind::NullValue(_) => JsonValue::Null,
            Kind::NumberValue(n) => JsonValue::from(*n),
            Kind::StringValue(s) => JsonValue::String(s.clone()),
            Kind::BoolValue(b) => JsonValue::Bool(*b),
            Kind::StructValue(s) => JsonValue::Object(
                s.fields
                    .iter()
                    .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
                    .collect(),
            ),
            Kind::ListValue(l) => JsonValue::Array(l.values.iter().map(prost_value_to_json).collect()),
        }
    }

    fn config_setting_to_pair(setting: &ConfigSetting) -> (String, Option<JsonValue>) {
        (setting.source.clone(), setting.value.as_ref().map(prost_value_to_json))
    }

    /// Compares two JSON values the way record.md's round trip must: numbers compare as `f64`
    /// bit-for-bit (so `1` and `1.0` are the same wire value), everything else structurally.
    fn json_values_match(a: &JsonValue, b: &JsonValue) -> bool {
        match (a, b) {
            (JsonValue::Number(x), JsonValue::Number(y)) => {
                x.as_f64().map(f64::to_bits) == y.as_f64().map(f64::to_bits)
            }
            (JsonValue::Array(x), JsonValue::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_values_match(a, b))
            }
            (JsonValue::Object(x), JsonValue::Object(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_values_match(v, w)))
            }
            _ => a == b,
        }
    }

    fn json_options_match(a: &Option<JsonValue>, b: &Option<JsonValue>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => json_values_match(a, b),
            (None, None) => true,
            _ => false,
        }
    }

    /// Every case in the real corpus, round-tripped: the events `build_events` produces, decoded
    /// back with the test-only inverse, must equal what the corpus itself records.
    #[test]
    fn round_trip_over_every_corpus_case() {
        let corpus = crate::corpus_replay::corpus();

        const BASE: i32 = 1_000_000;
        // One entry per (case, key) that had at least one `unset_source` in the corpus, counted
        // once regardless of how many events on that key line carried it.
        let mut unset_source_dropped: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();

        for case in &corpus.cases {
            let outcome =
                build_events(corpus, &case.name, BASE).unwrap_or_else(|e| panic!("case {:?}: {e}", case.name));

            let started = match &case.outcome {
                Outcome::StartupError(_) => {
                    assert_eq!(outcome, CaseEvents::StartupFailed, "case {:?}", case.name);
                    continue;
                }
                Outcome::Started(started) => {
                    let CaseEvents::Started(_) = &outcome else {
                        panic!(
                            "case {:?}: started in the corpus but build_events said it failed startup",
                            case.name
                        )
                    };
                    started
                }
            };
            let CaseEvents::Started(events) = outcome else {
                unreachable!("handled above")
            };

            // Layer 1: the rebuilt first snapshot.
            let snapshot = config_event_snapshot(&events[0]);
            assert_eq!(snapshot.origin, started.origin, "case {:?}", case.name);
            assert_eq!(snapshot.sequence_id, BASE, "case {:?}", case.name);

            let expected_snapshot = corpus
                .first_snapshot(&case.name)
                .unwrap_or_else(|| panic!("case {:?}: no first snapshot in the corpus", case.name));
            assert_eq!(
                snapshot.settings.len(),
                expected_snapshot.len(),
                "case {:?}: snapshot key count",
                case.name
            );
            let rebuilt: BTreeMap<&str, (String, Option<JsonValue>)> = snapshot
                .settings
                .iter()
                .map(|s| (s.key.as_str(), config_setting_to_pair(s)))
                .collect();
            for (key, expected) in &expected_snapshot {
                let (source, value) = rebuilt
                    .get(*key)
                    .unwrap_or_else(|| panic!("case {:?} key {key:?}: missing from rebuilt snapshot", case.name));
                assert_eq!(source, expected.source.as_str(), "case {:?} key {key:?}", case.name);
                assert!(
                    json_options_match(value, &expected.value),
                    "case {:?} key {key:?}: value {value:?} != {:?}",
                    case.name,
                    expected.value
                );
                if expected.unset_source.is_some() {
                    unset_source_dropped.insert((case.name.as_str(), key));
                }
            }

            // Layer 2: every later event, in the exact position record.md's sequencing dictates.
            let mut update_offsets = Vec::with_capacity(started.updates.len());
            let mut offset = i64::from(BASE);
            for update in &started.updates {
                update_offsets.push(offset);
                offset += update.seq_delta as i64;
            }
            let mut expected_events: Vec<(i64, &str, &Setting)> = Vec::new();
            for key_line in &started.keys {
                for event in &key_line.events {
                    let seq_id = update_offsets[event.update] + event.seq.get() as i64;
                    expected_events.push((seq_id, key_line.key.as_str(), &event.setting));
                }
            }
            expected_events.sort_by_key(|(seq_id, ..)| *seq_id);

            assert_eq!(
                events.len() - 1,
                expected_events.len(),
                "case {:?}: update event count",
                case.name
            );
            for (position, (event, (expected_seq, expected_key, expected_setting))) in
                events[1..].iter().zip(&expected_events).enumerate()
            {
                let update = config_event_update(event);
                assert_eq!(
                    i64::from(update.sequence_id),
                    *expected_seq,
                    "case {:?} position {position}",
                    case.name
                );
                let setting = update
                    .setting
                    .as_ref()
                    .unwrap_or_else(|| panic!("case {:?} position {position}: update event has no setting", case.name));
                assert_eq!(&setting.key, expected_key, "case {:?} position {position}", case.name);
                let (source, value) = config_setting_to_pair(setting);
                assert_eq!(
                    source,
                    expected_setting.source.as_str(),
                    "case {:?} position {position}",
                    case.name
                );
                assert!(
                    json_options_match(&value, &expected_setting.value),
                    "case {:?} position {position}: value {value:?} != {:?}",
                    case.name,
                    expected_setting.value
                );
                if expected_setting.unset_source.is_some() {
                    unset_source_dropped.insert((case.name.as_str(), *expected_key));
                }
            }
        }

        // A change to this count means the corpus started (or stopped) recording `unset_source`
        // settings; it does not mean the loader is wrong, but it should be looked at.
        assert_eq!(
            unset_source_dropped.len(),
            5,
            "count of settings whose unset_source the loader dropped"
        );
    }

    /// Checks the loader over the real corpus with rules that do not repeat its own arithmetic: the
    /// snapshot is resolved key by key, by layer precedence, and the update events must fill the
    /// sequence range with no gap, keep each key line's arrival order, and never go back to an
    /// earlier update.
    #[test]
    fn every_corpus_case_matches_independent_stream_invariants() {
        let corpus = crate::corpus_replay::corpus();
        let Outcome::Started(baseline) = &corpus
            .case(datadog_agent_config_corpus::BASELINE_CASE)
            .expect("baseline case")
            .outcome
        else {
            panic!("the baseline case started")
        };

        const BASE: i32 = 7;
        for case in &corpus.cases {
            let Outcome::Started(started) = &case.outcome else {
                continue;
            };
            let CaseEvents::Started(events) = build_events(corpus, &case.name, BASE).expect("builds") else {
                panic!("case {:?} started", case.name)
            };

            // Snapshot: the case's own key line wins, then its side effects, then the baseline.
            let own = |key: &str| started.keys.iter().find(|k| k.key == key).map(|k| k.snapshot.as_ref());
            let side = |key: &str| {
                started
                    .side_effects
                    .iter()
                    .find(|e| e.key == key)
                    .map(|e| e.setting.as_ref())
            };
            let base = |key: &str| {
                baseline
                    .keys
                    .iter()
                    .find(|k| k.key == key)
                    .and_then(|k| k.snapshot.as_ref())
            };
            let resolve = |key: &str| own(key).or_else(|| side(key)).unwrap_or_else(|| base(key));

            let snapshot = config_event_snapshot(&events[0]);
            let mut candidates: Vec<&str> = baseline.keys.iter().map(|k| k.key.as_str()).collect();
            candidates.extend(started.side_effects.iter().map(|e| e.key.as_str()));
            candidates.extend(started.keys.iter().map(|k| k.key.as_str()));
            candidates.sort_unstable();
            candidates.dedup();
            let expected: Vec<&str> = candidates.into_iter().filter(|k| resolve(k).is_some()).collect();
            let got: Vec<&str> = snapshot.settings.iter().map(|s| s.key.as_str()).collect();
            assert_eq!(got, expected, "case {:?}: snapshot keys", case.name);
            for setting in &snapshot.settings {
                let want = resolve(&setting.key).expect("resolved above");
                let (source, value) = config_setting_to_pair(setting);
                assert_eq!(
                    source,
                    want.source.as_str(),
                    "case {:?} key {:?}",
                    case.name,
                    setting.key
                );
                assert!(
                    json_options_match(&value, &want.value),
                    "case {:?} key {:?}",
                    case.name,
                    setting.key
                );
            }

            // Updates: no update timed out in this corpus, so every notification was recorded and
            // the events fill `BASE + 1 ..= BASE + total` exactly.
            assert!(started.updates.iter().all(|u| !u.timed_out), "case {:?}", case.name);
            let total: u64 = started.updates.iter().map(|u| u.seq_delta).sum();
            let ids: Vec<i64> = events[1..]
                .iter()
                .map(|e| i64::from(config_event_update(e).sequence_id))
                .collect();
            let want_ids: Vec<i64> = (1..=total as i64).map(|n| i64::from(BASE) + n).collect();
            assert_eq!(ids, want_ids, "case {:?}: update sequence ids", case.name);

            for key_line in &started.keys {
                let streamed: Vec<(String, Option<JsonValue>)> = events[1..]
                    .iter()
                    .filter_map(|e| config_event_update(e).setting.as_ref())
                    .filter(|s| s.key == key_line.key)
                    .map(config_setting_to_pair)
                    .collect();
                assert_eq!(
                    streamed.len(),
                    key_line.events.len(),
                    "case {:?} key {:?}",
                    case.name,
                    key_line.key
                );
                for ((source, value), recorded) in streamed.iter().zip(&key_line.events) {
                    assert_eq!(
                        source,
                        recorded.setting.source.as_str(),
                        "case {:?} key {:?}",
                        case.name,
                        key_line.key
                    );
                    assert!(
                        json_options_match(value, &recorded.setting.value),
                        "case {:?}",
                        case.name
                    );
                }
            }

            // Stream order never returns to an earlier update.
            let key_update =
                |key: &str, nth: usize| started.keys.iter().find(|k| k.key == key).map(|k| k.events[nth].update);
            let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            let mut last_update = 0;
            for event in &events[1..] {
                let key = config_event_update(event)
                    .setting
                    .as_ref()
                    .expect("setting")
                    .key
                    .as_str();
                let nth = seen.entry(key).or_insert(0);
                let update = key_update(key, *nth).expect("key line");
                *nth += 1;
                assert!(update >= last_update, "case {:?}: update order", case.name);
                last_update = update;
            }
        }
    }

    #[test]
    fn a_startup_error_case_is_reported_distinctly_from_an_empty_stream() {
        let lines = r#"{"case":"z-startup-error","group":"behavior","inputs":{"keys":[{"key":"a"}]},"startup_error":"construction blew up","type":"case","why":["w"]}"#;
        let corpus = corpus_with(lines);

        let outcome = build_events(&corpus, "z-startup-error", 1).expect("startup failure is not a build error");
        assert_eq!(outcome, CaseEvents::StartupFailed);
        assert_ne!(outcome, CaseEvents::Started(vec![]));
    }

    #[test]
    fn absent_side_effects_and_null_snapshots_both_remove_baseline_keys() {
        let lines = r#"{"case":"z-removed-keys","group":"behavior","inputs":{},"origin":"datadog.yaml","side_effects":[{"absent":true,"key":"a"}],"type":"case","why":["w"]}
{"case":"z-removed-keys","key":"b","reads":{"snapshot":{"getters":[{"getter":"GetString","result":"x"}],"go_type":"string","source":"unknown"}},"snapshot":null,"type":"key"}"#;
        let corpus = corpus_with(lines);

        let CaseEvents::Started(events) = build_events(&corpus, "z-removed-keys", 1).expect("case started") else {
            panic!("case started")
        };
        let snapshot = config_event_snapshot(&events[0]);
        // Baseline has a, b, c; the side effect removes a, the case's own null key line removes b.
        // Only c, untouched, survives.
        assert_eq!(snapshot.settings.len(), 1, "settings: {:?}", snapshot.settings);
        assert_eq!(snapshot.settings[0].key, "c");
    }

    #[test]
    fn a_non_representable_number_is_an_error_not_a_rounding() {
        // 2^53 + 1: the smallest positive integer with no exact f64 representation.
        let lines = r#"{"case":"z-huge-number","group":"behavior","inputs":{"yaml":"f: 9007199254740993\n"},"origin":"datadog.yaml","type":"case","why":["w"]}
{"case":"z-huge-number","key":"f","reads":{"snapshot":{"getters":[{"getter":"GetInt64","result":9007199254740993}],"go_type":"int64"}},"snapshot":{"source":"file","value":9007199254740993},"type":"key"}"#;
        let corpus = corpus_with(lines);

        let err = build_events(&corpus, "z-huge-number", 1).expect_err("non-representable number must error");
        assert!(err.contains("z-huge-number"), "{err}");
        assert!(err.contains('f'), "{err}");
    }

    #[test]
    fn duplicate_sequence_ids_across_keys_are_an_error() {
        // A single update names key "d", so key "d"'s event omits `update` (it is the only
        // candidate) and key "e"'s event (which no update names) must give `update` explicitly.
        // Both land on the same update, same seq: the same absolute sequence_id from two keys.
        let lines = r#"{"case":"z-duplicate-seq","group":"behavior","inputs":{"updates":[{"key":"d","source":"remote-config","value":2}]},"origin":"datadog.yaml","type":"case","updates":[{"seq_delta":1}],"why":["w"]}
{"case":"z-duplicate-seq","events":[{"seq":1,"source":"remote-config","value":2}],"key":"d","reads":{"final":{"getters":[{"getter":"GetInt","result":2}],"go_type":"int"},"snapshot":{"getters":[{"getter":"GetInt","result":1}],"go_type":"int"}},"snapshot":{"source":"default","value":1},"type":"key"}
{"case":"z-duplicate-seq","events":[{"seq":1,"source":"remote-config","update":0,"value":3}],"key":"e","reads":{"final":{"getters":[{"getter":"GetInt","result":3}],"go_type":"int"},"snapshot":{"getters":[{"getter":"GetInt","result":1}],"go_type":"int"}},"snapshot":{"source":"default","value":1},"type":"key"}"#;
        let corpus = corpus_with(lines);

        let err = build_events(&corpus, "z-duplicate-seq", 1).expect_err("duplicate sequence_id must error");
        assert!(err.contains("z-duplicate-seq"), "{err}");
    }

    #[test]
    fn an_events_sequence_id_offsets_by_every_earlier_updates_seq_delta() {
        let lines = r#"{"case":"z-offset","group":"behavior","inputs":{"updates":[{"key":"a","source":"remote-config","value":9},{"key":"g","source":"remote-config","value":5}]},"origin":"datadog.yaml","type":"case","updates":[{"seq_delta":3},{"seq_delta":2}],"why":["w"]}
{"case":"z-offset","events":[{"seq":2,"source":"remote-config","value":5}],"key":"g","reads":{"final":{"getters":[{"getter":"GetInt","result":5}],"go_type":"int"},"snapshot":{"getters":[{"getter":"GetInt","result":0}],"go_type":"int","source":"unknown"}},"snapshot":null,"type":"key"}"#;
        let corpus = corpus_with(lines);

        let CaseEvents::Started(events) = build_events(&corpus, "z-offset", 100).expect("case started") else {
            panic!("case started")
        };
        assert_eq!(events.len(), 2, "events: {events:?}");
        let update = config_event_update(&events[1]);
        // base(100) + update0.seq_delta(3) + this event's own seq(2) = 105, never just base + seq.
        assert_eq!(update.sequence_id, 105);
    }

    /// Feeding a loader-built snapshot and update through the moved conversions mirrors what
    /// `remote_agent.rs` does with the real Agent stream (remote_agent.rs:309-317).
    #[test]
    fn moved_conversions_turn_loader_events_into_the_expected_config_update_shapes() {
        let lines = r#"{"case":"z-shapes","group":"behavior","inputs":{"updates":[{"key":"a","source":"remote-config","value":9}]},"origin":"datadog.yaml","type":"case","updates":[{"seq_delta":1}],"why":["w"]}
{"case":"z-shapes","events":[{"seq":1,"source":"remote-config","value":9}],"key":"a","reads":{"final":{"getters":[{"getter":"GetInt","result":9}],"go_type":"int"},"snapshot":{"getters":[{"getter":"GetInt","result":1}],"go_type":"int"}},"snapshot":{"source":"default","value":1},"type":"key"}"#;
        let corpus = corpus_with(lines);

        let CaseEvents::Started(events) = build_events(&corpus, "z-shapes", 1).expect("case started") else {
            panic!("case started")
        };

        let snapshot = config_event_snapshot(&events[0]);
        let settings = crate::snapshot_to_settings(snapshot);
        let update = saluki_config::dynamic::ConfigUpdate::Snapshot(settings.clone());
        assert!(matches!(update, saluki_config::dynamic::ConfigUpdate::Snapshot(_)));
        assert_eq!(settings.len(), 3, "settings: {settings:?}");

        let update_event = config_event_update(&events[1]);
        let update_setting = update_event.setting.as_ref().expect("update has a setting");
        let converted = crate::setting_to_config_setting(update_setting);
        let wrapped = saluki_config::dynamic::ConfigUpdate::Partial(converted.clone());
        assert!(matches!(wrapped, saluki_config::dynamic::ConfigUpdate::Partial(_)));
        assert_eq!(converted.key, "a");
        assert_eq!(converted.value, JsonValue::from(9));
        assert_eq!(converted.provenance, saluki_config::dynamic::Provenance::Explicit);
    }
}
