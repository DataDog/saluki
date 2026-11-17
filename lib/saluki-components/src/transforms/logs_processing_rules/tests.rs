use std::collections::HashMap;

use metrics::Counter;
use saluki_core::{
    components::test_util::TestComponentDriver,
    data_model::{
        event::{log::Log, metric::Metric, Event},
        tags::{Tag, TagSet},
    },
};
use saluki_metrics::test::TestRecorder;
use serde_json::json;
use stringtheory::MetaString;

use super::*;

fn exclude(pattern: &str) -> LogsProcessingRule {
    LogsProcessingRule::ExcludeAtMatch {
        name: "exclude".to_string(),
        pattern: pattern.to_string(),
    }
}

fn include(pattern: &str) -> LogsProcessingRule {
    LogsProcessingRule::IncludeAtMatch {
        name: "include".to_string(),
        pattern: pattern.to_string(),
    }
}

fn mask(pattern: &str, placeholder: &str) -> LogsProcessingRule {
    LogsProcessingRule::MaskSequences {
        name: "mask".to_string(),
        pattern: pattern.to_string(),
        replace_placeholder: placeholder.to_string(),
    }
}

fn transform(rules: &[LogsProcessingRule]) -> LogsProcessingRules {
    LogsProcessingRules {
        rules: compile_rules(rules).expect("test rules should compile"),
        events_dropped: Counter::noop(),
    }
}

/// Applies `rules` to a log with the given message, returning its message afterwards, or `None` if a rule dropped it.
fn apply(rules: &[LogsProcessingRule], message: &str) -> Option<String> {
    let mut log = Log::new(message.to_string());
    transform(rules).process(&mut log).then(|| log.message().to_string())
}

/// A table of messages to process, and what the rules should leave of each, or `None` if they should drop it.
type Cases<'a> = &'a [(&'a str, Option<&'a str>)];

#[track_caller]
fn assert_cases(rules: &[LogsProcessingRule], cases: Cases<'_>) {
    for (message, expected) in cases {
        assert_eq!(
            apply(rules, message).as_deref(),
            *expected,
            "message {message:?} with rules {rules:?}"
        );
    }
}

// The cases in the `agent_*` tests are ported from the logs processor tests of the Datadog Agent (`exclusionTests`,
// `inclusionTests`, `exclusionInclusionTests`, and `masksTests` in `comp/logs-library/processor/processor_test.go`),
// at DataDog/datadog-agent@e0c8d8d93be. Their inputs are ASCII, so the differences between the regular expression
// dialects of Go and the `regex` crate don't affect them.

#[test]
fn agent_exclusion_cases() {
    assert_cases(
        &[exclude("world")],
        &[("hello", Some("hello")), ("world", None), ("a brand new world", None)],
    );
    assert_cases(
        &[exclude("$world")],
        &[("a brand new world", Some("a brand new world"))],
    );
}

#[test]
fn agent_inclusion_cases() {
    assert_cases(
        &[include("world")],
        &[
            ("hello", None),
            ("world", Some("world")),
            ("a brand new world", Some("a brand new world")),
        ],
    );
    // In the ported test, this is a global rule followed by a rule of the log's source. Every include must match.
    assert_cases(&[include("world"), include("^world")], &[("a brand new world", None)]);
}

#[test]
fn agent_exclusion_with_inclusion_cases() {
    assert_cases(
        &[exclude("^bob"), include(".*@datadoghq.com$")],
        &[
            ("bob@datadoghq.com", None),
            ("bill@datadoghq.com", Some("bill@datadoghq.com")),
            ("bob@amail.com", None),
            ("bill@amail.com", None),
        ],
    );
}

#[test]
fn agent_mask_cases() {
    assert_cases(
        &[mask("world", "[masked_world]")],
        &[
            ("hello", Some("hello")),
            ("hello world!", Some("hello [masked_world]!")),
        ],
    );
    assert_cases(
        &[mask("User=\\w+@datadoghq.com", "[masked_user]")],
        &[(
            "new test launched by User=beats@datadoghq.com on localhost",
            Some("new test launched by [masked_user] on localhost"),
        )],
    );
    let credit_card = "(?:4[0-9]{12}(?:[0-9]{3})?|[25][1-7][0-9]{14}|6(?:011|5[0-9][0-9])[0-9]{12}|3[47][0-9]{13}|3(?:0[0-5]|[68][0-9])[0-9]{11}|(?:2131|1800|35\\d{3})\\d{11})";
    assert_cases(
        &[mask(credit_card, "[masked_credit_card]")],
        &[(
            "The credit card 4323124312341234 was used to buy some time",
            Some("The credit card [masked_credit_card] was used to buy some time"),
        )],
    );
    assert_cases(
        &[mask("([Dd]ata_?values=)\\S+", "${1}[masked_value]")],
        &[
            (
                "New data added to Datavalues=123456 on prod",
                Some("New data added to Datavalues=[masked_value] on prod"),
            ),
            (
                "New data added to data_values=123456 on prod",
                Some("New data added to data_values=[masked_value] on prod"),
            ),
            (
                "New data added to data_values= on prod",
                Some("New data added to data_values= on prod"),
            ),
        ],
    );
}

