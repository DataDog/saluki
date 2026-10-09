//! Canonical forms of predicates.

use super::{Predicate, SetMatch, StringMatch, TagMatch};
use crate::{canonical::to_canonical_string, ir::Operand};

pub(super) fn canonicalize(predicate: &Predicate) -> Predicate {
    match predicate {
        Predicate::Const(_)
        | Predicate::Flag(_)
        | Predicate::IsEmpty(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::KindIn(_) => predicate.clone(),
        Predicate::Name(matcher) => Predicate::Name(canonical_string_match(matcher)),
        Predicate::IntakeName(matcher) => Predicate::IntakeName(canonical_string_match(matcher)),
        Predicate::Host(matcher) => Predicate::Host(canonical_string_match(matcher)),
        Predicate::Tag(tag) => Predicate::Tag(canonical_tag_match(tag)),
        Predicate::OriginTag(tag) => Predicate::OriginTag(canonical_tag_match(tag)),
        Predicate::All(operands) => canonical_junction(operands, true),
        Predicate::Any(operands) => canonical_junction(operands, false),
        Predicate::Not(operand) => match canonicalize(operand) {
            Predicate::Not(inner) => *inner,
            operand => Predicate::Not(Box::new(operand)),
        },
    }
}

/// Returns the canonical form of an `all` (when `is_all`) or an `any`.
fn canonical_junction(operands: &[Predicate], is_all: bool) -> Predicate {
    let mut flattened = Vec::with_capacity(operands.len());
    for operand in operands {
        match (canonicalize(operand), is_all) {
            // Canonical operands never directly contain a junction of their own kind, so one level of flattening is
            // enough.
            (Predicate::All(inner), true) | (Predicate::Any(inner), false) => flattened.extend(inner),
            (operand, _) => flattened.push(operand),
        }
    }

    let mut keyed = flattened
        .into_iter()
        .map(|operand| (sort_key(&operand), operand))
        .collect::<Vec<_>>();
    keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
    keyed.dedup_by(|(a, _), (b, _)| a == b);

    let mut operands = keyed.into_iter().map(|(_, operand)| operand).collect::<Vec<_>>();
    match operands.len() {
        0 => Predicate::Const(is_all),
        1 => operands.remove(0),
        _ if is_all => Predicate::All(operands),
        _ => Predicate::Any(operands),
    }
}

/// Returns the key operands are sorted and deduplicated by: their canonical JSON.
fn sort_key(predicate: &Predicate) -> String {
    // Predicates contain only strings, booleans, lists, and objects, so converting one to canonical JSON cannot fail.
    // Should that ever change, the debug representation still gives a deterministic total order.
    serde_json::to_value(predicate)
        .ok()
        .and_then(|value| to_canonical_string(&value).ok())
        .unwrap_or_else(|| format!("{:?}", predicate))
}

fn canonical_tag_match(tag: &TagMatch) -> TagMatch {
    TagMatch {
        key: tag.key.clone(),
        value: canonical_string_match(&tag.value),
    }
}

fn canonical_string_match(matcher: &StringMatch) -> StringMatch {
    match matcher {
        StringMatch::Eq(_) | StringMatch::Prefix(_) => matcher.clone(),
        StringMatch::In(values) => StringMatch::Matches(SetMatch {
            values: Operand::literal(sorted_unique(values)),
            prefix: Operand::literal(false),
        }),
        StringMatch::PrefixIn(values) => StringMatch::Matches(SetMatch {
            values: Operand::literal(sorted_unique(values)),
            prefix: Operand::literal(true),
        }),
        StringMatch::Matches(set) => StringMatch::Matches(SetMatch {
            values: match &set.values {
                Operand::Literal(values) => Operand::literal(sorted_unique(values)),
                Operand::Config { binding, resolved } => Operand::Config {
                    binding: binding.clone(),
                    resolved: resolved.as_deref().map(sorted_unique),
                },
            },
            prefix: set.prefix.clone(),
        }),
    }
}

fn sorted_unique(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(value: serde_json::Value) -> Predicate {
        serde_json::from_value(value).expect("test predicate is valid")
    }

    /// Hand-written canonical forms: (input, expected canonical form).
    #[test]
    fn canonical_forms() {
        let cases = vec![
            // Leaves are unchanged, apart from string-match shorthand.
            (json!({"name": {"eq": "a"}}), json!({"name": {"eq": "a"}})),
            (json!({"host_unset": {}}), json!({"host_unset": {}})),
            (
                json!({"name": {"in": ["b", "a", "b"]}}),
                json!({"name": {"matches": {"values": ["a", "b"], "prefix": false}}}),
            ),
            (
                json!({"intake_name": {"prefix_in": ["z.", "a."]}}),
                json!({"intake_name": {"matches": {"values": ["a.", "z."], "prefix": true}}}),
            ),
            (
                json!({"tag": {"key": "env", "value": {"in": ["prod", "dev"]}}}),
                json!({"tag": {"key": "env", "value": {"matches": {"values": ["dev", "prod"], "prefix": false}}}}),
            ),
            // Literal values of `matches` are sorted; resolved values are too, but bindings stay bindings.
            (
                json!({"host": {"matches": {"values": ["b", "a"], "prefix": {"config": "p"}}}}),
                json!({"host": {"matches": {"values": ["a", "b"], "prefix": {"config": "p"}}}}),
            ),
            (
                json!({"host": {"matches": {"values": {"config": "v", "resolved": ["b", "a", "a"]}, "prefix": true}}}),
                json!({"host": {"matches": {"values": {"config": "v", "resolved": ["a", "b"]}, "prefix": true}}}),
            ),
            // Nested junctions of the same kind are flattened; operands are sorted by canonical JSON and deduplicated.
            (
                json!({"all": [{"has_tag": "b"}, {"all": [{"has_tag": "a"}, {"has_tag": "b"}]}]}),
                json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}]}),
            ),
            (
                json!({"any": [{"const": true}, {"any": [{"any": [{"has_tag": "a"}]}, {"const": false}]}]}),
                json!({"any": [{"const": false}, {"const": true}, {"has_tag": "a"}]}),
            ),
            // Junctions of different kinds are not flattened into each other.
            (
                json!({"all": [{"any": [{"has_tag": "b"}, {"has_tag": "a"}]}, {"has_tag": "c"}]}),
                json!({"all": [{"any": [{"has_tag": "a"}, {"has_tag": "b"}]}, {"has_tag": "c"}]}),
            ),
            // Shorthand and its expansion are the same operand, so they deduplicate.
            (
                json!({"any": [{"name": {"in": ["a"]}}, {"name": {"matches": {"values": ["a"], "prefix": false}}}]}),
                json!({"name": {"matches": {"values": ["a"], "prefix": false}}}),
            ),
            // Single operands are unwrapped, and empty junctions become constants.
            (json!({"all": [{"has_tag": "a"}]}), json!({"has_tag": "a"})),
            (json!({"all": []}), json!({"const": true})),
            (json!({"any": []}), json!({"const": false})),
            (json!({"all": [{"any": []}]}), json!({"const": false})),
            // Double negation is removed, including when it only appears after unwrapping.
            (json!({"not": {"not": {"has_tag": "a"}}}), json!({"has_tag": "a"})),
            (
                json!({"not": {"not": {"not": {"has_tag": "a"}}}}),
                json!({"not": {"has_tag": "a"}}),
            ),
            (
                json!({"not": {"all": [{"not": {"has_tag": "a"}}]}}),
                json!({"has_tag": "a"}),
            ),
            // Constants are not folded: that is a rewrite, not a canonical form.
            (
                json!({"all": [{"const": true}, {"has_tag": "a"}]}),
                json!({"all": [{"const": true}, {"has_tag": "a"}]}),
            ),
        ];

        for (input, expected) in cases {
            let canonical = parse(input.clone()).canonical();
            assert_eq!(serde_json::to_value(&canonical).unwrap(), expected, "input: {}", input);
            assert_eq!(
                canonical.canonical(),
                canonical,
                "canonical forms are fixed points: {}",
                input
            );
        }
    }

    #[test]
    fn equivalent_predicates_share_a_canonical_form() {
        let pairs = [
            (
                json!({"all": [{"has_tag": "a"}, {"kind_in": ["gauge"]}]}),
                json!({"all": [{"kind_in": ["gauge"]}, {"all": [{"has_tag": "a"}, {"has_tag": "a"}]}]}),
            ),
            (
                json!({"name": {"prefix_in": ["b.", "a."]}}),
                json!({"name": {"matches": {"values": ["a.", "b.", "a."], "prefix": true}}}),
            ),
            (
                json!({"not": {"not": {"host_unset": {}}}}),
                json!({"any": [{"host_unset": {}}]}),
            ),
        ];
        for (a, b) in pairs {
            assert_eq!(
                parse(a.clone()).canonical(),
                parse(b.clone()).canonical(),
                "{} vs {}",
                a,
                b
            );
        }

        // Different predicates keep different canonical forms.
        assert_ne!(
            parse(json!({"name": {"in": ["a"]}})).canonical(),
            parse(json!({"name": {"prefix_in": ["a"]}})).canonical()
        );
        assert_ne!(
            parse(json!({"has_tag": "a"})).canonical(),
            parse(json!({"has_origin_tag": "a"})).canonical()
        );
    }

    mod properties {
        use proptest::prelude::*;

        use crate::{
            model::EventView,
            predicate::{NoBindings, Predicate},
            testing::{arb_metric, arb_predicate},
        };

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            #[test]
            fn property_test_canonical_form_preserves_evaluation(
                predicate in arb_predicate(),
                metrics in proptest::collection::vec(arb_metric(), 1..8),
            ) {
                let original = predicate.compile(&NoBindings).expect("generated predicates compile");
                let canonical = predicate.canonical().compile(&NoBindings).expect("canonical forms compile");
                for metric in &metrics {
                    prop_assert_eq!(
                        original.evaluate(&mut EventView::new(metric)),
                        canonical.evaluate(&mut EventView::new(metric)),
                        "canonical form {:?} disagrees with {:?} on {:?}",
                        predicate.canonical(),
                        predicate,
                        metric
                    );
                }
            }

            #[test]
            fn property_test_canonical_form_is_a_fixed_point(predicate in arb_predicate()) {
                let canonical = predicate.canonical();
                prop_assert_eq!(canonical.canonical(), canonical.clone());
                // Reading fields is unaffected by canonicalization.
                prop_assert_eq!(canonical.reads(), predicate.reads());
            }

            #[test]
            fn property_test_predicates_round_trip_through_json(predicate in arb_predicate()) {
                let json = serde_json::to_string(&predicate).expect("predicates serialize");
                let parsed = serde_json::from_str::<Predicate>(&json).expect("serialized predicates parse");
                prop_assert_eq!(parsed, predicate);
            }
        }
    }
}
