use std::fmt;

use facet_value::{Destructured, DestructuredMut, DestructuredRef, VArray, VNumber, VObject, Value};
use saluki_error::GenericError;
use stringtheory::MetaString;

enum KeySegment {
    Field(MetaString),
    Array(usize),
}

struct KeyPath {
    segments: Vec<KeySegment>,
}

impl KeyPath {
    fn empty() -> Self {
        KeyPath { segments: Vec::new() }
    }

    fn push_field(&mut self, field: &str) {
        self.segments.push(KeySegment::Field(field.into()));
    }

    fn push_index(&mut self, index: usize) {
        self.segments.push(KeySegment::Array(index));
    }

    fn pop(&mut self) {
        self.segments.pop();
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                KeySegment::Field(field) => {
                    if idx > 0 {
                        write!(f, ".{}", field)?
                    } else {
                        write!(f, "{}", field)?
                    }
                }
                KeySegment::Array(index) => write!(f, "[{}]", index)?,
            }
        }
        Ok(())
    }
}

/// A generic, type-erased map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values(VObject);

impl Values {
    /// Creates a new `Values` instance from a `Value`.
    ///
    /// Returns `None` if the value is not an object.
    pub fn from_value(value: Value) -> Option<Self> {
        match value.destructure() {
            Destructured::Object(obj) => Some(Self(obj)),
            _ => None,
        }
    }

    /// Returns a reference to the value associated with the given key.
    ///
    /// The key supports a dot-separated path to a nested value, where each segment is the key within the object at the
    /// given level.
    ///
    /// If the key does not exist, either in terms of an intermediate segment or the final key, `None` is returned.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let mut current = None;

        for key_segment in key.split('.') {
            match current {
                // Haven't yet recursed, so grab from our top-level object.
                None => current = self.0.get(key_segment),

                // We're actively recursing, so grab from the current object instead.
                Some(value) => match value.destructure_ref() {
                    DestructuredRef::Object(obj) => current = obj.get(key_segment),
                    _ => return None,
                },
            }
        }

        current
    }

    /// Merges `other` into `self`, returning the merge values.
    ///
    /// # Errors
    ///
    /// If there is a conflict between the values being merged, an error is returned.
    pub fn merge(self, other: Self) -> Result<Self, GenericError> {
        deep_merge(self.0, other.0).map(Self)
    }
}

fn deep_merge(base: VObject, other: VObject) -> Result<VObject, GenericError> {
    let mut key_path = KeyPath::empty();

    let mut base_value = base.into_value();
    let other_value = other.into_value();
    deep_merge_inner(&mut base_value, other_value, &mut key_path)?;

    match base_value.destructure() {
        Destructured::Object(obj) => Ok(obj),
        _ => unreachable!(),
    }
}

fn deep_merge_inner(base: &mut Value, other: Value, key_path: &mut KeyPath) -> Result<(), GenericError> {
    // We have a special case here for null values, where we tacitly assume that if the base value is null, that
    // it's not inherently a type mismatch to override it with _whatever_ the incoming value happens to be.
    //
    // Doing it this way results in slightly simpler code compared to doing it in our big match block below.
    if base.is_null() {
        *base = other;
        return Ok(());
    }

    match (base.destructure_mut(), other.destructure()) {
        (DestructuredMut::Object(base_obj), Destructured::Object(obj)) => deep_merge_object(base_obj, obj, key_path)?,
        (DestructuredMut::Array(base_arr), Destructured::Array(arr)) => deep_merge_array(base_arr, arr, key_path)?,
        // `DestructuredMut` doesn't provide a mutable reference to the boolean value, so we have to override `base` itself.
        (DestructuredMut::Bool(_), Destructured::Bool(val)) => *base = val.into(),
        // For straightforward scalars, we simply override the base value with the incoming value.
        //
        // TODO: Does this make sense for number values? In particular, do we want to allow the merging of different
        // number types?
        (DestructuredMut::Bytes(base_bytes), Destructured::Bytes(bytes)) => *base_bytes = bytes,
        (DestructuredMut::String(base_str), Destructured::String(str)) => *base_str = str,
        (DestructuredMut::Number(base_num), Destructured::Number(num)) => *base_num = num,
        (DestructuredMut::DateTime(base_dt), Destructured::DateTime(dt)) => *base_dt = dt,
        // We don't allow type mismatches other than the specific one for null values on the base side.
        (base, other) => return Err(type_mismatch_error(base, other, key_path)),
    }

    Ok(())
}

fn deep_merge_object(base: &mut VObject, other: VObject, key_path: &mut KeyPath) -> Result<(), GenericError> {
    for (other_key, other_value) in other.into_iter() {
        match base.get_mut(&other_key) {
            Some(base_value) => {
                key_path.push_field(&other_key);
                deep_merge_inner(base_value, other_value, key_path)?;
                key_path.pop();
            }
            None => {
                base.insert(other_key, other_value);
            }
        }
    }

    Ok(())
}

