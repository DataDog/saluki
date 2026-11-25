//! Trace tag replacement transform.
//!
//! Applies the `apm_config.replace_tags` rules to span tags, resources, and trace-level attributes. The matching and
//! rewriting come from a shared crate that works on Saluki's trace types through their trait implementations. This
//! transform builds the rules, validates them at startup, and applies them to each trace.

use async_trait::async_trait;
use libdd_trace_obfuscation::replacer::{replace_chunk_tags, replace_span_tags_trait, ReplaceRule as CompiledRule};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{transforms::*, BuildContext},
    data_model::event::trace::{Span, Trace},
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use tracing::error;

/// A regex-based trace tag replacement rule.
#[derive(Clone, Debug)]
pub struct ReplaceRule {
    /// Tag key the rule targets: `"*"`, `"resource.name"`, or a literal tag key.
    pub name: String,

    /// Regular expression matched against each targeted value.
    pub pattern: String,

    /// Text spliced in place of each match; `$1`-style group references are supported.
    pub repl: String,
}

/// Tags that hold stack traces, replaced whole when stack trace removal is enabled.
const STACK_TRACE_TAGS: [&str; 2] = ["error.stack", "exception.stacktrace"];

/// The span tag holding the env.
///
/// Stats and sampling can take their env from this tag, so no rule may rewrite it: rules named `env` are dropped at
/// build time.
const ENV_TAG: &str = "env";

/// Trace tag replacer configuration.
pub struct TraceTagReplacerConfiguration {
    rules: Vec<ReplaceRule>,
}

impl TraceTagReplacerConfiguration {
    /// Creates a new `TraceTagReplacerConfiguration` from the given rules.
    ///
    /// When `remove_stack_traces` is set, rules replacing the whole value of each stack trace tag with `?` run after
    /// the given rules. Patterns compile at build time, so an invalid one fails startup naming the rule.
    pub fn new(mut rules: Vec<ReplaceRule>, remove_stack_traces: bool) -> Self {
        // TODO: Add a way to specify a direct replace rule, one that overwrites the whole value without a regex. The
        // value is always discarded, but a regex replacement still allocates a buffer the size of the stack trace and
        // scans it, for every blanked tag of every span.
        if remove_stack_traces {
            rules.extend(STACK_TRACE_TAGS.iter().map(|tag| ReplaceRule {
                name: (*tag).to_owned(),
                // `(?s)` lets `.` match newlines, so the whole multi-line trace is replaced.
                pattern: "(?s).*".to_owned(),
                repl: "?".to_owned(),
            }));
        }
        Self { rules }
    }

    /// Returns the rules the replacer runs, in order, including any stack trace rules.
    pub fn rules(&self) -> &[ReplaceRule] {
        &self.rules
    }
}

#[async_trait]
impl SynchronousTransformBuilder for TraceTagReplacerConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let rules = compile_rules(&self.rules)?;
        Ok(Box::new(TraceTagReplacer { rules }))
    }
}

impl MemoryBounds for TraceTagReplacerConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<TraceTagReplacer>("component struct")
            .with_single_value::<Vec<ReplaceRule>>("replacement rules");
    }
}

/// The tag replacer transform that processes traces.
pub struct TraceTagReplacer {
    rules: Vec<CompiledRule>,
}

impl TraceTagReplacer {
    fn replace_span(&self, span: &mut Span) {
        replace_span_tags_trait(span, &self.rules);
    }
}

impl SynchronousTransform for TraceTagReplacer {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        if self.rules.is_empty() {
            return;
        }

        for trace in buffer.events_mut::<Trace>() {
            for span in trace.spans_mut() {
                self.replace_span(span);
            }

            // Attributes shared by every span of the trace are merged into each span when the trace is encoded, so they
            // need the same scrubbing.
            replace_chunk_tags(trace, &self.rules);
        }
    }
}

