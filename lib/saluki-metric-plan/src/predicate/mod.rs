//! Predicates over metrics.
//!
//! A [`Predicate`] decides, for one metric, whether it matches. Routing operators such as `split` take a predicate as a
//! parameter. Predicates read the raw `name`, the derived `intake_name`, the host, tags and origin tags, and the kind;
//! every leaf names the field it reads, so there is no default view of the name.
//!
//! # Serialization
//!
//! Every predicate is a JSON object with a single member that names its form:
//!
//! | Predicate | JSON |
//! |---|---|
//! | Constant | `{"const": true}` |
//! | Configuration flag | `{"flag": true}` or `{"flag": {"config": "<binding>"}}` |
//! | Empty list | `{"is_empty": ["a"]}` or `{"is_empty": {"config": "<binding>"}}` |
//! | Name, intake name, or host | `{"name": <string match>}`, `{"intake_name": ...}`, `{"host": ...}` |
//! | Unset host | `{"host_unset": {}}` |
//! | Tag key | `{"has_tag": "<key>"}`, `{"has_origin_tag": "<key>"}` |
//! | Tag value | `{"tag": {"key": "<key>", "value": <string match>}}`, and `origin_tag` likewise |
//! | Kind | `{"kind_in": ["counter", "rate"]}` or `{"kind_in": "all"}` |
//! | Composition | `{"all": [...]}`, `{"any": [...]}`, `{"not": {...}}` |
//!
//! A string match is `{"eq": "<s>"}`, `{"prefix": "<s>"}`, `{"in": [...]}`, `{"prefix_in": [...]}`, or
//! `{"matches": {"values": <list or binding>, "prefix": <bool or binding>}}`. `in` and `prefix_in` are shorthand for
//! `matches` with literal values and mode.
//!
//! # Semantics
//!
//! - String matches compare the field's value verbatim. `intake_name` compares the name as the intake stores it; a
//!   metric whose name the intake rejects has no intake name, and every `intake_name` leaf is false for it.
//! - `host` leaves are false for a metric with no host. `host_unset` is true only for a metric with no host; an empty
//!   host is a host.
//! - `has_tag(key)` is true if any tag has the key, with or without a value. `tag(key)` is true if **any** tag with the
//!   key has a value that matches; it never matches a bare tag, which has no value. A tag `key:` has the empty value.
//! - `origin_tag` and `has_origin_tag` read the origin tags, which are separate from the instrumented tags.
//! - `kind_in` matches the metric's kind, and never looks at a rate's interval.
//! - `flag` and `is_empty` depend only on their operands, which are fixed for a generation. They express configuration
//!   toggles, and allowlists for which an empty list forwards everything, without special-case nodes.
//!
//! [`Predicate::compile`] resolves a predicate's operands and builds its string matches once, so that
//! [evaluating][CompiledPredicate::evaluate] it never allocates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ir::Operand,
    model::{DerivedField, Field, FieldSet, Kind, KindSet},
};

mod canonical;

mod eval;
pub use self::eval::{BindingResolver, CompiledPredicate, NoBindings, PredicateError};

/// A predicate over one metric.
///
/// See the [module documentation](self) for the serialized form and the semantics of each predicate.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    /// Always true or always false.
    Const(bool),

    /// True if the flag is set.
    Flag(Operand<bool>),

    /// True if the list is empty.
    IsEmpty(Operand<Vec<String>>),

    /// Matches the raw name.
    Name(StringMatch),

    /// Matches the intake name. False when the metric has no intake name.
    IntakeName(StringMatch),

    /// Matches the host. False when the host is unset.
    Host(StringMatch),

    /// True when the host is unset.
    HostUnset {},

    /// True if any tag has the key, with or without a value.
    HasTag(String),

    /// True if any tag with the key has a value that matches.
    Tag(TagMatch),

    /// True if any origin tag has the key, with or without a value.
    HasOriginTag(String),

    /// True if any origin tag with the key has a value that matches.
    OriginTag(TagMatch),

    /// True if the metric's kind is in the set.
    KindIn(KindSet),

    /// True if every operand is true. True when there are no operands.
    All(Vec<Predicate>),

    /// True if any operand is true. False when there are no operands.
    Any(Vec<Predicate>),

    /// True if the operand is false.
    Not(Box<Predicate>),
}

