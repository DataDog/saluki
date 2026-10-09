//! Predicate evaluation.

use saluki_core::data_model::tags::TagSet;
use saluki_metric_kernels::matcher::SortedSetMatcher;
use snafu::Snafu;

use super::{Predicate, StringMatch, TagMatch};
use crate::{
    ir::{BindingName, Operand},
    model::{EventView, KindSet},
};

/// Supplies the values of configuration bindings that a plan has not resolved.
///
/// The value of an operand is its literal, or the resolved value of its binding when the plan carries one, as every
/// lowered plan does. Only a binding without a resolved value is looked up here: for example, when evaluating an
/// exported program, whose bindings are deliberately unresolved, against values supplied by a test or a preview.
pub trait BindingResolver {
    /// Returns the boolean bound to `binding`, if it is known.
    fn resolve_bool(&self, binding: &BindingName) -> Option<bool>;

    /// Returns the list of strings bound to `binding`, if it is known.
    fn resolve_strings(&self, binding: &BindingName) -> Option<Vec<String>>;
}

/// A [`BindingResolver`] that knows no bindings, so that only literals and resolved values can be used.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBindings;

impl BindingResolver for NoBindings {
    fn resolve_bool(&self, _binding: &BindingName) -> Option<bool> {
        None
    }

    fn resolve_strings(&self, _binding: &BindingName) -> Option<Vec<String>> {
        None
    }
}

/// An error compiling a predicate.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum PredicateError {
    /// An operand is bound to configuration, but neither the plan nor the resolver supplies its value.
    #[snafu(display(
        "The configuration binding `{}` has no value. Lowering resolves bindings into plans; to evaluate an unresolved \
         plan, supply the binding's value through the resolver.",
        binding
    ))]
    UnresolvedBinding {
        /// The binding without a value.
        binding: BindingName,
    },
}

fn resolve_bool<R>(operand: &Operand<bool>, resolver: &R) -> Result<bool, PredicateError>
where
    R: BindingResolver + ?Sized,
{
    match operand {
        Operand::Literal(value) => Ok(*value),
        Operand::Config {
            resolved: Some(value), ..
        } => Ok(*value),
        Operand::Config {
            binding,
            resolved: None,
        } => resolver
            .resolve_bool(binding)
            .ok_or_else(|| PredicateError::UnresolvedBinding {
                binding: binding.clone(),
            }),
    }
}

fn resolve_strings<R>(operand: &Operand<Vec<String>>, resolver: &R) -> Result<Vec<String>, PredicateError>
where
    R: BindingResolver + ?Sized,
{
    match operand {
        Operand::Literal(values) => Ok(values.clone()),
        Operand::Config {
            resolved: Some(values), ..
        } => Ok(values.clone()),
        Operand::Config {
            binding,
            resolved: None,
        } => resolver
            .resolve_strings(binding)
            .ok_or_else(|| PredicateError::UnresolvedBinding {
                binding: binding.clone(),
            }),
    }
}

/// A compiled node: every operand resolved and every string match built into a matcher.
#[derive(Clone, Debug)]
enum Compiled {
    Const(bool),
    Name(SortedSetMatcher),
    IntakeName(SortedSetMatcher),
    Host(SortedSetMatcher),
    HostUnset,
    HasTag(String),
    Tag(String, SortedSetMatcher),
    HasOriginTag(String),
    OriginTag(String, SortedSetMatcher),
    KindIn(KindSet),
    All(Vec<Compiled>),
    Any(Vec<Compiled>),
    Not(Box<Compiled>),
}

/// A predicate ready to evaluate, with its operands resolved and its string matches built.
///
/// Evaluation never allocates. Every string match, including `eq` and `prefix`, uses the same sorted-set matcher as
/// the native filterlists and allowlists, so all of them share one implementation of matching.
#[derive(Clone, Debug)]
pub struct CompiledPredicate {
    root: Compiled,
}

impl Predicate {
    /// Compiles this predicate for evaluation, resolving its operands.
    ///
    /// The value of an operand is its literal, or the resolved value of its binding; a binding without a resolved value
    /// is looked up in `resolver`.
    ///
    /// # Errors
    ///
    /// Returns an error if an operand is bound to configuration and has no value from the plan or the resolver.
    pub fn compile<R>(&self, resolver: &R) -> Result<CompiledPredicate, PredicateError>
    where
        R: BindingResolver + ?Sized,
    {
        Ok(CompiledPredicate {
            root: compile(self, resolver)?,
        })
    }
}

