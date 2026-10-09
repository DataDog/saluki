//! Owned, serializable descriptions of parameter types.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize};

use super::params::{ParamSpec, ParamType};

/// The type of a parameter or configuration binding value, owned and serializable.
///
/// This is the same type language as [`ParamType`], which operator schemas declare as constants and which therefore
/// cannot be built at run time. A catalog that a host exports, and that a tool reads back away from the host, describes
/// its bindings' types with `ValueType`s instead. Every `ParamType` converts into one.
///
/// Serialized as an object whose `type` member names the variant, with the variant's other members beside it, such as
/// `{"type": "list", "element": {"type": "string"}}` or `{"type": "integer", "min": 0, "max": 100}`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValueType {
    /// A boolean.
    Bool,

    /// An integer within `min..=max`.
    Integer {
        /// The smallest allowed value.
        min: i64,

        /// The largest allowed value.
        max: i64,
    },

    /// A decimal number, written as a string.
    Decimal,

    /// A string.
    String,

    /// A duration.
    Duration,

    /// One of a fixed set of strings.
    Enum {
        /// The allowed strings.
        choices: Vec<String>,
    },

    /// An ordered list.
    List {
        /// The type of each element.
        element: Box<ValueType>,
    },

    /// A set, written as a list of distinct elements.
    Set {
        /// The type of each element.
        element: Box<ValueType>,
    },

    /// A map from strings to values.
    StringMap {
        /// The type of each value.
        element: Box<ValueType>,
    },

    /// A predicate.
    Predicate,

    /// A reference to a declared output.
    OutputRef,

    /// A reference to a credential.
    CredentialRef,

    /// A value of the inner type, written literally or bound to configuration.
    Bindable {
        /// The type of the value.
        inner: Box<ValueType>,
    },

    /// An object with named, typed members.
    Record {
        /// The members, in declaration order.
        members: Vec<MemberType>,
    },

    /// A port type.
    PortType,
}

/// The serialized form of a [`ValueType`], with every member any variant has.
///
/// Deserializing through this form rejects members the named variant does not have, which serde's internally tagged
/// representation does not do for variants without members.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueTypeRepr {
    #[serde(rename = "type")]
    kind: String,
    min: Option<i64>,
    max: Option<i64>,
    choices: Option<Vec<String>>,
    element: Option<Box<ValueType>>,
    inner: Option<Box<ValueType>>,
    members: Option<Vec<MemberType>>,
}

impl<'de> Deserialize<'de> for ValueType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = ValueTypeRepr::deserialize(deserializer)?;
        let present = [
            ("min", repr.min.is_some()),
            ("max", repr.max.is_some()),
            ("choices", repr.choices.is_some()),
            ("element", repr.element.is_some()),
            ("inner", repr.inner.is_some()),
            ("members", repr.members.is_some()),
        ];
        let (value, members): (Option<Self>, &[&str]) = match repr.kind.as_str() {
            "bool" => (Some(Self::Bool), &[]),
            "integer" => (
                repr.min.zip(repr.max).map(|(min, max)| Self::Integer { min, max }),
                &["min", "max"],
            ),
            "decimal" => (Some(Self::Decimal), &[]),
            "string" => (Some(Self::String), &[]),
            "duration" => (Some(Self::Duration), &[]),
            "enum" => (repr.choices.map(|choices| Self::Enum { choices }), &["choices"]),
            "list" => (repr.element.map(|element| Self::List { element }), &["element"]),
            "set" => (repr.element.map(|element| Self::Set { element }), &["element"]),
            "string_map" => (repr.element.map(|element| Self::StringMap { element }), &["element"]),
            "predicate" => (Some(Self::Predicate), &[]),
            "output_ref" => (Some(Self::OutputRef), &[]),
            "credential_ref" => (Some(Self::CredentialRef), &[]),
            "bindable" => (repr.inner.map(|inner| Self::Bindable { inner }), &["inner"]),
            "record" => (repr.members.map(|members| Self::Record { members }), &["members"]),
            "port_type" => (Some(Self::PortType), &[]),
            other => return Err(de::Error::custom(format_args!("unknown value type `{}`", other))),
        };
        if let Some((name, _)) = present
            .iter()
            .find(|(name, present)| *present && !members.contains(name))
        {
            return Err(de::Error::custom(format_args!(
                "a `{}` value type has no member `{}`",
                repr.kind, name
            )));
        }
        value.ok_or_else(|| {
            de::Error::custom(format_args!(
                "a `{}` value type needs the members: {}",
                repr.kind,
                members.join(", ")
            ))
        })
    }
}

/// One member of a [record][ValueType::Record] type.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberType {
    /// The member's name.
    pub name: String,

    /// The member's type.
    #[serde(rename = "type")]
    pub ty: ValueType,

    /// Whether the member must be present.
    pub required: bool,
}

