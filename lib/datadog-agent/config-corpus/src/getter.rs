//! Getter results decoded by their getter's Go return type (getter-map.md §2) and the result
//! encoding (getter-map.md §3).
//!
//! Every number keeps its exact token next to its parsed value, so a consumer can compare exactly,
//! and integer and float tokens stay distinct at every depth.

use std::collections::BTreeMap;

use crate::json::Json;
use crate::lists::Getter;

/// A number and the exact token the corpus wrote for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Number<T> {
    /// The parsed value.
    pub value: T,
    /// The token as written.
    pub token: String,
}

/// A Go `float64`: a float token, or one of the non-finite forms `{"$float":...}`.
#[derive(Clone, Debug, PartialEq)]
pub enum GoFloat {
    /// A finite float, written with `.`, `e` or `E`.
    Finite(Number<f64>),
    /// `{"$float":"NaN"}`.
    NaN,
    /// `{"$float":"+Inf"}`.
    PosInf,
    /// `{"$float":"-Inf"}`.
    NegInf,
}

/// A generic Go value, as `Get` and `GetStringMap` return it.
#[derive(Clone, Debug, PartialEq)]
pub enum GoValue {
    /// `nil`, or a nil slice or map.
    Null,
    /// A `bool`.
    Bool(bool),
    /// Any Go integer type; `i128` holds every `int64` and `uint64`.
    Int(Number<i128>),
    /// A `float32` or `float64`.
    Float(GoFloat),
    /// A `string`.
    String(String),
    /// A slice or array.
    List(Vec<GoValue>),
    /// A map, keys in byte order.
    Map(BTreeMap<String, GoValue>),
}

/// A getter's result, typed by the getter's Go return type.
#[derive(Clone, Debug, PartialEq)]
pub enum GetterResult {
    /// `GetBool`.
    Bool(bool),
    /// `GetString`.
    String(String),
    /// `GetInt` and `GetInt64`.
    Int(Number<i64>),
    /// `GetInt32`.
    Int32(Number<i32>),
    /// `GetSizeInBytes`.
    SizeInBytes(Number<u64>),
    /// `GetDuration`, in nanoseconds.
    Duration(Number<i64>),
    /// `GetFloat64`.
    Float64(GoFloat),
    /// `GetFloat64Slice`; `None` for a nil slice.
    Float64Slice(Option<Vec<GoFloat>>),
    /// `GetStringSlice`; `None` for a nil slice.
    StringSlice(Option<Vec<String>>),
    /// `GetStringMap`; `None` for a nil map.
    StringMap(Option<BTreeMap<String, GoValue>>),
    /// `GetStringMapString`; `None` for a nil map.
    StringMapString(Option<BTreeMap<String, String>>),
    /// `GetStringMapStringSlice`, which never returns a nil map; a `None` value is a nil slice.
    StringMapStringSlice(BTreeMap<String, Option<Vec<String>>>),
    /// `Get`.
    Get(GoValue),
    /// `ReadConfigSection` (getter-map.md §2.1): the nested section map, never `null`.
    Section(BTreeMap<String, GoValue>),
    /// `IsConfigured` (getter-map.md §2.1): whether the user set the key.
    IsConfigured(bool),
}

type R<T> = Result<T, String>;

fn is_float_token(n: &str) -> bool {
    n.contains(['.', 'e', 'E'])
}

fn integer<T: std::str::FromStr>(v: &Json, what: &str) -> R<Number<T>> {
    match v {
        Json::Num(n) if !is_float_token(n) => n
            .parse()
            .map(|value| Number {
                value,
                token: n.clone(),
            })
            .map_err(|_| format!("{what}: integer {n} is out of range")),
        other => Err(format!("{what}: expected an integer token, found {}", describe(other))),
    }
}

fn describe(v: &Json) -> String {
    match v {
        Json::Num(n) => format!("number {n}"),
        other => other.kind().to_string(),
    }
}

/// The non-finite form, if `m` is an object whose only member is `$float`.
fn non_finite(m: &[(crate::json::JStr, Json)]) -> Option<R<GoFloat>> {
    let [(k, v)] = m else { return None };
    if k.text != "$float" {
        return None;
    }
    Some(match v.as_str() {
        Some("NaN") => Ok(GoFloat::NaN),
        Some("+Inf") => Ok(GoFloat::PosInf),
        Some("-Inf") => Ok(GoFloat::NegInf),
        _ => Err("a map whose only key is \"$float\" must be NaN, +Inf or -Inf".into()),
    })
}

