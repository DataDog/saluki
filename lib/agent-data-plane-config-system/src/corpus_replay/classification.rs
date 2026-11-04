//! Checks the compatibility classifier against the schema overlay, using values the Agent streamed.
//!
//! The corpus's unsupported, excluded and unknown cases set keys the typed configuration did not
//! model when the corpus was recorded. For every value those cases stream, the classifier must give
//! the classification that the key's current overlay declaration implies, whatever it is:
//!
//! | Overlay declaration                                                   | Classification               |
//! |-----------------------------------------------------------------------|------------------------------|
//! | `support: none`                                                       | incompatible at its severity |
//! | `support: unknown` with a severity                                    | incompatible at its severity |
//! | `support: partial` with `warn`                                        | partial                      |
//! | `full`, `partial` without `warn`, `unknown` without a severity, `excluded`, or no entry | none |
//!
//! A classification must never report a streamed value as the default. These checks compare the
//! overlay with the generated classifier, not ADP with the Agent, so a routine support change needs
//! no edit to them.

#[cfg(test)]
mod tests {
    use datadog_agent_config::classifier::{ConfigClassifier, Severity, SupportLevel};
    use datadog_agent_config_corpus::{Case, Group, Outcome};
    use datadog_agent_config_overlay_model::{self as overlay, Files, KnownEntry, SchemaOverlay};
    use serde_json::Value;

    use crate::corpus_replay::corpus;

    /// The groups whose keys the typed model left out when the corpus was recorded.
    const UNMODELED_GROUPS: [Group; 3] = [Group::Unsupported, Group::Excluded, Group::Unknown];

    /// Returns the started cases of `group`.
    fn started_cases_of(group: Group) -> impl Iterator<Item = &'static Case> {
        corpus()
            .cases
            .iter()
            .filter(move |case| case.group == group && matches!(case.outcome, Outcome::Started(_)))
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

    /// Returns the support level the overlay's declaration of `key` implies, or `None` if the
    /// classifier must not classify it (see the module's table).
    fn expected_classification(overlay: &SchemaOverlay, key: &str) -> Option<SupportLevel> {
        if overlay.excluded.contains_key(key) {
            return None;
        }
        match overlay.inventory.get(key)? {
            KnownEntry::Unsupported(u) => Some(SupportLevel::Incompatible(classifier_severity(u.severity))),
            KnownEntry::Unknown(u) => u.severity.map(|s| SupportLevel::Incompatible(classifier_severity(s))),
            KnownEntry::Partial(p) if p.warn => Some(SupportLevel::Partial),
            KnownEntry::Full(_) | KnownEntry::Partial(_) => None,
        }
    }

    /// Checks that the classifier reads every value `group`'s cases streamed as the overlay declares.
    /// Returns one message per disagreement, and the number of values checked.
    fn classification_mismatches(
        overlay: &SchemaOverlay, classifier: &ConfigClassifier, group: Group,
    ) -> (Vec<String>, usize) {
        let mut mismatches = Vec::new();
        let mut checked = 0;
        for case in started_cases_of(group) {
            for (key, value) in streamed_values(case) {
                let expected = expected_classification(overlay, key);
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

    /// Every value an unsupported, excluded or unknown case streamed classifies as the key's current
    /// overlay declaration implies, and each of those groups has at least one value checked.
    #[test]
    fn recorded_unmodeled_values_classify_as_the_overlay_declares() {
        let overlay = SchemaOverlay::load(Files::default()).unwrap_or_else(|e| panic!("loading the overlay: {e}"));
        let classifier = ConfigClassifier::new();
        for group in UNMODELED_GROUPS {
            let (mismatches, checked) = classification_mismatches(&overlay, &classifier, group);
            assert!(
                mismatches.is_empty(),
                "{group:?} values the classifier disagrees on:\n{}",
                mismatches.join("\n")
            );
            assert!(checked > 0, "no streamed value of a {group:?} case was checked");
        }
    }
}
