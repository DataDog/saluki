//! Property-testing strategies for metrics and predicates, and hand-written plan fixtures.
//!
//! The strategies favor the edge cases that metric semantics hinge on, rather than uniformly random values:
//!
//! - names that are empty, longer than the intake's 350-byte limit, without any ASCII letter, or with multi-byte UTF-8,
//!   alongside ordinary names and names that need normalizing;
//! - an unset host as well as an empty one;
//! - bare tags, tags with an empty value, values containing a colon, and repeated tags, in both tags and origin tags;
//! - every metric kind, with and without timestamps, and rates with zero and non-zero intervals.
//!
//! Names, tag keys, and tag values are drawn largely from small vocabularies shared with [`arb_predicate`], so that
//! generated predicates match generated metrics often enough to be interesting.
//!
//! The metric strategies and their vocabularies are those of `saluki_metric_kernels::testing`, re-exported here, so
//! that the kernels' property tests and the plan's draw from the same metrics.

use proptest::{collection::vec, prelude::*, sample::select};
pub use saluki_metric_kernels::testing::{
    arb_host, arb_metric, arb_metric_name, arb_metric_values, arb_tag, arb_tags, HOST_VOCABULARY, NAME_VOCABULARY,
    PREFIX_VOCABULARY, TAG_KEY_VOCABULARY, TAG_VALUE_VOCABULARY,
};

use crate::{
    ir::{BindingName, Operand},
    model::{Kind, KindSet},
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};

mod fixtures;
pub use self::fixtures::{default_program_catalog, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON};

fn arb_strings(vocabulary: &'static [&'static str]) -> impl Strategy<Value = Vec<String>> {
    vec(select(vocabulary).prop_map(str::to_string), 0..4)
}

fn arb_string_match(
    values: &'static [&'static str], prefixes: &'static [&'static str],
) -> impl Strategy<Value = StringMatch> {
    let binding = BindingName::new("test.values").expect("valid binding name");
    prop_oneof![
        select(values).prop_map(|value| StringMatch::Eq(value.to_string())),
        select(prefixes).prop_map(|value| StringMatch::Prefix(value.to_string())),
        arb_strings(values).prop_map(StringMatch::In),
        arb_strings(prefixes).prop_map(StringMatch::PrefixIn),
        (arb_strings(prefixes), any::<bool>()).prop_map(|(values, prefix)| {
            StringMatch::Matches(SetMatch {
                values: Operand::literal(values),
                prefix: Operand::literal(prefix),
            })
        }),
        (arb_strings(values), any::<bool>()).prop_map(move |(values, prefix)| {
            StringMatch::Matches(SetMatch {
                values: Operand::config(binding.clone()).with_resolved(values),
                prefix: Operand::literal(prefix),
            })
        }),
    ]
}

fn arb_tag_match() -> impl Strategy<Value = TagMatch> {
    (
        select(TAG_KEY_VOCABULARY),
        arb_string_match(TAG_VALUE_VOCABULARY, TAG_VALUE_VOCABULARY),
    )
        .prop_map(|(key, value)| TagMatch {
            key: key.to_string(),
            value,
        })
}

fn arb_leaf() -> impl Strategy<Value = Predicate> {
    let kinds = vec(select(Kind::ALL.to_vec()), 0..3).prop_map(|kinds| Predicate::KindIn(kinds.into_iter().collect()));
    prop_oneof![
        any::<bool>().prop_map(Predicate::Const),
        any::<bool>().prop_map(|flag| Predicate::Flag(Operand::literal(flag))),
        arb_strings(NAME_VOCABULARY).prop_map(|values| Predicate::IsEmpty(Operand::literal(values))),
        arb_string_match(NAME_VOCABULARY, PREFIX_VOCABULARY).prop_map(Predicate::Name),
        arb_string_match(NAME_VOCABULARY, PREFIX_VOCABULARY).prop_map(Predicate::IntakeName),
        arb_string_match(HOST_VOCABULARY, HOST_VOCABULARY).prop_map(Predicate::Host),
        Just(Predicate::HostUnset {}),
        select(TAG_KEY_VOCABULARY).prop_map(|key| Predicate::HasTag(key.to_string())),
        arb_tag_match().prop_map(Predicate::Tag),
        select(TAG_KEY_VOCABULARY).prop_map(|key| Predicate::HasOriginTag(key.to_string())),
        arb_tag_match().prop_map(Predicate::OriginTag),
        kinds,
        Just(Predicate::KindIn(KindSet::ALL)),
    ]
}

