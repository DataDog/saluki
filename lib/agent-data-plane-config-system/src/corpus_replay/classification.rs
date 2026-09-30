//! Checks what ADP makes of the keys the corpus sets but the typed model leaves out: the schema keys ADP does not
//! support or whose support is undetermined, the schema keys the overlay excludes, and keys that are not in the schema
//! at all.
//!
//! The leaf tier reports these keys only as not modeled. The tests here pin the two things ADP does with them: the
//! classifier behind the compatibility check reads each key as the overlay declares it, and replaying a case that
//! sets them fails no stage apart from the blank `api_key` validation every case without an `api_key` gets.

#[cfg(test)]
mod tests {
    use datadog_agent_config::classifier::{ConfigClassifier, Severity, SupportLevel};
    use datadog_agent_config_corpus::{Case, Group, Outcome};
    use datadog_agent_config_overlay_model::{self as overlay, Files, KnownEntry, SchemaOverlay};
    use serde_json::Value;

    use crate::corpus_replay::corpus;
    use crate::corpus_replay::driver::{replay_case, Stage};
    use crate::system::Error;

    /// The groups whose keys the typed model leaves out.
    const UNMODELED_GROUPS: [Group; 3] = [Group::Unsupported, Group::Excluded, Group::Unknown];

    /// Returns the started cases of `group`, and the names of the cases of `group` that failed to start.
    fn cases_of(group: Group) -> (Vec<&'static Case>, Vec<&'static str>) {
        let mut started = Vec::new();
        let mut failed = Vec::new();
        for case in corpus().cases.iter().filter(|case| case.group == group) {
            match &case.outcome {
                Outcome::Started(_) => started.push(case),
                Outcome::StartupError(_) => failed.push(case.name.as_str()),
            }
        }
        (started, failed)
    }

    /// Returns every value the case streamed for each of its key lines, as (key, value). A key line that streamed
    /// nothing yields one (key, `None`).
    fn streamed_values(case: &Case) -> Vec<(&str, Option<&Value>)> {
        let Outcome::Started(started) = &case.outcome else {
            panic!("case {} did not start", case.name);
        };
        let mut values = Vec::new();
        for line in &started.keys {
            let settings = line
                .snapshot
                .iter()
                .chain(line.events.iter().map(|event| &event.setting));
            let before = values.len();
            values.extend(
                settings
                    .filter_map(|s| s.value.as_ref())
                    .map(|v| (line.key.as_str(), Some(v))),
            );
            if values.len() == before {
                values.push((line.key.as_str(), None));
            }
        }
        values
    }

    fn classifier_severity(severity: overlay::Severity) -> Severity {
        match severity {
            overlay::Severity::Low => Severity::Low,
            overlay::Severity::Medium => Severity::Medium,
            overlay::Severity::High => Severity::High,
        }
    }

    /// Returns the support level `classify` should return for `key` given the overlay, or `None` for a key the
    /// classifier does not know. A key of undetermined support is classified only when the overlay estimates its
    /// severity.
    fn expected_classification(overlay: &SchemaOverlay, key: &str) -> Result<Option<SupportLevel>, String> {
        if overlay.excluded.contains_key(key) {
            return Ok(None);
        }
        match overlay.inventory.get(key) {
            None => Ok(None),
            Some(KnownEntry::Unsupported(u)) => Ok(Some(SupportLevel::Incompatible(classifier_severity(u.severity)))),
            Some(KnownEntry::Unknown(u)) => Ok(u.severity.map(|s| SupportLevel::Incompatible(classifier_severity(s)))),
            Some(KnownEntry::Full(_) | KnownEntry::Partial(_)) => Err("a supported key".to_string()),
        }
    }

