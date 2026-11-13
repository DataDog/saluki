//! Trace tag replacer transform.
//!
//! Applies regex-based find-and-replace rules to trace tags, span resources, and span-event
//! attributes. Rules run in order, after obfuscation and truncation and before stats and
//! sampling, so durable telemetry only ever sees rewritten values.

use std::borrow::Cow;

use async_trait::async_trait;
use regex::Regex;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::trace::{AttributeValue, Span, Trace},
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;

/// The compiled form of one [`ReplaceRule`].
#[derive(Clone, Debug)]
struct CompiledRule {
    target: RuleTarget,
    re: Regex,
    repl: String,
}

/// The set of fields a rule may rewrite, derived from the rule's `name`.
#[derive(Clone, Debug, PartialEq)]
enum RuleTarget {
    All,
    Resource,
    Tag(String),
}

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
        // value is always discarded, but `Regex::replace_all` still allocates a buffer the size of the stack trace and
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
        for rule in &self.rules {
            match &rule.target {
                RuleTarget::All => self.replace_everywhere(span, &rule.re, &rule.repl),
                RuleTarget::Resource => self.replace_resource(span, &rule.re, &rule.repl),
                RuleTarget::Tag(key) => self.replace_named(span, key, &rule.re, &rule.repl),
            }
        }
    }

    fn replace_everywhere(&self, span: &mut Span, re: &Regex, repl: &str) {
        rewrite_non_hidden(span.attributes.iter_mut(), re, repl);

        self.replace_resource(span, re, repl);

        for event in span.span_events_mut() {
            rewrite_non_hidden(event.attributes_mut().iter_mut(), re, repl);
        }
    }

    fn replace_resource(&self, span: &mut Span, re: &Regex, repl: &str) {
        if let Some(resource) = replace_str(span.resource(), re, repl) {
            span.set_resource(resource);
        }
    }

    fn replace_named(&self, span: &mut Span, key: &str, re: &Regex, repl: &str) {
        if let Some(value) = span.attributes.get_mut(key) {
            replace_in_place(value, re, repl);
        }

        for event in span.span_events_mut() {
            if let Some(value) = event.attributes_mut().get_mut(key) {
                replace_in_place(value, re, repl);
            }
        }
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
        }
    }
}

fn rewrite_non_hidden<'a, I>(entries: I, re: &Regex, repl: &str)
where
    I: Iterator<Item = (&'a MetaString, &'a mut AttributeValue)>,
{
    for (key, value) in entries {
        if key.as_ref().starts_with('_') {
            continue;
        }
        replace_in_place(value, re, repl);
    }
}

/// Rewrites `value` in place. An array is rewritten one element at a time, the way the Agent does, so a rule reaches
/// every string in it.
fn replace_in_place(value: &mut AttributeValue, re: &Regex, repl: &str) {
    if let AttributeValue::Array(elements) = value {
        for element in elements {
            replace_in_place(element, re, repl);
        }
    } else if let Some(replaced) = replace_value(value, re, repl) {
        *value = replaced;
    }
}

/// Formats a scalar value for regex matching; string values borrow, and values with no single string form, such as
/// key-value lists, are skipped. Arrays are handled element by element by [`replace_in_place`].
fn value_as_cow(value: &AttributeValue) -> Option<Cow<'_, str>> {
    match value {
        AttributeValue::String(s) => Some(Cow::Borrowed(s.as_ref())),
        AttributeValue::Bool(b) => Some(Cow::Owned(b.to_string())),
        AttributeValue::Int(i) => Some(Cow::Owned(i.to_string())),
        AttributeValue::Float(f) => Some(Cow::Owned(format!("{}", f))),
        _ => None,
    }
}

/// Returns the replacement only when the text changed; replacements are stored as strings. An
/// unmatched value never allocates: the regex runs over the borrowed form, and only a changed
/// value is materialized.
fn replace_value(value: &AttributeValue, re: &Regex, repl: &str) -> Option<AttributeValue> {
    let as_string = value_as_cow(value)?;
    match re.replace_all(as_string.as_ref(), repl) {
        // A borrowed result means the regex matched nothing.
        Cow::Borrowed(_) => None,
        Cow::Owned(replaced) => {
            if replaced != as_string.as_ref() {
                Some(AttributeValue::String(MetaString::from(replaced)))
            } else {
                None
            }
        }
    }
}

fn replace_str(value: &str, re: &Regex, repl: &str) -> Option<String> {
    match re.replace_all(value, repl) {
        Cow::Borrowed(_) => None,
        Cow::Owned(replaced) => (replaced != value).then_some(replaced),
    }
}