/// How a string field is matched.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StringMatch {
    /// The value equals the string.
    Eq(String),

    /// The value starts with the string.
    Prefix(String),

    /// The value equals one of the strings.
    In(Vec<String>),

    /// The value starts with one of the strings.
    PrefixIn(Vec<String>),

    /// The value matches one of the strings, exactly or by prefix.
    Matches(SetMatch),
}

/// A match against a list of strings, either of which may be bound to configuration.
///
/// This is the form native filterlists and allowlists lower to.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetMatch {
    /// The strings to match. Entries are compared verbatim, never normalized.
    pub values: Operand<Vec<String>>,

    /// Whether values match by prefix rather than exactly.
    pub prefix: Operand<bool>,
}

/// A match against the values of the tags with one key.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TagMatch {
    /// The tag key.
    pub key: String,

    /// How the tag's value is matched.
    pub value: StringMatch,
}

impl Predicate {
    /// Returns the stored fields this predicate reads.
    ///
    /// Reading a derived field counts as reading its sources, so an `intake_name` leaf reads `name`.
    pub fn reads(&self) -> FieldSet {
        match self {
            Self::Const(_) | Self::Flag(_) | Self::IsEmpty(_) => FieldSet::EMPTY,
            Self::Name(_) => FieldSet::of(&[Field::Name]),
            Self::IntakeName(_) => DerivedField::IntakeName.sources(),
            Self::Host(_) | Self::HostUnset {} => FieldSet::of(&[Field::Host]),
            Self::HasTag(_) | Self::Tag(_) => FieldSet::of(&[Field::Tags]),
            Self::HasOriginTag(_) | Self::OriginTag(_) => FieldSet::of(&[Field::OriginTags]),
            Self::KindIn(_) => FieldSet::of(&[Field::Kind]),
            Self::All(operands) | Self::Any(operands) => operands
                .iter()
                .fold(FieldSet::EMPTY, |reads, operand| reads.union(operand.reads())),
            Self::Not(operand) => operand.reads(),
        }
    }