    /// Checks that each key of `group`'s cases has the overlay entry the group is generated from, and that the
    /// classifier reads every streamed value of it as expected. Returns one message per disagreement, and the number
    /// of values checked.
    fn classification_mismatches(group: Group) -> (Vec<String>, usize) {
        let overlay = SchemaOverlay::load(Files::default()).unwrap_or_else(|e| panic!("loading the overlay: {e}"));
        let classifier = ConfigClassifier::new();
        let (cases, _) = cases_of(group);
        let mut mismatches = Vec::new();
        let mut checked = 0;
        for case in cases {
            for (key, value) in streamed_values(case) {
                let in_group = match group {
                    Group::Unsupported => matches!(
                        overlay.inventory.get(key),
                        Some(KnownEntry::Unsupported(_) | KnownEntry::Unknown(_))
                    ),
                    Group::Excluded => overlay.excluded.contains_key(key),
                    Group::Unknown => !overlay.inventory.contains_key(key) && !overlay.excluded.contains_key(key),
                    other => panic!("group {other:?} has modeled keys"),
                };
                if !in_group {
                    mismatches.push(format!(
                        "case {}: key {key}: the overlay does not declare it as a {group:?} key",
                        case.name
                    ));
                    continue;
                }
                let expected = match expected_classification(&overlay, key) {
                    Ok(expected) => expected,
                    Err(what) => {
                        mismatches.push(format!("case {}: key {key}: the overlay declares {what}", case.name));
                        continue;
                    }
                };
                // The classifier's `None` does not depend on the value; a key that streamed nothing is checked with
                // `null` only when `None` is expected, since a non-default value is what the case is for.
                let value = match (value, expected) {
                    (Some(value), _) => value,
                    (None, None) => &Value::Null,
                    (None, Some(_)) => {
                        mismatches.push(format!(
                            "case {}: key {key}: expected a streamed value, got none",
                            case.name
                        ));
                        continue;
                    }
                };
                checked += 1;
                let actual = classifier.classify(key, value);
                let agrees = match (&actual, expected) {
                    (None, None) => true,
                    (Some(actual), Some(level)) => actual.support_level == level && !actual.is_default,
                    _ => false,
                };
                if !agrees {
                    let actual = actual.map(|c| format!("{:?} with is_default {}", c.support_level, c.is_default));
                    let expected = expected.map(|level| format!("{level:?} with is_default false"));
                    mismatches.push(format!(
                        "case {}: key {key}: classify({value}) expected {expected:?}, got {actual:?}",
                        case.name
                    ));
                }
            }
        }
        (mismatches, checked)
    }

    fn assert_classified_as_the_overlay_declares(group: Group) {
        let (mismatches, checked) = classification_mismatches(group);
        assert!(
            mismatches.is_empty(),
            "{group:?} keys the classifier disagrees on:\n{}",
            mismatches.join("\n")
        );
        assert!(checked > 0, "no streamed value of a {group:?} case was checked");
    }

    /// Every unsupported key the corpus streams classifies as incompatible, at the overlay's severity, and not as
    /// its default, so the compatibility check reports it. The group also holds the keys whose support the overlay
    /// has not determined: those classify the same way when the overlay estimates a severity, and not at all
    /// otherwise.
    #[test]
    fn unsupported_corpus_keys_classify_as_incompatible_at_the_overlay_severity() {
        assert_classified_as_the_overlay_declares(Group::Unsupported);
    }

    /// Every excluded key the corpus streams is unknown to the classifier, so the compatibility check skips it.
    #[test]
    fn excluded_corpus_keys_are_not_classified() {
        assert_classified_as_the_overlay_declares(Group::Excluded);
    }

    /// Every key outside the schema that the corpus sets is unknown to the classifier, so the compatibility check
    /// skips it.
    #[test]
    fn unknown_corpus_keys_are_not_classified() {
        assert_classified_as_the_overlay_declares(Group::Unknown);
    }

    /// Replaying a case that sets only keys the typed model leaves out fails no stage, apart from the blank
    /// `api_key` validation failure every case without an `api_key` gets.
    #[test]
    fn unmodeled_corpus_keys_cause_no_replay_failure() {
        let corpus = corpus();
        let blank_api_key = Error::MissingApiKey.to_string();
        let mut failures = Vec::new();
        for group in UNMODELED_GROUPS {
            let (cases, not_started) = cases_of(group);
            assert!(!cases.is_empty(), "the corpus has no started {group:?} case");
            assert!(
                not_started.is_empty(),
                "{group:?} cases that failed to start: {not_started:?}"
            );
            for case in cases {
                let keys = streamed_values(case)
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>();
                let replayed = replay_case(corpus, &case.name).unwrap_or_else(|e| panic!("case {}: {e}", case.name));
                for (position, step) in replayed.steps.iter().enumerate() {
                    let Some(failure) = &step.failure else { continue };
                    if failure.stage == Stage::Validate && failure.error == blank_api_key {
                        continue;
                    }
                    failures.push(format!(
                        "case {} (keys {keys:?}): step {position} (key {:?}): expected no failure, got {:?}: {}",
                        case.name, step.key, failure.stage, failure.error
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "replay failures:\n{}", failures.join("\n"));
    }
}
