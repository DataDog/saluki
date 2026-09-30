//! Decides whether a typed `DatadogConfiguration` leaf agrees with a recorded Agent getter result.
//!
//! The rules are fixed by `lib/datadog-agent/config-recorder/docs/comparison.md`. They are per
//! (leaf kind, getter), never per key: each leaf kind stands for one getter (the emulation table,
//! comparison.md §3), and one pure function per pair compares exactly (comparison.md §4).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;

use datadog_agent_config::LeafValue;
use datadog_agent_config_corpus::{Getter, GetterRead, GetterResult, GoFloat, GoValue};
use serde_json::Value;

/// The outcome of comparing one leaf against one recorded getter result (comparison.md §5).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// The values agree under the pair's rule.
    Match,
    /// The values disagree; both sides are rendered exactly.
    Differs { adp: String, agent: String },
    /// The typed configuration could not be deserialized, so there is no leaf to compare.
    AdpRejects { error: String },
    /// No rule compares this pair; counted, never silent.
    NotCompared { reason: Reason },
}

/// Why a recorded getter result was not compared (comparison.md §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The getter is not the one the leaf kind stands for.
    NotEmulated { kind: LeafKind, emulated: Getter },
    /// The getter is an explicit-only read (getter-map.md §2.1), which no leaf stands for.
    ExplicitOnly,
    /// The result's shape is not the getter's; the corpus reader rules this out.
    ResultShape,
    /// The value is compared only with `compared`, and this read is of another getter.
    OtherGetter { compared: Getter },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NotEmulated { kind, emulated } => write!(f, "a {kind:?} leaf stands for {emulated}"),
            Reason::ExplicitOnly => f.write_str("explicit-only reads are not compared with a leaf"),
            Reason::ResultShape => f.write_str("the result does not have its getter's shape"),
            Reason::OtherGetter { compared } => write!(f, "the value is compared only with {compared}"),
        }
    }
}

/// The kind of a `LeafValue`: one per variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeafKind {
    Bool,
    Duration,
    F64,
    I64,
    JsonList,
    OptionI64,
    OptionStr,
    Str,
    StringList,
    StringListMap,
    StringMap,
    StringMapList,
}

impl LeafKind {
    /// Every kind, in `LeafValue` declaration order.
    #[cfg(test)]
    const ALL: &'static [LeafKind] = &[
        LeafKind::Bool,
        LeafKind::Duration,
        LeafKind::F64,
        LeafKind::I64,
        LeafKind::JsonList,
        LeafKind::OptionI64,
        LeafKind::OptionStr,
        LeafKind::Str,
        LeafKind::StringList,
        LeafKind::StringListMap,
        LeafKind::StringMap,
        LeafKind::StringMapList,
    ];

    pub(crate) fn of(leaf: &LeafValue<'_>) -> LeafKind {
        match leaf {
            LeafValue::Bool(_) => LeafKind::Bool,
            LeafValue::Duration(_) => LeafKind::Duration,
            LeafValue::F64(_) => LeafKind::F64,
            LeafValue::I64(_) => LeafKind::I64,
            LeafValue::JsonList(_) => LeafKind::JsonList,
            LeafValue::OptionI64(_) => LeafKind::OptionI64,
            LeafValue::OptionStr(_) => LeafKind::OptionStr,
            LeafValue::Str(_) => LeafKind::Str,
            LeafValue::StringList(_) => LeafKind::StringList,
            LeafValue::StringListMap(_) => LeafKind::StringListMap,
            LeafValue::StringMap(_) => LeafKind::StringMap,
            LeafValue::StringMapList(_) => LeafKind::StringMapList,
        }
    }

    /// The getters this kind stands for (comparison.md §3). Only `I64` has two: `GetInt` and
    /// `GetInt64` return the same Go integer read, and §1 of getter-map.md gives a key at most one.
    pub(crate) fn emulated(self) -> &'static [Getter] {
        match self {
            LeafKind::Bool => &[Getter::GetBool],
            LeafKind::Duration => &[Getter::GetDuration],
            LeafKind::F64 => &[Getter::GetFloat64],
            LeafKind::I64 => &[Getter::GetInt, Getter::GetInt64],
            LeafKind::JsonList => &[Getter::Get],
            LeafKind::OptionI64 => &[Getter::GetInt],
            LeafKind::OptionStr => &[Getter::GetString],
            LeafKind::Str => &[Getter::GetString],
            LeafKind::StringList => &[Getter::GetStringSlice],
            LeafKind::StringListMap => &[Getter::GetStringMapStringSlice],
            LeafKind::StringMap => &[Getter::GetStringMapString],
            LeafKind::StringMapList => &[Getter::Get],
        }
    }
}

