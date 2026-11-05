//! Steps a case procedure may name, built from their case-file fields.
//!
//! Steps register as [`StepFactory`] values, which the case loader calls to build each step.
//! A step names participants, processes and facts, independently of any particular product or realization.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::step::StepFactory;

mod attempts;
mod cli;
pub mod compare;
mod config_key_equals;
mod config_set;
mod dogstatsd_replay;
mod exec;
mod exits_with;
mod file_contains;
mod http_check;
mod intake_has_forwarded_packets;
mod intake_has_metric;
mod intake_has_service_check;
mod log_contains;
mod message;
pub mod metrics;
mod port_listening;
mod process_stable_for;
mod send;

/// Steps available to case files, sorted by name.
pub const REGISTERED_STEPS: &[&dyn StepFactory] = &[
    &cli::CliFactory,
    &compare::CompareFactory,
    &config_key_equals::ConfigKeyEqualsFactory,
    &config_set::ConfigSetFactory,
    &dogstatsd_replay::DogstatsdReplayFactory,
    &exec::ExecFactory,
    &exits_with::ExitsWithFactory,
    &file_contains::FileContainsFactory,
    &http_check::HttpCheckFactory,
    &intake_has_forwarded_packets::IntakeHasForwardedPacketsFactory,
    &intake_has_metric::IntakeHasMetricFactory,
    &intake_has_service_check::IntakeHasServiceCheckFactory,
    &log_contains::LogContainsFactory,
    &log_contains::LogNotContainsFactory,
    &port_listening::PortListeningFactory,
    &process_stable_for::ProcessStableForFactory,
    &send::SendFactory,
];

/// Matches text against a step's pattern, either literally or as a compiled regex.
#[derive(Debug)]
enum Matcher {
    /// The pattern is a substring to find.
    Literal(String),
    /// The pattern was compiled once when the step was built.
    Regex(regex::Regex),
}

impl Matcher {
    /// Builds the matcher for `pattern`, validating the pattern when it is a regex.
    fn build(pattern: &str, regex: bool) -> Result<Self, String> {
        if regex {
            regex::Regex::new(pattern)
                .map(Matcher::Regex)
                .map_err(|error| format!("pattern '{pattern}' is not a valid regex: {error}"))
        } else {
            Ok(Matcher::Literal(pattern.to_owned()))
        }
    }

    /// Returns the pattern as written in the step's fields.
    fn pattern(&self) -> &str {
        match self {
            Matcher::Literal(pattern) => pattern,
            Matcher::Regex(regex) => regex.as_str(),
        }
    }

    /// Returns whether `text` contains the pattern.
    fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Literal(pattern) => text.contains(pattern.as_str()),
            Matcher::Regex(regex) => regex.is_match(text),
        }
    }
}

/// Fields that build each registered step, shared by tests that need a valid step.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support;

#[cfg(test)]
mod tests {
    use panoramic_core::{
        endpoint::EndpointOperation,
        step::{Need, StepFields, StepKind},
    };
    use serde_json::Value;

    use super::*;
    use crate::{
        cli::Cli,
        compare::Compare,
        config_key_equals::ConfigKeyEquals,
        config_set::ConfigSet,
        dogstatsd_replay::DogstatsdReplay,
        exec::Exec,
        exits_with::ExitsWith,
        file_contains::FileContains,
        http_check::HttpCheck,
        intake_has_forwarded_packets::IntakeHasForwardedPackets,
        intake_has_metric::IntakeHasMetric,
        intake_has_service_check::IntakeHasServiceCheck,
        log_contains::{LogContains, LogNotContains},
        port_listening::PortListening,
        process_stable_for::ProcessStableFor,
        send::Send,
        test_support::valid_fields,
    };

    const PATTERN_STEPS: &[&dyn StepFactory] = &[
        &file_contains::FileContainsFactory,
        &http_check::HttpCheckFactory,
        &log_contains::LogContainsFactory,
        &log_contains::LogNotContainsFactory,
    ];