/// Compiles the configured rules, dropping rules for the env.
fn compile_rules(rules: &[ReplaceRule]) -> Result<Vec<CompiledRule>, GenericError> {
    let mut compiled = Vec::with_capacity(rules.len());
    for rule in rules {
        // A rule compiles its pattern when it is deserialized, so build it from its serialized form.
        let raw = serde_json::json!({ "name": rule.name, "pattern": rule.pattern, "repl": rule.repl });
        let compiled_rule: CompiledRule = serde_json::from_value(raw).map_err(|e| {
            generic_error!(
                "apm_config.replace_tags: rule with name '{}' has an invalid pattern '{}': {}. Each rule must \
                 provide 'name', 'pattern', and 'repl', where 'pattern' is a valid regular expression.",
                rule.name,
                rule.pattern,
                e
            )
        })?;
        if rule.name == ENV_TAG {
            error!(
                config_key = "apm_config.replace_tags",
                pattern = %rule.pattern,
                "Ignoring the replace rule for \"env\": replace rules cannot change the env, as it is used for sampling \
                 and stats. Set the env in the tracer instead, see \
                 https://docs.datadoghq.com/getting_started/tracing/#environment-name."
            );
            continue;
        }
        compiled.push(compiled_rule);
    }
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::{AttributeValue, AttributeValueExt, SpanEvent};
    use stringtheory::MetaString;

    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> FastHashMap<MetaString, AttributeValue> {
        pairs
            .iter()
            .map(|(k, v)| (MetaString::from(*k), AttributeValue::String(MetaString::from(*v))))
            .collect()
    }

    fn event(time: u64, name: &str, pairs: &[(&str, &str)]) -> SpanEvent {
        SpanEvent::new(time, name).with_attributes(attrs(pairs))
    }

    fn raw_rules(rules: &[(&str, &str, &str)]) -> Vec<ReplaceRule> {
        rules
            .iter()
            .map(|(name, pattern, repl)| ReplaceRule {
                name: (*name).to_owned(),
                pattern: (*pattern).to_owned(),
                repl: (*repl).to_owned(),
            })
            .collect()
    }

    fn replacer(rules: &[(&str, &str, &str)]) -> TraceTagReplacer {
        TraceTagReplacer {
            rules: compile_rules(&raw_rules(rules)).expect("test rules compile"),
        }
    }

    fn span_with(resource: &str, attrs: FastHashMap<MetaString, AttributeValue>, events: Vec<SpanEvent>) -> Span {
        Span::new("checkout", "process-payment", resource, "web", 1, 0, 0, 1, 0)
            .with_attributes(attrs)
            .with_span_events(events)
    }

    fn attr<'a>(span: &'a Span, key: &str) -> Option<&'a AttributeValue> {
        span.attributes.get(key)
    }

    fn string_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        attr(span, key).and_then(|v| v.as_string()).map(|s| s.as_ref())
    }

    #[test]
    fn wildcard_rule_rewrites_tags_and_resource_but_not_hidden() {
        let mut span = span_with(
            "POST /pay/checkout?token=abc123",
            attrs(&[
                ("http.url", "https://api.acme.com/pay/checkout?token=abc123"),
                ("user.name", "alice"),
                ("_dd.trace_token", "token=abc123"),
            ]),
            vec![],
        );

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            string_attr(&span, "http.url"),
            Some("https://api.acme.com/pay/checkout?token=?")
        );
        assert_eq!(span.resource(), "POST /pay/checkout?token=?");
        assert_eq!(string_attr(&span, "user.name"), Some("alice"));
        assert_eq!(
            string_attr(&span, "_dd.trace_token"),
            Some("token=abc123"),
            "hidden-prefixed keys are skipped before the regex ever runs"
        );
    }

    #[test]
    fn resource_name_rule_touches_only_the_resource() {
        let mut span = span_with(
            "POST /pay/checkout",
            attrs(&[("http.url", "https://api.acme.com/pay/checkout")]),
            vec![event(1, "e", &[("url", "https://api.acme.com/pay/checkout")])],
        );

        replacer(&[("resource.name", "^POST /pay/", "POST /v1/payments/")]).replace_span(&mut span);

        assert_eq!(span.resource(), "POST /v1/payments/checkout");
        assert_eq!(
            attr(&span, "http.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("https://api.acme.com/pay/checkout"),
            "tags are untouched by a resource.name rule"
        );
        assert_eq!(
            span.span_events()[0]
                .attributes()
                .get("url")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("https://api.acme.com/pay/checkout"),
            "span events are untouched by a resource.name rule"
        );
    }

    #[test]
    fn named_rule_touches_only_its_target_key() {
        let mut span = span_with(
            "POST /pay/checkout?token=abc123",
            attrs(&[
                ("http.url", "https://api.acme.com/pay/checkout?token=abc123"),
                ("other.url", "https://api.acme.com/pay/checkout?token=abc123"),
            ]),
            vec![],
        );

        replacer(&[("http.url", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            string_attr(&span, "http.url"),
            Some("https://api.acme.com/pay/checkout?token=?")
        );
        assert_eq!(
            string_attr(&span, "other.url"),
            Some("https://api.acme.com/pay/checkout?token=abc123")
        );
        assert_eq!(
            span.resource(),
            "POST /pay/checkout?token=abc123",
            "the resource is untouched"
        );
    }

    #[test]
    fn matched_numeric_tag_becomes_a_string() {
        let mut attrs: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
        attrs.insert(MetaString::from("http.status_code"), AttributeValue::Int(200));
        attrs.insert(MetaString::from("http.port"), AttributeValue::Int(8080));
        attrs.insert(MetaString::from("http.retry"), AttributeValue::Int(3));

        let mut span = span_with("POST /pay/checkout", attrs, vec![]);
        replacer(&[("*", "20", "2x"), ("*", "3", "3")]).replace_span(&mut span);

        let value = attr(&span, "http.status_code").expect("tag survives");
        assert_eq!(
            value.as_string().map(|s| s.as_ref()),
            Some("2x0"),
            "written back as a string"
        );
        assert_eq!(value.as_int(), None, "the numeric form does not survive a rewrite");
        assert_eq!(
            attr(&span, "http.port").and_then(|v| v.as_int()),
            Some(8080),
            "an unmatched numeric stays numeric"
        );
        assert_eq!(
            attr(&span, "http.retry").and_then(|v| v.as_int()),
            Some(3),
            "a match whose replacement equals the matched text is left as-is"
        );
    }

    fn string_array(values: &[&str]) -> AttributeValue {
        AttributeValue::Array(
            values
                .iter()
                .map(|value| AttributeValue::String(MetaString::from(*value)))
                .collect(),
        )
    }

    fn event_with(pairs: Vec<(&str, AttributeValue)>) -> SpanEvent {
        let attributes = pairs
            .into_iter()
            .map(|(key, value)| (MetaString::from(key), value))
            .collect::<FastHashMap<_, _>>();
        SpanEvent::new(1, "event").with_attributes(attributes)
    }

    #[test]
    fn wildcard_rule_rewrites_every_element_of_array_attributes_but_not_hidden_ones() {
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("list"), string_array(&["token=zzz", "plain"]));
        tags.insert(MetaString::from("_hidden"), string_array(&["token=zzz"]));
        let mut span = span_with("res", tags, vec![]);

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(attr(&span, "list"), Some(&string_array(&["token=?", "plain"])));
        assert_eq!(attr(&span, "_hidden"), Some(&string_array(&["token=zzz"])));
    }

    #[test]
    fn named_rule_rewrites_the_array_of_its_target_key_only() {
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("tokens"), string_array(&["token=zzz"]));
        tags.insert(MetaString::from("other"), string_array(&["token=zzz"]));
        let mut span = span_with("res", tags, vec![]);

        replacer(&[("tokens", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(attr(&span, "tokens"), Some(&string_array(&["token=?"])));
        assert_eq!(attr(&span, "other"), Some(&string_array(&["token=zzz"])));
    }

    #[test]
    fn array_elements_are_matched_by_their_string_form_and_stored_as_strings() {
        let numbers = AttributeValue::Array(vec![
            AttributeValue::Int(12345),
            AttributeValue::Bool(true),
            AttributeValue::String(MetaString::from("x")),
        ]);
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("numbers"), numbers);
        let mut span = span_with("res", tags, vec![]);

        replacer(&[("*", r"^\d+$", "N")]).replace_span(&mut span);

        // The matched int becomes the replacement text; elements that don't match keep their type.
        assert_eq!(
            attr(&span, "numbers"),
            Some(&AttributeValue::Array(vec![
                AttributeValue::String(MetaString::from("N")),
                AttributeValue::Bool(true),
                AttributeValue::String(MetaString::from("x")),
            ]))
        );
    }

    #[test]
    fn nested_arrays_are_rewritten_recursively() {
        let nested = AttributeValue::Array(vec![string_array(&["token=abc123"]), string_array(&["plain"])]);
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("nested"), nested);
        let mut span = span_with("res", tags, vec![]);

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            attr(&span, "nested"),
            Some(&AttributeValue::Array(vec![
                string_array(&["token=?"]),
                string_array(&["plain"])
            ]))
        );
    }

    #[test]
    fn arrays_with_no_match_are_left_untouched() {
        let mut span = span_with(
            "res",
            FastHashMap::default(),
            vec![event_with(vec![("plain", string_array(&["a", "b"]))])],
        );

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            span.span_events()[0].attributes().get("plain"),
            Some(&string_array(&["a", "b"]))
        );
    }

    #[test]
    fn rules_compose_in_order() {
        let mut span = span_with(
            "POST /pay/checkout?token=abc123",
            attrs(&[("http.url", "/pay?token=abc123")]),
            vec![],
        );

        replacer(&[
            ("*", "token=abc123", "token=?"),
            ("resource.name", "token=\\?", "token=REDACTED"),
        ])
        .replace_span(&mut span);

        assert_eq!(
            span.resource(),
            "POST /pay/checkout?token=REDACTED",
            "the second rule matched against the first rule's replacement"
        );
        assert_eq!(
            attr(&span, "http.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("/pay?token=?"),
            "the second rule is resource-only and leaves the tag at the first rule's output"
        );
    }

    #[test]
    fn empty_rule_set_leaves_span_untouched() {
        let mut span = span_with(
            "POST /pay/checkout?token=abc123",
            attrs(&[("http.url", "/x?token=abc123")]),
            vec![],
        );
        replacer(&[]).replace_span(&mut span);

        assert_eq!(
            attr(&span, "http.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("/x?token=abc123")
        );
        assert_eq!(span.resource(), "POST /pay/checkout?token=abc123");
    }

    fn configured_replacer(rules: &[(&str, &str, &str)], remove_stack_traces: bool) -> TraceTagReplacer {
        let config = TraceTagReplacerConfiguration::new(raw_rules(rules), remove_stack_traces);
        TraceTagReplacer {
            rules: compile_rules(&config.rules).expect("configured rules compile"),
        }
    }

    const STACK: &str = "Traceback (most recent call last):\n  File \"app.py\", line 7, in pay\nValueError: boom\n";

    fn span_with_stack_traces() -> Span {
        span_with(
            "POST /pay/checkout",
            attrs(&[
                ("error.stack", STACK),
                ("exception.stacktrace", STACK),
                ("error.message", "boom"),
            ]),
            vec![event(1, "exception", &[("exception.stacktrace", STACK)])],
        )
    }

    fn event_string_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        span.span_events()[0]
            .attributes()
            .get(key)
            .and_then(|v| v.as_string())
            .map(|s| s.as_ref())
    }

    #[test]
    fn stack_trace_removal_replaces_both_stack_trace_tags() {
        let mut span = span_with_stack_traces();

        configured_replacer(&[], true).replace_span(&mut span);

        assert_eq!(string_attr(&span, "error.stack"), Some("?"));
        assert_eq!(string_attr(&span, "exception.stacktrace"), Some("?"));
        assert_eq!(string_attr(&span, "error.message"), Some("boom"));
    }

    #[test]
    fn stack_trace_tags_are_untouched_without_stack_trace_removal() {
        let mut span = span_with_stack_traces();

        configured_replacer(&[], false).replace_span(&mut span);

        assert_eq!(string_attr(&span, "error.stack"), Some(STACK));
        assert_eq!(string_attr(&span, "exception.stacktrace"), Some(STACK));
        assert_eq!(event_string_attr(&span, "exception.stacktrace"), Some(STACK));
    }

    #[test]
    fn stack_trace_removal_runs_after_configured_rules() {
        let rules = [
            ("error.message", "boom", "failure"),
            ("error.stack", r"^\?$", "replaced late"),
        ];
        let config = TraceTagReplacerConfiguration::new(raw_rules(&rules), true);
        let names = config.rules.iter().map(|rule| rule.name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            names,
            ["error.message", "error.stack", "error.stack", "exception.stacktrace"]
        );

        let mut span = span_with_stack_traces();
        configured_replacer(&rules, true).replace_span(&mut span);

        assert_eq!(string_attr(&span, "error.message"), Some("failure"));
        assert_eq!(
            string_attr(&span, "error.stack"),
            Some("?"),
            "the configured rule saw the original trace, not the removal's output"
        );
        assert_eq!(string_attr(&span, "exception.stacktrace"), Some("?"));
    }

    #[test]
    fn invalid_pattern_fails_compilation_with_actionable_error() {
        let raw = vec![ReplaceRule {
            name: "http.url".to_owned(),
            pattern: "([".to_owned(),
            repl: "x".to_owned(),
        }];

        let err = compile_rules(&raw).expect_err("invalid pattern must fail");
        let message = format!("{}", err);
        assert!(
            message.contains("apm_config.replace_tags"),
            "error names the config key: {}",
            message
        );
        assert!(message.contains("http.url"), "error names the rule: {}", message);
    }

    // Ported from the Go trace agent's `TestCompileReplaceRulesDropsEnv` and `TestReplacerWildcardEnv`
    // (datadog-agent 927a054f307, https://github.com/DataDog/datadog-agent/pull/59980). Replacing the span env changed
    // the env used for stats and sampling, so tracers received sample rates for an env they didn't know.

    #[test]
    fn env_rules_are_dropped_and_other_rules_kept_in_order() {
        let rules = raw_rules(&[
            ("http.url", "guid", "[REDACTED]"),
            ("env", ".*", "myenv"),
            ("*", "secret", "?"),
            ("env", "prod", "production"),
        ]);

        let compiled = compile_rules(&rules).expect("rules compile");
        let names = compiled.iter().map(|rule| rule.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["http.url", "*"]);

        let only_env = compile_rules(&raw_rules(&[("env", ".*", "myenv")])).expect("rules compile");
        assert!(only_env.is_empty(), "a rule set of only env rules compiles to nothing");
    }

    #[test]
    fn env_rule_leaves_span_and_span_event_env_untouched() {
        let mut span = span_with(
            "res",
            attrs(&[("env", "prod")]),
            vec![event(1, "e", &[("env", "prod")])],
        );

        replacer(&[("env", "prod", "production")]).replace_span(&mut span);

        assert_eq!(string_attr(&span, "env"), Some("prod"));
        assert_eq!(event_string_attr(&span, "env"), Some("prod"));
    }

    #[test]
    fn env_rule_with_invalid_pattern_still_fails_compilation() {
        let err = compile_rules(&raw_rules(&[("env", "(", "myenv")])).expect_err("invalid env pattern must fail");
        let message = format!("{}", err);
        assert!(message.contains("'env'"), "error names the rule: {}", message);
    }

    #[test]
    fn wildcard_rule_skips_span_env_but_rewrites_other_tags_and_resource() {
        let mut span = span_with(
            "prod-resource",
            attrs(&[("env", "prod"), ("custom.tag", "prod-value")]),
            vec![],
        );

        replacer(&[("*", "prod", "stage")]).replace_span(&mut span);

        assert_eq!(string_attr(&span, "env"), Some("prod"));
        assert_eq!(string_attr(&span, "custom.tag"), Some("stage-value"));
        assert_eq!(span.resource(), "stage-resource");
    }

    #[test]
    fn wildcard_rule_skips_numeric_span_env() {
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("env"), AttributeValue::Float(1.0));
        tags.insert(MetaString::from("latency"), AttributeValue::Float(1.0));
        let mut span = span_with("res", tags, vec![]);

        replacer(&[("*", "1", "prod")]).replace_span(&mut span);

        assert_eq!(attr(&span, "env"), Some(&AttributeValue::Float(1.0)));
        assert_eq!(
            string_attr(&span, "latency"),
            Some("prod"),
            "other numeric tags are still replaced"
        );
    }

    // Rules apply to span event attributes as well as span attributes. The replacer doesn't do that yet, so these tests
    // assert the expected behavior and are ignored until it does.

    #[test]
    #[ignore = "not implemented yet: replace rules skip span event attributes (DataDog/libdatadog#2924)"]
    fn wildcard_rule_rewrites_span_event_attributes_but_not_hidden_ones() {
        let mut span = span_with(
            "res",
            FastHashMap::default(),
            vec![event(
                1,
                "e",
                &[
                    ("url", "/pay?token=abc123"),
                    ("_hidden", "token=abc123"),
                    ("env", "prod"),
                ],
            )],
        );

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?"), ("*", "prod", "stage")]).replace_span(&mut span);

        assert_eq!(event_string_attr(&span, "url"), Some("/pay?token=?"));
        assert_eq!(event_string_attr(&span, "_hidden"), Some("token=abc123"));
        assert_eq!(
            event_string_attr(&span, "env"),
            Some("stage"),
            "span event attributes are not the span env"
        );
    }

    #[test]
    #[ignore = "not implemented yet: replace rules skip span event attributes (DataDog/libdatadog#2924)"]
    fn named_rule_rewrites_the_same_key_on_span_events_only() {
        let mut span = span_with(
            "res",
            FastHashMap::default(),
            vec![event(
                1,
                "e",
                &[("http.url", "/x?token=abc123"), ("other.url", "/y?token=abc123")],
            )],
        );

        replacer(&[("http.url", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(event_string_attr(&span, "http.url"), Some("/x?token=?"));
        assert_eq!(event_string_attr(&span, "other.url"), Some("/y?token=abc123"));
    }

    #[test]
    #[ignore = "not implemented yet: replace rules skip span event attributes (DataDog/libdatadog#2924)"]
    fn rules_rewrite_arrays_on_span_events() {
        let event = event_with(vec![(
            "tokens",
            string_array(&["token=abc123", "plain", "token=def456"]),
        )]);
        let mut span = span_with("res", FastHashMap::default(), vec![event]);

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            span.span_events()[0].attributes().get("tokens"),
            Some(&string_array(&["token=?", "plain", "token=?"]))
        );
    }

    #[test]
    #[ignore = "not implemented yet: replace rules skip span event attributes (DataDog/libdatadog#2924)"]
    fn stack_trace_removal_replaces_the_stack_trace_on_span_events() {
        let mut span = span_with_stack_traces();

        configured_replacer(&[], true).replace_span(&mut span);

        assert_eq!(event_string_attr(&span, "exception.stacktrace"), Some("?"));
    }
}