fn compile<R>(predicate: &Predicate, resolver: &R) -> Result<Compiled, PredicateError>
where
    R: BindingResolver + ?Sized,
{
    Ok(match predicate {
        Predicate::Const(value) => Compiled::Const(*value),
        Predicate::Flag(flag) => Compiled::Const(resolve_bool(flag, resolver)?),
        Predicate::IsEmpty(list) => Compiled::Const(resolve_strings(list, resolver)?.is_empty()),
        Predicate::Name(matcher) => Compiled::Name(compile_match(matcher, resolver)?),
        Predicate::IntakeName(matcher) => Compiled::IntakeName(compile_match(matcher, resolver)?),
        Predicate::Host(matcher) => Compiled::Host(compile_match(matcher, resolver)?),
        Predicate::HostUnset {} => Compiled::HostUnset,
        Predicate::HasTag(key) => Compiled::HasTag(key.clone()),
        Predicate::Tag(TagMatch { key, value }) => Compiled::Tag(key.clone(), compile_match(value, resolver)?),
        Predicate::HasOriginTag(key) => Compiled::HasOriginTag(key.clone()),
        Predicate::OriginTag(TagMatch { key, value }) => {
            Compiled::OriginTag(key.clone(), compile_match(value, resolver)?)
        }
        Predicate::KindIn(kinds) => Compiled::KindIn(*kinds),
        Predicate::All(operands) => Compiled::All(
            operands
                .iter()
                .map(|operand| compile(operand, resolver))
                .collect::<Result<_, _>>()?,
        ),
        Predicate::Any(operands) => Compiled::Any(
            operands
                .iter()
                .map(|operand| compile(operand, resolver))
                .collect::<Result<_, _>>()?,
        ),
        Predicate::Not(operand) => Compiled::Not(Box::new(compile(operand, resolver)?)),
    })
}

fn compile_match<R>(matcher: &StringMatch, resolver: &R) -> Result<SortedSetMatcher, PredicateError>
where
    R: BindingResolver + ?Sized,
{
    Ok(match matcher {
        StringMatch::Eq(value) => SortedSetMatcher::new([value], false),
        StringMatch::Prefix(value) => SortedSetMatcher::new([value], true),
        StringMatch::In(values) => SortedSetMatcher::new(values, false),
        StringMatch::PrefixIn(values) => SortedSetMatcher::new(values, true),
        StringMatch::Matches(set) => SortedSetMatcher::new(
            resolve_strings(&set.values, resolver)?,
            resolve_bool(&set.prefix, resolver)?,
        ),
    })
}

impl CompiledPredicate {
    /// Returns whether the metric seen through `view` matches.
    ///
    /// `all` and `any` stop at the first operand that decides them, so derived fields are only computed when an
    /// evaluated leaf needs them.
    pub fn evaluate(&self, view: &mut EventView<'_>) -> bool {
        evaluate(&self.root, view)
    }
}

fn evaluate(node: &Compiled, view: &mut EventView<'_>) -> bool {
    match node {
        Compiled::Const(value) => *value,
        Compiled::Name(matcher) => matcher.matches(view.name().as_bytes()),
        Compiled::IntakeName(matcher) => view.intake_name().is_some_and(|name| matcher.matches(name)),
        Compiled::Host(matcher) => view.host().is_some_and(|host| matcher.matches(host.as_bytes())),
        Compiled::HostUnset => view.host().is_none(),
        Compiled::HasTag(key) => has_key(view.tags(), key),
        Compiled::Tag(key, matcher) => has_matching_value(view.tags(), key, matcher),
        Compiled::HasOriginTag(key) => has_key(view.origin_tags(), key),
        Compiled::OriginTag(key, matcher) => has_matching_value(view.origin_tags(), key, matcher),
        Compiled::KindIn(kinds) => kinds.contains(view.kind()),
        Compiled::All(operands) => operands.iter().all(|operand| evaluate(operand, view)),
        Compiled::Any(operands) => operands.iter().any(|operand| evaluate(operand, view)),
        Compiled::Not(operand) => !evaluate(operand, view),
    }
}

/// Returns `true` if any tag has the key `key`, with or without a value.
fn has_key(tags: &TagSet, key: &str) -> bool {
    tags.into_iter().any(|tag| tag.name() == key)
}