impl From<&ParamType> for ValueType {
    fn from(ty: &ParamType) -> Self {
        let boxed = |element: &ParamType| Box::new(Self::from(element));
        match ty {
            ParamType::Bool => Self::Bool,
            ParamType::Integer { min, max } => Self::Integer { min: *min, max: *max },
            ParamType::Decimal => Self::Decimal,
            ParamType::String => Self::String,
            ParamType::Duration => Self::Duration,
            ParamType::Enum(choices) => Self::Enum {
                choices: choices.iter().map(|choice| choice.to_string()).collect(),
            },
            ParamType::List(element) => Self::List {
                element: boxed(element),
            },
            ParamType::Set(element) => Self::Set {
                element: boxed(element),
            },
            ParamType::StringMap(element) => Self::StringMap {
                element: boxed(element),
            },
            ParamType::Predicate => Self::Predicate,
            ParamType::OutputRef => Self::OutputRef,
            ParamType::CredentialRef => Self::CredentialRef,
            ParamType::Bindable(inner) => Self::Bindable { inner: boxed(inner) },
            ParamType::Record(members) => Self::Record {
                members: members.iter().map(MemberType::from).collect(),
            },
            ParamType::PortType => Self::PortType,
        }
    }
}

impl From<ParamType> for ValueType {
    fn from(ty: ParamType) -> Self {
        Self::from(&ty)
    }
}

impl From<&ParamSpec> for MemberType {
    fn from(spec: &ParamSpec) -> Self {
        Self {
            name: spec.name.to_string(),
            ty: ValueType::from(&spec.ty),
            required: spec.required,
        }
    }
}