/// Compares a leaf against every getter recorded for its key at one checkpoint, one verdict per
/// recorded getter, in record order.
pub(crate) fn compare_leaf(leaf: LeafValue<'_>, reads: &[GetterRead]) -> Vec<(Getter, Verdict)> {
    reads
        .iter()
        .map(|r| (r.getter, compare_result(leaf, r.getter, &r.result)))
        .collect()
}

/// Compares a leaf against one recorded getter result.
pub(crate) fn compare_result(leaf: LeafValue<'_>, getter: Getter, result: &GetterResult) -> Verdict {
    if matches!(getter, Getter::ReadConfigSection | Getter::IsConfigured) {
        return not_compared(Reason::ExplicitOnly);
    }
    let kind = LeafKind::of(&leaf);
    if !kind.emulated().contains(&getter) {
        return not_compared(Reason::NotEmulated {
            kind,
            emulated: kind.emulated()[0],
        });
    }
    rule(leaf, result).unwrap_or_else(|| not_compared(Reason::ResultShape))
}

/// The rule for a leaf and a result of its emulated getter; `None` when no rule exists for the pair.
fn rule(leaf: LeafValue<'_>, result: &GetterResult) -> Option<Verdict> {
    Some(match (leaf, result) {
        (LeafValue::Bool(a), GetterResult::Bool(b)) => bool_get_bool(a, *b),
        (LeafValue::Duration(a), GetterResult::Duration(b)) => duration_get_duration(a, b.value),
        (LeafValue::F64(a), GetterResult::Float64(b)) => f64_get_float64(a, b),
        (LeafValue::I64(a), GetterResult::Int(b)) => i64_get_int(a, b.value),
        (LeafValue::JsonList(a), GetterResult::Get(b)) => json_list_get(a, b),
        (LeafValue::OptionI64(a), GetterResult::Int(b)) => option_i64_get_int(a, b.value),
        (LeafValue::OptionStr(a), GetterResult::String(b)) => option_str_get_string(a, b),
        (LeafValue::Str(a), GetterResult::String(b)) => str_get_string(a, b),
        (LeafValue::StringList(a), GetterResult::StringSlice(b)) => string_list_get_string_slice(a, b.as_deref()),
        (LeafValue::StringListMap(a), GetterResult::StringMapStringSlice(b)) => {
            string_list_map_get_string_map_string_slice(a, b)
        }
        (LeafValue::StringMap(a), GetterResult::StringMapString(b)) => string_map_get_string_map_string(a, b.as_ref()),
        (LeafValue::StringMapList(a), GetterResult::Get(b)) => string_map_list_get(a, b),
        _ => return None,
    })
}

/// Compares a byte count ADP computes with one recorded getter result (comparison.md decision 14).
///
/// Only `GetSizeInBytes` returns a byte count, which the corpus encodes as a JSON integer (a Go `uint`).
/// The rule is equal integers, with no other normalization. A read of any other getter is not compared,
/// and neither is a `GetSizeInBytes` result that is not an integer.
pub(crate) fn compare_byte_count(adp: u64, getter: Getter, result: &GetterResult) -> Verdict {
    if matches!(getter, Getter::ReadConfigSection | Getter::IsConfigured) {
        return not_compared(Reason::ExplicitOnly);
    }
    if getter != Getter::GetSizeInBytes {
        return not_compared(Reason::OtherGetter {
            compared: Getter::GetSizeInBytes,
        });
    }
    match result {
        GetterResult::SizeInBytes(agent) => verdict(adp == agent.value, || adp.to_string(), || agent.value.to_string()),
        _ => not_compared(Reason::ResultShape),
    }
}

