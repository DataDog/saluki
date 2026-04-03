//! A custom serde [`Deserializer`] for [`facet_value::Value`] with string-to-primitive coercion.
//!
//! Environment variables are always strings, but configuration fields often expect booleans, integers, or floats.
//! This deserializer handles that coercion transparently: when the target type requests a boolean and the underlying
//! value is a string like `"true"`, the string is parsed as a boolean. Native typed values (from YAML/JSON sources)
//! pass through without coercion.

use std::fmt;

use facet_value::Value;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserializer as _;

/// Deserializes a `T` from a `facet_value::Value`, with string-to-primitive coercion.
pub(crate) fn from_value<T: de::DeserializeOwned>(value: &Value) -> Result<T, de::value::Error> {
    T::deserialize(ValueDeserializer(value))
}

struct ValueDeserializer<'a>(&'a Value);

impl<'a> ValueDeserializer<'a> {
    fn unexpected(&self) -> de::Unexpected<'_> {
        if self.0.is_null() {
            de::Unexpected::Unit
        } else if let Some(b) = self.0.as_bool() {
            de::Unexpected::Bool(b)
        } else if let Some(n) = self.0.as_number() {
            if let Some(i) = n.to_i64() {
                de::Unexpected::Signed(i)
            } else if let Some(u) = n.to_u64() {
                de::Unexpected::Unsigned(u)
            } else if let Some(f) = n.to_f64() {
                de::Unexpected::Float(f)
            } else {
                de::Unexpected::Other("number")
            }
        } else if let Some(s) = self.0.as_string() {
            de::Unexpected::Str(s.as_str())
        } else if self.0.as_array().is_some() {
            de::Unexpected::Seq
        } else if self.0.as_object().is_some() {
            de::Unexpected::Map
        } else {
            de::Unexpected::Other("unknown value type")
        }
    }
}

impl<'de, 'a> de::Deserializer<'de> for ValueDeserializer<'a> {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.is_null() {
            visitor.visit_unit()
        } else if let Some(b) = self.0.as_bool() {
            visitor.visit_bool(b)
        } else if let Some(n) = self.0.as_number() {
            if let Some(i) = n.to_i64() {
                visitor.visit_i64(i)
            } else if let Some(u) = n.to_u64() {
                visitor.visit_u64(u)
            } else if let Some(f) = n.to_f64() {
                visitor.visit_f64(f)
            } else {
                Err(de::Error::custom("unsupported number type"))
            }
        } else if let Some(s) = self.0.as_string() {
            visitor.visit_str(s.as_str())
        } else if let Some(arr) = self.0.as_array() {
            visitor.visit_seq(SeqAccessor { iter: arr.into_iter() })
        } else if let Some(obj) = self.0.as_object() {
            visitor.visit_map(MapAccessor {
                iter: obj.iter().collect(),
                index: 0,
                pending_value: None,
            })
        } else {
            Err(de::Error::custom("unsupported value type"))
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(b) = self.0.as_bool() {
            visitor.visit_bool(b)
        } else if let Some(s) = self.0.as_string() {
            match s.as_str().to_lowercase().as_str() {
                "true" | "1" | "yes" => visitor.visit_bool(true),
                "false" | "0" | "no" => visitor.visit_bool(false),
                _ => Err(de::Error::invalid_value(self.unexpected(), &"a boolean")),
            }
        } else if let Some(n) = self.0.as_number() {
            if let Some(i) = n.to_i64() {
                match i {
                    1 => visitor.visit_bool(true),
                    0 => visitor.visit_bool(false),
                    _ => Err(de::Error::invalid_value(self.unexpected(), &"a boolean")),
                }
            } else {
                Err(de::Error::invalid_value(self.unexpected(), &"a boolean"))
            }
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"a boolean"))
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(n) = self.0.as_number() {
            if let Some(i) = n.to_i64() {
                return visitor.visit_i64(i);
            }
        }
        if let Some(s) = self.0.as_string() {
            if let Ok(i) = s.as_str().parse::<i64>() {
                return visitor.visit_i64(i);
            }
        }
        Err(de::Error::invalid_type(self.unexpected(), &"an integer"))
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(n) = self.0.as_number() {
            if let Some(u) = n.to_u64() {
                return visitor.visit_u64(u);
            }
        }
        if let Some(s) = self.0.as_string() {
            if let Ok(u) = s.as_str().parse::<u64>() {
                return visitor.visit_u64(u);
            }
        }
        Err(de::Error::invalid_type(self.unexpected(), &"an unsigned integer"))
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(n) = self.0.as_number() {
            if let Some(f) = n.to_f64() {
                return visitor.visit_f64(f);
            }
        }
        if let Some(s) = self.0.as_string() {
            if let Ok(f) = s.as_str().parse::<f64>() {
                return visitor.visit_f64(f);
            }
        }
        Err(de::Error::invalid_type(self.unexpected(), &"a float"))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(s) = self.0.as_string() {
            let s = s.as_str();
            let mut chars = s.chars();
            if let Some(c) = chars.next() {
                if chars.next().is_none() {
                    return visitor.visit_char(c);
                }
            }
        }
        Err(de::Error::invalid_type(self.unexpected(), &"a character"))
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(s) = self.0.as_string() {
            visitor.visit_str(s.as_str())
        } else if let Some(b) = self.0.as_bool() {
            visitor.visit_string(b.to_string())
        } else if let Some(n) = self.0.as_number() {
            if let Some(i) = n.to_i64() {
                visitor.visit_string(i.to_string())
            } else if let Some(u) = n.to_u64() {
                visitor.visit_string(u.to_string())
            } else if let Some(f) = n.to_f64() {
                visitor.visit_string(f.to_string())
            } else {
                Err(de::Error::invalid_type(self.unexpected(), &"a string"))
            }
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"a string"))
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.is_null() {
            visitor.visit_unit()
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"null"))
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self, _name: &'static str, visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self, _name: &'static str, visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(arr) = self.0.as_array() {
            visitor.visit_seq(SeqAccessor { iter: arr.into_iter() })
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"a sequence"))
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self, _name: &'static str, _len: usize, visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(obj) = self.0.as_object() {
            visitor.visit_map(MapAccessor {
                iter: obj.iter().collect(),
                index: 0,
                pending_value: None,
            })
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"a map"))
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self, _name: &'static str, _fields: &'static [&'static str], visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self, _name: &'static str, _variants: &'static [&'static str], visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if let Some(s) = self.0.as_string() {
            visitor.visit_enum(de::value::StrDeserializer::new(s.as_str()))
        } else if let Some(obj) = self.0.as_object() {
            // For externally tagged enums: { "VariantName": value }
            let mut iter = obj.iter();
            if let Some((key, value)) = iter.next() {
                if iter.next().is_none() {
                    return visitor.visit_enum(EnumAccessor {
                        variant: key.as_str(),
                        value,
                    });
                }
            }
            Err(de::Error::invalid_value(
                de::Unexpected::Map,
                &"a map with a single key as enum variant",
            ))
        } else {
            Err(de::Error::invalid_type(self.unexpected(), &"a string or map"))
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }
}