    fn duration_field(name: &str) -> &'static str {
        match name {
            LogNotContains::NAME => "during",
            ProcessStableFor::NAME => "duration",
            _ => "timeout",
        }
    }

    #[tokio::test]
    async fn every_step_errors_when_its_participant_did_not_start() {
        for factory in REGISTERED_STEPS {
            if factory.name() == Compare::NAME {
                continue;
            }
            let report = panoramic_core::step::test_support::run_step_offering(
                *factory,
                &valid_fields(factory.name()),
                crate::test_support::OPERATIONS,
            )
            .await;
            assert_eq!(report.status, panoramic_core::record::AssertionStatus::Errored);
            assert_eq!(report.message, "participant 'subject' has not started");
        }
    }

    #[test]
    fn steps_accept_references_only_in_their_declared_template_fields() {
        for factory in REGISTERED_STEPS {
            let expected: &[&str] = match factory.name() {
                Exec::NAME => &["argv"],
                FileContains::NAME => &["path"],
                LogContains::NAME | LogNotContains::NAME => &["pattern"],
                _ => &[],
            };
            assert_eq!(factory.template_fields(), expected, "{}", factory.name());
        }
    }

    #[test]
    fn every_registered_step_uses_its_declared_name_and_budget() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            if factory.name() != Compare::NAME {
                fields.insert(duration_field(factory.name()).into(), Value::String("7s".into()));
            }
            let step = factory.build(&fields).unwrap();
            assert_eq!(step.name(), factory.name());
            let expected = if factory.name() == Compare::NAME {
                std::time::Duration::from_secs(180)
            } else if factory.name() == DogstatsdReplay::NAME {
                std::time::Duration::from_millis(29_050)
            } else {
                std::time::Duration::from_secs(7)
            };
            assert_eq!(step.budget(), expected);
        }
    }

    #[test]
    fn registered_steps_have_unique_names_in_sorted_order() {
        let names: Vec<_> = REGISTERED_STEPS.iter().map(|factory| factory.name()).collect();
        assert_eq!(
            names,
            [
                "cli",
                "compare",
                "config_key_equals",
                "config_set",
                "dogstatsd_replay",
                "exec",
                "exits_with",
                "file_contains",
                "http_check",
                "intake_has_forwarded_packets",
                "intake_has_metric",
                "intake_has_service_check",
                "log_contains",
                "log_not_contains",
                "port_listening",
                "process_stable_for",
                "send",
            ]
        );
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn each_step_type_name_is_its_registered_factory_name() {
        assert_eq!(
            REGISTERED_STEPS
                .iter()
                .map(|factory| factory.name())
                .collect::<Vec<_>>(),
            [
                Cli::NAME,
                Compare::NAME,
                ConfigKeyEquals::NAME,
                ConfigSet::NAME,
                DogstatsdReplay::NAME,
                Exec::NAME,
                ExitsWith::NAME,
                FileContains::NAME,
                HttpCheck::NAME,
                IntakeHasForwardedPackets::NAME,
                IntakeHasMetric::NAME,
                IntakeHasServiceCheck::NAME,
                LogContains::NAME,
                LogNotContains::NAME,
                PortListening::NAME,
                ProcessStableFor::NAME,
                Send::NAME,
            ]
        );
    }

    #[test]
    fn registered_steps_declare_their_action_or_assertion_kind() {
        assert_eq!(
            REGISTERED_STEPS
                .iter()
                .map(|factory| (factory.name(), factory.kind()))
                .collect::<Vec<_>>(),
            [
                (Cli::NAME, StepKind::Action),
                (Compare::NAME, StepKind::Assertion),
                (ConfigKeyEquals::NAME, StepKind::Assertion),
                (ConfigSet::NAME, StepKind::Action),
                (DogstatsdReplay::NAME, StepKind::Action),
                (Exec::NAME, StepKind::Action),
                (ExitsWith::NAME, StepKind::Assertion),
                (FileContains::NAME, StepKind::Assertion),
                (HttpCheck::NAME, StepKind::Assertion),
                (IntakeHasForwardedPackets::NAME, StepKind::Assertion),
                (IntakeHasMetric::NAME, StepKind::Assertion),
                (IntakeHasServiceCheck::NAME, StepKind::Assertion),
                (LogContains::NAME, StepKind::Assertion),
                (LogNotContains::NAME, StepKind::Assertion),
                (PortListening::NAME, StepKind::Assertion),
                (ProcessStableFor::NAME, StepKind::Assertion),
                (Send::NAME, StepKind::Action),
            ]
        );
    }

    #[test]
    fn every_registered_step_rejects_an_unknown_field() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            let step = factory.build(&fields).unwrap();
            let needs = match factory.name() {
                Compare::NAME => vec![
                    Need::Operation("subject".into(), "dump".into()),
                    Need::Operation("baseline".into(), "dump".into()),
                ],
                Cli::NAME => vec![Need::Operation("subject".into(), "cli_main".into())],
                ConfigKeyEquals::NAME => vec![Need::Operation("subject".into(), "config_dump".into())],
                ConfigSet::NAME => vec![Need::Operation("subject".into(), "config_set".into())],
                DogstatsdReplay::NAME => vec![
                    Need::Operation("subject".into(), "dogstatsd_capture".into()),
                    Need::Operation("subject".into(), "dogstatsd_replay".into()),
                    Need::Operation("subject".into(), "dogstatsd_stats".into()),
                    Need::Endpoint("subject".into(), "udp".into(), EndpointOperation::Send),
                ],
                IntakeHasForwardedPackets::NAME | IntakeHasMetric::NAME | IntakeHasServiceCheck::NAME => {
                    vec![Need::Operation("subject".into(), "dump".into())]
                }
                HttpCheck::NAME => vec![Need::Endpoint(
                    "subject".into(),
                    "http".into(),
                    EndpointOperation::Request,
                )],
                Send::NAME => vec![Need::Endpoint("subject".into(), "udp".into(), EndpointOperation::Send)],
                ExitsWith::NAME => vec![Need::Process("subject".into(), "main".into())],
                PortListening::NAME => vec![Need::Endpoint("subject".into(), "tcp".into(), EndpointOperation::Probe)],
                _ => vec![Need::Handle("subject".into())],
            };
            assert_eq!(step.needs(), needs);
            fields.insert("unknown".into(), Value::String("x".into()));
            let message = factory.build(&fields).unwrap_err();
            assert!(
                message.contains("unknown field `unknown`"),
                "{}: {message}",
                factory.name()
            );
        }
    }

    #[test]
    fn string_fields_reject_yaml_booleans_and_numbers() {
        for factory in REGISTERED_STEPS {
            let string_fields: &[&str] = match factory.name() {
                Compare::NAME => &["subject", "baseline"],
                Cli::NAME => &["on", "process", "output_contains"],
                ConfigKeyEquals::NAME | ConfigSet::NAME => &["on", "key"],
                DogstatsdReplay::NAME => &["on", "endpoint"],
                IntakeHasMetric::NAME => &["on", "name"],
                IntakeHasServiceCheck::NAME => &["on", "name", "hostname"],
                Exec::NAME | IntakeHasForwardedPackets::NAME => &["on"],
                HttpCheck::NAME => &["on", "endpoint", "path", "pattern"],
                Send::NAME => &["on", "endpoint", "payload"],
                FileContains::NAME => &["on", "pattern", "path"],
                LogContains::NAME | LogNotContains::NAME => &["on", "pattern", "process"],
                ExitsWith::NAME | ProcessStableFor::NAME => &["on", "process"],
                PortListening::NAME => &["on", "endpoint"],
                other => panic!("missing string fields for '{other}'"),
            };
            for &field in string_fields {
                for value in ["true", "3"] {
                    let mut fields = valid_fields(factory.name());
                    fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                    let message = factory.build(&fields).unwrap_err();
                    assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                    assert!(message.ends_with("expected a string"), "{message}");
                }
                let mut fields = valid_fields(factory.name());
                let value = if factory.name() == HttpCheck::NAME && field == "path" {
                    "/true"
                } else {
                    "true"
                };
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: \"{value}\"")).unwrap());
                assert!(factory.build(&fields).is_ok(), "{}: {field}", factory.name());
            }
        }
    }

    #[test]
    fn durations_must_be_strings() {
        for factory in REGISTERED_STEPS {
            let mut fields = valid_fields(factory.name());
            if factory.name() == Compare::NAME {
                continue;
            }
            let field = duration_field(factory.name());
            for value in ["30", "true"] {
                fields.extend(serde_saphyr::from_str::<StepFields>(&format!("{field}: {value}")).unwrap());
                let message = factory.build(&fields).unwrap_err();
                assert!(message.starts_with(&format!("{field}: invalid type:")), "{message}");
                assert!(message.ends_with("expected a string"), "{message}");
            }
        }
    }

    #[test]
    fn a_regex_pattern_must_compile_and_a_literal_one_need_not() {
        for factory in PATTERN_STEPS {
            let mut fields = valid_fields(factory.name());
            fields.insert("pattern".into(), Value::String("(".into()));
            fields.insert("regex".into(), Value::Bool(true));
            let message = factory.build(&fields).unwrap_err();
            assert!(message.starts_with("pattern '(' is not a valid regex:"), "{message}");
            fields.insert("regex".into(), Value::Bool(false));
            assert!(factory.build(&fields).is_ok(), "{}", factory.name());
            fields.insert("regex".into(), Value::Bool(true));
            fields.insert("pattern".into(), Value::String("ready|done".into()));
            assert!(factory.build(&fields).is_ok(), "{}", factory.name());
        }
    }

    #[test]
    fn regex_flags_must_be_booleans() {
        for factory in PATTERN_STEPS {
            let mut fields = valid_fields(factory.name());
            fields.insert("regex".into(), Value::String("true".into()));
            let message = factory.build(&fields).unwrap_err();
            assert!(message.starts_with("regex: invalid type:"), "{message}");
            assert!(message.ends_with("expected a boolean"), "{message}");
        }
    }
}