fn not_compared(reason: Reason) -> Verdict {
    Verdict::NotCompared { reason }
}

fn verdict(equal: bool, adp: impl FnOnce() -> String, agent: impl FnOnce() -> String) -> Verdict {
    if equal {
        Verdict::Match
    } else {
        Verdict::Differs {
            adp: adp(),
            agent: agent(),
        }
    }
}

fn bool_get_bool(adp: bool, agent: bool) -> Verdict {
    verdict(adp == agent, || adp.to_string(), || agent.to_string())
}

/// `GetDuration` is nanoseconds (getter-map.md §3); the leaf is compared in whole nanoseconds.
fn duration_get_duration(adp: Duration, agent_ns: i64) -> Verdict {
    let equal = i128::try_from(adp.as_nanos()).is_ok_and(|ns| ns == i128::from(agent_ns));
    verdict(equal, || format!("{}ns", adp.as_nanos()), || format!("{agent_ns}ns"))
}

/// Floats are equal only bit for bit, so `-0.0` differs from `0.0`; NaN equals NaN and each
/// infinity equals itself, because the corpus encodes them by name, without a payload.
fn f64_get_float64(adp: f64, agent: &GoFloat) -> Verdict {
    let equal = match agent {
        GoFloat::Finite(n) => adp.to_bits() == n.value.to_bits(),
        GoFloat::NaN => adp.is_nan(),
        GoFloat::PosInf => adp == f64::INFINITY,
        GoFloat::NegInf => adp == f64::NEG_INFINITY,
    };
    verdict(equal, || render_f64(adp), || render_go_float(agent))
}

fn i64_get_int(adp: i64, agent: i64) -> Verdict {
    verdict(adp == agent, || adp.to_string(), || agent.to_string())
}

/// `None` matches `0`: the getter's `int` cannot say "unset", so Agent code sees `0` for it.
fn option_i64_get_int(adp: Option<i64>, agent: i64) -> Verdict {
    verdict(adp.unwrap_or(0) == agent, || format!("{adp:?}"), || agent.to_string())
}

/// `None` matches `""`: the getter's `string` cannot say "unset", so Agent code sees `""` for it.
fn option_str_get_string(adp: Option<&str>, agent: &str) -> Verdict {
    verdict(
        adp.unwrap_or("") == agent,
        || format!("{adp:?}"),
        || format!("{agent:?}"),
    )
}

fn str_get_string(adp: &str, agent: &str) -> Verdict {
    verdict(adp == agent, || format!("{adp:?}"), || format!("{agent:?}"))
}

/// Order matters; a nil slice (`null`) matches an empty list. Go code can test a slice for nil, but ADP's
/// typed value cannot hold one (comparison.md decision 6).
fn string_list_get_string_slice(adp: &[String], agent: Option<&[String]>) -> Verdict {
    verdict(
        agent.unwrap_or(&[]) == adp,
        || format!("{adp:?}"),
        || render_option(agent),
    )
}

/// Key order never matters; a nil slice value matches an empty list.
fn string_list_map_get_string_map_string_slice(
    adp: &HashMap<String, Vec<String>>, agent: &BTreeMap<String, Option<Vec<String>>>,
) -> Verdict {
    let equal = adp.len() == agent.len()
        && adp.iter().all(|(k, v)| {
            agent
                .get(k)
                .is_some_and(|a| a.as_deref().unwrap_or(&[]) == v.as_slice())
        });
    verdict(
        equal,
        || format!("{:?}", adp.iter().collect::<BTreeMap<_, _>>()),
        || {
            let shown: BTreeMap<_, _> = agent.iter().map(|(k, v)| (k, render_option(v.as_deref()))).collect();
            format!("{shown:?}")
        },
    )
}