#[test]
fn rules_apply_in_order_to_the_content_left_by_earlier_rules() {
    // A mask changes what later rules match.
    assert_cases(
        &[mask("secret", "[redacted]"), include("redacted")],
        &[("a secret", Some("a [redacted]")), ("nothing", None)],
    );
    assert_cases(
        &[mask("secret", "[redacted]"), include("secret")],
        &[("a secret", None)],
    );
    assert_cases(
        &[mask("[0-9]+", "NUM"), exclude("NUM")],
        &[("id 42", None), ("id x", Some("id x"))],
    );
    // Rules before a mask see the content before it.
    assert_cases(
        &[include("secret"), mask("secret", "[redacted]")],
        &[("a secret", Some("a [redacted]")), ("nothing", None)],
    );
    // Every include must match.
    assert_cases(
        &[include("a"), include("b")],
        &[("a", None), ("b", None), ("ab", Some("ab"))],
    );
    // A mask sees the placeholders of the masks before it.
    assert_cases(&[mask("a", "b"), mask("b", "c")], &[("ab", Some("cc"))]);
}

#[test]
fn masks_replace_every_match() {
    assert_cases(&[mask("x[0-9]", "X")], &[("x1 x2 y3 x4", Some("X X y3 X"))]);
    assert_cases(&[mask("secret", "")], &[("a secret b", Some("a  b"))]);
    assert_cases(&[mask("secret", "***")], &[("café secret ☕", Some("café *** ☕"))]);
}

#[test]
fn empty_content_and_empty_matches_behave_like_the_agent() {
    // These match the output of Go's `Regexp.ReplaceAll`.
    assert_cases(&[exclude("^$")], &[("", None), ("a", Some("a"))]);
    assert_cases(&[mask("x*", "M")], &[("", Some("M"))]);
    assert_cases(&[mask("b*", "-")], &[("abc", Some("-a-c-"))]);
    assert_cases(&[mask("", "-")], &[("abc", Some("-a-b-c-"))]);
}

#[test]
fn placeholder_expansion() {
    // `go` is what Go's `Regexp.ReplaceAll` gives, where it differs from what the `regex` crate gives.
    struct Case {
        pattern: &'static str,
        placeholder: &'static str,
        input: &'static str,
        rust: &'static str,
        go: Option<&'static str>,
    }
    let cases = [
        Case {
            pattern: "(a)",
            placeholder: "[$01]",
            input: "a",
            rust: "[a]",
            go: Some("[]"),
        },
        Case {
            pattern: "(a)",
            placeholder: "[${01}]",
            input: "a",
            rust: "[a]",
            go: Some("[]"),
        },
        Case {
            pattern: "(a)",
            placeholder: "[${}]",
            input: "a",
            rust: "[]",
            go: Some("[${}]"),
        },
        Case {
            pattern: "(a)",
            placeholder: "[${ 1}]",
            input: "a",
            rust: "[]",
            go: Some("[${ 1}]"),
        },
        Case {
            pattern: "(a)",
            placeholder: "[$é]",
            input: "a",
            rust: "[$é]",
            go: Some("[]"),
        },
        Case {
            pattern: "(a)",
            placeholder: "[$$1]",
            input: "a",
            rust: "[$1]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[$]",
            input: "a",
            rust: "[$]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[$-]",
            input: "a",
            rust: "[$-]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[${1]",
            input: "a",
            rust: "[${1]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[$1x]",
            input: "a",
            rust: "[]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[${1}x]",
            input: "a",
            rust: "[ax]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[$0|${0}]",
            input: "a",
            rust: "[a|a]",
            go: None,
        },
        Case {
            pattern: "(a)",
            placeholder: "[$10]",
            input: "a",
            rust: "[]",
            go: None,
        },
        Case {
            pattern: "(a)(b)?",
            placeholder: "[$2]",
            input: "a",
            rust: "[]",
            go: None,
        },
        Case {
            pattern: "(?P<n>a)",
            placeholder: "[${n}|$n]",
            input: "a",
            rust: "[a|a]",
            go: None,
        },
    ];

    for case in cases {
        assert_eq!(
            apply(&[mask(case.pattern, case.placeholder)], case.input).as_deref(),
            Some(case.rust),
            "placeholder {:?} for pattern {:?} (Go gives {:?})",
            case.placeholder,
            case.pattern,
            case.go.unwrap_or(case.rust),
        );
    }
}

#[test]
fn rules_that_dont_apply_to_these_logs_do_nothing() {
    let rules = [
        LogsProcessingRule::ExcludeTruncated {
            name: "truncated".to_string(),
        },
        LogsProcessingRule::RemapSource {
            name: "remap".to_string(),
            matching: vec![LogsSourceMatch {
                attribute: "message".to_string(),
                value: "hello".to_string(),
                new_source: "remapped".to_string(),
            }],
        },
        LogsProcessingRule::MultiLine {
            name: "multi_line".to_string(),
            pattern: "hello".to_string(),
        },
    ];
    let mut log = Log::new("hello").with_source(Some(MetaString::from("original")));
    let expected = log.clone();

    assert!(transform(&rules).process(&mut log));
    assert_eq!(log, expected);
}

