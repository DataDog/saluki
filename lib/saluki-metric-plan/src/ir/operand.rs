use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Metadata, ObjectValidation, Schema, SchemaObject, SubschemaValidation},
    JsonSchema,
};
use serde::{de, de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::id::BindingName;
use crate::canonical::{BINDING_MEMBER, RESOLVED_MEMBER};

/// A parameter value that is either written literally or bound to configuration.
///
/// A literal is serialized as the value itself. A binding is serialized as `{"config": "<binding>"}`: lowering resolves
/// the binding from configuration, and re-lowers whenever the bound value changes, so a bound parameter follows its
/// configuration. In a lowered plan a binding also carries its resolved value, as
/// `{"config": "<binding>", "resolved": <value>}`. Exports and artifacts never carry resolved values.
///
/// Because a binding is recognized by its `config` member, no other object in a plan may have a member named `config`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Operand<T> {
    /// A literal value.
    Literal(T),

    /// A value bound to configuration.
    Config {
        /// The binding that supplies the value.
        binding: BindingName,

        /// The value the binding resolved to, when lowering has filled it in.
        resolved: Option<T>,
    },
}

impl<T> Operand<T> {
    /// Creates a literal operand.
    pub fn literal(value: T) -> Self {
        Self::Literal(value)
    }

    /// Creates an unresolved binding.
    pub fn config(binding: BindingName) -> Self {
        Self::Config {
            binding,
            resolved: None,
        }
    }

    /// Returns the binding, if this operand is bound to configuration.
    pub fn binding(&self) -> Option<&BindingName> {
        match self {
            Self::Literal(_) => None,
            Self::Config { binding, .. } => Some(binding),
        }
    }

    /// Returns the value of this operand if it is known: the literal, or the resolved value of a binding.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Config { resolved, .. } => resolved.as_ref(),
        }
    }

    /// Returns this operand with `value` as its resolved value, if it is a binding. Literals are returned unchanged.
    pub fn with_resolved(self, value: T) -> Self {
        match self {
            Self::Literal(literal) => Self::Literal(literal),
            Self::Config { binding, .. } => Self::Config {
                binding,
                resolved: Some(value),
            },
        }
    }

    /// Returns this operand without a resolved value.
    pub fn without_resolved(self) -> Self {
        match self {
            Self::Literal(literal) => Self::Literal(literal),
            Self::Config { binding, .. } => Self::Config {
                binding,
                resolved: None,
            },
        }
    }
}

/// The serialized form of a binding.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BindingRepr<T> {
    config: BindingName,
    // A missing `Option` member deserializes as `None` without `#[serde(default)]`, which would add a `T: Default`
    // bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<T>,
}

impl<T> Serialize for Operand<T>
where
    T: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Literal(value) => value.serialize(serializer),
            Self::Config { binding, resolved } => BindingRepr {
                config: binding.clone(),
                resolved: resolved.as_ref(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for Operand<T>
where
    T: DeserializeOwned,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Plans are JSON, so buffering the value to look for the binding member first is always possible.
        let value = Value::deserialize(deserializer)?;
        if value
            .as_object()
            .is_some_and(|members| members.contains_key(BINDING_MEMBER))
        {
            let binding = serde_json::from_value::<BindingRepr<T>>(value).map_err(de::Error::custom)?;
            Ok(Self::Config {
                binding: binding.config,
                resolved: binding.resolved,
            })
        } else {
            serde_json::from_value(value)
                .map(Self::Literal)
                .map_err(de::Error::custom)
        }
    }
}

impl<T> JsonSchema for Operand<T>
where
    T: JsonSchema,
{
    fn schema_name() -> String {
        format!("Operand_for_{}", T::schema_name())
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        let mut properties = schemars::Map::new();
        properties.insert(BINDING_MEMBER.to_string(), gen.subschema_for::<BindingName>());
        properties.insert(RESOLVED_MEMBER.to_string(), gen.subschema_for::<T>());
        let binding = SchemaObject {
            instance_type: Some(InstanceType::Object.into()),
            object: Some(Box::new(ObjectValidation {
                properties,
                required: [BINDING_MEMBER.to_string()].into_iter().collect(),
                additional_properties: Some(Box::new(Schema::Bool(false))),
                ..Default::default()
            })),
            ..Default::default()
        };

        SchemaObject {
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "A literal value, or a configuration binding `{\"config\": \"<binding>\"}` with an optional \
                     resolved value."
                        .to_string(),
                ),
                ..Default::default()
            })),
            subschemas: Some(Box::new(SubschemaValidation {
                any_of: Some(vec![gen.subschema_for::<T>(), binding.into()]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn binding(name: &str) -> BindingName {
        BindingName::new(name).unwrap()
    }

    #[test]
    fn operands_serialize_as_literals_or_bindings() {
        let cases: Vec<(Operand<Vec<String>>, Value)> = vec![
            (Operand::literal(vec!["a".to_string()]), json!(["a"])),
            (
                Operand::config(binding("dogstatsd.metric_filter.values")),
                json!({"config": "dogstatsd.metric_filter.values"}),
            ),
            (
                Operand::config(binding("dogstatsd.metric_filter.values")).with_resolved(vec!["b".to_string()]),
                json!({"config": "dogstatsd.metric_filter.values", "resolved": ["b"]}),
            ),
        ];

        for (operand, expected) in cases {
            assert_eq!(serde_json::to_value(&operand).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<Operand<Vec<String>>>(expected).unwrap(),
                operand
            );
        }
    }

    #[test]
    fn malformed_bindings_are_rejected() {
        let parse = |value: Value| serde_json::from_value::<Operand<bool>>(value).unwrap_err().to_string();

        assert!(parse(json!({"config": "a", "extra": 1})).contains("unknown field `extra`"));
        assert!(parse(json!({"config": "Not Valid"})).contains("Invalid binding name"));
        assert!(parse(json!({"config": "a", "resolved": "yes"})).contains("expected a boolean"));
        assert!(parse(json!("yes")).contains("expected a boolean"));
        // A null resolved value is the same as none.
        assert_eq!(
            serde_json::from_value::<Operand<bool>>(json!({"config": "a", "resolved": null})).unwrap(),
            Operand::config(binding("a"))
        );
    }

    #[test]
    fn operand_accessors() {
        let literal = Operand::literal(true);
        assert_eq!(literal.value(), Some(&true));
        assert_eq!(literal.binding(), None);
        assert_eq!(literal.clone().with_resolved(false), literal);

        let bound = Operand::config(binding("mrf.enabled"));
        assert_eq!(bound.value(), None);
        assert_eq!(bound.binding(), Some(&binding("mrf.enabled")));

        let resolved = bound.clone().with_resolved(false);
        assert_eq!(resolved.value(), Some(&false));
        assert_eq!(resolved.without_resolved(), bound);
    }
}