/// Key order never matters; a nil map (`null`) matches an empty map.
fn string_map_get_string_map_string(
    adp: &HashMap<String, String>, agent: Option<&BTreeMap<String, String>>,
) -> Verdict {
    let equal = match agent {
        None => adp.is_empty(),
        Some(a) => a.len() == adp.len() && adp.iter().all(|(k, v)| a.get(k) == Some(v)),
    };
    verdict(
        equal,
        || format!("{:?}", adp.iter().collect::<BTreeMap<_, _>>()),
        || render_option(agent),
    )
}

/// The generic rule: a list of JSON values against `Get`'s dynamic value, with no numeric coercion.
/// A top-level `null` (a nil slice) matches an empty list; a `null` element is strict.
fn json_list_get(adp: &[Value], agent: &GoValue) -> Verdict {
    let equal = match agent {
        GoValue::Null => adp.is_empty(),
        GoValue::List(items) => items.len() == adp.len() && adp.iter().zip(items).all(|(a, b)| json_eq(a, b)),
        _ => false,
    };
    verdict(equal, || Value::Array(adp.to_vec()).to_string(), || render_go(agent))
}

/// A list of string maps against `Get`'s dynamic value: every value must be a Go string. A top-level
/// `null` (a nil slice) matches an empty list.
fn string_map_list_get(adp: &[HashMap<String, String>], agent: &GoValue) -> Verdict {
    let map_eq = |a: &HashMap<String, String>, b: &GoValue| match b {
        GoValue::Map(m) => {
            m.len() == a.len()
                && a.iter()
                    .all(|(k, v)| matches!(m.get(k), Some(GoValue::String(s)) if s == v))
        }
        _ => false,
    };
    let equal = match agent {
        GoValue::Null => adp.is_empty(),
        GoValue::List(items) => items.len() == adp.len() && adp.iter().zip(items).all(|(a, b)| map_eq(a, b)),
        _ => false,
    };
    verdict(
        equal,
        || {
            let shown: Vec<BTreeMap<_, _>> = adp.iter().map(|m| m.iter().collect()).collect();
            format!("{shown:?}")
        },
        || render_go(agent),
    )
}

