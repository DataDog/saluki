//! Trace tag replacer transform.
//!
//! Applies operator-declared, regex-based find-and-replace rules to trace tags, span resources,
//! and span-event attributes, mirroring the core agent's `ReplaceV1`
//! (`pkg/trace/filters/replacer.go`): rules run in the order they are listed, after obfuscation
//! and truncation and before stats and sampling, so every value that becomes durable telemetry
//! inherits the rewritten form. Tag keys prefixed with `_` are agent-internal metadata and are
//! never rewritten, even by a `"*"` rule.

use agent_data_plane_config::domains;
use async_trait::async_trait;
use regex::Regex;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{
        trace::{AttributeValue, Span},
        Event,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;

/// The compiled form of one `apm_config.replace_tags` rule.
#[derive(Clone, Debug)]
struct CompiledRule {
    /// Which fields the rule may rewrite.
    target: RuleTarget,
    /// The compiled `pattern`.
    re: Regex,
    /// The `repl` text spliced in place of each match.
    repl: String,
}

/// The set of fields a rule may rewrite, derived from the rule's `name`.
#[derive(Clone, Debug, PartialEq)]
enum RuleTarget {
    /// Every span tag whose key does not start with `_`, the span resource, and every span-event
    /// attribute (`name: "*"`).
    All,
    /// Only the span resource (`name: "resource.name"`).
    Resource,
    /// The exact tag key, on the span and its span events (any other `name`).
    Tag(String),
}

/// Trace tag replacer configuration.
pub struct TraceTagReplacerConfiguration {
    /// The raw replacement rules from the resolved trace configuration.
    rules: Vec<domains::traces::ReplaceRule>,
}

impl TraceTagReplacerConfiguration {
    /// Creates a new `TraceTagReplacerConfiguration` from the resolved trace configuration.
    ///
    /// The rules are carried as plain data; their patterns are compiled when the transform is
    /// built, so an invalid pattern fails startup with an actionable error rather than failing
    /// translation for the whole configuration.
    pub fn from_configuration(config: &domains::traces::Domain) -> Self {
        Self {
            rules: config.replace_tags.clone(),
        }
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
            .with_single_value::<Vec<domains::traces::ReplaceRule>>("replacement rules");
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

        if let Some(resource) = replace_str(span.resource(), re, repl) {
            span.set_resource(resource);
        }

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
            if let Some(replaced) = replace_value(value, re, repl) {
                *value = replaced;
            }
        }

        for event in span.span_events_mut() {
            if let Some(value) = event.attributes_mut().get_mut(key) {
                if let Some(replaced) = replace_value(value, re, repl) {
                    *value = replaced;
                }
            }
        }
    }
}

impl SynchronousTransform for TraceTagReplacer {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        for event in buffer {
            if let Event::Trace(ref mut trace) = event {
                for span in trace.spans_mut() {
                    self.replace_span(span);
                }
            }
        }
    }
}

/// Rewrites every non-`_`-prefixed entry in an attributes iterator, storing replacements as strings.
fn rewrite_non_hidden<'a, I>(entries: I, re: &Regex, repl: &str)
where
    I: Iterator<Item = (&'a MetaString, &'a mut AttributeValue)>,
{
    for (key, value) in entries {
        if key.as_ref().starts_with('_') {
            continue;
        }
        if let Some(replaced) = replace_value(value, re, repl) {
            *value = replaced;
        }
    }
}

/// Formats a scalar attribute value the way the agent's `AsString` does, ready for regex matching.
///
/// Composite values (`Bytes`, `Array`, `KeyValueList`) have no single string form and are skipped.
fn value_as_string(value: &AttributeValue) -> Option<String> {
    match value {
        AttributeValue::String(s) => Some(s.as_ref().to_owned()),
        AttributeValue::Bool(b) => Some(b.to_string()),
        AttributeValue::Int(i) => Some(i.to_string()),
        AttributeValue::Float(f) => Some(format!("{}", f)),
        _ => None,
    }
}

/// Applies a rule to a value's string form; returns the replacement only when the text changed,
/// mirroring `ReplaceV1`'s change-only write-back. Replacements are always stored as strings.
fn replace_value(value: &AttributeValue, re: &Regex, repl: &str) -> Option<AttributeValue> {
    let as_string = value_as_string(value)?;
    let replaced = re.replace_all(&as_string, repl).into_owned();
    (replaced != as_string).then(|| AttributeValue::String(MetaString::from(replaced)))
}

/// Applies a rule to a string; returns the replacement only when the text changed.
fn replace_str(value: &str, re: &Regex, repl: &str) -> Option<String> {
    let replaced = re.replace_all(value, repl).into_owned();
    (replaced != value).then_some(replaced)
}

/// Compiles the raw rules from configuration into their working form.
///
/// # Errors
///
/// Returns an error if any rule's `pattern` fails to compile, naming the offending rule's `name`
/// and pattern. This fails the build, and therefore startup, mirroring the core agent's
/// `compileReplaceRules` boot-time failure for the same condition.
fn compile_rules(rules: &[domains::traces::ReplaceRule]) -> Result<Vec<CompiledRule>, GenericError> {
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

    fn replacer(rules: &[(&str, &str, &str)]) -> TraceTagReplacer {
        let raw = rules
            .iter()
            .map(|(name, pattern, repl)| domains::traces::ReplaceRule {
                name: (*name).to_owned(),
                pattern: (*pattern).to_owned(),
                repl: (*repl).to_owned(),
            })
            .collect::<Vec<_>>();
        TraceTagReplacer {
            rules: compile_rules(&raw).expect("test rules compile"),
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

    // A `*` rule rewrites attribute values but skips `_`-prefixed keys, and also rewrites the
    // span resource and span-event attributes.
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

    // A `resource.name` rule rewrites only the resource.
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

    // A named-key rule rewrites only that tag, on the span and its events.
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

    // Replaced values are stored as strings, matching `SetAttributeFromString`: a matched numeric
    // tag becomes a string even when the replacement is itself numeric.
    #[test]
    fn matched_numeric_tag_becomes_a_string() {
        let mut attrs: FastHashMap<MetaString, AttributeValue> = FastHashMap::default();
        attrs.insert(MetaString::from("http.status_code"), AttributeValue::Int(200));

        let mut span = span_with("POST /pay/checkout", attrs, vec![]);
        replacer(&[("*", "20", "2x")]).replace_span(&mut span);

        let value = attr(&span, "http.status_code").expect("tag survives");
        assert_eq!(
            value.as_string().map(|s| s.as_ref()),
            Some("2x0"),
            "written back as a string"
        );
        assert_eq!(value.as_int(), None, "the numeric form does not survive a rewrite");
    }

    // Rules run in order and compose: the second rule sees the first rule's output.
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

    // A pattern that fails to compile is a configuration error that names the offending rule.
    #[test]
    fn invalid_pattern_fails_compilation_with_actionable_error() {
        let raw = vec![domains::traces::ReplaceRule {
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
