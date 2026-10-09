use std::{collections::BTreeMap, fmt, str::FromStr, time::Duration};

use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Metadata, ObjectValidation, Schema, SchemaObject},
    JsonSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use snafu::Snafu;

use super::{
    id::{BindingName, CredentialRef, OutputId, ParamName},
    plan::BLOB_MEMBER,
    value_type::ValueType,
};
use crate::{
    canonical::{safe_integer, to_canonical_string, BINDING_MEMBER, MAX_SAFE_INTEGER, RESOLVED_MEMBER},
    model::PortType,
    predicate::Predicate,
};

/// The parameters of a node, as written in a plan: a JSON object keyed by parameter name.
///
/// A plan does not record parameter types: each operator declares a schema of [`ParamSpec`]s, and its parameters are
/// interpreted against that schema with [`decode_params`]. Integers are limited to a magnitude of at most
/// [`MAX_SAFE_INTEGER`], and decimal values are written as strings, so that every parameter has one canonical form.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Params(BTreeMap<ParamName, Value>);

impl Params {
    /// Creates an empty parameter object.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if there are no parameters.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the raw value of the parameter `name`, if it is present.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// Sets the raw value of a parameter, returning the previous value.
    pub fn insert(&mut self, name: ParamName, value: Value) -> Option<Value> {
        self.0.insert(name, value)
    }

    /// Returns an iterator over the parameters, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&ParamName, &Value)> {
        self.0.iter()
    }
}

impl FromIterator<(ParamName, Value)> for Params {
    fn from_iter<I: IntoIterator<Item = (ParamName, Value)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl JsonSchema for Params {
    fn schema_name() -> String {
        "Params".to_string()
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        SchemaObject {
            instance_type: Some(InstanceType::Object.into()),
            object: Some(Box::new(ObjectValidation {
                property_names: Some(Box::new(gen.subschema_for::<ParamName>())),
                ..Default::default()
            })),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "Operator parameters, keyed by parameter name. Each operator's parameter schema defines their \
                     types."
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

/// The type of an operator parameter.
///
/// Types refer to their element types by `&'static` reference so that operator schemas can be defined as constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParamType {
    /// A boolean.
    Bool,

    /// An integer within `min..=max`, which must lie within ±[`MAX_SAFE_INTEGER`].
    Integer {
        /// The smallest allowed value.
        min: i64,

        /// The largest allowed value.
        max: i64,
    },

    /// A decimal number, written as a string such as `"0.25"`.
    Decimal,

    /// A string.
    String,

    /// A duration, written as a string of an integer and a unit, such as `"10s"`. See [`format_duration`].
    Duration,

    /// One of a fixed set of strings.
    Enum(&'static [&'static str]),

    /// An ordered list.
    List(&'static ParamType),

    /// A set, written as a list of distinct elements. Its elements are kept in canonical order.
    Set(&'static ParamType),

    /// A map from strings to values. A map cannot have the key `config` or `blob`, which mark configuration bindings
    /// and blob references.
    StringMap(&'static ParamType),

    /// A [predicate](crate::predicate).
    Predicate,

    /// A reference to a declared output, by its identifier.
    OutputRef,

    /// A reference to a credential, by name. Plans never contain credential values.
    CredentialRef,

    /// A value of the inner type, written literally, or bound to configuration as `{"config": "<binding>"}`.
    ///
    /// See [`Operand`](super::Operand).
    Bindable(&'static ParamType),

    /// An object with named, typed members, such as one rule of a list of rules.
    ///
    /// Members are declared like an operator's parameters: a member not declared is rejected, and a required member
    /// must be present. A record cannot have a member named `config` or `blob`, which mark configuration bindings and
    /// blob references.
    Record(&'static [ParamSpec]),

    /// A [port type](crate::model::PortType), written as `{"kinds": ..., "timestamps": ...}`.
    PortType,
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&ValueType::from(self), f)
    }
}

impl ParamType {
    /// Returns `true` if every valid value of type `other` is also a valid value of this type.
    ///
    /// See [`ValueType::admits`], which this is.
    pub fn admits(&self, other: &ParamType) -> bool {
        ValueType::from(self).admits(&ValueType::from(other))
    }
}

/// A decoded parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    /// A boolean.
    Bool(bool),

    /// An integer.
    Integer(i64),

    /// A decimal number.
    Decimal(Decimal),

    /// A string.
    String(String),

    /// A duration.
    Duration(Duration),

    /// One of an enumeration's strings.
    Enum(String),

    /// An ordered list.
    List(Vec<ParamValue>),

    /// A set, with its elements in canonical order and no duplicates.
    Set(Vec<ParamValue>),

    /// A map from strings to values.
    StringMap(BTreeMap<String, ParamValue>),

    /// A predicate.
    Predicate(Predicate),

    /// A reference to a declared output.
    Output(OutputId),

    /// A reference to a credential.
    Credential(CredentialRef),

    /// A value bound to configuration, with its resolved value when lowering has filled it in.
    Config {
        /// The binding that supplies the value.
        binding: BindingName,

        /// The resolved value, decoded as the bindable type's inner type.
        resolved: Option<Box<ParamValue>>,
    },

    /// A record, with its members decoded.
    Record(DecodedParams),

    /// A port type.
    PortType(PortType),
}

/// A decimal number, kept in the exact form it was written in.
///
/// Decimals are written as strings of an optional minus sign, an integer part without leading zeros, and an optional
/// fractional part, such as `"0.25"` or `"-3"`. They are kept as written, so that they never change through floating
/// point rounding.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Decimal(String);

impl Decimal {
    /// Parses a decimal.
    ///
    /// Returns `None` if `value` is not of the form `-?(0|[1-9][0-9]*)(\.[0-9]+)?`.
    pub fn new(value: &str) -> Option<Self> {
        let digits = value.strip_prefix('-').unwrap_or(value);
        let (integer, fraction) = match digits.split_once('.') {
            Some((integer, fraction)) => (integer, Some(fraction)),
            None => (digits, None),
        };
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let valid_integer = all_digits(integer) && (integer == "0" || !integer.starts_with('0'));
        let valid_fraction = fraction.is_none_or(all_digits);
        (valid_integer && valid_fraction).then(|| Self(value.to_string()))
    }

    /// Returns the decimal as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the nearest `f64` to this decimal.
    pub fn to_f64(&self) -> f64 {
        // The grammar is a subset of what `f64` parses, so this cannot fail.
        self.0.parse().unwrap_or(f64::NAN)
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The units a duration can be written in, from largest to smallest, with their length in nanoseconds.
const DURATION_UNITS: [(&str, u128); 6] = [
    ("h", 3_600_000_000_000),
    ("m", 60_000_000_000),
    ("s", 1_000_000_000),
    ("ms", 1_000_000),
    ("us", 1_000),
    ("ns", 1),
];

/// Parses a duration written as an integer and a unit, such as `"10s"`.
///
/// The units are `h`, `m`, `s`, `ms`, `us`, and `ns`. The integer has no sign and no leading zeros, and is at most
/// [`MAX_SAFE_INTEGER`]. Returns `None` for anything else, or for a duration too long to represent.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = value.split_at(split);
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    let count = digits
        .parse::<u64>()
        .ok()
        .filter(|&count| count <= MAX_SAFE_INTEGER as u64)?;
    let (_, unit_nanos) = DURATION_UNITS.iter().find(|(name, _)| *name == unit)?;

    let nanos = u128::from(count) * unit_nanos;
    let secs = u64::try_from(nanos / 1_000_000_000).ok()?;
    Some(Duration::new(secs, (nanos % 1_000_000_000) as u32))
}

/// Formats a duration in the canonical form that [`parse_duration`] accepts: in the largest unit that represents it
/// exactly, with zero written as `"0s"`.
///
/// Returns `None` if no unit represents the duration with an integer of at most [`MAX_SAFE_INTEGER`].
pub fn format_duration(duration: Duration) -> Option<String> {
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return Some("0s".to_string());
    }
    DURATION_UNITS
        .iter()
        .find(|(_, unit_nanos)| nanos.is_multiple_of(*unit_nanos))
        .and_then(|(name, unit_nanos)| {
            let count = nanos / unit_nanos;
            (count <= MAX_SAFE_INTEGER as u128).then(|| format!("{}{}", count, name))
        })
}

/// An error decoding a parameter against its schema.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display("Invalid parameter `{}`: {}", path, message))]
pub struct ParamError {
    /// The path to the offending value, such as `values[2]`.
    pub path: String,

    /// What is wrong with the value.
    pub message: String,
}

impl ParamError {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            message: message.into(),
        }
    }
}

/// Describes a JSON value briefly, for error messages.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn expected(path: &str, what: &str, value: &Value) -> ParamError {
    ParamError::new(path, format!("expected {}, found {}", what, describe(value)))
}

fn as_integer(value: &Value) -> Option<i64> {
    value.as_number().and_then(|number| safe_integer(number).ok())
}

impl ParamValue {
    /// Returns the value this parameter has, if it is known: the value itself, or the resolved value of a
    /// configuration binding.
    ///
    /// Returns `None` for a binding that has not been resolved, as in an exported program.
    pub fn known(&self) -> Option<&ParamValue> {
        match self {
            Self::Config { resolved, .. } => resolved.as_deref(),
            value => Some(value),
        }
    }