/// Compiles the raw rules into their working form.
///
/// # Errors
///
/// Returns an error if a rule's `pattern` fails to compile, naming the rule. Startup fails rather
/// than dropping the rule.
fn compile_rules(rules: &[ReplaceRule]) -> Result<Vec<CompiledRule>, GenericError> {
    let mut compiled = Vec::with_capacity(rules.len());
    for rule in rules {
        let re = Regex::new(&rule.pattern).map_err(|e| {
            generic_error!(
                "apm_config.replace_tags: rule with name '{}' has an invalid pattern '{}': {}. Each rule must \
                 provide 'name', 'pattern', and 'repl', where 'pattern' is a valid regular expression.",
                rule.name,
                rule.pattern,
                e
            )
        })?;
        let target = match rule.name.as_str() {
            "*" => RuleTarget::All,
            "resource.name" => RuleTarget::Resource,
            _ => RuleTarget::Tag(rule.name.clone()),
        };
        compiled.push(CompiledRule {
            target,
            re,
            repl: rule.repl.clone(),
        });
    }
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::SpanEvent;

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

    #[test]
    fn wildcard_rule_rewrites_tags_resource_and_events_but_not_hidden() {
        let mut span = span_with(
            "POST /pay/checkout?token=abc123",
            attrs(&[
                ("http.url", "https://api.acme.com/pay/checkout?token=abc123"),
                ("user.name", "alice"),
                ("_dd.trace_token", "token=abc123"),
            ]),
            vec![event(1, "db.query", &[("url", "/pay?token=abc123")])],
        );

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            attr(&span, "http.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("https://api.acme.com/pay/checkout?token=?")
        );
        assert_eq!(span.resource(), "POST /pay/checkout?token=?");
        assert_eq!(
            span.span_events()[0]
                .attributes()
                .get("url")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("/pay?token=?")
        );
        assert_eq!(
            attr(&span, "user.name").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("alice")
        );
        assert_eq!(
            attr(&span, "_dd.trace_token")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
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
            vec![
                event(1, "e1", &[("http.url", "/x?token=abc123")]),
                event(2, "e2", &[("other.url", "/y?token=abc123")]),
            ],
        );

        replacer(&[("http.url", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            attr(&span, "http.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("https://api.acme.com/pay/checkout?token=?")
        );
        assert_eq!(
            attr(&span, "other.url").and_then(|v| v.as_string()).map(|s| s.as_ref()),
            Some("https://api.acme.com/pay/checkout?token=abc123")
        );
        assert_eq!(
            span.span_events()[0]
                .attributes()
                .get("http.url")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("/x?token=?"),
            "the named key is rewritten on span events too"
        );
        assert_eq!(
            span.span_events()[1]
                .attributes()
                .get("other.url")
                .and_then(|v| v.as_string())
                .map(|s| s.as_ref()),
            Some("/y?token=abc123")
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
        let event = event_with(vec![(
            "tokens",
            string_array(&["token=abc123", "plain", "token=def456"]),
        )]);
        let mut span = span_with("res", tags, vec![event]);

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(attr(&span, "list"), Some(&string_array(&["token=?", "plain"])));
        assert_eq!(attr(&span, "_hidden"), Some(&string_array(&["token=zzz"])));
        assert_eq!(
            span.span_events()[0].attributes().get("tokens"),
            Some(&string_array(&["token=?", "plain", "token=?"]))
        );
    }

    #[test]
    fn named_rule_rewrites_the_array_of_its_target_key_only() {
        let event = event_with(vec![
            ("tokens", string_array(&["token=abc123"])),
            ("other", string_array(&["token=abc123"])),
        ]);
        let mut tags = FastHashMap::default();
        tags.insert(MetaString::from("tokens"), string_array(&["token=zzz"]));
        tags.insert(MetaString::from("other"), string_array(&["token=zzz"]));
        let mut span = span_with("res", tags, vec![event]);

        replacer(&[("tokens", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(attr(&span, "tokens"), Some(&string_array(&["token=?"])));
        assert_eq!(attr(&span, "other"), Some(&string_array(&["token=zzz"])));
        let attributes = span.span_events()[0].attributes();
        assert_eq!(attributes.get("tokens"), Some(&string_array(&["token=?"])));
        assert_eq!(attributes.get("other"), Some(&string_array(&["token=abc123"])));
    }

    #[test]
    fn array_elements_are_matched_by_their_string_form_and_stored_as_strings() {
        let numbers = AttributeValue::Array(vec![
            AttributeValue::Int(12345),
            AttributeValue::Bool(true),
            AttributeValue::String(MetaString::from("x")),
        ]);
        let mut span = span_with(
            "res",
            FastHashMap::default(),
            vec![event_with(vec![("numbers", numbers)])],
        );

        replacer(&[("*", r"^\d+$", "N")]).replace_span(&mut span);

        // The matched int becomes the replacement text; elements that don't match keep their type.
        assert_eq!(
            span.span_events()[0].attributes().get("numbers"),
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
        let mut span = span_with(
            "res",
            FastHashMap::default(),
            vec![event_with(vec![("nested", nested)])],
        );

        replacer(&[("*", "token=[A-Za-z0-9]+", "token=?")]).replace_span(&mut span);

        assert_eq!(
            span.span_events()[0].attributes().get("nested"),
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

    fn string_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        attr(span, key).and_then(|v| v.as_string()).map(|s| s.as_ref())
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
        assert_eq!(event_string_attr(&span, "exception.stacktrace"), Some("?"));
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
}