    /// Returns the predicate's value for every metric of `kind`, when the kind alone decides it.
    ///
    /// `Some(value)` means the predicate is `value` for every metric of that kind, whatever its other fields;
    /// `None` means the result can depend on other fields. `const` leaves and `kind_in` leaves decide; every other leaf
    /// is treated as undecided, including `flag` and `is_empty`: their operands are fixed for one generation but can
    /// change between generations, and port types must not depend on them. `all` and `any` follow three-valued logic,
    /// so `all` is `Some(false)` as soon as one operand is.
    ///
    /// Validation uses this to compute which kinds can leave each port of a `split`.
    pub fn truth_for_kind(&self, kind: Kind) -> Option<bool> {
        match self {
            Self::Const(value) => Some(*value),
            Self::KindIn(kinds) => Some(kinds.contains(kind)),
            Self::All(operands) => {
                let mut decided = Some(true);
                for operand in operands {
                    match operand.truth_for_kind(kind) {
                        Some(false) => return Some(false),
                        Some(true) => {}
                        None => decided = None,
                    }
                }
                decided
            }
            Self::Any(operands) => {
                let mut decided = Some(false);
                for operand in operands {
                    match operand.truth_for_kind(kind) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => decided = None,
                    }
                }
                decided
            }
            Self::Not(operand) => operand.truth_for_kind(kind).map(|value| !value),
            Self::Flag(_)
            | Self::IsEmpty(_)
            | Self::Name(_)
            | Self::IntakeName(_)
            | Self::Host(_)
            | Self::HostUnset {}
            | Self::HasTag(_)
            | Self::Tag(_)
            | Self::HasOriginTag(_)
            | Self::OriginTag(_) => None,
        }
    }

    /// Returns the kinds, among `kinds`, for which this predicate can be `value`.
    ///
    /// A kind is included unless [`truth_for_kind`][Self::truth_for_kind] decides the predicate is `!value` for it.
    pub fn kinds_where(&self, value: bool, kinds: KindSet) -> KindSet {
        kinds
            .iter()
            .filter(|kind| self.truth_for_kind(*kind) != Some(!value))
            .collect()
    }

    /// Returns the number of leaves: predicates that are not `all`, `any`, or `not`.
    ///
    /// Limits on predicate size count leaves.
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::All(operands) | Self::Any(operands) => operands.iter().map(Self::leaf_count).sum(),
            Self::Not(operand) => operand.leaf_count(),
            _ => 1,
        }
    }

    /// Returns the predicate's canonical form.
    ///
    /// Two predicates with the same canonical form are equivalent, which is how rewrites and the duplicate analysis
    /// compare predicates. Canonicalization:
    ///
    /// - flattens `all` nested directly in `all`, and `any` in `any`;
    /// - sorts the operands of `all` and `any` by their canonical JSON, and removes duplicates;
    /// - replaces an `all` or `any` of exactly one operand with that operand, an empty `all` with `const(true)`, and an
    ///   empty `any` with `const(false)`;
    /// - removes double negation;
    /// - rewrites `in` and `prefix_in` as the `matches` they are shorthand for, and sorts and deduplicates literal
    ///   value lists in `matches`, whose order does not matter.
    ///
    /// The canonical form is a fixed point: computing the canonical form of a canonical form changes nothing.
    pub fn canonical(&self) -> Self {
        canonical::canonicalize(self)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::{ir::BindingName, model::Kind};

    fn binding(name: &str) -> BindingName {
        BindingName::new(name).unwrap()
    }

    fn literal_strings(values: &[&str]) -> Operand<Vec<String>> {
        Operand::literal(values.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn every_form_round_trips_through_its_documented_json() {
        let cases: Vec<(Predicate, Value)> = vec![
            (Predicate::Const(false), json!({"const": false})),
            (Predicate::Flag(Operand::literal(true)), json!({"flag": true})),
            (
                Predicate::Flag(Operand::config(binding("mrf.enabled"))),
                json!({"flag": {"config": "mrf.enabled"}}),
            ),
            (Predicate::IsEmpty(literal_strings(&["a"])), json!({"is_empty": ["a"]})),
            (
                Predicate::Name(StringMatch::Eq("requests".into())),
                json!({"name": {"eq": "requests"}}),
            ),
            (
                Predicate::IntakeName(StringMatch::Prefix("demo.".into())),
                json!({"intake_name": {"prefix": "demo."}}),
            ),
            (
                Predicate::Host(StringMatch::In(vec!["a".into(), "b".into()])),
                json!({"host": {"in": ["a", "b"]}}),
            ),
            (
                Predicate::Name(StringMatch::PrefixIn(vec!["x.".into()])),
                json!({"name": {"prefix_in": ["x."]}}),
            ),
            (Predicate::HostUnset {}, json!({"host_unset": {}})),
            (Predicate::HasTag("env".into()), json!({"has_tag": "env"})),
            (
                Predicate::Tag(TagMatch {
                    key: "team".into(),
                    value: StringMatch::Eq("platform".into()),
                }),
                json!({"tag": {"key": "team", "value": {"eq": "platform"}}}),
            ),
            (Predicate::HasOriginTag("pod".into()), json!({"has_origin_tag": "pod"})),
            (
                Predicate::OriginTag(TagMatch {
                    key: "pod".into(),
                    value: StringMatch::Prefix("web-".into()),
                }),
                json!({"origin_tag": {"key": "pod", "value": {"prefix": "web-"}}}),
            ),
            (
                Predicate::KindIn(KindSet::of(&[Kind::Counter, Kind::Gauge])),
                json!({"kind_in": ["counter", "gauge"]}),
            ),
            (Predicate::KindIn(KindSet::ALL), json!({"kind_in": "all"})),
            (
                Predicate::All(vec![
                    Predicate::Const(true),
                    Predicate::Not(Box::new(Predicate::HostUnset {})),
                ]),
                json!({"all": [{"const": true}, {"not": {"host_unset": {}}}]}),
            ),
            (Predicate::Any(vec![]), json!({"any": []})),
        ];

        for (predicate, expected) in cases {
            assert_eq!(serde_json::to_value(&predicate).unwrap(), expected, "{:?}", predicate);
            assert_eq!(serde_json::from_value::<Predicate>(expected).unwrap(), predicate);
        }
    }

    /// The filterlist predicate from the implementation design's IR example.
    #[test]
    fn bound_matches_round_trip() {
        let value = json!({"intake_name": {"matches": {
            "values": {"config": "dogstatsd.metric_filter.values"},
            "prefix": {"config": "dogstatsd.metric_filter.match_prefix"}}}});
        let predicate = serde_json::from_value::<Predicate>(value.clone()).unwrap();
        assert_eq!(
            predicate,
            Predicate::IntakeName(StringMatch::Matches(SetMatch {
                values: Operand::config(binding("dogstatsd.metric_filter.values")),
                prefix: Operand::config(binding("dogstatsd.metric_filter.match_prefix")),
            }))
        );
        assert_eq!(serde_json::to_value(&predicate).unwrap(), value);
    }

    #[test]
    fn malformed_predicates_are_rejected() {
        let cases = [
            json!({"name": {"equals": "x"}}),
            json!({"name": {"eq": "x"}, "host_unset": {}}),
            json!({"host_unset": {"extra": 1}}),
            json!({"tag": {"key": "a", "value": {"eq": "b"}, "extra": 1}}),
            json!({"tag": {"key": "a"}}),
            json!({"intake_name": {"matches": {"values": ["a"]}}}),
            json!({"intake_name": {"matches": {"values": ["a"], "prefix": false, "extra": 1}}}),
            json!({"kind_in": ["counter", "counter"]}),
            json!({"not": [{"const": true}]}),
            json!({"const": "true"}),
            json!("host_unset"),
            json!({}),
        ];
        for value in cases {
            assert!(
                serde_json::from_value::<Predicate>(value.clone()).is_err(),
                "{} should be rejected",
                value
            );
        }
    }

    #[test]
    fn reads_report_the_stored_fields_each_leaf_depends_on() {
        let tag = TagMatch {
            key: "env".into(),
            value: StringMatch::Eq("prod".into()),
        };
        let cases: Vec<(Predicate, FieldSet)> = vec![
            (Predicate::Const(true), FieldSet::EMPTY),
            (Predicate::Flag(Operand::literal(true)), FieldSet::EMPTY),
            (Predicate::IsEmpty(literal_strings(&[])), FieldSet::EMPTY),
            (
                Predicate::Name(StringMatch::Eq("a".into())),
                FieldSet::of(&[Field::Name]),
            ),
            // The intake name is derived from the name, so reading it reads the name.
            (
                Predicate::IntakeName(StringMatch::Eq("a".into())),
                FieldSet::of(&[Field::Name]),
            ),
            (
                Predicate::Host(StringMatch::Eq("a".into())),
                FieldSet::of(&[Field::Host]),
            ),
            (Predicate::HostUnset {}, FieldSet::of(&[Field::Host])),
            (Predicate::HasTag("env".into()), FieldSet::of(&[Field::Tags])),
            (Predicate::Tag(tag.clone()), FieldSet::of(&[Field::Tags])),
            (
                Predicate::HasOriginTag("env".into()),
                FieldSet::of(&[Field::OriginTags]),
            ),
            (Predicate::OriginTag(tag), FieldSet::of(&[Field::OriginTags])),
            (Predicate::KindIn(KindSet::ALL), FieldSet::of(&[Field::Kind])),
            (
                Predicate::Not(Box::new(Predicate::Any(vec![
                    Predicate::HasTag("a".into()),
                    Predicate::All(vec![Predicate::KindIn(KindSet::EMPTY), Predicate::HostUnset {}]),
                ]))),
                FieldSet::of(&[Field::Tags, Field::Kind, Field::Host]),
            ),
        ];
        for (predicate, expected) in cases {
            assert_eq!(predicate.reads(), expected, "{:?}", predicate);
        }
    }

    #[test]
    fn leaf_counts_ignore_composition() {
        let predicate = Predicate::All(vec![
            Predicate::Const(true),
            Predicate::Not(Box::new(Predicate::Any(vec![
                Predicate::HostUnset {},
                Predicate::HasTag("a".into()),
            ]))),
            Predicate::Any(vec![]),
        ]);
        assert_eq!(predicate.leaf_count(), 3);
    }

    #[test]
    fn kind_truth_is_decided_only_by_constants_and_kinds() {
        let counters = Predicate::KindIn(KindSet::of(&[Kind::Counter]));
        let name = Predicate::Name(StringMatch::Eq("a".into()));
        let flag = Predicate::Flag(Operand::literal(true));
        // (predicate, kind, expected)
        let cases: Vec<(Predicate, Kind, Option<bool>)> = vec![
            (Predicate::Const(false), Kind::Gauge, Some(false)),
            (counters.clone(), Kind::Counter, Some(true)),
            (counters.clone(), Kind::Rate, Some(false)),
            (name.clone(), Kind::Counter, None),
            // Flags are fixed for a generation, but not across generations, so they never decide.
            (flag.clone(), Kind::Counter, None),
            (Predicate::IsEmpty(literal_strings(&[])), Kind::Counter, None),
            (
                Predicate::All(vec![counters.clone(), name.clone()]),
                Kind::Rate,
                Some(false),
            ),
            (
                Predicate::All(vec![counters.clone(), name.clone()]),
                Kind::Counter,
                None,
            ),
            (
                Predicate::All(vec![counters.clone(), Predicate::Const(true)]),
                Kind::Counter,
                Some(true),
            ),
            (
                Predicate::Any(vec![counters.clone(), name.clone()]),
                Kind::Counter,
                Some(true),
            ),
            (Predicate::Any(vec![counters.clone(), name.clone()]), Kind::Gauge, None),
            (Predicate::Any(vec![counters.clone(), flag]), Kind::Set, None),
            (Predicate::All(vec![]), Kind::Set, Some(true)),
            (Predicate::Any(vec![]), Kind::Set, Some(false)),
            (Predicate::Not(Box::new(counters.clone())), Kind::Counter, Some(false)),
            (Predicate::Not(Box::new(name)), Kind::Counter, None),
        ];
        for (predicate, kind, expected) in cases {
            assert_eq!(predicate.truth_for_kind(kind), expected, "{:?} for {}", predicate, kind);
        }
    }

    #[test]
    fn kinds_where_a_predicate_can_take_a_value() {
        let scalars = KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]);
        let gateway = Predicate::All(vec![
            Predicate::KindIn(scalars),
            Predicate::Name(StringMatch::Eq("a".into())),
        ]);
        assert_eq!(gateway.kinds_where(true, KindSet::ALL), scalars);
        assert_eq!(gateway.kinds_where(false, KindSet::ALL), KindSet::ALL);

        let counters = Predicate::KindIn(KindSet::of(&[Kind::Counter]));
        let aggregated = KindSet::of(&[Kind::Rate, Kind::Gauge, Kind::Set, Kind::Distribution]);
        assert_eq!(counters.kinds_where(true, aggregated), KindSet::EMPTY);
        assert_eq!(counters.kinds_where(false, aggregated), aggregated);
    }
}