fn float(v: &Json, what: &str) -> R<GoFloat> {
    match v {
        Json::Num(n) if is_float_token(n) => {
            let value: f64 = n.parse().map_err(|_| format!("{what}: bad float {n}"))?;
            if !value.is_finite() {
                return Err(format!("{what}: float {n} is out of range"));
            }
            Ok(GoFloat::Finite(Number {
                value,
                token: n.clone(),
            }))
        }
        Json::Obj(m) => non_finite(m).unwrap_or_else(|| Err(format!("{what}: expected a float, found object"))),
        other => Err(format!(
            "{what}: expected a float token (with '.', 'e' or 'E'), found {}",
            describe(other)
        )),
    }
}

fn string(v: &Json, what: &str) -> R<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{what}: expected a string, found {}", describe(v)))
}

fn nullable<T>(v: &Json, f: impl FnOnce(&Json) -> R<T>) -> R<Option<T>> {
    match v {
        Json::Null => Ok(None),
        v => f(v).map(Some),
    }
}

fn list<T>(v: &Json, what: &str, f: impl Fn(&Json) -> R<T>) -> R<Vec<T>> {
    match v {
        Json::Arr(a) => a.iter().map(f).collect(),
        other => Err(format!("{what}: expected an array, found {}", describe(other))),
    }
}

fn map<T>(v: &Json, what: &str, f: impl Fn(&Json) -> R<T>) -> R<BTreeMap<String, T>> {
    match v {
        Json::Obj(m) => {
            if non_finite(m).is_some() {
                return Err(format!("{what}: a map whose only key is \"$float\" is not a map"));
            }
            m.iter().map(|(k, x)| Ok((k.text.clone(), f(x)?))).collect()
        }
        other => Err(format!("{what}: expected an object, found {}", describe(other))),
    }
}

fn go_value(v: &Json) -> R<GoValue> {
    Ok(match v {
        Json::Null => GoValue::Null,
        Json::Bool(b) => GoValue::Bool(*b),
        Json::Num(n) if is_float_token(n) => GoValue::Float(float(v, "value")?),
        Json::Num(_) => GoValue::Int(integer(v, "value")?),
        Json::Str(s) => GoValue::String(s.text.clone()),
        Json::Arr(_) => GoValue::List(list(v, "value", go_value)?),
        Json::Obj(m) => match non_finite(m) {
            Some(f) => GoValue::Float(f?),
            None => GoValue::Map(map(v, "value", go_value)?),
        },
    })
}

/// Decodes `v` as the result of `getter`; a shape mismatch is an error.
pub(crate) fn decode(getter: Getter, v: &Json) -> R<GetterResult> {
    let what = getter.as_str();
    Ok(match getter {
        Getter::GetBool => match v {
            Json::Bool(b) => GetterResult::Bool(*b),
            other => return Err(format!("{what}: expected a boolean, found {}", describe(other))),
        },
        Getter::GetString => GetterResult::String(string(v, what)?),
        Getter::GetInt | Getter::GetInt64 => GetterResult::Int(integer(v, what)?),
        Getter::GetInt32 => GetterResult::Int32(integer(v, what)?),
        Getter::GetSizeInBytes => GetterResult::SizeInBytes(integer(v, what)?),
        Getter::GetDuration => GetterResult::Duration(integer(v, what)?),
        Getter::GetFloat64 => GetterResult::Float64(float(v, what)?),
        Getter::GetFloat64Slice => GetterResult::Float64Slice(nullable(v, |v| list(v, what, |x| float(x, what)))?),
        Getter::GetStringSlice => GetterResult::StringSlice(nullable(v, |v| list(v, what, |x| string(x, what)))?),
        Getter::GetStringMap => GetterResult::StringMap(nullable(v, |v| map(v, what, go_value))?),
        Getter::GetStringMapString => {
            GetterResult::StringMapString(nullable(v, |v| map(v, what, |x| string(x, what)))?)
        }
        Getter::GetStringMapStringSlice => {
            GetterResult::StringMapStringSlice(map(v, what, |x| nullable(x, |x| list(x, what, |s| string(s, what))))?)
        }
        Getter::Get => GetterResult::Get(go_value(v)?),
        Getter::ReadConfigSection => GetterResult::Section(map(v, what, go_value)?),
        Getter::IsConfigured => match v {
            Json::Bool(b) => GetterResult::IsConfigured(*b),
            other => return Err(format!("{what}: expected a boolean, found {}", describe(other))),
        },
    })
}