    /// Decodes `value` as a parameter of type `ty`.
    ///
    /// `path` names the value in errors, such as the parameter name.
    ///
    /// # Errors
    ///
    /// Returns an error naming the offending value if `value` is not a valid value of type `ty`.
    pub fn decode(ty: &ParamType, value: &Value, path: &str) -> Result<Self, ParamError> {
        match ty {
            ParamType::Bool => value
                .as_bool()
                .map(Self::Bool)
                .ok_or_else(|| expected(path, "a boolean", value)),
            ParamType::Integer { min, max } => {
                let integer = as_integer(value).ok_or_else(|| {
                    expected(
                        path,
                        "an integer between -9007199254740991 and 9007199254740991, written without a fraction or \
                         exponent",
                        value,
                    )
                })?;
                if integer < *min || integer > *max {
                    return Err(ParamError::new(
                        path,
                        format!("{} is outside the allowed range {}..={}", integer, min, max),
                    ));
                }
                Ok(Self::Integer(integer))
            }
            ParamType::Decimal => {
                let string = value
                    .as_str()
                    .ok_or_else(|| expected(path, "a decimal number written as a string, such as \"0.25\"", value))?;
                Decimal::new(string).map(Self::Decimal).ok_or_else(|| {
                    ParamError::new(
                        path,
                        format!("`{}` is not a decimal number such as \"0.25\" or \"-3\"", string),
                    )
                })
            }
            ParamType::String => value
                .as_str()
                .map(|s| Self::String(s.to_string()))
                .ok_or_else(|| expected(path, "a string", value)),
            ParamType::Duration => {
                let string = value
                    .as_str()
                    .ok_or_else(|| expected(path, "a duration such as \"10s\"", value))?;
                let duration = parse_duration(string).ok_or_else(|| {
                    ParamError::new(
                        path,
                        format!(
                            "`{}` is not a duration: write an integer and one of the units h, m, s, ms, us, or ns, \
                             such as \"10s\"",
                            string
                        ),
                    )
                })?;
                // Each duration has one spelling, so that equal plans are written, and hashed, the same way.
                match format_duration(duration) {
                    Some(canonical) if canonical == string => Ok(Self::Duration(duration)),
                    Some(canonical) => Err(ParamError::new(
                        path,
                        format!(
                            "`{}` is not in canonical form: write it as \"{}\", in the largest unit that represents it \
                             exactly",
                            string, canonical
                        ),
                    )),
                    None => Err(ParamError::new(path, format!("`{}` is too long a duration", string))),
                }
            }
            ParamType::Enum(choices) => {
                let string = value.as_str().ok_or_else(|| expected(path, "a string", value))?;
                if choices.contains(&string) {
                    Ok(Self::Enum(string.to_string()))
                } else {
                    Err(ParamError::new(
                        path,
                        format!("`{}` is not one of: {}", string, choices.join(", ")),
                    ))
                }
            }
            ParamType::List(element) => {
                let values = value.as_array().ok_or_else(|| expected(path, "a list", value))?;
                values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| Self::decode(element, value, &format!("{}[{}]", path, i)))
                    .collect::<Result<_, _>>()
                    .map(Self::List)
            }
            ParamType::Set(element) => {
                let values = value.as_array().ok_or_else(|| expected(path, "a list", value))?;
                let mut keyed = Vec::with_capacity(values.len());
                for (i, value) in values.iter().enumerate() {
                    let element_path = format!("{}[{}]", path, i);
                    let decoded = Self::decode(element, value, &element_path)?;
                    // Compare elements by their canonical encoding, so that elements whose decoded values are equal
                    // count as duplicates whatever their type.
                    let key = to_canonical_string(&decoded.encode())
                        .map_err(|error| ParamError::new(&element_path, error.to_string()))?;
                    keyed.push((key, decoded));
                }
                keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
                if let Some(pair) = keyed.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                    return Err(ParamError::new(
                        path,
                        format!("the set lists {} more than once", pair[0].0),
                    ));
                }
                Ok(Self::Set(keyed.into_iter().map(|(_, decoded)| decoded).collect()))
            }
            ParamType::StringMap(element) => {
                let members = value.as_object().ok_or_else(|| expected(path, "an object", value))?;
                if let Some(problem) = reserved_member(members, "a map cannot have") {
                    return Err(ParamError::new(path, problem));
                }
                members
                    .iter()
                    .map(|(key, value)| {
                        Self::decode(element, value, &format!("{}.{}", path, key)).map(|value| (key.clone(), value))
                    })
                    .collect::<Result<_, _>>()
                    .map(Self::StringMap)
            }
            ParamType::Predicate => serde_json::from_value(value.clone())
                .map(Self::Predicate)
                .map_err(|error| ParamError::new(path, format!("invalid predicate: {}", error))),
            ParamType::OutputRef => {
                let string = value.as_str().ok_or_else(|| expected(path, "an output ID", value))?;
                OutputId::new(string)
                    .map(Self::Output)
                    .map_err(|error| ParamError::new(path, error.to_string()))
            }
            ParamType::CredentialRef => {
                let string = value
                    .as_str()
                    .ok_or_else(|| expected(path, "a credential reference", value))?;
                CredentialRef::new(string)
                    .map(Self::Credential)
                    .map_err(|error| ParamError::new(path, error.to_string()))
            }
            ParamType::Bindable(inner) => match value.as_object() {
                Some(members) if members.contains_key(BINDING_MEMBER) => decode_binding(inner, members, path),
                _ => Self::decode(inner, value, path),
            },
            ParamType::Record(specs) => {
                let members = value.as_object().ok_or_else(|| expected(path, "an object", value))?;
                if let Some(problem) = reserved_member(members, "a record cannot have") {
                    return Err(ParamError::new(path, problem));
                }
                let mut errors = Vec::new();
                let decoded = decode_members(
                    specs,
                    members.keys().map(String::as_str),
                    |name| members.get(name),
                    Some(path),
                    &mut errors,
                );
                match errors.into_iter().next() {
                    Some(error) => Err(error),
                    None => Ok(Self::Record(decoded)),
                }
            }
            ParamType::PortType => serde_json::from_value(value.clone())
                .map(Self::PortType)
                .map_err(|error| ParamError::new(path, format!("invalid port type: {}", error))),
        }
    }

    /// Encodes this value as JSON, the inverse of [`decode`][Self::decode].
    pub fn encode(&self) -> Value {
        match self {
            Self::Bool(value) => Value::Bool(*value),
            Self::Integer(value) => Value::from(*value),
            Self::Decimal(value) => Value::String(value.as_str().to_string()),
            Self::String(value) | Self::Enum(value) => Value::String(value.clone()),
            Self::Duration(value) => match format_duration(*value) {
                Some(formatted) => Value::String(formatted),
                // Durations only come from decoding, which never produces one that cannot be formatted.
                None => Value::Null,
            },
            Self::List(values) | Self::Set(values) => Value::Array(values.iter().map(Self::encode).collect()),
            Self::StringMap(members) => Value::Object(
                members
                    .iter()
                    .map(|(key, value)| (key.clone(), value.encode()))
                    .collect(),
            ),
            Self::Predicate(predicate) => serde_json::to_value(predicate).unwrap_or(Value::Null),
            Self::Output(output) => Value::String(output.to_string()),
            Self::Credential(credential) => Value::String(credential.to_string()),
            Self::Config { binding, resolved } => {
                let mut members = Map::new();
                members.insert(BINDING_MEMBER.to_string(), Value::String(binding.to_string()));
                if let Some(resolved) = resolved {
                    members.insert(RESOLVED_MEMBER.to_string(), resolved.encode());
                }
                Value::Object(members)
            }
            Self::Record(members) => Value::Object(
                members
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.encode()))
                    .collect(),
            ),
            Self::PortType(port_type) => serde_json::to_value(port_type).unwrap_or(Value::Null),
        }
    }
}