/// Returns a strategy for predicates: every leaf, composed with `all`, `any`, and `not` up to a small depth.
///
/// Operands are literal or carry resolved values, so every generated predicate compiles without bindings.
pub fn arb_predicate() -> impl Strategy<Value = Predicate> {
    arb_leaf().prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..4).prop_map(Predicate::All),
            vec(inner.clone(), 0..4).prop_map(Predicate::Any),
            inner.prop_map(|operand| Predicate::Not(Box::new(operand))),
        ]
    })
}

#[cfg(test)]
mod tests {
    use proptest::strategy::ValueTree as _;
    use proptest::test_runner::TestRunner;
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;
    use crate::model::EventView;

    /// Draws `count` values from `strategy` with a deterministic runner.
    fn sample<S: Strategy>(strategy: S, count: usize) -> Vec<S::Value> {
        let mut runner = TestRunner::deterministic();
        (0..count)
            .map(|_| {
                strategy
                    .new_tree(&mut runner)
                    .expect("strategy produces values")
                    .current()
            })
            .collect()
    }

    #[test]
    fn metric_strategy_covers_the_edge_cases() {
        let metrics = sample(arb_metric(), 2000);
        let mut views = metrics.iter().map(EventView::new).collect::<Vec<_>>();

        let covered = |what: &str, found: bool| assert!(found, "no generated metric has {}", what);
        covered("an empty name", views.iter().any(|v| v.name().is_empty()));
        covered("an over-long name", views.iter().any(|v| v.name().len() > 350));
        covered(
            "a name without an ASCII letter",
            views
                .iter()
                .any(|v| !v.name().is_empty() && !v.name().bytes().any(|b| b.is_ascii_alphabetic())),
        );
        covered("a multi-byte name", views.iter().any(|v| !v.name().is_ascii()));
        covered("no intake name", views.iter_mut().any(|v| v.intake_name().is_none()));
        covered("an unset host", views.iter().any(|v| v.host().is_none()));
        covered("an empty host", views.iter().any(|v| v.host() == Some("")));
        covered(
            "a bare tag",
            views.iter().any(|v| v.tags().into_iter().any(|t| t.value().is_none())),
        );
        covered(
            "an empty tag value",
            views
                .iter()
                .any(|v| v.tags().into_iter().any(|t| t.value() == Some(""))),
        );
        covered("origin tags", views.iter().any(|v| !v.origin_tags().is_empty()));
        for kind in Kind::ALL {
            covered(kind.as_str(), views.iter().any(|v| v.kind() == kind));
        }
        covered(
            "timestamped points",
            metrics.iter().any(|m| m.values().any_timestamped()),
        );
        covered(
            "untimestamped points",
            metrics.iter().any(|m| !m.values().all_timestamped()),
        );
        covered(
            "a zero rate interval",
            metrics
                .iter()
                .any(|m| matches!(m.values(), MetricValues::Rate(_, interval) if interval.is_zero())),
        );
        covered(
            "a non-zero rate interval",
            metrics
                .iter()
                .any(|m| matches!(m.values(), MetricValues::Rate(_, interval) if !interval.is_zero())),
        );
    }

    #[test]
    fn predicate_strategy_produces_compilable_predicates_that_sometimes_match() {
        let predicates = sample(arb_predicate(), 500);
        let metrics = sample(arb_metric(), 50);

        let mut matched = 0;
        let mut evaluated = 0;
        for predicate in &predicates {
            let compiled = predicate
                .compile(&crate::predicate::NoBindings)
                .expect("generated predicates compile");
            for metric in &metrics {
                evaluated += 1;
                if compiled.evaluate(&mut EventView::new(metric)) {
                    matched += 1;
                }
            }
        }
        assert!(
            matched > evaluated / 20,
            "only {} of {} evaluations matched",
            matched,
            evaluated
        );
        assert!(
            matched < evaluated - evaluated / 20,
            "{} of {} evaluations matched",
            matched,
            evaluated
        );
    }
}