#[test]
fn a_mask_changes_only_the_message() {
    let mut log = Log::new("user=alice logged in")
        .with_status("warn")
        .with_source(Some(MetaString::from("app")))
        .with_hostname(Some(MetaString::from("host-1")))
        .with_service(Some(MetaString::from("auth")))
        .with_tags(Some(TagSet::from_iter([Tag::from("env:prod")])))
        .with_additional_properties(Some(HashMap::from([(MetaString::from("user"), json!("alice"))])));
    let mut expected = log.clone();
    expected.set_message("user=[masked] logged in");

    assert!(transform(&[mask("user=\\w+", "user=[masked]")]).process(&mut log));
    assert_eq!(log, expected);
}

#[test]
fn an_unchanged_message_isnt_replaced() {
    // Long enough that the message is stored on the heap, so a replacement would move it.
    let message = "a message that isn't masked, long enough to be stored on the heap";
    for rules in [
        vec![mask("secret", "x")],
        vec![mask("masked", "masked")],
        vec![include("message")],
    ] {
        let mut log = Log::new(message.to_string());
        let original = log.message().as_ptr();

        assert!(transform(&rules).process(&mut log));
        assert_eq!(log.message(), message);
        assert_eq!(
            log.message().as_ptr(),
            original,
            "rules {rules:?} should keep the original message"
        );
    }
}

fn describe(event: Event) -> String {
    match event {
        Event::Log(log) => format!("log:{}", log.message()),
        Event::Metric(metric) => format!("metric:{}", metric.context().name()),
        other => panic!("unexpected event: {other:?}"),
    }
}

/// Runs the transform with `rules` over `events`, returning what it outputs and the logs it counted as dropped.
async fn run(rules: Vec<LogsProcessingRule>, events: Vec<Event>) -> (Vec<String>, Option<u64>) {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let (control, mut output) =
        TestComponentDriver::synchronous_transform(LogsProcessingRulesConfiguration::new(rules))
            .await
            .expect("transform should build");
    control.send_events(events).await;
    control.shutdown().await.expect("transform should stop cleanly");

    let described = output.collect().await.into_iter().flatten().map(describe).collect();
    let dropped = recorder.counter((
        "component_events_dropped_total",
        &[
            ("component_id", "test"),
            ("component_type", "transform"),
            ("intentional", "true"),
            ("drop_reason", "processing_rule"),
        ][..],
    ));
    (described, dropped)
}

#[tokio::test]
async fn dropped_logs_are_counted_and_the_rest_keep_their_order() {
    let events = vec![
        Event::Log(Log::new("keep 1")),
        Event::Log(Log::new("drop 1")),
        Event::Log(Log::new("drop 2")),
        Event::Metric(Metric::counter("drop", 1.0)),
        Event::Log(Log::new("keep 2 secret")),
        Event::Log(Log::new("keep 3")),
        Event::Log(Log::new("drop 3")),
    ];

    let (output, dropped) = run(vec![exclude("drop"), mask("secret", "***")], events).await;

    assert_eq!(output, ["log:keep 1", "metric:drop", "log:keep 2 ***", "log:keep 3"]);
    assert_eq!(dropped, Some(3));
}

#[tokio::test]
async fn every_log_can_be_dropped() {
    let events = vec![Event::Log(Log::new("a")), Event::Log(Log::new("b"))];

    let (output, dropped) = run(vec![include("never")], events).await;

    assert!(output.is_empty(), "unexpected output: {output:?}");
    assert_eq!(dropped, Some(2));
}

#[tokio::test]
async fn no_rules_keep_every_log() {
    let events = vec![Event::Log(Log::new("a")), Event::Log(Log::new("b"))];

    let (output, dropped) = run(Vec::new(), events).await;

    assert_eq!(output, ["log:a", "log:b"]);
    assert_eq!(dropped.unwrap_or_default(), 0);
}

#[tokio::test]
async fn an_invalid_pattern_fails_the_build_naming_the_rule() {
    let rules = vec![LogsProcessingRule::MaskSequences {
        name: "bad_mask".to_string(),
        pattern: "(".to_string(),
        replace_placeholder: "x".to_string(),
    }];

    let error = match TestComponentDriver::synchronous_transform(LogsProcessingRulesConfiguration::new(rules)).await {
        Ok(_) => panic!("an invalid pattern should fail the build"),
        Err(error) => format!("{error:#}"),
    };

    assert!(
        error.contains("Invalid pattern '(' for logs processing rule 'bad_mask'"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_multi_line_pattern_isnt_compiled() {
    let rules = vec![LogsProcessingRule::MultiLine {
        name: "multi_line".to_string(),
        pattern: "(".to_string(),
    }];

    let (output, _) = run(rules, vec![Event::Log(Log::new("a"))]).await;

    assert_eq!(output, ["log:a"]);
}