/// Returns why `members` cannot be a map or a record, if it has a member that parameters reserve: `config`, which marks
/// configuration bindings, or `blob`, which marks an artifact's blob references.
fn reserved_member(members: &Map<String, Value>, what: &str) -> Option<String> {
    let (name, marks) = if members.contains_key(BINDING_MEMBER) {
        (BINDING_MEMBER, "configuration bindings")
    } else if members.contains_key(BLOB_MEMBER) {
        (BLOB_MEMBER, "blob references")
    } else {
        return None;
    };
    Some(format!("{} a `{}` member, which marks {}", what, name, marks))
}

fn decode_binding(inner: &ParamType, members: &Map<String, Value>, path: &str) -> Result<ParamValue, ParamError> {
    if let Some(unknown) = members
        .keys()
        .find(|key| key.as_str() != BINDING_MEMBER && key.as_str() != RESOLVED_MEMBER)
    {
        return Err(ParamError::new(
            path,
            format!("unknown member `{}` in configuration binding", unknown),
        ));
    }
    let binding = members
        .get(BINDING_MEMBER)
        .and_then(Value::as_str)
        .ok_or_else(|| ParamError::new(path, "a configuration binding's `config` must be a binding name"))?;
    let binding = BindingName::from_str(binding).map_err(|error| ParamError::new(path, error.to_string()))?;
    let resolved = match members.get(RESOLVED_MEMBER) {
        None | Some(Value::Null) => None,
        Some(value) => Some(Box::new(ParamValue::decode(
            inner,
            value,
            &format!("{}.{}", path, RESOLVED_MEMBER),
        )?)),
    };
    Ok(ParamValue::Config { binding, resolved })
}