/// Exact equality of a JSON value and a Go value: an integer never equals a float, even when the
/// two have the same numeric value, and floats are equal only bit for bit.
fn json_eq(adp: &Value, agent: &GoValue) -> bool {
    match (adp, agent) {
        (Value::Null, GoValue::Null) => true,
        (Value::Bool(a), GoValue::Bool(b)) => a == b,
        (Value::String(a), GoValue::String(b)) => a == b,
        (Value::Number(a), GoValue::Int(b)) => match (a.as_i64(), a.as_u64()) {
            (Some(i), _) => i128::from(i) == b.value,
            (None, Some(u)) => i128::from(u) == b.value,
            (None, None) => false,
        },
        (Value::Number(a), GoValue::Float(GoFloat::Finite(b))) => {
            a.is_f64() && a.as_f64().is_some_and(|f| f.to_bits() == b.value.to_bits())
        }
        (Value::Array(a), GoValue::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_eq(x, y)),
        (Value::Object(a), GoValue::Map(b)) => {
            a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => false,
    }
}

fn render_option<T: fmt::Debug>(v: Option<T>) -> String {
    v.map_or_else(|| "null".to_string(), |v| format!("{v:?}"))
}

/// Rust's `Debug` for `f64` prints the shortest text that parses back to the same bits.
fn render_f64(v: f64) -> String {
    format!("{v:?}")
}

fn render_go_float(v: &GoFloat) -> String {
    match v {
        GoFloat::Finite(n) => n.token.clone(),
        GoFloat::NaN => "NaN".to_string(),
        GoFloat::PosInf => "+Inf".to_string(),
        GoFloat::NegInf => "-Inf".to_string(),
    }
}

/// Renders a Go value in JSON form, numbers by their exact corpus token.
fn render_go(v: &GoValue) -> String {
    match v {
        GoValue::Null => "null".to_string(),
        GoValue::Bool(b) => b.to_string(),
        GoValue::Int(n) => n.token.clone(),
        GoValue::Float(f) => render_go_float(f),
        GoValue::String(s) => Value::String(s.clone()).to_string(),
        GoValue::List(items) => format!("[{}]", items.iter().map(render_go).collect::<Vec<_>>().join(",")),
        GoValue::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), render_go(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_config::{DatadogConfiguration, LEAVES};
    use datadog_agent_config_corpus::{Number, Outcome};

    use super::*;

    fn int(v: i64) -> Number<i64> {
        Number {
            value: v,
            token: v.to_string(),
        }
    }

    fn finite(v: f64, token: &str) -> GoFloat {
        GoFloat::Finite(Number {
            value: v,
            token: token.to_string(),
        })
    }

    fn go_int(v: i128) -> GoValue {
        GoValue::Int(Number {
            value: v,
            token: v.to_string(),
        })
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn differs(v: &Verdict) -> bool {
        matches!(v, Verdict::Differs { .. })
    }

    /// A value of each kind, for checking that every emulated pair has a rule.
    fn sample(kind: LeafKind) -> LeafValue<'static> {
        static EMPTY_LIST_MAP: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
        static EMPTY_MAP: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        match kind {
            LeafKind::Bool => LeafValue::Bool(false),
            LeafKind::Duration => LeafValue::Duration(Duration::ZERO),
            LeafKind::F64 => LeafValue::F64(0.0),
            LeafKind::I64 => LeafValue::I64(0),
            LeafKind::JsonList => LeafValue::JsonList(&[]),
            LeafKind::OptionI64 => LeafValue::OptionI64(None),
            LeafKind::OptionStr => LeafValue::OptionStr(None),
            LeafKind::Str => LeafValue::Str(""),
            LeafKind::StringList => LeafValue::StringList(&[]),
            LeafKind::StringListMap => LeafValue::StringListMap(EMPTY_LIST_MAP.get_or_init(HashMap::new)),
            LeafKind::StringMap => LeafValue::StringMap(EMPTY_MAP.get_or_init(HashMap::new)),
            LeafKind::StringMapList => LeafValue::StringMapList(&[]),
        }
    }

    /// A result of each getter's shape.
    fn sample_result(getter: Getter) -> GetterResult {
        match getter {
            Getter::Get => GetterResult::Get(GoValue::Null),
            Getter::GetString => GetterResult::String(String::new()),
            Getter::GetBool => GetterResult::Bool(false),
            Getter::GetInt | Getter::GetInt64 => GetterResult::Int(int(0)),
            Getter::GetInt32 => GetterResult::Int32(Number {
                value: 0,
                token: "0".into(),
            }),
            Getter::GetSizeInBytes => GetterResult::SizeInBytes(Number {
                value: 0,
                token: "0".into(),
            }),
            Getter::GetDuration => GetterResult::Duration(int(0)),
            Getter::GetFloat64 => GetterResult::Float64(finite(0.0, "0.0")),
            Getter::GetFloat64Slice => GetterResult::Float64Slice(None),
            Getter::GetStringSlice => GetterResult::StringSlice(None),
            Getter::GetStringMap => GetterResult::StringMap(None),
            Getter::GetStringMapString => GetterResult::StringMapString(None),
            Getter::GetStringMapStringSlice => GetterResult::StringMapStringSlice(BTreeMap::new()),
            Getter::ReadConfigSection => GetterResult::Section(BTreeMap::new()),
            Getter::IsConfigured => GetterResult::IsConfigured(false),
        }
    }

    #[test]
    fn every_leaf_kind_has_an_emulated_getter_with_a_rule() {
        for &kind in LeafKind::ALL {
            let leaf = sample(kind);
            assert_eq!(LeafKind::of(&leaf), kind);
            assert!(!kind.emulated().is_empty(), "{kind:?} has no emulated getter");
            for &getter in kind.emulated() {
                assert!(
                    rule(leaf, &sample_result(getter)).is_some(),
                    "{kind:?} x {getter} has no rule"
                );
            }
        }
    }

    #[test]
    fn every_supported_leaf_has_a_listed_kind() {
        let config = DatadogConfiguration::default();
        for leaf in LEAVES {
            let kind = LeafKind::of(&(leaf.get)(&config));
            assert!(LeafKind::ALL.contains(&kind), "{}: {kind:?} is not listed", leaf.key);
        }
    }

    #[test]
    fn non_emulated_and_explicit_only_getters_are_not_compared() {
        let leaf = LeafValue::I64(3);
        assert_eq!(
            compare_result(leaf, Getter::GetDuration, &GetterResult::Duration(int(3))),
            Verdict::NotCompared {
                reason: Reason::NotEmulated {
                    kind: LeafKind::I64,
                    emulated: Getter::GetInt
                }
            }
        );
        assert_eq!(
            compare_result(leaf, Getter::IsConfigured, &GetterResult::IsConfigured(true)),
            Verdict::NotCompared {
                reason: Reason::ExplicitOnly
            }
        );
        let strs = strings(&["1"]);
        let v = compare_result(LeafValue::StringList(&strs), Getter::Get, &GetterResult::Get(go_int(1)));
        assert!(matches!(v, Verdict::NotCompared { .. }));
    }

    #[test]
    fn byte_count_rule() {
        let size = |v: u64| {
            GetterResult::SizeInBytes(Number {
                value: v,
                token: v.to_string(),
            })
        };
        let rule = |adp: u64, getter: Getter, result: &GetterResult| compare_byte_count(adp, getter, result);
        assert_eq!(
            rule(10_485_760, Getter::GetSizeInBytes, &size(10_485_760)),
            Verdict::Match
        );
        assert_eq!(
            rule(10_000_000, Getter::GetSizeInBytes, &size(10_485_760)),
            Verdict::Differs {
                adp: "10000000".to_string(),
                agent: "10485760".to_string()
            }
        );
        assert_eq!(rule(0, Getter::GetSizeInBytes, &size(0)), Verdict::Match);
        assert_eq!(rule(u64::MAX, Getter::GetSizeInBytes, &size(u64::MAX)), Verdict::Match);
        // A result of another shape is never read as a byte count, even when its number is equal.
        assert_eq!(
            rule(3, Getter::GetSizeInBytes, &GetterResult::Int(int(3))),
            Verdict::NotCompared {
                reason: Reason::ResultShape
            }
        );
        assert_eq!(
            rule(3, Getter::GetSizeInBytes, &GetterResult::String("3".to_string())),
            Verdict::NotCompared {
                reason: Reason::ResultShape
            }
        );
        assert_eq!(
            rule(10, Getter::GetString, &GetterResult::String("10".to_string())),
            Verdict::NotCompared {
                reason: Reason::OtherGetter {
                    compared: Getter::GetSizeInBytes
                }
            }
        );
        assert_eq!(
            rule(10, Getter::IsConfigured, &GetterResult::IsConfigured(true)),
            Verdict::NotCompared {
                reason: Reason::ExplicitOnly
            }
        );
    }

    #[test]
    fn bool_rule() {
        assert_eq!(bool_get_bool(true, true), Verdict::Match);
        assert!(differs(&bool_get_bool(true, false)));
    }

    #[test]
    fn duration_rule_compares_whole_nanoseconds() {
        assert_eq!(
            duration_get_duration(Duration::from_secs(10), 10_000_000_000),
            Verdict::Match
        );
        assert_eq!(
            duration_get_duration(Duration::from_secs(10), 10_000_000_001),
            Verdict::Differs {
                adp: "10000000000ns".into(),
                agent: "10000000001ns".into()
            }
        );
        assert!(differs(&duration_get_duration(Duration::ZERO, -1)));
    }

    #[test]
    fn f64_rule_is_bit_exact() {
        assert_eq!(f64_get_float64(1.0, &finite(1.0, "1.0")), Verdict::Match);
        assert_eq!(f64_get_float64(f64::NAN, &GoFloat::NaN), Verdict::Match);
        assert_eq!(f64_get_float64(f64::NEG_INFINITY, &GoFloat::NegInf), Verdict::Match);
        assert!(differs(&f64_get_float64(0.0, &finite(-0.0, "-0.0"))));
        assert!(differs(&f64_get_float64(f64::INFINITY, &GoFloat::NegInf)));
        assert_eq!(
            f64_get_float64(0.1, &GoFloat::NaN),
            Verdict::Differs {
                adp: "0.1".into(),
                agent: "NaN".into()
            }
        );
        let next = f64::from_bits(0.3f64.to_bits() + 1);
        assert_eq!(
            f64_get_float64(next, &finite(0.3, "0.3")),
            Verdict::Differs {
                adp: "0.30000000000000004".into(),
                agent: "0.3".into()
            }
        );
    }

    #[test]
    fn i64_rule() {
        assert_eq!(
            i64_get_int(9_007_199_254_740_993, 9_007_199_254_740_993),
            Verdict::Match
        );
        assert!(differs(&i64_get_int(9_007_199_254_740_993, 9_007_199_254_740_992)));
        let v = compare_result(LeafValue::I64(5), Getter::GetInt64, &GetterResult::Int(int(5)));
        assert_eq!(v, Verdict::Match);
    }

    #[test]
    fn option_rules_read_none_as_the_zero_value() {
        assert_eq!(option_i64_get_int(Some(0), 0), Verdict::Match);
        assert_eq!(option_i64_get_int(None, 0), Verdict::Match);
        assert!(differs(&option_i64_get_int(None, 5)));
        assert!(differs(&option_i64_get_int(Some(5), 0)));
        assert_eq!(option_str_get_string(Some(""), ""), Verdict::Match);
        assert_eq!(option_str_get_string(None, ""), Verdict::Match);
        assert_eq!(
            option_str_get_string(None, "x"),
            Verdict::Differs {
                adp: "None".into(),
                agent: "\"x\"".into()
            }
        );
    }

    #[test]
    fn str_rule_does_not_trim_or_fold_case() {
        assert_eq!(str_get_string("a", "a"), Verdict::Match);
        assert!(differs(&str_get_string("a", "a ")));
        assert!(differs(&str_get_string("a", "A")));
    }

    #[test]
    fn string_list_rule_keeps_order_and_reads_nil_as_empty() {
        let ab = strings(&["a", "b"]);
        let ba = strings(&["b", "a"]);
        assert_eq!(string_list_get_string_slice(&ab, Some(&ab)), Verdict::Match);
        assert!(differs(&string_list_get_string_slice(&ab, Some(&ba))));
        assert_eq!(string_list_get_string_slice(&[], None), Verdict::Match);
        assert_eq!(
            string_list_get_string_slice(&ab, None),
            Verdict::Differs {
                adp: "[\"a\", \"b\"]".into(),
                agent: "null".into()
            }
        );
    }

    #[test]
    fn string_list_map_rule_ignores_key_order_and_reads_nil_as_empty() {
        let adp: HashMap<_, _> = [("b".to_string(), strings(&["k"])), ("a".to_string(), vec![])].into();
        let agent: BTreeMap<_, _> = [
            ("a".to_string(), Some(vec![])),
            ("b".to_string(), Some(strings(&["k"]))),
        ]
        .into();
        assert_eq!(
            string_list_map_get_string_map_string_slice(&adp, &agent),
            Verdict::Match
        );
        let nil: BTreeMap<_, _> = [("a".to_string(), None), ("b".to_string(), Some(strings(&["k"])))].into();
        assert_eq!(string_list_map_get_string_map_string_slice(&adp, &nil), Verdict::Match);
        let nil_b: BTreeMap<_, _> = [("a".to_string(), Some(vec![])), ("b".to_string(), None)].into();
        assert!(differs(&string_list_map_get_string_map_string_slice(&adp, &nil_b)));
        let missing: BTreeMap<_, _> = [("b".to_string(), Some(strings(&["k"])))].into();
        assert!(differs(&string_list_map_get_string_map_string_slice(&adp, &missing)));
    }

    #[test]
    fn string_map_rule_ignores_key_order_and_reads_nil_as_empty() {
        let adp: HashMap<_, _> = [("b".to_string(), "2".to_string()), ("a".to_string(), "1".to_string())].into();
        let agent: BTreeMap<_, _> = adp.clone().into_iter().collect();
        assert_eq!(string_map_get_string_map_string(&adp, Some(&agent)), Verdict::Match);
        let mut missing = agent.clone();
        missing.remove("a");
        assert!(differs(&string_map_get_string_map_string(&adp, Some(&missing))));
        assert_eq!(string_map_get_string_map_string(&HashMap::new(), None), Verdict::Match);
        assert!(differs(&string_map_get_string_map_string(&adp, None)));
    }

    #[test]
    fn json_list_rule_keeps_integers_and_floats_apart() {
        let adp: Vec<Value> = serde_json::from_str(r#"[1, 1.5, "x", null, {"k": [true]}]"#).unwrap();
        let agent = GoValue::List(vec![
            go_int(1),
            GoValue::Float(finite(1.5, "1.5")),
            GoValue::String("x".into()),
            GoValue::Null,
            GoValue::Map([("k".to_string(), GoValue::List(vec![GoValue::Bool(true)]))].into()),
        ]);
        assert_eq!(json_list_get(&adp, &agent), Verdict::Match);

        let one_float: Vec<Value> = serde_json::from_str("[1.0]").unwrap();
        assert_eq!(
            json_list_get(&one_float, &GoValue::List(vec![go_int(1)])),
            Verdict::Differs {
                adp: "[1.0]".into(),
                agent: "[1]".into()
            }
        );
        let one_int: Vec<Value> = serde_json::from_str("[1]").unwrap();
        assert!(differs(&json_list_get(
            &one_int,
            &GoValue::List(vec![GoValue::Float(finite(1.0, "1.0"))])
        )));
        assert_eq!(json_list_get(&[], &GoValue::Null), Verdict::Match);
        assert!(differs(&json_list_get(&one_int, &GoValue::Null)));
        let null_item: Vec<Value> = serde_json::from_str("[null]").unwrap();
        assert!(differs(&json_list_get(&null_item, &GoValue::List(vec![]))));
        let reordered = GoValue::List(vec![GoValue::String("x".into()), go_int(1)]);
        let ordered: Vec<Value> = serde_json::from_str(r#"[1, "x"]"#).unwrap();
        assert!(differs(&json_list_get(&ordered, &reordered)));
    }

    #[test]
    fn string_map_list_rule_needs_string_values() {
        let adp = vec![HashMap::from([("name".to_string(), "a".to_string())])];
        let map = |v: GoValue| GoValue::List(vec![GoValue::Map([("name".to_string(), v)].into())]);
        assert_eq!(
            string_map_list_get(&adp, &map(GoValue::String("a".into()))),
            Verdict::Match
        );
        assert!(differs(&string_map_list_get(&adp, &map(go_int(1)))));
        assert_eq!(string_map_list_get(&[], &GoValue::Null), Verdict::Match);
        assert!(differs(&string_map_list_get(&adp, &GoValue::Null)));
        assert_eq!(
            string_map_list_get(&adp, &GoValue::List(vec![GoValue::Map(BTreeMap::new())])),
            Verdict::Differs {
                adp: r#"[{"name": "a"}]"#.into(),
                agent: "[{}]".into()
            }
        );
    }

    #[test]
    fn compare_leaf_gives_one_verdict_per_recorded_getter() {
        let corpus = super::super::corpus();
        let reads = corpus
            .cases
            .iter()
            .filter_map(|c| match &c.outcome {
                Outcome::Started(s) => Some(s),
                Outcome::StartupError(_) => None,
            })
            .flat_map(|s| &s.keys)
            .map(|k| &k.reads.snapshot.getters)
            .find(|g| g.len() > 1)
            .expect("the corpus should record a key with more than one getter");
        let verdicts = compare_leaf(LeafValue::Bool(false), reads);
        assert_eq!(verdicts.len(), reads.len());
        for ((getter, _), read) in verdicts.iter().zip(reads) {
            assert_eq!(*getter, read.getter);
        }
    }
}