impl ValueType {
    /// Returns `true` if every valid value of type `other` is also a valid value of this type.
    ///
    /// The validator uses this to check that a configuration binding of type `other` can supply a parameter of this
    /// type. Integer ranges and enumerations may be narrower in `other`; a set is a valid list; a record must declare
    /// only members this record declares, with admitted types, and every member this record requires.
    pub fn admits(&self, other: &ValueType) -> bool {
        match (self, other) {
            (Self::Bool, Self::Bool)
            | (Self::Decimal, Self::Decimal)
            | (Self::String, Self::String)
            | (Self::Duration, Self::Duration)
            | (Self::Predicate, Self::Predicate)
            | (Self::OutputRef, Self::OutputRef)
            | (Self::CredentialRef, Self::CredentialRef)
            | (Self::PortType, Self::PortType)
            | (Self::String, Self::Enum { .. }) => true,
            (
                Self::Integer { min, max },
                Self::Integer {
                    min: other_min,
                    max: other_max,
                },
            ) => min <= other_min && other_max <= max,
            (Self::Enum { choices }, Self::Enum { choices: other_choices }) => {
                other_choices.iter().all(|choice| choices.contains(choice))
            }
            (Self::List { element }, Self::List { element: other_element } | Self::Set { element: other_element })
            | (Self::Set { element }, Self::Set { element: other_element })
            | (Self::StringMap { element }, Self::StringMap { element: other_element }) => {
                element.admits(other_element)
            }
            (Self::Record { members }, Self::Record { members: other_members }) => {
                let declared = other_members.iter().all(|other| {
                    members
                        .iter()
                        .find(|member| member.name == other.name)
                        .is_some_and(|member| member.ty.admits(&other.ty))
                });
                let required = members.iter().filter(|member| member.required).all(|member| {
                    other_members
                        .iter()
                        .any(|other| other.name == member.name && other.required)
                });
                declared && required
            }
            (Self::Bindable { inner }, Self::Bindable { inner: other_inner }) => inner.admits(other_inner),
            (Self::Bindable { inner }, other) => inner.admits(other),
            _ => false,
        }
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool => f.write_str("boolean"),
            Self::Integer { min, max } => write!(f, "integer in {}..={}", min, max),
            Self::Decimal => f.write_str("decimal"),
            Self::String => f.write_str("string"),
            Self::Duration => f.write_str("duration"),
            Self::Enum { choices } => write!(f, "one of {}", choices.join(", ")),
            Self::List { element } => write!(f, "list of ({})", element),
            Self::Set { element } => write!(f, "set of ({})", element),
            Self::StringMap { element } => write!(f, "map of ({})", element),
            Self::Predicate => f.write_str("predicate"),
            Self::OutputRef => f.write_str("output reference"),
            Self::CredentialRef => f.write_str("credential reference"),
            Self::Bindable { inner } => write!(f, "{}, or a configuration binding", inner),
            Self::Record { members } => {
                f.write_str("record {")?;
                for (i, member) in members.iter().enumerate() {
                    let separator = if i == 0 { " " } else { ", " };
                    let optional = if member.required { "" } else { "?" };
                    write!(f, "{}{}{}: {}", separator, member.name, optional, member.ty)?;
                }
                f.write_str(" }")
            }
            Self::PortType => f.write_str("port type"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const RULE: &[ParamSpec] = &[
        ParamSpec::required("name", ParamType::String),
        ParamSpec::optional("limit", ParamType::Integer { min: 0, max: 10 }),
    ];

    #[test]
    fn value_types_serialize_with_a_type_member() {
        // (parameter type, its serialized description)
        let cases = [
            (ParamType::Bool, json!({"type": "bool"})),
            (
                ParamType::Integer { min: 0, max: 100 },
                json!({"type": "integer", "min": 0, "max": 100}),
            ),
            (
                ParamType::List(&ParamType::String),
                json!({"type": "list", "element": {"type": "string"}}),
            ),
            (
                ParamType::Enum(&["mixed", "untimestamped"]),
                json!({"type": "enum", "choices": ["mixed", "untimestamped"]}),
            ),
            (
                ParamType::Bindable(&ParamType::Duration),
                json!({"type": "bindable", "inner": {"type": "duration"}}),
            ),
            (
                ParamType::Record(RULE),
                json!({"type": "record", "members": [
                    {"name": "name", "type": {"type": "string"}, "required": true},
                    {"name": "limit", "type": {"type": "integer", "min": 0, "max": 10}, "required": false}
                ]}),
            ),
            (
                ParamType::StringMap(&ParamType::PortType),
                json!({"type": "string_map", "element": {"type": "port_type"}}),
            ),
        ];
        for (ty, expected) in cases {
            let value_type = ValueType::from(&ty);
            assert_eq!(serde_json::to_value(&value_type).unwrap(), expected, "{:?}", ty);
            assert_eq!(serde_json::from_value::<ValueType>(expected).unwrap(), value_type);
            // Both forms describe themselves the same way.
            assert_eq!(value_type.to_string(), ty.to_string());
        }

        // (malformed description, expected message fragment)
        let malformed = [
            (json!({"type": "list"}), "needs the members: element"),
            (json!({"type": "integer", "min": 0}), "needs the members: min, max"),
            (
                json!({"type": "bool", "element": {"type": "bool"}}),
                "has no member `element`",
            ),
            (json!({"type": "bool", "extra": 1}), "unknown field `extra`"),
            (json!({"type": "blob"}), "unknown value type `blob`"),
            (json!({"element": {"type": "bool"}}), "missing field `type`"),
        ];
        for (value, fragment) in malformed {
            let error = serde_json::from_value::<ValueType>(value.clone())
                .unwrap_err()
                .to_string();
            assert!(error.contains(fragment), "{}: {}", value, error);
        }
    }

    #[test]
    fn admission_follows_the_parameter_type_rules() {
        let strings = ValueType::from(ParamType::List(&ParamType::String));
        // (parameter type, binding type, admitted)
        let cases = [
            (
                strings.clone(),
                ValueType::from(ParamType::Set(&ParamType::String)),
                true,
            ),
            (
                ValueType::from(ParamType::Set(&ParamType::String)),
                strings.clone(),
                false,
            ),
            (
                ValueType::from(ParamType::Integer { min: 0, max: 100 }),
                ValueType::from(ParamType::Integer { min: 1, max: 10 }),
                true,
            ),
            (
                ValueType::from(ParamType::Integer { min: 1, max: 10 }),
                ValueType::from(ParamType::Integer { min: 0, max: 100 }),
                false,
            ),
            (ValueType::String, ValueType::from(ParamType::Enum(&["a"])), true),
            (
                ValueType::from(ParamType::Enum(&["a", "b"])),
                ValueType::from(ParamType::Enum(&["a"])),
                true,
            ),
            (
                ValueType::from(ParamType::Enum(&["a"])),
                ValueType::from(ParamType::Enum(&["a", "b"])),
                false,
            ),
            (
                ValueType::from(ParamType::Bindable(&ParamType::Bool)),
                ValueType::Bool,
                true,
            ),
            (
                ValueType::Bool,
                ValueType::from(ParamType::Bindable(&ParamType::Bool)),
                false,
            ),
            (
                ValueType::from(ParamType::Record(RULE)),
                ValueType::from(ParamType::Record(&RULE[..1])),
                true,
            ),
            (
                ValueType::from(ParamType::Record(&RULE[1..])),
                ValueType::from(ParamType::Record(RULE)),
                false,
            ),
            (ValueType::Bool, ValueType::String, false),
        ];
        for (parameter, binding, admitted) in cases {
            assert_eq!(parameter.admits(&binding), admitted, "{} admits {}", parameter, binding);
        }
    }
}