fn deep_merge_array(base: &mut VArray, other: VArray, key_path: &mut KeyPath) -> Result<(), GenericError> {
    // For array merges, we preserve the order of both the `base` and `other` arrays, and only override overlapping
    // elements.
    //
    // This means that if `base` is smaller than `other`, we override the overlapping elements in `base` with those from
    // `other`, and then append the remaining elements from `other` to `base`. If `other` is smaller than `base`, we
    // simply override the overlapping elements in `base` with those from `other`.
    for (index, other_value) in other.into_iter().enumerate() {
        if index < base.len() {
            let base_value = &mut base[index];
            key_path.push_index(index);
            deep_merge_inner(base_value, other_value, key_path)?;
            key_path.pop();
        } else {
            base.push(other_value);
        }
    }

    Ok(())
}

fn type_mismatch_error(base: DestructuredMut<'_>, other: Destructured, key_path: &KeyPath) -> GenericError {
    let base_type = match base {
        DestructuredMut::Null => "null",
        DestructuredMut::Bool(_) => "bool",
        DestructuredMut::Number(vnumber) => vnumber_type_str(vnumber),
        DestructuredMut::String(_) => "string",
        DestructuredMut::Bytes(_) => "bytes",
        DestructuredMut::Array(_) => "array",
        DestructuredMut::Object(_) => "object",
        DestructuredMut::DateTime(_) => "datetime",
        DestructuredMut::QName(_) => "qualified-name",
        DestructuredMut::Uuid(_) => "uuid",
    };

    let other_type = match other {
        Destructured::Null => "null",
        Destructured::Bool(_) => "bool",
        Destructured::Number(vnumber) => vnumber_type_str(&vnumber),
        Destructured::String(_) => "string",
        Destructured::Bytes(_) => "bytes",
        Destructured::Array(_) => "array",
        Destructured::Object(_) => "object",
        Destructured::DateTime(_) => "datetime",
        Destructured::QName(_) => "qualified-name",
        Destructured::Uuid(_) => "uuid",
    };

    GenericError::msg(format!(
        "Type mismatch for field '{}': expected '{}', got '{}'.",
        key_path, base_type, other_type
    ))
}

fn vnumber_type_str(number: &VNumber) -> &'static str {
    if number.is_float() {
        "number(float)"
    } else {
        // TODO: Submit PR to expose whether or not it's a signed or unsigned integer.
        "number(int)"
    }
}

#[cfg(test)]
mod tests {
    use facet_value::{VQName, VUuid};
    use proptest::{collection::vec as arb_vec, prelude::*};

    use super::*;