// -- SeqAccess for arrays --

struct SeqAccessor<'a> {
    iter: std::slice::Iter<'a, Value>,
}

impl<'de, 'a> SeqAccess<'de> for SeqAccessor<'a> {
    type Error = de::value::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error> {
        match self.iter.next() {
            Some(value) => seed.deserialize(ValueDeserializer(value)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

// -- MapAccess for objects --

struct MapAccessor<'a> {
    iter: Vec<(&'a facet_value::VString, &'a Value)>,
    index: usize,
    pending_value: Option<&'a Value>,
}

impl<'de, 'a> MapAccess<'de> for MapAccessor<'a> {
    type Error = de::value::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error> {
        if self.index < self.iter.len() {
            let (key, value) = self.iter[self.index];
            self.index += 1;
            self.pending_value = Some(value);
            seed.deserialize(de::value::StrDeserializer::new(key.as_str()))
                .map(Some)
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Self::Error> {
        let value = self
            .pending_value
            .take()
            .expect("next_value_seed called before next_key_seed");
        seed.deserialize(ValueDeserializer(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len() - self.index)
    }
}

// -- EnumAccess for externally tagged enums --

struct EnumAccessor<'a> {
    variant: &'a str,
    value: &'a Value,
}

impl<'de, 'a> de::EnumAccess<'de> for EnumAccessor<'a> {
    type Error = de::value::Error;
    type Variant = VariantAccessor<'a>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error> {
        let variant = seed.deserialize(de::value::StrDeserializer::new(self.variant))?;
        Ok((variant, VariantAccessor(self.value)))
    }
}

struct VariantAccessor<'a>(&'a Value);

impl<'de, 'a> de::VariantAccess<'de> for VariantAccessor<'a> {
    type Error = de::value::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        if self.0.is_null() {
            Ok(())
        } else {
            de::Deserialize::deserialize(ValueDeserializer(self.0))
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Self::Error> {
        seed.deserialize(ValueDeserializer(self.0))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error> {
        ValueDeserializer(self.0).deserialize_seq(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self, _fields: &'static [&'static str], visitor: V,
    ) -> Result<V::Value, Self::Error> {
        ValueDeserializer(self.0).deserialize_map(visitor)
    }
}

// -- Display for error context --

impl fmt::Display for ValueDeserializer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ValueDeserializer")
    }
}

#[cfg(test)]
mod tests {
    use facet_value::value;

    use super::*;

    #[test]
    fn test_string_to_bool_coercion() {
        assert!(from_value::<bool>(&Value::from("true")).unwrap());
        assert!(!from_value::<bool>(&Value::from("false")).unwrap());
        assert!(from_value::<bool>(&Value::from("TRUE")).unwrap());
        assert!(!from_value::<bool>(&Value::from("FALSE")).unwrap());
        assert!(from_value::<bool>(&Value::from("1")).unwrap());
        assert!(!from_value::<bool>(&Value::from("0")).unwrap());
        assert!(from_value::<bool>(&Value::from("yes")).unwrap());
        assert!(!from_value::<bool>(&Value::from("no")).unwrap());
    }

    #[test]
    fn test_native_bool() {
        assert!(from_value::<bool>(&Value::TRUE).unwrap());
        assert!(!from_value::<bool>(&Value::FALSE).unwrap());
    }

    #[test]
    fn test_string_to_integer_coercion() {
        assert_eq!(from_value::<i64>(&Value::from("42")).unwrap(), 42);
        assert_eq!(from_value::<i64>(&Value::from("-10")).unwrap(), -10);
        assert_eq!(from_value::<u64>(&Value::from("8080")).unwrap(), 8080);
        assert_eq!(from_value::<u16>(&Value::from("443")).unwrap(), 443);
    }

    #[test]
    fn test_string_to_float_coercion() {
        assert!((from_value::<f64>(&Value::from("0.5")).unwrap() - 0.5).abs() < f64::EPSILON);
        assert!((from_value::<f64>(&Value::from("2.72")).unwrap() - 2.72).abs() < f64::EPSILON);
    }

    #[test]
    fn test_string_passthrough() {
        assert_eq!(from_value::<String>(&Value::from("hello")).unwrap(), "hello");
        assert_eq!(from_value::<String>(&Value::from("true")).unwrap(), "true");
        assert_eq!(from_value::<String>(&Value::from("42")).unwrap(), "42");
    }

    #[test]
    fn test_number_to_string_coercion() {
        assert_eq!(from_value::<String>(&Value::from(42i64)).unwrap(), "42");
        assert_eq!(from_value::<String>(&Value::from(8080u64)).unwrap(), "8080");
    }

    #[test]
    fn test_bool_to_string_coercion() {
        assert_eq!(from_value::<String>(&Value::TRUE).unwrap(), "true");
        assert_eq!(from_value::<String>(&Value::FALSE).unwrap(), "false");
    }

    #[test]
    fn test_native_number() {
        assert_eq!(from_value::<i64>(&Value::from(42i64)).unwrap(), 42);
        assert_eq!(from_value::<u64>(&Value::from(100u64)).unwrap(), 100);
    }

    #[test]
    fn test_option_some() {
        assert_eq!(from_value::<Option<bool>>(&Value::from("true")).unwrap(), Some(true));
        assert_eq!(from_value::<Option<i64>>(&Value::from("42")).unwrap(), Some(42));
    }

    #[test]
    fn test_option_none() {
        assert_eq!(from_value::<Option<bool>>(&Value::NULL).unwrap(), None);
    }

    #[test]
    fn test_struct_deserialization() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Config {
            enabled: bool,
            port: u16,
            name: String,
        }

        let val = value!({
            "enabled": true,
            "port": 8080,
            "name": "test"
        });
        let config: Config = from_value(&val).unwrap();
        assert_eq!(
            config,
            Config {
                enabled: true,
                port: 8080,
                name: "test".to_string(),
            }
        );
    }

    #[test]
    fn test_struct_with_string_coercion() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Config {
            enabled: bool,
            port: u16,
            rate: f64,
        }

        // All values as strings (simulating env vars)
        let val = value!({
            "enabled": "true",
            "port": "8080",
            "rate": "0.5"
        });
        let config: Config = from_value(&val).unwrap();
        assert_eq!(
            config,
            Config {
                enabled: true,
                port: 8080,
                rate: 0.5,
            }
        );
    }

    #[test]
    fn test_array_deserialization() {
        let val = value!(["a", "b", "c"]);
        let result: Vec<String> = from_value(&val).unwrap();
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_invalid_bool_string() {
        assert!(from_value::<bool>(&Value::from("not_a_bool")).is_err());
    }

    #[test]
    fn test_invalid_integer_string() {
        assert!(from_value::<i64>(&Value::from("not_a_number")).is_err());
    }

    #[test]
    fn test_enum_unit_variant() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "lowercase")]
        enum Level {
            Debug,
            Info,
            Warn,
        }

        assert_eq!(from_value::<Level>(&Value::from("debug")).unwrap(), Level::Debug);
        assert_eq!(from_value::<Level>(&Value::from("info")).unwrap(), Level::Info);
    }
}