/// The declaration of one operator parameter, or of one member of a [record][ParamType::Record].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParamSpec {
    /// The parameter's name.
    pub name: &'static str,

    /// The parameter's type.
    pub ty: ParamType,

    /// Whether the parameter must be present.
    pub required: bool,
}

impl ParamSpec {
    /// Declares a parameter that must be present.
    pub const fn required(name: &'static str, ty: ParamType) -> Self {
        Self {
            name,
            ty,
            required: true,
        }
    }

    /// Declares a parameter that may be left out.
    pub const fn optional(name: &'static str, ty: ParamType) -> Self {
        Self {
            name,
            ty,
            required: false,
        }
    }
}

/// Parameters decoded against an operator's schema, keyed by parameter name.
///
/// Decoding checks every value against its declared type, so the typed accessors such as
/// [`integer`][Self::integer] only return `None` when a parameter is absent, or when it is bound to configuration and
/// its value is not known (see [`ParamValue::known`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedParams(BTreeMap<&'static str, ParamValue>);

impl DecodedParams {
    /// Returns the value of the parameter `name`, if it is present.
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.0.get(name)
    }

    /// Returns the known value of the parameter `name`: its literal, or the resolved value of its binding.
    pub fn known(&self, name: &str) -> Option<&ParamValue> {
        self.get(name).and_then(ParamValue::known)
    }

    /// Returns the known value of the boolean parameter `name`.
    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.known(name) {
            Some(ParamValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }

    /// Returns the known value of the integer parameter `name`.
    pub fn integer(&self, name: &str) -> Option<i64> {
        match self.known(name) {
            Some(ParamValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    /// Returns the known value of the string or enumeration parameter `name`.
    pub fn str(&self, name: &str) -> Option<&str> {
        match self.known(name) {
            Some(ParamValue::String(value) | ParamValue::Enum(value)) => Some(value),
            _ => None,
        }
    }

    /// Returns the known value of the duration parameter `name`.
    pub fn duration(&self, name: &str) -> Option<Duration> {
        match self.known(name) {
            Some(ParamValue::Duration(value)) => Some(*value),
            _ => None,
        }
    }

    /// Returns the known elements of the list or set parameter `name`.
    pub fn list(&self, name: &str) -> Option<&[ParamValue]> {
        match self.known(name) {
            Some(ParamValue::List(values) | ParamValue::Set(values)) => Some(values),
            _ => None,
        }
    }

    /// Returns the known value of the predicate parameter `name`.
    pub fn predicate(&self, name: &str) -> Option<&Predicate> {
        match self.known(name) {
            Some(ParamValue::Predicate(predicate)) => Some(predicate),
            _ => None,
        }
    }

    /// Returns the known value of the port type parameter `name`.
    pub fn port_type(&self, name: &str) -> Option<PortType> {
        match self.known(name) {
            Some(ParamValue::PortType(port_type)) => Some(*port_type),
            _ => None,
        }
    }

    /// Returns an iterator over the parameters, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &ParamValue)> {
        self.0.iter().map(|(name, value)| (*name, value))
    }

    /// Encodes the parameters as JSON, the inverse of decoding.
    pub fn encode(&self) -> Params {
        self.0
            .iter()
            .filter_map(|(name, value)| ParamName::new(*name).ok().map(|name| (name, value.encode())))
            .collect()
    }
}

/// Decodes the members of an object against `specs`, pushing an error for each unknown member, each missing required
/// member, and each member that does not match its type.
///
/// `prefix` is the path of the object, for nested records, and `None` for an operator's parameters.
fn decode_members<'a, K, G>(
    specs: &[ParamSpec], keys: K, get: G, prefix: Option<&str>, errors: &mut Vec<ParamError>,
) -> DecodedParams
where
    K: Iterator<Item = &'a str>,
    G: Fn(&str) -> Option<&'a Value>,
{
    let path = |name: &str| match prefix {
        Some(prefix) => format!("{}.{}", prefix, name),
        None => name.to_string(),
    };

    for name in keys {
        if !specs.iter().any(|spec| spec.name == name) {
            let accepted = specs.iter().map(|spec| spec.name).collect::<Vec<_>>();
            let (what, none) = match prefix {
                Some(_) => ("member", "this record has no members"),
                None => ("parameter", "this operator takes no parameters"),
            };
            let accepted = if accepted.is_empty() {
                none.to_string()
            } else {
                format!("accepted {}s are: {}", what, accepted.join(", "))
            };
            errors.push(ParamError::new(&path(name), format!("unknown {}; {}", what, accepted)));
        }
    }

    let mut decoded = BTreeMap::new();
    for spec in specs {
        match get(spec.name) {
            Some(value) => match ParamValue::decode(&spec.ty, value, &path(spec.name)) {
                Ok(value) => {
                    decoded.insert(spec.name, value);
                }
                Err(error) => errors.push(error),
            },
            None if spec.required => {
                let what = if prefix.is_some() { "member" } else { "parameter" };
                errors.push(ParamError::new(&path(spec.name), format!("missing required {}", what)));
            }
            None => {}
        }
    }
    DecodedParams(decoded)
}

/// Decodes `params` against an operator's parameter schema.
///
/// # Errors
///
/// Returns every problem found: one error for each parameter that is not declared in `specs`, each required parameter
/// that is missing, and each value that does not match its declared type.
pub fn decode_params(specs: &[ParamSpec], params: &Params) -> Result<DecodedParams, Vec<ParamError>> {
    let mut errors = Vec::new();
    let decoded = decode_members(
        specs,
        params.iter().map(|(name, _)| name.as_str()),
        |name| params.get(name),
        None,
        &mut errors,
    );
    if errors.is_empty() {
        Ok(decoded)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const STRINGS: ParamType = ParamType::List(&ParamType::String);

    fn decode(ty: ParamType, value: Value) -> Result<ParamValue, ParamError> {
        ParamValue::decode(&ty, &value, "p")
    }

    #[test]
    fn scalar_parameters_decode_by_type() {
        let int = ParamType::Integer { min: -10, max: 10 };
        let cases: Vec<(ParamType, Value, ParamValue)> = vec![
            (ParamType::Bool, json!(true), ParamValue::Bool(true)),
            (int, json!(-10), ParamValue::Integer(-10)),
            (
                ParamType::Decimal,
                json!("0.25"),
                ParamValue::Decimal(Decimal::new("0.25").unwrap()),
            ),
            (
                ParamType::Decimal,
                json!("-3"),
                ParamValue::Decimal(Decimal::new("-3").unwrap()),
            ),
            (ParamType::String, json!(""), ParamValue::String(String::new())),
            (
                ParamType::Duration,
                json!("15s"),
                ParamValue::Duration(Duration::from_secs(15)),
            ),
            (
                ParamType::Duration,
                json!("250ms"),
                ParamValue::Duration(Duration::from_millis(250)),
            ),
            (
                ParamType::Enum(&["mixed", "timestamped"]),
                json!("mixed"),
                ParamValue::Enum("mixed".into()),
            ),
            (
                ParamType::OutputRef,
                json!("dd_metrics_encode"),
                ParamValue::Output(OutputId::new("dd_metrics_encode").unwrap()),
            ),
            (
                ParamType::CredentialRef,
                json!("api_key/0"),
                ParamValue::Credential(CredentialRef::new("api_key/0").unwrap()),
            ),
        ];
        for (ty, value, expected) in cases {
            let decoded = decode(ty, value.clone()).unwrap_or_else(|e| panic!("{:?} {}: {}", ty, value, e));
            assert_eq!(decoded, expected, "{:?} {}", ty, value);
        }
    }

    #[test]
    fn invalid_scalar_parameters_are_rejected_with_a_reason() {
        let int = ParamType::Integer { min: 0, max: 10 };
        // (type, value, expected message fragment)
        let cases: Vec<(ParamType, Value, &str)> = vec![
            (ParamType::Bool, json!("true"), "expected a boolean, found a string"),
            (int, json!(11), "outside the allowed range 0..=10"),
            (int, json!(1.5), "expected an integer"),
            (int, json!(3.0), "expected an integer"),
            (int, json!(9007199254740992_u64), "expected an integer"),
            (ParamType::Decimal, json!(0.25), "written as a string"),
            (ParamType::Decimal, json!("1e5"), "is not a decimal number"),
            (ParamType::Decimal, json!("01.5"), "is not a decimal number"),
            (ParamType::Decimal, json!(".5"), "is not a decimal number"),
            (ParamType::Duration, json!(15), "expected a duration"),
            (ParamType::Duration, json!("15"), "is not a duration"),
            (ParamType::Duration, json!("1.5s"), "is not a duration"),
            (ParamType::Duration, json!("-1s"), "is not a duration"),
            (ParamType::Duration, json!("010s"), "is not a duration"),
            (ParamType::Duration, json!("10d"), "is not a duration"),
            // Each duration has one spelling: the largest unit that represents it exactly.
            (
                ParamType::Duration,
                json!("60s"),
                "`60s` is not in canonical form: write it as \"1m\"",
            ),
            (ParamType::Duration, json!("300s"), "write it as \"5m\""),
            (
                ParamType::Duration,
                json!("1700000300000ms"),
                "write it as \"1700000300s\"",
            ),
            (ParamType::Duration, json!("0ms"), "write it as \"0s\""),
            (ParamType::Duration, json!("2000ns"), "write it as \"2us\""),
            (ParamType::Enum(&["a", "b"]), json!("c"), "is not one of: a, b"),
            (ParamType::OutputRef, json!("Bad"), "Invalid output ID"),
            (
                ParamType::CredentialRef,
                json!("has space"),
                "Invalid credential reference",
            ),
        ];
        for (ty, value, fragment) in cases {
            let error = decode(ty, value.clone()).unwrap_err();
            assert!(error.to_string().contains(fragment), "{:?} {}: {}", ty, value, error);
            assert_eq!(error.path, "p");
        }
    }

    #[test]
    fn collections_decode_their_elements_and_name_the_failing_one() {
        assert_eq!(
            decode(STRINGS, json!(["b", "a"])).unwrap(),
            ParamValue::List(vec![ParamValue::String("b".into()), ParamValue::String("a".into())])
        );
        let error = decode(STRINGS, json!(["a", 1])).unwrap_err();
        assert_eq!(error.path, "p[1]");

        // Sets are kept in canonical order and may not repeat an element.
        assert_eq!(
            decode(ParamType::Set(&ParamType::String), json!(["b", "a"])).unwrap(),
            ParamValue::Set(vec![ParamValue::String("a".into()), ParamValue::String("b".into())])
        );
        let error = decode(ParamType::Set(&ParamType::String), json!(["a", "b", "a"])).unwrap_err();
        assert!(error.message.contains("lists \"a\" more than once"), "{}", error);
        let error = decode(ParamType::Set(&ParamType::Duration), json!(["1m", "1m"])).unwrap_err();
        assert!(error.message.contains("lists \"1m\" more than once"), "{}", error);

        let map = decode(ParamType::StringMap(&ParamType::Bool), json!({"x": true, "y": false})).unwrap();
        assert_eq!(
            map,
            ParamValue::StringMap(BTreeMap::from([
                ("x".to_string(), ParamValue::Bool(true)),
                ("y".to_string(), ParamValue::Bool(false)),
            ]))
        );
        let error = decode(ParamType::StringMap(&ParamType::Bool), json!({"x": 1})).unwrap_err();
        assert_eq!(error.path, "p.x");
        // Parameters reserve the members that mark bindings and blob references.
        for (reserved, marks) in [("config", "configuration bindings"), ("blob", "blob references")] {
            let error = decode(ParamType::StringMap(&ParamType::Bool), json!({reserved: true})).unwrap_err();
            assert_eq!(
                error.message,
                format!("a map cannot have a `{}` member, which marks {}", reserved, marks)
            );
            const RECORD: ParamType = ParamType::Record(&[ParamSpec::optional("x", ParamType::Bool)]);
            let error = decode(RECORD, json!({reserved: true})).unwrap_err();
            assert_eq!(
                error.message,
                format!("a record cannot have a `{}` member, which marks {}", reserved, marks)
            );
        }
    }

    #[test]
    fn bindable_parameters_accept_literals_and_bindings() {
        let bindable = ParamType::Bindable(&STRINGS);
        let binding = BindingName::new("dogstatsd.metric_filter.values").unwrap();

        assert_eq!(
            decode(bindable, json!(["a"])).unwrap(),
            ParamValue::List(vec![ParamValue::String("a".into())])
        );
        assert_eq!(
            decode(bindable, json!({"config": "dogstatsd.metric_filter.values"})).unwrap(),
            ParamValue::Config {
                binding: binding.clone(),
                resolved: None
            }
        );
        let resolved = ParamValue::Config {
            binding,
            resolved: Some(Box::new(ParamValue::List(vec![ParamValue::String("a".into())]))),
        };
        let value = json!({"config": "dogstatsd.metric_filter.values", "resolved": ["a"]});
        assert_eq!(decode(bindable, value.clone()).unwrap(), resolved);
        assert_eq!(resolved.encode(), value);

        // The resolved value must have the inner type, and nothing else may be in the binding.
        let error = decode(bindable, json!({"config": "x", "resolved": [1]})).unwrap_err();
        assert_eq!(error.path, "p.resolved[0]");
        let error = decode(bindable, json!({"config": "x", "value": []})).unwrap_err();
        assert!(error.message.contains("unknown member `value`"), "{}", error);
        // A type that is not bindable treats a binding like any other value of the wrong type.
        assert!(decode(STRINGS, json!({"config": "x"})).is_err());
    }

    #[test]
    fn predicates_decode() {
        let predicate = decode(ParamType::Predicate, json!({"name": {"eq": "x"}})).unwrap();
        assert!(matches!(predicate, ParamValue::Predicate(_)));
        let error = decode(ParamType::Predicate, json!({"name": {"equals": "x"}})).unwrap_err();
        assert!(error.message.starts_with("invalid predicate"), "{}", error);
    }

    #[test]
    fn plans_never_contain_blob_references() {
        // Composition inlines an artifact's blobs, so a blob reference left in a plan is a value of the wrong type.
        let reference = json!({"blob": "sha256:0000000000000000000000000000000000000000000000000000000000000000"});
        for ty in [STRINGS, ParamType::String, ParamType::Predicate] {
            assert!(decode(ty, reference.clone()).is_err(), "{:?}", ty);
        }
    }

    #[test]
    fn decoded_values_encode_back_to_their_canonical_json() {
        let cases: Vec<(ParamType, Value, Value)> = vec![
            (ParamType::Duration, json!("1m"), json!("1m")),
            (ParamType::Duration, json!("1500ms"), json!("1500ms")),
            (ParamType::Duration, json!("0s"), json!("0s")),
            (ParamType::Integer { min: 0, max: 5 }, json!(2), json!(2)),
            (ParamType::Set(&ParamType::String), json!(["z", "a"]), json!(["a", "z"])),
            (ParamType::Decimal, json!("1.50"), json!("1.50")),
            (STRINGS, json!(["z", "a"]), json!(["z", "a"])),
        ];
        for (ty, input, expected) in cases {
            assert_eq!(
                decode(ty, input.clone()).unwrap().encode(),
                expected,
                "{:?} {}",
                ty,
                input
            );
        }
    }

    #[test]
    fn durations_parse_and_format() {
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("7us"), Some(Duration::from_micros(7)));
        assert_eq!(parse_duration("9ns"), Some(Duration::from_nanos(9)));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("9007199254740992ns"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration(""), None);

        assert_eq!(format_duration(Duration::from_secs(7200)).as_deref(), Some("2h"));
        assert_eq!(format_duration(Duration::from_secs(90)).as_deref(), Some("90s"));
        assert_eq!(
            format_duration(Duration::from_nanos(1_000_001)).as_deref(),
            Some("1000001ns")
        );
        assert_eq!(format_duration(Duration::from_secs(u64::MAX)), None);
    }

    #[test]
    fn params_are_checked_against_the_operator_schema() {
        const SPECS: &[ParamSpec] = &[
            ParamSpec {
                name: "values",
                ty: ParamType::Bindable(&STRINGS),
                required: true,
            },
            ParamSpec {
                name: "prefix",
                ty: ParamType::Bool,
                required: false,
            },
        ];
        let params = |value: Value| serde_json::from_value::<Params>(value).unwrap();

        let decoded = decode_params(SPECS, &params(json!({"values": ["a"]}))).unwrap();
        assert_eq!(
            decoded.get("values"),
            Some(&ParamValue::List(vec![ParamValue::String("a".into())]))
        );
        assert_eq!(decoded.get("prefix"), None);
        assert_eq!(decoded.iter().count(), 1);

        let errors = decode_params(SPECS, &params(json!({"prefix": true}))).unwrap_err();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Invalid parameter `values`: missing required parameter"]
        );

        let errors = decode_params(SPECS, &params(json!({"values": [], "mode": "x"}))).unwrap_err();
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["Invalid parameter `mode`: unknown parameter; accepted parameters are: values, prefix"]
        );

        let errors = decode_params(&[], &params(json!({"x": 1}))).unwrap_err();
        assert!(errors[0].message.contains("takes no parameters"), "{:?}", errors);

        // Every problem is reported, not only the first.
        let errors = decode_params(SPECS, &params(json!({"prefix": "yes", "mode": "x"}))).unwrap_err();
        assert_eq!(
            errors.iter().map(|error| error.path.as_str()).collect::<Vec<_>>(),
            vec!["mode", "values", "prefix"]
        );

        // Parameter names follow the identifier grammar.
        assert!(serde_json::from_value::<Params>(json!({"Bad": 1})).is_err());
    }

    const RULE: &[ParamSpec] = &[
        ParamSpec::required("name", ParamType::String),
        ParamSpec::optional("limit", ParamType::Integer { min: 0, max: 10 }),
    ];

    #[test]
    fn records_decode_their_members_by_declaration() {
        let record = ParamType::Record(RULE);
        let decoded = decode(record, json!({"name": "a", "limit": 3})).unwrap();
        let ParamValue::Record(members) = &decoded else {
            panic!("expected a record, got {:?}", decoded);
        };
        assert_eq!(members.str("name"), Some("a"));
        assert_eq!(members.integer("limit"), Some(3));
        assert_eq!(decoded.encode(), json!({"name": "a", "limit": 3}));

        // (value, expected path, expected message fragment)
        let cases = [
            (json!({"limit": 3}), "p.name", "missing required member"),
            (
                json!({"name": "a", "extra": 1}),
                "p.extra",
                "unknown member; accepted members are: name, limit",
            ),
            (
                json!({"name": "a", "limit": 11}),
                "p.limit",
                "outside the allowed range",
            ),
            (
                json!({"name": "a", "config": "x"}),
                "p",
                "cannot have a `config` member",
            ),
            (json!(["a"]), "p", "expected an object, found a list"),
        ];
        for (value, path, fragment) in cases {
            let error = decode(record, value.clone()).unwrap_err();
            assert_eq!(error.path, path, "{}", value);
            assert!(error.message.contains(fragment), "{}: {}", value, error);
        }

        // Errors inside a list of records name the element and the member.
        let rules = ParamType::List(&ParamType::Record(RULE));
        let error = decode(rules, json!([{"name": "a"}, {"name": 1}])).unwrap_err();
        assert_eq!(error.path, "p[1].name");
    }

    #[test]
    fn port_types_decode_from_their_serialized_form() {
        use crate::model::{Kind, KindSet, TimestampForm};

        let decoded = decode(
            ParamType::PortType,
            json!({"kinds": ["gauge"], "timestamps": "timestamped"}),
        )
        .unwrap();
        let expected = PortType {
            kinds: KindSet::of(&[Kind::Gauge]),
            timestamps: TimestampForm::Timestamped,
        };
        assert_eq!(decoded, ParamValue::PortType(expected));
        assert_eq!(
            decoded.encode(),
            json!({"kinds": ["gauge"], "timestamps": "timestamped"})
        );

        let error = decode(ParamType::PortType, json!({"kinds": "all"})).unwrap_err();
        assert!(error.message.starts_with("invalid port type"), "{}", error);
    }

    #[test]
    fn known_values_see_through_resolved_bindings() {
        let literal = ParamValue::Integer(3);
        let resolved = ParamValue::Config {
            binding: BindingName::new("a.b").unwrap(),
            resolved: Some(Box::new(ParamValue::Integer(4))),
        };
        let unresolved = ParamValue::Config {
            binding: BindingName::new("a.b").unwrap(),
            resolved: None,
        };
        assert_eq!(literal.known(), Some(&ParamValue::Integer(3)));
        assert_eq!(resolved.known(), Some(&ParamValue::Integer(4)));
        assert_eq!(unresolved.known(), None);

        const SPECS: &[ParamSpec] = &[
            ParamSpec::optional("a", ParamType::Bindable(&ParamType::Integer { min: 0, max: 9 })),
            ParamSpec::optional("b", ParamType::Bindable(&ParamType::Duration)),
            ParamSpec::optional("c", ParamType::Bool),
        ];
        let params = serde_json::from_value::<Params>(json!({
            "a": {"config": "x.a", "resolved": 4},
            "b": {"config": "x.b"},
            "c": true,
        }))
        .unwrap();
        let decoded = decode_params(SPECS, &params).unwrap();
        assert_eq!(decoded.integer("a"), Some(4));
        assert_eq!(decoded.duration("b"), None, "an unresolved binding has no known value");
        assert!(matches!(decoded.get("b"), Some(ParamValue::Config { .. })));
        assert_eq!(decoded.bool("c"), Some(true));
        // The wrong accessor finds no value of its type.
        assert_eq!(decoded.str("c"), None);
        assert_eq!(decoded.encode(), params);
    }

    #[test]
    fn types_admit_the_values_of_narrower_types() {
        const NARROW_RULE: &[ParamSpec] = &[ParamSpec::required("name", ParamType::String)];
        const STRICT_RULE: &[ParamSpec] = &[
            ParamSpec::required("name", ParamType::String),
            ParamSpec::required("limit", ParamType::Integer { min: 0, max: 10 }),
        ];
        const EXTRA_RULE: &[ParamSpec] = &[
            ParamSpec::required("name", ParamType::String),
            ParamSpec::optional("extra", ParamType::Bool),
        ];
        let int = |min, max| ParamType::Integer { min, max };
        // (accepting type, binding type, admitted)
        let cases: Vec<(ParamType, ParamType, bool)> = vec![
            (ParamType::Bool, ParamType::Bool, true),
            (ParamType::Bool, ParamType::String, false),
            (int(0, 10), int(2, 5), true),
            (int(2, 5), int(0, 10), false),
            (ParamType::Enum(&["a", "b"]), ParamType::Enum(&["a"]), true),
            (ParamType::Enum(&["a"]), ParamType::Enum(&["a", "b"]), false),
            (ParamType::String, ParamType::Enum(&["a"]), true),
            (STRINGS, ParamType::Set(&ParamType::String), true),
            (ParamType::Set(&ParamType::String), STRINGS, false),
            (
                ParamType::List(&ParamType::Integer { min: 0, max: 10 }),
                ParamType::List(&ParamType::Integer { min: 0, max: 3 }),
                true,
            ),
            (
                ParamType::StringMap(&ParamType::Bool),
                ParamType::StringMap(&ParamType::Bool),
                true,
            ),
            // A record admits a record with fewer optional members, or with an optional member made required, but not
            // one that may lack a member it requires or that has a member it does not declare.
            (ParamType::Record(RULE), ParamType::Record(NARROW_RULE), true),
            (ParamType::Record(RULE), ParamType::Record(STRICT_RULE), true),
            (ParamType::Record(STRICT_RULE), ParamType::Record(RULE), false),
            (ParamType::Record(RULE), ParamType::Record(EXTRA_RULE), false),
            (ParamType::Bindable(&ParamType::Duration), ParamType::Duration, true),
            (ParamType::Duration, ParamType::Bindable(&ParamType::Duration), false),
            (ParamType::PortType, ParamType::PortType, true),
        ];
        for (accepting, binding, expected) in cases {
            assert_eq!(
                accepting.admits(&binding),
                expected,
                "{:?} admits {:?}",
                accepting,
                binding
            );
        }
    }

    #[test]
    fn types_describe_themselves() {
        // (type, expected description)
        let cases: Vec<(ParamType, &str)> = vec![
            (ParamType::Integer { min: 0, max: 9 }, "integer in 0..=9"),
            (ParamType::Enum(&["a", "b"]), "one of a, b"),
            (
                ParamType::Bindable(&STRINGS),
                "list of (string), or a configuration binding",
            ),
            (ParamType::Set(&ParamType::Duration), "set of (duration)"),
            (ParamType::StringMap(&ParamType::PortType), "map of (port type)"),
            (
                ParamType::Record(RULE),
                "record { name: string, limit?: integer in 0..=10 }",
            ),
        ];
        for (ty, expected) in cases {
            assert_eq!(ty.to_string(), expected);
        }
    }
}