/// Returns `true` if any tag with the key `key` has a value that `matcher` matches. Bare tags have no value.
fn has_matching_value(tags: &TagSet, key: &str, matcher: &SortedSetMatcher) -> bool {
    tags.into_iter().any(|tag| {
        let (name, value) = tag.as_borrowed().name_and_value();
        name == key && value.is_some_and(|value| matcher.matches(value.as_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use saluki_core::data_model::{
        event::metric::{context::Context, HistogramPoints, Metric, SetPoints, SketchPoints},
        tags::Tag,
    };
    use serde_json::{json, Value};
    use stringtheory::MetaString;

    use super::*;

    /// Builds a test metric from its parts. `None` for the host leaves it unset.
    fn metric(name: &str, host: Option<&str>, tags: &[&str], origin_tags: &[&str]) -> Metric {
        let tags = tags.iter().map(|tag| Tag::from(*tag)).collect::<TagSet>();
        let origin_tags = origin_tags.iter().map(|tag| Tag::from(*tag)).collect::<TagSet>();
        let context = Context::from_parts(name.to_string(), tags)
            .with_origin_tags(origin_tags)
            .with_host(host.map(MetaString::from));
        Metric::gauge(context, 1.0)
    }

    fn evaluate_json(predicate: Value, metric: &Metric) -> bool {
        let predicate = serde_json::from_value::<Predicate>(predicate).expect("test predicate is valid");
        let compiled = predicate.compile(&NoBindings).expect("test predicate compiles");
        compiled.evaluate(&mut EventView::new(metric))
    }

    /// Hand-written expected results for every leaf, including every tag edge case.
    #[test]
    fn leaf_semantics() {
        let plain = metric("app.requests", Some("web-1"), &["env:prod", "team:"], &["pod:web-1"]);
        let unnormalized = metric("my metric-name", None, &[], &[]);
        let no_intake_name = metric("123", Some(""), &[], &[]);
        let bare_and_repeated = metric(
            "x",
            None,
            &["canary", "env:prod", "env:staging", "region:us:east"],
            &["canary"],
        );

        // (predicate, metric, expected)
        let cases: Vec<(Value, &Metric, bool)> = vec![
            (json!({"const": true}), &plain, true),
            (json!({"const": false}), &plain, false),
            (json!({"flag": true}), &plain, true),
            (json!({"flag": false}), &plain, false),
            (json!({"is_empty": []}), &plain, true),
            (json!({"is_empty": ["a"]}), &plain, false),
            // The raw name is compared verbatim.
            (json!({"name": {"eq": "app.requests"}}), &plain, true),
            (json!({"name": {"eq": "app"}}), &plain, false),
            (json!({"name": {"prefix": "app."}}), &plain, true),
            (json!({"name": {"prefix": ""}}), &plain, true),
            (json!({"name": {"in": ["a", "app.requests"]}}), &plain, true),
            (json!({"name": {"prefix_in": ["b.", "app"]}}), &plain, true),
            (json!({"name": {"eq": "my_metric_name"}}), &unnormalized, false),
            (json!({"name": {"eq": "my metric-name"}}), &unnormalized, true),
            (json!({"name": {"eq": "123"}}), &no_intake_name, true),
            // The intake name is the normalized name, and a rejected name has none.
            (json!({"intake_name": {"eq": "my_metric_name"}}), &unnormalized, true),
            (json!({"intake_name": {"eq": "my metric-name"}}), &unnormalized, false),
            (json!({"intake_name": {"prefix": "my_"}}), &unnormalized, true),
            (json!({"intake_name": {"eq": "app.requests"}}), &plain, true),
            (json!({"intake_name": {"eq": "123"}}), &no_intake_name, false),
            (json!({"intake_name": {"prefix": ""}}), &no_intake_name, false),
            (json!({"not": {"intake_name": {"prefix": ""}}}), &no_intake_name, true),
            (
                json!({"intake_name": {"matches": {"values": ["123", ""], "prefix": true}}}),
                &no_intake_name,
                false,
            ),
            // An unset host matches no host leaf, an empty host is a host.
            (json!({"host": {"eq": "web-1"}}), &plain, true),
            (json!({"host": {"prefix": "web-"}}), &plain, true),
            (json!({"host": {"eq": "web"}}), &plain, false),
            (json!({"host": {"prefix": ""}}), &unnormalized, false),
            (json!({"host": {"eq": ""}}), &unnormalized, false),
            (json!({"host": {"eq": ""}}), &no_intake_name, true),
            (json!({"host_unset": {}}), &unnormalized, true),
            (json!({"host_unset": {}}), &no_intake_name, false),
            (json!({"host_unset": {}}), &plain, false),
            // `has_tag` matches bare and valued tags, including an empty value.
            (json!({"has_tag": "env"}), &plain, true),
            (json!({"has_tag": "team"}), &plain, true),
            (json!({"has_tag": "canary"}), &bare_and_repeated, true),
            (json!({"has_tag": "pod"}), &plain, false),
            (json!({"has_tag": "env:prod"}), &plain, false),
            // `tag` matches if any tag with the key has a matching value.
            (json!({"tag": {"key": "env", "value": {"eq": "prod"}}}), &plain, true),
            (
                json!({"tag": {"key": "env", "value": {"eq": "staging"}}}),
                &bare_and_repeated,
                true,
            ),
            (
                json!({"tag": {"key": "env", "value": {"eq": "prod"}}}),
                &bare_and_repeated,
                true,
            ),
            (
                json!({"tag": {"key": "env", "value": {"eq": "dev"}}}),
                &bare_and_repeated,
                false,
            ),
            (
                json!({"tag": {"key": "env", "value": {"in": ["dev", "staging"]}}}),
                &bare_and_repeated,
                true,
            ),
            // `tag` never matches a bare tag, not even with an empty or match-everything value.
            (
                json!({"tag": {"key": "canary", "value": {"eq": ""}}}),
                &bare_and_repeated,
                false,
            ),
            (
                json!({"tag": {"key": "canary", "value": {"prefix": ""}}}),
                &bare_and_repeated,
                false,
            ),
            // An empty value is a value.
            (json!({"tag": {"key": "team", "value": {"eq": ""}}}), &plain, true),
            (json!({"tag": {"key": "team", "value": {"prefix": ""}}}), &plain, true),
            (json!({"tag": {"key": "team", "value": {"eq": "a"}}}), &plain, false),
            // The value is everything after the first colon.
            (
                json!({"tag": {"key": "region", "value": {"eq": "us:east"}}}),
                &bare_and_repeated,
                true,
            ),
            (
                json!({"tag": {"key": "region:us", "value": {"eq": "east"}}}),
                &bare_and_repeated,
                false,
            ),
            // Origin tags are separate from instrumented tags.
            (json!({"has_origin_tag": "pod"}), &plain, true),
            (json!({"has_tag": "pod"}), &plain, false),
            (json!({"has_origin_tag": "env"}), &plain, false),
            (
                json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
                &plain,
                true,
            ),
            (
                json!({"tag": {"key": "pod", "value": {"prefix": "web-"}}}),
                &plain,
                false,
            ),
            (
                json!({"origin_tag": {"key": "env", "value": {"eq": "prod"}}}),
                &plain,
                false,
            ),
            (json!({"has_origin_tag": "canary"}), &bare_and_repeated, true),
            (
                json!({"origin_tag": {"key": "canary", "value": {"eq": ""}}}),
                &bare_and_repeated,
                false,
            ),
            // Kinds.
            (json!({"kind_in": ["gauge"]}), &plain, true),
            (json!({"kind_in": ["counter", "rate"]}), &plain, false),
            (json!({"kind_in": "all"}), &plain, true),
            (json!({"kind_in": []}), &plain, false),
            // Composition.
            (json!({"all": []}), &plain, true),
            (json!({"any": []}), &plain, false),
            (json!({"all": [{"has_tag": "env"}, {"host_unset": {}}]}), &plain, false),
            (
                json!({"any": [{"has_tag": "nope"}, {"kind_in": ["gauge"]}]}),
                &plain,
                true,
            ),
            (json!({"not": {"has_tag": "env"}}), &plain, false),
        ];

        for (predicate, metric, expected) in cases {
            assert_eq!(
                evaluate_json(predicate.clone(), metric),
                expected,
                "{} on {}",
                predicate,
                metric.context()
            );
        }
    }

    #[test]
    fn kind_leaves_match_every_variant() {
        let context = || Context::from_static_name("m");
        let metrics = [
            (Metric::counter(context(), 1.0), "counter"),
            (Metric::rate(context(), 1.0, Duration::from_secs(10)), "rate"),
            (Metric::rate(context(), 1.0, Duration::ZERO), "rate"),
            (Metric::gauge(context(), 1.0), "gauge"),
            (Metric::set(context(), SetPoints::from("a")), "set"),
            (Metric::histogram(context(), HistogramPoints::from(1.0)), "histogram"),
            (Metric::distribution(context(), SketchPoints::from(1.0)), "distribution"),
        ];
        let kinds = ["counter", "rate", "gauge", "set", "histogram", "distribution"];

        for (metric, kind) in &metrics {
            for candidate in kinds {
                assert_eq!(
                    evaluate_json(json!({"kind_in": [candidate]}), metric),
                    candidate == *kind,
                    "{} on a {}",
                    candidate,
                    kind
                );
            }
        }
    }

    #[test]
    fn intake_name_is_not_computed_when_short_circuited() {
        // `all` stops at the first false operand, so the intake name is never needed.
        let predicate = serde_json::from_value::<Predicate>(json!({"all": [
            {"const": false}, {"intake_name": {"eq": "x"}}
        ]}))
        .unwrap()
        .compile(&NoBindings)
        .unwrap();
        let metric = metric("needs normalizing", None, &[], &[]);
        let mut view = EventView::new(&metric);
        assert!(!predicate.evaluate(&mut view));
        assert!(
            !view.has_computed_intake_name(),
            "the intake name was computed but never needed"
        );
    }

    struct Bindings(HashMap<&'static str, Value>);

    impl BindingResolver for Bindings {
        fn resolve_bool(&self, binding: &BindingName) -> Option<bool> {
            self.0.get(binding.as_str()).and_then(Value::as_bool)
        }

        fn resolve_strings(&self, binding: &BindingName) -> Option<Vec<String>> {
            serde_json::from_value(self.0.get(binding.as_str())?.clone()).ok()
        }
    }

    #[test]
    fn operands_resolve_from_the_plan_then_the_resolver() {
        let filterlist = json!({"intake_name": {"matches": {
            "values": {"config": "filter.values"},
            "prefix": {"config": "filter.prefix"}}}});
        let predicate = serde_json::from_value::<Predicate>(filterlist).unwrap();
        let metric = metric("blocked.metric", None, &[], &[]);

        // Unresolved bindings with no resolver fail to compile, naming the binding.
        let error = predicate.compile(&NoBindings).unwrap_err();
        assert!(error.to_string().contains("`filter.values` has no value"), "{}", error);

        // The resolver supplies unresolved bindings.
        let bindings = Bindings(HashMap::from([
            ("filter.values", json!(["blocked."])),
            ("filter.prefix", json!(true)),
        ]));
        let compiled = predicate.compile(&bindings).unwrap();
        assert!(compiled.evaluate(&mut EventView::new(&metric)));

        // A resolved value in the plan takes precedence over the resolver.
        let resolved = serde_json::from_value::<Predicate>(json!({"intake_name": {"matches": {
            "values": {"config": "filter.values", "resolved": ["blocked."]},
            "prefix": {"config": "filter.prefix", "resolved": false}}}}))
        .unwrap();
        assert!(!resolved
            .compile(&bindings)
            .unwrap()
            .evaluate(&mut EventView::new(&metric)));
        assert!(!resolved
            .compile(&NoBindings)
            .unwrap()
            .evaluate(&mut EventView::new(&metric)));

        // Flags and emptiness checks resolve the same way. This is the shape of a mirroring gateway: forward nothing
        // unless enabled, and then everything if the allowlist is empty, or only the names it lists.
        let gateway = serde_json::from_value::<Predicate>(json!({"all": [
            {"flag": {"config": "mirroring.enabled"}},
            {"any": [
                {"is_empty": {"config": "mirroring.allowlist"}},
                {"name": {"matches": {"values": {"config": "mirroring.allowlist"}, "prefix": false}}}
            ]}
        ]}))
        .unwrap();
        let bindings = |enabled: bool, allowlist: Value| {
            Bindings(HashMap::from([
                ("mirroring.enabled", json!(enabled)),
                ("mirroring.allowlist", allowlist),
            ]))
        };
        let listed = self::metric("listed", None, &[], &[]);
        let unlisted = self::metric("unlisted", None, &[], &[]);
        let run = |bindings: &Bindings, metric: &Metric| {
            gateway.compile(bindings).unwrap().evaluate(&mut EventView::new(metric))
        };

        assert!(
            run(&bindings(true, json!([])), &unlisted),
            "an empty allowlist forwards everything"
        );
        assert!(
            !run(&bindings(false, json!([])), &unlisted),
            "a disabled gateway forwards nothing"
        );
        assert!(
            !run(&bindings(false, json!(["listed"])), &listed),
            "a disabled gateway forwards nothing"
        );
        assert!(
            run(&bindings(true, json!(["listed"])), &listed),
            "a listed name is forwarded"
        );
        assert!(
            !run(&bindings(true, json!(["listed"])), &unlisted),
            "an unlisted name is not"
        );
    }
}