    macro_rules! values {
        ($rest:tt) => {{
            let obj = ::facet_value::value!($rest);
            $crate::value::Values::from_value(obj).unwrap()
        }};
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::NULL),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<f64>()
                .prop_filter("f64 must be finite", |f| f.is_finite())
                .prop_map(Value::from),
            any::<String>().prop_map(Value::from),
            arb_vec(any::<u8>(), 0..32).prop_map(Value::from),
            any::<(String, String)>().prop_map(|(ns, n)| { Value::from(VQName::new(ns, n)) }),
            any::<u128>().prop_map(|uuid| { Value::from(VUuid::from_u128(uuid)) }),
        ];

        leaf.prop_recursive(8, 128, 8, |inner| {
            prop_oneof![
                arb_vec(inner.clone(), 0..10).prop_map(|values| VArray::from_iter(values).into_value()),
                arb_vec((".*", inner), 0..10).prop_map(|pairs| VObject::from_iter(pairs).into_value()),
            ]
        })
    }

    #[test]
    fn test_merge_null() {
        // Merging a value _into_ a null value should always succeed since we don't do type checking, as the implication
        // is that since it's null in the "base", we can't actually say that it's wrong for it to be whatever the type
        // on the "other" side is.
        let base_values = values!({
            "bool": null,
            "number": null,
            "string": null,
            "bytes": null,
        });

        let other_values = values!({
            "bool": true,
            "number": 1.2345,
            "string": "hello",
            "bytes": (b"world".to_vec()),
        });

        // Simple assertion that each object has the keys we expect, and that the values are not equal.
        assert_ne!(base_values.get("bool"), other_values.get("bool"));
        assert_ne!(base_values.get("number"), other_values.get("number"));
        assert_ne!(base_values.get("string"), other_values.get("string"));
        assert_ne!(base_values.get("bytes"), other_values.get("bytes"));

        // Merge the values together and assert that the merged values match the values from "other".
        let merged = base_values.merge(other_values.clone()).unwrap();

        assert_eq!(merged.get("bool"), other_values.get("bool"));
        assert_eq!(merged.get("number"), other_values.get("number"));
        assert_eq!(merged.get("string"), other_values.get("string"));
        assert_eq!(merged.get("bytes"), other_values.get("bytes"));
    }

    #[test]
    fn test_merge_scalars() {
        // Build our "base" and "other" objects with the same keys but differing values: same value _type_, but not the same value itself.
        let base_values = values!({
            "bool": false,
            "number": 1.2345,
            "string": "hello",
            "bytes": (b"world".to_vec()),
        });

        let other_values = values!({
            "bool": true,
            "number": 4.20,
            "string": "world",
            "bytes": (b"hello".to_vec()),
        });

        // Simple assertion that each object has the keys we expect, and that the values are not equal.
        assert_ne!(base_values.get("bool"), other_values.get("bool"));
        assert_ne!(base_values.get("number"), other_values.get("number"));
        assert_ne!(base_values.get("string"), other_values.get("string"));
        assert_ne!(base_values.get("bytes"), other_values.get("bytes"));

        // Merge the values together and assert that the merged values match the values from "other".
        let merged = base_values.merge(other_values.clone()).unwrap();

        assert_eq!(merged.get("bool"), other_values.get("bool"));
        assert_eq!(merged.get("number"), other_values.get("number"));
        assert_eq!(merged.get("string"), other_values.get("string"));
        assert_eq!(merged.get("bytes"), other_values.get("bytes"));
    }

    #[test]
    fn test_merge_nested() {
        // Build our "base" and "other" so that they're both nested objects with partial overlap.
        let base_values = values!({
            "data_plane": {
                "enabled": false,
                "dogstatsd": {
                    "enabled": false,
                },
            },
        });

        let other_values = values!({
            "api_key": "super-secret-key",
            "data_plane": {
                "enabled": true,
                "otlp": {
                    "enabled": true,
                },
            },
        });

        // Simple assertion that the objects are not equal.
        assert_ne!(base_values, other_values);

        // Merge the values together and then check that the keys present in both objects have the value from the
        // "other" side, while non-overlapping keys retain their original values.
        let merged = base_values.clone().merge(other_values.clone()).unwrap();

        assert_eq!(merged.get("api_key"), other_values.get("api_key"));
        assert_eq!(merged.get("data_plane.enabled"), other_values.get("data_plane.enabled"));
        assert_eq!(
            merged.get("data_plane.otlp.enabled"),
            other_values.get("data_plane.otlp.enabled")
        );
        assert_eq!(
            merged.get("data_plane.dogstatsd.enabled"),
            base_values.get("data_plane.dogstatsd.enabled")
        );
    }

    #[test]
    fn test_merge_array_base_larger() {
        // Build our "base" and "other" objects each with the same key holding an array, such that the "base" array is larger
        // than the "other" array.
        let base_values = values!({
            "items": ["base_one", "base_two", "base_three"],
        });

        let other_values = values!({
            "items": ["other_one", "other_two"],
        });

        // Merge the values together and then check that the arrays have been merged, with only the overlapping elements
        // having been overridden.
        let merged = base_values.clone().merge(other_values.clone()).unwrap();

        let merged_array = merged.get("items").unwrap().as_array().unwrap();
        let base_array = base_values.get("items").unwrap().as_array().unwrap();
        let other_array = other_values.get("items").unwrap().as_array().unwrap();

        assert!(other_array.len() < base_array.len());
        assert_eq!(merged_array.len(), base_array.len());
        assert_eq!(merged_array[0], other_array[0]);
        assert_eq!(merged_array[1], other_array[1]);
        assert_eq!(merged_array[2], base_array[2]);
    }

    #[test]
    fn test_merge_array_other_larger() {
        // Build our "base" and "other" objects each with the same key holding an array, such that the "other" array is larger
        // than the "base" array.
        let base_values = values!({
            "items": ["base_one", "base_two"],
        });

        let other_values = values!({
            "items": ["other_one", "other_two", "other_three"],
        });

        // Merge the values together and then check that the arrays have been merged, with both the overlapping elements
        // having been overridden, as well as the non-overlapping elements from the "other" array having been appended.
        let merged = base_values.clone().merge(other_values.clone()).unwrap();

        let merged_array = merged.get("items").unwrap().as_array().unwrap();
        let base_array = base_values.get("items").unwrap().as_array().unwrap();
        let other_array = other_values.get("items").unwrap().as_array().unwrap();

        assert!(other_array.len() > base_array.len());
        assert_eq!(merged_array.len(), other_array.len());
        assert_eq!(merged_array[0], other_array[0]);
        assert_eq!(merged_array[1], other_array[1]);
        assert_eq!(merged_array[2], other_array[2]);
    }

    proptest! {
        #[test]
        fn property_test_merge_type_mismatch(mut base_value in arb_value(), other_value in arb_value()) {
            // We only want to test merging values of different types.
            prop_assume!(base_value.value_type() != other_value.value_type());

            // We don't care about cases where the base value is null.
            prop_assume!(!base_value.is_null());

            // We create our "base" and "other" objects with the same key, each using the respective value passed in
            // to the test function.
            let base_values = values!({ "value": (base_value.clone()) });
            let other_values = values!({ "value": (other_value.clone()) });

            // Merge the values together and assert that an error is returned due to type mismatch.
            let result = base_values.merge(other_values);
            let actual_error = result.unwrap_err().to_string();

            let mut key_path = KeyPath::empty();
            key_path.push_field("value");

            let expected_error = type_mismatch_error(base_value.destructure_mut(), other_value.destructure(), &key_path);

            assert_eq!(expected_error.to_string(), actual_error.to_string());
        }
    }
}
